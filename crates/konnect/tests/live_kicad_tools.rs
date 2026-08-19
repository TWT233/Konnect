//! Full MCP-tool regression against a running KiCad PCB Editor.
//!
//! The workflow opens a disposable board under Xvfb and supplies its socket.
//! This test intentionally crosses every layer: JSON-RPC stdio, tool routing,
//! platform footprint-library discovery, `.kicad_mod` preparation, and live IPC.

use konnect_ipc::builders::any_is;
use konnect_ipc::client::KiCadIpcClient;
use konnect_ipc::gen::kiapi;
use konnect_sexp::{parse_sexp, SexpNode};
use prost::Message;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpProcess {
    fn spawn(socket: &str) -> Self {
        let config = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let kicad_cli = std::env::var("KICAD_CLI").unwrap_or_else(|_| "kicad-cli".to_string());
        std::fs::write(
            config.path(),
            serde_json::to_vec(&json!({"ipc_address": socket, "kicad_cli": kicad_cli})).unwrap(),
        )
        .unwrap();
        let (_, config_path) = config.keep().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_konnect"))
            .arg("--config")
            .arg(config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to start Konnect MCP server");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut process = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        process.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "live-kicad-tools", "version": "0"}
            }),
        );
        process
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        self.stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "Konnect exited before replying"
            );
            let response: Value = serde_json::from_str(line.trim()).unwrap();
            if response["id"] == id {
                return response;
            }
        }
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let result = &response["result"];
        assert_ne!(
            result["isError"], true,
            "tool {name} failed: {}",
            result["content"][0]["text"]
        );
        result.clone()
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn footprint<'a>(tree: &'a SexpNode, reference: &str) -> &'a SexpNode {
    tree.find_all("footprint")
        .into_iter()
        .find(|node| {
            node.find_all("property").into_iter().any(|property| {
                property.get(1).and_then(SexpNode::as_str) == Some("Reference")
                    && property.get(2).and_then(SexpNode::as_str) == Some(reference)
            })
        })
        .unwrap_or_else(|| panic!("placed footprint {reference} is missing from saved board"))
}

