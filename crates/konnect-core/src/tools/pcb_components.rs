//! `pcb_components` toolset — place, move, rotate, query, and array footprints on the PCB.
//!
//! Most operations use the KiCAD IPC API so they integrate with KiCAD's undo/redo
//! system and don't require a separate file-sync step. `get_board_2d_view` uses
//! kicad-cli to render a PNG.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use anyhow::Context;
use konnect_ipc::client::KiCadIpcClient;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ─── IPC helper ───────────────────────────────────────────────────────────────

async fn with_ipc<T, F>(addr: String, f: F) -> anyhow::Result<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(&KiCadIpcClient::new(&addr))).await {
        Ok(Ok(r)) => Ok(Ok(r)),
        Ok(Err(e)) => Ok(Err(format!("{e:#}"))),
        Err(e) => Err(anyhow::anyhow!("Thread error: {}", e)),
    }
}

macro_rules! ipc {
    ($ctx:expr, $args:expr, |$c:ident| $body:expr) => {{
        let addr = $ctx.config.ipc_address.clone();
        let requested_board = get_path($args, "board")?;
        match with_ipc(addr, move |$c| {
            $c.ensure_board_is_active(&requested_board)?;
            $body
        })
        .await?
        {
            Ok(v) => v,
            Err(msg) => {
                return Ok(CallToolResult::error(format!(
                    "KiCAD must be running with the board loaded (IPC error: {})",
                    msg
                )))
            }
        }
    }};
}

// ─── Footprint-library resolution ───────────────────────────────────────────

fn footprint_library_dirs() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for variable in ["KICAD10_FOOTPRINT_DIR", "KICAD9_FOOTPRINT_DIR"] {
        if let Some(path) = std::env::var_os(variable).map(PathBuf::from) {
            if path.is_dir() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }

    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[
            r"C:\Program Files\KiCad\10.0\share\kicad\footprints",
            r"C:\KiCad\10.0\share\kicad\footprints",
            r"C:\Program Files\KiCad\9.0\share\kicad\footprints",
            r"C:\KiCad\9.0\share\kicad\footprints",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints",
            "/usr/local/share/kicad/footprints",
        ]
    } else {
        &[
            "/usr/share/kicad/footprints",
            "/usr/local/share/kicad/footprints",
            "/snap/kicad/current/usr/share/kicad/footprints",
            "/var/lib/flatpak/app/org.kicad.KiCad/current/active/files/share/kicad/footprints",
        ]
    };
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.is_dir() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    if cfg!(not(any(target_os = "windows", target_os = "macos"))) {
        if let Some(home) = dirs::home_dir() {
            let path = home.join(
                ".local/share/flatpak/app/org.kicad.KiCad/current/active/files/share/kicad/footprints",
            );
            if path.is_dir() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn expand_library_uri(uri: &str, project_dir: &Path) -> Option<PathBuf> {
    let mut expanded = uri.strip_prefix("file://").unwrap_or(uri).to_string();
    expanded = expanded.replace("${KIPRJMOD}", &project_dir.to_string_lossy());
    expanded = expanded.replace("$(KIPRJMOD)", &project_dir.to_string_lossy());

    for delimiters in [("${", "}"), ("$(", ")")] {
        while let Some(start) = expanded.find(delimiters.0) {
            let name_start = start + delimiters.0.len();
            let end = expanded[name_start..].find(delimiters.1)? + name_start;
            let name = &expanded[name_start..end];
            let value = std::env::var(name).ok()?;
            expanded.replace_range(start..end + delimiters.1.len(), &value);
        }
    }
    Some(PathBuf::from(expanded))
}

fn footprint_from_table(
    table: &Path,
    nickname: &str,
    entry: &str,
    project_dir: &Path,
) -> Option<PathBuf> {
    let content = std::fs::read_to_string(table).ok()?;
    let parsed = konnect_sexp::parse_sexp(&content).ok()?;
    let library = parsed
        .find_all("lib")
        .into_iter()
        .find(|library| library.find_str("name") == Some(nickname))?;
    let uri = library.find_str("uri")?;

    // KiCad defines this variable internally rather than exporting it to the
    // process environment. Resolve the standard-library form against every
    // discovered platform installation directory.
    for prefix in ["${KICAD10_FOOTPRINT_DIR}", "$(KICAD10_FOOTPRINT_DIR)"] {
        if let Some(suffix) = uri.strip_prefix(prefix) {
            let suffix = suffix.trim_start_matches(['/', '\\']);
            return footprint_library_dirs()
                .into_iter()
                .map(|base| base.join(suffix).join(format!("{entry}.kicad_mod")))
                .find(|path| path.is_file());
        }
    }

    let library_dir = expand_library_uri(uri, project_dir)?;
    let path = library_dir.join(format!("{entry}.kicad_mod"));
    path.is_file().then_some(path)
}

fn resolve_footprint_source(lib_id: &str, board: &Path) -> anyhow::Result<String> {
    let (nickname, entry) = lib_id.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("footprint must use Library:Footprint syntax, got '{lib_id}'")
    })?;
    if nickname.is_empty() || entry.is_empty() {
        anyhow::bail!("footprint must use a non-empty Library:Footprint identifier");
    }
    let project_dir = board.parent().unwrap_or_else(|| Path::new("."));
    let project_table = project_dir.join("fp-lib-table");
    let global_table = super::kicad_config_dir().join("fp-lib-table");
    for table in [&project_table, &global_table] {
        if let Some(path) = footprint_from_table(table, nickname, entry, project_dir) {
            return std::fs::read_to_string(&path)
                .map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display()));
        }
    }

    let filename = format!("{entry}.kicad_mod");
    let attempted: Vec<PathBuf> = footprint_library_dirs()
        .into_iter()
        .map(|base| base.join(format!("{nickname}.pretty")).join(&filename))
        .collect();
    for path in &attempted {
        if path.is_file() {
            return std::fs::read_to_string(path)
                .map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display()));
        }
    }
    anyhow::bail!(
        "footprint '{lib_id}' was not found in the project/global fp-lib-table or KiCad library directories (set KICAD10_FOOTPRINT_DIR for a non-standard install)"
    )
}

