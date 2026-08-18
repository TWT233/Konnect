//! `sch_batch` toolset — bulk/batch operations on schematic elements.
//!
//! **Critical invariant**: every write handler performs a single file read,
//! collects ALL mutations as `SexpEdit` values against the original content,
//! then calls `write_atomic` exactly once. This fixes the Python bug where
//! `batch_connect_to_net` did N separate read/write cycles.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    find_all_symbol_instance_blocks, get_path, opt_str, project_name_for, require_array,
    require_f64, require_str, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident, snap_point},
    schematic::{
        extract_labels, extract_lib_pins, extract_symbol_instances, extract_wires, find_lib_symbol,
        format_net_label, format_wire, pin_endpoint, pin_label_rotation, read_schematic,
    },
    writer::{
        apply_edits, find_block_with_leading_whitespace, find_direct_child_blocks,
        find_enclosing_direct_child_block, new_uuid, read_consistent, write_atomic_if_unchanged,
        SexpEdit,
    },
    SexpError,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::Path;

// Re-use the crate-internal net-graph primitives from sch_analysis.
use super::sch_analysis::build_net_graph;
// Re-use the single-item component placer and pin-to-pin router.
use super::sch_components::place_one_component;
use super::sch_wiring::{resolve_pin_endpoint, resolve_placed_pin, route_between};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "batch_connect_to_net",
            "Connect multiple component pins to a named net by adding net labels at each pin \
             endpoint. Single file read → all labels inserted → single file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "net_name": { "type": "string", "description": "Name of the net to connect pins to" },
                    "pins": {
                        "type": "array",
                        "description": "List of {reference, pin_number} objects to connect",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "pin_number": { "type": "string" }
                            },
                            "required": ["reference", "pin_number"]
                        }
                    }
                },
                "required": ["schematic", "net_name", "pins"]
            }),
            |args, ctx| async move { handle_batch_connect_to_net(args, ctx).await }
        ),
        tool!(
            "batch_place_components",
            "Place multiple symbols from KiCAD libraries in a single file read/write cycle. \
             Pass explicit references -- there is no auto-numbering; an omitted reference \
             becomes '?' like an eeschema-unannotated symbol, same as add_schematic_component.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "components": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "lib_id": { "type": "string" },
                                "x": { "type": "number" }, "y": { "type": "number" },
                                "rotation": { "type": "number", "default": 0 },
                                "reference": { "type": "string" },
                                "value": { "type": "string" },
                                "unit": { "type": "integer", "default": 1 }
                            },
                            "required": ["lib_id", "x", "y"]
                        }
                    }
                },
                "required": ["schematic", "components"]
            }),
            |args, ctx| async move { handle_batch_place_components(args, ctx).await }
        ),
        tool!(
            "batch_connect_pins",
            "Connect multiple component pin pairs by reference and pin number, in a single \
             file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "connections": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref1": { "type": "string" }, "pin1": { "type": "string" },
                                "ref2": { "type": "string" }, "pin2": { "type": "string" }
                            },
                            "required": ["ref1", "pin1", "ref2", "pin2"]
                        }
                    }
                },
                "required": ["schematic", "connections"]
            }),
            |args, ctx| async move { handle_batch_connect_pins(args, ctx).await }
        ),
        tool!(
            "batch_delete",
            "Delete multiple schematic items (wires, labels, junctions, components) by UUID \
             or component reference designator — single file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "uuids": {
                        "type": "array",
                        "description": "UUIDs of items to delete",
                        "items": { "type": "string" }
                    },
                    "references": {
                        "type": "array",
                        "description": "Component reference designators to delete",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_batch_delete(args, ctx).await }
        ),
        tool!(
            "bulk_move_schematic_components",
            "Move multiple components by a uniform dx/dy offset in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Reference designators to move",
                        "items": { "type": "string" }
                    },
                    "dx": { "type": "number", "description": "X offset in mm" },
                    "dy": { "type": "number", "description": "Y offset in mm" }
                },
                "required": ["schematic", "references", "dx", "dy"]
            }),
            |args, ctx| async move { handle_bulk_move(args, ctx).await }
        ),
        tool!(
            "batch_edit_schematic_components",
            "Apply field updates (Value, Footprint, custom properties) to multiple components \
             in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "edits": {
                        "type": "array",
                        "description": "List of {reference, value?, footprint?, fields?} edit objects",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "value": { "type": "string" },
                                "footprint": { "type": "string" },
                                "fields": {
                                    "type": "object",
                                    "description": "Additional property fields as key:value pairs"
                                }
                            },
                            "required": ["reference"]
                        }
                    }
                },
                "required": ["schematic", "edits"]
            }),
            |args, ctx| async move { handle_batch_edit(args, ctx).await }
        ),
        tool!(
            "batch_set_schematic_field_visibility",
            "Set placed Reference/Value field visibility on one or more schematic components \
             in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "reference_visible": { "type": "boolean" },
                                "value_visible": { "type": "boolean" }
                            },
                            "required": ["reference"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["schematic", "edits"]
            }),
            |args, ctx| async move { handle_batch_set_schematic_field_visibility(args, ctx).await }
        ),
        tool!(
            "batch_delete_schematic_components",
            "Delete multiple components by reference designator in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Reference designators to delete",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic", "references"]
            }),
            |args, ctx| async move { handle_batch_delete_components(args, ctx).await }
        ),
        tool!(
            "connect_passthrough",
            "Add a wire stub and matching net label at a point to route a signal through \
             a region without drawing a full wire path. Direction controls stub orientation.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "net_name": { "type": "string", "description": "Net name for the passthrough label" },
                    "x": { "type": "number", "description": "X position of the stub root in mm" },
                    "y": { "type": "number", "description": "Y position of the stub root in mm" },
                    "direction": {
                        "type": "string",
                        "description": "Stub direction. 'auto' (default) points it away from \
                                        the symbol body when a pin sits at (x, y), so the label \
                                        text does not run back across the symbol; it falls back \
                                        to 'right' on a bare point.",
                        "enum": ["auto", "right", "left", "up", "down"],
                        "default": "auto"
                    }
                },
                "required": ["schematic", "net_name", "x", "y"]
            }),
            |args, ctx| async move { handle_connect_passthrough(args, ctx).await }
        ),
        tool!(
            "add_schematic_text",
            "Add a text annotation (non-net label) to the schematic at a given position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "text": { "type": "string", "description": "Text content to add" },
                    "x": { "type": "number", "description": "X position in mm" },
                    "y": { "type": "number", "description": "Y position in mm" },
                    "size": { "type": "number", "description": "Font size in mm", "default": 1.27 },
                    "rotation": { "type": "number", "description": "Rotation in degrees", "default": 0 }
                },
                "required": ["schematic", "text", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_text(args, ctx).await }
        ),
        tool!(
            "get_schematic_layout",
            "Return a compact spatial summary of the schematic: component positions, \
             bounding box, and optionally wire segments and label locations.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "include_wires": { "type": "boolean", "description": "Include wire data", "default": true },
                    "include_labels": { "type": "boolean", "description": "Include label data", "default": true }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_layout(args, ctx).await }
        ),
        tool!(
            "validate_wire_connections",
            "Check all wire endpoints for floating ends (not connected to a pin, label, \
             or another wire). Reports each floating endpoint with its coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "tolerance": { "type": "number", "description": "Snap tolerance in mm", "default": 0.01 }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_wire_connections(args, ctx).await }
        ),
        tool!(
            "validate_component_connections",
            "Check that every non-passive pin on every component has at least one wire \
             or label connected. Reports unconnected pins with reference, pin number, \
             and schematic position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "ignore_power_pins": {
                        "type": "boolean",
                        "description": "Skip power-type pins in the check",
                        "default": false
                    },
                    "references": {
                        "type": "array",
                        "description": "Limit check to these reference designators (empty = all)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_component_connections(args, ctx).await }
        ),
    ]
}

// ─── Private helpers ──────────────────────────────────────────────────────────

/// Find every `(symbol ...)` block for a reference designator, each with its
/// leading whitespace so deletion leaves clean formatting.
///
/// One entry per unit: deleting a multi-unit part means deleting all of them.
/// Returns `(block_start, block_end)` byte offsets in `content`.
fn find_symbol_blocks(content: &str, reference: &str) -> Vec<(usize, usize)> {
    find_all_symbol_instance_blocks(content, reference)
        .into_iter()
        .filter_map(|(sym_start, _)| find_block_with_leading_whitespace(content, sym_start))
        .collect()
}

/// Return `(val_start, val_end)` byte offsets in `content` for the *value* portion
/// of a `(property "FieldName" "VALUE" ...)` node, once per placed instance of
/// `reference`. Only the bytes inside the opening quote are included (i.e. the
/// replacement does NOT need to include surrounding quotes).
///
/// Multi-unit parts repeat their fields in every unit's block and KiCad expects
/// those copies to agree, so a field edit has to rewrite all of them.
fn field_value_ranges(content: &str, reference: &str, field: &str) -> Vec<(usize, usize)> {
    find_all_symbol_instance_blocks(content, reference)
        .into_iter()
        .filter_map(|(sym_start, sym_end)| {
            let sym_block = &content[sym_start..sym_end];

            let field_search = format!(r#"(property "{field}" ""#);
            let field_rel = sym_block.find(&field_search)?;
            let val_start = sym_start + field_rel + field_search.len();
            // find the closing quote of the current value
            let val_end = val_start + content[val_start..].find('"')?;
            Some((val_start, val_end))
        })
        .collect()
}

#[derive(Debug, Clone)]
struct FieldVisibilityRequest {
    reference: String,
    reference_visible: Option<bool>,
    value_visible: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
struct VisibilityTransition {
    old: bool,
    new: bool,
}

#[derive(Debug)]
struct PreparedFieldVisibilityUpdate {
    content: String,
    updated_count: usize,
    unchanged_count: usize,
    results: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FieldVisibilityKind {
    Reference,
    Value,
}

impl FieldVisibilityKind {
    fn property_name(self) -> &'static str {
        match self {
            Self::Reference => "Reference",
            Self::Value => "Value",
        }
    }

    fn result_key(self) -> &'static str {
        match self {
            Self::Reference => "reference_visible",
            Self::Value => "value_visible",
        }
    }
}

#[derive(Debug)]
struct PropertyVisibilityState {
    visible: bool,
    hide_start: Option<usize>,
    effects_start: Option<usize>,
    closing_line_start: usize,
    child_indent: String,
}

fn invalid_field_visibility_arg(reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        ToolErrorKind::InvalidArgument {
            field: "edits".to_string(),
            reason: reason.clone(),
        },
        format!("Argument 'edits' is invalid: {reason}"),
    )
}

