//! The bundled skills and agent prompts name toolsets and tools in prose.
//! Nothing compiles those names, so they rot silently as the registry moves —
//! and an LLM following a stale instruction calls a tool that does not exist.
//!
//! PR #112 fixed a batch of these by hand (`flip_component`,
//! `distribute_components`, `audit_esd_protection` had all been removed), and
//! the same sweep still left `sch_query`, `jlcpcb`, and `3d` in
//! kicad-manufacture — toolset names that have never existed. The class is
//! mechanically checkable, so check it.

use konnect_core::router::{registry, ToolRouter};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn asset_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let mut files = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    assert!(!files.is_empty(), "no markdown assets found to check");
    files
}

/// Every `load_toolset('name')` in the shipped prose names a real toolset.
#[test]
fn documented_toolsets_exist_in_the_registry() {
    let known: BTreeSet<&str> = registry::ALL_TOOLSETS.iter().map(|ts| ts.name).collect();
    let mut bad = Vec::new();

    for path in asset_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        for (line_no, line) in text.lines().enumerate() {
            for name in toolset_names_in(line) {
                if !known.contains(name.as_str()) {
                    bad.push(format!(
                        "{}:{}: load_toolset('{name}') — no such toolset",
                        display(&path),
                        line_no + 1
                    ));
                }
            }
        }
    }

    assert!(
        bad.is_empty(),
        "shipped docs reference toolsets that do not exist:\n  {}\n\nValid: {:?}",
        bad.join("\n  "),
        known
    );
}

/// Tool names listed in a `load_toolset(...)` trailing comment must resolve,
/// and must resolve to the toolset that comment is advertising — otherwise the
/// reader loads one toolset and calls a tool from another.
#[test]
fn tools_listed_beside_a_toolset_belong_to_it() {
    let router = ToolRouter::new();
    let known: BTreeSet<&str> = registry::ALL_TOOLSETS.iter().map(|ts| ts.name).collect();
    let mut bad = Vec::new();

    for path in asset_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        for (line_no, line) in text.lines().enumerate() {
            let Some((call, comment)) = line.split_once('#') else {
                continue;
            };
            let mut names = toolset_names_in(call);
            let (Some(toolset), None) = (names.next(), names.next()) else {
                continue;
            };
            if !known.contains(toolset.as_str()) {
                continue; // reported by the other test
            }
            for word in comment.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                if word.len() < 4 || !word.contains('_') {
                    continue;
                }
                match router.find_toolset_for_tool(word) {
                    None => {} // prose, not a tool name
                    Some(owner) if owner == toolset => {}
                    Some(owner) => bad.push(format!(
                        "{}:{}: load_toolset('{toolset}') lists `{word}`, which lives in '{owner}'",
                        display(&path),
                        line_no + 1
                    )),
                }
            }
        }
    }

    assert!(
        bad.is_empty(),
        "shipped docs point readers at the wrong toolset:\n  {}",
        bad.join("\n  ")
    );
}

/// A signature-shaped example — `tool(arg, arg, …)` — names real schema
/// properties, and names every required one.
///
/// The two checks above validate *tool* and *toolset* names, so an example
/// could pass them while every argument in it was invented. That is what
/// shipped: `route_pad_to_pad(from_reference, from_pad, to_reference, to_pad,
/// width, layer)` matches the schema on `width` and `layer` alone — the four
/// that identify the pads are all wrong, and the required `board` and
/// `net_name` are missing. An agent following it fails the call six ways and
/// has nothing in the error to tell it the doc was the problem (#217).
///
/// Worse, `from_reference` and friends had been added to `NOT_TOOLS` to quiet
/// the phantom-tool check — an allowlist entry asserting "this is a parameter,
/// not a tool" with nothing checking the first half of that claim (#183).
#[test]
fn call_examples_name_real_parameters() {
    let mut bad = Vec::new();

    for path in asset_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        for (lineno, line) in text.lines().enumerate() {
            for (tool, args) in signature_examples(line) {
                let Some(schema) = schema_for(&tool) else {
                    continue; // not a tool, or reported by the phantom check
                };
                let props: BTreeSet<&str> = schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|o| o.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                let required: BTreeSet<&str> = schema
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                let named: BTreeSet<&str> = args.iter().map(String::as_str).collect();
                for arg in &args {
                    if !props.contains(arg.as_str()) {
                        bad.push(format!(
                            "{}:{}: {tool}(…) names `{arg}`, which is not in its schema. Has: {props:?}",
                            display(&path),
                            lineno + 1
                        ));
                    }
                }
                for missing in required.difference(&named) {
                    bad.push(format!(
                        "{}:{}: {tool}(…) omits required `{missing}`",
                        display(&path),
                        lineno + 1
                    ));
                }
            }
        }
    }

    assert!(
        bad.is_empty(),
        "shipped call examples do not match the tool schemas:\n  {}\n\n\
         An agent copying one of these gets an invalid_argument error it cannot \
         diagnose. Fix the example to the real property names.",
        bad.join("\n  ")
    );
}

