//! MCP protocol tests over stdio — spawn the real binary and speak JSON-RPC.
//!
//! Codifies the smoke tests that were run by hand at release time: handshake,
//! toolset loading for the entire registry, a real file-based tool call, and
//! the structured-error taxonomy the LLM relies on for recovery.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpProcess {
    fn spawn() -> Self {
        Self::spawn_in_dir(None)
    }

    /// Spawn with the process working directory set to `dir`, so
    /// `Config::load()`'s first search path (`konnect.toml` in cwd) picks up
    /// a test config file placed there.
    fn spawn_in_dir(dir: Option<&std::path::Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_konnect"));
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn konnect binary");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut p = McpProcess {
            child,
            stdin,
            reader,
            next_id: 1,
        };
        // MCP handshake
        let init = p.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "protocol-test", "version": "0"}
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "konnect");
        p.notify("notifications/initialized");
        p
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", msg).unwrap();
        self.stdin.flush().unwrap();
        // Read lines until the response with our id arrives (skips any
        // notifications the server might emit).
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(
                n > 0,
                "server closed stdout waiting for response to {method}"
            );
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{}", msg).unwrap();
        self.stdin.flush().unwrap();
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": name, "arguments": args}));
        resp["result"].clone()
    }

    /// Send a `tools/call`, then a fencing `ping`, and return every line the
    /// server emits up to and including the ping response. The fence
    /// guarantees the read loop terminates even when the tool call emits no
    /// notification (as in bug #19), so a test can assert on side-effect
    /// notifications without risking a hang.
    fn call_tool_then_fence(&mut self, name: &str, args: Value) -> Vec<Value> {
        let call_id = self.next_id;
        self.next_id += 1;
        let call = json!({
            "jsonrpc": "2.0", "id": call_id, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        });
        writeln!(self.stdin, "{}", call).unwrap();
        let fence_id = self.next_id;
        self.next_id += 1;
        let fence = json!({"jsonrpc": "2.0", "id": fence_id, "method": "ping", "params": {}});
        writeln!(self.stdin, "{}", fence).unwrap();
        self.stdin.flush().unwrap();

        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(n > 0, "server closed stdout before fence response");
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            let is_fence = v.get("id").and_then(Value::as_i64) == Some(fence_id);
            lines.push(v);
            if is_fence {
                break;
            }
        }
        lines
    }

    /// Parse the JSON body of a tool result's first text content.
    fn tool_body(result: &Value) -> Value {
        let text = result["content"][0]["text"].as_str().unwrap_or("{}");
        serde_json::from_str(text).unwrap_or(Value::Null)
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Return a field anchor from the named symbol's direct property block. This
/// deliberately parses the generated library instead of matching independent
/// strings, so an `(at ...)` belonging to another property cannot satisfy it.
fn symbol_property_anchor(
    library: &str,
    symbol_name: &str,
    property_name: &str,
) -> (f64, f64, f64) {
    let root = konnect_sexp::parser::parse_sexp(library).expect("parse generated symbol library");
    let symbol = root
        .find_all("symbol")
        .into_iter()
        .find(|symbol| symbol.get(1).and_then(|node| node.as_str()) == Some(symbol_name))
        .unwrap_or_else(|| panic!("missing symbol {symbol_name}"));
    let property = symbol
        .find_all("property")
        .into_iter()
        .find(|property| property.get(1).and_then(|node| node.as_str()) == Some(property_name))
        .unwrap_or_else(|| panic!("missing {property_name} property"));
    let at = property
        .find("at")
        .unwrap_or_else(|| panic!("missing anchor on {property_name} property"));
    (
        at.get_f64(1).expect("anchor x"),
        at.get_f64(2).expect("anchor y"),
        at.get_f64(3).expect("anchor rotation"),
    )
}

#[test]
fn handshake_baseline_and_full_registry_loads() {
    let mut p = McpProcess::spawn();

    // Baseline tools/list: starter kit + meta-tools only (small context).
    let list = p.request("tools/list", json!({}));
    let baseline = list["result"]["tools"].as_array().unwrap().len();
    assert!(
        (10..30).contains(&baseline),
        "baseline tools/list should be the small starter kit, got {baseline}"
    );

    // list_toolboxes reports the registry; every toolset must load.
    let boxes = McpProcess::tool_body(&p.call_tool("list_toolboxes", json!({})));
    let toolsets: Vec<String> = boxes["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        toolsets.len() >= 17,
        "expected 17+ toolsets, got {}",
        toolsets.len()
    );
    // No license-era fields may reappear.
    assert!(boxes.get("license_tier").is_none());
    assert!(boxes["toolsets"][0].get("tier").is_none());

    let mut total = 0u64;
    for name in &toolsets {
        let loaded = McpProcess::tool_body(&p.call_tool("load_toolset", json!({"name": name})));
        let added = loaded["tools_added"].as_u64().unwrap_or(0);
        assert!(added > 0, "toolset '{name}' loaded no tools");
        total += added;
    }
    assert_eq!(
        total,
        boxes["total_tools"].as_u64().unwrap(),
        "sum of loaded tools disagrees with list_toolboxes total"
    );
}

#[test]
fn file_based_tool_roundtrip_in_temp_project() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proto_demo");
    let mut p = McpProcess::spawn();

    let created = p.call_tool(
        "create_project",
        json!({"name": "proto_demo", "path": proj.to_string_lossy()}),
    );
    assert_ne!(
        created["isError"],
        json!(true),
        "create_project failed: {created}"
    );
    assert!(proj.join("proto_demo.kicad_sch").exists());

    let info = p.call_tool(
        "get_project_info",
        json!({"path": proj.join("proto_demo.kicad_pro").to_string_lossy()}),
    );
    assert_ne!(
        info["isError"],
        json!(true),
        "get_project_info failed: {info}"
    );
}

