//! The tool counts quoted in the docs must match the registry.
//!
//! CONTRIBUTING asks for `router/registry.rs`, `tool-directory.md`, DEV.md's
//! "Current Stats" and the README to move together, and notes that "those
//! three counts have drifted apart before precisely because only one of them
//! got updated". Nothing enforced it: `registry_tool_counts_match_reality`
//! only checks each toolset's `tool_count` against `tools_for()`, so a PR that
//! adds a tool and updates two of the four documents is green.
//!
//! That is not hypothetical — PRs #159 and #160 each bump `registry.rs`,
//! `tool-directory.md` and DEV.md while leaving README.md behind, and CI has
//! nothing to say about it.
//!
//! So derive the numbers from the registry and require the prose to agree.

use konnect_core::router::registry;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // crates/konnect -> crates -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root above crates/konnect")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

/// Ground truth: what the registry actually declares.
fn counts() -> (usize, usize, usize) {
    let toolsets = registry::ALL_TOOLSETS.len();
    let registered: usize = registry::ALL_TOOLSETS.iter().map(|t| t.tool_count).sum();
    let meta = konnect_core::router::meta_tools::meta_tool_descriptions().len();
    (toolsets, registered, meta)
}

/// Every number a document is required to quote, with the exact spelling to
/// look for. Kept as whole phrases rather than bare integers so a coincidental
/// "187" elsewhere in the file cannot satisfy the check.
fn required_phrases() -> Vec<(&'static str, String)> {
    let (toolsets, registered, meta) = counts();
    let total = registered + meta;
    vec![
        (
            "README.md",
            format!("**{registered} tools across {toolsets} on-demand toolsets.**"),
        ),
        (
            "DEV.md",
            format!("**{toolsets} toolsets, {registered} tools** + {meta} meta-tools"),
        ),
        (
            "DEV.md",
            format!("{total} tools ({registered} registered + {meta} meta)"),
        ),
    ]
}

#[test]
fn docs_quote_the_registry_tool_counts() {
    let mut wrong = Vec::new();
    for (file, phrase) in required_phrases() {
        if !read(file).contains(&phrase) {
            wrong.push(format!("{file} is missing: {phrase}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "tool counts have drifted from the registry.\n{}\n\n\
         Update registry.rs, tool-directory.md, DEV.md's \"Current Stats\" and \
         the README together — see CONTRIBUTING.",
        wrong.join("\n")
    );
}

/// `tool-directory.md` lists every tool in a table, so its row count is a
/// second, independent statement of the same number. A tool added to the
/// registry without a directory entry is undocumented; one listed but not
/// registered is a phantom the LLM will try to call.
#[test]
fn tool_directory_lists_every_registered_tool() {
    let directory = read("tool-directory.md");

    let mut missing = Vec::new();
    for ts in registry::ALL_TOOLSETS {
        for def in registry::tools_for(ts.name).expect("toolset resolves") {
            if !directory.contains(&format!("`{}`", def.name)) {
                missing.push(format!("{} (in {})", def.name, ts.name));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "tool-directory.md does not document {} registered tool(s):\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

/// The meta-tool count is quoted in prose too, and it moves far less often —
/// which is exactly why a change to it is easy to forget. PR #176 proposes
/// taking it from 6 to 7.
#[test]
fn docs_quote_the_meta_tool_count() {
    let (_, _, meta) = counts();
    let dev = read("DEV.md");
    assert!(
        dev.contains(&format!("{meta} meta-tools")),
        "DEV.md must state \"{meta} meta-tools\" — the registry defines {meta}"
    );
}
