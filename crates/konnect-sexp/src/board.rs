//! Structural queries over a `.kicad_pcb` tree.
//!
//! These exist because `SexpNode::find_all` is **direct children only**, which
//! is easy to forget: `footprint`, `segment`, `via` and `net` *are* direct
//! children of `(kicad_pcb …)`, so `tree.find_all("footprint")` is right — and
//! `pad` is not, so `tree.find_all("pad")` silently returns 0 on every board
//! ever written. Design review reported `pads: 0` for the whole life of its
//! coverage block because of exactly that (#246).
//!
//! # Malformed-item policy
//!
//! A copper item missing a load-bearing field (a segment without an endpoint,
//! a via without a drill) is **skipped and counted**, never fabricated: a
//! coordinate defaulted to 0.0 places phantom copper at the board origin and
//! everything downstream — placement, test-point selection, voltage drop —
//! computes confidently from it. Every scan therefore returns a [`Scan`]
//! carrying the parsed items *and* how many matching nodes were dropped, so a
//! caller can refuse to trust a board that lost items rather than never
//! finding out.
//!
//! # Net identity
//!
//! Items name their net two ways depending on the file format (see
//! [`crate::net`]): KiCad 10 writes `(net "GND")` in place; KiCad ≤ 9 writes
//! `(net 2)` and declares `(net 2 "GND")` once at top level. The scans here
//! resolve both to the net **name**, using the board's own top-level table for
//! the numeric form, so the same physical net gets the same key whichever
//! format wrote the file. The unconnected pseudo-net (net 0 / the empty name)
//! resolves to `None` — copper with no net is a real state, not a net called
//! `""`.

use crate::net;
use crate::parser::SexpNode;
use std::collections::HashMap;

/// Every footprint on a board, in file order.
///
/// Footprints are direct children of `(kicad_pcb …)`, so this is a thin
/// wrapper — it exists so pad counting has an obvious partner and callers stop
/// reaching for `find_all` directly on the root.
pub fn footprints(tree: &SexpNode) -> Vec<&SexpNode> {
    tree.find_all("footprint")
}

/// Every pad on the board, across all footprints.
///
/// Pads live one level down, inside each `(footprint …)`. Call this rather
/// than `tree.find_all("pad")`, which cannot ever match.
pub fn pads(tree: &SexpNode) -> Vec<&SexpNode> {
    footprints(tree)
        .into_iter()
        .flat_map(|fp| fp.find_all("pad"))
        .collect()
}

/// How many pads the board has. Zero from a board that has footprints means
/// something is wrong with the board or the parse — it is not a normal state.
pub fn count_pads(tree: &SexpNode) -> usize {
    pads(tree).len()
}

/// The result of a lossy structural scan: everything that parsed, plus how
/// many nodes matched the tag but were too malformed to represent.
///
/// `skipped` is the module's alternative to silently dropping or — worse —
/// zero-filling broken items (see the module docs). `skipped > 0` on a
/// KiCad-authored board means the file or the parser is wrong; callers that
/// feed analysis tools should surface it, not ignore it.
#[derive(Debug, Clone, PartialEq)]
pub struct Scan<T> {
    /// Items that carried every load-bearing field, in file order.
    pub items: Vec<T>,
    /// Nodes with the right tag that were dropped for missing or non-finite
    /// load-bearing fields.
    pub skipped: usize,
}

/// One `(segment …)` — a straight copper track.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    /// Track width in mm. Guaranteed finite and > 0 — a segment claiming
    /// otherwise is skipped, because zero-width copper poisons every
    /// resistance/current computation built on it.
    pub width: f64,
    pub layer: String,
    /// Resolved net name (see the module docs); `None` is the unconnected
    /// pseudo-net.
    pub net: Option<String>,
    pub uuid: Option<String>,
}

/// One `(via …)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Via {
    pub x: f64,
    pub y: f64,
    /// Pad (annular) diameter in mm; finite and > 0.
    pub size: f64,
    /// Drill diameter in mm; finite and > 0. KiCad always writes it, so a via
    /// without one is malformed — the netclass default it once implied cannot
    /// be recovered from the board file alone.
    pub drill: f64,
    /// The copper layers the via spans, e.g. `["F.Cu", "B.Cu"]`.
    pub layers: Vec<String>,
    /// Resolved net name; `None` is the unconnected pseudo-net.
    pub net: Option<String>,
    pub uuid: Option<String>,
}