fn escape_sexp_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn replace_quoted_after(source: &mut String, marker: &str, value: &str) -> anyhow::Result<()> {
    let start = source
        .find(marker)
        .map(|offset| offset + marker.len())
        .ok_or_else(|| anyhow::anyhow!("footprint library data is missing {marker}"))?;
    let bytes = source.as_bytes();
    let mut escaped = false;
    let end = (start..bytes.len())
        .find(|index| {
            let byte = bytes[*index];
            if escaped {
                escaped = false;
                false
            } else if byte == b'\\' {
                escaped = true;
                false
            } else {
                byte == b'"'
            }
        })
        .ok_or_else(|| anyhow::anyhow!("unterminated quoted value after {marker}"))?;
    source.replace_range(start..end, &escape_sexp_string(value));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_footprint_source(
    source: &str,
    lib_id: &str,
    reference: &str,
    value: Option<&str>,
    x: f64,
    y: f64,
    rotation: f64,
    layer: &str,
) -> anyhow::Result<String> {
    if !matches!(layer, "F.Cu" | "B.Cu") {
        anyhow::bail!("footprints can only be placed on F.Cu or B.Cu, got '{layer}'");
    }
    let mut prepared = source.to_string();
    replace_quoted_after(&mut prepared, "(footprint \"", lib_id)?;
    replace_quoted_after(&mut prepared, "(property \"Reference\" \"", reference)?;
    if let Some(value) = value {
        replace_quoted_after(&mut prepared, "(property \"Value\" \"", value)?;
    }

    if layer == "B.Cu" {
        prepared = prepared.replace("\"F.", "\"__KONNECT_FRONT__.");
        prepared = prepared.replace("\"B.", "\"F.");
        prepared = prepared.replace("\"__KONNECT_FRONT__.", "\"B.");
    }
    replace_quoted_after(&mut prepared, "(layer \"", layer)?;
    let layer_start = prepared
        .find("(layer \"")
        .context("footprint library data has no root layer")?;
    let layer_end = prepared[layer_start..]
        .find(')')
        .map(|offset| layer_start + offset + 1)
        .context("footprint root layer is unterminated")?;
    prepared.insert_str(layer_end, &format!("\n\t(at {x} {y} {rotation})"));
    konnect_sexp::parse_sexp(&prepared).context("prepared footprint is not valid S-expression")?;
    Ok(prepared)
}

fn extract_pad_definitions(source: &str) -> anyhow::Result<Vec<konnect_ipc::IpcPadDefinition>> {
    let footprint = konnect_sexp::parse_sexp(source)?;
    footprint
        .find_all("pad")
        .into_iter()
        .map(|pad| {
            let required = |index: usize, label: &str| {
                pad.get(index)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .ok_or_else(|| anyhow::anyhow!("footprint pad is missing {label}"))
            };
            let shape = required(3, "shape")?.to_string();
            if shape == "custom" {
                anyhow::bail!(
                    "custom-shape pads are not supported by KiCad 10's typed placement path"
                );
            }
            let at = pad
                .find("at")
                .context("footprint pad is missing its position")?;
            let size = pad
                .find("size")
                .context("footprint pad is missing its size")?;
            let layers = pad
                .find("layers")
                .context("footprint pad is missing its layer set")?
                .children()
                .unwrap_or_default()
                .iter()
                .skip(1)
                .filter_map(konnect_sexp::SexpNode::as_str)
                .map(str::to_string)
                .collect();
            let (drill_x, drill_y, drill_oval) = match pad.find("drill") {
                Some(drill)
                    if drill.get(1).and_then(konnect_sexp::SexpNode::as_str) == Some("oval") =>
                {
                    (
                        drill.get_f64(2),
                        drill.get_f64(3).or_else(|| drill.get_f64(2)),
                        true,
                    )
                }
                Some(drill) => (
                    drill.get_f64(1),
                    drill.get_f64(2).or_else(|| drill.get_f64(1)),
                    false,
                ),
                None => (None, None, false),
            };
            Ok(konnect_ipc::IpcPadDefinition {
                number: required(1, "number")?.to_string(),
                pad_type: required(2, "type")?.to_string(),
                shape,
                x: at
                    .get_f64(1)
                    .context("footprint pad has an invalid X position")?,
                y: at
                    .get_f64(2)
                    .context("footprint pad has an invalid Y position")?,
                rotation: at.get_f64(3).unwrap_or(0.0),
                size_x: size
                    .get_f64(1)
                    .context("footprint pad has an invalid width")?,
                size_y: size
                    .get_f64(2)
                    .context("footprint pad has an invalid height")?,
                drill_x,
                drill_y,
                drill_oval,
                layers,
                roundrect_ratio: pad.find_f64("roundrect_rratio").unwrap_or(0.0),
            })
        })
        .collect()
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "place_component",
            "Place a footprint on the PCB at the given position and layer via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":      { "type": "string" },
                    "footprint":  { "type": "string", "description": "Library:Footprint (e.g. 'Resistor_SMD:R_0402')" },
                    "reference":  { "type": "string", "description": "Reference designator" },
                    "x":          { "type": "number" },
                    "y":          { "type": "number" },
                    "rotation":   { "type": "number", "default": 0 },
                    "layer":      { "type": "string", "default": "F.Cu" }
                },
                "required": ["board", "footprint", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_place_component(args, ctx).await }
        ),
        tool!(
            "move_component",
            "Move a placed footprint to a new X/Y position via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" }
                },
                "required": ["board", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_component(args, ctx).await }
        ),
        tool!(
            "rotate_component",
            "Set the rotation angle of a placed footprint via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "rotation":  { "type": "number", "description": "Rotation angle in degrees" }
                },
                "required": ["board", "reference", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_component(args, ctx).await }
        ),
        tool!(
            "delete_component",
            "Remove a footprint from the board via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_delete_component(args, ctx).await }
        ),
        tool!(
            "edit_component",
            "Update the value or other properties of a placed footprint via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "value":     { "type": "string", "description": "New value string (optional)" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_edit_component(args, ctx).await }
        ),
        tool!(
            "find_component",
            "Find a footprint on the board by reference designator and return its position.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_find_component(args, ctx).await }
        ),
        tool!(
            "get_component_pads",
            "Return the pad positions and net assignments for a footprint.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_get_component_pads(args, ctx).await }
        ),
        tool!(
            "get_pad_position",
            "Return the schematic-space position of a specific pad number on a footprint.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "reference":   { "type": "string" },
                    "pad_number":  { "type": "string" }
                },
                "required": ["board", "reference", "pad_number"]
            }),
            |args, ctx| async move { handle_get_pad_position(args, ctx).await }
        ),
        tool!(
            "get_component_list",
            "List all footprints on the board with their positions, layers, and values.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_component_list(args, ctx).await }
        ),
        tool!(
            "place_component_array",
            "Place multiple copies of a footprint in a grid or line array via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string" },
                    "footprint":    { "type": "string" },
                    "start_x":      { "type": "number" },
                    "start_y":      { "type": "number" },
                    "count_x":      { "type": "integer", "description": "Number of columns" },
                    "count_y":      { "type": "integer", "description": "Number of rows", "default": 1 },
                    "spacing_x":    { "type": "number", "description": "Column spacing in mm" },
                    "spacing_y":    { "type": "number", "description": "Row spacing in mm", "default": 0 },
                    "ref_prefix":   { "type": "string", "description": "Reference prefix (e.g. 'R')", "default": "U" },
                    "ref_start":    { "type": "integer", "description": "Starting reference number", "default": 1 }
                },
                "required": ["board", "footprint", "start_x", "start_y", "count_x", "spacing_x"]
            }),
            |args, ctx| async move { handle_place_array(args, ctx).await }
        ),
        tool!(
            "align_components",
            "Align multiple footprints along a common X or Y axis via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "references":  { "type": "array", "items": { "type": "string" } },
                    "axis":        { "type": "string", "description": "'x' or 'y'", "default": "x" },
                    "value":       { "type": "number", "description": "Target coordinate to align to" }
                },
                "required": ["board", "references", "value"]
            }),
            |args, ctx| async move { handle_align_components(args, ctx).await }
        ),
        tool!(
            "duplicate_component",
            "Duplicate an existing footprint at a new position via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":         { "type": "string" },
                    "reference":     { "type": "string", "description": "Reference to duplicate" },
                    "new_reference": { "type": "string", "description": "New reference designator" },
                    "x":             { "type": "number" },
                    "y":             { "type": "number" }
                },
                "required": ["board", "reference", "new_reference", "x", "y"]
            }),
            |args, ctx| async move { handle_duplicate_component(args, ctx).await }
        ),
        tool!(
            "get_board_2d_view",
            "Render the PCB as a 2-D image using kicad-cli and return it as a base64 PNG.",
            json!({
                "type": "object",
                "properties": {
                    "board":  { "type": "string" },
                    "layers": {
                        "type": "array",
                        "description": "Layers to include (empty = default copper + silkscreen)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_board_2d_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_place_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    let source = match resolve_footprint_source(&footprint, &board) {
        Ok(source) => source,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let prepared = match prepare_footprint_source(
        &source, &footprint, &reference, None, x, y, rotation, &layer,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let pads = match extract_pad_definitions(&prepared) {
        Ok(pads) => pads,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };

    let value = footprint
        .split_once(':')
        .map(|(_, entry)| entry)
        .unwrap_or(&footprint)
        .to_string();
    let fp = ipc!(ctx, args, |c| c.place_footprint(
        &footprint, &reference, &value, &pads, x, y, rotation, &layer
    ));
    Ok(CallToolResult::json(&json!({
        "placed": fp.reference,
        "footprint": fp.footprint,
        "x": fp.position.x, "y": fp.position.y,
        "rotation": fp.rotation, "layer": fp.layer
    })))
}

async fn handle_move_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    ipc!(ctx, args, |c| c.move_footprint(&ref_ipc, x, y));
    Ok(CallToolResult::json(
        &json!({ "moved": reference, "x": x, "y": y }),
    ))
}

async fn handle_rotate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    ipc!(ctx, args, |c| c.rotate_footprint(&ref_ipc, rotation));
    Ok(CallToolResult::json(
        &json!({ "rotated": reference, "rotation": rotation }),
    ))
}

async fn handle_delete_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    ipc!(ctx, args, |c| c.delete_footprint(&ref_ipc));
    Ok(CallToolResult::json(&json!({ "deleted": reference })))
}

async fn handle_edit_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    if let Some(value) = args["value"].as_str() {
        let reference_for_ipc = reference.clone();
        let value_for_ipc = value.to_string();
        ipc!(ctx, args, |c| c
            .set_footprint_value(&reference_for_ipc, &value_for_ipc));
    }
    let lookup_reference = reference.clone();
    let fp = ipc!(ctx, args, |c| {
        c.get_footprint(&lookup_reference)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", lookup_reference))
    });
    Ok(CallToolResult::json(&json!({
        "reference": fp.reference,
        "value": fp.value,
        "footprint": fp.footprint
    })))
}

async fn handle_find_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let fp = ipc!(ctx, args, |c| {
        c.get_footprint(&reference)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", reference))
    });
    Ok(CallToolResult::json(&json!({
        "reference": fp.reference,
        "value": fp.value,
        "footprint": fp.footprint,
        "x": fp.position.x, "y": fp.position.y,
        "rotation": fp.rotation, "layer": fp.layer
    })))
}

