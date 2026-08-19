//! Pure schematic-to-board synchronization planning.
//!
//! The public tool handler and KiCad IPC adapter live outside this module.
//! This module owns the deep planning interface: turn a KiCad-exported
//! flattened netlist plus a board snapshot into a complete, immutable plan.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::mcp::protocol::{CallToolResult, ToolContent};
use crate::tools::{
    pcb_board::{attempt_ipc_write, BoardWrite},
    ToolContext,
};
use anyhow::{bail, Context, Result};
use konnect_sexp::SexpNode;
use prost::Message;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExportedDesign {
    components: Vec<DesignComponent>,
    skipped: Vec<SkippedComponent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedComponent {
    reference: String,
    symbol_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesignComponent {
    reference: String,
    value: String,
    footprint_id: String,
    symbol_path: String,
    dnp: bool,
    pad_nets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct Bounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct BoardFootprint {
    kiid: String,
    reference: String,
    value: String,
    footprint_id: String,
    symbol_path: Option<String>,
    pad_nets: BTreeMap<String, String>,
    position: Point,
    rotation: f64,
    layer: String,
    locked: bool,
    dnp: bool,
    not_in_schematic: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct BoardState {
    footprints: Vec<BoardFootprint>,
    /// Net name to the number of routed copper objects (tracks, arcs, vias,
    /// and zones) carrying the net.
    routed_nets: BTreeMap<String, usize>,
    bounds: Bounds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PlanStatus {
    Ready,
    Noop,
    Conflict,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
struct CountPair {
    planned: usize,
    applied: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct SyncCounts {
    added: CountPair,
    updated: CountPair,
    pads_reassigned: CountPair,
    board_only_preserved: CountPair,
    skipped_by_flag: CountPair,
    conflicts: CountPair,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct PreservedBoardState {
    position: Point,
    rotation: f64,
    layer: String,
    locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PlannedChange {
    Add {
        reference: String,
        value: String,
        footprint_id: String,
        symbol_path: String,
        dnp: bool,
        pad_nets: BTreeMap<String, String>,
        position: Point,
    },
    Update {
        kiid: String,
        reference: String,
        value: String,
        symbol_path: String,
        dnp: bool,
        pad_nets: BTreeMap<String, String>,
        preserve: PreservedBoardState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SyncDiagnostic {
    code: String,
    message: String,
    reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct SyncPlan {
    status: PlanStatus,
    plan_revision: String,
    counts: SyncCounts,
    changes: Vec<PlannedChange>,
    diagnostics: Vec<SyncDiagnostic>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct IdentityRebindCounts {
    requested: usize,
    eligible: usize,
    planned: usize,
    applied: usize,
    conflicts: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct IdentityRebindChange {
    reference: String,
    kiid: String,
    old_symbol_path: String,
    new_symbol_path: String,
    value: String,
    footprint_id: String,
    dnp: bool,
    pad_nets: BTreeMap<String, String>,
    preserve: PreservedBoardState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct IdentityRebindPlan {
    status: PlanStatus,
    plan_revision: String,
    counts: IdentityRebindCounts,
    changes: Vec<IdentityRebindChange>,
    diagnostics: Vec<SyncDiagnostic>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentityRebindArgs {
    schematic: PathBuf,
    board: PathBuf,
    references: Vec<String>,
    dry_run: bool,
    expected_plan_revision: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct RebindPathFacts {
    schematic: PathBuf,
    schematic_exists: bool,
    board: PathBuf,
    board_exists: bool,
}

#[derive(Debug)]
struct LiveSnapshot {
    state: BoardState,
    items: BTreeMap<String, prost_types::Any>,
    net_codes: BTreeMap<String, i32>,
    document: konnect_ipc::gen::kiapi::common::types::DocumentSpecifier,
}

#[derive(Debug)]
struct PreparedFootprint {
    pads: Vec<konnect_ipc::IpcPadDefinition>,
    graphics: Vec<konnect_ipc::IpcGraphicDefinition>,
    fields: konnect_ipc::IpcFieldPlacement,
    width: f64,
    height: f64,
}

pub(crate) async fn handle_update_pcb_from_schematic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> Result<CallToolResult> {
    let schematic = crate::tools::get_path(args, "schematic")?;
    let board = crate::tools::get_path(args, "board")?;
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let expected_revision = args["expected_plan_revision"].as_str().map(str::to_string);
    if !dry_run && expected_revision.is_none() {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "expected_plan_revision".to_string(),
                reason: "required when dry_run is false".to_string(),
            },
            "Apply requires the plan revision returned by a current dry run.",
        ));
    }
    if !schematic.exists() || !board.exists() {
        let missing = if !schematic.exists() {
            &schematic
        } else {
            &board
        };
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::FileNotFound {
                path: missing.display().to_string(),
            },
            format!("{} does not exist", missing.display()),
        ));
    }

    let hierarchy = match saved_hierarchy_files(&schematic) {
        Ok(files) => files,
        Err(error) => {
            return Ok(conflict_result(format!(
                "saved schematic preflight failed: {error:#}"
            )))
        }
    };
    let temp = tempfile::Builder::new().suffix(".net").tempfile()?;
    if let Err(error) =
        super::cli::export_netlist(&ctx.config.kicad_cli, &schematic, temp.path(), "kicadsexpr")
            .await
    {
        return Ok(conflict_result(format!(
            "KiCad netlist export failed: {error:#}"
        )));
    }
    let netlist_source = match std::fs::read_to_string(temp.path()) {
        Ok(source) => source,
        Err(error) => {
            return Ok(conflict_result(format!(
                "KiCad netlist export could not be read: {error}"
            )))
        }
    };
    let mut design = match parse_exported_netlist(&netlist_source) {
        Ok(design) => design,
        Err(error) => {
            return Ok(conflict_result(format!(
                "netlist preflight failed: {error:#}"
            )))
        }
    };
    if let Err(error) = apply_saved_symbol_flags(&hierarchy, &mut design) {
        return Ok(conflict_result(format!(
            "schematic flag preflight failed: {error:#}"
        )));
    }

    let what = if dry_run {
        "PCB sync dry run"
    } else {
        "PCB sync apply"
    };
    let ipc_board = board.clone();
    let library_board = board.clone();
    let result = attempt_ipc_write(
        ctx.config.ipc_address.clone(),
        &board,
        what,
        move |client| {
            let snapshot = snapshot_board(client, &ipc_board)?;
            let mut plan = plan_sync(&netlist_source, &design, &snapshot.state);
            let prepared = match prepare_additions(&library_board, &plan) {
                Ok(prepared) => prepared,
                Err(error) => {
                    plan.status = PlanStatus::Conflict;
                    plan.counts.added.planned = 0;
                    plan.counts.updated.planned = 0;
                    plan.counts.pads_reassigned.planned = 0;
                    plan.counts.conflicts.planned += 1;
                    plan.diagnostics.push(conflict(
                        "footprint_library_resolution_failed",
                        format!("{error:#}"),
                        None,
                    ));
                    plan.changes.clear();
                    return Ok(sync_response(&plan, "conflict", hierarchy.len(), false));
                }
            };
            restage_additions(&mut plan, &prepared, snapshot.state.bounds);
            refresh_revision_with_staging(&mut plan);

            if dry_run || plan.status == PlanStatus::Conflict {
                let status = match plan.status {
                    PlanStatus::Ready => "ready",
                    PlanStatus::Noop => "noop",
                    PlanStatus::Conflict => "conflict",
                };
                return Ok(sync_response(&plan, status, hierarchy.len(), false));
            }
            if expected_revision.as_deref() != Some(plan.plan_revision.as_str()) {
                plan.status = PlanStatus::Conflict;
                plan.counts.conflicts.planned += 1;
                plan.diagnostics.push(conflict(
                    "stale_plan_revision",
                    "The live board or saved schematic changed; rerun dry run and apply its new plan revision."
                        .to_string(),
                    None,
                ));
                plan.changes.clear();
                return Ok(sync_response(&plan, "conflict", hierarchy.len(), false));
            }
            if plan.status == PlanStatus::Noop {
                return Ok(sync_response(&plan, "noop", hierarchy.len(), false));
            }

            let (creates, updates) = build_mutation_items(client, &plan, &prepared, &snapshot)?;
            // What we are about to send, so the board can be held to it.
            let expected = footprint_shapes(creates.iter().chain(updates.iter()));
            client.run_commit("Update PCB from saved schematic", |client| {
                client.create_items_in(snapshot.document.clone(), creates)?;
                client.update_items_in(snapshot.document.clone(), updates)?;
                Ok(())
            })?;
            for detail in verify_board_matches_what_was_sent(client, &snapshot.document, &expected)?
            {
                plan.diagnostics.push(conflict(
                    "board_readback_differs",
                    format!(
                        "the board KiCad wrote differs from what was sent — {detail}. \
                         No pad was invented, so this is reported rather than \
                         refused; check the footprint before relying on it."
                    ),
                    None,
                ));
            }
            plan.counts.added.applied = plan.counts.added.planned;
            plan.counts.updated.applied = plan.counts.updated.planned;
            plan.counts.pads_reassigned.applied = plan.counts.pads_reassigned.planned;
            plan.counts.board_only_preserved.applied =
                plan.counts.board_only_preserved.planned;
            plan.counts.skipped_by_flag.applied = plan.counts.skipped_by_flag.planned;
            Ok(sync_response(&plan, "applied", hierarchy.len(), true))
        },
    )
    .await?;

    Ok(match result {
        BoardWrite::Ipc(result) => result,
        BoardWrite::File => conflict_result(
            "KiCad IPC is unreachable. update_pcb_from_schematic is live-IPC-only and never edits the board file directly. Open the requested board in KiCad and retry."
                .to_string(),
        ),
        BoardWrite::Refused(result) => {
            let message = result
                .content
                .into_iter()
                .find_map(|content| match content {
                    ToolContent::Text { text } => Some(text),
                    _ => None,
                })
                .unwrap_or_else(|| "KiCad refused the sync request".to_string());
            conflict_result(message)
        }
    })
}

fn sync_response(
    plan: &SyncPlan,
    status: &str,
    hierarchy_files: usize,
    applied: bool,
) -> CallToolResult {
    let value = serde_json::json!({
        "status": status,
        "plan_revision": plan.plan_revision,
        "coverage": {
            "source": "saved_schematic_hierarchy",
            "hierarchy_files": hierarchy_files,
            "transport": "live_kicad_ipc",
            "atomicity": "single_kicad_undo_commit",
            "footprints_added": plan.counts.added,
            "footprints_updated": plan.counts.updated,
            "pads_reassigned": plan.counts.pads_reassigned,
            "board_only_preserved": plan.counts.board_only_preserved,
            "skipped_by_flag": plan.counts.skipped_by_flag,
            "conflicts": plan.counts.conflicts
        },
        "changes": plan.changes,
        "diagnostics": plan.diagnostics,
        "undo": if applied { Some("Ctrl-Z reverses the whole schematic-to-PCB update.") } else { None }
    });
    CallToolResult::json(&value)
}

fn conflict_result(message: String) -> CallToolResult {
    let value = serde_json::json!({
        "status": "conflict",
        "coverage": {
            "transport": "live_kicad_ipc",
            "footprints_added": CountPair::default(),
            "footprints_updated": CountPair::default(),
            "pads_reassigned": CountPair::default(),
            "board_only_preserved": CountPair::default(),
            "skipped_by_flag": CountPair::default(),
            "conflicts": CountPair { planned: 1, applied: 0 }
        },
        "diagnostics": [{ "code": "preflight_conflict", "message": message }]
    });
    CallToolResult {
        content: vec![ToolContent::Text {
            text: value.to_string(),
        }],
        is_error: true,
    }
}

fn invalid_argument_result(field: &str, reason: &str, message: &str) -> CallToolResult {
    CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.to_string(),
        },
        message.to_string(),
    )
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_rebind_request(
    args: &serde_json::Value,
) -> std::result::Result<IdentityRebindArgs, CallToolResult> {
    let schematic = args
        .get("schematic")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            invalid_argument_result(
                "schematic",
                "required non-empty string",
                "schematic must be a non-empty path.",
            )
        })?;
    let board = args
        .get("board")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            invalid_argument_result(
                "board",
                "required non-empty string",
                "board must be a non-empty path.",
            )
        })?;
    let references = args
        .get("references")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            invalid_argument_result(
                "references",
                "required non-empty array of unique non-empty strings",
                "references must be a non-empty array of unique non-empty strings.",
            )
        })?;
    if references.is_empty() {
        return Err(invalid_argument_result(
            "references",
            "required non-empty array of unique non-empty strings",
            "references must be a non-empty array of unique non-empty strings.",
        ));
    }
    let mut parsed_references = Vec::with_capacity(references.len());
    let mut seen = HashSet::new();
    for reference in references {
        let reference = reference
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                invalid_argument_result(
                    "references",
                    "required non-empty array of unique non-empty strings",
                    "references must be a non-empty array of unique non-empty strings.",
                )
            })?;
        if !seen.insert(reference) {
            return Err(invalid_argument_result(
                "references",
                "duplicate reference",
                "references must not contain duplicates.",
            ));
        }
        parsed_references.push(reference.to_string());
    }

    let dry_run = match args.get("dry_run") {
        Some(value) => value.as_bool().ok_or_else(|| {
            invalid_argument_result(
                "dry_run",
                "expected boolean",
                "dry_run must be a boolean when provided.",
            )
        })?,
        None => true,
    };
    let expected_plan_revision = match args.get("expected_plan_revision") {
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    invalid_argument_result(
                        "expected_plan_revision",
                        "required non-empty string",
                        "expected_plan_revision must be a non-empty string when provided.",
                    )
                })?
                .to_string(),
        ),
        None => None,
    };
    if !dry_run && expected_plan_revision.is_none() {
        return Err(invalid_argument_result(
            "expected_plan_revision",
            "required when dry_run is false",
            "Apply requires the plan revision returned by a current dry run.",
        ));
    }

    Ok(IdentityRebindArgs {
        schematic,
        board,
        references: parsed_references,
        dry_run,
        expected_plan_revision,
    })
}

