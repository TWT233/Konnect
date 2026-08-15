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

/// No file anywhere quotes a catalogue total that is not the current one.
///
/// The checks above name the four files CONTRIBUTING lists, which is why
/// `packaging/metadata.json` and `plugin/plugin.json` sat at "185 tools" while
/// the guarded documents said 200 — and those two are the ones users read, in
/// the PCM package description. `docs/TROUBLESHOOTING.md` said 189.
///
/// Sweeping instead of listing means a new document is covered the day it is
/// written rather than the day someone remembers to add it here. Only
/// three-digit counts are checked: a per-toolset count cannot reach 100 with
/// `MAX_TOOLS_PER_TOOLSET` at 20, so DEV.md's per-toolset tables are
/// unambiguously not catalogue totals and are left alone.
#[test]
fn no_file_quotes_a_stale_catalogue_total() {
    let (_, registered, meta) = counts();
    let total = registered + meta;

    let mut stale = Vec::new();
    for path in text_files(&repo_root()) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (lineno, line) in text.lines().enumerate() {
            for n in counts_in(line) {
                if n >= 100 && n != registered && n != total {
                    let rel = path
                        .strip_prefix(repo_root())
                        .unwrap_or(&path)
                        .display()
                        .to_string()
                        .replace('\\', "/");
                    stale.push(format!(
                        "{rel}:{}: says \"{n} tools\" — the registry has {registered} \
                         registered, {total} with meta-tools",
                        lineno + 1
                    ));
                }
            }
        }
    }

    assert!(
        stale.is_empty(),
        "a document quotes a tool count the registry does not support:\n  {}",
        stale.join("\n  ")
    );
}

/// Markdown and JSON under the repo, skipping build output and vendored trees.
fn text_files(root: &Path) -> Vec<PathBuf> {
    const SKIP: &[&str] = &["target", "node_modules", ".git", "dist", "build"];
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !SKIP.contains(&name.as_ref()) {
                    stack.push(path);
                }
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("md") | Some("json")
            ) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Numbers written immediately before the word "tools", ignoring any `~`.
fn counts_in(line: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (at, _) in line.match_indices("tools") {
        let before = line[..at].trim_end();
        let digits: String = before
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() {
            continue;
        }
        // Only a bare number counts: "20250610 tools" would be a version.
        if let Ok(n) = digits.chars().rev().collect::<String>().parse::<usize>() {
            out.push(n);
        }
    }
    out
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
