# PCB Schematic Identity Rebind Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a revision-gated live KiCad tool that safely rebinds PCB footprint schematic identities without changing any other board state, then use it to unblock the guarded LH60 TP-to-header synchronization.

**Architecture:** Extend the existing `sch_export` / `pcb_sync` deep module rather than weakening routine sync conflict handling. The new planner consumes the same saved netlist and live board snapshot as normal sync, accepts an explicit reference allowlist, proves every non-identity field equivalent, and applies cloned `FootprintInstance` protobufs with only `symbol_path` changed in one KiCad undo commit. LH60 adds a consumer-side orchestration stage before its existing sync transaction and retains the single-save/discard-on-failure boundary.

**Tech Stack:** Rust 1.96, serde/serde_json, sha2, prost, KiCad 10 NNG/protobuf IPC, Konnect MCP stdio, Python 3 unittest/FakeClient, KiCad 10 AppImage under Xvfb.

## Global Constraints

- Existing `update_pcb_from_schematic` identity-conflict behavior remains unchanged.
- The new tool is named `rebind_pcb_schematic_identities` and belongs to `sch_export`.
- It modifies only `FootprintInstance.symbol_path`; it never adds, deletes, moves, rotates, flips, reroutes, renumbers, changes values, changes DNP, changes footprint IDs, changes pad nets, or saves.
- Input requires exact nonempty `schematic`, `board`, and unique nonempty `references`; apply also requires the exact latest `expected_plan_revision`.
- A requested reference is eligible only when exact reference, value, footprint ID, DNP, pad-number set, and normalized pad-net mapping match while nonempty old/new symbol paths differ.
- A mixed request is atomic and fail-closed: one conflict clears every planned change.
- Apply is one KiCad undo commit and readback must prove every protobuf field except `symbol_path` unchanged.
- The saved schematic must be closed and the exact target PCB must be open over live IPC.
- The tool never writes a `.kicad_*` file directly and never falls back to file mutation.
- LH60 rebind targets exactly `U1`, `D1..D70`, `SW1..SW58`, and `SW60..SW76` (146 references).
- LH60 continues to require exact inventories `169 -> 146 -> 152`, zero zones, zero segment/via/arc count, and actual slash-prefixed board-net trace checks.
- LH60 may save the PCB exactly once, only after live rebind/sync gates; post-save failure never triggers a second save.
- Any failed live mutation causes the whole isolated LH60 worktree/session to be discarded and recreated from its latest pushed commit; no restore, stash, or reset is permitted.
- Every commit and merge commit ends exactly once with `Co-authored-by: TRAE CLI <noreply@bytedance.com>` and is pushed immediately.
- Root repositories remain clean main/master mirrors; all work stays under the requirement-scoped worktrees.

## Workspaces and Integration Branches

- Konnect implementation worktree: `/data00/home/wangqiyilang/playground/.worktree/debug-connectors-identity-rebind/konnect`
- Konnect branch: `feat/pcb-schematic-identity-rebind`
- Konnect base/spec commit: `2ec9f30e8e07dd9a4c1ffd9cceb7f7be236f22a6`
- LH60 consumer worktree: `/data00/home/wangqiyilang/playground/lh60/.worktree/debug-connectors-pcb-sync/lh60`
- LH60 branch: `task/debug-connectors-pcb-sync`
- LH60 current safe commit: `940412322b4b7615384441f03c1e48e5812f37f2`
- LH60 integration worktree: `/data00/home/wangqiyilang/playground/lh60/.worktree/debug-connectors-layout/lh60`
- LH60 integration branch: `task/debug-connectors-layout`

## Units, Dependency Graph, and Interface Freeze

| Unit | Deliverable | Dependency | Edge type |
|---|---|---|---|
| R0 | Pure rebind planner and exact conflict model | Spec | true blocking |
| R1 | Live handler, revision gate, atomic apply/readback | R0 | true blocking |
| R2 | Public schema, stdio/protocol docs and tool totals | R1 interface | shared interface only |
| R3 | Disposable live KiCad and LH60 acceptance | R1 + R2 | true blocking |
| R4 | LH60 FakeClient orchestration and deployed capability gate | R3 deployed schema | true blocking |
| R5 | Fresh L5B baseline and single live L5C transaction | R4 | true blocking |
| R6 | Integrate L5 commits into LH60 integration branch | R5 accepted commits | true blocking |

