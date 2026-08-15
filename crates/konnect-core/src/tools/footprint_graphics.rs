use konnect_schematic_editor::types::fmt_f64;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