#[test]
#[ignore = "requires a running KiCad GUI, API socket, and standard footprint libraries"]
fn place_component_loads_real_library_geometry() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let lib_id = std::env::var("KONNECT_LIVE_KICAD_FOOTPRINT")
        .unwrap_or_else(|_| "Resistor_SMD:R_0402_1005Metric".into());
    let reference =
        std::env::var("KONNECT_LIVE_KICAD_PLACE_REFERENCE").unwrap_or_else(|_| "R900".into());

    let ipc = KiCadIpcClient::new(&socket);
    let ready = (0..100).any(|_| {
        if ipc.ping().unwrap_or(false) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
        false
    });
    assert!(ready, "KiCad IPC socket never became ready");

    let mut mcp = McpProcess::spawn(&socket);
    mcp.tool("load_toolset", json!({"name": "pcb_components"}));
    let placed = mcp.tool(
        "place_component",
        json!({
            "board": board,
            "footprint": lib_id,
            "reference": reference,
            "x": 42.0,
            "y": 37.0,
            "rotation": 90.0,
            "layer": "F.Cu"
        }),
    );
    let body: Value = serde_json::from_str(placed["content"][0]["text"].as_str().unwrap())
        .expect("place_component did not return JSON");
    assert_eq!(body["placed"], reference);
    assert_eq!(body["footprint"], lib_id);

    let wrong_board = std::path::Path::new(&board)
        .with_file_name("not-the-active-board.kicad_pcb")
        .to_string_lossy()
        .into_owned();
    let rejected = mcp.request(
        "tools/call",
        json!({
            "name": "move_component",
            "arguments": {
                "board": wrong_board,
                "reference": reference,
                "x": 99.0,
                "y": 99.0
            }
        }),
    );
    assert_eq!(
        rejected["result"]["isError"], true,
        "wrong-board mutation was not rejected: {rejected}"
    );

    let edited = mcp.tool(
        "edit_component",
        json!({"board": board, "reference": reference, "value": "10k"}),
    );
    let edited: Value = serde_json::from_str(edited["content"][0]["text"].as_str().unwrap())
        .expect("edit_component did not return JSON");
    assert_eq!(edited["value"], "10k");

    let array = mcp.tool(
        "place_component_array",
        json!({
            "board": board,
            "footprint": lib_id,
            "start_x": 30.0,
            "start_y": 50.0,
            "count_x": 2,
            "count_y": 1,
            "spacing_x": 5.0,
            "ref_prefix": "R",
            "ref_start": 910
        }),
    );
    let array: Value = serde_json::from_str(array["content"][0]["text"].as_str().unwrap())
        .expect("place_component_array did not return JSON");
    assert_eq!(array["placed_count"], 2);

    let aligned = mcp.tool(
        "align_components",
        json!({
            "board": board,
            "references": ["R910", "R911"],
            "axis": "y",
            "value": 55.0
        }),
    );
    let aligned: Value = serde_json::from_str(aligned["content"][0]["text"].as_str().unwrap())
        .expect("align_components did not return JSON");
    assert_eq!(aligned["aligned_count"], 2);

    KiCadIpcClient::new(&socket)
        .save_board()
        .expect("failed to save board after placement");
    let tree = parse_sexp(&std::fs::read_to_string(&board).unwrap()).unwrap();
    let placed = footprint(&tree, &reference);
    assert!(
        placed.find_all("pad").len() >= 2,
        "placed library footprint lost its pads"
    );
    assert!(placed.find_all("property").into_iter().any(|property| {
        property.get(1).and_then(SexpNode::as_str) == Some("Value")
            && property.get(2).and_then(SexpNode::as_str) == Some("10k")
    }));
    let at = placed
        .find("at")
        .expect("placed footprint has no board position");
    assert!((at.get_f64(1).unwrap() - 42.0).abs() < 1e-6);
    assert!((at.get_f64(2).unwrap() - 37.0).abs() < 1e-6);
    assert!((at.get_f64(3).unwrap() - 90.0).abs() < 1e-6);

    for array_reference in ["R910", "R911"] {
        let array_footprint = footprint(&tree, array_reference);
        let at = array_footprint
            .find("at")
            .expect("array footprint has no board position");
        assert!((at.get_f64(2).unwrap() - 55.0).abs() < 1e-6);
    }
}

