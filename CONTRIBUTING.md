# Contributing to Konnect

Thanks for your interest! Bug reports, feature requests, and pull requests are welcome.

## Before you start

- Read [GOVERNANCE.md](GOVERNANCE.md) — who maintains what, how work is claimed,
  and what has to be true before a change lands.
- Check [ROADMAP.md](ROADMAP.md) — your idea may already be planned (or intentionally
  out of scope).
- For anything non-trivial, open an issue first so we can agree on the approach before
  you invest time.
- Keep each pull request focused on one reviewable outcome. Split unrelated platform,
  protocol, feature, and documentation changes into a short PR series.
- Read [docs/NAMING_CONVENTIONS.md](docs/NAMING_CONVENTIONS.md) before adding public
  tools, schema fields, CLI options, environment variables, or user-facing terms.

## Development setup

```bash
# protoc is required for protobuf code generation (konnect-ipc crate). The build also
# needs protobuf's well-known .proto files, which some distributions package separately.
# Windows: choco install protoc   (shim installs may also need PROTOC_INCLUDE — see DEV.md)
# macOS:   brew install protobuf
# Debian:  sudo apt install protobuf-compiler libprotobuf-dev
# Fedora:  sudo dnf install protobuf-compiler protobuf-devel

cargo check --workspace
cargo test --workspace --lib --tests
cargo build --release -p konnect
```

See [DEV.md](DEV.md) for the architecture guide, tool conventions, and how to add a
new tool.

## Pull request shape

Use an imperative title such as `fix(schematic): preserve tab-indented wire blocks`.
The description should state:

1. the user-visible problem and scope;
2. the root cause and chosen design;
3. compatibility or migration effects;
4. tests run, including intentionally skipped environment-dependent checks;
5. risk and rollback notes for file formats, IPC, packaging, or release changes.

Treat MCP tools, schema fields, CLI flags, environment variables, config keys, and
documented paths as public API. Preserve compatibility or provide an explicit
migration. Keep generated artifacts, personal settings, downloaded catalogs, build
output, and unrelated cleanup out of the diff.

## Pull request checklist

These are exactly the commands CI runs — if they pass locally, CI should be green:

- `cargo test --workspace --locked --lib --tests` passes
- `cargo test --workspace --locked --doc` passes
- `cargo clippy --workspace --locked --all-targets -- -D warnings` is clean
- `cargo fmt --all -- --check` is clean
- New names follow [the naming conventions](docs/NAMING_CONVENTIONS.md); public name
  changes include compatibility handling and migration notes
- **Read required arguments through the helpers**, never `unwrap_or`:
  `require_str`, `require_f64`, `require_array`, `require_u64` (each returns a
  structured `invalid_argument` naming the field), or `get_path` for paths.
  A handler that substitutes a default for a schema-required argument runs on a
  value the caller never supplied and reports success — that was 25 sites across
  18 tools in #218. The dispatch refuses a *missing* required argument before
  your handler runs, but only the helpers catch a wrong *type*.
- **Map layers for a KiCAD write with `try_layer_from_name`**, not
  `layer_from_name`. The infallible one returns `BL_UNDEFINED` for a name it
  does not know, and KiCAD 10.0.5 does not validate that field — it faults and
  the editor dies, taking the user's unsaved board (#237). The fallible one
  refuses at Konnect's boundary, where a refusal is still possible.
- **Fixtures for anything that parses a board or footprint should come from
  real KiCAD output.** A synthetic one passed 67 tests while `flip_component`
  refused every stock demo board, because KiCAD writes
  `(property ki_fp_filters "…")` — no position, no layer — into every footprint
  it places from a library, and the hand-written fixture had no such thing.
- If you added or removed tools: update `tool_count` in `router/registry.rs`,
  regenerate the matching section of `tool-directory.md`, and update the total tool
  counts in DEV.md's "Current Stats" and the README — those three counts have
  drifted apart before precisely because only one of them got updated

First PR from a fork? CI workflows may sit at "waiting for approval" until a
maintainer approves the run — that's a GitHub setting for first-time contributors,
not a failure on your part.

## Contributor License Agreement

Konnect is dual-licensed: AGPL-3.0 for the community, with commercial licenses
available for organizations that can't comply with the AGPL (see
[COMMERCIAL.md](COMMERCIAL.md)). To make that possible, the project must be able to
relicense contributed code.

By submitting a contribution, you agree that:

1. You have the right to submit the work under the project's licenses.
2. You grant the project maintainer a perpetual, worldwide, non-exclusive,
   royalty-free, irrevocable license to use, reproduce, modify, distribute, and
   sublicense your contribution — including under licenses other than the AGPL.
3. Your contribution remains available to the community under the AGPL-3.0.

If you can't agree to these terms, please open an issue describing the change
instead of a pull request — reimplementations from descriptions are fine.