fn conflict_result(path: &Path) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::Conflict {
            paths: vec![path.display().to_string()],
        },
        format!(
            "Write conflict: '{}' changed since it was read; reload and retry.",
            path.display()
        ),
    )
}

fn parse_field_visibility_requests(
    edits: &[serde_json::Value],
) -> Result<Vec<FieldVisibilityRequest>, CallToolResult> {
    let mut requests = Vec::with_capacity(edits.len());
    let mut seen = HashSet::new();

    for edit in edits {
        let Some(object) = edit.as_object() else {
            return Err(invalid_field_visibility_arg(
                "each edit must be an object with reference and optional visibility keys",
            ));
        };

        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "reference" | "reference_visible" | "value_visible"
            ) {
                return Err(invalid_field_visibility_arg(format!(
                    "unknown key '{}' in visibility edit",
                    key
                )));
            }
        }

        let Some(reference) = object.get("reference").and_then(|value| value.as_str()) else {
            return Err(invalid_field_visibility_arg(
                "each edit must include string field 'reference'",
            ));
        };

        let reference_visible = match object.get("reference_visible") {
            Some(value) => Some(value.as_bool().ok_or_else(|| {
                invalid_field_visibility_arg(
                    "field 'reference_visible' must be boolean when present",
                )
            })?),
            None => None,
        };
        let value_visible = match object.get("value_visible") {
            Some(value) => Some(value.as_bool().ok_or_else(|| {
                invalid_field_visibility_arg("field 'value_visible' must be boolean when present")
            })?),
            None => None,
        };

        if reference_visible.is_none() && value_visible.is_none() {
            return Err(invalid_field_visibility_arg(format!(
                "edit for '{}' must set at least one of reference_visible or value_visible",
                reference
            )));
        }

        if !seen.insert(reference.to_string()) {
            return Err(invalid_field_visibility_arg(format!(
                "duplicate edit for reference '{}'",
                reference
            )));
        }

        requests.push(FieldVisibilityRequest {
            reference: reference.to_string(),
            reference_visible,
            value_visible,
        });
    }

    Ok(requests)
}

fn property_name_from_block(block: &str) -> Option<&str> {
    let rest = block.strip_prefix("(property ")?;
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn line_indent(content: &str, start: usize) -> String {
    let line_start = content[..start].rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    content[line_start..start]
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect()
}

fn property_visibility_state(
    content: &str,
    property_start: usize,
    property_end: usize,
) -> Result<PropertyVisibilityState, String> {
    let property = &content[property_start..property_end];
    let direct_children = find_direct_child_blocks(property, "property");
    let closing_line_start = content[..property_end - 1]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(property_end - 1);
    let child_indent = direct_children
        .last()
        .map(|(start, _)| line_indent(property, *start))
        .unwrap_or_else(|| {
            let closing_indent = &content[closing_line_start..property_end - 1];
            format!("{closing_indent}  ")
        });

    let mut hide_start = None;
    let mut effects_start = None;
    for (child_start, child_end) in direct_children {
        let child = &property[child_start..child_end];
        match sexp_tag(child) {
            "hide" => {
                if hide_start.is_some() {
                    return Err("property contains multiple direct hide nodes".to_string());
                }
                if child.trim() != "(hide yes)" {
                    return Err(format!(
                        "property has malformed direct hide node '{}'",
                        child.trim()
                    ));
                }
                hide_start = Some(property_start + child_start);
            }
            "effects" if effects_start.is_none() => {
                effects_start = Some(property_start + child_start);
            }
            _ => {}
        }
    }

    Ok(PropertyVisibilityState {
        visible: hide_start.is_none(),
        hide_start,
        effects_start,
        closing_line_start,
        child_indent,
    })
}

fn symbol_property_range(
    content: &str,
    symbol_start: usize,
    symbol_end: usize,
    property_name: &str,
) -> Result<(usize, usize), String> {
    let symbol = &content[symbol_start..symbol_end];
    let mut matches = Vec::new();

    for (child_start, child_end) in find_direct_child_blocks(symbol, "symbol") {
        let child = &symbol[child_start..child_end];
        if sexp_tag(child) == "property" && property_name_from_block(child) == Some(property_name) {
            matches.push((symbol_start + child_start, symbol_start + child_end));
        }
    }

    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!("property '{}' not found", property_name)),
        _ => Err(format!(
            "property '{}' appears multiple times",
            property_name
        )),
    }
}

fn visibility_edits_for_field(
    content: &str,
    reference: &str,
    kind: FieldVisibilityKind,
    target_visible: bool,
) -> Result<(VisibilityTransition, Vec<SexpEdit>), String> {
    let symbol_blocks = find_all_symbol_instance_blocks(content, reference);
    if symbol_blocks.is_empty() {
        return Err(format!("component '{}' not found", reference));
    }

    let mut edits = Vec::new();
    let mut current_visible: Option<bool> = None;

    for (symbol_start, symbol_end) in symbol_blocks {
        let (property_start, property_end) =
            symbol_property_range(content, symbol_start, symbol_end, kind.property_name())
                .map_err(|reason| format!("component '{}' {}", reference, reason))?;
        let state =
            property_visibility_state(content, property_start, property_end).map_err(|e| {
                format!(
                    "component '{}' property '{}' {}",
                    reference,
                    kind.property_name(),
                    e
                )
            })?;

        if let Some(previous) = current_visible {
            if previous != state.visible {
                return Err(format!(
                    "component '{}' property '{}' differs across units",
                    reference,
                    kind.property_name()
                ));
            }
        } else {
            current_visible = Some(state.visible);
        }

        match (state.visible, target_visible) {
            (true, false) => {
                if let Some(effects_start) = state.effects_start {
                    let indent = line_indent(content, effects_start);
                    edits.push(SexpEdit::insert(
                        effects_start,
                        format!("(hide yes)\n{indent}"),
                    ));
                } else {
                    edits.push(SexpEdit::insert(
                        state.closing_line_start,
                        format!("{}(hide yes)\n", state.child_indent),
                    ));
                }
            }
            (false, true) => {
                let hide_start = state.hide_start.ok_or_else(|| {
                    format!(
                        "component '{}' property '{}' expected direct hide node",
                        reference,
                        kind.property_name()
                    )
                })?;
                let (delete_start, delete_end) =
                    find_block_with_leading_whitespace(content, hide_start).ok_or_else(|| {
                        format!(
                            "component '{}' property '{}' direct hide node could not be removed",
                            reference,
                            kind.property_name()
                        )
                    })?;
                edits.push(SexpEdit::delete(delete_start, delete_end));
            }
            _ => {}
        }
    }

    let old = current_visible.unwrap_or(true);
    Ok((
        VisibilityTransition {
            old,
            new: target_visible,
        },
        edits,
    ))
}

fn prepare_field_visibility_update(
    content: &str,
    requests: &[FieldVisibilityRequest],
) -> Result<PreparedFieldVisibilityUpdate, String> {
    let mut edits = Vec::new();
    let mut updated_count = 0usize;
    let mut unchanged_count = 0usize;
    let mut results = Vec::with_capacity(requests.len());

    for request in requests {
        let mut field_results = HashMap::new();
        let mut request_changed = false;

        for (kind, target_visible) in [
            (FieldVisibilityKind::Reference, request.reference_visible),
            (FieldVisibilityKind::Value, request.value_visible),
        ] {
            let Some(target_visible) = target_visible else {
                continue;
            };
            let (transition, field_edits) =
                visibility_edits_for_field(content, &request.reference, kind, target_visible)?;
            request_changed |= transition.old != transition.new;
            edits.extend(field_edits);
            field_results.insert(
                kind.result_key(),
                json!({
                    "old": transition.old,
                    "new": transition.new,
                }),
            );
        }

        let mut result = serde_json::Map::new();
        result.insert("reference".to_string(), json!(request.reference));
        if let Some(value) = field_results.remove("reference_visible") {
            result.insert("reference_visible".to_string(), value);
        }
        if let Some(value) = field_results.remove("value_visible") {
            result.insert("value_visible".to_string(), value);
        }
        results.push(serde_json::Value::Object(result));

        if request_changed {
            updated_count += 1;
        } else {
            unchanged_count += 1;
        }
    }

    let new_content = apply_edits(content.to_string(), edits);
    Ok(PreparedFieldVisibilityUpdate {
        content: new_content,
        updated_count,
        unchanged_count,
        results,
    })
}

fn persist_field_visibility_update(
    path: &Path,
    expected: &str,
    replacement: &str,
) -> CallToolResult {
    match write_atomic_if_unchanged(path, expected, replacement) {
        Ok(()) => CallToolResult::text("ok"),
        Err(SexpError::Conflict { .. }) => conflict_result(path),
        Err(error) => CallToolResult::error(format!(
            "Failed to persist field visibility update to '{}': {error}",
            path.display()
        )),
    }
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_batch_set_schematic_field_visibility(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let edits = match require_array(args, "edits") {
        Ok(edits) => edits,
        Err(error) => return Ok(error),
    };
    let requests = match parse_field_visibility_requests(edits) {
        Ok(requests) => requests,
        Err(error) => return Ok(error),
    };

    let content = read_consistent(&sch_path)?;
    let prepared = match prepare_field_visibility_update(&content, &requests) {
        Ok(prepared) => prepared,
        Err(reason) => return Ok(invalid_field_visibility_arg(reason)),
    };

    if prepared.content != content {
        let persisted = persist_field_visibility_update(&sch_path, &content, &prepared.content);
        if persisted.is_error {
            return Ok(persisted);
        }
    }

    Ok(CallToolResult::json(&json!({
        "updated_count": prepared.updated_count,
        "unchanged_count": prepared.unchanged_count,
        "results": prepared.results,
    })))
}

async fn handle_batch_connect_to_net(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pins = match args["pins"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'pins' array")),
    };

    let (content, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut inserts = String::new();
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    // Endpoints already carrying this net's label, so a second never lands
    // on the first. Seeded from the file, extended as we go.
    let mut labelled: Vec<(f64, f64)> = extract_labels(&tree)
        .iter()
        .filter(|l| l.net == net_name)
        .map(|l| (l.x, l.y))
        .collect();

    for pin_spec in &pins {
        let reference = match pin_spec["reference"].as_str() {
            Some(r) => r,
            None => {
                errors.push("Missing 'reference' in pin spec".into());
                continue;
            }
        };
        let pin_number = match pin_spec["pin_number"].as_str() {
            Some(p) => p,
            None => {
                errors.push("Missing 'pin_number' in pin spec".into());
                continue;
            }
        };

        let (pin, t) = match resolve_placed_pin(&instances, &lib_syms, reference, pin_number) {
            Ok(p) => p,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        let (px, py) = pin_endpoint(&pin, t);
        let rotation = pin_label_rotation(&pin, t);

        // Symbols stack several pins on one endpoint; a label each renders as
        // a smear. They stay connected by that endpoint.
        let duplicate = labelled
            .iter()
            .any(|(lx, ly)| points_coincident(*lx, *ly, px, py, 0.01));
        if !duplicate {
            inserts.push_str(&format_net_label(&net_name, px, py, rotation));
            labelled.push((px, py));
        }
        let mut entry = json!({
            "reference": reference,
            "pin": pin_number,
            "x": px,
            "y": py,
            "rotation": rotation
        });
        if duplicate {
            entry["deduplicated"] = json!(true);
        }
        added.push(entry);
    }

    if !inserts.is_empty() {
        let expected = content.clone();
        // Labels are element class 2; symbol instances MUST come last, so a
        // splice at the file's final `)` puts them after the instances and
        // KiCad refuses the whole file (#156, same bug as add_schematic_text).
        let new_content = crate::tools::sch_wiring::insert_before_close(&content, &inserts);
        write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    }

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "added": added,
        "added_count": added.len(),
        "errors": errors
    })))
}