/// The authored outline of one `(zone …)` — the polygon the user drew, not
/// the filled copper (`filled_polygon`), which refills on every pour and can
/// be absent entirely on an unfilled zone.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneOutline {
    /// Resolved net name; `None` covers both the unconnected pseudo-net and
    /// net-less zones (keepouts).
    pub net: Option<String>,
    /// Layers the zone lives on: KiCad 10 writes `(layers …)` (plural), older
    /// single-layer zones `(layer …)`. Both shapes land here.
    pub layers: Vec<String>,
    /// Outline vertices of the zone's first `(polygon (pts …))`, in file
    /// order. Always ≥ 3 points — fewer cannot enclose area, so such a zone
    /// is skipped.
    pub points: Vec<(f64, f64)>,
}

/// Every routed track segment on the board.
///
/// Segments are direct children of `(kicad_pcb …)` in every format KiCad has
/// written — verified against the KiCad 9 and 10 demo corpus, not assumed
/// (#246). Curved tracks (`(arc …)`) are a different node and are *not*
/// included here.
pub fn tracks(tree: &SexpNode) -> Scan<Track> {
    let table = top_level_net_table(tree);
    let mut items = Vec::new();
    let mut skipped = 0usize;
    for seg in tree.find_all("segment") {
        let parsed = (|| {
            let (x1, y1) = point(seg, "start")?;
            let (x2, y2) = point(seg, "end")?;
            let width = seg
                .find_f64("width")
                .filter(|w| w.is_finite() && *w > 0.0)?;
            let layer = seg.find_str("layer")?.to_string();
            Some(Track {
                x1,
                y1,
                x2,
                y2,
                width,
                layer,
                net: resolve_net(seg, &table),
                uuid: seg.find_str("uuid").map(str::to_string),
            })
        })();
        match parsed {
            Some(t) => items.push(t),
            None => skipped += 1,
        }
    }
    Scan { items, skipped }
}

/// Every via on the board (direct children of `(kicad_pcb …)`).
pub fn vias(tree: &SexpNode) -> Scan<Via> {
    let table = top_level_net_table(tree);
    let mut items = Vec::new();
    let mut skipped = 0usize;
    for via in tree.find_all("via") {
        let parsed = (|| {
            let (x, y) = point(via, "at")?;
            let size = via.find_f64("size").filter(|v| v.is_finite() && *v > 0.0)?;
            let drill = via
                .find_f64("drill")
                .filter(|v| v.is_finite() && *v > 0.0)?;
            let layers_node = via.find("layers")?;
            let layers: Vec<String> = layers_node
                .children()?
                .iter()
                .skip(1)
                .filter_map(|c| c.as_str())
                .map(str::to_string)
                .collect();
            if layers.is_empty() {
                return None;
            }
            Some(Via {
                x,
                y,
                size,
                drill,
                layers,
                net: resolve_net(via, &table),
                uuid: via.find_str("uuid").map(str::to_string),
            })
        })();
        match parsed {
            Some(v) => items.push(v),
            None => skipped += 1,
        }
    }
    Scan { items, skipped }
}

/// The authored outline of every zone on the board.
///
/// Only the first `(polygon …)` of each zone is read — that is the outline
/// the user drew; a zone can additionally carry rule areas and per-layer
/// `filled_polygon`s that are derived data.
pub fn zones(tree: &SexpNode) -> Scan<ZoneOutline> {
    let table = top_level_net_table(tree);
    let mut items = Vec::new();
    let mut skipped = 0usize;
    for zone in tree.find_all("zone") {
        let parsed = (|| {
            // KiCad 10 zones write (layers …); legacy single-layer zones
            // write (layer …). Read whichever shape is present.
            let layers: Vec<String> = match zone.find("layers") {
                Some(node) => node
                    .children()?
                    .iter()
                    .skip(1)
                    .filter_map(|c| c.as_str())
                    .map(str::to_string)
                    .collect(),
                None => vec![zone.find_str("layer")?.to_string()],
            };
            if layers.is_empty() {
                return None;
            }
            let pts = zone.find("polygon")?.find("pts")?;
            let points: Vec<(f64, f64)> = pts
                .find_all("xy")
                .into_iter()
                .map(|xy| {
                    let (x, y) = (xy.get_f64(1)?, xy.get_f64(2)?);
                    (x.is_finite() && y.is_finite()).then_some((x, y))
                })
                .collect::<Option<Vec<_>>>()?;
            if points.len() < 3 {
                return None; // fewer than 3 vertices encloses no area
            }
            Some(ZoneOutline {
                net: resolve_net(zone, &table),
                layers,
                points,
            })
        })();
        match parsed {
            Some(z) => items.push(z),
            None => skipped += 1,
        }
    }
    Scan { items, skipped }
}

