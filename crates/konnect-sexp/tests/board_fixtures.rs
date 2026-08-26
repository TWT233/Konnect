//! Board-geometry parsing against KiCad's own output.
//!
//! Two oracles, one rule (the fixture rule this repo learned the hard way:
//! a fixture that shares the code's wrong assumption hides the bug):
//!
//! 1. `tests/fixtures/*.kicad_pcb` — verbatim, unmodified copies of boards
//!    from the KiCad demo corpus, so plain CI without KiCad installed still
//!    exercises the parsers on files pcbnew actually wrote.
//! 2. The installed demo corpus itself (`share/kicad/demos`), which SKIPS
//!    silently when KiCad is absent — same pattern as
//!    `konnect-core/tests/conformance_test.rs`.
//!
//! Fixture provenance (KiCad 10.0 installer, `C:\KiCad\10.0\share\kicad\demos`):
//! - `ecc83-pp.kicad_pcb` — KiCad 9 format (20241229): net table, numeric net
//!   refs, 59 segments, one GND zone, rectangular gr_line outline.
//! - `RoyalBlue54L-NFC-Antenna.kicad_pcb` — KiCad 9 format: 2 vias, and an
//!   Edge.Cuts outline with 10 `gr_arc` corner fillets/tabs whose extrema
//!   extend past the gr_line hull.
//! - `pic_programmer.kicad_pcb` — KiCad 10 format (20260206): no net table,
//!   `(net "NAME")` in place on segments, vias and zones.

use konnect_sexp::board::{board_outline_bbox, tracks, vias, zones};
use konnect_sexp::parse_sexp;
use std::path::PathBuf;

fn fixture(name: &str) -> konnect_sexp::SexpNode {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", path.display()));
    parse_sexp(&content).unwrap_or_else(|e| panic!("fixture {name} failed to parse: {e}"))
}

fn assert_bbox_eq(got: (f64, f64, f64, f64), expected: (f64, f64, f64, f64), label: &str) {
    let ok = (got.0 - expected.0).abs() < 1e-6
        && (got.1 - expected.1).abs() < 1e-6
        && (got.2 - expected.2).abs() < 1e-6
        && (got.3 - expected.3).abs() < 1e-6;
    assert!(ok, "{label}: got {got:?}, expected {expected:?}");
}

/// ecc83 (KiCad 9): counts taken from the file itself (`grep -c`), the bbox
/// from its four Edge.Cuts gr_lines.
#[test]
fn ecc83_kicad9_board_geometry() {
    let tree = fixture("ecc83-pp.kicad_pcb");

    let t = tracks(&tree);
    assert_eq!(t.skipped, 0, "pcbnew-authored segments must all parse");
    assert_eq!(t.items.len(), 59);
    for track in &t.items {
        assert!(track.width.is_finite() && track.width > 0.0);
        // Numeric refs must have resolved through the net table to names —
        // an id leaking through as "2" would mean the table lookup is dead.
        if let Some(net) = &track.net {
            assert!(
                net.parse::<u64>().is_err(),
                "net {net:?} looks like an unresolved numeric id"
            );
        }
    }
    // GND itself is only poured (the zone), never routed as segments; the
    // grid net Net-(U1A-G) is, on 17 of the 59 segments.
    assert_eq!(
        t.items
            .iter()
            .filter(|tr| tr.net.as_deref() == Some("Net-(U1A-G)"))
            .count(),
        17
    );

    assert_eq!(
        vias(&tree),
        konnect_sexp::board::Scan {
            items: vec![],
            skipped: 0
        }
    );

    let z = zones(&tree);
    assert_eq!((z.items.len(), z.skipped), (1, 0));
    assert_eq!(z.items[0].net.as_deref(), Some("GND"));
    assert_eq!(z.items[0].layers, vec!["B.Cu"]);
    assert_eq!(z.items[0].points.len(), 4);

    assert_bbox_eq(
        board_outline_bbox(&tree).expect("ecc83 has an Edge.Cuts outline"),
        (121.285, 90.17, 173.355, 136.525),
        "ecc83 outline",
    );
}

/// NFC antenna (KiCad 9): the outline's top tab is closed by gr_arcs, so the
/// true bbox reaches y = 69.058195 — the gr_lines alone stop at 71.060695.
/// Expected values independently computed (Python) from the file's Edge.Cuts
/// primitives with exact arc extrema.
#[test]
fn nfc_antenna_outline_needs_arc_extrema() {
    let tree = fixture("RoyalBlue54L-NFC-Antenna.kicad_pcb");

    let t = tracks(&tree);
    assert_eq!((t.items.len(), t.skipped), (112, 0));

    let v = vias(&tree);
    assert_eq!((v.items.len(), v.skipped), (2, 0));
    for via in &v.items {
        assert_eq!(via.net.as_deref(), Some("/ANT"));
        assert_eq!(via.layers, vec!["F.Cu", "B.Cu"]);
        assert!((via.size - 1.27).abs() < 1e-9);
        assert!((via.drill - 0.7112).abs() < 1e-9);
    }

    let bbox = board_outline_bbox(&tree).expect("outline present");
    assert_bbox_eq(
        bbox,
        (139.94971, 69.058195, 162.94971, 127.108195),
        "NFC antenna outline",
    );
    // The load-bearing part: the gr_line hull alone would report
    // min_y = 71.060695, 2 mm inside the real board edge.
    assert!(
        bbox.1 < 71.0,
        "min_y {} ignores the arc-closed tab at the top of the outline",
        bbox.1
    );
}

