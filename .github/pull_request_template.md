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
- [ ] `cargo test --locked --workspace --all-targets`
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps`
- [ ] Relevant viewer, plugin, packaging, and real-KiCad checks

## Review checklist

- [ ] The diff is focused and contains no generated output, personal data, or unrelated cleanup.
- [ ] New names follow `docs/NAMING_CONVENTIONS.md`; public renames include compatibility handling.
- [ ] New behavior and failure paths have regression coverage.
- [ ] File mutations are atomic and preserve unrelated content.
- [ ] IPC mutations verify the requested board and do not leave partial batches.
- [ ] Tool counts, `tool-directory.md`, user docs, and examples are updated where applicable.