/// Extract the message text out of a `CallToolResult` error, for folding a
/// single-item handler's structured error into a batch tool's `errors` list.
fn error_text(result: &CallToolResult) -> String {
    match result.content.first() {
        Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
        _ => "unknown error".to_string(),
    }
}

async fn handle_batch_place_components(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let components = match args["components"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'components' array")),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    let root_uuid = crate::tools::ensure_root_uuid(&mut sch);
    let project_name = project_name_for(&sch_path);
    // Built once: the lib-table parse is memoised across the whole batch.
    let src = crate::tools::library::KiCadSymbolSource::for_file(&sch_path);

    let mut placed: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for comp in &components {
        let Some(lib_id) = comp["lib_id"].as_str() else {
            errors.push("Missing 'lib_id' in component spec".into());
            continue;
        };
        let (Some(x), Some(y)) = (comp["x"].as_f64(), comp["y"].as_f64()) else {
            errors.push(format!("Missing 'x'/'y' for '{}'", lib_id));
            continue;
        };
        let rotation = comp["rotation"].as_f64().unwrap_or(0.0);
        let reference = comp["reference"].as_str().unwrap_or("?");
        let value = comp["value"].as_str();
        let unit = comp["unit"].as_f64().unwrap_or(1.0) as u32;

        match place_one_component(
            &mut sch,
            &root_uuid,
            &project_name,
            lib_id,
            x,
            y,
            rotation,
            reference,
            value,
            unit,
            &src,
        ) {
            Ok(v) => placed.push(v),
            Err(e) => errors.push(error_text(&e)),
        }
    }

    if !placed.is_empty() {
        sch.overwrite()?;
    }

    let mut result = CallToolResult::json(&json!({
        "placed": placed,
        "placed_count": placed.len(),
        "errors": errors
    }));
    result.is_error = placed.is_empty() && !errors.is_empty();
    Ok(result)
}

async fn handle_batch_connect_pins(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let connections = match args["connections"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'connections' array")),
    };

    let (content, tree) = read_schematic(&sch_path)?;
    let expected = content.clone();
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Resolve every endpoint from the initial tree before any wire is
    // inserted -- symbols/lib_symbols never change as wires are added, so
    // this is safe to do up front instead of re-resolving per connection.
    let mut resolved: Vec<(f64, f64, f64, f64)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for conn in &connections {
        let (Some(ref1), Some(pin1), Some(ref2), Some(pin2)) = (
            conn["ref1"].as_str(),
            conn["pin1"].as_str(),
            conn["ref2"].as_str(),
            conn["pin2"].as_str(),
        ) else {
            errors.push("Missing ref1/pin1/ref2/pin2 in connection spec".into());
            continue;
        };
        match (
            resolve_pin_endpoint(&instances, &lib_syms, ref1, pin1),
            resolve_pin_endpoint(&instances, &lib_syms, ref2, pin2),
        ) {
            (Ok((x1, y1)), Ok((x2, y2))) => resolved.push((x1, y1, x2, y2)),
            (Err(e), _) | (_, Err(e)) => errors.push(e.to_string()),
        }
    }

    // ponytail: re-parses content per wire; incremental tree edits if batches get huge.
    let mut new_content = content;
    for (x1, y1, x2, y2) in &resolved {
        new_content = route_between(new_content, *x1, *y1, *x2, *y2);
    }

    if !resolved.is_empty() {
        write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    }

    let mut result = CallToolResult::json(&json!({
        "connected_count": resolved.len(),
        "errors": errors
    }));
    result.is_error = resolved.is_empty() && !errors.is_empty();
    Ok(result)
}

async fn handle_batch_delete(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut delete_ranges: HashSet<(usize, usize)> = HashSet::new();

    // Delete by UUID — walk back from uuid node to enclosing top-level block
    if let Some(uuids) = args["uuids"].as_array() {
        for uuid_val in uuids {
            let uuid = match uuid_val.as_str() {
                Some(u) => u,
                None => continue,
            };
            let pattern = format!(r#"(uuid "{}")"#, uuid);
            match content.find(&pattern) {
                Some(uuid_pos) => {
                    match find_enclosing_direct_child_block(&content, "kicad_sch", uuid_pos) {
                        Some((block_start, block_end)) => {
                            let item = &content[block_start..block_end];
                            if !is_deletable_schematic_item(item) {
                                errors.push(format!(
                                    "UUID '{}' belongs to protected schematic structure '{}'",
                                    uuid,
                                    sexp_tag(item)
                                ));
                                continue;
                            }
                            match find_block_with_leading_whitespace(&content, block_start) {
                                Some((del_start, del_end)) => {
                                    if delete_ranges.insert((del_start, del_end)) {
                                        edits.push(SexpEdit::delete(del_start, del_end));
                                        deleted.push(uuid.to_string());
                                    }
                                }
                                None => {
                                    errors.push(format!("Cannot parse block for UUID '{}'", uuid))
                                }
                            }
                        }
                        None => errors.push(format!("Cannot locate block for UUID '{}'", uuid)),
                    }
                }
                None => errors.push(format!("UUID '{}' not found", uuid)),
            }
        }
    }

    // Delete by reference designator
    if let Some(refs) = args["references"].as_array() {
        for ref_val in refs {
            let reference = match ref_val.as_str() {
                Some(r) => r,
                None => continue,
            };
            let blocks = find_symbol_blocks(&content, reference);
            if blocks.is_empty() {
                errors.push(format!("Component '{}' not found", reference));
                continue;
            }
            // Every unit of a multi-unit part, or the whole component is not gone.
            let mut any = false;
            for (del_start, del_end) in blocks {
                if delete_ranges.insert((del_start, del_end)) {
                    edits.push(SexpEdit::delete(del_start, del_end));
                    any = true;
                }
            }
            if any {
                deleted.push(reference.to_string());
            }
        }
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "errors": errors
    })))
}

fn sexp_tag(block: &str) -> &str {
    let Some(after_open) = block.strip_prefix('(') else {
        return "";
    };
    let end = after_open
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(after_open.len());
    &after_open[..end]
}

// Blocklist of structural forms, not an allowlist of item kinds: deleting a
// drawing item (text, bus, sheet, image, polyline, …) by UUID has always
// worked and must keep working — only the schematic's skeleton is protected.
fn is_deletable_schematic_item(block: &str) -> bool {
    !matches!(
        sexp_tag(block),
        "version"
            | "generator"
            | "generator_version"
            | "uuid"
            | "paper"
            | "title_block"
            | "lib_symbols"
            | "sheet_instances"
            | "symbol_instances"
            | "embedded_fonts"
    )
}

/// Edits translating every `(property …)` anchor inside the symbol block at
/// `sym_start..sym_end` by `(ddx, ddy)`.
///
/// A property's own rotation is left untouched: a translation does not turn
/// text. Block starts come from `find_block_starts`, which is string-aware, so
/// a property *value* containing `(property` cannot be mistaken for one.
fn property_translation_edits(
    content: &str,
    sym_start: usize,
    sym_end: usize,
    ddx: f64,
    ddy: f64,
) -> Vec<SexpEdit> {
    if ddx == 0.0 && ddy == 0.0 {
        return Vec::new();
    }
    let mut edits = Vec::new();
    for prop_start in konnect_sexp::writer::find_block_starts(content, "property") {
        if prop_start < sym_start || prop_start >= sym_end {
            continue;
        }
        let Some((_, prop_end)) = konnect_sexp::writer::find_balanced_block(content, prop_start)
        else {
            continue;
        };
        let prop = &content[prop_start..prop_end];
        // The property's own (at …), not one nested deeper in (effects …).
        let Some(at_rel) = prop.find("(at ") else {
            continue;
        };
        let at_abs = prop_start + at_rel + "(at ".len();
        let Some(close_rel) = prop[at_rel..].find(')') else {
            continue;
        };
        let at_end = prop_start + at_rel + close_rel;
        let parts: Vec<&str> = content[at_abs..at_end].split_whitespace().collect();
        let (Some(px), Some(py)) = (
            parts.first().and_then(|s| s.parse::<f64>().ok()),
            parts.get(1).and_then(|s| s.parse::<f64>().ok()),
        ) else {
            continue;
        };
        let rot = parts.get(2).copied().unwrap_or("0");
        edits.push(SexpEdit::replace(
            at_abs,
            at_end,
            format!(
                "{} {} {rot}",
                cse::types::fmt_f64(px + ddx),
                cse::types::fmt_f64(py + ddy)
            ),
        ));
    }
    edits
}

async fn handle_bulk_move(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = match require_array(args, "references") {
        Ok(a) => a.clone(),
        Err(e) => return Ok(e),
    };
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut moved: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };

        // Every placement of this reference — a multi-unit part has one block
        // per unit, and shifting only the first would tear the part apart.
        let blocks = find_all_symbol_instance_blocks(&content, reference);
        if blocks.is_empty() {
            errors.push(format!("'{}' not found", reference));
            continue;
        }

        let mut placements: Vec<serde_json::Value> = Vec::new();
        for (sym_start, sym_end) in blocks {
            // Find first (at X Y [ROT]) inside this symbol block
            let sym_block = &content[sym_start..sym_end];
            let at_pat = "(at ";
            let at_rel = match sym_block.find(at_pat) {
                Some(r) => r,
                None => {
                    errors.push(format!("No (at) in symbol '{}'", reference));
                    continue;
                }
            };
            let at_abs = sym_start + at_rel + at_pat.len();
            let close_rel = sym_block[at_rel..].find(')').unwrap_or(0);
            let at_end = sym_start + at_rel + close_rel;

            let at_str = &content[at_abs..at_end];
            let parts: Vec<&str> = at_str.split_whitespace().collect();
            let x = parts
                .first()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let y = parts
                .get(1)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let rot = parts
                .get(2)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);

            let (new_x, new_y) = snap_point(x + dx, y + dy, 1.27);
            edits.push(SexpEdit::replace(
                at_abs,
                at_end,
                format!("{new_x} {new_y} {rot}"),
            ));
            // Property coordinates are ABSOLUTE in .kicad_sch, so the field
            // text does not follow the symbol on its own — moving only the
            // symbol's own (at …) strands Reference and Value at the old
            // location (#202). Shift them by the delta the symbol *actually*
            // moved, which is the snapped one, or they drift relative to the
            // part. `Symbol::translate` does the same on the typed path.
            edits.extend(property_translation_edits(
                &content,
                sym_start,
                sym_end,
                new_x - x,
                new_y - y,
            ));
            placements.push(json!({
                "old_x": x, "old_y": y,
                "new_x": new_x, "new_y": new_y
            }));
        }

        if !placements.is_empty() {
            moved.push(json!({
                "reference": reference,
                "units": placements.len(),
                "placements": placements
            }));
        }
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved,
        "dx": dx, "dy": dy,
        "errors": errors
    })))
}