#[test]
fn structured_errors_guide_recovery() {
    let mut p = McpProcess::spawn();

    // Known tool in an unloaded toolset → toolset_not_loaded naming the owner.
    let r = p.call_tool("route_trace", json!({}));
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "toolset_not_loaded");
    assert_eq!(body["error"]["toolset"], "pcb_routing");

    // Unknown tool → unknown_tool.
    let r = p.call_tool("frobnicate_board", json!({}));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "unknown_tool");

    // Missing required argument → invalid_argument naming the field.
    let r = p.call_tool("create_project", json!({"path": "/tmp/x"}));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "name");
}

#[test]
fn unknown_method_is_json_rpc_error_not_crash() {
    let mut p = McpProcess::spawn();
    let resp = p.request("tools/definitely_not_a_method", json!({}));
    assert!(
        resp.get("error").is_some(),
        "expected JSON-RPC error: {resp}"
    );
    // Server must still be alive afterwards.
    let ping = p.request("ping", json!({}));
    assert!(ping.get("result").is_some());
}

/// Regression test for issue #19. After `load_toolset`, the server must emit
/// `notifications/tools/list_changed` **over stdio** — not only over HTTP/SSE.
/// Without it, stdio clients (Claude Code) never re-fetch `tools/list`, so
/// every tool added by `load_toolset` stays uncallable for the session.
#[test]
fn load_toolset_emits_list_changed_over_stdio() {
    let mut p = McpProcess::spawn();
    let lines = p.call_tool_then_fence("load_toolset", json!({"name": "sch_components"}));
    let saw_notification = lines.iter().any(|v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
            && v.get("id").is_none()
    });
    assert!(
        saw_notification,
        "expected notifications/tools/list_changed after load_toolset (issue #19); saw: {lines:#?}"
    );
}

/// The same guarantee for `unload_toolset` — removing tools must also tell the
/// client to refresh its tool list.
#[test]
fn unload_toolset_emits_list_changed_over_stdio() {
    let mut p = McpProcess::spawn();
    let _ = p.call_tool_then_fence("load_toolset", json!({"name": "sch_components"}));
    let lines = p.call_tool_then_fence("unload_toolset", json!({"name": "sch_components"}));
    let saw_notification = lines.iter().any(|v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
            && v.get("id").is_none()
    });
    assert!(
        saw_notification,
        "expected notifications/tools/list_changed after unload_toolset; saw: {lines:#?}"
    );
}

/// `load_toolset` accepts an array of names in one call: all listed toolsets
/// load, tools_added sums across them, and only one list_changed notification
/// fires for the whole batch.
#[test]
fn load_toolset_batch_form_loads_all_and_notifies_once() {
    let mut p = McpProcess::spawn();
    let lines = p.call_tool_then_fence(
        "load_toolset",
        json!({"name": ["sch_components", "sch_wiring"]}),
    );
    let r = lines
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("expected a tools/call result")["result"]
        .clone();
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["tools_added"].as_u64(), Some(40));
    // tools items are {name, description} objects, matching the legacy
    // single-name result shape -- not bare name strings.
    let tools = body["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty());
    for t in tools {
        assert!(t.get("name").and_then(Value::as_str).is_some(), "{t:#?}");
        assert!(
            t.get("description").and_then(Value::as_str).is_some(),
            "{t:#?}"
        );
    }

    let notification_count = lines
        .iter()
        .filter(|v| {
            v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
                && v.get("id").is_none()
        })
        .count();
    assert_eq!(
        notification_count, 1,
        "expected exactly one list_changed notification for the batch; saw: {lines:#?}"
    );

    // Mixed valid/invalid names: partial failure is not isError, but the
    // errors array names the unknown toolset and loaded lists only the real one.
    let lines = p.call_tool_then_fence(
        "load_toolset",
        json!({"name": ["templates", "bogus_toolset"]}),
    );
    let r = lines
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("expected a tools/call result")["result"]
        .clone();
    assert_ne!(r["isError"].as_bool(), Some(true), "{r:#?}");
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["loaded"], json!(["templates"]));
    let errors = body["errors"].as_array().expect("errors array");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].as_str().unwrap().contains("list_toolboxes"),
        "{errors:#?}"
    );
}

