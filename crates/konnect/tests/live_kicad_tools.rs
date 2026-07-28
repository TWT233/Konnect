//! Full MCP-tool regression against a running KiCad PCB Editor.
//!
//! The workflow opens a disposable board under Xvfb and supplies its socket.
//! This test intentionally crosses every layer: JSON-RPC stdio, tool routing,
//! platform footprint-library discovery, `.kicad_mod` preparation, and live IPC.

use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::{parse_sexp, SexpNode};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
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
        std::fs::write(
            config.path(),
            serde_json::to_vec(&json!({"ipc_address": socket})).unwrap(),
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
