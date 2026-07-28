## Summary

<!-- What user-visible problem does this solve? Keep the scope to one reviewable outcome. -->

Closes #

## Approach

<!-- Explain the root cause, design, and important alternatives or trade-offs. -->

## Compatibility and safety

<!--
List public API/config/schema changes and their migration path.
For file or IPC mutations, explain target validation, atomicity/rollback, and failure behavior.
Write "No public compatibility impact" when applicable.
-->

## Validation

<!-- Paste the exact commands and results. Note environment-dependent checks not run and why. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace --locked --lib --tests` (what CI runs)
- [ ] `cargo test --workspace --locked --doc`
- [ ] `cargo clippy --workspace --locked -- -D warnings`
- [ ] `cargo fmt --all -- --check`
- [ ] Relevant viewer, plugin, packaging, and real-KiCad checks

## Review checklist

- [ ] The diff is focused and contains no generated output, personal data, or unrelated cleanup.
- [ ] New names follow `docs/NAMING_CONVENTIONS.md`; public renames include compatibility handling.
- [ ] New behavior and failure paths have regression coverage.
- [ ] File mutations are atomic and preserve unrelated content.
- [ ] IPC mutations verify the requested board and do not leave partial batches.
- [ ] If tools were added/removed: counts and docs updated per CONTRIBUTING.md (registry `tool_count`, `tool-directory.md`, DEV.md stats, README count).