async fn handle_batch_edit(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let edits_arr = match args["edits"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'edits' array")),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut file_edits: Vec<SexpEdit> = Vec::new();
    let mut changed: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for edit_spec in &edits_arr {
        let reference = match edit_spec["reference"].as_str() {
            Some(r) => r,
            None => {
                errors.push("Missing 'reference' in edit spec".into());
                continue;
            }
        };

        let mut component_changes: Vec<String> = Vec::new();

        // Standard fields, then arbitrary extra fields from the "fields" object.
        // Each is rewritten in every unit's block, which is where a multi-unit
        // part keeps its copies of the value.
        let extra = edit_spec["fields"].as_object();
        let specs = [("Value", "value"), ("Footprint", "footprint")]
            .into_iter()
            .filter_map(|(field, key)| Some((field.to_string(), edit_spec[key].as_str()?)))
            .chain(
                extra
                    .into_iter()
                    .flatten()
                    .filter_map(|(name, val)| Some((name.clone(), val.as_str()?))),
            );

        for (field, new_val) in specs {
            let ranges = field_value_ranges(&content, reference, &field);
            if ranges.is_empty() {
                errors.push(format!("Field '{}' not found on '{}'", field, reference));
                continue;
            }
            let units = ranges.len();
            for (start, end) in ranges {
                file_edits.push(SexpEdit::replace(start, end, new_val.to_string()));
            }
            component_changes.push(if units > 1 {
                format!("{} → {} ({} units)", field, new_val, units)
            } else {
                format!("{} → {}", field, new_val)
            });
        }

        if !component_changes.is_empty() {
            changed.push(json!({
                "reference": reference,
                "changes": component_changes
            }));
        }
    }

    let new_content = apply_edits(content, file_edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "updated_count": changed.len(),
        "updated": changed,
        "errors": errors
    })))
}

async fn handle_batch_delete_components(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = match args["references"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'references' array")),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };
        let blocks = find_symbol_blocks(&content, reference);
        if blocks.is_empty() {
            errors.push(format!("Component '{}' not found", reference));
            continue;
        }
        // Every unit of a multi-unit part, or the whole component is not gone.
        for (del_start, del_end) in blocks {
            edits.push(SexpEdit::delete(del_start, del_end));
        }
        deleted.push(reference.to_string());
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "errors": errors
    })))
}

async fn handle_connect_passthrough(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net_name = match require_str(args, "net_name") {
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
    let direction = opt_str(args, "direction").unwrap_or("auto");

    let (content, tree) = read_schematic(&sch_path)?;
    let dir = crate::tools::resolve_stub_direction(direction, (x, y), &tree);

    // Stub is 2.54mm (2×1.27 grid units)
    let stub = 2.54_f64;
    let (wire_end_x, wire_end_y) = (x + dir.dx * stub, y + dir.dy * stub);

    let wire_sexp = format_wire(x, y, wire_end_x, wire_end_y);
    let label_sexp = format_net_label(&net_name, wire_end_x, wire_end_y, dir.label_rotation);

    let expected = content.clone();
    // Wires and labels are element class 2; symbol instances MUST come last.
    let new_content = crate::tools::sch_wiring::insert_before_close(
        &content,
        &format!("{wire_sexp}{label_sexp}"),
    );
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "stub_root": { "x": x, "y": y },
        "label_position": { "x": wire_end_x, "y": wire_end_y },
        "direction": dir.name,
        "label_rotation": dir.label_rotation
    })))
}

async fn handle_add_schematic_text(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let text = match require_str(args, "text") {
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
    let size = args["size"].as_f64().unwrap_or(1.27);
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let uuid = new_uuid();

    // Escape for a KiCad quoted string. Newlines and tabs must become their
    // two-character escapes: KiCad's reader rejects a literal newline inside
    // quotes, and it fails at the *file* level — a multi-line annotation makes
    // the whole schematic unloadable with only "Failed to load schematic".
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "")
        .replace('\n', "\\n")
        .replace('\t', "\\t");

    let text_sexp = format!(
        "\n  (text \"{escaped}\"\n    (at {x} {y} {rotation})\n    \
         (effects (font (size {size} {size})))\n    (uuid \"{uuid}\")\n  )"
    );

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    // Before the first symbol instance, not at the end of the file: KiCad 10
    // requires symbol instances to come last and refuses to load a schematic
    // with a `(text …)` after them.
    let new_content = crate::tools::sch_wiring::insert_before_close(&content, &text_sexp);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added": text,
        "x": x, "y": y,
        "size": size,
        "rotation": rotation,
        "uuid": uuid
    })))
}

async fn handle_get_layout(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let include_wires = args["include_wires"].as_bool().unwrap_or(true);
    let include_labels = args["include_labels"].as_bool().unwrap_or(true);

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);

    let components: Vec<serde_json::Value> = instances
        .iter()
        .map(|i| {
            json!({
                "reference": i.reference,
                "value": i.value,
                "lib_id": i.lib_id,
                "x": i.x, "y": i.y,
                "rotation": i.rotation,
                "mirror_x": i.mirror_x,
                "mirror_y": i.mirror_y
            })
        })
        .collect();

    // Bounding box over component origins
    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
    for i in &instances {
        min_x = min_x.min(i.x);
        min_y = min_y.min(i.y);
        max_x = max_x.max(i.x);
        max_y = max_y.max(i.y);
    }
    let bbox = if instances.is_empty() {
        json!({ "x_min": 0, "y_min": 0, "x_max": 0, "y_max": 0 })
    } else {
        json!({ "x_min": min_x, "y_min": min_y, "x_max": max_x, "y_max": max_y })
    };

    let mut result = json!({
        "component_count": components.len(),
        "components": components,
        "bounding_box": bbox
    });

    if include_wires {
        let wires = extract_wires(&tree);
        let wire_data: Vec<serde_json::Value> = wires
            .iter()
            .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2, "uuid": w.uuid }))
            .collect();
        result["wire_count"] = json!(wire_data.len());
        result["wires"] = json!(wire_data);
    }

    if include_labels {
        let labels = extract_labels(&tree);
        let label_data: Vec<serde_json::Value> = labels
            .iter()
            .map(|l| json!({ "net": l.net, "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
            .collect();
        result["label_count"] = json!(label_data.len());
        result["labels"] = json!(label_data);
    }

    Ok(CallToolResult::json(&result))
}

async fn handle_validate_wire_connections(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tol = args["tolerance"].as_f64().unwrap_or(0.01);

    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Collect all valid pin endpoints
    let mut pin_points: Vec<(f64, f64)> = Vec::new();
    for inst in &instances {
        let lib_sym = find_lib_symbol(&lib_syms, inst);
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            for pin in extract_lib_pins(sym) {
                pin_points.push(pin_endpoint(&pin, t));
            }
        }
    }

    let label_points: Vec<(f64, f64)> = labels.iter().map(|l| (l.x, l.y)).collect();
    // All wire endpoints as a flat list (for quick counting)
    let all_wire_eps: Vec<(f64, f64)> = wires
        .iter()
        .flat_map(|w| [(w.x1, w.y1), (w.x2, w.y2)])
        .collect();

    let is_connected = |px: f64, py: f64| -> bool {
        // Another wire endpoint at the same position (count >= 2 because px/py itself is in the list)
        let same_ep_count = all_wire_eps
            .iter()
            .filter(|(wx, wy)| points_coincident(px, py, *wx, *wy, tol))
            .count();
        if same_ep_count >= 2 {
            return true;
        }

        // T-junction: lies on the INTERIOR of another wire
        if wires.iter().any(|w| {
            point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, tol)
                && !points_coincident(px, py, w.x1, w.y1, tol)
                && !points_coincident(px, py, w.x2, w.y2, tol)
        }) {
            return true;
        }

        // Label at this point
        if label_points
            .iter()
            .any(|(lx, ly)| points_coincident(px, py, *lx, *ly, tol))
        {
            return true;
        }

        // Pin endpoint at this point
        if pin_points
            .iter()
            .any(|(ppx, ppy)| points_coincident(px, py, *ppx, *ppy, tol))
        {
            return true;
        }

        false
    };

    let mut floating: Vec<serde_json::Value> = Vec::new();
    for w in &wires {
        for (px, py) in [(w.x1, w.y1), (w.x2, w.y2)] {
            if !is_connected(px, py) {
                floating.push(json!({ "x": px, "y": py, "wire_uuid": w.uuid }));
            }
        }
    }

    Ok(CallToolResult::json(&json!({
        "valid": floating.is_empty(),
        "floating_count": floating.len(),
        "floating_endpoints": floating
    })))
}