/// pic_programmer (KiCad 10): names in place, no net table to lean on.
#[test]
fn pic_programmer_kicad10_board_geometry() {
    let tree = fixture("pic_programmer.kicad_pcb");

    let t = tracks(&tree);
    assert_eq!((t.items.len(), t.skipped), (370, 0));
    assert!(
        t.items.iter().any(|tr| tr.net.as_deref() == Some("VCC")),
        "pic_programmer routes VCC copper"
    );

    let v = vias(&tree);
    assert_eq!((v.items.len(), v.skipped), (6, 0));
    assert!(v.items.iter().all(|via| via.net.is_some()));

    let z = zones(&tree);
    assert_eq!((z.items.len(), z.skipped), (1, 0));
    assert_eq!(z.items[0].net.as_deref(), Some("GND"));

    assert_bbox_eq(
        board_outline_bbox(&tree).expect("outline present"),
        (73.66, 40.64, 233.68, 139.7),
        "pic_programmer outline",
    );
}

/// Totality over the fixture corpus: every fixture parses and every scan runs
/// without panicking, whatever else the files contain.
#[test]
fn fixture_corpus_scans_are_total() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut seen = 0usize;
    for entry in std::fs::read_dir(&dir).expect("fixtures dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "kicad_pcb") {
            continue;
        }
        seen += 1;
        let content = std::fs::read_to_string(&path).expect("readable fixture");
        let tree = parse_sexp(&content)
            .unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()));
        for track in &tracks(&tree).items {
            assert!(track.width.is_finite() && track.width > 0.0);
            for c in [track.x1, track.y1, track.x2, track.y2] {
                assert!(c.is_finite());
            }
        }
        let _ = vias(&tree);
        let _ = zones(&tree);
        let _ = board_outline_bbox(&tree);
    }
    assert_eq!(seen, 3, "expected the three committed board fixtures");
}

// ─── Installed-demo oracle (skips without KiCad) ─────────────────────────────

fn demo_dirs() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("KICAD_DEMOS") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[
            r"C:\KiCad\10.0\share\kicad\demos",
            r"C:\Program Files\KiCad\10.0\share\kicad\demos",
        ]
    } else if cfg!(target_os = "macos") {
        &["/Applications/KiCad/KiCad.app/Contents/SharedSupport/demos"]
    } else {
        &["/usr/share/kicad/demos", "/usr/local/share/kicad/demos"]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

fn collect_boards(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "kicad_pcb") {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// Every board pcbnew ships must scan losslessly: `skipped == 0` everywhere,
/// every track sane. This is the conformance oracle for the malformed-item
/// policy itself — if a KiCad-authored file trips the skip path, the *scan*
/// is what's malformed, not the board.
#[test]
fn every_installed_demo_board_scans_losslessly() {
    let Some(root) = demo_dirs() else {
        eprintln!("SKIP: no KiCAD demos found (set KICAD_DEMOS to enable)");
        return;
    };
    let boards = collect_boards(&root);
    assert!(
        !boards.is_empty(),
        "demo dir exists but contains no .kicad_pcb files: {}",
        root.display()
    );

    let (mut n_tracks, mut n_vias, mut n_zones, mut n_bboxes) = (0usize, 0usize, 0usize, 0usize);
    let mut failures = Vec::new();
    for board in &boards {
        let content = std::fs::read_to_string(board).unwrap_or_default();
        let tree = match parse_sexp(&content) {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("{}: parse: {e}", board.display()));
                continue;
            }
        };
        let t = tracks(&tree);
        let v = vias(&tree);
        let z = zones(&tree);
        if t.skipped + v.skipped + z.skipped > 0 {
            failures.push(format!(
                "{}: skipped {} segments / {} vias / {} zones from a pcbnew-authored board",
                board.display(),
                t.skipped,
                v.skipped,
                z.skipped
            ));
        }
        for track in &t.items {
            if !(track.width.is_finite() && track.width > 0.0) {
                failures.push(format!(
                    "{}: track with width {}",
                    board.display(),
                    track.width
                ));
            }
        }
        n_tracks += t.items.len();
        n_vias += v.items.len();
        n_zones += z.items.len();
        if let Some((x0, y0, x1, y1)) = board_outline_bbox(&tree) {
            n_bboxes += 1;
            if !(x0 <= x1 && y0 <= y1) {
                failures.push(format!("{}: inverted bbox", board.display()));
            }
        }
    }
    eprintln!(
        "scanned {} demo boards: {n_tracks} tracks, {n_vias} vias, {n_zones} zones, {n_bboxes} outlines",
        boards.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Guard against a scan that quietly stops matching and passes vacuously.
    assert!(n_tracks > 500, "suspiciously few tracks ({n_tracks})");
    assert!(n_vias >= 8, "suspiciously few vias ({n_vias})");
    assert!(n_zones >= 2, "suspiciously few zones ({n_zones})");
    assert!(n_bboxes >= 3, "suspiciously few outlines ({n_bboxes})");
}
