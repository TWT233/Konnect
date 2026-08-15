use konnect_schematic_editor::types::fmt_f64;
use konnect_sexp::writer::{
    apply_edits, find_balanced_block, find_block_with_leading_whitespace, find_direct_child_blocks,
    SexpEdit,
};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Fill {
    None,
    Solid,
}

impl Fill {
    fn as_kicad(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Solid => "solid",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
enum FootprintGraphic {
    Line {
        start: Point,
        end: Point,
        stroke_width_mm: f64,
    },
    Arc {
        start: Point,
        mid: Point,
        end: Point,
        stroke_width_mm: f64,
    },
    Rect {
        start: Point,
        end: Point,
        stroke_width_mm: f64,
        fill: Fill,
    },
    Circle {
        center: Point,
        radius_mm: f64,
        stroke_width_mm: f64,
        fill: Fill,
    },
    Poly {
        points: Vec<Point>,
        stroke_width_mm: f64,
        fill: Fill,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum GraphicsMode {
    Append,
    Replace,
    Delete,
}

#[derive(Debug, thiserror::Error)]
enum FootprintGraphicsError {
    #[error("invalid {field}: {reason}")]
    InvalidArgument { field: String, reason: String },
    #[error("{0}")]
    Conflict(String),
}

#[derive(Debug)]
struct PreparedMutation {
    replacement: String,
    matched_count: usize,
    added_count: usize,
}

fn parse_graphics(value: &serde_json::Value) -> Result<Vec<FootprintGraphic>, String> {
    let mut graphics: Vec<FootprintGraphic> =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    for graphic in &mut graphics {
        graphic.validate_and_normalize()?;
    }
    Ok(graphics)
}

impl Point {
    fn validate(self, field: &str) -> Result<(), String> {
        if self.x.is_finite() && self.y.is_finite() {
            Ok(())
        } else {
            Err(format!("{field} coordinates must be finite"))
        }
    }
}

fn validate_stroke(width_mm: f64, fill: Option<&Fill>) -> Result<(), String> {
    if !width_mm.is_finite() || width_mm < 0.0 {
        return Err("stroke_width_mm must be finite and non-negative".to_string());
    }
    if width_mm == 0.0 && !matches!(fill, Some(Fill::Solid)) {
        return Err("an unfilled graphic requires a positive stroke_width_mm".to_string());
    }
    Ok(())
}

fn points_differ(a: Point, b: Point) -> bool {
    a.x != b.x || a.y != b.y
}

impl FootprintGraphic {
    fn validate_and_normalize(&mut self) -> Result<(), String> {
        match self {
            Self::Line {
                start,
                end,
                stroke_width_mm,
            } => {
                start.validate("start")?;
                end.validate("end")?;
                validate_stroke(*stroke_width_mm, None)?;
                if !points_differ(*start, *end) {
                    return Err("line start and end must differ".to_string());
                }
            }
            Self::Arc {
                start,
                mid,
                end,
                stroke_width_mm,
            } => {
                start.validate("start")?;
                mid.validate("mid")?;
                end.validate("end")?;
                validate_stroke(*stroke_width_mm, None)?;
                if !points_differ(*start, *mid)
                    || !points_differ(*mid, *end)
                    || !points_differ(*start, *end)
                {
                    return Err("arc start, mid, and end must be distinct".to_string());
                }
                let twice_area =
                    (mid.x - start.x) * (end.y - start.y) - (mid.y - start.y) * (end.x - start.x);
                if twice_area.abs() <= f64::EPSILON {
                    return Err("arc start, mid, and end must not be collinear".to_string());
                }
            }
            Self::Rect {
                start,
                end,
                stroke_width_mm,
                fill,
            } => {
                start.validate("start")?;
                end.validate("end")?;
                validate_stroke(*stroke_width_mm, Some(fill))?;
                if start.x == end.x || start.y == end.y {
                    return Err("rectangle width and height must be positive".to_string());
                }
            }
            Self::Circle {
                center,
                radius_mm,
                stroke_width_mm,
                fill,
            } => {
                center.validate("center")?;
                validate_stroke(*stroke_width_mm, Some(fill))?;
                if !radius_mm.is_finite() || *radius_mm <= 0.0 {
                    return Err("radius_mm must be finite and positive".to_string());
                }
            }
            Self::Poly {
                points,
                stroke_width_mm,
                fill,
            } => {
                validate_stroke(*stroke_width_mm, Some(fill))?;
                for (index, point) in points.iter().copied().enumerate() {
                    point.validate(&format!("points[{index}]"))?;
                }
                if points.len() > 1 && points.first() == points.last() {
                    points.pop();
                }
                if points.len() < 3 {
                    return Err("polygon requires at least three points".to_string());
                }
                let mut distinct = Vec::new();
                for point in points.iter().copied() {
                    if !distinct.contains(&point) {
                        distinct.push(point);
                    }
                }
                if distinct.len() < 3 {
                    return Err("polygon requires at least three distinct points".to_string());
                }
            }
        }
        Ok(())
    }
}

fn point_clause(tag: &str, point: Point) -> String {
    format!("({tag} {} {})", fmt_f64(point.x), fmt_f64(point.y))
}

fn stroke_clause(width_mm: f64) -> String {
    format!("(stroke (width {}) (type solid))", fmt_f64(width_mm))
}

fn serialize_graphics(layer: &str, graphics: &[FootprintGraphic], indent: &str) -> String {
    let mut output = String::new();
    for graphic in graphics {
        output.push('\n');
        output.push_str(indent);
        match graphic {
            FootprintGraphic::Line {
                start,
                end,
                stroke_width_mm,
            } => output.push_str(&format!(
                "(fp_line {} {} {} (layer \"{}\"))",
                point_clause("start", *start),
                point_clause("end", *end),
                stroke_clause(*stroke_width_mm),
                layer
            )),
            FootprintGraphic::Arc {
                start,
                mid,
                end,
                stroke_width_mm,
            } => output.push_str(&format!(
                "(fp_arc {} {} {} {} (layer \"{}\"))",
                point_clause("start", *start),
                point_clause("mid", *mid),
                point_clause("end", *end),
                stroke_clause(*stroke_width_mm),
                layer
            )),
            FootprintGraphic::Rect {
                start,
                end,
                stroke_width_mm,
                fill,
            } => output.push_str(&format!(
                "(fp_rect {} {} {} (fill {}) (layer \"{}\"))",
                point_clause("start", *start),
                point_clause("end", *end),
                stroke_clause(*stroke_width_mm),
                fill.as_kicad(),
                layer
            )),
            FootprintGraphic::Circle {
                center,
                radius_mm,
                stroke_width_mm,
                fill,
            } => {
                let end = Point {
                    x: center.x + radius_mm,
                    y: center.y,
                };
                output.push_str(&format!(
                    "(fp_circle {} {} {} (fill {}) (layer \"{}\"))",
                    point_clause("center", *center),
                    point_clause("end", end),
                    stroke_clause(*stroke_width_mm),
                    fill.as_kicad(),
                    layer
                ));
            }
            FootprintGraphic::Poly {
                points,
                stroke_width_mm,
                fill,
            } => {
                let points = points
                    .iter()
                    .map(|point| point_clause("xy", *point))
                    .collect::<Vec<_>>()
                    .join(" ");
                output.push_str(&format!(
                    "(fp_poly (pts {points}) {} (fill {}) (layer \"{}\"))",
                    stroke_clause(*stroke_width_mm),
                    fill.as_kicad(),
                    layer
                ));
            }
        }
    }
    output
}

fn invalid(field: &str, reason: impl Into<String>) -> FootprintGraphicsError {
    FootprintGraphicsError::InvalidArgument {
        field: field.to_string(),
        reason: reason.into(),
    }
}

fn is_supported_graphic(tag: &str) -> bool {
    matches!(
        tag,
        "fp_line" | "fp_arc" | "fp_rect" | "fp_circle" | "fp_poly"
    )
}

fn child_indent(source: &str, start: usize) -> String {
    let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
    let indent = &source[line_start..start];
    if !indent.is_empty()
        && indent
            .chars()
            .all(|character| matches!(character, ' ' | '\t'))
    {
        indent.to_string()
    } else {
        "  ".to_string()
    }
}

fn prepare_mutation(
    source: &str,
    layer: &str,
    mode: GraphicsMode,
    graphics: &[FootprintGraphic],
) -> Result<PreparedMutation, FootprintGraphicsError> {
    if !konnect_sexp::layers::is_canonical_name(layer) {
        return Err(invalid("selector.layer", "not a canonical KiCad layer"));
    }
    let tree = konnect_sexp::parse_sexp(source)
        .map_err(|error| invalid("footprint_path", format!("invalid S-expression: {error}")))?;
    if tree.head() != Some("footprint") {
        return Err(invalid("footprint_path", "file root must be a footprint"));
    }

    let direct_children = find_direct_child_blocks(source, "footprint");
    let mut selected = Vec::new();
    let mut group_members = Vec::new();
    let mut insertion_anchor = None;
    for (start, end) in direct_children {
        let block = &source[start..end];
        let Ok(node) = konnect_sexp::parse_sexp(block) else {
            return Err(invalid(
                "footprint_path",
                "contains an invalid top-level item",
            ));
        };
        let Some(tag) = node.head() else {
            continue;
        };
        if insertion_anchor.is_none() && matches!(tag, "pad" | "group" | "model") {
            insertion_anchor =
                find_block_with_leading_whitespace(source, start).map(|range| range.0);
        }
        if tag == "group" {
            if let Some(members) = node.find("members").and_then(|members| members.children()) {
                group_members.extend(
                    members
                        .iter()
                        .skip(1)
                        .filter_map(|member| member.as_str())
                        .map(str::to_string),
                );
            }
        }
        if is_supported_graphic(tag) && node.find_str("layer") == Some(layer) {
            let range = find_block_with_leading_whitespace(source, start)
                .ok_or_else(|| invalid("footprint_path", "contains an unbalanced graphic"))?;
            let item_id = node
                .find_str("uuid")
                .or_else(|| node.find_str("tstamp"))
                .map(str::to_string);
            selected.push((range, item_id));
        }
    }

    if !matches!(mode, GraphicsMode::Append) {
        if let Some(item_id) = selected
            .iter()
            .filter_map(|(_, item_id)| item_id.as_deref())
            .find(|item_id| group_members.iter().any(|member| member == item_id))
        {
            return Err(FootprintGraphicsError::Conflict(format!(
                "selected graphic '{item_id}' is referenced by a footprint group"
            )));
        }
    }

    let indent = selected
        .first()
        .map(|(range, _)| {
            let block_start = find_balanced_block(source, range.0)
                .map(|range| range.0)
                .unwrap_or(range.0);
            child_indent(source, block_start)
        })
        .or_else(|| {
            insertion_anchor.map(|anchor| {
                let block_start = find_balanced_block(source, anchor)
                    .map(|range| range.0)
                    .unwrap_or(anchor);
                child_indent(source, block_start)
            })
        })
        .unwrap_or_else(|| "  ".to_string());
    let serialized = serialize_graphics(layer, graphics, &indent);
    let matched_count = selected.len();
    let added_count = if matches!(mode, GraphicsMode::Delete) {
        0
    } else {
        graphics.len()
    };

    let mut edits = Vec::new();
    match mode {
        GraphicsMode::Replace => {
            if let Some((first, rest)) = selected.split_first() {
                let (first_range, _) = first;
                edits.push(SexpEdit::replace(first_range.0, first_range.1, serialized));
                edits.extend(
                    rest.iter()
                        .map(|(range, _)| SexpEdit::delete(range.0, range.1)),
                );
            } else if !serialized.is_empty() {
                let root_end = find_balanced_block(source, 0)
                    .map(|range| range.1 - 1)
                    .ok_or_else(|| invalid("footprint_path", "unbalanced footprint root"))?;
                edits.push(SexpEdit::insert(
                    insertion_anchor.unwrap_or(root_end),
                    serialized,
                ));
            }
        }
        GraphicsMode::Append => {
            if !serialized.is_empty() {
                let root_end = find_balanced_block(source, 0)
                    .map(|range| range.1 - 1)
                    .ok_or_else(|| invalid("footprint_path", "unbalanced footprint root"))?;
                let offset = selected
                    .last()
                    .map(|(range, _)| range.1)
                    .or(insertion_anchor)
                    .unwrap_or(root_end);
                edits.push(SexpEdit::insert(offset, serialized));
            }
        }
        GraphicsMode::Delete => {
            edits.extend(
                selected
                    .iter()
                    .map(|(range, _)| SexpEdit::delete(range.0, range.1)),
            );
        }
    }

    let replacement = apply_edits(source.to_string(), edits);
    let parsed = konnect_sexp::parse_sexp(&replacement)
        .map_err(|error| invalid("graphics", format!("replacement does not parse: {error}")))?;
    if parsed.head() != Some("footprint") {
        return Err(FootprintGraphicsError::Conflict(
            "replacement changed the footprint root".to_string(),
        ));
    }

    Ok(PreparedMutation {
        replacement,
        matched_count,
        added_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OLD_FOOTPRINT: &str = r#"(footprint "Fixture" (version 20221018) (generator pcbnew)
  (layer "F.Cu")
  (fp_line (start -1 -1) (end 1 -1)
    (stroke (width 0.1) (type solid)) (layer "F.SilkS")
    (tstamp 10000000-0000-4000-8000-000000000001))
  (fp_poly
    (pts (xy 0 0) (xy 2 0) (xy 2 2))
    (stroke (width 0.05) (type solid)) (fill none) (layer "B.CrtYd")
    (tstamp 20000000-0000-4000-8000-000000000001))
  (fp_text user "KEEP ON B.CrtYd" (at 0 0) (layer "B.CrtYd")
    (effects (font (size 1 1)))
    (tstamp 30000000-0000-4000-8000-000000000001))
  (fp_poly
    (pts (xy 3 3) (xy 4 3) (xy 4 4))
    (stroke (width 0.05) (type solid)) (fill none) (layer "B.CrtYd")
    (tstamp 20000000-0000-4000-8000-000000000002))
  (pad "1" thru_hole circle (at 0 0) (size 1.8 1.8) (drill 1)
    (layers "*.Cu" "*.Mask")
    (tstamp 40000000-0000-4000-8000-000000000001))
  (model "KEEP.step"
    (offset (xyz 0 0 0))
    (scale (xyz 1 1 1))
    (rotate (xyz 0 0 0)))
)
"#;

    #[test]
    fn serializes_every_supported_primitive_as_a_footprint_graphic() {
        let graphics = parse_graphics(&json!([
            {
                "type": "line",
                "start": {"x": 0.0, "y": 1.0},
                "end": {"x": 2.0, "y": 3.0},
                "stroke_width_mm": 0.05
            },
            {
                "type": "arc",
                "start": {"x": 0.0, "y": 0.0},
                "mid": {"x": 1.0, "y": 1.0},
                "end": {"x": 2.0, "y": 0.0},
                "stroke_width_mm": 0.05
            },
            {
                "type": "rect",
                "start": {"x": -1.0, "y": -2.0},
                "end": {"x": 3.0, "y": 4.0},
                "stroke_width_mm": 0.05,
                "fill": "none"
            },
            {
                "type": "circle",
                "center": {"x": 1.0, "y": 2.0},
                "radius_mm": 1.5,
                "stroke_width_mm": 0.05,
                "fill": "solid"
            },
            {
                "type": "poly",
                "points": [
                    {"x": 0.0, "y": 0.0},
                    {"x": 2.0, "y": 0.0},
                    {"x": 2.0, "y": 1.0}
                ],
                "stroke_width_mm": 0.05,
                "fill": "none"
            }
        ]))
        .expect("valid graphics");

        let output = serialize_graphics("B.CrtYd", &graphics, "  ");

        for tag in ["fp_line", "fp_arc", "fp_rect", "fp_circle", "fp_poly"] {
            assert!(
                output.contains(&format!("({tag}")),
                "missing {tag}:\n{output}"
            );
        }
        assert_eq!(output.matches("(layer \"B.CrtYd\")").count(), 5);
        assert_eq!(output.matches("(stroke (width 0.05)").count(), 5);
        assert_eq!(output.matches("(fill none)").count(), 2);
        assert_eq!(output.matches("(fill solid)").count(), 1);
        assert!(
            konnect_sexp::parse_sexp(&format!("(footprint \"T\" (layer \"F.Cu\"){output}\n)"))
                .is_ok()
        );
    }

    #[test]
    fn rejects_invalid_or_invisible_geometry() {
        let invalid = [
            json!([{
                "type": "line",
                "start": {"x": 0.0, "y": 0.0},
                "end": {"x": 1.0, "y": 1.0},
                "stroke_width_mm": -0.05
            }]),
            json!([{
                "type": "line",
                "start": {"x": 0.0, "y": 0.0},
                "end": {"x": 0.0, "y": 0.0},
                "stroke_width_mm": 0.05
            }]),
            json!([{
                "type": "rect",
                "start": {"x": 0.0, "y": 0.0},
                "end": {"x": 0.0, "y": 1.0},
                "stroke_width_mm": 0.05,
                "fill": "none"
            }]),
            json!([{
                "type": "circle",
                "center": {"x": 0.0, "y": 0.0},
                "radius_mm": 0.0,
                "stroke_width_mm": 0.05,
                "fill": "none"
            }]),
            json!([{
                "type": "arc",
                "start": {"x": 0.0, "y": 0.0},
                "mid": {"x": 1.0, "y": 1.0},
                "end": {"x": 2.0, "y": 2.0},
                "stroke_width_mm": 0.05
            }]),
            json!([{
                "type": "poly",
                "points": [
                    {"x": 0.0, "y": 0.0},
                    {"x": 1.0, "y": 0.0},
                    {"x": 0.0, "y": 0.0}
                ],
                "stroke_width_mm": 0.05,
                "fill": "none"
            }]),
            json!([{
                "type": "poly",
                "points": [
                    {"x": 0.0, "y": 0.0},
                    {"x": 1.0, "y": 0.0},
                    {"x": 1.0, "y": 1.0}
                ],
                "stroke_width_mm": 0.0,
                "fill": "none"
            }]),
        ];

        for value in invalid {
            assert!(parse_graphics(&value).is_err(), "accepted {value}");
        }

        let non_finite = json!([{
            "type": "circle",
            "center": {"x": 0.0, "y": 0.0},
            "radius_mm": 1.0,
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]);
        let mut non_finite = non_finite;
        non_finite[0]["center"]["x"] = serde_json::Value::from(f64::INFINITY);
        assert!(parse_graphics(&non_finite).is_err());
    }

    #[test]
    fn normalizes_a_repeated_polygon_closing_point() {
        let graphics = parse_graphics(&json!([{
            "type": "poly",
            "points": [
                {"x": 0.0, "y": 0.0},
                {"x": 2.0, "y": 0.0},
                {"x": 2.0, "y": 1.0},
                {"x": 0.0, "y": 0.0}
            ],
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]))
        .unwrap();

        let output = serialize_graphics("B.CrtYd", &graphics, "  ");
        assert_eq!(output.matches("(xy 0 0)").count(), 1, "{output}");
    }

    #[test]
    fn replace_changes_only_supported_graphics_on_the_selected_layer() {
        let graphics = parse_graphics(&json!([{
            "type": "poly",
            "points": [
                {"x": -8.695, "y": 3.615},
                {"x": -8.690, "y": 3.566},
                {"x": -8.650, "y": 3.500}
            ],
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]))
        .unwrap();

        let prepared =
            prepare_mutation(OLD_FOOTPRINT, "B.CrtYd", GraphicsMode::Replace, &graphics).unwrap();

        assert_eq!(prepared.matched_count, 2);
        assert_eq!(prepared.added_count, 1);
        assert_eq!(prepared.replacement.matches("(fp_poly").count(), 1);
        assert!(!prepared
            .replacement
            .contains("20000000-0000-4000-8000-000000000001"));
        assert!(!prepared
            .replacement
            .contains("20000000-0000-4000-8000-000000000002"));
        for unchanged in [
            r#"(fp_line (start -1 -1) (end 1 -1)
    (stroke (width 0.1) (type solid)) (layer "F.SilkS")
    (tstamp 10000000-0000-4000-8000-000000000001))"#,
            r#"(fp_text user "KEEP ON B.CrtYd" (at 0 0) (layer "B.CrtYd")
    (effects (font (size 1 1)))
    (tstamp 30000000-0000-4000-8000-000000000001))"#,
            r#"(pad "1" thru_hole circle (at 0 0) (size 1.8 1.8) (drill 1)
    (layers "*.Cu" "*.Mask")
    (tstamp 40000000-0000-4000-8000-000000000001))"#,
            r#"(model "KEEP.step"
    (offset (xyz 0 0 0))
    (scale (xyz 1 1 1))
    (rotate (xyz 0 0 0)))"#,
        ] {
            assert!(
                prepared.replacement.contains(unchanged),
                "changed unrelated block:\n{unchanged}\n---\n{}",
                prepared.replacement
            );
        }
        assert!(konnect_sexp::parse_sexp(&prepared.replacement).is_ok());
    }

    #[test]
    fn append_preserves_existing_graphics_and_accepts_two_polygons() {
        let graphics = parse_graphics(&json!([
            {
                "type": "poly",
                "points": [
                    {"x": 10.0, "y": 10.0},
                    {"x": 12.0, "y": 10.0},
                    {"x": 12.0, "y": 12.0}
                ],
                "stroke_width_mm": 0.05,
                "fill": "none"
            },
            {
                "type": "poly",
                "points": [
                    {"x": -10.0, "y": -10.0},
                    {"x": -12.0, "y": -10.0},
                    {"x": -12.0, "y": -12.0}
                ],
                "stroke_width_mm": 0.05,
                "fill": "none"
            }
        ]))
        .unwrap();

        let prepared =
            prepare_mutation(OLD_FOOTPRINT, "B.CrtYd", GraphicsMode::Append, &graphics).unwrap();

        assert_eq!(prepared.matched_count, 2);
        assert_eq!(prepared.added_count, 2);
        assert_eq!(prepared.replacement.matches("(fp_poly").count(), 4);
        assert!(prepared
            .replacement
            .contains("20000000-0000-4000-8000-000000000001"));
        assert!(prepared
            .replacement
            .contains("20000000-0000-4000-8000-000000000002"));
        assert!(konnect_sexp::parse_sexp(&prepared.replacement).is_ok());
    }

    #[test]
    fn delete_removes_only_selected_supported_graphics() {
        let prepared =
            prepare_mutation(OLD_FOOTPRINT, "B.CrtYd", GraphicsMode::Delete, &[]).unwrap();

        assert_eq!(prepared.matched_count, 2);
        assert_eq!(prepared.added_count, 0);
        assert!(!prepared.replacement.contains("(fp_poly"));
        assert!(prepared
            .replacement
            .contains(r#"(fp_text user "KEEP ON B.CrtYd""#));
        assert!(prepared.replacement.contains("(pad \"1\""));
        assert!(prepared.replacement.contains("(model \"KEEP.step\""));
        assert!(konnect_sexp::parse_sexp(&prepared.replacement).is_ok());
    }

    #[test]
    fn replace_without_a_match_inserts_before_pads_and_models() {
        let source = r#"(footprint "NoGraphics" (version 20221018) (generator pcbnew)
  (layer "F.Cu")
  (pad "1" thru_hole circle (at 0 0) (size 1.8 1.8) (drill 1)
    (layers "*.Cu" "*.Mask"))
  (model "KEEP.step")
)
"#;
        let graphics = parse_graphics(&json!([{
            "type": "circle",
            "center": {"x": 0.0, "y": 0.0},
            "radius_mm": 2.0,
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]))
        .unwrap();

        let prepared =
            prepare_mutation(source, "B.CrtYd", GraphicsMode::Replace, &graphics).unwrap();

        assert_eq!(prepared.matched_count, 0);
        assert!(
            prepared.replacement.find("(fp_circle").unwrap()
                < prepared.replacement.find("(pad \"1\"").unwrap(),
            "{}",
            prepared.replacement
        );
        assert!(konnect_sexp::parse_sexp(&prepared.replacement).is_ok());
    }

    #[test]
    fn grouped_selected_graphics_are_not_deleted_in_old_or_new_syntax() {
        let old_group = OLD_FOOTPRINT.replace(
            "  (model \"KEEP.step\"",
            r#"  (group "owned" (id 50000000-0000-4000-8000-000000000001)
    (members
      20000000-0000-4000-8000-000000000001
    )
  )
  (model "KEEP.step""#,
        );
        let new_group = OLD_FOOTPRINT.replace(
            "  (model \"KEEP.step\"",
            r#"  (group "owned"
    (uuid "50000000-0000-4000-8000-000000000001")
    (members "20000000-0000-4000-8000-000000000002")
  )
  (model "KEEP.step""#,
        );

        for source in [old_group, new_group] {
            let error =
                prepare_mutation(&source, "B.CrtYd", GraphicsMode::Delete, &[]).unwrap_err();
            assert!(
                matches!(error, FootprintGraphicsError::Conflict(_)),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_non_footprint_roots_and_noncanonical_layers() {
        let graphics = parse_graphics(&json!([{
            "type": "line",
            "start": {"x": 0.0, "y": 0.0},
            "end": {"x": 1.0, "y": 1.0},
            "stroke_width_mm": 0.05
        }]))
        .unwrap();

        for (source, layer) in [
            ("(kicad_pcb (version 20240108))", "B.CrtYd"),
            (OLD_FOOTPRINT, "BottomCourtyard"),
        ] {
            assert!(matches!(
                prepare_mutation(source, layer, GraphicsMode::Replace, &graphics),
                Err(FootprintGraphicsError::InvalidArgument { .. })
            ));
        }
    }
}