The shared public interface is frozen by the approved spec: tool name, input schema, response fields, change object fields, status vocabulary, revision semantics, and undo text requirements cannot drift between R0-R4. R0-R2 touch the same Konnect module family and are therefore serial. R4 is in a different repository but cannot start until the deployed R3 schema is final. Production PCB writes in R5 are strictly serial.

## File Responsibility Map

### Konnect

- `crates/konnect-core/src/tools/pcb_sync.rs`: shared saved-netlist/live-board model, pure rebind planner, revision hashing, handler, protobuf mutation and readback proof.
- `crates/konnect-core/src/tools/sch_export.rs`: public MCP schema and handler registration only.
- `crates/konnect-core/src/router/registry.rs`: `sch_export` tool count.
- `crates/konnect/tests/protocol_stdio.rs`: public schema discovery and invalid-argument protocol behavior.
- `crates/konnect/tests/live_kicad_tools.rs`: ignored disposable live apply/noop convergence test.
- `crates/konnect/tests/asset_references.rs`: existing documentation/tool-name guard; run it unchanged because the new snake-case name is a registered tool and every public parameter comes from its schema.
- `crates/konnect/tests/doc_tool_counts.rs`: existing count gates; no new test logic expected.
- `crates/konnect/assets/skills/kicad-pcb/SKILL.md`: exceptional rebind workflow and warning.
- `tool-directory.md`: add the tool row, change `sch_export` from 7 to 8, and change catalogue totals from 204/210 to 205/211.
- `README.md`: change both registered-tool claims from 204 to 205.
- `DEV.md`: change registered/total claims from 204/210 to 205/211 while retaining 6 meta-tools and 19 toolsets.
- `packaging/metadata.json`, `plugin/plugin.json`: change the description total from 204 to 205.
- `docs/TROUBLESHOOTING.md`: change the eager catalogue total from 210 to 211.

### LH60

- `tools/sync_debug_connectors.py`: deployed schema gate, exact 146-reference rebind review/apply/noop stage, evidence persistence and save boundary.
- `tools/verify_pcb_sync.py`: complete FakeClient shapes and refusal/order tests.
- `docs/reports/2026-08-18-debug-connectors-baseline.json`: R4 changes consumer code, so R5 always regenerates this report at the new code SHA and commits it alone before mutation.
- `lh60.kicad_pcb`: changed only by live KiCad/Konnect in the final transaction.
- Integration branch receives reviewed commits one by one; no direct production edit occurs there.

---

### Task R0: Pure Identity-Rebind Planner

**Files:**
- Modify: `crates/konnect-core/src/tools/pcb_sync.rs`

**Interfaces:**
- Consumes: existing `ExportedDesign`, `DesignComponent`, `BoardState`, `BoardFootprint`, `PreservedBoardState`, `SyncDiagnostic`, `netlist_identity`.
- Produces: `fn plan_identity_rebind(netlist_source: &str, design: &ExportedDesign, board: &BoardState, requested: &[String]) -> IdentityRebindPlan`; serializable `IdentityRebindChange`, `IdentityRebindCounts`, and stable `plan_revision`.

- [ ] **Step 1: Freeze the success response model in a failing pure test**

Add a `#[test] fn identity_rebind_plans_only_exact_equivalent_references()` beside the existing `plan_sync` tests. Use three literal components: two requested with different old/new paths and one unrequested. Assert:

```rust
let plan = plan_identity_rebind(
    "(export (components) (nets))",
    &design,
    &board,
    &["D1".into(), "SW1".into()],
);
assert_eq!(plan.status, PlanStatus::Ready);
assert_eq!(plan.counts.requested, 2);
assert_eq!(plan.counts.eligible, 2);
assert_eq!(plan.counts.planned, 2);
assert_eq!(plan.counts.conflicts, 0);
assert_eq!(
    plan.changes.iter().map(|change| change.reference.as_str()).collect::<Vec<_>>(),
    vec!["D1", "SW1"],
);
assert_eq!(plan.changes[0].old_symbol_path, "/old/d1");
assert_eq!(plan.changes[0].new_symbol_path, "/new/d1");
assert_eq!(plan.changes[0].preserve.layer, "F.Cu");
```