async fn handle_validate_component_connections(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let filter_refs: Vec<String> = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let tol = 0.01_f64;

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // No-connect positions (pins with intentional no-connect markers are exempt)
    let no_connect_pts: Vec<(f64, f64)> = tree
        .find_all("no_connect")
        .iter()
        .filter_map(|n| {
            let at = n.find("at")?;
            Some((at.get_f64(1)?, at.get_f64(2)?))
        })
        .collect();

    // Build net graph so we can check connectivity. Junctions matter: a pin
    // sitting mid-wire is connected only through a junction dot, so without
    // them this validator reports false "not connected" (#104).
    let junction_pts = konnect_sexp::schematic::extract_junctions(&tree);
    let mut g = build_net_graph(&wires, &labels, &junction_pts);
    // Also build flat wire-endpoint list for direct presence checks
    let all_wire_eps: Vec<(f64, f64)> = wires
        .iter()
        .flat_map(|w| [(w.x1, w.y1), (w.x2, w.y2)])
        .collect();

    // `g.net_at` requires &mut self, so we need a `mut` closure.
    let mut has_connection = |px: f64, py: f64| -> bool {
        // Connected to a wire endpoint
        if all_wire_eps
            .iter()
            .any(|(wx, wy)| points_coincident(px, py, *wx, *wy, tol))
        {
            return true;
        }
        // A pin landing mid-wire connects only through a junction dot — KiCad's
        // netlister registers the unsplit wire at a junction point, so a dot
        // alone is enough and no wire split is required (#104).
        if junction_pts
            .iter()
            .any(|(jx, jy)| points_coincident(px, py, *jx, *jy, tol))
            && wires
                .iter()
                .any(|w| point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, tol))
        {
            return true;
        }
        // Or has a named net (label at or reachable from pin via wires)
        g.net_at(px, py).is_some()
    };

    let mut unconnected: Vec<serde_json::Value> = Vec::new();

    for inst in &instances {
        if !filter_refs.is_empty() && !filter_refs.contains(&inst.reference) {
            continue;
        }
        let lib_sym = find_lib_symbol(&lib_syms, inst);
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            for pin in extract_lib_pins(sym) {
                let (px, py) = pin_endpoint(&pin, t);

                // Skip intentional no-connects
                if no_connect_pts
                    .iter()
                    .any(|(nx, ny)| points_coincident(px, py, *nx, *ny, tol))
                {
                    continue;
                }

                if !has_connection(px, py) {
                    unconnected.push(json!({
                        "reference": inst.reference,
                        "value": inst.value,
                        "pin": pin.number,
                        "pin_name": pin.name,
                        "x": px,
                        "y": py
                    }));
                }
            }
        }
    }

    Ok(CallToolResult::json(&json!({
        "valid": unconnected.is_empty(),
        "unconnected_count": unconnected.len(),
        "unconnected_pins": unconnected
    })))
}

#[cfg(test)]
mod batch_delete_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    #[tokio::test]
    async fn batch_delete_uuid_is_tab_indentation_safe_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-delete.kicad_sch");
        let uuid = "11111111-1111-1111-1111-111111111111";
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(wire\n\t\t(pts (xy 0 0) (xy 10 0))\n\t\t(uuid \"{uuid}\")\n\t)\n\t(text \"keep me\" (at 5 5 0) (uuid \"text\"))\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();

        let result = handle_batch_delete(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [uuid, "root", uuid]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(uuid));
        assert!(after.contains("(uuid \"root\")"));
        assert!(after.contains("keep me"));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn batch_delete_uuid_removes_top_level_text_but_preserves_structure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-delete-text.kicad_sch");
        let text_uuid = "22222222-2222-2222-2222-222222222222";
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20260306)\n  (generator \"eeschema\")\n  (uuid \"root\")\n  (text \"obsolete caption\"\n    (at 5 5 0)\n    (effects (font (size 1.27 1.27)))\n    (uuid \"{text_uuid}\")\n  )\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();

        let result = handle_batch_delete(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [text_uuid]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("obsolete caption"));
        assert!(after.contains("(uuid \"root\")"));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }
}