#[allow(dead_code)]
fn observe_rebind_paths(request: &IdentityRebindArgs) -> RebindPathFacts {
    RebindPathFacts {
        schematic: request.schematic.clone(),
        schematic_exists: request.schematic.exists(),
        board: request.board.clone(),
        board_exists: request.board.exists(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn require_rebind_paths(facts: &RebindPathFacts) -> std::result::Result<(), CallToolResult> {
    if !facts.schematic_exists {
        return Err(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::FileNotFound {
                path: facts.schematic.display().to_string(),
            },
            format!("{} does not exist", facts.schematic.display()),
        ));
    }
    if !facts.board_exists {
        return Err(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::FileNotFound {
                path: facts.board.display().to_string(),
            },
            format!("{} does not exist", facts.board.display()),
        ));
    }
    Ok(())
}

fn tool_result_text(result: &CallToolResult) -> Option<&str> {
    result.content.iter().find_map(|content| match content {
        ToolContent::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

fn rebind_preflight_conflict_result(message: String) -> CallToolResult {
    let value = serde_json::json!({
        "status": "conflict",
        "coverage": {
            "transport": "live_kicad_ipc"
        },
        "changes": [],
        "diagnostics": [{ "code": "preflight_conflict", "message": message }],
        "undo": serde_json::Value::Null
    });
    CallToolResult {
        content: vec![ToolContent::Text {
            text: value.to_string(),
        }],
        is_error: true,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn require_live_rebind_ipc<T>(result: BoardWrite<T>) -> std::result::Result<T, CallToolResult> {
    match result {
        BoardWrite::Ipc(value) => Ok(value),
        BoardWrite::File => Err(rebind_preflight_conflict_result(
            "KiCad IPC is unreachable. rebind_pcb_schematic_identities is live-IPC-only and never edits the board file directly. Open the requested board in KiCad and retry."
                .to_string(),
        )),
        BoardWrite::Refused(result) => Err(rebind_preflight_conflict_result(
            tool_result_text(&result)
                .unwrap_or("KiCad refused the rebind request")
                .to_string(),
        )),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn identity_rebind_response(
    plan: &IdentityRebindPlan,
    hierarchy_files: usize,
    undo: Option<&str>,
) -> CallToolResult {
    let status = match plan.status {
        PlanStatus::Ready => "ready",
        PlanStatus::Noop => "noop",
        PlanStatus::Conflict => "conflict",
    };
    let value = serde_json::json!({
        "status": status,
        "plan_revision": plan.plan_revision,
        "coverage": {
            "source": "saved_schematic_hierarchy",
            "hierarchy_files": hierarchy_files,
            "transport": "live_kicad_ipc",
            "atomicity": "single_kicad_undo_commit",
            "requested": plan.counts.requested,
            "eligible": plan.counts.eligible,
            "planned": plan.counts.planned,
            "applied": plan.counts.applied,
            "conflicts": plan.counts.conflicts
        },
        "changes": plan.changes,
        "diagnostics": plan.diagnostics,
        "undo": undo
    });
    CallToolResult::json(&value)
}

fn plan_sync(netlist_source: &str, design: &ExportedDesign, board: &BoardState) -> SyncPlan {
    let mut diagnostics = Vec::new();
    let mut counts = SyncCounts::default();
    let mut changes = Vec::new();
    let mut board_by_path = HashMap::new();
    let mut board_by_reference = HashMap::new();

    for (index, footprint) in board.footprints.iter().enumerate() {
        if board_by_reference
            .insert(footprint.reference.as_str(), index)
            .is_some()
        {
            diagnostics.push(conflict(
                "duplicate_board_reference",
                format!("board contains duplicate reference {}", footprint.reference),
                Some(&footprint.reference),
            ));
        }
        if let Some(path) = footprint.symbol_path.as_deref() {
            if board_by_path.insert(path, index).is_some() {
                diagnostics.push(conflict(
                    "duplicate_board_identity",
                    format!("board contains duplicate schematic identity {path}"),
                    Some(&footprint.reference),
                ));
            }
        }
    }

    let mut matched = std::collections::HashSet::new();
    let mut design_references = std::collections::HashSet::new();
    let mut design_paths = std::collections::HashSet::new();
    let staging_x = board.bounds.max_x + 10.0;
    let mut add_index = 0usize;

    let mut skipped_references = HashSet::new();
    let mut skipped_paths = HashSet::new();
    for skipped in &design.skipped {
        if !skipped_references.insert(skipped.reference.as_str())
            || !skipped_paths.insert(skipped.symbol_path.as_str())
        {
            diagnostics.push(conflict(
                "duplicate_skipped_identity",
                format!(
                    "on_board=no instance {} has a duplicate reference or identity",
                    skipped.reference
                ),
                Some(&skipped.reference),
            ));
            continue;
        }
        counts.skipped_by_flag.planned += 1;
        let existing = board_by_path
            .get(skipped.symbol_path.as_str())
            .copied()
            .or_else(|| board_by_reference.get(skipped.reference.as_str()).copied());
        if let Some(index) = existing {
            matched.insert(index);
            diagnostics.push(conflict(
                "on_board_exclusion_conflict",
                format!(
                    "{} is marked on_board=no but already exists on the board",
                    skipped.reference
                ),
                Some(&skipped.reference),
            ));
        }
    }

    for component in &design.components {
        if !design_references.insert(component.reference.as_str()) {
            diagnostics.push(conflict(
                "duplicate_schematic_reference",
                format!(
                    "schematic export contains duplicate reference {}",
                    component.reference
                ),
                Some(&component.reference),
            ));
            continue;
        }
        if !design_paths.insert(component.symbol_path.as_str()) {
            diagnostics.push(conflict(
                "duplicate_schematic_identity",
                format!(
                    "schematic export contains duplicate identity {}",
                    component.symbol_path
                ),
                Some(&component.reference),
            ));
            continue;
        }

        let matched_index = board_by_path
            .get(component.symbol_path.as_str())
            .copied()
            .or_else(|| {
                board_by_reference
                    .get(component.reference.as_str())
                    .copied()
                    .filter(|index| board.footprints[*index].symbol_path.is_none())
            });

        let Some(index) = matched_index else {
            if let Some(index) = board_by_reference
                .get(component.reference.as_str())
                .copied()
            {
                diagnostics.push(conflict(
                    "reference_identity_conflict",
                    format!(
                        "reference {} belongs to a different schematic identity on the board",
                        component.reference
                    ),
                    Some(&board.footprints[index].reference),
                ));
                continue;
            }
            let possible_renames = board
                .footprints
                .iter()
                .enumerate()
                .filter(|(index, footprint)| {
                    !matched.contains(index)
                        && footprint.symbol_path.is_none()
                        && !footprint.not_in_schematic
                        && footprint.footprint_id == component.footprint_id
                        && footprint.value == component.value
                })
                .collect::<Vec<_>>();
            if !possible_renames.is_empty() {
                diagnostics.push(conflict(
                    "reference_only_rename_ambiguous",
                    format!(
                        "{} has no stable board identity and could be a rename of {}; link or resolve the identity in KiCad",
                        component.reference,
                        possible_renames
                            .iter()
                            .map(|(_, footprint)| footprint.reference.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    Some(&component.reference),
                ));
                continue;
            }
            let position = Point {
                x: staging_x,
                y: board.bounds.min_y + add_index as f64 * 10.0,
            };
            add_index += 1;
            changes.push(PlannedChange::Add {
                reference: component.reference.clone(),
                value: component.value.clone(),
                footprint_id: component.footprint_id.clone(),
                symbol_path: component.symbol_path.clone(),
                dnp: component.dnp,
                pad_nets: component.pad_nets.clone(),
                position,
            });
            counts.added.planned += 1;
            continue;
        };

        matched.insert(index);
        let footprint = &board.footprints[index];
        if footprint.reference != component.reference {
            if let Some(other_index) = board_by_reference
                .get(component.reference.as_str())
                .copied()
                .filter(|other_index| *other_index != index)
            {
                diagnostics.push(conflict(
                    "reference_rename_collision",
                    format!(
                        "cannot rename {} to {} because that reference belongs to board footprint {}",
                        footprint.reference,
                        component.reference,
                        board.footprints[other_index].kiid
                    ),
                    Some(&component.reference),
                ));
                continue;
            }
        }
        if footprint.footprint_id != component.footprint_id {
            diagnostics.push(conflict(
                "footprint_id_changed",
                format!(
                    "{} uses {} on the board but {} in the schematic",
                    component.reference, footprint.footprint_id, component.footprint_id
                ),
                Some(&component.reference),
            ));
            continue;
        }

        let mut changed_pads = 0usize;
        let pad_numbers = component
            .pad_nets
            .keys()
            .chain(footprint.pad_nets.keys())
            .collect::<std::collections::BTreeSet<_>>();
        for number in pad_numbers {
            let new_net = component
                .pad_nets
                .get(number)
                .map(String::as_str)
                .unwrap_or("");
            let old_net = footprint
                .pad_nets
                .get(number)
                .map(String::as_str)
                .unwrap_or("");
            if old_net == new_net {
                continue;
            }
            if board.routed_nets.contains_key(old_net) || board.routed_nets.contains_key(new_net) {
                diagnostics.push(conflict(
                    "routed_pad_net_change",
                    format!(
                        "{} pad {} would change from '{}' to '{}' while routed copper uses that net",
                        component.reference, number, old_net, new_net
                    ),
                    Some(&component.reference),
                ));
            } else {
                changed_pads += 1;
            }
        }

        let needs_update = footprint.reference != component.reference
            || footprint.value != component.value
            || footprint.symbol_path.as_deref() != Some(component.symbol_path.as_str())
            || footprint.dnp != component.dnp
            || changed_pads > 0;
        if needs_update {
            changes.push(PlannedChange::Update {
                kiid: footprint.kiid.clone(),
                reference: component.reference.clone(),
                value: component.value.clone(),
                symbol_path: component.symbol_path.clone(),
                dnp: component.dnp,
                pad_nets: component.pad_nets.clone(),
                preserve: PreservedBoardState {
                    position: footprint.position,
                    rotation: footprint.rotation,
                    layer: footprint.layer.clone(),
                    locked: footprint.locked,
                },
            });
            counts.updated.planned += 1;
            counts.pads_reassigned.planned += changed_pads;
        }
    }

    counts.board_only_preserved.planned = board.footprints.len() - matched.len();
    counts.conflicts.planned = diagnostics.len();
    if !diagnostics.is_empty() {
        changes.clear();
        counts.added.planned = 0;
        counts.updated.planned = 0;
        counts.pads_reassigned.planned = 0;
    }
    let status = if !diagnostics.is_empty() {
        PlanStatus::Conflict
    } else if changes.is_empty() {
        PlanStatus::Noop
    } else {
        PlanStatus::Ready
    };
    let plan_revision = plan_revision(netlist_source, board);
    SyncPlan {
        status,
        plan_revision,
        counts,
        changes,
        diagnostics,
    }
}

fn conflict(code: &str, message: String, reference: Option<&str>) -> SyncDiagnostic {
    SyncDiagnostic {
        code: code.to_string(),
        message,
        reference: reference.map(str::to_string),
    }
}

fn plan_identity_rebind(
    netlist_source: &str,
    design: &ExportedDesign,
    board: &BoardState,
    requested: &[String],
) -> IdentityRebindPlan {
    let mut diagnostics = Vec::new();
    let mut counts = IdentityRebindCounts {
        requested: requested.len(),
        ..Default::default()
    };

    let mut requested_references = requested.to_vec();
    requested_references.sort();
    for pair in requested_references.windows(2) {
        if pair[0] == pair[1] {
            diagnostics.push(conflict(
                "duplicate_requested_reference",
                format!("requested reference {} appears more than once", pair[0]),
                Some(&pair[0]),
            ));
        }
    }

    let design_by_reference = design
        .components
        .iter()
        .map(|component| (component.reference.as_str(), component))
        .collect::<HashMap<_, _>>();
    let duplicate_design_references =
        design
            .components
            .iter()
            .fold(HashMap::<&str, usize>::new(), |mut counts, component| {
                *counts.entry(component.reference.as_str()).or_insert(0) += 1;
                counts
            });
    let mut board_by_reference = HashMap::new();
    for footprint in &board.footprints {
        board_by_reference
            .entry(footprint.reference.as_str())
            .or_insert_with(Vec::new)
            .push(footprint);
    }
    let requested_set = requested_references
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut board_identity_owner = HashMap::new();
    for footprint in &board.footprints {
        if let Some(path) = footprint.symbol_path.as_deref() {
            board_identity_owner
                .entry(path)
                .or_insert_with(Vec::new)
                .push(footprint.reference.as_str());
        }
    }
    let mut schematic_identity_owner = HashMap::new();
    for component in &design.components {
        schematic_identity_owner
            .entry(component.symbol_path.as_str())
            .or_insert_with(Vec::new)
            .push(component.reference.as_str());
    }

    let mut changes = Vec::new();
    let mut prospective_changes = Vec::new();
    let mut matching_references = Vec::new();

    for reference in &requested_references {
        let Some(component) = design_by_reference.get(reference.as_str()) else {
            diagnostics.push(conflict(
                "requested_reference_missing_from_schematic",
                format!("requested reference {reference} is missing from the schematic"),
                Some(reference),
            ));
            continue;
        };
        if duplicate_design_references
            .get(reference.as_str())
            .is_some_and(|count| *count > 1)
        {
            diagnostics.push(conflict(
                "duplicate_schematic_reference",
                format!("requested reference {reference} appears more than once in the schematic"),
                Some(reference),
            ));
            continue;
        }
        let Some(board_matches) = board_by_reference.get(reference.as_str()) else {
            diagnostics.push(conflict(
                "requested_reference_missing_from_board",
                format!("requested reference {reference} is missing from the board"),
                Some(reference),
            ));
            continue;
        };
        let [footprint] = board_matches.as_slice() else {
            diagnostics.push(conflict(
                "requested_reference_missing_from_board",
                format!("requested reference {reference} is ambiguous on the board"),
                Some(reference),
            ));
            continue;
        };
        let prospective_change = IdentityRebindChange {
            reference: reference.clone(),
            kiid: footprint.kiid.clone(),
            old_symbol_path: footprint.symbol_path.clone().unwrap_or_default(),
            new_symbol_path: component.symbol_path.clone(),
            value: component.value.clone(),
            footprint_id: component.footprint_id.clone(),
            dnp: component.dnp,
            pad_nets: normalize_pad_nets(&component.pad_nets),
            preserve: PreservedBoardState {
                position: footprint.position,
                rotation: footprint.rotation,
                layer: footprint.layer.clone(),
                locked: footprint.locked,
            },
        };
        prospective_changes.push(prospective_change.clone());

        if footprint.not_in_schematic {
            diagnostics.push(conflict(
                "board_only_footprint",
                format!("board reference {reference} is marked not_in_schematic"),
                Some(reference),
            ));
            continue;
        }
        let Some(old_symbol_path) = footprint.symbol_path.as_deref() else {
            diagnostics.push(conflict(
                "missing_board_identity",
                format!("board reference {reference} has no schematic identity"),
                Some(reference),
            ));
            continue;
        };
        if component.symbol_path.is_empty() {
            diagnostics.push(conflict(
                "missing_schematic_identity",
                format!("schematic reference {reference} has no schematic identity"),
                Some(reference),
            ));
            continue;
        }
        if board_identity_owner
            .get(old_symbol_path)
            .is_some_and(|owners| owners.len() > 1)
        {
            diagnostics.push(conflict(
                "duplicate_board_identity",
                format!(
                    "board schematic identity {old_symbol_path} is used by more than one footprint"
                ),
                Some(reference),
            ));
            continue;
        }
        if schematic_identity_owner
            .get(component.symbol_path.as_str())
            .is_some_and(|owners| owners.len() > 1)
        {
            diagnostics.push(conflict(
                "duplicate_schematic_identity",
                format!(
                    "schematic identity {} is used by more than one component",
                    component.symbol_path
                ),
                Some(reference),
            ));
            continue;
        }
        if !valid_symbol_path(old_symbol_path) {
            diagnostics.push(conflict(
                "invalid_board_identity",
                format!("board reference {reference} has an invalid schematic identity"),
                Some(reference),
            ));
            continue;
        }
        if !valid_symbol_path(&component.symbol_path) {
            diagnostics.push(conflict(
                "invalid_schematic_identity",
                format!("schematic reference {reference} has an invalid schematic identity"),
                Some(reference),
            ));
            continue;
        }
        if footprint.value != component.value {
            diagnostics.push(conflict(
                "value_mismatch",
                format!(
                    "requested reference {reference} has board value {} but schematic value {}",
                    footprint.value, component.value
                ),
                Some(reference),
            ));
            continue;
        }
        if footprint.footprint_id != component.footprint_id {
            diagnostics.push(conflict(
                "footprint_mismatch",
                format!(
                    "requested reference {reference} has board footprint {} but schematic footprint {}",
                    footprint.footprint_id, component.footprint_id
                ),
                Some(reference),
            ));
            continue;
        }
        if footprint.dnp != component.dnp {
            diagnostics.push(conflict(
                "dnp_mismatch",
                format!(
                    "requested reference {reference} has board dnp {} but schematic dnp {}",
                    footprint.dnp, component.dnp
                ),
                Some(reference),
            ));
            continue;
        }
        if footprint.pad_nets.len() != component.pad_nets.len()
            || footprint.pad_nets.keys().collect::<Vec<_>>()
                != component.pad_nets.keys().collect::<Vec<_>>()
        {
            diagnostics.push(conflict(
                "pad_set_mismatch",
                format!("requested reference {reference} has a different pad set on the board"),
                Some(reference),
            ));
            continue;
        }
        let normalized_board_pads = normalize_pad_nets(&footprint.pad_nets);
        let normalized_component_pads = normalize_pad_nets(&component.pad_nets);
        if normalized_board_pads != normalized_component_pads {
            diagnostics.push(conflict(
                "pad_net_mismatch",
                format!("requested reference {reference} has a different logical pad-net mapping"),
                Some(reference),
            ));
            continue;
        }
        if old_symbol_path == component.symbol_path {
            matching_references.push(reference.clone());
            continue;
        }
        if board
            .footprints
            .iter()
            .filter(|candidate| !requested_set.contains(candidate.reference.as_str()))
            .any(|candidate| {
                candidate.symbol_path.as_deref() == Some(component.symbol_path.as_str())
            })
        {
            diagnostics.push(conflict(
                "target_identity_in_use",
                format!(
                    "schematic identity {} is already used by an unrequested board footprint",
                    component.symbol_path
                ),
                Some(reference),
            ));
            continue;
        }

        counts.eligible += 1;
        changes.push(prospective_change);
    }

    if !matching_references.is_empty() && !changes.is_empty() {
        for reference in matching_references {
            diagnostics.push(conflict(
                "identity_already_matches_in_mixed_request",
                format!(
                    "requested reference {reference} already matches the schematic identity while other requested references still need rebinding"
                ),
                Some(&reference),
            ));
        }
    }

    changes.sort_by(|a, b| a.reference.cmp(&b.reference));
    prospective_changes.sort_by(|a, b| a.reference.cmp(&b.reference));
    counts.planned = changes.len();
    counts.conflicts = diagnostics.len();
    if !diagnostics.is_empty() {
        changes.clear();
        counts.planned = 0;
    }
    let status = if !diagnostics.is_empty() {
        PlanStatus::Conflict
    } else if changes.is_empty() {
        PlanStatus::Noop
    } else {
        PlanStatus::Ready
    };
    let plan_revision = identity_rebind_plan_revision(
        netlist_source,
        &requested_references,
        &prospective_changes,
        board,
    );
    IdentityRebindPlan {
        status,
        plan_revision,
        counts,
        changes,
        diagnostics,
    }
}

fn normalize_pad_nets(pad_nets: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    pad_nets
        .iter()
        .map(|(pad, net)| (pad.clone(), normalize_root_net(net)))
        .collect()
}

fn normalize_root_net(net: &str) -> String {
    if let Some(stripped) = net.strip_prefix('/') {
        if !stripped.is_empty() && !stripped.contains('/') {
            return stripped.to_string();
        }
    }
    net.to_string()
}

fn valid_symbol_path(path: &str) -> bool {
    let Some(remainder) = path.strip_prefix('/') else {
        return false;
    };
    if remainder.is_empty() || remainder.ends_with('/') {
        return false;
    }
    remainder
        .split('/')
        .all(|segment| !segment.is_empty() && uuid::Uuid::parse_str(segment).is_ok())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct IdentityRebindRevisionSnapshot {
    requested: Vec<String>,
    board: Vec<IdentityRebindRevisionBoardSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct IdentityRebindRevisionBoardSnapshot {
    reference: String,
    kiid: String,
    symbol_path: String,
    value: String,
    footprint_id: String,
    dnp: bool,
    not_in_schematic: bool,
    pad_nets: BTreeMap<String, String>,
    position_x: String,
    position_y: String,
    rotation: String,
    layer: String,
    locked: bool,
}

fn identity_rebind_plan_revision(
    netlist_source: &str,
    requested: &[String],
    changes: &[IdentityRebindChange],
    board: &BoardState,
) -> String {
    let board_by_reference = board
        .footprints
        .iter()
        .map(|footprint| (footprint.reference.as_str(), footprint))
        .collect::<HashMap<_, _>>();
    let mut snapshot = IdentityRebindRevisionSnapshot {
        requested: requested.to_vec(),
        board: Vec::new(),
    };
    for reference in requested {
        if let Some(footprint) = board_by_reference.get(reference.as_str()) {
            snapshot.board.push(IdentityRebindRevisionBoardSnapshot {
                reference: reference.clone(),
                kiid: footprint.kiid.clone(),
                symbol_path: footprint.symbol_path.clone().unwrap_or_default(),
                value: footprint.value.clone(),
                footprint_id: footprint.footprint_id.clone(),
                dnp: footprint.dnp,
                not_in_schematic: footprint.not_in_schematic,
                pad_nets: normalize_pad_nets(&footprint.pad_nets),
                position_x: footprint.position.x.to_string(),
                position_y: footprint.position.y.to_string(),
                rotation: footprint.rotation.to_string(),
                layer: footprint.layer.clone(),
                locked: footprint.locked,
            });
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(netlist_identity(netlist_source));
    hasher.update(serde_json::to_vec(&snapshot).expect("rebind snapshot serializes"));
    hasher.update(serde_json::to_vec(changes).expect("rebind changes serialize"));
    format!("{:x}", hasher.finalize())
}

/// A stable identity for the design-bearing netlist sections.
///
/// `kicad-cli sch export netlist` stamps `(date "…T14:48:16")` and the
/// exporting tool's version into every export, so hashing the raw source
/// yields a different revision **every second** for a design nobody touched —
/// and since apply requires the revision a dry run returned, apply could only
/// ever succeed if both calls landed inside the same wall-clock second. That
/// is a race, not a guarantee: it passes on a fast machine and fails on a
/// human reviewing the plan first, which is the whole point of the plan.
///
/// The revision must cover what the plan *read*: the complete top-level
/// `components` and `nets` trees. Hashing those trees structurally ignores the
/// volatile header without confusing nested nodes or quoted text for header
/// metadata.
fn netlist_identity(netlist_source: &str) -> Vec<u8> {
    let Ok(root) = konnect_sexp::parse_sexp(netlist_source) else {
        // Production reaches this function only after successful netlist
        // parsing. Keeping invalid synthetic planner inputs distinct makes the
        // pure planner tests useful without creating a second error path here.
        return netlist_source.as_bytes().to_vec();
    };

    let mut identity = Vec::new();
    for tag in ["components", "nets"] {
        match root.find(tag) {
            Some(node) => {
                identity.push(1);
                append_sexp_identity(node, &mut identity);
            }
            None => identity.push(0),
        }
    }
    identity
}

fn append_sexp_identity(node: &SexpNode, identity: &mut Vec<u8>) {
    match node {
        SexpNode::Atom(value) => {
            identity.push(0);
            append_identity_bytes(value.as_bytes(), identity);
        }
        SexpNode::Str(value) => {
            identity.push(1);
            append_identity_bytes(value.as_bytes(), identity);
        }
        SexpNode::List(children) => {
            identity.push(2);
            identity.extend_from_slice(&(children.len() as u64).to_le_bytes());
            for child in children {
                append_sexp_identity(child, identity);
            }
        }
    }
}

fn append_identity_bytes(value: &[u8], identity: &mut Vec<u8>) {
    identity.extend_from_slice(&(value.len() as u64).to_le_bytes());
    identity.extend_from_slice(value);
}

fn plan_revision(netlist_source: &str, board: &BoardState) -> String {
    let mut footprints = board.footprints.iter().collect::<Vec<_>>();
    footprints.sort_by(|a, b| a.kiid.cmp(&b.kiid));
    let mut hasher = Sha256::new();
    hasher.update(netlist_identity(netlist_source));
    hasher.update(serde_json::to_vec(&board.bounds).expect("bounds serialize"));
    for footprint in footprints {
        hasher.update(footprint.kiid.as_bytes());
        hasher.update(footprint.reference.as_bytes());
        hasher.update(footprint.footprint_id.as_bytes());
        hasher.update(footprint.symbol_path.as_deref().unwrap_or("").as_bytes());
        for (pad, net) in &footprint.pad_nets {
            hasher.update(pad.as_bytes());
            hasher.update(net.as_bytes());
        }
    }
    for (net, count) in &board.routed_nets {
        hasher.update(net.as_bytes());
        hasher.update(count.to_le_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn refresh_revision_with_staging(plan: &mut SyncPlan) {
    let mut hasher = Sha256::new();
    hasher.update(plan.plan_revision.as_bytes());
    hasher.update(serde_json::to_vec(&plan.changes).expect("planned changes serialize"));
    plan.plan_revision = format!("{:x}", hasher.finalize());
}

fn parse_exported_netlist(source: &str) -> Result<ExportedDesign> {
    let root = konnect_sexp::parse_sexp(source).context("invalid KiCad netlist S-expression")?;
    let components_node = root
        .find("components")
        .context("KiCad netlist has no components section")?;

    let mut components = Vec::new();
    let mut by_reference = HashMap::new();
    for component_node in components_node.find_all("comp") {
        let reference = required_value(component_node, "ref")?;
        if by_reference.contains_key(&reference) {
            bail!("KiCad netlist contains duplicate component reference {reference}");
        }

        let sheet_stamp = component_node
            .find("sheetpath")
            .and_then(|sheet| sheet.find_str("tstamps"))
            .context("KiCad netlist component has no sheet timestamp")?;
        let symbol_stamp = component_node
            .find_str("tstamps")
            .context("KiCad netlist component has no symbol timestamp")?;
        let symbol_path = format!(
            "/{}/{}",
            sheet_stamp.trim_matches('/'),
            symbol_stamp.trim_matches('/')
        )
        .replace("//", "/");
        let dnp = component_node.find_all("property").iter().any(|property| {
            property.find_str("name") == Some("dnp")
                || property.get(1).and_then(SexpNode::as_str) == Some("dnp")
        });

        let index = components.len();
        by_reference.insert(reference.clone(), index);
        components.push(DesignComponent {
            reference,
            value: required_value(component_node, "value")?,
            footprint_id: required_value(component_node, "footprint")?,
            symbol_path,
            dnp,
            pad_nets: BTreeMap::new(),
        });
    }

    if components.is_empty() {
        bail!("KiCad netlist contains zero components");
    }

    if let Some(nets_node) = root.find("nets") {
        for net_node in nets_node.find_all("net") {
            let net_name = required_value(net_node, "name")?;
            for node in net_node.find_all("node") {
                let reference = required_value(node, "ref")?;
                let pin = required_value(node, "pin")?;
                let Some(&index) = by_reference.get(&reference) else {
                    bail!("net {net_name} refers to unknown component {reference}");
                };
                if components[index]
                    .pad_nets
                    .insert(pin.clone(), net_name.clone())
                    .is_some()
                {
                    bail!("component {reference} pad {pin} appears in more than one net");
                }
            }
        }
    }

    Ok(ExportedDesign {
        components,
        skipped: Vec::new(),
    })
}

fn required_value(node: &SexpNode, tag: &str) -> Result<String> {
    node.find_str(tag)
        .map(str::to_owned)
        .with_context(|| format!("KiCad netlist node is missing {tag}"))
}

fn update_footprint_item(
    item: &prost_types::Any,
    change: &PlannedChange,
    net_codes: &BTreeMap<String, i32>,
) -> Result<prost_types::Any> {
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    let PlannedChange::Update {
        kiid,
        reference,
        value,
        symbol_path,
        dnp,
        pad_nets,
        ..
    } = change
    else {
        bail!("an add change cannot update an existing footprint");
    };
    let mut footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
        .context("KiCad returned an invalid footprint item")?;
    if footprint.id.as_ref().map(|id| id.value.as_str()) != Some(kiid.as_str()) {
        bail!("planned footprint {kiid} no longer matches the live board item");
    }

    apply_footprint_fields(
        &mut footprint,
        reference,
        value,
        symbol_path,
        *dnp,
        pad_nets,
        net_codes,
    )?;

    Ok(konnect_ipc::builders::pack_any(
        &footprint,
        "kiapi.board.types.FootprintInstance",
    ))
}

fn rebind_footprint_item(
    item: &prost_types::Any,
    change: &IdentityRebindChange,
) -> Result<prost_types::Any> {
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    if !konnect_ipc::builders::any_is(item, "kiapi.board.types.FootprintInstance") {
        bail!("identity rebind requires a FootprintInstance item");
    }

    let footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
        .context("KiCad returned an invalid footprint item")?;
    if footprint.id.as_ref().map(|id| id.value.as_str()) != Some(change.kiid.as_str()) {
        bail!(
            "planned footprint {} no longer matches the live board item",
            change.kiid
        );
    }
    if field_text(&footprint.reference_field) != change.reference {
        bail!(
            "planned footprint {} no longer has reference {}",
            change.kiid,
            change.reference
        );
    }
    let current_path = footprint
        .symbol_path
        .as_ref()
        .map(sheet_path_string)
        .unwrap_or_default();
    if current_path != change.old_symbol_path {
        bail!(
            "planned footprint {} no longer has schematic identity {}",
            change.kiid,
            change.old_symbol_path
        );
    }
    if !valid_symbol_path(&change.new_symbol_path) {
        bail!(
            "planned footprint {} has invalid target schematic identity {}",
            change.kiid,
            change.new_symbol_path
        );
    }

    let mut rebound = footprint.clone();
    rebound.symbol_path = Some(kiapi::common::types::SheetPath {
        path: change
            .new_symbol_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(|segment| kiapi::common::types::Kiid {
                value: segment.to_string(),
            })
            .collect(),
        path_human_readable: String::new(),
    });

    Ok(konnect_ipc::builders::pack_any(
        &rebound,
        "kiapi.board.types.FootprintInstance",
    ))
}

fn canonicalize_identity_rebind_child(
    item: &prost_types::Any,
) -> Result<(String, Vec<u8>, prost_types::Any)> {
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    let type_url = item.type_url.clone();
    let type_name = konnect_ipc::builders::any_type_name(item);
    let value = match type_name {
        "kiapi.board.types.Pad" => {
            let pad = kiapi::board::types::Pad::decode(item.value.as_slice())
                .context("cannot decode pad child for identity rebind canonicalization")?;
            pad.encode_to_vec()
        }
        "kiapi.board.types.BoardGraphicShape" => {
            let shape = kiapi::board::types::BoardGraphicShape::decode(item.value.as_slice())
                .context("cannot decode graphic child for identity rebind canonicalization")?;
            shape.encode_to_vec()
        }
        "kiapi.board.types.BoardText" => {
            let text = kiapi::board::types::BoardText::decode(item.value.as_slice())
                .context("cannot decode text child for identity rebind canonicalization")?;
            text.encode_to_vec()
        }
        "kiapi.board.types.Footprint3DModel" => {
            let model = kiapi::board::types::Footprint3DModel::decode(item.value.as_slice())
                .context("cannot decode model child for identity rebind canonicalization")?;
            model.encode_to_vec()
        }
        "kiapi.board.types.Group" => {
            let group = kiapi::board::types::Group::decode(item.value.as_slice())
                .context("cannot decode group child for identity rebind canonicalization")?;
            group.encode_to_vec()
        }
        _ => item.value.clone(),
    };
    Ok((
        type_url.clone(),
        value.clone(),
        prost_types::Any { type_url, value },
    ))
}

fn canonicalize_footprint_for_identity_rebind(
    item: &prost_types::Any,
    ignore_symbol_path: bool,
) -> Result<konnect_ipc::gen::kiapi::board::types::FootprintInstance> {
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    if !konnect_ipc::builders::any_is(item, "kiapi.board.types.FootprintInstance") {
        bail!("identity rebind requires a FootprintInstance item");
    }
    let mut footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
        .context("KiCad returned an invalid footprint item")?;
    if ignore_symbol_path {
        footprint.symbol_path = None;
    }
    if footprint.orientation == Some(kiapi::common::types::Angle::default()) {
        footprint.orientation = None;
    }
    if footprint.attributes == Some(kiapi::board::types::FootprintAttributes::default()) {
        footprint.attributes = None;
    }
    for field in [
        &mut footprint.datasheet_field,
        &mut footprint.description_field,
    ] {
        if field.as_ref().and_then(|field| field.text.as_ref())
            == Some(&kiapi::board::types::BoardText::default())
        {
            field.as_mut().unwrap().text = None;
        }
        if *field == Some(kiapi::board::types::Field::default()) {
            *field = None;
        }
    }
    if let Some(definition) = footprint.definition.as_mut() {
        if definition.attributes == Some(kiapi::board::types::FootprintAttributes::default()) {
            definition.attributes = None;
        }
        for field in [
            &mut definition.datasheet_field,
            &mut definition.description_field,
        ] {
            if field.as_ref().and_then(|field| field.text.as_ref())
                == Some(&kiapi::board::types::BoardText::default())
            {
                field.as_mut().unwrap().text = None;
            }
            if *field == Some(kiapi::board::types::Field::default()) {
                *field = None;
            }
        }
        let mut keyed = definition
            .items
            .iter()
            .map(canonicalize_identity_rebind_child)
            .collect::<Result<Vec<_>>>()?;
        keyed.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        definition.items = keyed.into_iter().map(|(_, _, item)| item).collect();
    }
    Ok(footprint)
}

fn canonicalize_footprint_bytes_for_identity_rebind(
    item: &prost_types::Any,
    ignore_symbol_path: bool,
) -> Result<Vec<u8>> {
    use prost::Message;

    Ok(canonicalize_footprint_for_identity_rebind(item, ignore_symbol_path)?.encode_to_vec())
}

fn footprint_field_positions_snapshot(
    footprint: &konnect_ipc::gen::kiapi::board::types::FootprintInstance,
) -> Result<Vec<Vec<u8>>> {
    use prost::Message;

    let mut fields = Vec::new();
    for field in [
        footprint.reference_field.as_ref(),
        footprint.value_field.as_ref(),
        footprint.datasheet_field.as_ref(),
        footprint.description_field.as_ref(),
        footprint
            .definition
            .as_ref()
            .and_then(|definition| definition.reference_field.as_ref()),
        footprint
            .definition
            .as_ref()
            .and_then(|definition| definition.value_field.as_ref()),
        footprint
            .definition
            .as_ref()
            .and_then(|definition| definition.datasheet_field.as_ref()),
        footprint
            .definition
            .as_ref()
            .and_then(|definition| definition.description_field.as_ref()),
    ] {
        if let Some(field) = field {
            fields.push(field.encode_to_vec());
        }
    }
    Ok(fields)
}

fn canonical_identity_rebind_children_by_type<T>(
    items: &[prost_types::Any],
    expected_type: &str,
) -> Result<Vec<Vec<u8>>>
where
    T: prost::Message + Default,
{
    let mut values = Vec::new();
    for item in items {
        if !konnect_ipc::builders::any_is(item, expected_type) {
            continue;
        }
        let decoded = T::decode(item.value.as_slice())
            .with_context(|| format!("cannot decode {expected_type}"))?;
        values.push(decoded.encode_to_vec());
    }
    values.sort();
    Ok(values)
}

fn canonical_unknown_identity_rebind_children(
    items: &[prost_types::Any],
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut values = Vec::new();
    for item in items {
        let type_name = konnect_ipc::builders::any_type_name(item);
        if matches!(
            type_name,
            "kiapi.board.types.Pad"
                | "kiapi.board.types.BoardGraphicShape"
                | "kiapi.board.types.BoardText"
                | "kiapi.board.types.Footprint3DModel"
                | "kiapi.board.types.Group"
        ) {
            continue;
        }

        let (type_url, value, _) = canonicalize_identity_rebind_child(item)?;
        values.push((type_url, value));
    }
    values.sort();
    Ok(values)
}

fn verify_rebound_footprint(
    before: &prost_types::Any,
    after: &prost_types::Any,
    expected_new_path: &str,
) -> Result<()> {
    use konnect_ipc::gen::kiapi;

    let canonical_before = canonicalize_footprint_for_identity_rebind(before, false)?;
    let canonical_after = canonicalize_footprint_for_identity_rebind(after, false)?;

    let rebound_path = canonical_after
        .symbol_path
        .as_ref()
        .map(sheet_path_string)
        .unwrap_or_default();
    if rebound_path != expected_new_path {
        bail!("symbol_path changed unexpectedly: expected {expected_new_path}, got {rebound_path}");
    }

    let canonical_before_no_path = canonicalize_footprint_bytes_for_identity_rebind(before, true)?;
    let canonical_after_no_path = canonicalize_footprint_bytes_for_identity_rebind(after, true)?;
    if canonical_before_no_path == canonical_after_no_path {
        return Ok(());
    }

    if canonical_before.position != canonical_after.position {
        bail!("position changed during identity rebind readback");
    }
    if canonical_before.orientation != canonical_after.orientation {
        bail!("orientation changed during identity rebind readback");
    }
    if canonical_before.layer != canonical_after.layer {
        bail!("layer changed during identity rebind readback");
    }
    if canonical_before.locked != canonical_after.locked {
        bail!("locked state changed during identity rebind readback");
    }
    if field_text(&canonical_before.reference_field) != field_text(&canonical_after.reference_field)
    {
        bail!("reference changed during identity rebind readback");
    }
    if field_text(&canonical_before.value_field) != field_text(&canonical_after.value_field) {
        bail!("value changed during identity rebind readback");
    }
    if canonical_before.attributes != canonical_after.attributes
        || canonical_before
            .definition
            .as_ref()
            .and_then(|definition| definition.attributes.as_ref())
            != canonical_after
                .definition
                .as_ref()
                .and_then(|definition| definition.attributes.as_ref())
    {
        bail!("attributes changed during identity rebind readback");
    }
    if canonical_before
        .definition
        .as_ref()
        .and_then(|definition| definition.id.as_ref())
        != canonical_after
            .definition
            .as_ref()
            .and_then(|definition| definition.id.as_ref())
    {
        bail!("footprint definition id changed during identity rebind readback");
    }
    if footprint_field_positions_snapshot(&canonical_before)?
        != footprint_field_positions_snapshot(&canonical_after)?
    {
        bail!("field placement changed during identity rebind readback");
    }

    let before_definition = canonical_before
        .definition
        .as_ref()
        .context("board footprint has no library definition")?;
    let after_definition = canonical_after
        .definition
        .as_ref()
        .context("board footprint has no library definition")?;

    if before_definition.items.len() != after_definition.items.len() {
        bail!("definition item count changed during identity rebind readback");
    }

    let before_pads = canonical_identity_rebind_children_by_type::<kiapi::board::types::Pad>(
        &before_definition.items,
        "kiapi.board.types.Pad",
    )?;
    let after_pads = canonical_identity_rebind_children_by_type::<kiapi::board::types::Pad>(
        &after_definition.items,
        "kiapi.board.types.Pad",
    )?;
    if before_pads != after_pads {
        bail!("pad content changed during identity rebind readback");
    }

    let before_graphics =
        canonical_identity_rebind_children_by_type::<kiapi::board::types::BoardGraphicShape>(
            &before_definition.items,
            "kiapi.board.types.BoardGraphicShape",
        )?;
    let after_graphics =
        canonical_identity_rebind_children_by_type::<kiapi::board::types::BoardGraphicShape>(
            &after_definition.items,
            "kiapi.board.types.BoardGraphicShape",
        )?;
    if before_graphics != after_graphics {
        bail!("graphic content changed during identity rebind readback");
    }

    let before_models =
        canonical_identity_rebind_children_by_type::<kiapi::board::types::Footprint3DModel>(
            &before_definition.items,
            "kiapi.board.types.Footprint3DModel",
        )?;
    let after_models =
        canonical_identity_rebind_children_by_type::<kiapi::board::types::Footprint3DModel>(
            &after_definition.items,
            "kiapi.board.types.Footprint3DModel",
        )?;
    if before_models != after_models {
        bail!("model content changed during identity rebind readback");
    }

    let before_unknown = canonical_unknown_identity_rebind_children(&before_definition.items)?;
    let after_unknown = canonical_unknown_identity_rebind_children(&after_definition.items)?;
    if before_unknown != after_unknown {
        let before_unknown_types = before_unknown
            .iter()
            .map(|(type_url, _)| type_url.clone())
            .collect::<Vec<_>>();
        let after_unknown_types = after_unknown
            .iter()
            .map(|(type_url, _)| type_url.clone())
            .collect::<Vec<_>>();
        if before_unknown_types != after_unknown_types {
            bail!("unknown footprint child type changed during identity rebind readback");
        }
        bail!("unknown footprint child payload changed during identity rebind readback");
    }

    bail!("non-identity footprint state changed during identity rebind readback");
}

#[allow(clippy::too_many_arguments)]
fn apply_footprint_fields(
    footprint: &mut konnect_ipc::gen::kiapi::board::types::FootprintInstance,
    reference: &str,
    value: &str,
    symbol_path: &str,
    dnp: bool,
    pad_nets: &BTreeMap<String, String>,
    net_codes: &BTreeMap<String, i32>,
) -> Result<()> {
    use konnect_ipc::gen::kiapi;

    set_field_text(&mut footprint.reference_field, "Reference", reference);
    set_field_text(&mut footprint.value_field, "Value", value);
    let definition = footprint
        .definition
        .as_mut()
        .context("board footprint has no library definition")?;
    set_field_text(&mut definition.reference_field, "Reference", reference);
    set_field_text(&mut definition.value_field, "Value", value);

    footprint.symbol_path = Some(kiapi::common::types::SheetPath {
        path: symbol_path
            .split('/')
            .filter(|part| !part.is_empty())
            .map(|part| kiapi::common::types::Kiid {
                value: part.to_string(),
            })
            .collect(),
        path_human_readable: String::new(),
    });
    footprint
        .attributes
        .get_or_insert_with(Default::default)
        .do_not_populate = dnp;
    definition
        .attributes
        .get_or_insert_with(Default::default)
        .do_not_populate = dnp;

    let mut seen_pads = std::collections::HashSet::new();
    for child in &mut definition.items {
        // `definition.items` mixes pads, graphics and text in one repeated
        // field, so the type URL is the only sound discriminator. Filtering by
        // "did `Pad::decode` succeed" instead accepted every graphic — proto3
        // skips unrecognised field numbers rather than failing — and the write
        // back below then re-typed each one as a pad, so every footprint this
        // tool touched lost its artwork and gained a nameless pad at (0,0)
        // for each shape it used to have (#244).
        if !konnect_ipc::builders::any_is(child, "kiapi.board.types.Pad") {
            continue;
        }
        // A child that *declares* itself a pad and will not decode is a real
        // failure, not something to skip past silently.
        let mut pad =
            kiapi::board::types::Pad::decode(child.value.as_slice()).with_context(|| {
                format!("footprint {reference} has a pad KiCad sent in a form Konnect cannot read")
            })?;
        seen_pads.insert(pad.number.clone());
        pad.net = pad_nets
            .get(&pad.number)
            .map(|name| kiapi::board::types::Net {
                // Net codes are KiCad-internal. Preserve a resolved live code
                // when one exists; for a schematic-only net, the name is the
                // public identity and lets KiCad create the new board net.
                code: net_codes
                    .get(name)
                    .copied()
                    .map(|value| kiapi::board::types::NetCode { value }),
                name: name.clone(),
            });
        *child = konnect_ipc::builders::pack_any(&pad, "kiapi.board.types.Pad");
    }
    for number in pad_nets.keys() {
        if !seen_pads.contains(number) {
            bail!("footprint {reference} has no pad {number}");
        }
    }

    Ok(())
}

/// How many pads and how many drawn items a footprint carries.
///
/// The two numbers #244 got wrong in opposite directions: every graphic became
/// a pad, so pads went up by exactly the number of drawings, and drawings went
/// to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct FootprintShape {
    pads: usize,
    drawings: usize,
}

/// Tally the pads and drawings of each footprint in a set of packed items,
/// keyed by reference.
fn footprint_shapes<'a>(
    items: impl Iterator<Item = &'a prost_types::Any>,
) -> BTreeMap<String, FootprintShape> {
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    let mut out = BTreeMap::new();
    for item in items {
        let Ok(footprint) = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
        else {
            continue;
        };
        let Some(definition) = footprint.definition.as_ref() else {
            continue;
        };
        let reference = field_text(&footprint.reference_field);
        if reference.is_empty() {
            continue;
        }
        let mut shape = FootprintShape::default();
        for child in &definition.items {
            match konnect_ipc::builders::any_type_name(child) {
                "kiapi.board.types.Pad" => shape.pads += 1,
                "kiapi.board.types.BoardGraphicShape" | "kiapi.board.types.BoardText" => {
                    shape.drawings += 1
                }
                _ => {}
            }
        }
        out.insert(reference, shape);
    }
    out
}

/// Read the board back and hold it to what was just sent.
///
/// `create_items`/`update_items` only confirm that KiCad *accepted* each item,
/// and the counts this tool reports are copied from the plan — so when #244
/// turned every footprint graphic into a nameless pad, KiCad returned ISC_OK
/// for each one and the response said the sync succeeded. Nothing anywhere
/// looked at what actually landed.
///
/// This is a backstop for that class, not for that bug: with the type-URL fix
/// in place it should never fire. `delete_footprint` already re-queries after
/// mutating; this follows it.
///
/// **It fails the call only on a gained pad.** KiCad has no business inventing
/// one, so that is unambiguous and is #244's exact signature. A *drop* in
/// drawings is reported instead of refused, because it has a benign
/// explanation this check cannot yet rule out — KiCad re-creates a footprint's
/// children from the message on deserialize, and if it promotes a `BoardText`
/// child into a `Field` (which this tally deliberately ignores) the count
/// would fall without anything being wrong. Turning a working sync into an
/// error over that is worse than the warning. Tighten it once it has been
/// watched against a live KiCad; see the note on #244.
fn verify_board_matches_what_was_sent(
    client: &konnect_ipc::KiCadIpcClient,
    document: &konnect_ipc::gen::kiapi::common::types::DocumentSpecifier,
    expected: &BTreeMap<String, FootprintShape>,
) -> Result<Vec<String>> {
    use konnect_ipc::gen::kiapi;

    if expected.is_empty() {
        return Ok(Vec::new());
    }
    let items = client.get_items_in(
        document.clone(),
        kiapi::common::types::KiCadObjectType::KotPcbFootprint,
    )?;
    let actual = footprint_shapes(items.iter());

    let mut corrupted = Vec::new();
    let mut suspicious = Vec::new();
    for (reference, want) in expected {
        // A reference the read-back cannot see is its own problem, but not this
        // check's: KiCad may name it differently after a rename, and failing
        // here would turn a successful sync into an error over bookkeeping.
        let Some(got) = actual.get(reference) else {
            continue;
        };
        let detail = format!(
            "{reference}: sent {} pads and {} drawings, board now has {} and {}",
            want.pads, want.drawings, got.pads, got.drawings
        );
        if got.pads > want.pads {
            corrupted.push(detail);
        } else if got != want {
            suspicious.push(detail);
        }
    }
    if !corrupted.is_empty() {
        bail!(
            "KiCad's board gained pads this sync never sent, so the footprints on \
             it are not the ones that were planned — inspect the board and do not \
             save it: {}",
            corrupted.join("; ")
        );
    }
    Ok(suspicious)
}

fn set_field_text(
    field: &mut Option<konnect_ipc::gen::kiapi::board::types::Field>,
    name: &str,
    value: &str,
) {
    let field = field.get_or_insert_with(Default::default);
    field.name = name.to_string();
    let board_text = field.text.get_or_insert_with(Default::default);
    board_text.text.get_or_insert_with(Default::default).text = value.to_string();
}

fn saved_hierarchy_files(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(
        path: &Path,
        seen: &mut HashSet<PathBuf>,
        active: &mut HashSet<PathBuf>,
        files: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("cannot resolve schematic {}", path.display()))?;
        if active.contains(&canonical) {
            bail!(
                "schematic hierarchy contains a cycle at {}",
                canonical.display()
            );
        }
        if !seen.insert(canonical.clone()) {
            return Ok(());
        }
        active.insert(canonical.clone());
        let name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .context("schematic path has no file name")?;
        let lock = canonical.with_file_name(format!("~{name}.lck"));
        if lock.exists() {
            bail!(
                "{} is open in the schematic editor; save and close the hierarchy before syncing",
                canonical.display()
            );
        }
        let schematic = konnect_schematic_editor::Schematic::load(&canonical)
            .with_context(|| format!("cannot load schematic {}", canonical.display()))?;
        files.push(canonical.clone());
        let parent = canonical.parent().unwrap_or_else(|| Path::new("."));
        for sheet in schematic.sheets.iter() {
            let child = parent.join(sheet.file());
            if !child.exists() {
                bail!(
                    "hierarchical sheet {} referenced by {} does not exist",
                    child.display(),
                    canonical.display()
                );
            }
            visit(&child, seen, active, files)?;
        }
        active.remove(&canonical);
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, &mut HashSet::new(), &mut HashSet::new(), &mut files)?;
    Ok(files)
}

fn apply_saved_symbol_flags(files: &[PathBuf], design: &mut ExportedDesign) -> Result<()> {
    #[derive(Debug)]
    struct Flags {
        reference: String,
        symbol_path: String,
        in_bom: bool,
        on_board: bool,
        dnp: bool,
    }

    let mut flags = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(file)?;
        let tree = konnect_sexp::parse_sexp(&source)?;
        let root_uuid = tree.find_str("uuid").unwrap_or("");
        for symbol in tree.find_all("symbol") {
            let Some(uuid) = symbol.find_str("uuid") else {
                continue;
            };
            let in_bom = symbol.find_str("in_bom") != Some("no");
            let on_board = symbol.find_str("on_board") != Some("no");
            let dnp = symbol.find_str("dnp") == Some("yes");
            let projects = symbol
                .find("instances")
                .map(|instances| instances.find_all("project"))
                .unwrap_or_default();
            for project in projects {
                for path in project.find_all("path") {
                    let Some(reference) = path.find_str("reference") else {
                        continue;
                    };
                    let instance = path.get(1).and_then(SexpNode::as_str).unwrap_or("/");
                    let base = if instance == "/" && !root_uuid.is_empty() {
                        format!("/{root_uuid}")
                    } else {
                        instance.trim_end_matches('/').to_string()
                    };
                    flags.push(Flags {
                        reference: reference.to_string(),
                        symbol_path: format!("{base}/{uuid}").replace("//", "/"),
                        in_bom,
                        on_board,
                        dnp,
                    });
                }
            }
        }
    }

    for reference in flags
        .iter()
        .map(|entry| entry.reference.as_str())
        .collect::<HashSet<_>>()
    {
        let entries = flags
            .iter()
            .filter(|entry| entry.reference == reference)
            .collect::<Vec<_>>();
        if entries.iter().any(|entry| {
            entry.in_bom != entries[0].in_bom
                || entry.on_board != entries[0].on_board
                || entry.dnp != entries[0].dnp
        }) {
            bail!("multi-unit reference {reference} has inconsistent board/BOM/DNP flags");
        }
    }

    design.components.retain_mut(|component| {
        let path_match = flags
            .iter()
            .find(|entry| entry.symbol_path == component.symbol_path);
        let reference_matches = flags
            .iter()
            .filter(|entry| entry.reference == component.reference)
            .collect::<Vec<_>>();
        let entry = path_match.or_else(|| reference_matches.first().copied());
        let Some(entry) = entry else {
            return true;
        };
        if !entry.in_bom {
            return false;
        }
        if !entry.on_board {
            design.skipped.push(SkippedComponent {
                reference: entry.reference.clone(),
                symbol_path: entry.symbol_path.clone(),
            });
            return false;
        }
        component.dnp = entry.dnp;
        true
    });
    let mut skipped_references = HashSet::new();
    for entry in flags.iter().filter(|entry| entry.in_bom && !entry.on_board) {
        if !skipped_references.insert(entry.reference.as_str()) {
            continue;
        }
        if !design
            .skipped
            .iter()
            .any(|skipped| skipped.symbol_path == entry.symbol_path)
        {
            design.skipped.push(SkippedComponent {
                reference: entry.reference.clone(),
                symbol_path: entry.symbol_path.clone(),
            });
        }
    }
    Ok(())
}

fn snapshot_board(client: &konnect_ipc::KiCadIpcClient, board: &Path) -> Result<LiveSnapshot> {
    use kiapi::common::types::KiCadObjectType as ObjectType;
    use konnect_ipc::gen::kiapi;

    let document = client.find_open_board(board)?;
    let footprint_items = client.get_items_in(document.clone(), ObjectType::KotPcbFootprint)?;
    let mut footprints = Vec::new();
    let mut items = BTreeMap::new();
    for item in footprint_items {
        let footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            .context("KiCad returned an invalid footprint item")?;
        let kiid = footprint
            .id
            .as_ref()
            .map(|id| id.value.clone())
            .filter(|id| !id.is_empty())
            .context("KiCad returned a footprint without a KIID")?;
        let definition = footprint
            .definition
            .as_ref()
            .context("KiCad returned a footprint without a definition")?;
        let mut pad_nets = BTreeMap::new();
        for child in &definition.items {
            // Same discriminator as `apply_footprint_fields`, for the same
            // reason: a graphic decodes happily as an empty pad.
            //
            // No test covers this one, and deliberately so — it has no
            // observable effect today. A graphic decoded as a pad has
            // `net: None`, so the filter below drops it anyway, and this
            // function never writes. It is here because the next person to add
            // a field to this loop should not have to rediscover why reading
            // `definition.items` untyped is unsafe. Neutering it changes
            // nothing, which is the honest result.
            if !konnect_ipc::builders::any_is(child, "kiapi.board.types.Pad") {
                continue;
            }
            let Ok(pad) = kiapi::board::types::Pad::decode(child.value.as_slice()) else {
                continue;
            };
            if let Some(net) = pad.net.filter(|net| !net.name.is_empty()) {
                pad_nets.insert(pad.number, net.name);
            }
        }
        let position = footprint.position.as_ref();
        footprints.push(BoardFootprint {
            kiid: kiid.clone(),
            reference: field_text(&footprint.reference_field),
            value: field_text(&footprint.value_field),
            footprint_id: definition
                .id
                .as_ref()
                .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
                .unwrap_or_default(),
            symbol_path: footprint.symbol_path.as_ref().map(sheet_path_string),
            pad_nets,
            position: Point {
                x: position
                    .map(|point| konnect_ipc::builders::nm_to_mm(point.x_nm))
                    .unwrap_or(0.0),
                y: position
                    .map(|point| konnect_ipc::builders::nm_to_mm(point.y_nm))
                    .unwrap_or(0.0),
            },
            rotation: footprint
                .orientation
                .as_ref()
                .map(|angle| angle.value_degrees)
                .unwrap_or(0.0),
            layer: board_layer_name(footprint.layer),
            locked: footprint.locked == kiapi::common::types::LockedState::LsLocked as i32,
            dnp: footprint
                .attributes
                .as_ref()
                .map(|attributes| attributes.do_not_populate)
                .unwrap_or(false),
            not_in_schematic: footprint
                .attributes
                .as_ref()
                .map(|attributes| attributes.not_in_schematic)
                .unwrap_or(false),
        });
        items.insert(kiid, item);
    }

    let nets = client.get_nets_in(document.clone())?;
    let net_codes = nets
        .iter()
        .map(|net| (net.name.clone(), net.netcode))
        .collect::<BTreeMap<_, _>>();
    let mut routed_nets = BTreeMap::new();
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbTrace)? {
        if let Ok(track) = kiapi::board::types::Track::decode(item.value.as_slice()) {
            record_routed_net(&mut routed_nets, track.net.as_ref());
        }
    }
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbArc)? {
        if let Ok(arc) = kiapi::board::types::Arc::decode(item.value.as_slice()) {
            record_routed_net(&mut routed_nets, arc.net.as_ref());
        }
    }
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbVia)? {
        if let Ok(via) = kiapi::board::types::Via::decode(item.value.as_slice()) {
            record_routed_net(&mut routed_nets, via.net.as_ref());
        }
    }
    if !client
        .get_items_in(document.clone(), ObjectType::KotPcbZone)?
        .is_empty()
    {
        // KiCad 10's Zone protobuf does not expose the zone net. A pad-net
        // reassignment on a zoned board therefore fails closed.
        for net in net_codes.keys() {
            *routed_nets.entry(net.clone()).or_insert(0) += 1;
        }
    }
    let extents = client
        .get_optional_board_extents_in(document.clone())?
        .unwrap_or(konnect_ipc::IpcBoardExtents {
            min: konnect_ipc::IpcVector2 { x: 0.0, y: 0.0 },
            max: konnect_ipc::IpcVector2 { x: 0.0, y: 0.0 },
        });
    Ok(LiveSnapshot {
        state: BoardState {
            footprints,
            routed_nets,
            bounds: Bounds {
                min_x: extents.min.x,
                min_y: extents.min.y,
                max_x: extents.max.x,
                max_y: extents.max.y,
            },
        },
        items,
        net_codes,
        document,
    })
}

fn field_text(field: &Option<konnect_ipc::gen::kiapi::board::types::Field>) -> String {
    field
        .as_ref()
        .and_then(|field| field.text.as_ref())
        .and_then(|text| text.text.as_ref())
        .map(|text| text.text.clone())
        .unwrap_or_default()
}

fn sheet_path_string(path: &konnect_ipc::gen::kiapi::common::types::SheetPath) -> String {
    format!(
        "/{}",
        path.path
            .iter()
            .map(|part| part.value.as_str())
            .collect::<Vec<_>>()
            .join("/")
    )
}

fn board_layer_name(layer: i32) -> String {
    use konnect_ipc::gen::kiapi::board::types::BoardLayer;
    match BoardLayer::try_from(layer).ok() {
        Some(BoardLayer::BlFCu) => "F.Cu".to_string(),
        Some(BoardLayer::BlBCu) => "B.Cu".to_string(),
        Some(layer) => layer.as_str_name().to_string(),
        None => format!("layer_{layer}"),
    }
}

fn record_routed_net(
    routed: &mut BTreeMap<String, usize>,
    net: Option<&konnect_ipc::gen::kiapi::board::types::Net>,
) {
    if let Some(net) = net.filter(|net| !net.name.is_empty()) {
        *routed.entry(net.name.clone()).or_insert(0) += 1;
    }
}

fn prepare_additions(board: &Path, plan: &SyncPlan) -> Result<BTreeMap<String, PreparedFootprint>> {
    let mut prepared = BTreeMap::new();
    for change in &plan.changes {
        let PlannedChange::Add { footprint_id, .. } = change else {
            continue;
        };
        if prepared.contains_key(footprint_id) {
            continue;
        }
        let source = super::pcb_components::resolve_footprint_source(footprint_id, board)?;
        let pads = super::pcb_components::extract_pad_definitions(&source)?;
        let graphics = super::pcb_components::extract_graphic_definitions(&source)?;
        let fields = super::pcb_components::extract_field_placement(&source);
        let (width, height) = footprint_dimensions(&pads, &graphics);
        prepared.insert(
            footprint_id.clone(),
            PreparedFootprint {
                pads,
                graphics,
                fields,
                width,
                height,
            },
        );
    }
    Ok(prepared)
}

fn footprint_dimensions(
    pads: &[konnect_ipc::IpcPadDefinition],
    graphics: &[konnect_ipc::IpcGraphicDefinition],
) -> (f64, f64) {
    use konnect_ipc::IpcGraphicDefinition as Graphic;

    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut include = |x: f64, y: f64| {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    };
    for pad in pads {
        include(pad.x - pad.size_x / 2.0, pad.y - pad.size_y / 2.0);
        include(pad.x + pad.size_x / 2.0, pad.y + pad.size_y / 2.0);
    }
    for graphic in graphics {
        match graphic {
            Graphic::Line { start, end, .. } | Graphic::Rect { start, end, .. } => {
                include(start.0, start.1);
                include(end.0, end.1);
            }
            Graphic::Circle { center, end, .. } => {
                let radius = ((end.0 - center.0).powi(2) + (end.1 - center.1).powi(2)).sqrt();
                include(center.0 - radius, center.1 - radius);
                include(center.0 + radius, center.1 + radius);
            }
            Graphic::Arc {
                start, mid, end, ..
            } => {
                include(start.0, start.1);
                include(mid.0, mid.1);
                include(end.0, end.1);
            }
            Graphic::Poly { points, .. } => {
                for point in points {
                    include(point.0, point.1);
                }
            }
            Graphic::Text { position, size, .. } => {
                include(position.0 - size / 2.0, position.1 - size / 2.0);
                include(position.0 + size / 2.0, position.1 + size / 2.0);
            }
        }
    }
    if !min_x.is_finite() {
        return (10.0, 10.0);
    }
    ((max_x - min_x).max(1.0), (max_y - min_y).max(1.0))
}

fn restage_additions(
    plan: &mut SyncPlan,
    prepared: &BTreeMap<String, PreparedFootprint>,
    bounds: Bounds,
) {
    let mut next_y = bounds.min_y;
    for change in &mut plan.changes {
        let PlannedChange::Add {
            footprint_id,
            position,
            ..
        } = change
        else {
            continue;
        };
        let dimensions = prepared.get(footprint_id);
        let width = dimensions.map(|part| part.width).unwrap_or(10.0);
        let height = dimensions.map(|part| part.height).unwrap_or(10.0);
        *position = Point {
            x: bounds.max_x + 5.0 + width / 2.0,
            y: next_y + height / 2.0,
        };
        next_y += height + 5.0;
    }
}

fn build_mutation_items(
    client: &konnect_ipc::KiCadIpcClient,
    plan: &SyncPlan,
    prepared: &BTreeMap<String, PreparedFootprint>,
    snapshot: &LiveSnapshot,
) -> Result<(Vec<prost_types::Any>, Vec<prost_types::Any>)> {
    use konnect_ipc::gen::kiapi;

    let mut creates = Vec::new();
    let mut updates = Vec::new();
    for change in &plan.changes {
        match change {
            PlannedChange::Add {
                reference,
                value,
                footprint_id,
                symbol_path,
                dnp,
                pad_nets,
                position,
            } => {
                let part = prepared
                    .get(footprint_id)
                    .with_context(|| format!("no prepared footprint for {footprint_id}"))?;
                let item = client.build_footprint_item(
                    footprint_id,
                    reference,
                    value,
                    &part.pads,
                    &part.graphics,
                    &part.fields,
                    position.x,
                    position.y,
                    0.0,
                    "F.Cu",
                )?;
                let mut footprint =
                    kiapi::board::types::FootprintInstance::decode(item.value.as_slice())?;
                apply_footprint_fields(
                    &mut footprint,
                    reference,
                    value,
                    symbol_path,
                    *dnp,
                    pad_nets,
                    &snapshot.net_codes,
                )?;
                creates.push(konnect_ipc::builders::pack_any(
                    &footprint,
                    "kiapi.board.types.FootprintInstance",
                ));
            }
            PlannedChange::Update { kiid, .. } => {
                let item = snapshot
                    .items
                    .get(kiid)
                    .with_context(|| format!("planned footprint {kiid} disappeared"))?;
                updates.push(update_footprint_item(item, change, &snapshot.net_codes)?);
            }
        }
    }
    Ok((creates, updates))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const ONE_RESISTOR: &str = r#"
(export
  (components
    (comp
      (ref "R1")
      (value "10k")
      (footprint "Resistor_SMD:R_0603_1608Metric")
      (sheetpath (names "/Power/") (tstamps "/sheet-uuid/"))
      (tstamps "symbol-uuid")
      (units (unit (name "A") (pins (pin (num "1")) (pin (num "2")))))))
  (nets
    (net (code "1") (name "/Power/VCC") (class "Default")
      (node (ref "R1") (pin "1") (pintype "passive")))
    (net (code "2") (name "GND") (class "Default")
      (node (ref "R1") (pin "2") (pintype "passive")))))
"#;

    #[test]
    fn exported_netlist_is_one_flattened_source_of_component_and_pad_truth() {
        let design = parse_exported_netlist(ONE_RESISTOR).expect("valid KiCad netlist");

        assert_eq!(design.components.len(), 1);
        let component = &design.components[0];
        assert_eq!(component.reference, "R1");
        assert_eq!(component.value, "10k");
        assert_eq!(component.footprint_id, "Resistor_SMD:R_0603_1608Metric");
        assert_eq!(component.symbol_path, "/sheet-uuid/symbol-uuid");
        assert_eq!(
            component.pad_nets.get("1").map(String::as_str),
            Some("/Power/VCC")
        );
        assert_eq!(component.pad_nets.get("2").map(String::as_str), Some("GND"));
        assert!(!component.dnp);
    }

    fn resistor(reference: &str, symbol_path: &str) -> DesignComponent {
        DesignComponent {
            reference: reference.to_string(),
            value: "10k".to_string(),
            footprint_id: "Resistor_SMD:R_0603_1608Metric".to_string(),
            symbol_path: symbol_path.to_string(),
            dnp: false,
            pad_nets: BTreeMap::from([
                ("1".to_string(), "VCC".to_string()),
                ("2".to_string(), "GND".to_string()),
            ]),
        }
    }

    fn board_resistor(reference: &str, symbol_path: Option<&str>) -> BoardFootprint {
        BoardFootprint {
            kiid: format!("{reference}-kiid"),
            reference: reference.to_string(),
            value: "10k".to_string(),
            footprint_id: "Resistor_SMD:R_0603_1608Metric".to_string(),
            symbol_path: symbol_path.map(str::to_string),
            pad_nets: BTreeMap::from([
                ("1".to_string(), "VCC".to_string()),
                ("2".to_string(), "GND".to_string()),
            ]),
            position: Point { x: 1.0, y: 2.0 },
            rotation: 0.0,
            layer: "F.Cu".to_string(),
            locked: false,
            dnp: false,
            not_in_schematic: false,
        }
    }

    fn board_with(footprints: Vec<BoardFootprint>) -> BoardState {
        BoardState {
            footprints,
            routed_nets: BTreeMap::new(),
            bounds: Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        }
    }

    fn rebind_board_footprint(
        reference: &str,
        kiid: &str,
        symbol_path: Option<&str>,
        value: &str,
        footprint_id: &str,
        dnp: bool,
        pad_nets: &[(&str, &str)],
        layer: &str,
    ) -> BoardFootprint {
        BoardFootprint {
            kiid: kiid.to_string(),
            reference: reference.to_string(),
            value: value.to_string(),
            footprint_id: footprint_id.to_string(),
            symbol_path: symbol_path.map(str::to_string),
            pad_nets: pad_nets
                .iter()
                .map(|(pad, net)| (pad.to_string(), net.to_string()))
                .collect(),
            position: Point { x: 1.0, y: 2.0 },
            rotation: 0.0,
            layer: layer.to_string(),
            locked: false,
            dnp,
            not_in_schematic: false,
        }
    }

    fn identity_rebind_fixture() -> (String, ExportedDesign, BoardState) {
        let (source, design) = identity_rebind_source_and_design(
            "2026-08-19T10:00:00",
            "kicad-cli (10.0.5)",
            "22222222-2222-4222-8222-222222222222",
            "1N4148WS",
            "lh60-core:D_SOD-323_Bottom",
            false,
            "ROW0",
        );
        let board = board_with(vec![
            rebind_board_footprint(
                "D1",
                "d1-kiid",
                Some("/11111111-1111-4111-8111-111111111111"),
                "1N4148WS",
                "lh60-core:D_SOD-323_Bottom",
                false,
                &[("1", "KEY_00"), ("2", "/ROW0")],
                "F.Cu",
            ),
            rebind_board_footprint(
                "SW1",
                "sw1-kiid",
                Some("/66666666-6666-4666-8666-666666666666"),
                "SW_Push",
                "Button_Switch_SMD:SW_SPST_SKQG_WithoutStem",
                false,
                &[("1", "COL0"), ("2", "ROW0")],
                "B.Cu",
            ),
            rebind_board_footprint(
                "R1",
                "r1-kiid",
                Some("/77777777-7777-4777-8777-777777777777"),
                "10k",
                "Resistor_SMD:R_0603_1608Metric",
                false,
                &[("1", "VCC"), ("2", "GND")],
                "F.Cu",
            ),
        ]);
        (source, design, board)
    }

    fn requested_identity_rebind_references() -> Vec<String> {
        vec!["D1".to_string(), "SW1".to_string()]
    }

    fn identity_rebind_source_and_design(
        date: &str,
        tool: &str,
        d1_symbol_stamp: &str,
        d1_value: &str,
        d1_footprint: &str,
        d1_dnp: bool,
        d1_pad_2: &str,
    ) -> (String, ExportedDesign) {
        let d1_dnp_property = if d1_dnp {
            "(property (name \"dnp\") (value \"1\"))"
        } else {
            ""
        };
        let source = format!(
            r#"(export (version "E")
  (design
    (source "/tmp/identity-rebind.kicad_sch")
    (date "{date}")
    (tool "{tool}")
  )
  (components
    (comp (ref "D1")
      (value "{d1_value}")
      (footprint "{d1_footprint}")
      (sheetpath (tstamps "/"))
      (tstamps "{d1_symbol_stamp}")
      {d1_dnp_property})
    (comp (ref "SW1")
      (value "SW_Push")
      (footprint "Button_Switch_SMD:SW_SPST_SKQG_WithoutStem")
      (sheetpath (tstamps "/33333333-3333-4333-8333-333333333333/"))
      (tstamps "44444444-4444-4444-8444-444444444444"))
    (comp (ref "R1")
      (value "10k")
      (footprint "Resistor_SMD:R_0603_1608Metric")
      (sheetpath (tstamps "/"))
      (tstamps "55555555-5555-4555-8555-555555555555")))
  (nets
    (net (code "1") (name "KEY_00")
      (node (ref "D1") (pin "1")))
    (net (code "2") (name "{d1_pad_2}")
      (node (ref "D1") (pin "2"))
      (node (ref "SW1") (pin "2")))
    (net (code "3") (name "COL0")
      (node (ref "SW1") (pin "1")))
    (net (code "4") (name "VCC")
      (node (ref "R1") (pin "1")))
    (net (code "5") (name "GND")
      (node (ref "R1") (pin "2")))))"#
        );
        let design = parse_exported_netlist(&source).expect("valid identity-rebind netlist");
        (source, design)
    }

    fn parse_identity_rebind_source(source: &str) -> ExportedDesign {
        parse_exported_netlist(source).expect("valid identity-rebind netlist")
    }

    fn tool_result_json(result: &CallToolResult) -> Value {
        result
            .content
            .iter()
            .find_map(|content| match content {
                ToolContent::Text { text } => Some(serde_json::from_str::<Value>(text).unwrap()),
                _ => None,
            })
            .expect("tool result contains JSON text")
    }

    #[test]
    fn rebind_request_rejects_missing_empty_and_duplicate_references() {
        let missing = parse_rebind_request(&json!({
            "schematic": "/tmp/test.kicad_sch",
            "board": "/tmp/test.kicad_pcb"
        }))
        .expect_err("references are required");
        let missing_json = tool_result_json(&missing);
        assert_eq!(missing_json["error"]["kind"], "invalid_argument");
        assert_eq!(missing_json["error"]["field"], "references");

        let empty = parse_rebind_request(&json!({
            "schematic": "/tmp/test.kicad_sch",
            "board": "/tmp/test.kicad_pcb",
            "references": [""]
        }))
        .expect_err("references must be non-empty");
        let empty_json = tool_result_json(&empty);
        assert_eq!(empty_json["error"]["kind"], "invalid_argument");
        assert_eq!(empty_json["error"]["field"], "references");

        let duplicate = parse_rebind_request(&json!({
            "schematic": "/tmp/test.kicad_sch",
            "board": "/tmp/test.kicad_pcb",
            "references": ["D1", "D1"]
        }))
        .expect_err("references must be unique");
        let duplicate_json = tool_result_json(&duplicate);
        assert_eq!(duplicate_json["error"]["kind"], "invalid_argument");
        assert_eq!(duplicate_json["error"]["field"], "references");

        let ok = parse_rebind_request(&json!({
            "schematic": "/tmp/test.kicad_sch",
            "board": "/tmp/test.kicad_pcb",
            "references": ["D1", "SW1"]
        }))
        .expect("valid request");
        assert!(ok.dry_run);
        assert_eq!(ok.references, vec!["D1", "SW1"]);
        assert_eq!(ok.expected_plan_revision, None);
    }

    #[test]
    fn rebind_request_apply_requires_expected_plan_revision() {
        let result = parse_rebind_request(&json!({
            "schematic": "/tmp/test.kicad_sch",
            "board": "/tmp/test.kicad_pcb",
            "references": ["D1"],
            "dry_run": false
        }))
        .expect_err("apply requires expected_plan_revision");

        let value = tool_result_json(&result);
        assert_eq!(value["error"]["kind"], "invalid_argument");
        assert_eq!(value["error"]["field"], "expected_plan_revision");
    }

    #[test]
    fn rebind_path_preflight_reports_missing_schematic_before_board() {
        let facts = RebindPathFacts {
            schematic: PathBuf::from("/tmp/missing.kicad_sch"),
            schematic_exists: false,
            board: PathBuf::from("/tmp/missing.kicad_pcb"),
            board_exists: false,
        };

        let result = require_rebind_paths(&facts).expect_err("missing schematic must fail first");
        let value = tool_result_json(&result);
        assert_eq!(value["error"]["kind"], "file_not_found");
        assert_eq!(value["error"]["path"], "/tmp/missing.kicad_sch");
    }

    #[test]
    fn rebind_path_preflight_reports_missing_board_when_schematic_exists() {
        let facts = RebindPathFacts {
            schematic: PathBuf::from("/tmp/present.kicad_sch"),
            schematic_exists: true,
            board: PathBuf::from("/tmp/missing.kicad_pcb"),
            board_exists: false,
        };

        let result = require_rebind_paths(&facts).expect_err("missing board must fail");
        let value = tool_result_json(&result);
        assert_eq!(value["error"]["kind"], "file_not_found");
        assert_eq!(value["error"]["path"], "/tmp/missing.kicad_pcb");
    }

    #[test]
    fn rebind_transport_gate_reports_file_fallback_as_conflict() {
        let result = require_live_rebind_ipc::<()>(BoardWrite::File)
            .expect_err("file fallback must fail closed");

        let value = tool_result_json(&result);
        assert_eq!(value["status"], "conflict");
        assert_eq!(value["coverage"]["transport"], "live_kicad_ipc");
        assert_eq!(value["diagnostics"][0]["code"], "preflight_conflict");
        assert_eq!(value["changes"], json!([]));
        assert_eq!(value["undo"], serde_json::Value::Null);
    }

    #[test]
    fn rebind_transport_gate_wraps_refusal_as_preflight_conflict() {
        let refusal = CallToolResult::error("KiCad refused the live rebind");
        let result = require_live_rebind_ipc::<()>(BoardWrite::Refused(refusal))
            .expect_err("refusal must fail closed");

        let value = tool_result_json(&result);
        assert_eq!(value["status"], "conflict");
        assert_eq!(value["coverage"]["transport"], "live_kicad_ipc");
        assert_eq!(value["diagnostics"][0]["code"], "preflight_conflict");
        assert_eq!(
            value["diagnostics"][0]["message"],
            "KiCad refused the live rebind"
        );
        assert_eq!(value["changes"], json!([]));
        assert_eq!(value["undo"], serde_json::Value::Null);
    }

    #[test]
    fn rebind_response_formats_ready_dry_run_shape() {
        let plan = IdentityRebindPlan {
            status: PlanStatus::Ready,
            plan_revision: "abc123".to_string(),
            counts: IdentityRebindCounts {
                requested: 2,
                eligible: 2,
                planned: 2,
                applied: 0,
                conflicts: 0,
            },
            changes: vec![IdentityRebindChange {
                reference: "D1".to_string(),
                kiid: "d1-kiid".to_string(),
                old_symbol_path: "/11111111-1111-4111-8111-111111111111".to_string(),
                new_symbol_path: "/22222222-2222-4222-8222-222222222222".to_string(),
                value: "1N4148WS".to_string(),
                footprint_id: "lh60-core:D_SOD-323_Bottom".to_string(),
                dnp: false,
                pad_nets: BTreeMap::from([
                    ("1".to_string(), "KEY_00".to_string()),
                    ("2".to_string(), "ROW0".to_string()),
                ]),
                preserve: PreservedBoardState {
                    position: Point { x: 1.0, y: 2.0 },
                    rotation: 0.0,
                    layer: "F.Cu".to_string(),
                    locked: false,
                },
            }],
            diagnostics: Vec::new(),
        };

        let result = identity_rebind_response(&plan, 3, None);
        let value = tool_result_json(&result);
        assert!(!result.is_error);
        assert_eq!(value["status"], "ready");
        assert_eq!(value["plan_revision"], "abc123");
        assert_eq!(value["coverage"]["source"], "saved_schematic_hierarchy");
        assert_eq!(value["coverage"]["hierarchy_files"], 3);
        assert_eq!(value["coverage"]["transport"], "live_kicad_ipc");
        assert_eq!(value["coverage"]["atomicity"], "single_kicad_undo_commit");
        assert_eq!(value["coverage"]["requested"], 2);
        assert_eq!(value["coverage"]["eligible"], 2);
        assert_eq!(value["coverage"]["planned"], 2);
        assert_eq!(value["coverage"]["applied"], 0);
        assert_eq!(value["coverage"]["conflicts"], 0);
        assert_eq!(value["changes"][0]["reference"], "D1");
        assert_eq!(value["undo"], serde_json::Value::Null);
    }

    fn replace_once(source: &str, needle: &str, replacement: &str) -> String {
        assert!(source.contains(needle), "missing source snippet: {needle}");
        source.replacen(needle, replacement, 1)
    }

    fn identity_rebind_source_without_d1(source: &str) -> String {
        let source = replace_once(source, "(comp (ref \"D1\")", "(comp (ref \"DX1\")");
        let source = replace_once(
            source.as_str(),
            "(node (ref \"D1\") (pin \"1\"))",
            "(node (ref \"DX1\") (pin \"1\"))",
        );
        replace_once(
            source.as_str(),
            "(node (ref \"D1\") (pin \"2\"))",
            "(node (ref \"DX1\") (pin \"2\"))",
        )
    }

    fn identity_rebind_source_with_duplicate_sw1_identity(source: &str) -> String {
        replace_once(
            source,
            "      (sheetpath (tstamps \"/33333333-3333-4333-8333-333333333333/\"))\n      (tstamps \"44444444-4444-4444-8444-444444444444\"))",
            "      (sheetpath (tstamps \"/\"))\n      (tstamps \"22222222-2222-4222-8222-222222222222\"))",
        )
    }

    fn identity_rebind_source_with_invalid_d1_identity(source: &str) -> String {
        replace_once(
            source,
            "(tstamps \"22222222-2222-4222-8222-222222222222\")",
            "(tstamps \"not-a-uuid\")",
        )
    }

    fn identity_rebind_source_with_duplicate_d1_reference(source: &str) -> String {
        replace_once(source, "(comp (ref \"SW1\")", "(comp (ref \"D1\")")
    }

    fn identity_rebind_source_without_d1_symbol_timestamp(source: &str) -> String {
        replace_once(
            source,
            "      (tstamps \"22222222-2222-4222-8222-222222222222\")\n",
            "",
        )
    }

    fn design_component_mut<'a>(
        design: &'a mut ExportedDesign,
        reference: &str,
    ) -> &'a mut DesignComponent {
        design
            .components
            .iter_mut()
            .find(|component| component.reference == reference)
            .unwrap_or_else(|| panic!("missing design component {reference}"))
    }

    fn board_footprint_mut<'a>(
        board: &'a mut BoardState,
        reference: &str,
    ) -> &'a mut BoardFootprint {
        board
            .footprints
            .iter_mut()
            .find(|footprint| footprint.reference == reference)
            .unwrap_or_else(|| panic!("missing board footprint {reference}"))
    }

    #[test]
    fn identity_rebind_plans_only_exact_equivalent_references() {
        let (source, design, board) = identity_rebind_fixture();

        let plan = plan_identity_rebind(
            &source,
            &design,
            &board,
            &requested_identity_rebind_references(),
        );
        assert_eq!(plan.status, PlanStatus::Ready);
        assert_eq!(plan.counts.requested, 2);
        assert_eq!(plan.counts.eligible, 2);
        assert_eq!(plan.counts.planned, 2);
        assert_eq!(plan.counts.conflicts, 0);
        assert_eq!(
            plan.changes
                .iter()
                .map(|change| change.reference.as_str())
                .collect::<Vec<_>>(),
            vec!["D1", "SW1"],
        );
        assert_eq!(
            plan.changes[0].old_symbol_path,
            "/11111111-1111-4111-8111-111111111111",
        );
        assert_eq!(
            plan.changes[0].new_symbol_path,
            "/22222222-2222-4222-8222-222222222222",
        );
        assert_eq!(plan.changes[0].preserve.layer, "F.Cu");
    }

    #[test]
    fn identity_rebind_conflicts_are_fail_closed() {
        struct ConflictCase {
            name: &'static str,
            mutate_source: Option<fn(&str) -> String>,
            mutate_board: fn(&mut BoardState),
            mutate_requested: fn(&mut Vec<String>),
            code: &'static str,
        }

        let cases = vec![
            ConflictCase {
                name: "missing schematic reference",
                mutate_source: Some(identity_rebind_source_without_d1),
                mutate_board: |_| {},
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "requested_reference_missing_from_schematic",
            },
            ConflictCase {
                name: "missing board reference",
                mutate_source: None,
                mutate_board: |board| {
                    board
                        .footprints
                        .retain(|footprint| footprint.reference != "D1");
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "requested_reference_missing_from_board",
            },
            ConflictCase {
                name: "duplicate request",
                mutate_source: None,
                mutate_board: |_| {},
                mutate_requested: |requested| *requested = vec!["D1".to_string(), "D1".to_string()],
                code: "duplicate_requested_reference",
            },
            ConflictCase {
                name: "missing old identity",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1").symbol_path = None;
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "missing_board_identity",
            },
            ConflictCase {
                name: "mixed already matching identity",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1").symbol_path =
                        Some("/22222222-2222-4222-8222-222222222222".to_string());
                },
                mutate_requested: |requested| *requested = requested_identity_rebind_references(),
                code: "identity_already_matches_in_mixed_request",
            },
            ConflictCase {
                name: "value drift",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1").value = "WRONG".to_string();
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "value_mismatch",
            },
            ConflictCase {
                name: "footprint drift",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1").footprint_id = "wrong:Footprint".to_string();
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "footprint_mismatch",
            },
            ConflictCase {
                name: "dnp drift",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1").dnp = true;
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "dnp_mismatch",
            },
            ConflictCase {
                name: "pad set drift",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1").pad_nets.remove("2");
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "pad_set_mismatch",
            },
            ConflictCase {
                name: "pad net drift",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1")
                        .pad_nets
                        .insert("2".to_string(), "ROW1".to_string());
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "pad_net_mismatch",
            },
            ConflictCase {
                name: "nested net mismatch",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1")
                        .pad_nets
                        .insert("2".to_string(), "/sheet/ROW0".to_string());
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "pad_net_mismatch",
            },
            ConflictCase {
                name: "board only footprint",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1").not_in_schematic = true;
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "board_only_footprint",
            },
            ConflictCase {
                name: "duplicate old identity",
                mutate_source: None,
                mutate_board: |board| {
                    let d1_path = board_footprint_mut(board, "D1").symbol_path.clone();
                    board_footprint_mut(board, "SW1").symbol_path = d1_path;
                },
                mutate_requested: |requested| *requested = requested_identity_rebind_references(),
                code: "duplicate_board_identity",
            },
            ConflictCase {
                name: "duplicate new identity",
                mutate_source: Some(identity_rebind_source_with_duplicate_sw1_identity),
                mutate_board: |_| {},
                mutate_requested: |requested| *requested = requested_identity_rebind_references(),
                code: "duplicate_schematic_identity",
            },
            ConflictCase {
                name: "target identity collides with unrequested board item",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "R1").symbol_path =
                        Some("/22222222-2222-4222-8222-222222222222".to_string());
                },
                mutate_requested: |requested| *requested = requested_identity_rebind_references(),
                code: "target_identity_in_use",
            },
            ConflictCase {
                name: "invalid old kiid path",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1").symbol_path = Some("/not-a-uuid".to_string());
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "invalid_board_identity",
            },
            ConflictCase {
                name: "invalid new kiid path",
                mutate_source: Some(identity_rebind_source_with_invalid_d1_identity),
                mutate_board: |_| {},
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "invalid_schematic_identity",
            },
            ConflictCase {
                name: "trailing path segment",
                mutate_source: None,
                mutate_board: |board| {
                    board_footprint_mut(board, "D1").symbol_path =
                        Some("/11111111-1111-4111-8111-111111111111/".to_string());
                },
                mutate_requested: |requested| *requested = vec!["D1".to_string()],
                code: "invalid_board_identity",
            },
        ];

        for case in cases {
            let (source, _, mut board) = identity_rebind_fixture();
            let source = case
                .mutate_source
                .map(|mutate| mutate(&source))
                .unwrap_or(source);
            let design = parse_identity_rebind_source(&source);
            let mut requested = requested_identity_rebind_references();
            (case.mutate_board)(&mut board);
            (case.mutate_requested)(&mut requested);

            let plan = plan_identity_rebind(&source, &design, &board, &requested);

            assert_eq!(plan.status, PlanStatus::Conflict, "{}", case.name);
            assert!(plan.changes.is_empty(), "{}", case.name);
            assert_eq!(plan.counts.planned, 0, "{}", case.name);
            assert!(
                plan.diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code == case.code),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn identity_rebind_is_noop_when_every_requested_identity_already_matches() {
        let (source, design, mut board) = identity_rebind_fixture();
        board_footprint_mut(&mut board, "D1").symbol_path = Some(
            design_component_mut(&mut design.clone(), "D1")
                .symbol_path
                .clone(),
        );
        board_footprint_mut(&mut board, "SW1").symbol_path = Some(
            design_component_mut(&mut design.clone(), "SW1")
                .symbol_path
                .clone(),
        );

        let plan = plan_identity_rebind(
            &source,
            &design,
            &board,
            &requested_identity_rebind_references(),
        );

        assert_eq!(plan.status, PlanStatus::Noop);
        assert_eq!(plan.counts.planned, 0);
        assert!(plan.changes.is_empty());
        assert!(plan.diagnostics.is_empty());
    }

    #[test]
    fn identity_rebind_plan_revision_is_stable_for_equivalent_inputs_and_changes_for_drift() {
        let (source, design) = identity_rebind_source_and_design(
            "2026-08-19T10:00:00",
            "kicad-cli (10.0.5)",
            "22222222-2222-4222-8222-222222222222",
            "1N4148WS",
            "lh60-core:D_SOD-323_Bottom",
            false,
            "ROW0",
        );
        let (_, _, board) = identity_rebind_fixture();
        let requested = requested_identity_rebind_references();

        let base = plan_identity_rebind(&source, &design, &board, &requested);
        let (header_source, header_design) = identity_rebind_source_and_design(
            "2026-08-19T10:00:05",
            "kicad-cli (10.1.0)",
            "22222222-2222-4222-8222-222222222222",
            "1N4148WS",
            "lh60-core:D_SOD-323_Bottom",
            false,
            "ROW0",
        );
        let different_header =
            plan_identity_rebind(&header_source, &header_design, &board, &requested);
        assert_eq!(base.plan_revision, different_header.plan_revision);

        let reversed = plan_identity_rebind(
            &source,
            &design,
            &board,
            &["SW1".to_string(), "D1".to_string()],
        );
        assert_eq!(base.plan_revision, reversed.plan_revision);

        let mut changed_board = board.clone();
        board_footprint_mut(&mut changed_board, "D1").symbol_path =
            Some("/88888888-8888-4888-8888-888888888888".to_string());
        assert_ne!(
            base.plan_revision,
            plan_identity_rebind(&source, &design, &changed_board, &requested).plan_revision
        );

        let (path_source, path_design) = identity_rebind_source_and_design(
            "2026-08-19T10:00:00",
            "kicad-cli (10.0.5)",
            "99999999-9999-4999-8999-999999999999",
            "1N4148WS",
            "lh60-core:D_SOD-323_Bottom",
            false,
            "ROW0",
        );
        assert_ne!(
            base.plan_revision,
            plan_identity_rebind(&path_source, &path_design, &board, &requested).plan_revision
        );

        for (value, footprint, dnp, pad_2) in [
            ("DIFFERENT", "lh60-core:D_SOD-323_Bottom", false, "ROW0"),
            ("1N4148WS", "other:Footprint", false, "ROW0"),
            ("1N4148WS", "lh60-core:D_SOD-323_Bottom", true, "ROW0"),
            ("1N4148WS", "lh60-core:D_SOD-323_Bottom", false, "ROW1"),
        ] {
            let (changed_source, changed_design) = identity_rebind_source_and_design(
                "2026-08-19T10:00:00",
                "kicad-cli (10.0.5)",
                "22222222-2222-4222-8222-222222222222",
                value,
                footprint,
                dnp,
                pad_2,
            );
            assert_ne!(
                base.plan_revision,
                plan_identity_rebind(&changed_source, &changed_design, &board, &requested)
                    .plan_revision
            );
        }

        let mut changed_board = board.clone();
        board_footprint_mut(&mut changed_board, "D1").kiid = "changed-kiid".to_string();
        assert_ne!(
            base.plan_revision,
            plan_identity_rebind(&source, &design, &changed_board, &requested).plan_revision
        );

        let mut changed_board = board.clone();
        board_footprint_mut(&mut changed_board, "D1").position.x = 99.0;
        assert_ne!(
            base.plan_revision,
            plan_identity_rebind(&source, &design, &changed_board, &requested).plan_revision
        );

        let mut changed_board = board.clone();
        board_footprint_mut(&mut changed_board, "D1").layer = "B.Cu".to_string();
        assert_ne!(
            base.plan_revision,
            plan_identity_rebind(&source, &design, &changed_board, &requested).plan_revision
        );

        let mut changed_board = board.clone();
        board_footprint_mut(&mut changed_board, "D1").locked = true;
        assert_ne!(
            base.plan_revision,
            plan_identity_rebind(&source, &design, &changed_board, &requested).plan_revision
        );

        assert_ne!(
            base.plan_revision,
            plan_identity_rebind(&source, &design, &board, &["D1".to_string()]).plan_revision
        );
    }

    #[test]
    fn identity_rebind_conflict_revisions_change_with_distinct_requested_design_inputs() {
        let (_, _, board) = identity_rebind_fixture();
        let requested = vec!["D1".to_string()];

        let (wrong_value_source, wrong_value_design) = identity_rebind_source_and_design(
            "2026-08-19T10:00:00",
            "kicad-cli (10.0.5)",
            "22222222-2222-4222-8222-222222222222",
            "WRONG",
            "lh60-core:D_SOD-323_Bottom",
            false,
            "ROW0",
        );
        let value_conflict =
            plan_identity_rebind(&wrong_value_source, &wrong_value_design, &board, &requested);
        assert_eq!(value_conflict.status, PlanStatus::Conflict);
        assert!(value_conflict.changes.is_empty());
        assert!(value_conflict
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "value_mismatch"));

        let (wrong_footprint_source, wrong_footprint_design) = identity_rebind_source_and_design(
            "2026-08-19T10:00:00",
            "kicad-cli (10.0.5)",
            "22222222-2222-4222-8222-222222222222",
            "1N4148WS",
            "wrong:Footprint",
            false,
            "ROW0",
        );
        let footprint_conflict = plan_identity_rebind(
            &wrong_footprint_source,
            &wrong_footprint_design,
            &board,
            &requested,
        );
        assert_eq!(footprint_conflict.status, PlanStatus::Conflict);
        assert!(footprint_conflict.changes.is_empty());
        assert!(footprint_conflict
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "footprint_mismatch"));

        assert_ne!(
            value_conflict.plan_revision,
            footprint_conflict.plan_revision
        );
    }

    #[test]
    fn identity_rebind_plan_revision_changes_when_requested_not_in_schematic_changes() {
        let (source, design, board) = identity_rebind_fixture();
        let requested = vec!["D1".to_string()];

        let base = plan_identity_rebind(&source, &design, &board, &requested);
        let mut board_only = board.clone();
        board_footprint_mut(&mut board_only, "D1").not_in_schematic = true;
        let changed = plan_identity_rebind(&source, &design, &board_only, &requested);

        assert_ne!(base.plan_revision, changed.plan_revision);
    }

    #[test]
    fn duplicate_schematic_references_are_rejected_by_parser() {
        let (source, _, _) = identity_rebind_fixture();
        let duplicate = identity_rebind_source_with_duplicate_d1_reference(&source);

        let error = parse_exported_netlist(&duplicate).unwrap_err().to_string();

        assert!(
            error.contains("duplicate component reference D1"),
            "{error}"
        );
    }

    #[test]
    fn missing_schematic_identity_is_rejected_by_parser() {
        let (source, _, _) = identity_rebind_fixture();
        let missing_identity = identity_rebind_source_without_d1_symbol_timestamp(&source);

        let error = parse_exported_netlist(&missing_identity)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("component has no symbol timestamp"),
            "{error}"
        );
    }

    #[test]
    fn empty_path_segment_is_rejected_by_symbol_path_validation() {
        assert!(!valid_symbol_path("//22222222-2222-4222-8222-222222222222"));
    }

    #[test]
    fn planner_matches_identity_preserves_pose_and_stages_new_parts_deterministically() {
        let design = ExportedDesign {
            components: vec![
                resistor("R2", "/sheet/existing"),
                resistor("R3", "/sheet/new"),
            ],
            skipped: Vec::new(),
        };
        let board = BoardState {
            footprints: vec![
                BoardFootprint {
                    kiid: "existing-kiid".to_string(),
                    reference: "R1".to_string(),
                    value: "1k".to_string(),
                    footprint_id: "Resistor_SMD:R_0603_1608Metric".to_string(),
                    symbol_path: Some("/sheet/existing".to_string()),
                    pad_nets: BTreeMap::from([
                        ("1".to_string(), "VCC".to_string()),
                        ("2".to_string(), "GND".to_string()),
                    ]),
                    position: Point { x: 25.0, y: 30.0 },
                    rotation: 90.0,
                    layer: "B.Cu".to_string(),
                    locked: true,
                    dnp: false,
                    not_in_schematic: false,
                },
                BoardFootprint {
                    kiid: "board-only".to_string(),
                    reference: "MH1".to_string(),
                    value: "MountingHole".to_string(),
                    footprint_id: "MountingHole:MountingHole_3.2mm_M3".to_string(),
                    symbol_path: None,
                    pad_nets: BTreeMap::new(),
                    position: Point { x: 2.0, y: 2.0 },
                    rotation: 0.0,
                    layer: "F.Cu".to_string(),
                    locked: true,
                    dnp: false,
                    not_in_schematic: true,
                },
            ],
            routed_nets: BTreeMap::new(),
            bounds: Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 50.0,
                max_y: 40.0,
            },
        };

        let first = plan_sync("netlist bytes", &design, &board);
        let second = plan_sync("netlist bytes", &design, &board);

        assert_eq!(first.status, PlanStatus::Ready);
        assert_eq!(first.plan_revision, second.plan_revision);
        assert_eq!(first.counts.added.planned, 1);
        assert_eq!(first.counts.updated.planned, 1);
        assert_eq!(first.counts.board_only_preserved.planned, 1);
        assert_eq!(first.changes, second.changes);
        assert!(first.changes.iter().any(|change| matches!(
            change,
            PlannedChange::Update { kiid, reference, preserve, .. }
                if kiid == "existing-kiid"
                    && reference == "R2"
                    && preserve.position == Point { x: 25.0, y: 30.0 }
                    && preserve.rotation == 90.0
                    && preserve.layer == "B.Cu"
                    && preserve.locked
        )));
        assert!(first.changes.iter().any(|change| matches!(
            change,
            PlannedChange::Add { reference, position, .. }
                if reference == "R3" && position.x > board.bounds.max_x
        )));
    }

    /// Build a footprint carrying one pad and one child of every graphic kind,
    /// the way a real library footprint arrives from KiCad. The existing sync
    /// test passes `&[]` for graphics, which is precisely why #244 survived it.
    fn footprint_with_artwork(reference: &str) -> prost_types::Any {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let client = konnect_ipc::KiCadIpcClient::new("inproc://not-connected");
        let silk = || "F.SilkS".to_string();
        let item = client
            .build_footprint_item(
                "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
                reference,
                "NE555",
                &[konnect_ipc::IpcPadDefinition {
                    number: "1".to_string(),
                    pad_type: "smd".to_string(),
                    shape: "rect".to_string(),
                    x: 0.0,
                    y: 0.0,
                    rotation: 0.0,
                    size_x: 1.0,
                    size_y: 1.0,
                    drill_x: None,
                    drill_y: None,
                    drill_oval: false,
                    layers: vec!["F.Cu".to_string()],
                    roundrect_ratio: 0.0,
                }],
                &[
                    konnect_ipc::IpcGraphicDefinition::Line {
                        start: (-2.0, -2.5),
                        end: (2.0, -2.5),
                        layer: silk(),
                        width: 0.12,
                    },
                    konnect_ipc::IpcGraphicDefinition::Rect {
                        start: (-2.6, -3.0),
                        end: (2.6, 3.0),
                        layer: "F.CrtYd".to_string(),
                        width: 0.05,
                        filled: false,
                    },
                    konnect_ipc::IpcGraphicDefinition::Circle {
                        center: (-1.8, -1.8),
                        end: (-1.6, -1.8),
                        layer: silk(),
                        width: 0.12,
                        filled: true,
                    },
                    konnect_ipc::IpcGraphicDefinition::Arc {
                        start: (-1.0, -2.5),
                        mid: (0.0, -2.0),
                        end: (1.0, -2.5),
                        layer: "F.Fab".to_string(),
                        width: 0.1,
                    },
                    konnect_ipc::IpcGraphicDefinition::Poly {
                        points: vec![(-1.0, 2.0), (1.0, 2.0), (0.0, 2.8)],
                        layer: "F.Fab".to_string(),
                        width: 0.1,
                        filled: true,
                    },
                    konnect_ipc::IpcGraphicDefinition::Text {
                        text: "U1".to_string(),
                        position: (0.0, -3.5),
                        rotation: 0.0,
                        layer: silk(),
                        size: 1.0,
                    },
                ],
                &konnect_ipc::IpcFieldPlacement::default(),
                25.0,
                30.0,
                0.0,
                "F.Cu",
            )
            .unwrap();
        // Give it the KIID the update path matches against.
        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        footprint.id = Some(kiapi::common::types::Kiid {
            value: format!("{}-kiid", reference.to_lowercase()),
        });
        konnect_ipc::builders::pack_any(&footprint, "kiapi.board.types.FootprintInstance")
    }

    fn identity_rebind_fixture_item_and_change() -> (prost_types::Any, IdentityRebindChange, String)
    {
        use konnect_ipc::gen::kiapi;
        use prost::Message;

        let old_symbol_path =
            "/11111111-1111-4111-8111-111111111111/22222222-2222-4222-8222-222222222222";
        let new_symbol_path =
            "/33333333-3333-4333-8333-333333333333/44444444-4444-4444-8444-444444444444";
        let item = footprint_with_artwork("D1");
        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        footprint.id = Some(kiapi::common::types::Kiid {
            value: "rebind-d1-kiid".to_string(),
        });
        footprint.orientation = Some(kiapi::common::types::Angle {
            value_degrees: 37.5,
        });
        footprint.locked = kiapi::common::types::LockedState::LsLocked as i32;
        footprint.symbol_path = Some(kiapi::common::types::SheetPath {
            path: old_symbol_path
                .split('/')
                .filter(|segment| !segment.is_empty())
                .map(|segment| kiapi::common::types::Kiid {
                    value: segment.to_string(),
                })
                .collect(),
            path_human_readable: "old-human-readable".to_string(),
        });
        footprint
            .attributes
            .get_or_insert_with(Default::default)
            .do_not_populate = true;
        let definition = footprint.definition.as_mut().unwrap();
        definition
            .attributes
            .get_or_insert_with(Default::default)
            .do_not_populate = true;
        definition.items.push(konnect_ipc::builders::pack_any(
            &kiapi::board::types::Footprint3DModel::default(),
            "kiapi.board.types.Footprint3DModel",
        ));

        let item =
            konnect_ipc::builders::pack_any(&footprint, "kiapi.board.types.FootprintInstance");
        let change = IdentityRebindChange {
            reference: "D1".to_string(),
            kiid: "rebind-d1-kiid".to_string(),
            old_symbol_path: old_symbol_path.to_string(),
            new_symbol_path: new_symbol_path.to_string(),
            value: "NE555".to_string(),
            footprint_id: "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm".to_string(),
            dnp: true,
            pad_nets: BTreeMap::from([("1".to_string(), "GND".to_string())]),
            preserve: PreservedBoardState {
                position: Point { x: 25.0, y: 30.0 },
                rotation: 37.5,
                layer: "F.Cu".to_string(),
                locked: true,
            },
        };
        (item, change, new_symbol_path.to_string())
    }

    fn mutate_rebound_footprint_item(
        item: &prost_types::Any,
        mutate: impl FnOnce(&mut konnect_ipc::gen::kiapi::board::types::FootprintInstance),
    ) -> prost_types::Any {
        use konnect_ipc::gen::kiapi;
        use prost::Message;

        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        mutate(&mut footprint);
        konnect_ipc::builders::pack_any(&footprint, "kiapi.board.types.FootprintInstance")
    }

    #[test]
    fn rebind_footprint_item_changes_only_symbol_path() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;

        let (before_item, change, expected_new_path) = identity_rebind_fixture_item_and_change();

        let updated = rebind_footprint_item(&before_item, &change).unwrap();

        let mut before =
            kiapi::board::types::FootprintInstance::decode(before_item.value.as_slice()).unwrap();
        let mut after =
            kiapi::board::types::FootprintInstance::decode(updated.value.as_slice()).unwrap();

        let rebound_path = after.symbol_path.take().expect("rebound symbol_path");
        assert_eq!(
            sheet_path_string(&rebound_path),
            expected_new_path,
            "rebind must write the exact structured KIID sequence"
        );
        assert!(
            rebound_path.path_human_readable.is_empty(),
            "rebind must clear human-readable sheet path"
        );

        before.symbol_path = None;
        after.symbol_path = None;
        assert_eq!(
            before, after,
            "rebind must preserve every non-identity footprint field"
        );
    }

    #[test]
    fn rebind_footprint_item_rejects_wrong_type_kiid_reference_old_or_invalid_new_path() {
        use konnect_ipc::gen::kiapi;

        struct Case {
            name: &'static str,
            mutate_item: fn(&prost_types::Any) -> prost_types::Any,
            mutate_change: fn(&mut IdentityRebindChange),
            expected: &'static str,
        }

        let cases = [
            Case {
                name: "wrong any type",
                mutate_item: |item| prost_types::Any {
                    type_url: "type.googleapis.com/kiapi.board.types.Pad".to_string(),
                    value: item.value.clone(),
                },
                mutate_change: |_| {},
                expected: "FootprintInstance",
            },
            Case {
                name: "wrong kiid",
                mutate_item: |_| {
                    let (item, _, _) = identity_rebind_fixture_item_and_change();
                    mutate_rebound_footprint_item(&item, |footprint| {
                        footprint.id = Some(kiapi::common::types::Kiid {
                            value: "wrong-kiid".to_string(),
                        });
                    })
                },
                mutate_change: |_| {},
                expected: "rebind-d1-kiid",
            },
            Case {
                name: "wrong reference",
                mutate_item: |_| {
                    let (item, _, _) = identity_rebind_fixture_item_and_change();
                    mutate_rebound_footprint_item(&item, |footprint| {
                        set_field_text(&mut footprint.reference_field, "Reference", "D9");
                    })
                },
                mutate_change: |_| {},
                expected: "reference D1",
            },
            Case {
                name: "wrong old path",
                mutate_item: |_| {
                    let (item, _, _) = identity_rebind_fixture_item_and_change();
                    mutate_rebound_footprint_item(&item, |footprint| {
                        footprint.symbol_path = Some(kiapi::common::types::SheetPath {
                            path: vec![kiapi::common::types::Kiid {
                                value: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
                            }],
                            path_human_readable: String::new(),
                        });
                    })
                },
                mutate_change: |_| {},
                expected: "schematic identity",
            },
            Case {
                name: "invalid uuid segment",
                mutate_item: |item| item.clone(),
                mutate_change: |change| {
                    change.new_symbol_path =
                        "/not-a-uuid/44444444-4444-4444-8444-444444444444".to_string();
                },
                expected: "invalid target schematic identity",
            },
            Case {
                name: "empty segment",
                mutate_item: |item| item.clone(),
                mutate_change: |change| {
                    change.new_symbol_path =
                        "/33333333-3333-4333-8333-333333333333//44444444-4444-4444-8444-444444444444".to_string();
                },
                expected: "invalid target schematic identity",
            },
            Case {
                name: "trailing slash",
                mutate_item: |item| item.clone(),
                mutate_change: |change| {
                    change.new_symbol_path =
                        "/33333333-3333-4333-8333-333333333333/44444444-4444-4444-8444-444444444444/".to_string();
                },
                expected: "invalid target schematic identity",
            },
        ];

        for case in cases {
            let (base_item, mut change, _) = identity_rebind_fixture_item_and_change();
            let item = (case.mutate_item)(&base_item);
            (case.mutate_change)(&mut change);

            let error = rebind_footprint_item(&item, &change)
                .expect_err(case.name)
                .to_string();
            assert!(
                error.contains(case.expected),
                "{}: expected error containing {:?}, got {:?}",
                case.name,
                case.expected,
                error
            );
        }
    }

    #[test]
    fn rebind_readback_accepts_only_expected_symbol_path_change() {
        let (before_item, change, expected_new_path) = identity_rebind_fixture_item_and_change();
        let rebound_item = rebind_footprint_item(&before_item, &change).unwrap();

        verify_rebound_footprint(&before_item, &rebound_item, &expected_new_path).unwrap();

        let error = verify_rebound_footprint(
            &before_item,
            &rebound_item,
            "/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("symbol_path"), "{error}");
    }

    #[test]
    fn rebind_readback_rejects_every_non_identity_mutation() {
        use konnect_ipc::builders::vec2;
        use konnect_ipc::gen::kiapi;

        struct Case {
            name: &'static str,
            mutate: fn(&mut kiapi::board::types::FootprintInstance),
            expected_message: &'static str,
        }

        let cases = [
            Case {
                name: "position",
                mutate: |footprint| {
                    footprint.position = Some(vec2(99.0, 30.0));
                },
                expected_message: "position",
            },
            Case {
                name: "orientation",
                mutate: |footprint| {
                    footprint.orientation = Some(kiapi::common::types::Angle {
                        value_degrees: 91.0,
                    });
                },
                expected_message: "orientation",
            },
            Case {
                name: "reference",
                mutate: |footprint| {
                    set_field_text(&mut footprint.reference_field, "Reference", "D999");
                },
                expected_message: "reference",
            },
            Case {
                name: "layer",
                mutate: |footprint| {
                    footprint.layer = kiapi::board::types::BoardLayer::BlBCu as i32;
                },
                expected_message: "layer",
            },
            Case {
                name: "locked",
                mutate: |footprint| {
                    footprint.locked = kiapi::common::types::LockedState::LsUnlocked as i32;
                },
                expected_message: "locked",
            },
            Case {
                name: "value",
                mutate: |footprint| {
                    set_field_text(&mut footprint.value_field, "Value", "DIFFERENT");
                },
                expected_message: "value",
            },
            Case {
                name: "dnp",
                mutate: |footprint| {
                    footprint
                        .attributes
                        .get_or_insert_with(Default::default)
                        .do_not_populate = false;
                    footprint
                        .definition
                        .as_mut()
                        .unwrap()
                        .attributes
                        .get_or_insert_with(Default::default)
                        .do_not_populate = false;
                },
                expected_message: "attributes",
            },
            Case {
                name: "definition id",
                mutate: |footprint| {
                    footprint
                        .definition
                        .as_mut()
                        .unwrap()
                        .id
                        .as_mut()
                        .unwrap()
                        .entry_name = "Different_Entry".to_string();
                },
                expected_message: "definition id",
            },
            Case {
                name: "pad uuid",
                mutate: |footprint| {
                    let definition = footprint.definition.as_mut().unwrap();
                    let pad = definition
                        .items
                        .iter_mut()
                        .find(|item| konnect_ipc::builders::any_is(item, "kiapi.board.types.Pad"))
                        .unwrap();
                    let mut decoded =
                        kiapi::board::types::Pad::decode(pad.value.as_slice()).unwrap();
                    decoded.id = Some(kiapi::common::types::Kiid {
                        value: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
                    });
                    *pad = konnect_ipc::builders::pack_any(&decoded, "kiapi.board.types.Pad");
                },
                expected_message: "pad",
            },
            Case {
                name: "pad number",
                mutate: |footprint| {
                    let definition = footprint.definition.as_mut().unwrap();
                    let pad = definition
                        .items
                        .iter_mut()
                        .find(|item| konnect_ipc::builders::any_is(item, "kiapi.board.types.Pad"))
                        .unwrap();
                    let mut decoded =
                        kiapi::board::types::Pad::decode(pad.value.as_slice()).unwrap();
                    decoded.number = "99".to_string();
                    *pad = konnect_ipc::builders::pack_any(&decoded, "kiapi.board.types.Pad");
                },
                expected_message: "pad",
            },
            Case {
                name: "pad net",
                mutate: |footprint| {
                    let definition = footprint.definition.as_mut().unwrap();
                    let pad = definition
                        .items
                        .iter_mut()
                        .find(|item| konnect_ipc::builders::any_is(item, "kiapi.board.types.Pad"))
                        .unwrap();
                    let mut decoded =
                        kiapi::board::types::Pad::decode(pad.value.as_slice()).unwrap();
                    decoded.net = Some(kiapi::board::types::Net {
                        code: Some(kiapi::board::types::NetCode { value: 99 }),
                        name: "DIFF_NET".to_string(),
                    });
                    *pad = konnect_ipc::builders::pack_any(&decoded, "kiapi.board.types.Pad");
                },
                expected_message: "pad",
            },
            Case {
                name: "pad geometry position",
                mutate: |footprint| {
                    let definition = footprint.definition.as_mut().unwrap();
                    let pad = definition
                        .items
                        .iter_mut()
                        .find(|item| konnect_ipc::builders::any_is(item, "kiapi.board.types.Pad"))
                        .unwrap();
                    let mut decoded =
                        kiapi::board::types::Pad::decode(pad.value.as_slice()).unwrap();
                    decoded.position = Some(vec2(123.0, 456.0));
                    *pad = konnect_ipc::builders::pack_any(&decoded, "kiapi.board.types.Pad");
                },
                expected_message: "pad",
            },
            Case {
                name: "graphic content",
                mutate: |footprint| {
                    let definition = footprint.definition.as_mut().unwrap();
                    let graphic = definition
                        .items
                        .iter_mut()
                        .find(|item| {
                            konnect_ipc::builders::any_is(
                                item,
                                "kiapi.board.types.BoardGraphicShape",
                            )
                        })
                        .unwrap();
                    let mut decoded =
                        kiapi::board::types::BoardGraphicShape::decode(graphic.value.as_slice())
                            .unwrap();
                    decoded.layer = kiapi::board::types::BoardLayer::BlFCrtYd as i32;
                    *graphic = konnect_ipc::builders::pack_any(
                        &decoded,
                        "kiapi.board.types.BoardGraphicShape",
                    );
                },
                expected_message: "graphic",
            },
            Case {
                name: "field placement",
                mutate: |footprint| {
                    let board_text = footprint
                        .reference_field
                        .as_mut()
                        .unwrap()
                        .text
                        .as_mut()
                        .unwrap()
                        .text
                        .as_mut()
                        .unwrap();
                    board_text.position = Some(vec2(777.0, 888.0));
                },
                expected_message: "field placement",
            },
            Case {
                name: "model",
                mutate: |footprint| {
                    let definition = footprint.definition.as_mut().unwrap();
                    let model = definition
                        .items
                        .iter_mut()
                        .find(|item| {
                            konnect_ipc::builders::any_is(
                                item,
                                "kiapi.board.types.Footprint3DModel",
                            )
                        })
                        .unwrap();
                    let mut decoded =
                        kiapi::board::types::Footprint3DModel::decode(model.value.as_slice())
                            .unwrap();
                    decoded.filename = "different-model.step".to_string();
                    decoded.visible = !decoded.visible;
                    *model = konnect_ipc::builders::pack_any(
                        &decoded,
                        "kiapi.board.types.Footprint3DModel",
                    );
                },
                expected_message: "model",
            },
            Case {
                name: "item count",
                mutate: |footprint| {
                    footprint.definition.as_mut().unwrap().items.pop();
                },
                expected_message: "item count",
            },
        ];

        let (before_item, change, expected_new_path) = identity_rebind_fixture_item_and_change();
        let rebound_item = rebind_footprint_item(&before_item, &change).unwrap();

        for case in cases {
            let corrupted = mutate_rebound_footprint_item(&rebound_item, case.mutate);
            let error = verify_rebound_footprint(&before_item, &corrupted, &expected_new_path)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(case.expected_message),
                "{}: expected {error} to mention {}",
                case.name,
                case.expected_message
            );
        }
    }

    #[test]
    fn rebind_readback_accepts_semantic_child_reordering_and_preserves_unknown_any() {
        let (before_item, change, expected_new_path) = identity_rebind_fixture_item_and_change();
        let rebound_item = rebind_footprint_item(&before_item, &change).unwrap();

        let reordered = mutate_rebound_footprint_item(&rebound_item, |footprint| {
            let items = &mut footprint.definition.as_mut().unwrap().items;
            items.reverse();
        });
        verify_rebound_footprint(&before_item, &reordered, &expected_new_path).unwrap();

        let unknown_any = prost_types::Any {
            type_url: "type.googleapis.com/example.UnknownFootprintChild".to_string(),
            value: vec![1, 2, 3, 4],
        };
        let before_with_unknown = mutate_rebound_footprint_item(&before_item, |footprint| {
            footprint
                .definition
                .as_mut()
                .unwrap()
                .items
                .push(unknown_any.clone());
        });
        let after_with_unknown = mutate_rebound_footprint_item(&rebound_item, |footprint| {
            let items = &mut footprint.definition.as_mut().unwrap().items;
            items.push(unknown_any.clone());
            items.rotate_right(1);
        });
        verify_rebound_footprint(
            &before_with_unknown,
            &after_with_unknown,
            &expected_new_path,
        )
        .unwrap();

        let mutated_unknown_payload =
            mutate_rebound_footprint_item(&after_with_unknown, |footprint| {
                let unknown = footprint
                    .definition
                    .as_mut()
                    .unwrap()
                    .items
                    .iter_mut()
                    .find(|item| {
                        item.type_url == "type.googleapis.com/example.UnknownFootprintChild"
                    })
                    .unwrap();
                unknown.value = vec![9, 9, 9];
            });
        let payload_error = verify_rebound_footprint(
            &before_with_unknown,
            &mutated_unknown_payload,
            &expected_new_path,
        )
        .unwrap_err()
        .to_string();
        assert!(
            payload_error.contains("unknown footprint child payload"),
            "{payload_error}"
        );

        let mutated_unknown_type =
            mutate_rebound_footprint_item(&after_with_unknown, |footprint| {
                let unknown = footprint
                    .definition
                    .as_mut()
                    .unwrap()
                    .items
                    .iter_mut()
                    .find(|item| {
                        item.type_url == "type.googleapis.com/example.UnknownFootprintChild"
                    })
                    .unwrap();
                unknown.type_url = "type.googleapis.com/example.OtherUnknown".to_string();
            });
        let type_error = verify_rebound_footprint(
            &before_with_unknown,
            &mutated_unknown_type,
            &expected_new_path,
        )
        .unwrap_err()
        .to_string();
        assert!(
            type_error.contains("unknown footprint child type"),
            "{type_error}"
        );
    }

    #[test]
    fn rebind_readback_treats_optional_default_messages_as_semantically_equal() {
        use konnect_ipc::gen::kiapi;

        let (before_item, change, expected_new_path) = identity_rebind_fixture_item_and_change();
        let rebound_item = rebind_footprint_item(&before_item, &change).unwrap();

        let orientation_pair_before = mutate_rebound_footprint_item(&before_item, |footprint| {
            footprint.orientation = Some(kiapi::common::types::Angle::default());
        });
        let orientation_pair_after = mutate_rebound_footprint_item(&rebound_item, |footprint| {
            footprint.orientation = None;
        });
        verify_rebound_footprint(
            &orientation_pair_before,
            &orientation_pair_after,
            &expected_new_path,
        )
        .unwrap();
        let orientation_non_default =
            mutate_rebound_footprint_item(&orientation_pair_after, |footprint| {
                footprint.orientation = Some(kiapi::common::types::Angle { value_degrees: 1.0 });
            });
        let orientation_error = verify_rebound_footprint(
            &orientation_pair_before,
            &orientation_non_default,
            &expected_new_path,
        )
        .unwrap_err()
        .to_string();
        assert!(
            orientation_error.contains("orientation"),
            "{orientation_error}"
        );

        let instance_attributes_before = mutate_rebound_footprint_item(&before_item, |footprint| {
            footprint.attributes = Some(kiapi::board::types::FootprintAttributes::default());
        });
        let instance_attributes_after = mutate_rebound_footprint_item(&rebound_item, |footprint| {
            footprint.attributes = None;
        });
        verify_rebound_footprint(
            &instance_attributes_before,
            &instance_attributes_after,
            &expected_new_path,
        )
        .unwrap();
        let instance_attributes_non_default =
            mutate_rebound_footprint_item(&instance_attributes_after, |footprint| {
                footprint
                    .attributes
                    .get_or_insert_with(Default::default)
                    .do_not_populate = true;
            });
        let instance_attributes_error = verify_rebound_footprint(
            &instance_attributes_before,
            &instance_attributes_non_default,
            &expected_new_path,
        )
        .unwrap_err()
        .to_string();
        assert!(
            instance_attributes_error.contains("attributes"),
            "{instance_attributes_error}"
        );

        let definition_attributes_before =
            mutate_rebound_footprint_item(&before_item, |footprint| {
                footprint.definition.as_mut().unwrap().attributes =
                    Some(kiapi::board::types::FootprintAttributes::default());
            });
        let definition_attributes_after =
            mutate_rebound_footprint_item(&rebound_item, |footprint| {
                footprint.definition.as_mut().unwrap().attributes = None;
            });
        verify_rebound_footprint(
            &definition_attributes_before,
            &definition_attributes_after,
            &expected_new_path,
        )
        .unwrap();
        let definition_attributes_non_default =
            mutate_rebound_footprint_item(&definition_attributes_after, |footprint| {
                footprint
                    .definition
                    .as_mut()
                    .unwrap()
                    .attributes
                    .get_or_insert_with(Default::default)
                    .do_not_populate = true;
            });
        let definition_attributes_error = verify_rebound_footprint(
            &definition_attributes_before,
            &definition_attributes_non_default,
            &expected_new_path,
        )
        .unwrap_err()
        .to_string();
        assert!(
            definition_attributes_error.contains("attributes"),
            "{definition_attributes_error}"
        );

        let field_default_before = mutate_rebound_footprint_item(&before_item, |footprint| {
            footprint.datasheet_field = Some(kiapi::board::types::Field {
                text: Some(kiapi::board::types::BoardText::default()),
                ..Default::default()
            });
        });
        let field_default_after = mutate_rebound_footprint_item(&rebound_item, |footprint| {
            footprint.datasheet_field = None;
        });
        verify_rebound_footprint(
            &field_default_before,
            &field_default_after,
            &expected_new_path,
        )
        .unwrap();
        let field_non_default = mutate_rebound_footprint_item(&field_default_after, |footprint| {
            footprint.datasheet_field = Some(kiapi::board::types::Field {
                name: "Datasheet".to_string(),
                text: Some(kiapi::board::types::BoardText {
                    text: Some(kiapi::common::types::Text {
                        text: "not-default".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        });
        let field_error = verify_rebound_footprint(
            &field_default_before,
            &field_non_default,
            &expected_new_path,
        )
        .unwrap_err()
        .to_string();
        assert!(field_error.contains("field placement"), "{field_error}");
    }

    /// Tally a footprint definition's children by the protobuf type they
    /// declare — the property #244 destroyed.
    fn child_types(item: &prost_types::Any) -> BTreeMap<String, usize> {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        let mut counts = BTreeMap::new();
        for child in &footprint.definition.as_ref().unwrap().items {
            *counts
                .entry(konnect_ipc::builders::any_type_name(child).to_string())
                .or_insert(0) += 1;
        }
        counts
    }

    /// #244. A footprint's pads, graphics and text all live in one repeated
    /// `Any` field, and proto3 skips field numbers it does not recognise rather
    /// than failing — so a `BoardGraphicShape` decodes cleanly as a near-empty
    /// `Pad`. Filtering that list with `Pad::decode(..).ok()` therefore matched
    /// every graphic, and packing the decoded value back re-typed it. In
    /// neusse's benchmark an 8-pad SOIC-8 came out of a sync with 28 pads —
    /// the 20 extras nameless, at (0,0), one per lost graphic — and no artwork.
    #[test]
    fn syncing_a_footprint_leaves_its_graphics_as_graphics() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let item = footprint_with_artwork("U1");
        let before = child_types(&item);

        // Sanity: the fixture must actually carry the mixture, or this test
        // proves nothing — which is the trap the pre-existing sync test fell
        // into by passing `&[]` graphics.
        assert_eq!(before.get("kiapi.board.types.Pad"), Some(&1));
        assert_eq!(before.get("kiapi.board.types.BoardGraphicShape"), Some(&5));
        assert_eq!(before.get("kiapi.board.types.BoardText"), Some(&1));

        let change = PlannedChange::Update {
            kiid: "u1-kiid".to_string(),
            reference: "U1".to_string(),
            value: "NE555".to_string(),
            symbol_path: "/root/u1".to_string(),
            dnp: false,
            pad_nets: BTreeMap::from([("1".to_string(), "GND".to_string())]),
            preserve: PreservedBoardState {
                position: Point { x: 25.0, y: 30.0 },
                rotation: 0.0,
                layer: "F.Cu".to_string(),
                locked: false,
            },
        };
        let updated =
            update_footprint_item(&item, &change, &BTreeMap::from([("GND".to_string(), 1)]))
                .unwrap();

        assert_eq!(
            child_types(&updated),
            before,
            "sync re-typed footprint children; graphics must survive as graphics"
        );

        // And the pad still got the net it was there to get.
        let footprint =
            kiapi::board::types::FootprintInstance::decode(updated.value.as_slice()).unwrap();
        let pad = footprint
            .definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|child| konnect_ipc::builders::any_is(child, "kiapi.board.types.Pad"))
            .map(|child| kiapi::board::types::Pad::decode(child.value.as_slice()).unwrap())
            .next()
            .expect("the pad survived");
        assert_eq!(pad.net.as_ref().unwrap().name, "GND");
    }

    /// The add path calls `apply_footprint_fields` too (`build_mutation_items`),
    /// so a brand-new footprint was corrupted before it ever reached KiCad.
    #[test]
    fn a_newly_added_footprint_keeps_its_graphics_too() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let item = footprint_with_artwork("U2");
        let before = child_types(&item);
        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();

        apply_footprint_fields(
            &mut footprint,
            "U2",
            "NE555",
            "/root/u2",
            false,
            &BTreeMap::from([("1".to_string(), "VCC".to_string())]),
            &BTreeMap::from([("VCC".to_string(), 3)]),
        )
        .unwrap();

        let repacked =
            konnect_ipc::builders::pack_any(&footprint, "kiapi.board.types.FootprintInstance");
        assert_eq!(child_types(&repacked), before);
    }

    /// The invariant that would have caught #244 on its own.
    ///
    /// `create_items`/`update_items` only confirm KiCad *accepted* each item,
    /// and the reported counts are copied from the plan — so the corruption
    /// travelled all the way to a success message. Here the exact damage is
    /// reproduced (every drawing re-typed as a pad, which is what the old
    /// `Pad::decode` filter did) and the shape comparison is shown to see it.
    #[test]
    fn the_post_apply_check_sees_drawings_turned_into_pads() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;

        let sent = footprint_with_artwork("U4");
        let expected = footprint_shapes(std::iter::once(&sent));
        assert_eq!(
            expected["U4"],
            FootprintShape {
                pads: 1,
                drawings: 6
            }
        );

        // Exactly #244: decode every child as a Pad and pack it back as one.
        let mut corrupted =
            kiapi::board::types::FootprintInstance::decode(sent.value.as_slice()).unwrap();
        for child in &mut corrupted.definition.as_mut().unwrap().items {
            if let Ok(pad) = kiapi::board::types::Pad::decode(child.value.as_slice()) {
                *child = konnect_ipc::builders::pack_any(&pad, "kiapi.board.types.Pad");
            }
        }
        let corrupted =
            konnect_ipc::builders::pack_any(&corrupted, "kiapi.board.types.FootprintInstance");
        let actual = footprint_shapes(std::iter::once(&corrupted));

        // The reported symptom, reproduced: the five graphic shapes each become
        // a pad. The text survives — `BoardText`'s bytes genuinely fail to
        // decode as a `Pad`, while `BoardGraphicShape`'s do not — which is why
        // #239 reported footprints losing their *graphics* while their
        // reference and value text stayed put.
        assert_eq!(
            actual["U4"],
            FootprintShape {
                pads: 6,
                drawings: 1
            }
        );
        assert_ne!(actual["U4"], expected["U4"]);
    }

    /// A child that declares itself a pad and will not decode is a real
    /// failure, and has to be reported as *that*.
    ///
    /// Skipping it silently does still end in an error — the "footprint has no
    /// pad N" check downstream fires, because the pad never made it into
    /// `seen_pads` — but that error sends the reader looking for a missing pad
    /// that is in fact present and unreadable. So this asserts the specific
    /// message, not merely that something failed: a neuter that restored the
    /// silent skip passed an assertion that only checked for the reference.
    #[test]
    fn an_undecodable_pad_is_reported_not_skipped() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let item = footprint_with_artwork("U3");
        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        for child in &mut footprint.definition.as_mut().unwrap().items {
            if konnect_ipc::builders::any_is(child, "kiapi.board.types.Pad") {
                // Wire type 7 does not exist; nothing can decode this.
                child.value = vec![0xff, 0xff, 0xff];
            }
        }

        let error = apply_footprint_fields(
            &mut footprint,
            "U3",
            "NE555",
            "/root/u3",
            false,
            &BTreeMap::from([("1".to_string(), "VCC".to_string())]),
            &BTreeMap::new(),
        )
        .expect_err("an unreadable pad must not pass silently");
        let text = format!("{error:#}");
        assert!(
            text.contains("U3") && text.contains("cannot read"),
            "must say the pad is unreadable, not that it is missing: {text}"
        );
    }

    #[test]
    fn update_item_changes_only_schematic_owned_fields() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;

        let client = konnect_ipc::KiCadIpcClient::new("inproc://not-connected");
        let item = client
            .build_footprint_item(
                "Resistor_SMD:R_0603_1608Metric",
                "R1",
                "1k",
                &[konnect_ipc::IpcPadDefinition {
                    number: "1".to_string(),
                    pad_type: "smd".to_string(),
                    shape: "rect".to_string(),
                    x: 0.0,
                    y: 0.0,
                    rotation: 0.0,
                    size_x: 1.0,
                    size_y: 1.0,
                    drill_x: None,
                    drill_y: None,
                    drill_oval: false,
                    layers: vec!["F.Cu".to_string()],
                    roundrect_ratio: 0.0,
                }],
                &[],
                &konnect_ipc::IpcFieldPlacement::default(),
                25.0,
                30.0,
                90.0,
                "F.Cu",
            )
            .unwrap();
        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        footprint.id = Some(kiapi::common::types::Kiid {
            value: "keep-kiid".to_string(),
        });
        footprint.locked = kiapi::common::types::LockedState::LsLocked as i32;
        let item =
            konnect_ipc::builders::pack_any(&footprint, "kiapi.board.types.FootprintInstance");
        let change = PlannedChange::Update {
            kiid: "keep-kiid".to_string(),
            reference: "R2".to_string(),
            value: "10k".to_string(),
            symbol_path: "/root/symbol".to_string(),
            dnp: true,
            pad_nets: BTreeMap::from([("1".to_string(), "VCC".to_string())]),
            preserve: PreservedBoardState {
                position: Point { x: 25.0, y: 30.0 },
                rotation: 90.0,
                layer: "F.Cu".to_string(),
                locked: true,
            },
        };

        let updated =
            update_footprint_item(&item, &change, &BTreeMap::from([("VCC".to_string(), 7)]))
                .unwrap();
        let updated =
            kiapi::board::types::FootprintInstance::decode(updated.value.as_slice()).unwrap();

        assert_eq!(updated.id.as_ref().unwrap().value, "keep-kiid");
        assert_eq!(updated.position, footprint.position);
        assert_eq!(updated.orientation, footprint.orientation);
        assert_eq!(updated.layer, footprint.layer);
        assert_eq!(updated.locked, footprint.locked);
        assert!(updated.attributes.as_ref().unwrap().do_not_populate);
        let pad = updated
            .definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find_map(|item| kiapi::board::types::Pad::decode(item.value.as_slice()).ok())
            .unwrap();
        assert_eq!(pad.net.as_ref().unwrap().name, "VCC");
        assert_eq!(pad.net.as_ref().unwrap().code.as_ref().unwrap().value, 7);
    }

    #[test]
    fn removing_a_pad_from_a_routed_net_conflicts_the_whole_plan() {
        let design = ExportedDesign {
            components: vec![DesignComponent {
                pad_nets: BTreeMap::new(),
                ..resistor("R1", "/sheet/existing")
            }],
            skipped: Vec::new(),
        };
        let board = BoardState {
            footprints: vec![BoardFootprint {
                kiid: "existing-kiid".to_string(),
                reference: "R1".to_string(),
                value: "10k".to_string(),
                footprint_id: "Resistor_SMD:R_0603_1608Metric".to_string(),
                symbol_path: Some("/sheet/existing".to_string()),
                pad_nets: BTreeMap::from([("1".to_string(), "VCC".to_string())]),
                position: Point { x: 1.0, y: 2.0 },
                rotation: 0.0,
                layer: "F.Cu".to_string(),
                locked: false,
                dnp: false,
                not_in_schematic: false,
            }],
            routed_nets: BTreeMap::from([("VCC".to_string(), 1)]),
            bounds: Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        };

        let plan = plan_sync("netlist", &design, &board);

        assert_eq!(plan.status, PlanStatus::Conflict);
        assert!(plan.changes.is_empty());
        assert!(plan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "routed_pad_net_change"));
    }

    #[test]
    fn already_synchronized_design_is_noop() {
        let design = ExportedDesign {
            components: vec![resistor("R1", "/sheet/existing")],
            skipped: Vec::new(),
        };
        let plan = plan_sync(
            "netlist",
            &design,
            &board_with(vec![board_resistor("R1", Some("/sheet/existing"))]),
        );

        assert_eq!(plan.status, PlanStatus::Noop);
        assert!(plan.changes.is_empty());
        assert_eq!(plan.counts.conflicts.planned, 0);
    }

    #[test]
    fn footprint_swap_conflicts_but_an_unrouted_net_change_is_planned() {
        let design = ExportedDesign {
            components: vec![resistor("R1", "/sheet/existing")],
            skipped: Vec::new(),
        };
        let mut footprint = board_resistor("R1", Some("/sheet/existing"));
        footprint.footprint_id = "Resistor_SMD:R_0805_2012Metric".to_string();
        let swap = plan_sync("netlist", &design, &board_with(vec![footprint]));
        assert_eq!(swap.status, PlanStatus::Conflict);
        assert!(swap
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "footprint_id_changed"));

        let mut footprint = board_resistor("R1", Some("/sheet/existing"));
        footprint
            .pad_nets
            .insert("1".to_string(), "OLD_VCC".to_string());
        let net_change = plan_sync("netlist", &design, &board_with(vec![footprint]));
        assert_eq!(net_change.status, PlanStatus::Ready);
        assert_eq!(net_change.counts.pads_reassigned.planned, 1);
    }

    #[test]
    fn on_board_no_skips_absent_but_conflicts_when_present() {
        let design = ExportedDesign {
            components: Vec::new(),
            skipped: vec![SkippedComponent {
                reference: "R1".to_string(),
                symbol_path: "/sheet/existing".to_string(),
            }],
        };
        let absent = plan_sync("netlist", &design, &board_with(Vec::new()));
        assert_eq!(absent.status, PlanStatus::Noop);
        assert_eq!(absent.counts.skipped_by_flag.planned, 1);

        let present = plan_sync(
            "netlist",
            &design,
            &board_with(vec![board_resistor("R1", Some("/sheet/existing"))]),
        );
        assert_eq!(present.status, PlanStatus::Conflict);
        assert!(present
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "on_board_exclusion_conflict"));
    }

    #[test]
    fn reference_only_possible_rename_is_a_conflict() {
        let design = ExportedDesign {
            components: vec![resistor("R2", "/sheet/existing")],
            skipped: Vec::new(),
        };
        let plan = plan_sync(
            "netlist",
            &design,
            &board_with(vec![board_resistor("R1", None)]),
        );

        assert_eq!(plan.status, PlanStatus::Conflict);
        assert!(plan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "reference_only_rename_ambiguous"));
    }

    #[test]
    fn empty_and_duplicate_component_exports_are_rejected() {
        let empty = parse_exported_netlist("(export (components) (nets))")
            .unwrap_err()
            .to_string();
        assert!(empty.contains("zero components"), "{empty}");

        let duplicate = r#"
(export
  (components
    (comp (ref "R1") (value "1k") (footprint "Resistor_SMD:R_0603_1608Metric")
      (sheetpath (tstamps "/one/")) (tstamps "one"))
    (comp (ref "R1") (value "2k") (footprint "Resistor_SMD:R_0603_1608Metric")
      (sheetpath (tstamps "/two/")) (tstamps "two")))
  (nets))
"#;
        let duplicate = parse_exported_netlist(duplicate).unwrap_err().to_string();
        assert!(
            duplicate.contains("duplicate component reference R1"),
            "{duplicate}"
        );
    }

    #[test]
    fn plan_revision_changes_when_reviewed_board_bounds_change() {
        let design = ExportedDesign {
            components: vec![resistor("R1", "/sheet/new")],
            skipped: Vec::new(),
        };
        let first = plan_sync("netlist", &design, &board_with(Vec::new()));
        let mut changed_board = board_with(Vec::new());
        changed_board.bounds.max_x = 11.0;
        let second = plan_sync("netlist", &design, &changed_board);

        assert_ne!(first.plan_revision, second.plan_revision);
    }

    /// A plan revision must survive the clock. `kicad-cli` stamps the export
    /// time and its own version into every netlist, so hashing the raw source
    /// changed the revision every second — and apply, which requires the
    /// revision a dry run returned, could then only succeed if both calls
    /// landed inside the same wall-clock second.
    #[test]
    fn plan_revision_ignores_the_export_timestamp_and_tool_version() {
        let netlist = |date: &str, tool: &str| {
            format!(
                "(export (version \"E\")
  (design
    (source \"/tmp/x.kicad_sch\")
    (date \"{date}\")
    (tool \"{tool}\")
  )
  (components
    (comp (ref \"R1\")
      (value \"10k\")
      (footprint \"Resistor_SMD:R_0805\")
      (tstamps \"/aaa\")))
  (nets
    (net (code \"1\") (name \"GND\")
      (node (ref \"R1\") (pin \"1\")))))
"
            )
        };
        let board = BoardState {
            footprints: Vec::new(),
            routed_nets: BTreeMap::new(),
            bounds: Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 0.0,
                max_y: 0.0,
            },
        };
        let a = plan_revision(
            &netlist("2026-08-15T14:48:16", "kicad-cli (10.0.5)"),
            &board,
        );
        let b = plan_revision(
            &netlist("2026-08-15T14:48:18", "kicad-cli (10.0.5)"),
            &board,
        );
        assert_eq!(a, b, "two seconds apart is not a design change");

        let c = plan_revision(
            &netlist("2026-08-15T14:48:16", "kicad-cli (10.1.0)"),
            &board,
        );
        assert_eq!(a, c, "a KiCad upgrade is not a design change");

        // A real change still moves it, or the guard is worthless.
        let changed = netlist("2026-08-15T14:48:16", "kicad-cli (10.0.5)")
            .replace("Resistor_SMD:R_0805", "Resistor_SMD:R_0603");
        assert_ne!(
            a,
            plan_revision(&changed, &board),
            "a footprint swap must move the revision"
        );
    }

    #[test]
    fn plan_revision_keeps_nested_and_quoted_design_content() {
        let netlist = |nested_date: &str, value: &str| {
            format!(
                r#"(export
  (design (date "2026-08-15T14:48:16") (tool "kicad-cli (10.0.5)"))
  (components
    (comp (ref "R1")
      (value "{value}")
      (footprint "Resistor_SMD:R_0805")
      (date "{nested_date}")
      (tstamps "/aaa")))
  (nets
    (net (code "1") (name "GND")
      (node (ref "R1") (pin "1")))))"#
            )
        };
        let board = board_with(Vec::new());
        let baseline = plan_revision(&netlist("2025-01-01", "literal (tool alpha)"), &board);

        assert_ne!(
            baseline,
            plan_revision(&netlist("2025-01-02", "literal (tool alpha)"), &board),
            "a nested date node is component content, not export metadata"
        );
        assert_ne!(
            baseline,
            plan_revision(&netlist("2025-01-01", "literal (tool beta)"), &board),
            "tool-like text inside a quoted value is design content"
        );
    }
}