async fn handle_get_component_pads(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    // Find the footprint with matching reference
    let fp_node = tree.find_all("footprint").into_iter().find(|fp| {
        fp.find_all("property").iter().any(|p| {
            p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                && p.get(2).and_then(|n| n.as_str()) == Some(reference.as_str())
        })
    });

    let fp_node = match fp_node {
        Some(n) => n,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found",
                reference
            )))
        }
    };

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pads: Vec<serde_json::Value> = fp_node
        .find_all("pad")
        .iter()
        .filter_map(|pad| {
            let number = pad.get(1)?.as_str()?.to_string();
            let pad_at = pad.find("at")?;
            let local_x = pad_at.get_f64(1)?;
            let local_y = pad_at.get_f64(2)?;
            // Transform local pad coords to board space (rotation only).
            // Uses the canonical KiCAD transform — see konnect_sexp::geometry.
            let (board_x, board_y) =
                konnect_sexp::geometry::transform_pad(local_x, local_y, fp_x, fp_y, fp_rot);
            let net = pad
                .find("net")
                .and_then(|n| n.get(2))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            Some(json!({ "number": number, "x": board_x, "y": board_y, "net": net }))
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "reference": reference, "pad_count": pads.len(), "pads": pads }),
    ))
}

async fn handle_get_pad_position(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let pad_number = match require_str(args, "pad_number") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pads_result = handle_get_component_pads(args, ctx).await?;
    // Parse the result and filter for the specific pad number
    if let Some(crate::mcp::protocol::ToolContent::Text { text }) = pads_result.content.first() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(pads) = parsed["pads"].as_array() {
                if let Some(pad) = pads
                    .iter()
                    .find(|p| p["number"].as_str() == Some(&pad_number))
                {
                    return Ok(CallToolResult::json(pad));
                }
            }
        }
    }
    Ok(CallToolResult::error(format!(
        "Pad '{}' not found",
        pad_number
    )))
}