/// Bounding box `(min_x, min_y, max_x, max_y)` of the board outline: every
/// `Edge.Cuts` graphic that is a direct child of `(kicad_pcb …)` — `gr_line`,
/// `gr_rect`, `gr_arc`, `gr_circle`, `gr_curve`.
///
/// Arcs use exact extrema ([`crate::geometry::arc_bbox`]): a board whose
/// outline bulges through a fillet or a semicircular edge is wider than its
/// endpoints say. Bézier `gr_curve`s use the control-point hull, which is a
/// (tight enough) superset of the curve.
///
/// All-or-nothing: `None` when there are no Edge.Cuts graphics **or when any
/// of them is malformed**. A partial outline bbox looks exactly like a
/// finished one and silently mis-sizes the board, so a single broken edge
/// graphic invalidates the answer rather than shrinking it.
pub fn board_outline_bbox(tree: &SexpNode) -> Option<(f64, f64, f64, f64)> {
    const EDGE_TAGS: [&str; 5] = ["gr_line", "gr_rect", "gr_arc", "gr_circle", "gr_curve"];
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for child in tree.children().unwrap_or(&[]) {
        let Some(head) = child.head() else { continue };
        if !EDGE_TAGS.contains(&head) || child.find_str("layer") != Some("Edge.Cuts") {
            continue;
        }
        let bb = graphic_bbox(child, head)?;
        acc = Some(match acc {
            None => bb,
            Some((x0, y0, x1, y1)) => (x0.min(bb.0), y0.min(bb.1), x1.max(bb.2), y1.max(bb.3)),
        });
    }
    acc
}