/// All names in one `load_toolset` call unknown -> a typed `invalid_argument`
/// error (not a JSON body with a hand-set `isError`), so the observer keeps a
/// real `error_kind` column instead of degrading to `handler_error`.
#[test]
fn load_toolset_batch_total_failure_is_typed_error() {
    let mut p = McpProcess::spawn();
    let r = p.call_tool("load_toolset", json!({"name": ["bogus_one", "bogus_two"]}));
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "name");
    assert!(
        body["message"].as_str().unwrap().contains("list_toolboxes"),
        "{body:#?}"
    );
}

/// With `auto_load_toolsets = true` in `konnect.toml` (picked up from the
/// server process's cwd), calling a tool from an unloaded toolset auto-loads
/// it and executes in the same call instead of returning `toolset_not_loaded`.
/// Default-off behavior (no config file) is covered by
/// `structured_errors_guide_recovery`.
#[test]
fn auto_load_toolsets_config_loads_and_executes_on_miss() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("konnect.toml"),
        "auto_load_toolsets = true\n",
    )
    .unwrap();
    let mut p = McpProcess::spawn_in_dir(Some(tmp.path()));

    // route_trace is in pcb_routing, not loaded at startup. With auto-load on,
    // the toolset loads, a list_changed notification fires, and the call gets
    // as far as argument validation instead of failing with
    // toolset_not_loaded — which is what this test is about.
    //
    // The field named is `board`, the first entry in route_trace's own
    // `required` list. This used to be `net_name`, whichever argument the
    // handler happened to read first; since #218 the dispatch checks
    // `required` in schema order before the handler runs, which is the order
    // the client was shown.
    let lines = p.call_tool_then_fence("route_trace", json!({}));
    let r = lines
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("expected a tools/call result")["result"]
        .clone();
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "board");

    let saw_notification = lines.iter().any(|v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
            && v.get("id").is_none()
    });
    assert!(
        saw_notification,
        "expected notifications/tools/list_changed after auto-load; saw: {lines:#?}"
    );
}
#[test]
fn schematic_field_display_round_trips_over_stdio() {
    let temp = tempfile::tempdir().expect("create temporary protocol fixture directory");
    let schematic = temp.path().join("display.kicad_sch");
    std::fs::copy("../konnect-core/tests/fixtures/test.kicad_sch", &schematic)
        .expect("copy disposable schematic fixture");
    let labels = temp.path().join("duplicate-labels.kicad_sch");
    std::fs::write(
        &labels,
        r#"(kicad_sch
  (version 20240108)
  (generator "protocol-test")
  (uuid "00000000-0000-0000-0000-000000000000")
  (lib_symbols)
  (net_label "DUP" (at 10 20 0)
    (effects (font (size 1.27 1.27)) (justify left))
    (uuid "11111111-1111-1111-1111-111111111111")
  )
  (net_label "DUP" (at 10 20 0)
    (effects (font (size 1.27 1.27)) (justify left))
    (uuid "22222222-2222-2222-2222-222222222222")
  )
)
"#,
    )
    .expect("write duplicate labels fixture");
    let board = temp.path().join("two-zones.kicad_pcb");
    std::fs::write(
        &board,
        r#"(kicad_pcb
  (version 20260206)
  (generator "pcbnew")
  (paper "A4")
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "GND")
  (zone (net 1) (net_name "GND") (layer "F.Cu") (uuid "z1")
    (hatch edge 0.5)
    (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10))))
  (zone (net 1) (net_name "GND") (layer "B.Cu") (uuid "z2")
    (hatch edge 0.5)
    (polygon (pts (xy 20 20) (xy 30 20) (xy 30 30))))
)
"#,
    )
    .expect("write two-zone board fixture");
    let library = temp.path().join("anchors.kicad_sym");
    std::fs::write(
        &library,
        "(kicad_symbol_lib (version 20231120) (generator \"protocol-test\"))\n",
    )
    .expect("write disposable symbol library");

    let mut p = McpProcess::spawn();
    let loaded = p.call_tool(
        "load_toolset",
        json!({"name": ["sch_batch", "sch_analysis", "pcb_board", "library"]}),
    );
    assert_ne!(loaded["isError"], json!(true), "load toolsets: {loaded:#?}");

    let edit = json!([{"reference": "R1", "reference_visible": false, "value_visible": false}]);
    let hidden = McpProcess::tool_body(&p.call_tool(
        "batch_set_schematic_field_visibility",
        json!({"schematic": schematic, "edits": edit}),
    ));
    assert_eq!(hidden["updated_count"], 1, "{hidden:#?}");
    assert_eq!(
        hidden["results"][0]["reference_visible"],
        json!({"old": true, "new": false})
    );
    assert_eq!(
        hidden["results"][0]["value_visible"],
        json!({"old": true, "new": false})
    );
    let hidden_file = std::fs::read_to_string(&schematic).expect("read hidden schematic");
    assert!(hidden_file.contains("(property \"Footprint\" \"Resistor_SMD:R_0402\""));

    let shown_edit = json!([{"reference": "R1", "reference_visible": true, "value_visible": true}]);
    let shown = McpProcess::tool_body(&p.call_tool(
        "batch_set_schematic_field_visibility",
        json!({"schematic": schematic, "edits": shown_edit}),
    ));
    assert_eq!(
        shown["results"][0]["reference_visible"],
        json!({"old": false, "new": true})
    );
    assert_eq!(
        shown["results"][0]["value_visible"],
        json!({"old": false, "new": true})
    );
    let before_noop = std::fs::read(&schematic).expect("read schematic before no-op");
    let noop = p.call_tool(
        "batch_set_schematic_field_visibility",
        json!({"schematic": schematic, "edits": shown_edit}),
    );
    assert_ne!(noop["isError"], json!(true), "{noop:#?}");
    let noop_body = McpProcess::tool_body(&noop);
    assert_eq!(noop_body["updated_count"], 0, "{noop_body:#?}");
    assert_eq!(noop_body["unchanged_count"], 1, "{noop_body:#?}");
    assert_eq!(noop_body["results"][0]["reference"], "R1");
    assert_eq!(
        noop_body["results"][0]["reference_visible"],
        json!({"old": true, "new": true})
    );
    assert_eq!(
        noop_body["results"][0]["value_visible"],
        json!({"old": true, "new": true})
    );
    assert_eq!(
        std::fs::read(&schematic).expect("read schematic after no-op"),
        before_noop
    );

    let missing_edits = p.call_tool(
        "batch_set_schematic_field_visibility",
        json!({"schematic": schematic}),
    );
    assert_eq!(missing_edits["isError"], json!(true));
    let missing_body = McpProcess::tool_body(&missing_edits);
    assert_eq!(missing_body["error"]["kind"], "invalid_argument");
    assert_eq!(missing_body["error"]["field"], "edits");

    let listed =
        McpProcess::tool_body(&p.call_tool("list_schematic_labels", json!({"schematic": labels})));
    let label_uuids: Vec<&str> = listed["labels"]
        .as_array()
        .expect("labels response array")
        .iter()
        .filter(|label| {
            label["net"] == "DUP"
                && label["x"].as_f64() == Some(10.0)
                && label["y"].as_f64() == Some(20.0)
        })
        .map(|label| label["uuid"].as_str().expect("stable label uuid"))
        .collect();
    assert_eq!(label_uuids.len(), 2, "{listed:#?}");
    assert!(label_uuids.contains(&"11111111-1111-1111-1111-111111111111"));
    assert!(label_uuids.contains(&"22222222-2222-2222-2222-222222222222"));

    let board_info = McpProcess::tool_body(&p.call_tool("get_board_info", json!({"board": board})));
    assert_eq!(board_info["zone_count"], 2, "{board_info:#?}");
    assert!(board_info["paper"].is_string());
    assert!(board_info["net_count"].is_number());

    let created = p.call_tool(
        "create_symbol",
        json!({
            "library_path": library, "name": "ANCHOR_TEST", "reference_prefix": "U",
            "pins": [{"number": "1", "name": "IN", "type": "input", "x": -7.62, "y": 0.0, "angle": 0, "length": 2.54}],
            "reference_at": {"x": 0, "y": 17.78, "rotation": 0},
            "value_at": {"x": 0, "y": -20.32, "rotation": 0}
        }),
    );
    assert_ne!(
        created["isError"],
        json!(true),
        "create symbol: {created:#?}"
    );
    let library_content =
        std::fs::read_to_string(&library).expect("read disposable symbol library");
    assert_eq!(
        symbol_property_anchor(&library_content, "ANCHOR_TEST", "Reference"),
        (0.0, 17.78, 0.0)
    );
    assert_eq!(
        symbol_property_anchor(&library_content, "ANCHOR_TEST", "Value"),
        (0.0, -20.32, 0.0)
    );
}
