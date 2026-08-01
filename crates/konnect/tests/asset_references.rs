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