/// Every registered tool's input schema, by name.
fn schema_for(tool: &str) -> Option<serde_json::Value> {
    registry::ALL_TOOLSETS
        .iter()
        .flat_map(|ts| registry::tools_for(ts.name).unwrap_or_default())
        .find(|d| d.name == tool)
        .map(|d| d.input_schema)
}

/// `tool(a, b, c)` where every argument is a bare identifier — the form the
/// skills use to write a signature.
///
/// Anything else is left alone: a call with literal values (`load_toolset('x')`,
/// `add_via(board, "GND", 10, 20)`) is illustrating a value, not claiming a
/// parameter list, and `tool_name(params)` is the syntax itself being written
/// up. A trailing `?` marks an optional argument in the schematic skill and is
/// not part of the name.
fn signature_examples(line: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    for (open, _) in line.match_indices('(') {
        let Some(close) = line[open..].find(')').map(|i| open + i) else {
            continue;
        };
        // Walk back over the identifier that opens the call.
        let start = bytes[..open]
            .iter()
            .rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
            .map_or(0, |i| i + 1);
        let tool = &line[start..open];
        if tool.is_empty() || !tool.starts_with(|c: char| c.is_ascii_lowercase()) {
            continue;
        }
        let inner = &line[open + 1..close];
        if inner.trim().is_empty() {
            continue;
        }
        let args: Vec<String> = inner
            .split(',')
            .map(|a| a.trim().trim_end_matches('?').to_string())
            .collect();
        let bare = args.iter().all(|a| {
            !a.is_empty()
                && a.starts_with(|c: char| c.is_ascii_lowercase())
                && a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if bare {
            out.push((tool.to_string(), args));
        }
    }
    out
}

fn toolset_names_in(line: &str) -> impl Iterator<Item = String> + '_ {
    line.match_indices("load_toolset(").filter_map(|(at, _)| {
        let rest = &line[at + "load_toolset(".len()..];
        let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
        let end = rest[1..].find(quote)? + 1;
        let name = &rest[1..end];
        // `load_toolset("name")` is how the syntax itself is written up.
        (name != "name").then(|| name.to_string())
    })
}

/// Path relative to `assets/`, so the six same-named SKILL.md files are
/// distinguishable in failure output.
fn display(path: &Path) -> String {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    path.strip_prefix(&assets)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// A backticked `snake_case` word that reads like a tool name must be one.
///
/// The existing checks only look at `load_toolset(...)` call sites, so a tool
/// named in ordinary prose escapes them entirely. That is how
/// `update_pcb_from_schematic` shipped in the PCB skill's numbered layout
/// order for months — a tool that has never existed in any toolset, instructed
/// as step 2 of the standard workflow (#187). An agent following it calls a
/// name the server does not know, at the exact handoff where the netlist
/// should arrive.
///
/// Deliberately narrow: only backticked words, only snake_case with a verb-ish
/// shape, and an explicit allowlist for the non-tool identifiers the prose
/// legitimately uses. A broad heuristic here would fail on every future doc
/// edit and get deleted; this one should only fire on a real phantom.
///
/// Parameter names are exempted *from the schemas*, not by hand. The manual
/// list used to carry `from_reference`, `net_positive`, `fab_options` and
/// friends — names that are not parameters of anything, allowlisted here on the
/// strength of appearing in an example that was itself wrong. Deriving the
/// exemption from the registry means a name only escapes this check by being a
/// real property of a real tool (#183).
#[test]
fn backticked_tool_names_in_prose_exist_in_the_registry() {
    let known: BTreeSet<String> = registry::ALL_TOOLSETS
        .iter()
        .flat_map(|ts| registry::tools_for(ts.name).unwrap_or_default())
        .map(|d| d.name.to_string())
        .chain(
            ToolRouter::new()
                .all_toolsets()
                .iter()
                .map(|t| t.name.to_string()),
        )
        .collect();

    let parameters: BTreeSet<String> = registry::ALL_TOOLSETS
        .iter()
        .flat_map(|ts| registry::tools_for(ts.name).unwrap_or_default())
        .filter_map(|d| {
            d.input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect::<Vec<_>>())
        })
        .flatten()
        .collect();

    // Identifiers the prose uses that are not tools: meta-tools, config keys,
    // KiCad's own vocabulary, and file/format names.
    const NOT_TOOLS: &[&str] = &[
        "load_toolset",
        "unload_toolset",
        "list_toolboxes",
        "get_active_toolsets",
        "get_recent_calls",
        "server_stats",
        "auto_load_toolsets",
        "eager_toolsets",
        "kicad_cli",
        "kicad_binary",
        "ipc_address",
        "project_dir",
        "lib_id",
        "lib_name",
        "sym_lib_table",
        "fp_lib_table",
        "kicad_sch",
        "kicad_pcb",
        "kicad_pro",
        "kicad_sym",
        "no_connect",
        "power_in",
        "power_out",
        "open_collector",
        "open_emitter",
        "tri_state",
        // Nested public schema fields are not tool names.
        "reference_visible",
        "value_visible",
        "reference_at",
        "value_at",
        // Tool parameters are exempted from the schemas, not from this list —
        // see the doc comment. Only values and vocabulary belong here.
        "usb_c_5v_sink",
        // File extensions and other tooling vocabulary.
        "kicad_mod",
        "create_file",
        "str_replace",
        "net_label",
        "global_label",
        "hierarchical_label",
        "thru_hole",
        "np_thru_hole",
        "pin_x",
        "pin_y",
        "orientation_degrees",
        "tool_name",
        "footprint_path",
        "hot_swap",
        "exclude_from_pos_files",
        "exclude_from_bom",
        "new_number",
        "match_all",
        "replace_existing",
        "roundrect_rratio",
    ];

    let mut phantom = Vec::new();
    for path in asset_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (lineno, line) in text.lines().enumerate() {
            for word in snake_words(line) {
                if known.contains(&word)
                    || parameters.contains(&word)
                    || NOT_TOOLS.contains(&word.as_str())
                {
                    continue;
                }
                phantom.push(format!(
                    "{}:{}: `{word}` reads like a tool but is in no toolset",
                    display(&path),
                    lineno + 1
                ));
            }
        }
    }

    assert!(
        phantom.is_empty(),
        "shipped docs instruct tools that do not exist:\n  {}\n\n\
         Either the tool was renamed or removed, or the doc invented it. If the \
         word is not a tool, add it to NOT_TOOLS.",
        phantom.join("\n  ")
    );
}

