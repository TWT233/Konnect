# Roadmap

Where Konnect is going, in the order the work actually depends on itself.
No dates — items ship when they survive verification. Opening an issue is the
best way to influence priority; several of the largest items below exist
because a contributor measured something we hadn't.

This revision (post-v0.7.0) is built in large part on
[@neusse's improvement backlog](https://github.com/mixelpixx/Konnect/discussions/165)
and his benchmark work (discussions
[#239](https://github.com/mixelpixx/Konnect/discussions/239),
[#295](https://github.com/mixelpixx/Konnect/discussions/295),
[#224](https://github.com/mixelpixx/Konnect/discussions/224)) — hands-on,
evidence-first evaluations that found what CI could not. If you want to work
on any item here, say so on its issue; design agreement first, then one
focused PR.

## 1. Truth and write-safety (the standing doctrine)

Every response field derives from the **result**, never from the request. A
verdict without evidence is `INCOMPLETE`, never approval. Fixtures come from
real KiCAD output. These rules are enforced by tests
(`doc_tool_counts.rs`, `schema_parameter_usage.rs`, `schema_migrations.rs`)
and documented in [docs/DEVELOPING_TOOLS.md](docs/DEVELOPING_TOOLS.md); every
new tool must satisfy them.

Open work in this theme:

- **Stale-board detection** ([#240](https://github.com/mixelpixx/Konnect/issues/240)) —
  after KiCAD dies, the next closed-board call cannot tell that from "KiCAD
  was never running." Lock files become supporting evidence, not sole
  authority. Depends on a document-answering IPC mock
  ([#241](https://github.com/mixelpixx/Konnect/issues/241)) so refusal
  branches finally get shared test coverage.
- **Project and library-table targeting**
  ([#189](https://github.com/mixelpixx/Konnect/issues/189)) — ambiguity in
  the ancestor walk fails with a structured error instead of guessing an
  unrelated project.
- **Server lifecycle ownership**
  ([#103](https://github.com/mixelpixx/Konnect/issues/103)) and a
  non-mutating startup
  ([#242](https://github.com/mixelpixx/Konnect/issues/242)) — a server
  process must not reinstall configuration as a side effect of launching.

## 2. Schematic correctness

- **Multi-unit symbols** ([#182](https://github.com/mixelpixx/Konnect/issues/182)) —
  the read half now flows through the shared `ConnectivityIndex`
  ([#323](https://github.com/mixelpixx/Konnect/pull/323)); the mutation half
  ([#273](https://github.com/mixelpixx/Konnect/pull/273)) lands next after a
  rebase onto the index.
- **Connectivity on move and delete**
  ([#120](https://github.com/mixelpixx/Konnect/issues/120)) — junction
  creation/pruning when things move. This is also the prerequisite for a real
  `move_connected`
  ([#315](https://github.com/mixelpixx/Konnect/issues/315)), which currently
  refuses rather than pretending.
- **Remaining indentation-sensitive scans**
  ([#84](https://github.com/mixelpixx/Konnect/issues/84)) — replace the last
  literal-string probes with structural parsing.

## 3. Autorouting: a real Freerouting bridge

The single biggest capability gap. KiCad 10's CLI never had Specctra
DSN/SES, so Konnect's old `autoroute` could only fail; it was removed rather
than faked ([#253](https://github.com/mixelpixx/Konnect/issues/253)).

The agreed direction is @neusse's design from #253: **drive a standalone
Freerouting JAR directly** — DSN export, route, SES import, editor refresh —
with discovery reporting *engine found* and *bridge available* as separate
facts, and PCM-installed JARs discovered on every platform. Board identity
validation and atomic SES import are requirements, not options. Design is
agreed; implementation is open for whoever claims it on #253.

## 4. Client compatibility

The dynamic-toolset architecture (small starter kit, `load_toolset` on
demand) assumes MCP clients either expose many tools or re-fetch after
`tools/list_changed`. Two reports show real clients violating both
assumptions: VS Code Copilot caps callable tools
([#325](https://github.com/mixelpixx/Konnect/issues/325)) and a Linux client
never re-fetched ([#233](https://github.com/mixelpixx/Konnect/issues/233)).

Planned: a **compact tool surface mode** (a few generic call/help tools
proxying the full catalog — credit @simachines' working prototype in #325),
MCP `resources/` exposure of the tool directory, and documenting
`eager_toolsets` per client. The 200-tool catalog is a feature; falling over
on real clients is not.

## 5. Workflow and skills layer

@neusse's benchmark-driven proposals in
[#295](https://github.com/mixelpixx/Konnect/discussions/295): an evidence
hierarchy the bundled skills teach (KiCad's own DRC/ERC outrank Konnect's
own summaries), single-owner rules for the live board, Freerouting-first PCB
flow, and a physical pin-map requirement before custom-part creation. Gated
on the tool surface stabilizing (v0.8.0); invited as skill-layer PRs.

## 6. Platform and KiCad-forward

- **KiCad 11 readiness** ([#257](https://github.com/mixelpixx/Konnect/issues/257)) —
  the SWIG `pcbnew` bindings the legacy ActionPlugin uses are **removed in
  KiCad 11**; the executable IPC plugin is the replacement path and needs its
  lifecycle/settings parity planned *before* KiCad 11 ships. The one item
  here with a deadline.
- **macOS signing/notarization**
  ([#131](https://github.com/mixelpixx/Konnect/issues/131)), then Homebrew
  ([#154](https://github.com/mixelpixx/Konnect/issues/154)) once artifact
  identity is stable.
- **Per-user Windows installs**
  ([#254](https://github.com/mixelpixx/Konnect/issues/254)) — discovery must
  find `%LOCALAPPDATA%\Programs\KiCad`.
- **KiCAD PCM publication** — submit to the official addon repository.

## 7. The quality flywheel

What keeps the rest honest:

- **End-to-end benchmarks against reality.** @neusse's konnect-codex runs
  found silent corruption, fail-open verdicts, and contract gaps that 900+
  unit tests missed. A re-run against each minor release is part of the
  release rhythm now, and the standing invitation is open.
- **Live-test honesty** ([#221](https://github.com/mixelpixx/Konnect/issues/221)) —
  fix the stale CI claims and the rotation-readback flake.
- **A real-KiCAD fixture corpus** — boards and footprints saved by KiCAD
  itself (tabs, CRLF, `ki_fp_filters`, positionless properties), so no
  fixture can share the code's wrong assumption again.
- The catalogue-wide guards stay: doc counts, schema-parameter usage, and
  schema migrations are all swept by CI on every PR.

## Done (eras, not items)

- ~~v0.1–v0.3~~ — full toolset surface across schematic, PCB, library,
  export; three-platform PCM packaging; HTTP transport; hierarchical sheets.
- ~~v0.4–v0.5~~ — atomic `update_pcb_from_schematic`; footprint graphics
  authoring and board-side editing; client-scoped installs.
- ~~v0.6–v0.7~~ — the truth-and-safety arc: required-argument enforcement,
  the layer-crash fix, sync corruption fixed with post-apply read-back,
  DRC-evidence-gated review verdicts, honest autoroute removal, and the
  developer docs set ([docs/](docs/DEVELOPER_OVERVIEW.md)).