async fn handle_get_component_list(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let fps = ipc!(ctx, _args, |c| c.list_footprints());
    let items: Vec<serde_json::Value> = fps
        .iter()
        .map(|fp| {
            json!({
                "reference": fp.reference,
                "value": fp.value,
                "footprint": fp.footprint,
                "x": fp.position.x, "y": fp.position.y,
                "rotation": fp.rotation, "layer": fp.layer
            })
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "components": items }),
    ))
}

async fn handle_place_array(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let start_x = match require_f64(args, "start_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let start_y = match require_f64(args, "start_y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let count_x = args["count_x"].as_u64().unwrap_or(1);
    let count_y = args["count_y"].as_u64().unwrap_or(1);
    let Some(total_count) = count_x.checked_mul(count_y) else {
        return Ok(CallToolResult::error("Array dimensions overflow."));
    };
    if count_x == 0 || count_y == 0 || total_count > 10_000 {
        return Ok(CallToolResult::error(
            "Array dimensions must be non-zero and contain at most 10,000 components.",
        ));
    }
    let count_x = count_x as usize;
    let count_y = count_y as usize;
    let spacing_x = match require_f64(args, "spacing_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let spacing_y = args["spacing_y"].as_f64().unwrap_or(spacing_x);
    let prefix = args["ref_prefix"].as_str().unwrap_or("U").to_string();
    let ref_start = args["ref_start"].as_u64().unwrap_or(1);
    if ref_start.checked_add(total_count - 1).is_none() {
        return Ok(CallToolResult::error("Reference number overflow."));
    }
    let source = match resolve_footprint_source(&footprint, &board) {
        Ok(source) => source,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };

    let value = footprint
        .split_once(':')
        .map(|(_, entry)| entry)
        .unwrap_or(&footprint)
        .to_string();
    let mut planned = Vec::with_capacity(total_count as usize);
    for row in 0..count_y {
        for col in 0..count_x {
            let x = start_x + col as f64 * spacing_x;
            let y = start_y + row as f64 * spacing_y;
            let reference = format!("{prefix}{}", ref_start + planned.len() as u64);
            let prepared = match prepare_footprint_source(
                &source, &footprint, &reference, None, x, y, 0.0, "F.Cu",
            ) {
                Ok(prepared) => prepared,
                Err(error) => return Ok(CallToolResult::error(error.to_string())),
            };
            let pads = match extract_pad_definitions(&prepared) {
                Ok(pads) => pads,
                Err(error) => return Ok(CallToolResult::error(error.to_string())),
            };
            planned.push((reference, pads, x, y));
        }
    }

    let requested_board = board.clone();
    let footprint_id = footprint.clone();
    let placed = match with_ipc(ctx.config.ipc_address.clone(), move |c| {
        c.ensure_board_is_active(&requested_board)?;
        let existing = c
            .list_footprints()?
            .into_iter()
            .map(|footprint| footprint.reference)
            .collect::<HashSet<_>>();
        let conflicts = planned
            .iter()
            .filter(|(reference, _, _, _)| existing.contains(reference))
            .map(|(reference, _, _, _)| reference.as_str())
            .collect::<Vec<_>>();
        if !conflicts.is_empty() {
            anyhow::bail!(
                "footprint references already exist on the board: {}",
                conflicts.join(", ")
            );
        }

        let items = planned
            .iter()
            .map(|(reference, pads, x, y)| {
                c.build_footprint_item(&footprint_id, reference, &value, pads, *x, *y, 0.0, "F.Cu")
                    .with_context(|| format!("failed to prepare {reference}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        c.run_commit("Place footprint array", |c| c.create_items(items))?;

        let mut created = c
            .list_footprints()?
            .into_iter()
            .map(|footprint| (footprint.reference.clone(), footprint))
            .collect::<HashMap<_, _>>();
        planned
            .into_iter()
            .map(|(reference, _, _, _)| {
                let footprint = created.remove(&reference).with_context(|| {
                    format!("committed footprint '{reference}' was not found on the board")
                })?;
                Ok(json!({
                    "reference": reference,
                    "x": footprint.position.x,
                    "y": footprint.position.y
                }))
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await?
    {
        Ok(placed) => placed,
        Err(error) => return Ok(CallToolResult::error(format!("IPC array error: {error:#}"))),
    };
    Ok(CallToolResult::json(
        &json!({ "placed_count": placed.len(), "components": placed }),
    ))
}

async fn handle_align_components(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let refs = match args["references"].as_array() {
        Some(references) if !references.is_empty() => references,
        _ => {
            return Ok(CallToolResult::error(
                "'references' must be a non-empty array.",
            ))
        }
    };
    let references = match refs
        .iter()
        .map(|reference| reference.as_str().map(String::from))
        .collect::<Option<Vec<_>>>()
    {
        Some(references) => references,
        None => return Ok(CallToolResult::error("Every reference must be a string.")),
    };
    let axis = args["axis"].as_str().unwrap_or("x").to_string();
    if axis != "x" && axis != "y" {
        return Ok(CallToolResult::error("'axis' must be either 'x' or 'y'."));
    }
    let value = match require_f64(args, "value") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let requested_board = board.clone();
    let aligned = match with_ipc(ctx.config.ipc_address.clone(), move |c| {
        c.ensure_board_is_active(&requested_board)?;
        c.run_commit("Align footprints", |c| {
            references
                .iter()
                .map(|reference| {
                    let footprint = c
                        .get_footprint(reference)?
                        .with_context(|| format!("footprint '{reference}' not found"))?;
                    let (x, y) = if axis == "y" {
                        (footprint.position.x, value)
                    } else {
                        (value, footprint.position.y)
                    };
                    c.move_footprint(reference, x, y)?;
                    Ok(json!({ "reference": reference, "x": x, "y": y }))
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
    })
    .await?
    {
        Ok(aligned) => aligned,
        Err(error) => return Ok(CallToolResult::error(format!("IPC align error: {error:#}"))),
    };
    Ok(CallToolResult::json(
        &json!({ "aligned_count": aligned.len(), "components": aligned }),
    ))
}

async fn handle_duplicate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_reference = match require_str(args, "new_reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    // Get the source footprint's footprint ID and rotation
    let ref_ipc = reference.clone();
    let src = ipc!(ctx, args, |c| {
        c.get_footprint(&ref_ipc)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", ref_ipc))
    });
    let source = match resolve_footprint_source(&src.footprint, &board) {
        Ok(source) => source,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let prepared = match prepare_footprint_source(
        &source,
        &src.footprint,
        &new_reference,
        Some(&src.value),
        x,
        y,
        src.rotation,
        &src.layer,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let ipc_reference = new_reference.clone();
    let fp_id = src.footprint.clone();
    let fp_value = src.value.clone();
    let fp_layer = src.layer.clone();
    let fp_rotation = src.rotation;
    let pads = match extract_pad_definitions(&prepared) {
        Ok(pads) => pads,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let fp = ipc!(ctx, args, |c| c.place_footprint(
        &fp_id,
        &ipc_reference,
        &fp_value,
        &pads,
        x,
        y,
        fp_rotation,
        &fp_layer
    ));
    Ok(CallToolResult::json(&json!({
        "duplicated_from": reference,
        "new_reference": fp.reference,
        "x": fp.position.x, "y": fp.position.y
    })))
}

async fn handle_get_board_2d_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    use base64::Engine;
    let board_path = get_path(args, "board")?;
    let layers: Vec<String> = args["layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "F.Cu".into(),
                "B.Cu".into(),
                "F.SilkS".into(),
                "B.SilkS".into(),
                "Edge.Cuts".into(),
            ]
        });

    // Keep generated output away from the project directory: a board named
    // `controller.kicad_pcb` may already have a user-owned
    // `controller.render.png`, and concurrent calls need distinct targets.
    let render_dir = tempfile::Builder::new()
        .prefix("konnect-pcb-render-")
        .tempdir()
        .context("failed to create temporary PCB render directory")?;
    let tmp = render_dir.path().join("render.png");
    let layer_refs: Vec<&str> = layers.iter().map(String::as_str).collect();
    super::cli::render_pcb_png(&ctx.config.kicad_cli, &board_path, &tmp, &layer_refs).await?;
    let bytes = tokio::fs::read(&tmp).await?;

    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(CallToolResult::image(b64, "image/png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOOTPRINT: &str = r#"(footprint "R_0402"
  (version 20240108)
  (generator pcbnew)
  (layer "F.Cu")
  (property "Reference" "REF**" (at 0 -1 0) (layer "F.SilkS"))
  (property "Value" "R_0402" (at 0 1 0) (layer "F.Fab"))
  (pad "1" smd roundrect (at -0.5 0) (size 0.5 0.5)
    (layers "F.Cu" "F.Paste" "F.Mask")))"#;

    #[test]
    fn prepares_complete_front_footprint() {
        let prepared = prepare_footprint_source(
            FOOTPRINT,
            "Resistor_SMD:R_0402",
            "R17",
            None,
            12.5,
            8.25,
            90.0,
            "F.Cu",
        )
        .unwrap();
        assert!(prepared.starts_with("(footprint \"Resistor_SMD:R_0402\""));
        assert!(prepared.contains("(property \"Reference\" \"R17\""));
        assert!(prepared.contains("(at 12.5 8.25 90)"));
        assert!(prepared.contains("(pad \"1\""));
        assert!(prepared.contains("(layers \"F.Cu\" \"F.Paste\" \"F.Mask\")"));
        let pads = extract_pad_definitions(&prepared).unwrap();
        assert_eq!(pads.len(), 1);
        assert_eq!(pads[0].number, "1");
        assert_eq!(pads[0].shape, "roundrect");
        assert_eq!(pads[0].layers, ["F.Cu", "F.Paste", "F.Mask"]);
    }

    #[test]
    fn flips_all_copper_side_layers_for_back_placement() {
        let prepared = prepare_footprint_source(
            FOOTPRINT,
            "Resistor_SMD:R_0402",
            "R18",
            Some("10k"),
            1.0,
            2.0,
            0.0,
            "B.Cu",
        )
        .unwrap();
        assert!(prepared.contains("(layer \"B.Cu\")"));
        assert!(prepared.contains("(layer \"B.SilkS\")"));
        assert!(prepared.contains("(layer \"B.Fab\")"));
        assert!(prepared.contains("(layers \"B.Cu\" \"B.Paste\" \"B.Mask\")"));
        assert!(prepared.contains("(property \"Value\" \"10k\""));
        assert!(!prepared.contains("__KONNECT_FRONT__"));
    }

    #[test]
    fn rejects_non_outer_copper_layer() {
        let error = prepare_footprint_source(
            FOOTPRINT,
            "Resistor_SMD:R_0402",
            "R19",
            None,
            0.0,
            0.0,
            0.0,
            "In1.Cu",
        )
        .unwrap_err();
        assert!(error.to_string().contains("F.Cu or B.Cu"));
    }
}