#[cfg(test)]
mod batch_place_and_connect_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    // Pre-seed lib_symbols so ensure_lib_symbol short-circuits without KiCad
    // (precedent: sch_components.rs add_schematic_component_hides_power_reference).
    const DEVICE_R: &str = "    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 0 0 0))\n      (property \"Value\" \"R\" (at 0 0 0))\n    )\n";

    fn seeded_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("place.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n{DEVICE_R}  )\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn batch_place_components_dedupes_lib_symbols() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Device:R", "x": 100.0, "y": 100.0, "reference": "R1" },
                    { "lib_id": "Device:R", "x": 110.0, "y": 100.0, "reference": "R2" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(sch.symbols.by_reference("R1").is_some());
        assert!(sch.symbols.by_reference("R2").is_some());

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.matches("(symbol \"Device:R\"").count(),
            1,
            "lib_symbols entry must not be duplicated: {after}"
        );
        assert!(
            !after
                .lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "batch placement must not leave trailing whitespace: {after:?}"
        );
    }

    #[tokio::test]
    async fn batch_place_components_collects_per_item_errors() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Device:R", "x": 100.0, "y": 100.0, "reference": "R1" },
                    { "lib_id": "Nonexistent_xyzzy:Foo", "x": 110.0, "y": 100.0, "reference": "R2" },
                    { "lib_id": "Device:R", "x": 120.0, "y": 100.0, "reference": "R3" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["placed_count"], 2);
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 1);

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(sch.symbols.by_reference("R1").is_some());
        assert!(sch.symbols.by_reference("R3").is_some());
        assert!(sch.symbols.by_reference("R2").is_none());
    }

    #[tokio::test]
    async fn batch_place_components_total_failure_sets_is_error() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Nonexistent_xyzzy:Foo", "x": 100.0, "y": 100.0, "reference": "R1" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{result:?}");
    }

    /// Six single-pin instances of a synthetic part, positioned so that
    /// connecting them by pin pairs produces a T-junction on the second pair.
    fn multi_point_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let pin_def = "\t\t\t(pin passive line (at 0 0 0) (length 0)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n";
        let lib_sym = format!("\t\t(symbol \"Test:PT\"\n{pin_def}\t\t)\n");
        let inst = |reference: &str, x: f64, y: f64, uuid: &str| {
            format!(
                "\t(symbol\n\t\t(lib_id \"Test:PT\")\n\t\t(at {x} {y} 0)\n\t\t(uuid \"{uuid}\")\n\t\t(property \"Reference\" \"{reference}\"\n\t\t\t(at {x} {y} 0)\n\t\t)\n\t)\n"
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("points.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"3af69a4c-1faa-40bd-91dc-c4fc245c4cbd\")\n\t(lib_symbols\n{}\t)\n{}{}{}{}{}{})\n",
                lib_sym,
                inst("R1", 100.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000001"),
                inst("R2", 120.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000002"),
                inst("R3", 110.0, 80.0, "aaaaaaaa-0000-0000-0000-000000000003"),
                inst("R4", 110.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000004"),
                inst("R5", 200.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000005"),
                inst("R6", 220.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000006"),
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn batch_connect_pins_dedupes_junction_and_collects_errors() {
        // R3-R4's wire T-lands on R1-R2's wire at (110, 100) -- without the
        // STEP 1 fix, processing the third connection re-detects that same
        // T-junction from the raw wire list and inserts a second dot.
        let (_d, path) = multi_point_schematic();
        let result = handle_batch_connect_pins(
            &json!({
                "schematic": path.display().to_string(),
                "connections": [
                    { "ref1": "R1", "pin1": "1", "ref2": "R2", "pin2": "1" },
                    { "ref1": "R3", "pin1": "1", "ref2": "R4", "pin2": "1" },
                    { "ref1": "R5", "pin1": "1", "ref2": "R6", "pin2": "1" },
                    { "ref1": "Rbad", "pin1": "1", "ref2": "R6", "pin2": "1" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.matches("(junction").count(),
            1,
            "the T-junction at (110, 100) must not be re-inserted: {after}"
        );

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["connected_count"], 3);
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod midwire_pin_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// U1 has a single pin at (100,80), sitting strictly mid-segment on a wire
    /// from (90,80) to (110,80).
    fn midwire_schematic(with_junction: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let junction = if with_junction {
            "\t(junction (at 100 80) (diameter 0) (color 0 0 0 0) (uuid \"j1\"))\n"
        } else {
            ""
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("midwire.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Test:P1\"\n\t\t\t(symbol \"P1_1_1\"\n\t\t\t\t(pin passive line (at 0 0 0) (length 2.54)\n\t\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n\t(wire\n\t\t(pts (xy 90 80) (xy 110 80))\n\t\t(uuid \"w1\")\n\t)\n{junction}\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 100 75 0)\n\t\t)\n\t)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    /// KiCad connects a pin mid-wire only through a junction dot; the
    /// validator must mirror that instead of demanding a wire endpoint.
    #[tokio::test]
    async fn midwire_pin_connects_with_junction_only() {
        for (with_junction, expect_valid) in [(true, true), (false, false)] {
            let (_d, path) = midwire_schematic(with_junction);
            let result = handle_validate_component_connections(
                &json!({ "schematic": path.display().to_string() }),
                &test_ctx(),
            )
            .await
            .unwrap();
            let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
                panic!("expected text content");
            };
            let body: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(
                body["valid"].as_bool(),
                Some(expect_valid),
                "with_junction={with_junction}: {body}"
            );
        }
    }
}

#[cfg(test)]
mod connect_to_net_orientation_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// One pin per edge, plus two pins stacked on one endpoint. Placed at
    /// (100, 100): west tip (89.84, 100), east (110.16, 100), north
    /// (100, 89.84), south (100, 110.16), stack (89.84, 94.92).
    fn quad_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let pin = |x: f64, y: f64, angle: f64, name: &str, number: &str| {
            format!(
                "        (pin passive line (at {x} {y} {angle}) (length 2.54)\n\
                 \x20         (name \"{name}\") (number \"{number}\"))\n"
            )
        };
        let body = format!(
            "{}{}{}{}{}{}",
            pin(-10.16, 0.0, 0.0, "WEST", "1"),
            pin(10.16, 0.0, 180.0, "EAST", "2"),
            pin(0.0, 10.16, 270.0, "NORTH", "3"),
            pin(0.0, -10.16, 90.0, "SOUTH", "4"),
            pin(-10.16, 5.08, 0.0, "GND", "5"),
            pin(-10.16, 5.08, 0.0, "GND", "6"),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quad.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  \
                 (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  \
                 (lib_symbols\n    (symbol \"Test:QUAD\"\n      (symbol \"QUAD_1_1\"\n\
                 {body}      )\n    )\n  )\n  (symbol\n    (lib_id \"Test:QUAD\")\n    \
                 (at 100 100 0)\n    (unit 1)\n    \
                 (property \"Reference\" \"U1\" (at 100 90 0))\n    \
                 (property \"Value\" \"QUAD\" (at 100 110 0))\n  )\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    /// The `(at x y ROT)` and justify of the label for `net`.
    fn label_of(body: &str, net: &str) -> (String, String) {
        let start = body
            .find(&format!("(label \"{net}\""))
            .expect("label present");
        let block = &body[start..];
        let end = block.find("(uuid").unwrap_or(block.len());
        let block = &block[..end];
        let at = {
            let i = block.find("(at ").expect("at present") + 4;
            block[i..][..block[i..].find(')').unwrap()]
                .trim()
                .to_string()
        };
        let justify = match block.find("(justify ") {
            Some(j) => {
                let rest = &block[j + "(justify ".len()..];
                rest[..rest.find(')').unwrap()].trim().to_string()
            }
            None => "<none>".to_string(),
        };
        (at, justify)
    }

    async fn connect(path: &std::path::Path, net: &str, pin_number: &str) -> String {
        let result = handle_batch_connect_to_net(
            &json!({
                "schematic": path.display().to_string(),
                "net_name": net,
                "pins": [{ "reference": "U1", "pin_number": pin_number }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        std::fs::read_to_string(path).unwrap()
    }

    /// The reported bug: a left-edge pin's label was written at rotation 0,
    /// so its text ran east across the body, over the pin names.
    #[tokio::test]
    async fn a_left_edge_pin_gets_a_label_reading_away_from_the_body() {
        let (_d, path) = quad_schematic();
        let after = connect(&path, "SWDIO", "1").await;
        assert_eq!(
            label_of(&after, "SWDIO"),
            ("89.84 100 180".into(), "right bottom".into())
        );
        assert!(konnect_sexp::parse_sexp(&after).is_ok(), "{after}");
        assert!(
            !after
                .lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "label insertion must not leave the symbol line's indent behind: {after:?}"
        );
    }

    #[tokio::test]
    async fn a_right_edge_pin_keeps_reading_east() {
        let (_d, path) = quad_schematic();
        let after = connect(&path, "XTAL", "2").await;
        assert_eq!(
            label_of(&after, "XTAL"),
            ("110.16 100 0".into(), "left bottom".into())
        );
    }

    /// eeschema never turns a pin-anchored label sideways, whichever way a
    /// vertical pin faces — see `pin_label_rotation`.
    #[tokio::test]
    async fn vertical_pins_keep_their_label_horizontal() {
        let (_d, path) = quad_schematic();
        let after = connect(&path, "TOP", "3").await;
        assert_eq!(label_of(&after, "TOP").0, "100 89.84 0");
        let after = connect(&path, "BOTTOM", "4").await;
        assert_eq!(label_of(&after, "BOTTOM").0, "100 110.16 0");
    }

    /// Pins on one endpoint are already connected, so one label serves them
    /// all; superimposed copies render as a smear.
    #[tokio::test]
    async fn stacked_pins_share_a_single_label() {
        let (_d, path) = quad_schematic();
        let result = handle_batch_connect_to_net(
            &json!({
                "schematic": path.display().to_string(),
                "net_name": "GND",
                "pins": [
                    { "reference": "U1", "pin_number": "5" },
                    { "reference": "U1", "pin_number": "6" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        // Both pins are reported connected — the second is not an error.
        assert_eq!(parsed["added_count"], 2);
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 0);
        assert_eq!(parsed["added"][1]["deduplicated"], json!(true));

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.matches("(label \"GND\"").count(), 1, "{after}");
    }

    /// Re-running must not stack a second label on the first.
    #[tokio::test]
    async fn re_connecting_the_same_pin_adds_no_second_label() {
        let (_d, path) = quad_schematic();
        connect(&path, "SWDIO", "1").await;
        let after = connect(&path, "SWDIO", "1").await;
        assert_eq!(after.matches("(label \"SWDIO\"").count(), 1, "{after}");
    }
}

#[cfg(test)]
mod multi_unit_pin_tests {
    use crate::tools::sch_batch::tools;
    use konnect_sexp::schematic::{
        extract_lib_pins_for_unit, extract_symbol_instances, pin_endpoint, read_schematic,
    };
    use std::io::Write;

    /// Two units of one symbol, placed 15.24mm apart. Unit 1 owns pin 1, unit 2
    /// owns pin 3; both sit at local x = -7.62 in their own unit's drawing.
    const SCH: &str = r#"(kicad_sch
	(version 20241209)
	(lib_symbols
		(symbol "74xx:74HC14"
			(symbol "74HC14_1_1"
				(pin input line (at -7.62 0 0) (length 2.54)
					(name "A" (effects (font (size 1.27 1.27))))
					(number "1" (effects (font (size 1.27 1.27))))
				)
			)
			(symbol "74HC14_2_1"
				(pin input line (at -7.62 0 0) (length 2.54)
					(name "A" (effects (font (size 1.27 1.27))))
					(number "3" (effects (font (size 1.27 1.27))))
				)
			)
		)
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 100 0)
		(unit 1)
		(property "Reference" "U1" (at 100 100 0))
		(property "Value" "74HC14" (at 100 100 0))
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 115.24 0)
		(unit 2)
		(property "Reference" "U1" (at 100 115.24 0))
		(property "Value" "74HC14" (at 100 115.24 0))
	)
)
"#;

    /// The regression: resolving a pin used the FIRST instance with a matching
    /// reference, so every pin of a multi-unit part was transformed by unit 1's
    /// placement. Two nets then landed on one coordinate and were silently
    /// shorted — no error, no warning.
    #[test]
    fn each_unit_resolves_its_own_pin_position() {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let (_c, tree) = read_schematic(f.path()).unwrap();
        let instances = extract_symbol_instances(&tree);
        let lib_syms = tree
            .find("lib_symbols")
            .map(|n| n.find_all("symbol"))
            .unwrap_or_default();

        let resolve = |number: &str| -> Option<(f64, f64)> {
            instances
                .iter()
                .filter(|i| i.reference == "U1")
                .find_map(|inst| {
                    let sym = lib_syms
                        .iter()
                        .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id))?;
                    extract_lib_pins_for_unit(sym, inst.unit)
                        .into_iter()
                        .find(|p| p.number == number)
                        .map(|p| pin_endpoint(&p, inst.pin_transform()))
                })
        };

        let p1 = resolve("1").expect("unit 1 pin 1");
        let p3 = resolve("3").expect("unit 2 pin 3");

        assert!(
            (p1.1 - p3.1).abs() > 1.0,
            "unit 1 and unit 2 pins must not land on the same point \
             (got {p1:?} and {p3:?}) — that is the short this guards against"
        );
        assert!(
            (p1.1 - 100.0).abs() < 0.01,
            "unit 1 pin should sit at y=100, got {p1:?}"
        );
        assert!(
            (p3.1 - 115.24).abs() < 0.01,
            "unit 2 pin should sit at y=115.24, got {p3:?}"
        );
    }

    #[test]
    fn batch_connect_to_net_is_registered() {
        assert!(tools().iter().any(|t| t.name == "batch_connect_to_net"));
    }
}

#[cfg(test)]
mod multi_unit_field_tests {
    use super::{field_value_ranges, find_symbol_blocks};
    use konnect_sexp::writer::{apply_edits, SexpEdit};

    /// A 3-unit part plus an unrelated single-unit part. Every unit repeats the
    /// reference and carries its own copy of the shared fields, which is how
    /// eeschema writes them.
    const SCH: &str = r#"(kicad_sch
	(version 20241209)
	(lib_symbols
		(symbol "74xx:74HC14"
			(property "Reference" "U")
			(property "Footprint" "")
		)
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 100 0)
		(unit 1)
		(property "Reference" "U6" (at 100 100 0))
		(property "Value" "74HC14" (at 100 100 0))
		(property "Footprint" "" (at 100 100 0))
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 115.24 0)
		(unit 2)
		(property "Reference" "U6" (at 100 115.24 0))
		(property "Value" "74HC14" (at 100 115.24 0))
		(property "Footprint" "" (at 100 115.24 0))
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 130.48 0)
		(unit 7)
		(property "Reference" "U6" (at 100 130.48 0))
		(property "Value" "74HC14" (at 100 130.48 0))
		(property "Footprint" "" (at 100 130.48 0))
	)
	(symbol
		(lib_id "Device:R")
		(at 200 100 0)
		(unit 1)
		(property "Reference" "R1" (at 200 100 0))
		(property "Value" "10k" (at 200 100 0))
		(property "Footprint" "" (at 200 100 0))
	)
)
"#;

    /// The regression: field lookup stopped at the first instance, so assigning
    /// a footprint to a multi-unit part left units 2..n blank. KiCad then had
    /// one part claiming two different footprints.
    #[test]
    fn field_edit_reaches_every_unit() {
        let ranges = field_value_ranges(SCH, "U6", "Footprint");
        assert_eq!(
            ranges.len(),
            3,
            "expected one Footprint per unit: {ranges:?}"
        );

        let edits = ranges
            .iter()
            .map(|&(s, e)| SexpEdit::replace(s, e, "Package_SO:SOIC-14".to_string()))
            .collect();
        let out = apply_edits(SCH.to_string(), edits);
        assert_eq!(
            out.matches(r#"(property "Footprint" "Package_SO:SOIC-14""#)
                .count(),
            3
        );
        // The neighbouring single-unit part must be untouched.
        assert!(out.contains(r#"(property "Reference" "R1" (at 200 100 0))"#));
        assert_eq!(
            out.matches(r#"(property "Footprint" "" (at 200"#).count(),
            1
        );
    }

    #[test]
    fn single_unit_part_still_edits_once() {
        let ranges = field_value_ranges(SCH, "R1", "Value");
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn missing_field_yields_no_ranges() {
        assert!(field_value_ranges(SCH, "U6", "Datasheet").is_empty());
        assert!(field_value_ranges(SCH, "U99", "Value").is_empty());
    }

    /// Deleting one unit's block used to leave the other six behind as orphans
    /// referencing a component the caller believes is gone.
    #[test]
    fn delete_removes_every_unit() {
        let blocks = find_symbol_blocks(SCH, "U6");
        assert_eq!(blocks.len(), 3, "expected one block per unit: {blocks:?}");

        let edits = blocks
            .iter()
            .map(|&(s, e)| SexpEdit::delete(s, e))
            .collect();
        let out = apply_edits(SCH.to_string(), edits);
        assert!(
            !out.contains(r#""Reference" "U6""#),
            "no U6 unit should survive:\n{out}"
        );
        assert!(out.contains(r#""Reference" "R1""#), "R1 must survive");
        // The lib_symbols definition is not an instance and must stay.
        assert!(out.contains(r#"(symbol "74xx:74HC14""#));
    }

    /// The blocks must not overlap, or apply_edits would splice the file wrong.
    #[test]
    fn unit_blocks_are_disjoint_and_ordered() {
        let blocks = find_symbol_blocks(SCH, "U6");
        for w in blocks.windows(2) {
            assert!(w[0].1 <= w[1].0, "blocks overlap: {:?} {:?}", w[0], w[1]);
        }
    }
}

#[cfg(test)]
mod field_visibility_tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use serde_json::{json, Value};
    use std::path::Path;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn result_body(result: &CallToolResult) -> Value {
        let Some(crate::mcp::protocol::ToolContent::Text { text }) = result.content.first() else {
            panic!("expected text content: {result:?}");
        };
        serde_json::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
    }

    fn write_fixture(name: &str, content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    const SCH_VISIBLE_REF_HIDDEN_VALUE: &str = "(kicad_sch\n  (version 20260306)\n  (generator \"konnect\")\n  (uuid \"root\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\")\n      (property \"Value\" \"R\")\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 10 10 0)\n    (unit 1)\n    (uuid \"sym-r1\")\n    (property \"Reference\" \"R1\"\n      (at 10 8 0)\n      (effects (font (size 1.27 1.27)))\n    )\n    (property \"Value\" \"10k\"\n      (at 10 12 0)\n      (hide yes)\n      (effects (font (size 1.27 1.27)))\n    )\n  )\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n";

    const SCH_NO_EFFECTS_VALUE: &str = "(kicad_sch\n  (version 20260306)\n  (generator \"konnect\")\n  (uuid \"root\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\")\n      (property \"Value\" \"R\")\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 10 10 0)\n    (unit 1)\n    (uuid \"sym-r1\")\n    (property \"Reference\" \"R1\"\n      (at 10 8 0)\n      (effects (font (size 1.27 1.27)))\n    )\n    (property \"Value\" \"10k\"\n      (at 10 12 0)\n    )\n  )\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n";

    const SCH_MULTI_UNIT_VISIBLE: &str = "(kicad_sch\n  (version 20260306)\n  (generator \"konnect\")\n  (uuid \"root\")\n  (lib_symbols\n    (symbol \"74xx:74HC14\"\n      (property \"Reference\" \"U\")\n      (property \"Value\" \"74HC14\")\n    )\n  )\n  (symbol\n    (lib_id \"74xx:74HC14\")\n    (at 10 10 0)\n    (unit 1)\n    (uuid \"sym-u1-1\")\n    (property \"Reference\" \"U1\"\n      (at 10 8 0)\n      (effects (font (size 1.27 1.27)))\n    )\n    (property \"Value\" \"74HC14\"\n      (at 10 12 0)\n      (effects (font (size 1.27 1.27)))\n    )\n  )\n  (symbol\n    (lib_id \"74xx:74HC14\")\n    (at 10 25 0)\n    (unit 2)\n    (uuid \"sym-u1-2\")\n    (property \"Reference\" \"U1\"\n      (at 10 23 0)\n      (effects (font (size 1.27 1.27)))\n    )\n    (property \"Value\" \"74HC14\"\n      (at 10 27 0)\n      (effects (font (size 1.27 1.27)))\n    )\n  )\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n";

    const SCH_DUPLICATE_HIDE: &str = "(kicad_sch\n  (version 20260306)\n  (generator \"konnect\")\n  (uuid \"root\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\")\n      (property \"Value\" \"R\")\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 10 10 0)\n    (unit 1)\n    (uuid \"sym-r1\")\n    (property \"Reference\" \"R1\"\n      (at 10 8 0)\n      (hide yes)\n      (hide yes)\n      (effects (font (size 1.27 1.27)))\n    )\n    (property \"Value\" \"10k\"\n      (at 10 12 0)\n      (effects (font (size 1.27 1.27)))\n    )\n  )\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n";

    #[test]
    fn batch_set_schematic_field_visibility_is_registered_with_the_frozen_schema() {
        let tool = tools()
            .into_iter()
            .find(|tool| tool.name == "batch_set_schematic_field_visibility")
            .expect("tool is registered");
        let schema = tool.input_schema;
        assert_eq!(schema["required"], json!(["schematic", "edits"]));
        assert_eq!(schema["properties"]["schematic"]["type"], json!("string"));
        assert_eq!(schema["properties"]["edits"]["type"], json!("array"));
        assert_eq!(
            schema["properties"]["edits"]["items"]["properties"]["reference_visible"]["type"],
            json!("boolean")
        );
        assert_eq!(
            schema["properties"]["edits"]["items"]["properties"]["value_visible"]["type"],
            json!("boolean")
        );
        assert_eq!(
            schema["properties"]["edits"]["items"]["additionalProperties"],
            json!(false)
        );
    }

    #[tokio::test]
    async fn toggles_reference_and_value_visibility_atomically() {
        let (_dir, path) = write_fixture("visibility.kicad_sch", SCH_VISIBLE_REF_HIDDEN_VALUE);

        let result = handle_batch_set_schematic_field_visibility(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{
                    "reference": "R1",
                    "reference_visible": false,
                    "value_visible": true
                }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        let body = result_body(&result);
        assert_eq!(body["updated_count"], json!(1));
        assert_eq!(body["unchanged_count"], json!(0));
        assert_eq!(body["results"][0]["reference"], json!("R1"));
        assert_eq!(
            body["results"][0]["reference_visible"],
            json!({"old": true, "new": false})
        );
        assert_eq!(
            body["results"][0]["value_visible"],
            json!({"old": false, "new": true})
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains(
            "(property \"Reference\" \"R1\"\n      (at 10 8 0)\n      (hide yes)\n      (effects"
        ));
        assert!(!after.contains(
            "(property \"Value\" \"10k\"\n      (at 10 12 0)\n      (hide yes)\n      (effects"
        ));
        assert!(konnect_sexp::parse_sexp(&after).is_ok(), "{after}");
    }

    #[tokio::test]
    async fn can_update_only_one_requested_field_and_preserve_the_other() {
        let (_dir, path) = write_fixture(
            "visibility-one-field.kicad_sch",
            SCH_VISIBLE_REF_HIDDEN_VALUE,
        );

        let result = handle_batch_set_schematic_field_visibility(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{
                    "reference": "R1",
                    "reference_visible": false
                }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        let body = result_body(&result);
        assert!(body["results"][0].get("value_visible").is_none(), "{body}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains(
            "(property \"Reference\" \"R1\"\n      (at 10 8 0)\n      (hide yes)\n      (effects"
        ));
        assert!(after.contains(
            "(property \"Value\" \"10k\"\n      (at 10 12 0)\n      (hide yes)\n      (effects"
        ));
    }

    #[tokio::test]
    async fn inserts_hide_before_property_close_when_effects_are_absent() {
        let (_dir, path) = write_fixture("visibility-no-effects.kicad_sch", SCH_NO_EFFECTS_VALUE);

        let result = handle_batch_set_schematic_field_visibility(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{
                    "reference": "R1",
                    "value_visible": false
                }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after
            .contains("(property \"Value\" \"10k\"\n      (at 10 12 0)\n      (hide yes)\n    )"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok(), "{after}");
    }

    #[tokio::test]
    async fn repeated_request_is_a_successful_byte_identical_no_op() {
        let (_dir, path) = write_fixture("visibility-noop.kicad_sch", SCH_VISIBLE_REF_HIDDEN_VALUE);
        let before = std::fs::read_to_string(&path).unwrap();

        let result = handle_batch_set_schematic_field_visibility(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{
                    "reference": "R1",
                    "reference_visible": true,
                    "value_visible": false
                }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        let body = result_body(&result);
        assert_eq!(body["updated_count"], json!(0));
        assert_eq!(body["unchanged_count"], json!(1));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn empty_edits_is_a_successful_byte_identical_no_op() {
        let (_dir, path) = write_fixture(
            "visibility-empty-edits.kicad_sch",
            SCH_VISIBLE_REF_HIDDEN_VALUE,
        );
        let before = std::fs::read_to_string(&path).unwrap();

        let result = handle_batch_set_schematic_field_visibility(
            &json!({
                "schematic": path.display().to_string(),
                "edits": []
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        let body = result_body(&result);
        assert_eq!(
            body,
            json!({
                "updated_count": 0,
                "unchanged_count": 0,
                "results": []
            })
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn multi_unit_reference_updates_every_unit_copy() {
        let (_dir, path) = write_fixture("visibility-multi-unit.kicad_sch", SCH_MULTI_UNIT_VISIBLE);

        let result = handle_batch_set_schematic_field_visibility(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{
                    "reference": "U1",
                    "reference_visible": false,
                    "value_visible": false
                }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.matches("(property \"Reference\" \"U1\"").count(), 2);
        assert_eq!(
            after
                .matches("(property \"Reference\" \"U1\"\n      (at ")
                .count(),
            2
        );
        assert_eq!(
            after
                .matches("(property \"Reference\" \"U1\"\n      (at ")
                .count(),
            2
        );
        assert_eq!(
            after.matches("(hide yes)\n      (effects").count(),
            4,
            "{after}"
        );
    }

    #[tokio::test]
    async fn hide_show_edit_changes_only_direct_hide_nodes_and_local_whitespace() {
        let (_dir, path) = write_fixture(
            "visibility-byte-preservation.kicad_sch",
            SCH_VISIBLE_REF_HIDDEN_VALUE,
        );

        let result = handle_batch_set_schematic_field_visibility(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{
                    "reference": "R1",
                    "reference_visible": false,
                    "value_visible": true
                }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        let after = std::fs::read_to_string(&path).unwrap();
        let expected = "(kicad_sch\n  (version 20260306)\n  (generator \"konnect\")\n  (uuid \"root\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\")\n      (property \"Value\" \"R\")\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 10 10 0)\n    (unit 1)\n    (uuid \"sym-r1\")\n    (property \"Reference\" \"R1\"\n      (at 10 8 0)\n      (hide yes)\n      (effects (font (size 1.27 1.27)))\n    )\n    (property \"Value\" \"10k\"\n      (at 10 12 0)\n      (effects (font (size 1.27 1.27)))\n    )\n  )\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n";
        assert_eq!(after, expected);
    }

    #[tokio::test]
    async fn invalid_batches_are_rejected_without_partial_writes() {
        let (_dir, path) = write_fixture(
            "visibility-invalid-batch.kicad_sch",
            SCH_VISIBLE_REF_HIDDEN_VALUE,
        );
        let before = std::fs::read_to_string(&path).unwrap();

        let result = handle_batch_set_schematic_field_visibility(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [
                    {
                        "reference": "R1",
                        "reference_visible": false
                    },
                    {
                        "value_visible": true
                    }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error, "{result:?}");
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn duplicate_request_references_and_unknown_keys_are_rejected() {
        let (_dir, path) = write_fixture(
            "visibility-invalid-keys.kicad_sch",
            SCH_VISIBLE_REF_HIDDEN_VALUE,
        );
        let before = std::fs::read_to_string(&path).unwrap();

        for args in [
            json!({
                "schematic": path.display().to_string(),
                "edits": [
                    {"reference": "R1", "reference_visible": false},
                    {"reference": "R1", "value_visible": true}
                ]
            }),
            json!({
                "schematic": path.display().to_string(),
                "edits": [
                    {"reference": "R1", "reference_visible": false, "unexpected": 1}
                ]
            }),
        ] {
            let result = handle_batch_set_schematic_field_visibility(&args, &test_ctx())
                .await
                .unwrap();
            assert!(result.is_error, "{args}: {result:?}");
            assert_eq!(
                extract_error_kind(&result).as_deref(),
                Some("invalid_argument")
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn missing_symbol_missing_property_and_duplicate_hide_leave_the_file_unchanged() {
        for (name, content, args) in [
            (
                "visibility-missing-symbol.kicad_sch",
                SCH_VISIBLE_REF_HIDDEN_VALUE,
                json!({
                    "edits": [{"reference": "R99", "reference_visible": false}]
                }),
            ),
            (
                "visibility-missing-property.kicad_sch",
                "(kicad_sch\n  (version 20260306)\n  (generator \"konnect\")\n  (uuid \"root\")\n  (lib_symbols\n    (symbol \"Device:R\")\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 10 10 0)\n    (unit 1)\n    (uuid \"sym-r1\")\n    (property \"Reference\" \"R1\"\n      (at 10 8 0)\n      (effects (font (size 1.27 1.27)))\n    )\n  )\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n",
                json!({
                    "edits": [{"reference": "R1", "value_visible": false}]
                }),
            ),
            (
                "visibility-duplicate-hide.kicad_sch",
                SCH_DUPLICATE_HIDE,
                json!({
                    "edits": [{"reference": "R1", "reference_visible": true}]
                }),
            ),
        ] {
            let (_dir, path) = write_fixture(name, content);
            let before = std::fs::read_to_string(&path).unwrap();
            let mut full_args = args;
            full_args["schematic"] = json!(path.display().to_string());

            let result = handle_batch_set_schematic_field_visibility(&full_args, &test_ctx())
                .await
                .unwrap();

            assert!(result.is_error, "{full_args}: {result:?}");
            assert_eq!(extract_error_kind(&result).as_deref(), Some("invalid_argument"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn stale_conflicts_are_reported_structurally_without_overwriting_the_newer_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stale-visibility.kicad_sch");
        std::fs::write(&path, SCH_VISIBLE_REF_HIDDEN_VALUE).unwrap();

        let prepared = prepare_field_visibility_update(
            SCH_VISIBLE_REF_HIDDEN_VALUE,
            &[FieldVisibilityRequest {
                reference: "R1".to_string(),
                reference_visible: Some(false),
                value_visible: Some(true),
            }],
        )
        .expect("request prevalidates");

        let externally_changed = SCH_VISIBLE_REF_HIDDEN_VALUE.replace("\"10k\"", "\"22k\"");
        std::fs::write(&path, &externally_changed).unwrap();

        let result = persist_field_visibility_update(
            Path::new(&path),
            SCH_VISIBLE_REF_HIDDEN_VALUE,
            &prepared.content,
        );

        assert!(result.is_error, "{result:?}");
        assert_eq!(extract_error_kind(&result).as_deref(), Some("conflict"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), externally_changed);
    }
}

#[cfg(test)]
mod add_text_placement_tests {
    use super::tools;
    use crate::tools::ToolContext;
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    const SCH: &str = "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\")\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 100 75 0)\n\t\t)\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n";

    async fn add_text(text: &str) -> String {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let def = tools()
            .into_iter()
            .find(|t| t.name == "add_schematic_text")
            .unwrap();
        let cfg = crate::tools::ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        };
        let router = Arc::new(crate::router::ToolRouter::new());
        let ctx = Arc::new(ToolContext::new(cfg, router));
        let args = json!({
            "schematic": f.path().to_str().unwrap(),
            "text": text, "x": 30.0, "y": 114.3
        });
        (def.handler)(&args, ctx).await.unwrap();
        std::fs::read_to_string(f.path()).unwrap()
    }

    /// The regression: the text was spliced in at the file's last `)`, which
    /// puts it *after* the symbol instances and `sheet_instances`. KiCad 10
    /// requires instances last and rejects the whole file — "Failed to load
    /// schematic", with no hint as to which element is misplaced.
    #[tokio::test]
    async fn text_goes_before_the_symbol_instances() {
        let out = add_text("hello").await;
        let text_at = out.find("(text \"hello\"").expect("text written");
        let sym_at = out.find("(symbol\n\t\t(lib_id").expect("instance present");
        let sheets_at = out
            .find("(sheet_instances")
            .expect("sheet_instances present");
        assert!(
            text_at < sym_at && text_at < sheets_at,
            "text must precede symbol instances (text {text_at}, symbol {sym_at})"
        );
        // and it must land after lib_symbols, not inside it
        assert!(text_at > out.find("(lib_symbols").unwrap());
    }

    /// The other half of the same incident: the content was written with the
    /// newline as a literal byte inside the quoted string. KiCad wants the
    /// two-character escape and refuses the file otherwise.
    #[tokio::test]
    async fn multiline_text_escapes_its_newlines() {
        let out = add_text("line one\nline two").await;
        let text_at = out
            .find(r#"(text "line one\nline two""#)
            .expect("newline must be written as an escape, not a raw byte");
        assert!(text_at < out.find("(symbol\n\t\t(lib_id").unwrap());
    }

    #[tokio::test]
    async fn quotes_backslashes_and_tabs_are_escaped() {
        let out = add_text("a \"b\" c\\d\te").await;
        assert!(out.contains(r#"(text "a \"b\" c\\d\te""#), "got:\n{out}");
    }
}

/// `add_schematic_text` was not the only handler splicing at the file's last
/// `)`. `batch_connect_to_net` and `connect_to_net` did the same, and a label
/// or wire written after the symbol instances breaks the file exactly as #156
/// described — KiCad reports only "Failed to load schematic", and because the
/// file no longer loads, `kicad-cli erc` leaves a stale report in place.
#[cfg(test)]
mod insert_order_tests {
    use crate::tools::sch_wiring::insert_before_close;

    const SCH: &str = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Device:R\")\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(uuid \"u1\")\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n";

    #[test]
    fn labels_land_before_the_symbol_instances() {
        let out = insert_before_close(SCH, "\n  (label \"NET\" (at 10 10 0))");
        let label = out.find("(label \"NET\"").expect("label written");
        let inst = out.find("(symbol\n\t\t(lib_id").expect("instance present");
        assert!(
            label < inst,
            "a label after the instances makes the file unloadable:\n{out}"
        );
        assert!(
            !out.contains(")(symbol"),
            "elements must not be glued: {out}"
        );
        assert!(
            !out.lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "insertion must consume the target line's indent: {out:?}"
        );
    }

    /// The old splice point, for contrast: the file's final `)` sits after
    /// everything, so anything inserted there lands last.
    #[test]
    fn the_old_final_paren_splice_would_land_after_the_instances() {
        let close = SCH.rfind(')').unwrap();
        let inst = SCH.find("(symbol\n\t\t(lib_id").unwrap();
        assert!(
            close > inst,
            "this test is meaningless if the last paren precedes the instances"
        );
    }
}

/// #202: `bulk_move` shifted only the symbol's own `(at …)`. Property `(at …)`
/// coordinates are absolute in `.kicad_sch`, so Reference and Value text
/// stayed at the old location while the symbol moved away. The typed path
/// (`move_schematic_component` → `Symbol::translate`) always translated the
/// properties too — this was the second, text-based implementation that never
/// got the fix.
#[cfg(test)]
mod bulk_move_field_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// One symbol with Reference and Value at eeschema-style offsets beside
    /// it. Reference carries a rotation, which must survive the move.
    const SCH: &str = "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 101.6 101.6 0)\n\t\t(unit 1)\n\t\t(uuid \"sym-1\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 105.232 100.33 90)\n\t\t\t(effects (font (size 1.27 1.27)))\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 105.232 102.87 0)\n\t\t)\n\t\t(instances\n\t\t\t(project \"p\"\n\t\t\t\t(path \"/root\" (reference \"R1\") (unit 1))\n\t\t\t)\n\t\t)\n\t)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n";

    /// The placed symbol's `(at …)` and each property's, read back from the
    /// written file. Numeric, so a float-formatting change can't break the
    /// test and a wrong coordinate can't hide behind one.
    fn positions(sch: &str) -> (Vec<f64>, Vec<(String, Vec<f64>)>) {
        let tree = konnect_sexp::parse_sexp(sch).expect("parses");
        let symbol = tree
            .children()
            .unwrap()
            .iter()
            .find(|n| n.head() == Some("symbol") && n.find("lib_id").is_some())
            .expect("placed symbol");
        let at_of = |n: &konnect_sexp::SexpNode| -> Vec<f64> {
            let at = n.find("at").expect("(at …)");
            (1..at.children().unwrap().len())
                .filter_map(|i| at.get_f64(i))
                .collect()
        };
        let props = symbol
            .find_all("property")
            .into_iter()
            .map(|p| {
                (
                    p.get(1).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    at_of(p),
                )
            })
            .collect();
        (at_of(symbol), props)
    }

    async fn bulk_move(dx: f64, dy: f64) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("move.kicad_sch");
        std::fs::write(&path, SCH).unwrap();
        let result = handle_bulk_move(
            &json!({ "schematic": path.to_str().unwrap(),
                     "references": ["R1"], "dx": dx, "dy": dy }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        std::fs::read_to_string(&path).unwrap()
    }

    /// Every property keeps its offset from the symbol — which is the same as
    /// saying it moved by whatever the symbol actually moved.
    async fn assert_fields_follow(dx: f64, dy: f64) {
        let (before_sym, before_props) = positions(SCH);
        let after_src = bulk_move(dx, dy).await;
        let (after_sym, after_props) = positions(&after_src);

        // The handler snaps to the 1.27 grid, so the effective delta is not
        // necessarily the requested one — the fields must follow the real one.
        let (mdx, mdy) = (after_sym[0] - before_sym[0], after_sym[1] - before_sym[1]);
        assert_eq!(before_props.len(), after_props.len());
        for ((name, before), (after_name, after)) in before_props.iter().zip(&after_props) {
            assert_eq!(name, after_name, "property order preserved");
            assert!(
                (after[0] - (before[0] + mdx)).abs() < 1e-6
                    && (after[1] - (before[1] + mdy)).abs() < 1e-6,
                "'{name}' must move with the symbol (delta {mdx}, {mdy}): \
                 {before:?} -> {after:?}\n{after_src}"
            );
            // A property's own rotation is independent of a translation.
            assert_eq!(
                before.get(2),
                after.get(2),
                "'{name}' rotation must not change"
            );
        }
        assert!(konnect_sexp::parse_sexp(&after_src).is_ok());
    }

    #[tokio::test]
    async fn field_text_moves_with_the_symbol() {
        // On-grid delta: symbol lands exactly where asked.
        assert_fields_follow(12.7, 2.54).await;
    }

    #[tokio::test]
    async fn fields_follow_the_snapped_delta_not_the_requested_one() {
        // Off-grid delta: the symbol snaps, so the fields must move by the
        // snapped amount or they drift relative to the part.
        assert_fields_follow(1.0, 0.0).await;
    }

    /// A negative move exercises the same path in the other direction.
    #[tokio::test]
    async fn field_text_follows_a_negative_move() {
        assert_fields_follow(-25.4, -12.7).await;
    }
}
