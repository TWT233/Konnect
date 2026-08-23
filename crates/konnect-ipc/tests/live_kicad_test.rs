//! Live KiCad GUI IPC regression tests.
//!
//! These tests are ignored by default. The CI live-GUI job launches pcbnew
//! under Xvfb and supplies the socket and a disposable board path.
//! `fixtures/live_ipc.kicad_pcb` is KiCad's GPL-licensed built-in
//! EuroCard160mmX100mm template, used here as a realistic footprint fixture.

use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::{parse_sexp, SexpNode};
use std::path::Path;

fn footprint<'a>(tree: &'a SexpNode, reference: &str) -> &'a SexpNode {
    tree.find_all("footprint")
        .into_iter()
        .find(|node| {
            node.find_all("property").into_iter().any(|property| {
                property.get(1).and_then(SexpNode::as_str) == Some("Reference")
                    && property.get(2).and_then(SexpNode::as_str) == Some(reference)
            })
        })
        .unwrap_or_else(|| panic!("footprint {reference} not found in saved board"))
}

fn at(node: &SexpNode) -> (f64, f64) {
    let at = node.find("at").expect("item has no (at ...) position");
    (
        at.get_f64(1).expect("invalid X coordinate"),
        at.get_f64(2).expect("invalid Y coordinate"),
    )
}

fn footprint_at(node: &SexpNode) -> (f64, f64, f64) {
    let position = node.find("at").expect("footprint has no (at ...) position");
    (
        position.get_f64(1).expect("invalid footprint X"),
        position.get_f64(2).expect("invalid footprint Y"),
        position.get_f64(3).unwrap_or(0.0),
    )
}

fn collect_geometry(node: &SexpNode, output: &mut Vec<(String, f64, f64)>) {
    if matches!(
        node.head(),
        Some("at" | "start" | "mid" | "end" | "center" | "xy")
    ) {
        if let (Some(x), Some(y)) = (node.get_f64(1), node.get_f64(2)) {
            output.push((node.head().unwrap().to_string(), x, y));
        }
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_geometry(child, output);
        }
    }
}

/// Footprint-relative child coordinates, in a canonical order.
///
/// KiCad is free to re-serialize a footprint's graphics in a different order
/// when it rewrites the file — a rotate on a footprint with several silk and
/// courtyard segments reliably shuffles them. The invariant under test is that
/// no child coordinate *changed*, not that KiCad preserved its own ordering,
/// so compare as a sorted multiset.
fn child_geometry(footprint: &SexpNode) -> Vec<(String, f64, f64)> {
    let mut output = Vec::new();
    for child in footprint.children().unwrap_or_default() {
        // The footprint's own position is the only coordinate expected to
        // change. Every nested coordinate is footprint-relative on disk.
        if child.head() != Some("at") {
            collect_geometry(child, &mut output);
        }
    }
    output.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then(a.2.total_cmp(&b.2))
    });
    output
}

fn pad_offsets(footprint: &SexpNode) -> Vec<(f64, f64)> {
    footprint.find_all("pad").into_iter().map(at).collect()
}

fn load_board(path: &Path) -> SexpNode {
    let source = std::fs::read_to_string(path).expect("failed to read live KiCad board");
    parse_sexp(&source).expect("failed to parse live KiCad board")
}