/// `snake_case` words that look like a tool name — at least two
/// underscore-separated lowercase parts, so `F.Cu` and `findings` are ignored.
///
/// Both backticked and bare occurrences, because the two phantoms this exists
/// to catch took different forms: `audit_esd_protection` was backticked in a
/// reference table, and `update_pcb_from_schematic` was bare, in parentheses,
/// in a numbered workflow step (#187). Checking only one form would have
/// missed one of them.
fn snake_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    // Whether the run we are accumulating began at a real token boundary.
    // Without this, `Conn_01x02` contributes `onn_01x02` and `SOIC-8_3.9x4.9`
    // contributes `8_3` — fragments of a longer identifier, not tool names.
    let mut at_boundary = true;
    let mut started_clean = true;

    let flush = |word: &mut String, clean: bool, out: &mut Vec<String>| {
        let ok = clean
            && word.starts_with(|c: char| c.is_ascii_lowercase())
            && word.split('_').filter(|p| !p.is_empty()).count() >= 2
            && !word.split('_').any(str::is_empty);
        if ok {
            out.push(word.clone());
        }
        word.clear();
    };

    for ch in line.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            if word.is_empty() {
                started_clean = at_boundary;
            }
            word.push(ch);
        } else {
            flush(&mut word, started_clean, &mut out);
        }
        // A letter, digit or underscore means the next run is a continuation.
        at_boundary = !(ch.is_ascii_alphanumeric() || ch == '_');
    }
    flush(&mut word, started_clean, &mut out);
    out
}