#[test]
#[ignore = "requires a running KiCad GUI, API socket, saved schematic, and matching open board"]
fn schematic_sync_apply_then_dry_run_is_noop() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let schematic = std::env::var("KONNECT_LIVE_KICAD_SCHEMATIC")
        .expect("KONNECT_LIVE_KICAD_SCHEMATIC must name the saved, closed root schematic");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let mut mcp = McpProcess::spawn(&socket);
    mcp.tool("load_toolset", json!({"name": "sch_export"}));

    let dry_run = mcp.tool(
        "update_pcb_from_schematic",
        json!({"schematic": schematic, "board": board, "dry_run": true}),
    );
    let dry_run: Value =
        serde_json::from_str(dry_run["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        dry_run["status"], "ready",
        "fixture must require a sync: {dry_run}"
    );
    let revision = dry_run["plan_revision"]
        .as_str()
        .expect("dry run returned no plan revision");

    let applied = mcp.tool(
        "update_pcb_from_schematic",
        json!({
            "schematic": schematic,
            "board": board,
            "dry_run": false,
            "expected_plan_revision": revision
        }),
    );
    let applied: Value =
        serde_json::from_str(applied["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(applied["status"], "applied", "{applied}");

    let after = mcp.tool(
        "update_pcb_from_schematic",
        json!({"schematic": schematic, "board": board, "dry_run": true}),
    );
    let after: Value = serde_json::from_str(after["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(after["status"], "noop", "apply did not converge: {after}");
}

#[test]
#[ignore = "requires a running KiCad GUI, API socket, saved schematic, and matching open board"]
fn schematic_identity_rebind_apply_then_dry_run_is_noop() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let schematic = std::env::var("KONNECT_LIVE_KICAD_SCHEMATIC")
        .expect("KONNECT_LIVE_KICAD_SCHEMATIC must name the saved, closed root schematic");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let refs_str = std::env::var("KONNECT_LIVE_KICAD_REBIND_REFERENCES")
        .expect("KONNECT_LIVE_KICAD_REBIND_REFERENCES must be comma-separated footprint references to rebind");

    let raw_refs: Vec<String> = refs_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert!(!raw_refs.is_empty(), "at least one reference required");
    let mut references = BTreeSet::new();
    for r in raw_refs {
        assert!(
            references.insert(r.clone()),
            "duplicate reference in input: {r}"
        );
    }

    let ipc = KiCadIpcClient::new(&socket);
    let ready = (0..100).any(|_| {
        if ipc.ping().unwrap_or(false) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
        false
    });
    assert!(ready, "KiCad IPC socket never became ready");

    let document = ipc
        .find_open_board(Path::new(&board))
        .expect("board must be open in KiCad");
    let items = ipc
        .get_items_in(
            document.clone(),
            kiapi::common::types::KiCadObjectType::KotPcbFootprint,
        )
        .expect("failed to get footprints from board");

    struct SnapshotEntry {
        reference: String,
        symbol_path: Option<String>,
        canonical_bytes: Vec<u8>,
    }

    fn sheet_path_string(sp: &kiapi::common::types::SheetPath) -> String {
        assert!(!sp.path.is_empty(), "SheetPath has empty path vector");
        format!(
            "/{}",
            sp.path
                .iter()
                .map(|kiid| kiid.value.as_str())
                .collect::<Vec<_>>()
                .join("/")
        )
    }

    fn reference_text(instance: &kiapi::board::types::FootprintInstance) -> String {
        instance
            .reference_field
            .as_ref()
            .and_then(|f| f.text.as_ref())
            .and_then(|bt| bt.text.as_ref())
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    fn canonical_footprint(item: &prost_types::Any) -> Vec<u8> {
        use konnect_ipc::builders::any_type_name;
        let type_name = any_type_name(item);
        if type_name != "kiapi.board.types.FootprintInstance" {
            return Vec::new();
        }
        let mut footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            .expect("invalid FootprintInstance");

        // Always ignore symbol_path for canonical equality
        footprint.symbol_path = None;

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
            if field.as_ref().and_then(|f| f.text.as_ref())
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
                if field.as_ref().and_then(|f| f.text.as_ref())
                    == Some(&kiapi::board::types::BoardText::default())
                {
                    field.as_mut().unwrap().text = None;
                }
                if *field == Some(kiapi::board::types::Field::default()) {
                    *field = None;
                }
            }
            let mut keyed: Vec<(String, Vec<u8>, prost_types::Any)> = definition
                .items
                .iter()
                .map(|item| -> (String, Vec<u8>, prost_types::Any) {
                    let type_url = item.type_url.clone();
                    let type_name = any_type_name(item);
                    let value = match type_name {
                        "kiapi.board.types.Pad" => {
                            let pad = kiapi::board::types::Pad::decode(item.value.as_slice())
                                .expect("decode Pad");
                            pad.encode_to_vec()
                        }
                        "kiapi.board.types.BoardGraphicShape" => {
                            let shape = kiapi::board::types::BoardGraphicShape::decode(
                                item.value.as_slice(),
                            )
                            .expect("decode BoardGraphicShape");
                            shape.encode_to_vec()
                        }
                        "kiapi.board.types.BoardText" => {
                            let text =
                                kiapi::board::types::BoardText::decode(item.value.as_slice())
                                    .expect("decode BoardText");
                            text.encode_to_vec()
                        }
                        "kiapi.board.types.Footprint3DModel" => {
                            let model = kiapi::board::types::Footprint3DModel::decode(
                                item.value.as_slice(),
                            )
                            .expect("decode Footprint3DModel");
                            model.encode_to_vec()
                        }
                        "kiapi.board.types.Group" => {
                            let group = kiapi::board::types::Group::decode(item.value.as_slice())
                                .expect("decode Group");
                            group.encode_to_vec()
                        }
                        _ => item.value.clone(),
                    };
                    (
                        type_url.clone(),
                        value.clone(),
                        prost_types::Any { type_url, value },
                    )
                })
                .collect();
            keyed.sort_by(|(t1, v1, _), (t2, v2, _)| t1.cmp(t2).then(v1.cmp(v2)));
            definition.items = keyed.into_iter().map(|(_, _, a)| a).collect();
        }
        footprint.encode_to_vec()
    }

    let mut before: BTreeMap<String, SnapshotEntry> = BTreeMap::new();
    let mut requested_kiids: BTreeSet<String> = BTreeSet::new();

    for item in &items {
        assert!(
            any_is(item, "kiapi.board.types.FootprintInstance"),
            "expected only FootprintInstance"
        );
        let footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            .expect("failed to decode FootprintInstance");

        let kiid = footprint
            .id
            .as_ref()
            .map(|id| id.value.clone())
            .unwrap_or_default();
        assert!(!kiid.is_empty(), "footprint has no KIID");
        let reference = reference_text(&footprint);

        let entry = SnapshotEntry {
            reference: reference.clone(),
            symbol_path: footprint.symbol_path.as_ref().map(sheet_path_string),
            canonical_bytes: canonical_footprint(item),
        };
        assert!(
            before.insert(kiid.clone(), entry).is_none(),
            "duplicate KIID in snapshot: {kiid}"
        );
        if references.contains(&reference) {
            requested_kiids.insert(kiid);
        }
    }

    for r in &references {
        assert!(
            before.values().any(|e| e.reference == *r),
            "reference {r} not found on open board"
        );
    }

    let mut mcp = McpProcess::spawn(&socket);
    mcp.tool("load_toolset", json!({"name": "sch_export"}));

    let dry_run = mcp.tool(
        "rebind_pcb_schematic_identities",
        json!({
            "schematic": schematic,
            "board": board,
            "references": references,
            "dry_run": true
        }),
    );
    let dry_run: Value =
        serde_json::from_str(dry_run["content"][0]["text"].as_str().unwrap()).unwrap();

    assert_eq!(
        dry_run["status"].as_str().unwrap(),
        "ready",
        "expected ready status, got: {dry_run}"
    );
    let coverage = &dry_run["coverage"];
    assert_eq!(
        coverage["requested"].as_u64().unwrap(),
        references.len() as u64
    );
    assert_eq!(
        coverage["eligible"].as_u64().unwrap(),
        references.len() as u64
    );
    assert_eq!(
        coverage["planned"].as_u64().unwrap(),
        references.len() as u64
    );
    assert_eq!(coverage["conflicts"].as_u64().unwrap(), 0);
    assert_eq!(dry_run["diagnostics"].as_array().unwrap().len(), 0);
    assert!(!dry_run["plan_revision"].as_str().unwrap().is_empty());
    assert!(dry_run["undo"].is_null());
    let changes = dry_run["changes"].as_array().unwrap();
    assert_eq!(changes.len(), references.len());
    let change_refs: BTreeSet<String> = changes
        .iter()
        .map(|c| c["reference"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(&change_refs, &references);

    let mut ref_to_newpath = BTreeMap::new();
    let mut ref_to_oldpath = BTreeMap::new();
    for change in changes {
        let r = change["reference"].as_str().unwrap();
        let kiid = change["kiid"].as_str().unwrap();
        assert!(
            before.contains_key(kiid),
            "change kiid {kiid} not in snapshot"
        );
        assert!(!change["old_symbol_path"].as_str().unwrap().is_empty());
        assert!(!change["new_symbol_path"].as_str().unwrap().is_empty());
        assert_ne!(
            change["old_symbol_path"].as_str().unwrap(),
            change["new_symbol_path"].as_str().unwrap(),
            "old and new symbol path must differ for {r}"
        );
        ref_to_newpath.insert(
            r.to_string(),
            change["new_symbol_path"].as_str().unwrap().to_string(),
        );
        ref_to_oldpath.insert(
            r.to_string(),
            change["old_symbol_path"].as_str().unwrap().to_string(),
        );
    }

    let revision = dry_run["plan_revision"].as_str().unwrap();
    let applied = mcp.tool(
        "rebind_pcb_schematic_identities",
        json!({
            "schematic": schematic,
            "board": board,
            "references": references,
            "dry_run": false,
            "expected_plan_revision": revision
        }),
    );
    let applied: Value =
        serde_json::from_str(applied["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        applied["status"].as_str().unwrap(),
        "applied",
        "expected applied status, got: {applied}"
    );
    let coverage = &applied["coverage"];
    assert_eq!(
        coverage["requested"].as_u64().unwrap(),
        references.len() as u64
    );
    assert_eq!(
        coverage["eligible"].as_u64().unwrap(),
        references.len() as u64
    );
    assert_eq!(
        coverage["planned"].as_u64().unwrap(),
        references.len() as u64
    );
    assert_eq!(
        coverage["applied"].as_u64().unwrap(),
        references.len() as u64
    );
    assert_eq!(coverage["conflicts"].as_u64().unwrap(), 0);
    assert_eq!(applied["diagnostics"].as_array().unwrap().len(), 0);
    assert!(!applied["undo"].as_str().unwrap().is_empty());

    let after_dry = mcp.tool(
        "rebind_pcb_schematic_identities",
        json!({
            "schematic": schematic,
            "board": board,
            "references": references,
            "dry_run": true
        }),
    );
    let after_dry: Value =
        serde_json::from_str(after_dry["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        after_dry["status"].as_str().unwrap(),
        "noop",
        "expected noop after apply, got: {after_dry}"
    );
    assert_eq!(after_dry["coverage"]["planned"].as_u64().unwrap(), 0);
    assert_eq!(after_dry["coverage"]["applied"].as_u64().unwrap(), 0);
    assert_eq!(after_dry["coverage"]["conflicts"].as_u64().unwrap(), 0);
    assert_eq!(after_dry["changes"].as_array().unwrap().len(), 0);
    assert_eq!(after_dry["diagnostics"].as_array().unwrap().len(), 0);
    assert!(after_dry["undo"].is_null());

    let after_items = ipc
        .get_items_in(
            document.clone(),
            kiapi::common::types::KiCadObjectType::KotPcbFootprint,
        )
        .expect("failed to get footprints from board after apply");

    let mut after: BTreeMap<String, SnapshotEntry> = BTreeMap::new();
    for item in &after_items {
        assert!(
            any_is(item, "kiapi.board.types.FootprintInstance"),
            "expected only FootprintInstance"
        );
        let footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            .expect("failed to decode FootprintInstance");
        let kiid = footprint
            .id
            .as_ref()
            .map(|id| id.value.clone())
            .unwrap_or_default();
        let reference = reference_text(&footprint);
        let entry = SnapshotEntry {
            reference,
            symbol_path: footprint.symbol_path.as_ref().map(sheet_path_string),
            canonical_bytes: canonical_footprint(item),
        };
        assert!(
            after.insert(kiid.clone(), entry).is_none(),
            "duplicate KIID in after-snapshot: {kiid}"
        );
    }

    let before_kiids: BTreeSet<&String> = before.keys().collect();
    let after_kiids: BTreeSet<&String> = after.keys().collect();
    assert_eq!(&before_kiids, &after_kiids, "KIID set changed unexpectedly");

    for kiid in before_kiids {
        let before_entry = before.get(kiid).unwrap();
        let after_entry = after.get(kiid).unwrap();

        if requested_kiids.contains(kiid) {
            let reference = &before_entry.reference;
            let new_path = ref_to_newpath.get(reference).unwrap();
            let old_path = ref_to_oldpath.get(reference).unwrap();
            assert_eq!(
                after_entry.symbol_path.as_deref(),
                Some(new_path.as_str()),
                "KIID {kiid} reference {reference} new path mismatch"
            );
            assert_ne!(
                after_entry.symbol_path.as_deref(),
                Some(old_path.as_str()),
                "KIID {kiid} reference {reference} path was not updated"
            );
        } else {
            assert_eq!(
                before_entry.symbol_path, after_entry.symbol_path,
                "unrelated KIID {kiid} has unexpected change to symbol_path"
            );
        }

        assert_eq!(
            before_entry.canonical_bytes, after_entry.canonical_bytes,
            "KIID {kiid} non-symbol-path fields differ after rebind"
        );
    }
}