#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn moving_and_rotating_footprint_preserves_child_geometry() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let reference = std::env::var("KONNECT_LIVE_KICAD_REFERENCE").unwrap_or_else(|_| "MH1".into());
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match client.get_open_documents() {
            Ok(documents) if !documents.is_empty() => break,
            Ok(_) if std::time::Instant::now() < deadline => {}
            Ok(_) => panic!("KiCad has no PCB document open"),
            Err(error)
                if error.to_string().contains("AS_NOT_READY")
                    && std::time::Instant::now() < deadline => {}
            Err(error) => panic!("KiCad IPC connection failed: {error:#}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    client.save_board().expect("initial board save failed");
    let before_tree = load_board(Path::new(&board));
    let before = footprint(&before_tree, &reference);
    let original_at = footprint_at(before);
    let original_pads = pad_offsets(before);
    let original_geometry = child_geometry(before);
    assert!(!original_pads.is_empty(), "test footprint has no pads");

    let target = (original_at.0 + 10.0, original_at.1 + 7.0);
    client
        .move_footprint(&reference, target.0, target.1)
        .expect("footprint move failed");
    client.save_board().expect("moved board save failed");

    let after_tree = load_board(Path::new(&board));
    let after = footprint(&after_tree, &reference);
    let moved_at = at(after);
    assert!((moved_at.0 - target.0).abs() < 1e-6);
    assert!((moved_at.1 - target.1).abs() < 1e-6);
    assert_eq!(
        pad_offsets(after),
        original_pads,
        "moving a footprint must not rewrite its child-relative pad positions"
    );
    assert_eq!(
        child_geometry(after),
        original_geometry,
        "moving a footprint must preserve all child-relative geometry"
    );

    let target_rotation = (original_at.2 + 90.0) % 360.0;
    client
        .rotate_footprint(&reference, target_rotation)
        .expect("footprint rotation failed");
    client.save_board().expect("rotated board save failed");

    let rotated_tree = load_board(Path::new(&board));
    let rotated = footprint(&rotated_tree, &reference);
    assert!((footprint_at(rotated).2 - target_rotation).abs() < 1e-6);
    assert_eq!(
        child_geometry(rotated),
        original_geometry,
        "rotating a footprint must preserve all child-relative geometry"
    );
}

/// #117 regression: v0.2.1 shipped an `add_via` that KiCad rejected outright
/// with `AS_BAD_REQUEST "could not unpack PCB_VIA"`, because the padstack
/// carried two copper entries under PST_NORMAL.
///
/// Nothing offline can catch that class: the message is schema-valid, so it
/// encodes and decodes cleanly — only KiCad's own `Deserialize` refuses it.
/// This test is the gate; run it (and the rest of this file) before tagging a
/// release, not just weekly.
#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn adding_a_via_actually_creates_it_on_the_board() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);

    let net = client
        .get_nets()
        .expect("net list query failed")
        .into_iter()
        .find(|net| !net.name.is_empty())
        .expect("board has no named net to attach a via to");

    client.save_board().expect("initial board save failed");
    let vias_before = load_board(Path::new(&board)).find_all("via").len();

    // Somewhere clear of the EuroCard template's own content.
    let (x, y) = (40.0, 40.0);
    client
        .add_via(&net.name, x, y, 0.4, 0.8)
        .expect("add_via reported an error");
    client
        .save_board()
        .expect("board save after add_via failed");

    let after = load_board(Path::new(&board));
    let vias: Vec<_> = after.find_all("via");
    assert_eq!(
        vias.len(),
        vias_before + 1,
        "add_via returned Ok but the saved board has no new via — this is \
         exactly the v0.2.1 failure mode (silent success, nothing created)"
    );
    let placed = vias
        .iter()
        .find(|via| {
            via.find("at")
                .map(|node| {
                    (node.get_f64(1).unwrap_or_default() - x).abs() < 1e-6
                        && (node.get_f64(2).unwrap_or_default() - y).abs() < 1e-6
                })
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no via at ({x}, {y}) in the saved board"));
    assert!(
        placed.find("size").is_some() && placed.find("drill").is_some(),
        "via is missing its size/drill: {placed:?}"
    );
}

/// `add_zone` over IPC, end to end: create, read the zone back out of KiCad,
/// and delete it again.
///
/// This is the gate for the same class of defect `add_via` hit in v0.2.1 and
/// for the one `add_zone` itself shipped: a zone written only into the
/// `.kicad_pcb` file is invisible to an open pcbnew and is discarded by its
/// next save, so "the tool returned Ok" proves nothing. Only a live KiCad can
/// say whether the `Zone` message it was handed deserialises at all — the
/// mocks accept any schema-valid protobuf, which is exactly why a malformed
/// padstack got through offline testing before.
///
/// Reads the zone back over IPC rather than from the saved file so the
/// assertion is against KiCad's own model, including the fill it computed.
#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn adding_a_zone_creates_it_on_the_live_board() {
    use konnect_ipc::gen::kiapi::board::types::{BoardLayer, Zone, ZoneConnectionStyle, ZoneType};
    use prost::Message;

    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);

    let net = client
        .get_nets()
        .expect("net list query failed")
        .into_iter()
        .find(|net| !net.name.is_empty())
        .expect("board has no named net to attach a zone to");

    // Distinctive so the read-back cannot pick up a zone the board already had.
    let name = "konnect live add_zone";
    // Clear of the EuroCard template's own content, as the via test's spot is.
    let points = [(40.0, 60.0), (60.0, 60.0), (60.0, 75.0), (40.0, 75.0)];

    let zone_id = client
        .add_zone(&konnect_ipc::builders::ZoneSpec {
            layer: "B.Cu",
            net_name: &net.name,
            points: &points,
            clearance_mm: 0.3,
            min_thickness_mm: 0.25,
            name,
            priority: 2,
            connection: ZoneConnectionStyle::ZcsFull,
        })
        .expect("add_zone reported an error");

    let read_back = || -> Vec<Zone> {
        client
            .get_items(konnect_ipc::gen::kiapi::common::types::KiCadObjectType::KotPcbZone)
            .expect("zone query failed")
            .iter()
            .filter_map(|item| Zone::decode(item.value.as_slice()).ok())
            .filter(|zone| zone.name == name)
            .collect()
    };

    let found = read_back();
    assert_eq!(
        found.len(),
        1,
        "add_zone returned Ok but KiCad holds {} zones named {name:?} — a silent \
         success with nothing created is the v0.2.1 failure mode",
        found.len()
    );
    let zone = &found[0];

    assert_eq!(zone.r#type, ZoneType::ZtCopper as i32);
    assert_eq!(zone.layers, vec![BoardLayer::BlBCu as i32]);
    assert_eq!(zone.priority, 2);

    let outline = zone.outline.as_ref().expect("zone has no outline");
    assert_eq!(outline.polygons.len(), 1);
    let nodes = outline.polygons[0]
        .outline
        .as_ref()
        .expect("outline polyline")
        .nodes
        .len();
    assert_eq!(nodes, points.len(), "KiCad kept a different vertex count");

    let settings = match zone.settings.as_ref().expect("zone settings") {
        konnect_ipc::gen::kiapi::board::types::zone::Settings::CopperSettings(s) => s,
        other => panic!("expected copper zone settings, got {other:?}"),
    };
    assert_eq!(
        settings.net.as_ref().expect("zone net").name,
        net.name,
        "the zone landed on the wrong net"
    );
    assert_eq!(
        settings
            .connection
            .as_ref()
            .expect("connection settings")
            .zone_connection,
        ZoneConnectionStyle::ZcsFull as i32
    );
    assert!(
        zone.filled && !zone.filled_polygons.is_empty(),
        "add_zone refills before returning, so KiCad should hold computed fill \
         polygons — an outline with no copper is what the user would see"
    );

    // Leave the disposable board as we found it.
    let id = zone_id
        .or_else(|| zone.id.as_ref().map(|id| id.value.clone()))
        .expect("no KIID to delete the zone by");
    client.delete_items(vec![id]).expect("zone cleanup failed");
    assert!(
        read_back().is_empty(),
        "the test zone survived its own cleanup"
    );
}