- [ ] **Step 2: Run the focused test and observe RED**

Run:

```bash
cargo test -p konnect-core identity_rebind_plans_only_exact_equivalent_references -- --nocapture
```

Expected: compile failure because `plan_identity_rebind` / rebind types do not exist.

- [ ] **Step 3: Implement the minimal planner types and deterministic success path**

Add focused private types in `pcb_sync.rs`:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct IdentityRebindCounts {
    requested: usize,
    eligible: usize,
    planned: usize,
    applied: usize,
    conflicts: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct IdentityRebindChange {
    reference: String,
    kiid: String,
    old_symbol_path: String,
    new_symbol_path: String,
    value: String,
    footprint_id: String,
    dnp: bool,
    pad_nets: BTreeMap<String, String>,
    preserve: PreservedBoardState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct IdentityRebindPlan {
    status: PlanStatus,
    plan_revision: String,
    counts: IdentityRebindCounts,
    changes: Vec<IdentityRebindChange>,
    diagnostics: Vec<SyncDiagnostic>,
}
```

Implement sorted requested-reference traversal, exact reference lookup, root-net normalization (`/VSYS` equals `VSYS`; nested paths remain distinct), exact non-identity comparisons, and deterministic change sorting. The normal sync planner must remain byte-for-byte behaviorally unchanged.

- [ ] **Step 4: Run success test GREEN**

Run the focused command from Step 2. Expected: one test PASS.

- [ ] **Step 5: Add table-driven RED tests for every conflict family**

Add literal cases, each asserting `status=Conflict`, `changes.is_empty()`, `planned=0`, and the listed diagnostic code:

| Case | Exact fixture mutation from the passing D1/SW1 fixture | Diagnostic code |
|---|---|---|
| Missing schematic reference | Remove `D1` from `design.components`; request `D1` | `requested_reference_missing_from_schematic` |
| Missing board reference | Remove the `D1` `BoardFootprint`; request `D1` | `requested_reference_missing_from_board` |
| Duplicate request | Pass `vec!["D1", "D1"]` | `duplicate_requested_reference` |
| Missing old identity | Set board `D1.symbol_path = None` | `missing_board_identity` |
| Missing new identity | Set schematic `D1.symbol_path = String::new()` | `missing_schematic_identity` |
| Mixed already-matching identity | Request `D1,SW1`; set board `D1.symbol_path` equal to schematic D1 while SW1 still differs | `identity_already_matches_in_mixed_request` |
| Value drift | Set board `D1.value = "WRONG"` | `value_mismatch` |
| Footprint drift | Set board `D1.footprint_id = "wrong:Footprint"` | `footprint_mismatch` |
| DNP drift | Toggle board `D1.dnp` | `dnp_mismatch` |
| Pad-set drift | Remove pad `"2"` from board D1 `pad_nets` | `pad_set_mismatch` |
| Pad-net drift | Set board D1 pad `"2"` to `ROW1` while schematic stays `ROW0` | `pad_net_mismatch` |
| Nested-net mismatch | Set board D1 pad `"2"` to `/sheet/ROW0` while schematic stays `ROW0` | `pad_net_mismatch` |
| Board-only footprint | Set board `D1.not_in_schematic = true` | `board_only_footprint` |
| Duplicate old identity | Set board SW1 old path equal to board D1 old path | `duplicate_board_identity` |
| Duplicate new identity | Set schematic SW1 new path equal to schematic D1 new path | `duplicate_schematic_identity` |
| Target identity collides with unrequested board item | Add unrequested board R1 whose old path equals schematic D1 new path | `target_identity_in_use` |
| Invalid old KIID | Set board D1 old path to `/not-a-uuid` | `invalid_board_identity` |
| Invalid new KIID | Set schematic D1 new path to `/not-a-uuid` | `invalid_schematic_identity` |

Also add a separate all-matching request asserting `status=Noop`, `planned=0`, empty changes and diagnostics.

- [ ] **Step 6: Run conflict tests RED, then implement fail-closed diagnostics**

Run:

```bash
cargo test -p konnect-core identity_rebind_ -- --nocapture
```

Expected first: failures for unimplemented diagnostics. Implement one validation branch per frozen code; rerun until all focused tests pass.

- [ ] **Step 7: Lock revision stability and drift**

Add tests requiring: same logical netlist with changed export header date/tool yields the same revision; requested-reference order yields the same revision; any requested old/new path, value, footprint, DNP, pad map, board KIID, position/layer/locked, or requested-set change yields a different revision. Hash only the requested snapshot plus netlist structural identity and serialized changes.

- [ ] **Step 8: Run planner and existing sync tests**

```bash
cargo test -p konnect-core identity_rebind_ -- --nocapture
cargo test -p konnect-core pcb_sync -- --nocapture
```

Expected: all new planner tests and all existing normal-sync tests pass.

- [ ] **Step 9: Commit and push R0**

```bash
git add crates/konnect-core/src/tools/pcb_sync.rs
hooks_dir=$(mktemp -d /tmp/konnect-empty-hooks.XXXXXX)
git -c core.hooksPath="$hooks_dir" commit \
  -m "feat(sync): plan explicit schematic identity rebinds" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
rmdir "$hooks_dir"
git push origin feat/pcb-schematic-identity-rebind
```

---

### Task R1: Revision-Gated Live IPC Apply and Readback

**Files:**
- Modify: `crates/konnect-core/src/tools/pcb_sync.rs`

**Interfaces:**
- Consumes: `plan_identity_rebind(netlist_source: &str, design: &ExportedDesign, board: &BoardState, requested: &[String]) -> IdentityRebindPlan` from R0 and existing `LiveSnapshot` / `attempt_ipc_write`.
- Produces: `pub(crate) async fn handle_rebind_pcb_schematic_identities(args: &serde_json::Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult>` and the frozen JSON response.

- [ ] **Step 1: Write RED handler tests for dry-run and apply arguments**

Add async/unit seams around the planner/IPC closure so tests assert:

- missing/empty/duplicate `references` returns structured `invalid_argument`;
- `dry_run=false` without revision returns structured `invalid_argument`;
- missing schematic/board returns `file_not_found`;
- IPC unreachable returns a conflict response and never file-edits;
- dry run returns `ready`, `applied=0`, `undo=null`;
- stale revision returns conflict, clears changes, and sends no update.

Run:

```bash
cargo test -p konnect-core rebind_handler_ -- --nocapture
```

Expected: RED because the handler does not exist.

- [ ] **Step 2: Implement saved-hierarchy/netlist/live-snapshot orchestration**

Reuse `saved_hierarchy_files`, `cli::export_netlist`, `parse_exported_netlist`, `apply_saved_symbol_flags`, `snapshot_board`, and `attempt_ipc_write`. Do not duplicate parsing or file-fallback code. Return response coverage exactly as specified:

```rust
json!({
  "status": status,
  "plan_revision": plan.plan_revision,
  "coverage": {
    "source": "saved_schematic_hierarchy",
    "hierarchy_files": hierarchy_files,
    "transport": "live_kicad_ipc",
    "atomicity": "single_kicad_undo_commit",
    "requested": plan.counts.requested,
    "eligible": plan.counts.eligible,
    "planned": plan.counts.planned,
    "applied": plan.counts.applied,
    "conflicts": plan.counts.conflicts
  },
  "changes": plan.changes,
  "diagnostics": plan.diagnostics,
  "undo": undo
})
```

- [ ] **Step 3: Write RED mutation test proving only symbol_path changes**

Create a literal `FootprintInstance` with reference/value fields, attributes, position, layer, lock state, pads, graphics, models and old path. Call a new pure helper:

```rust
fn rebind_footprint_item(
    item: &prost_types::Any,
    change: &IdentityRebindChange,
) -> Result<prost_types::Any>
```

Decode before/after, clear only `symbol_path` in both, and assert the remaining protobufs are equal. Assert the outgoing path equals the new KIID sequence.

- [ ] **Step 4: Implement clone-and-rebind mutation**

Decode the exact live `FootprintInstance`, validate KIID/reference/current old path, clone it, replace only `symbol_path`, and pack it. Do not call `apply_footprint_fields`; that helper intentionally changes fields/pads and is too broad.

- [ ] **Step 5: Add RED readback/corruption regressions**

Extract a canonical comparison helper that returns a detailed error when any non-identity field differs. Tests must independently mutate position, rotation, layer, locked, reference, value, DNP, footprint definition, pad UUID/number/net/geometry, silk/courtyard graphic, field placement, model and item count. Every mutation must fail; changing only `symbol_path` must pass.

- [ ] **Step 6: Implement one atomic commit and strict readback**

Build every update first, then:

```rust
client.run_commit("Rebind PCB schematic identities", |client| {
    client.update_items_in(snapshot.document.clone(), updates)?;
    Ok(())
})?;
```

Reread by KIID, compare canonical protobufs ignoring only `symbol_path`, ensure new paths are globally unique, and fail the tool call on any difference. Do not call `save_board`.

- [ ] **Step 7: Verify apply convergence and no normal-sync regression**

```bash
cargo test -p konnect-core identity_rebind_ -- --nocapture
cargo test -p konnect-core rebind_ -- --nocapture
cargo test -p konnect-core pcb_sync -- --nocapture
```

Expected: ready -> applied -> noop behavior and unchanged existing sync suite.

- [ ] **Step 8: Commit and push R1**

```bash
git add crates/konnect-core/src/tools/pcb_sync.rs
hooks_dir=$(mktemp -d /tmp/konnect-empty-hooks.XXXXXX)
git -c core.hooksPath="$hooks_dir" commit \
  -m "feat(sync): apply identity rebinds atomically" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
rmdir "$hooks_dir"
git push origin feat/pcb-schematic-identity-rebind
```

---

### Task R2: Public Schema, Protocol Contract, and Documentation

**Files:**
- Modify: `crates/konnect-core/src/tools/sch_export.rs`
- Modify: `crates/konnect-core/src/router/registry.rs`
- Modify: `crates/konnect/tests/protocol_stdio.rs`
- Modify: `crates/konnect/assets/skills/kicad-pcb/SKILL.md`
- Modify: `tool-directory.md`
- Modify: `README.md`
- Modify: `DEV.md`
- Modify: `packaging/metadata.json`
- Modify: `plugin/plugin.json`
- Modify: `docs/TROUBLESHOOTING.md`
- Test unchanged: `crates/konnect/tests/asset_references.rs`

**Interfaces:**
- Consumes: R1 handler and frozen input/response schema.
- Produces: discoverable tool, `sch_export` count 8, stdio schema contract, published exceptional workflow.

- [ ] **Step 1: Add failing schema/discovery tests**

In `protocol_stdio.rs`, load `sch_export`, fetch `tools/list`, and assert the new tool schema has exactly required `schematic`, `board`, `references`; properties include `dry_run` and `expected_plan_revision`; `references` has `minItems=1` and `uniqueItems=true`. Call apply without revision and assert `invalid_argument.expected_plan_revision`.

- [ ] **Step 2: Run protocol/count tests RED**

```bash
cargo test -p konnect --test protocol_stdio rebind -- --nocapture
cargo test -p konnect-core registry_tool_counts_match_reality -- --nocapture
```

Expected: tool absent and count mismatch once schema is added without the registry bump.

- [ ] **Step 3: Register the exact schema and handler**

Add the `tool!` block after `update_pcb_from_schematic` in `sch_export.rs`, delegating only to `handle_rebind_pcb_schematic_identities`. Bump `sch_export.tool_count` from 7 to 8.

- [ ] **Step 4: Run protocol tests GREEN**

Run Step 2 commands. Expected: PASS.

- [ ] **Step 5: Document the exceptional workflow and update totals**

Add the tool row to `tool-directory.md`, change the heading to `sch_export · 8 tools`, and insert this warning before normal sync in the PCB skill:

```text
Use rebind_pcb_schematic_identities only when reviewed schematic recreation
changed symbol UUIDs while reference/value/footprint/DNP/pad nets remain exact.
Always dry-run and apply its exact revision before normal PCB sync. It is not a
general conflict override and it never saves the board.
```

Update exact totals: 204 registered -> 205, 210 total -> 211, while retaining 19 toolsets and 6 meta-tools. The required files are `tool-directory.md`, `README.md`, `DEV.md`, `packaging/metadata.json`, `plugin/plugin.json`, and `docs/TROUBLESHOOTING.md`. Do not modify `NOT_TOOLS`: the new name is registered and its parameter names are schema-derived.

- [ ] **Step 6: Run documentation and asset gates**

```bash
cargo test -p konnect --test doc_tool_counts -- --nocapture
cargo test -p konnect --test asset_references -- --nocapture
cargo test -p konnect-core registry_tool_counts_match_reality -- --nocapture
```

- [ ] **Step 7: Run formatting/lints and focused suites**

```bash
cargo fmt --all -- --check
cargo clippy -p konnect-core -p konnect --all-targets -- -D warnings
cargo test -p konnect-core identity_rebind_ -- --nocapture
cargo test -p konnect --test protocol_stdio rebind -- --nocapture
```

- [ ] **Step 8: Commit and push R2**

Stage only schema/registry/protocol/docs/count files, verify `git diff --cached --check`, commit `feat(sync): expose schematic identity rebind`, append the exact trailer once, and push immediately.

---

### Task R3: Disposable Live KiCad and LH60 Acceptance

**Files:**
- Modify: `crates/konnect/tests/live_kicad_tools.rs`
- Create ignored evidence under the plan SDD workspace; do not commit generated boards/logs.

**Interfaces:**
- Consumes: deployed R2 tool/schema.
- Produces: accepted feature binary SHA and evidence that only identity changes live.

- [ ] **Step 1: Add an ignored live convergence test**

Add `schematic_identity_rebind_apply_then_dry_run_is_noop`. It reads:

```text
KONNECT_LIVE_KICAD_BOARD
KONNECT_LIVE_KICAD_SCHEMATIC
KONNECT_LIVE_KICAD_REBIND_REFERENCES (comma-separated)
KICAD_API_SOCKET
```

It snapshots all returned footprint protobufs, dry-runs, applies exact revision, requires `applied`, dry-runs to `noop`, and asserts canonical equality ignoring only symbol paths. It never saves.

- [ ] **Step 2: Build the feature binary**

```bash
cargo build --release -p konnect
sha256sum target/release/konnect
```

- [ ] **Step 3: Run the complete non-live verification**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

For every filtered command in this plan, read the output and require
`running N tests` with `N > 0`; a zero-test success is a failed verification.

- [ ] **Step 4: Run a disposable live fixture**

Create a temporary project through Konnect/KiCad-aware tools, not by text-editing protected sources. Save an initial matching board, close it, recreate its schematic symbols through Konnect component APIs to change identities while preserving exact contracts, reopen only the disposable PCB under task-owned Xvfb, and run the ignored live test. Close without saving and discard the fixture.

- [ ] **Step 5: Run a disposable LH60 acceptance copy**

Create a disposable requirement-local worktree from LH60 `9404123`, open its PCB, and call rebind dry-run for the exact 146 references. Require:

```text
status=ready; requested=eligible=planned=146; conflicts=0; 146 exact changes
```

Apply exact revision, require `applied=146`, then rebind dry-run `noop`. Immediately call normal sync dry-run and require exactly six J additions, `board_only_preserved.planned=23`, `skipped_by_flag.planned=3`, no update/pad/conflict diagnostics. Close without saving and discard the entire disposable worktree.

- [ ] **Step 6: Independent review and fix loop**

Generate a review package from `2ec9f30..HEAD`. Require Spec PASS, Quality PASS, and no Critical/Important. Fix findings with focused tests and scoped re-review before deployment.

- [ ] **Step 7: Commit live test, push, and record accepted SHA**

Commit only the ignored-test source with subject `test(sync): verify live identity rebind convergence`, exact trailer once, push, and record final source commit plus binary SHA in the plan ledger.

- [ ] **Step 8: Deploy the accepted feature build locally**

Verify no stale Konnect process is serving a deleted executable with `readlink /proc/<pid>/exe`. Atomically repoint `/data00/home/wangqiyilang/.local/bin/konnect` to the accepted release binary, then spawn a fresh process and verify `konnect --version`, tool schema ownership, and tool count. Record the previous symlink target for rollback.

---

### Task R4: LH60 Consumer Rebind Orchestration

**Files:**
- Modify: `/data00/home/wangqiyilang/playground/lh60/.worktree/debug-connectors-pcb-sync/lh60/tools/sync_debug_connectors.py`
- Modify: `/data00/home/wangqiyilang/playground/lh60/.worktree/debug-connectors-pcb-sync/lh60/tools/verify_pcb_sync.py`

**Interfaces:**
- Consumes: deployed `rebind_pcb_schematic_identities` schema/response.
- Produces: guarded `sync_debug_connectors` ordering `preflight -> rebind dry/apply/noop -> normal sync -> delete/apply -> one save`.

- [ ] **Step 1: Extend the capability gate RED**

Require `sch_export.rebind_pcb_schematic_identities` with exact required inputs `schematic,board,references` and supported `dry_run,expected_plan_revision`. Add missing-tool, unexpected-required, and wrong-toolset ownership cases. Run focused test and observe failure.

- [ ] **Step 2: Add full FakeClient response fixtures**

Mirror every deployed response field. Freeze `REBIND_REFS` as exact sorted 146 shared references and assert it equals `SHARED_REFS`.

- [ ] **Step 3: Add RED success-order test**

Assert calls occur exactly:

```text
all pre-delete baseline/hash/inventory/pad/trace gates
rebind dry_run=true
rebind dry_run=false expected_plan_revision=<rebind revision>
rebind dry_run=true -> noop
normal sync dry_run=true -> six adds + board-only 23
delete TP1..TP23
normal sync dry_run=true -> board-only 0
normal sync apply exact second revision
pre-save 152/live pad/trace/noop gates
save_project exactly once
post-save hash/inventory/pad/zone/track/DRC/noop gates
```

- [ ] **Step 4: Add every rebind refusal test**

Before first delete and before save, reject wrong status, empty revision, diagnostics, count mismatch, unexpected reference/change fields, old/new path equality, nonempty/invalid undo, apply revision mismatch, final rebind non-noop, malformed JSON, or a rebind call changing any baseline inventory/pad/trace evidence.

- [ ] **Step 5: Implement minimal orchestration**

Add `def _validate_rebind_plan(payload: dict[str, Any], *, expected_status: str, expected_planned: int, expected_applied: int, require_undo: bool) -> dict[str, Any]` and invoke it before normal sync. Do not add a CLI mode or separate save. Evidence returns all three rebind responses.

- [ ] **Step 6: Run fresh LH60 gates**

```bash
PYTHONDONTWRITEBYTECODE=1 python -m unittest -v tools.verify_pcb_sync tools.verify_pcb_placement
PYTHONDONTWRITEBYTECODE=1 python -m compileall -q tools
git diff --check
sha256sum lh60.kicad_sch lh60.kicad_pcb
```

Expected protected hashes before live production: schematic `7ae8a38afc453579f8f24de23e57772eff73056d12acd4fd9fcc6f0bf57533f9`, PCB `0a5722685ee378e9c9b240aa01a1f151f382cab83216edfa14a0663a1ac80664`.

- [ ] **Step 7: Independent review, commit, and push**

Require no Critical/Important, then commit the two tool/test files as `fix(pcb): rebind recreated schematic identities`, exact trailer once, and push `task/debug-connectors-pcb-sync`.

---

### Task R5: Fresh Baseline and Single Production L5C Transaction

**Files:**
- Regenerate and commit: `docs/reports/2026-08-18-debug-connectors-baseline.json` after the R4 code commit; the report commit must contain no other path.
- Modify through live Konnect/KiCad only: `lh60.kicad_pcb`

**Interfaces:**
- Consumes: pushed R4 consumer code and accepted deployed R3 binary.
- Produces: saved board with no TP, exact J1..J6, no routing objects, complete evidence.

- [ ] **Step 1: Recreate the L5 consumer worktree if any prior live mutation occurred**

The current worktree never passed first dry-run and has unchanged hash, so it may be reused only after verifying clean status, exact `0a5722685ee378e9c9b240aa01a1f151f382cab83216edfa14a0663a1ac80664` PCB hash, and no KiCad process. If any mutation is discovered, remove the whole worktree and recreate it from the latest pushed branch commit.

- [ ] **Step 2: Start one task-owned Xvfb/KiCad IPC session**

Use an isolated `XDG_CONFIG_HOME` with API enabled, the exact L5 board path, and `KICAD_API_SOCKET=ipc:///home/wangqiyilang/.cache/tmp/kicad/api.sock`. Require live inventory 169/23/0 before any mutation.

- [ ] **Step 3: Regenerate the baseline read-only at the R4 code SHA**

Run:

```bash
KICAD_API_SOCKET=ipc:///home/wangqiyilang/.cache/tmp/kicad/api.sock \
PYTHONDONTWRITEBYTECODE=1 python -m tools.sync_debug_connectors \
  --capture-baseline \
  --report docs/reports/2026-08-18-debug-connectors-baseline.json
```

Validate 169 refs, 23 logical/board-net TP pads, zero zones, manufacturing track count zero, 23 actual board-net traces empty and complete untruncated DRC. Commit only the report and push.

- [ ] **Step 4: Execute the guarded transaction once**

Run:

```bash
KICAD_API_SOCKET=ipc:///home/wangqiyilang/.cache/tmp/kicad/api.sock \
PYTHONDONTWRITEBYTECODE=1 python -m tools.sync_debug_connectors \
  --apply \
  --baseline docs/reports/2026-08-18-debug-connectors-baseline.json \
  --report /tmp/lh60-debug-pcb-sync.json
```

Do not manually interleave tools. The script must complete rebind, normal sync, exact TP deletion, J apply, live gates, one save and post-save gates.

- [ ] **Step 5: On any failure, enforce recovery**

Terminate KiCad without another save, terminate Xvfb, retain only external logs/evidence, delete the entire L5 worktree, recreate from the latest pushed branch, and restart at Step 2. Never restore, stash, reset, or patch a dirty protected board.

- [ ] **Step 6: Verify accepted evidence**

Require exact 152 refs, no TP, J1..J6 exact values/footprints/pad logical+board nets, staged F.Cu/outside board, zero zones/track objects, 23 actual board-net traces empty, rebind noop, normal sync noop, complete DRC, changed PCB hash, unchanged schematic hash, and one save call.

- [ ] **Step 7: Independent live-evidence/PCB review**

Reviewer receives the before baseline, apply evidence, board-only diff, post-save query summary and DRC delta. Require Spec PASS, Quality PASS, no Critical/Important.

- [ ] **Step 8: Commit and push the PCB only**

```bash
git add lh60.kicad_pcb
hooks_dir=$(mktemp -d /tmp/lh60-empty-hooks.XXXXXX)
git -c core.hooksPath="$hooks_dir" commit \
  -m "refactor(pcb): replace test pads with debug headers" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
rmdir "$hooks_dir"
git push origin task/debug-connectors-pcb-sync
```

---

### Task R6: Integrate L5 into the LH60 Integration Branch

**Files:**
- No new source files; cherry-pick reviewed R4/R5 commits and any refreshed baseline commit.

**Interfaces:**
- Consumes: pushed L5 consumer commit sequence.
- Produces: `task/debug-connectors-layout` containing accepted L5 code, evidence and PCB.

- [ ] **Step 1: Record the exact L5 commit sequence**

List the complete source sequence after integration commit `d7dceda`, starting with the already pushed `bf1f1a1`, `4e1de23`, and `9404123`, then append the R4 code commit, regenerated R5 baseline commit, and R5 PCB commit. Classify every code/baseline/PCB commit, require exactly one trailer, require a clean remote counterpart, and ensure no required commit is omitted.

- [ ] **Step 2: Cherry-pick one commit at a time**

For each non-merge commit, create a temporary empty hooks directory and run `git -c core.hooksPath="$hooks_dir" cherry-pick <exact-sha>`, then remove the directory. The original message already contains exactly one trailer; verify the new commit still contains exactly one. After each cherry-pick, run its focused tests and push integration immediately.

- [ ] **Step 3: Run integrated fresh verification**

```bash
PYTHONDONTWRITEBYTECODE=1 python -m unittest -v tools.verify_pcb_sync tools.verify_pcb_placement
PYTHONDONTWRITEBYTECODE=1 python -m compileall -q tools
PYTHONDONTWRITEBYTECODE=1 python tools/check_schematic_acceptance.py --production
git diff --check
```

Open the integration PCB in a fresh read-only IPC session and require accepted 152/J/no-TP/zero-routing state and final normal-sync noop.

- [ ] **Step 4: Update the SDD ledger and hand off to L6**

Record Konnect source SHA/binary SHA, every L5 commit, baseline/apply evidence hashes, PCB hash, DRC totals/delta, review verdicts and the fact that J1..J6 remain unsited on F.Cu outside the board. Mark L5 complete and L6 in progress.