/// Bbox of a single edge graphic; `None` when a load-bearing coordinate is
/// missing or non-finite.
fn graphic_bbox(node: &SexpNode, head: &str) -> Option<(f64, f64, f64, f64)> {
    match head {
        "gr_line" | "gr_rect" => {
            let (x1, y1) = point(node, "start")?;
            let (x2, y2) = point(node, "end")?;
            Some((x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)))
        }
        "gr_arc" => {
            let start = point(node, "start")?;
            let mid = point(node, "mid")?;
            let end = point(node, "end")?;
            Some(crate::geometry::arc_bbox(start, mid, end))
        }
        "gr_circle" => {
            let (cx, cy) = point(node, "center")?;
            let (ex, ey) = point(node, "end")?;
            let r = (ex - cx).hypot(ey - cy);
            Some((cx - r, cy - r, cx + r, cy + r))
        }
        "gr_curve" => {
            // Cubic Bézier: the control polygon contains the curve, so its
            // hull is a valid (if slightly loose) bbox.
            let pts = node.find("pts")?;
            let mut acc: Option<(f64, f64, f64, f64)> = None;
            for xy in pts.find_all("xy") {
                let (x, y) = (xy.get_f64(1)?, xy.get_f64(2)?);
                if !x.is_finite() || !y.is_finite() {
                    return None;
                }
                acc = Some(match acc {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
            acc
        }
        _ => None,
    }
}

/// `(tag x y)` as a finite coordinate pair, or `None` — never a zero-filled
/// stand-in (see the module docs).
fn point(node: &SexpNode, tag: &str) -> Option<(f64, f64)> {
    let p = node.find(tag)?;
    let (x, y) = (p.get_f64(1)?, p.get_f64(2)?);
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

/// The board's top-level net table (`(net N "NAME")` direct children), which
/// only KiCad ≤ 9 writes. Keys are the numeric ids as written.
fn top_level_net_table(tree: &SexpNode) -> HashMap<String, String> {
    tree.find_all("net")
        .into_iter()
        .filter_map(|n| Some((net::net_id(n)?.to_string(), net::net_name(n)?.to_string())))
        .collect()
}

/// Resolve an item's `(net …)` child to a net name using both format shapes
/// (see [`crate::net`]). `None` means unconnected — net 0, the empty name, or
/// no net node at all. A numeric reference the table cannot resolve keeps the
/// id digits as its key: that is real identity from the file (the same
/// fallback [`net::collect_net_keys`] uses), unlike a fabricated name.
fn resolve_net(item: &SexpNode, table: &HashMap<String, String>) -> Option<String> {
    let node = item.find("net")?;
    if let Some(name) = net::net_name(node) {
        return (!name.is_empty()).then(|| name.to_string());
    }
    let id = net::net_id(node)?;
    if id == "0" {
        return None; // the unconnected pseudo-net
    }
    match table.get(id) {
        Some(name) if !name.is_empty() => Some(name.clone()),
        Some(_) => None, // declared as the unconnected pseudo-net
        None => Some(id.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_sexp;

    /// A board carrying two footprints of two pads each, in KiCad's own
    /// tab-indented layout.
    const BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(footprint \"R_0402\"\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(pad \"1\" smd roundrect\n\
        \t\t\t(at -0.51 0)\n\
        \t\t\t(size 0.54 0.64)\n\
        \t\t)\n\
        \t\t(pad \"2\" smd roundrect\n\
        \t\t\t(at 0.51 0)\n\
        \t\t\t(size 0.54 0.64)\n\
        \t\t)\n\
        \t)\n\
        \t(footprint \"C_0402\"\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(pad \"1\" smd roundrect\n\
        \t\t\t(at -0.51 0)\n\
        \t\t)\n\
        \t\t(pad \"2\" smd roundrect\n\
        \t\t\t(at 0.51 0)\n\
        \t\t)\n\
        \t)\n\
        )";

    /// The bug this module exists to prevent: `find_all` does not recurse, so
    /// asking the root for pads is not merely inaccurate, it is always zero.
    #[test]
    fn pads_are_nested_so_the_root_never_sees_them() {
        let tree = parse_sexp(BOARD).unwrap();

        assert_eq!(
            tree.find_all("pad").len(),
            0,
            "if this ever becomes non-zero, find_all started recursing and \
             every caller needs rechecking"
        );
        assert_eq!(count_pads(&tree), 4);
        assert_eq!(footprints(&tree).len(), 2);
    }

    #[test]
    fn a_board_with_no_footprints_has_no_pads() {
        let tree = parse_sexp("(kicad_pcb\n\t(version 20260206)\n)").unwrap();
        assert_eq!(count_pads(&tree), 0);
        assert_eq!(footprints(&tree).len(), 0);
    }

    /// KiCad ≤ 9 shapes, node-for-node as pcbnew 9 writes them (taken from the
    /// ecc83 and RoyalBlue54L-NFC-Antenna demo boards): top-level net table,
    /// numeric net references, single-layer zone with `(net_name …)`.
    const KICAD9_BOARD: &str = "(kicad_pcb\n\
        \t(version 20241229)\n\
        \t(generator \"pcbnew\")\n\
        \t(net 0 \"\")\n\
        \t(net 1 \"GND\")\n\
        \t(net 2 \"Net-(P3-P1)\")\n\
        \t(segment\n\
        \t\t(start 139.573 99.695)\n\
        \t\t(end 141.605 99.695)\n\
        \t\t(width 0.8)\n\
        \t\t(layer \"B.Cu\")\n\
        \t\t(net 2)\n\
        \t\t(uuid \"1d6285fd-2d49-4956-932f-458079ff628a\")\n\
        \t)\n\
        \t(segment\n\
        \t\t(start 0 0)\n\
        \t\t(end 1 0)\n\
        \t\t(width 0.5)\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(net 0)\n\
        \t)\n\
        \t(via\n\
        \t\t(at 152.65011 73.152695)\n\
        \t\t(size 1.27)\n\
        \t\t(drill 0.7112)\n\
        \t\t(layers \"F.Cu\" \"B.Cu\")\n\
        \t\t(tenting front back)\n\
        \t\t(net 1)\n\
        \t\t(uuid \"44daf9b6-a0ef-4d27-aaa3-1cdd2fffc238\")\n\
        \t)\n\
        \t(zone\n\
        \t\t(net 1)\n\
        \t\t(net_name \"GND\")\n\
        \t\t(layer \"B.Cu\")\n\
        \t\t(hatch edge 0.508)\n\
        \t\t(polygon\n\
        \t\t\t(pts\n\
        \t\t\t\t(xy 172.085 135.89) (xy 172.085 91.313) (xy 122.555 91.44) (xy 122.555 135.89)\n\
        \t\t\t)\n\
        \t\t)\n\
        \t)\n\
        )";

    /// KiCad 10 shapes (from the pic_programmer demo): no net table, names in
    /// place on every item.
    const KICAD10_BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(segment\n\
        \t\t(start 110.49 124.155)\n\
        \t\t(end 110.49 119.38)\n\
        \t\t(width 0.8)\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(net \"VCC\")\n\
        \t\t(uuid \"234aa39b-64ad-4fb9-b7a9-cdc8cbfb4541\")\n\
        \t)\n\
        \t(via\n\
        \t\t(at 189.865 110.49)\n\
        \t\t(size 1.6)\n\
        \t\t(drill 0.6)\n\
        \t\t(layers \"F.Cu\" \"B.Cu\")\n\
        \t\t(net \"/CLOCK-RB6\")\n\
        \t\t(uuid \"00c62925-76e5-4da4-9776-805e7e214afd\")\n\
        \t)\n\
        \t(zone\n\
        \t\t(net \"GND\")\n\
        \t\t(layer \"B.Cu\")\n\
        \t\t(polygon\n\
        \t\t\t(pts\n\
        \t\t\t\t(xy 223.52 138.43) (xy 232.41 128.905) (xy 232.41 53.975)\n\
        \t\t\t)\n\
        \t\t)\n\
        \t)\n\
        )";

    #[test]
    fn tracks_resolve_numeric_nets_through_the_table() {
        let tree = parse_sexp(KICAD9_BOARD).unwrap();
        let scan = tracks(&tree);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items.len(), 2);

        let t = &scan.items[0];
        assert_eq!((t.x1, t.y1, t.x2, t.y2), (139.573, 99.695, 141.605, 99.695));
        assert_eq!(t.width, 0.8);
        assert_eq!(t.layer, "B.Cu");
        assert_eq!(t.net.as_deref(), Some("Net-(P3-P1)"));
        assert_eq!(
            t.uuid.as_deref(),
            Some("1d6285fd-2d49-4956-932f-458079ff628a")
        );

        // (net 0) is the unconnected pseudo-net, not a net named "0".
        assert_eq!(scan.items[1].net, None);
        assert_eq!(scan.items[1].uuid, None);
    }

    #[test]
    fn tracks_read_kicad_10_names_in_place() {
        let tree = parse_sexp(KICAD10_BOARD).unwrap();
        let scan = tracks(&tree);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items.len(), 1);
        assert_eq!(scan.items[0].net.as_deref(), Some("VCC"));
        assert_eq!(scan.items[0].width, 0.8);
    }

    #[test]
    fn vias_read_both_net_shapes() {
        let k9 = parse_sexp(KICAD9_BOARD).unwrap();
        let scan = vias(&k9);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items.len(), 1);
        let v = &scan.items[0];
        assert_eq!((v.x, v.y), (152.65011, 73.152695));
        assert_eq!((v.size, v.drill), (1.27, 0.7112));
        assert_eq!(v.layers, vec!["F.Cu", "B.Cu"]);
        assert_eq!(v.net.as_deref(), Some("GND"));

        let k10 = parse_sexp(KICAD10_BOARD).unwrap();
        let scan = vias(&k10);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items[0].net.as_deref(), Some("/CLOCK-RB6"));
    }

    #[test]
    fn zones_take_the_authored_outline() {
        let k9 = parse_sexp(KICAD9_BOARD).unwrap();
        let scan = zones(&k9);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items.len(), 1);
        let z = &scan.items[0];
        assert_eq!(z.net.as_deref(), Some("GND"));
        assert_eq!(z.layers, vec!["B.Cu"]);
        assert_eq!(
            z.points,
            vec![
                (172.085, 135.89),
                (172.085, 91.313),
                (122.555, 91.44),
                (122.555, 135.89),
            ]
        );

        let k10 = parse_sexp(KICAD10_BOARD).unwrap();
        assert_eq!(zones(&k10).items[0].net.as_deref(), Some("GND"));
    }

    #[test]
    fn a_kicad_10_zone_may_span_several_layers() {
        let tree = parse_sexp(
            "(kicad_pcb\n\t(zone\n\t\t(net \"GND\")\n\t\t(layers \"F.Cu\" \"B.Cu\")\n\
             \t\t(polygon (pts (xy 0 0) (xy 10 0) (xy 10 10)))\n\t))",
        )
        .unwrap();
        let scan = zones(&tree);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items[0].layers, vec!["F.Cu", "B.Cu"]);
    }

    /// The module's malformed-item policy: broken nodes are dropped *and
    /// counted*, and are never zero-filled into phantom copper at the origin.
    #[test]
    fn malformed_items_are_counted_not_fabricated() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(segment (start 0 0) (width 0.5) (layer \"F.Cu\"))\n\
             \t(segment (start 0 0) (end 1 0) (width 0) (layer \"F.Cu\"))\n\
             \t(segment (start 0 0) (end 1 0) (width 0.5) (layer \"F.Cu\"))\n\
             \t(via (at 1 1) (size 0.8) (layers \"F.Cu\" \"B.Cu\"))\n\
             \t(zone (layer \"F.Cu\") (polygon (pts (xy 0 0) (xy 1 0))))\n\
             )",
        )
        .unwrap();
        let t = tracks(&tree);
        assert_eq!((t.items.len(), t.skipped), (1, 2)); // no end; zero width
        let v = vias(&tree);
        assert_eq!((v.items.len(), v.skipped), (0, 1)); // no drill
        let z = zones(&tree);
        assert_eq!((z.items.len(), z.skipped), (0, 1)); // 2 points enclose nothing
    }

    #[test]
    fn every_parsed_track_has_positive_finite_width() {
        // "NaN" parses as a valid f64 — the finiteness filter must catch it.
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(segment (start 0 0) (end 1 0) (width NaN) (layer \"F.Cu\"))\n\
             \t(segment (start inf 0) (end 1 0) (width 0.5) (layer \"F.Cu\"))\n\
             )",
        )
        .unwrap();
        let t = tracks(&tree);
        assert_eq!((t.items.len(), t.skipped), (0, 2));
    }

    /// Edge.Cuts bbox over the ecc83 demo's outline shape: four `gr_line`s
    /// forming a rectangle (coordinates verbatim from the demo board).
    #[test]
    fn outline_bbox_of_a_rectangular_line_outline() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(gr_line (start 173.355 90.17) (end 173.355 136.525) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 121.285 90.17) (end 121.285 136.525) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 173.355 90.17) (end 121.285 90.17) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 121.285 136.525) (end 173.355 136.525) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 0 0) (end 500 500) (layer \"F.SilkS\"))\n\
             )",
        )
        .unwrap();
        // The silkscreen line must not leak into the outline.
        assert_eq!(
            board_outline_bbox(&tree),
            Some((121.285, 90.17, 173.355, 136.525))
        );
    }

    /// An arc that bulges past both of its endpoints must widen the bbox by
    /// its true extrema, not its endpoint hull: semicircular board edge from
    /// (0, 0) to (10, 0) bulging down through (5, -5).
    #[test]
    fn outline_bbox_uses_exact_arc_extrema() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(gr_line (start 0 0) (end 10 0) (layer \"Edge.Cuts\"))\n\
             \t(gr_arc (start 0 0) (mid 5 -5) (end 10 0) (layer \"Edge.Cuts\"))\n\
             )",
        )
        .unwrap();
        let (x0, y0, x1, y1) = board_outline_bbox(&tree).unwrap();
        assert!((x0 - 0.0).abs() < 1e-9 && (x1 - 10.0).abs() < 1e-9);
        assert!((y1 - 0.0).abs() < 1e-9);
        // The bulge: min_y is the arc's -Y extreme at -5, far below the
        // endpoint hull's 0.
        assert!((y0 - -5.0).abs() < 1e-9, "min_y = {y0}, expected -5");
    }

    #[test]
    fn outline_bbox_handles_circles_rects_and_curves() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(gr_circle (center 50 50) (end 53 54) (layer \"Edge.Cuts\"))\n\
             \t(gr_rect (start 60 60) (end 40 45) (layer \"Edge.Cuts\"))\n\
             \t(gr_curve (pts (xy 30 50) (xy 32 48) (xy 35 47) (xy 38 50)) (layer \"Edge.Cuts\"))\n\
             )",
        )
        .unwrap();
        // circle r = 5 about (50, 50) → (45, 45, 55, 55); rect corners are
        // unordered; curve hull reaches x = 30, y = 47.
        assert_eq!(board_outline_bbox(&tree), Some((30.0, 45.0, 60.0, 60.0)));
    }

    /// All-or-nothing: one malformed edge graphic poisons the whole bbox —
    /// a partial outline is indistinguishable from a complete one.
    #[test]
    fn outline_bbox_refuses_a_partially_readable_outline() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(gr_line (start 0 0) (end 10 0) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 0 0) (layer \"Edge.Cuts\"))\n\
             )",
        )
        .unwrap();
        assert_eq!(board_outline_bbox(&tree), None);
    }

    #[test]
    fn outline_bbox_is_none_without_edge_cuts() {
        let tree = parse_sexp("(kicad_pcb\n\t(version 20260206)\n)").unwrap();
        assert_eq!(board_outline_bbox(&tree), None);
    }
}
