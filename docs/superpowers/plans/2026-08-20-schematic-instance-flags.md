# Atomic Schematic Instance Flags Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add one fail-closed Konnect transaction for placed-symbol BOM/PCB/DNP flags, use it to mark LH60's three power flags schematic-only without changing any other identity or PCB byte, then resume the 146-reference disposable PCB acceptance.

**Architecture:** Extend `sch_batch.batch_edit_schematic_components` instead of adding another tool. The schema gains optional `in_bom`, `on_board`, and `dnp` booleans; the complete mixed field/flag request is validated before one revision-checked write; existing component queries expose final state. LH60 records the policy in its generator but repairs production through one narrow batch call.

**Tech Stack:** Rust 1.96, serde/serde_json, konnect-sexp atomic writes, Python 3 unittest/FakeClient, KiCad 10 file-aware schematic APIs, and KiCad 10 PCB IPC under Xvfb for final disposable acceptance.

## Global Constraints

- Never text-edit protected KiCad sources or library tables; every mutation goes through Konnect/KiCad-aware tools.
- Keep `batch_edit_schematic_components` as the sole flag-write interface. Do not add a tool or extend the singular editor.
- Exact optional names are `in_bom`, `on_board`, and `dnp`. Omission preserves state.
- Mixed field/flag batches are all-or-nothing. Any duplicate reference, malformed value, missing target/token, or inconsistent multi-unit state leaves complete file bytes unchanged.
- Exact repeats succeed as byte-identical no-ops.
- `get_schematic_component` and `list_schematic_components` return exact boolean values for all three flags.
- Registry/tool totals stay unchanged; `sch_batch` remains 13 tools.
- LH60 `#FLG01..03` are exactly `in_bom=true, on_board=false, dnp=false`. No other component gets an explicit generator override.
- Production migration is one flag-only batch call. It never places, deletes, rewires, refreshes, syncs, opens, or saves a PCB.
- All component, wire, and label UUIDs remain unchanged.
- LH60 component hash remains `028d14843b05b9483765e68bb59fc9e5bd8e0d8b9a2e60b539314c6578c79d18`.
- LH60 pin hash remains `85f400c94abdb1e70a6da80177fbba76b774a3105d0b15081b54f318a06d7f58`.
- Production PCB stays byte-identical at `0a5722685ee378e9c9b240aa01a1f151f382cab83216edfa14a0663a1ac80664` throughout F3/F4.
- Failed live mutation kills the session and discards/recreates the entire disposable worktree; no restore, stash, or reset.
- Each logical unit is tested, reviewed, committed with exactly one TRAE trailer, and pushed immediately.
- Root repositories stay clean main/master mirrors.

## Workspaces and Branches

- Konnect integration: `/data00/home/wangqiyilang/playground/.worktree/debug-connectors-identity-rebind/konnect`, branch `feat/pcb-schematic-identity-rebind`, base `3e94a870e4b288be3f3cd5e95d2a42a1492bee46`.
- Konnect F1 task: `/data00/home/wangqiyilang/playground/.worktree/schematic-instance-flags-code/konnect`, branch `task/schematic-instance-flags-code`.
- Konnect F2 task: `/data00/home/wangqiyilang/playground/.worktree/schematic-instance-flags-docs/konnect`, branch `task/schematic-instance-flags-docs`.
- LH60 F3/F4: `/data00/home/wangqiyilang/playground/lh60/.worktree/debug-connectors-r3c-acceptance/lh60`, branch `task/debug-connectors-r3c-flag-instance`, base `940412322b4b7615384441f03c1e48e5812f37f2`.

## Units and Frozen Interface

| Unit | Deliverable | Dependency | Edge | Write scope |
|---|---|---|---|---|
| F0 | Approved spec and execution plan | R3c diagnosis | true blocking | Konnect docs |
| F1 | Atomic Konnect flag edit/readback and sync guard | F0 | true blocking | Konnect Rust |
| F2 | Konnect Skill/directory docs | frozen F1 schema | shared interface | Konnect docs/tests |
| F3 | LH60 generator/migration contract | frozen F1 schema | shared interface | LH60 Python |
| F4 | Three-symbol production migration | accepted/deployed F1 + F3 | true blocking | `lh60.kicad_sch` |
| F5 | Fresh disposable acceptance | F4 | true blocking | disposable PCB memory |

Frozen request:

```json
{
  "schematic": "/absolute/project.kicad_sch",
  "edits": [{"reference":"#FLG01","in_bom":true,"on_board":false,"dnp":false}]
}
```

Successful response includes `atomic: true`, `updated_count`, `updated[]` and `unchanged[]`; flag-bearing entries include final `flags` and `changed_flags`. F2 and F3 may run in parallel after F0. F4/F5 are serial.

## File Responsibility Map

- F1: `sch_batch.rs` owns schema/transaction/tests; `sch_components.rs` owns readback; `pcb_sync.rs` owns the three-skipped regression.
- F2: bundled schematic Skill, existing tool-directory row, and only necessary asset-token allowlist entries.
- F3: `tools/lh60_design/schematic.py` plus the four schematic contract/acceptance test modules.
- F4: `lh60.kicad_sch` through deployed Konnect only. PCB is forbidden.

---

### Task F0: Commit the Approved Plan

**Files:** Create `docs/superpowers/plans/2026-08-20-schematic-instance-flags.md`.

- [ ] Run placeholder/spec checks:

```bash
rg -n 'T[B]D|T[O]DO|implement lat[e]r|fill in detail[s]|appropriate error handlin[g]|S[i]milar to Task' docs/superpowers/plans/2026-08-20-schematic-instance-flags.md
git diff --check
```

Expected: `rg` prints nothing; diff check exits 0.

- [ ] Commit and push:

```bash
git add docs/superpowers/plans/2026-08-20-schematic-instance-flags.md
git diff --cached --check
hooks_dir=$(mktemp -d /tmp/konnect-empty-hooks.XXXXXX)
git -c core.hooksPath="$hooks_dir" commit -m "docs(schematic): plan atomic instance flag edits" -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
rmdir "$hooks_dir"
git push origin feat/pcb-schematic-identity-rebind
```

---

### Task F1: Implement Atomic Konnect Instance-Flag Editing

**Files:** Modify `crates/konnect-core/src/tools/sch_batch.rs`, `sch_components.rs`, and `pcb_sync.rs`.

**Interfaces:** Reuse `find_all_symbol_instance_blocks`, `find_direct_child_blocks`, `SexpEdit`, `read_consistent`, `write_atomic_if_unchanged`, and typed `cse::Symbol` flags.

- [ ] Create the isolated worktree from exact F0 base and verify clean status.

```bash
git worktree add -b task/schematic-instance-flags-code /data00/home/wangqiyilang/playground/.worktree/schematic-instance-flags-code/konnect 3e94a870e4b288be3f3cd5e95d2a42a1492bee46
```

- [ ] RED: add `batch_component_flag_tests` beside field-visibility tests:

```rust
batch_edit_schema_exposes_boolean_instance_flags
three_power_flags_change_only_on_board_atomically
explicit_flags_round_trip_and_repeat_is_byte_identical
multi_unit_flag_edit_updates_every_unit
invalid_flag_inputs_reject_without_any_write
mixed_field_and_flag_batch_is_all_or_nothing
```

The invalid table covers duplicate/empty/missing reference, nonexistent target, non-boolean flags, missing or duplicate direct flag token, and unrelated multi-unit inconsistency. Every case snapshots complete source bytes.

- [ ] Verify RED:

```bash
cargo test -p konnect-core batch_component_flag_tests -- --nocapture
```

Expected: at least six tests run and fail because schema/behavior are absent or non-atomic.

- [ ] GREEN: introduce minimal private seams:

```rust
struct BatchComponentEditRequest { reference: String, value: Option<String>, footprint: Option<String>, fields: BTreeMap<String,String>, in_bom: Option<bool>, on_board: Option<bool>, dnp: Option<bool> }
struct PreparedBatchComponentUpdate { content: String, updated: Vec<Value>, unchanged: Vec<Value> }
fn parse_batch_component_edit_requests(
    args: &serde_json::Value,
) -> Result<Vec<BatchComponentEditRequest>, CallToolResult>
fn direct_symbol_flag_atom_range(
    symbol: &str,
    flag: &str,
) -> Result<(usize, usize, bool), String>
fn prepare_batch_component_update(
    content: &str,
    requests: &[BatchComponentEditRequest],
) -> Result<PreparedBatchComponentUpdate, CallToolResult>
fn persist_batch_component_update(
    path: &Path,
    expected: &str,
    prepared: PreparedBatchComponentUpdate,
) -> anyhow::Result<CallToolResult>
```

Prevalidate the complete request before edits. Enumerate placed units with `find_all_symbol_instance_blocks` and direct flags with `find_direct_child_blocks(symbol,"symbol")`. Accept only exact `yes/no` atoms and replace only those atoms. Skip disk write when output equals input.

- [ ] Extend `edits.items.properties` with boolean `in_bom/on_board/dnp` and `additionalProperties:false`. Return final flags, changed flags, and atomic updated/unchanged accounting.

- [ ] Verify batch GREEN and regressions:

```bash
cargo test -p konnect-core batch_component_flag_tests -- --nocapture
cargo test -p konnect-core multi_unit_field_tests -- --nocapture
cargo test -p konnect-core field_visibility_tests -- --nocapture
```

- [ ] RED: add `get_schematic_component_includes_instance_flags` and `list_schematic_components_include_instance_flags`; verify they fail because response keys are absent.

- [ ] GREEN: extract one serializer shared by getter/list, preserve existing keys, and add exact booleans.

- [ ] Add a saved-schematic characterization test: three footprint-less `#FLG` instances with `in_bom=yes/on_board=no/dnp=no` become three `design.skipped` entries and produce `skipped_by_flag.planned==3`. It may be GREEN immediately and must not alter counter semantics.

- [ ] Complete F1 gates:

```bash
cargo fmt --all -- --check
cargo test -p konnect-core batch_component_flag_tests -- --nocapture
cargo test -p konnect-core instance_flags -- --nocapture
cargo test -p konnect-core pcb_sync -- --nocapture
cargo test -p konnect-core registry_tool_counts_match_reality -- --nocapture
cargo clippy -p konnect-core --all-targets -- -D warnings
git diff --check
```

Every filtered command must report `running N tests` with `N>0`.

- [ ] Commit only the three Rust files and push `task/schematic-instance-flags-code` with subject `feat(schematic): atomically edit instance flags` and the exact TRAE trailer.

---

### Task F2: Document the Frozen Konnect Contract

**Files:** Modify `crates/konnect/assets/skills/kicad-schematic/SKILL.md` and `tool-directory.md`; modify `crates/konnect/tests/asset_references.rs` only if required by exact prose-token validation.

**Interfaces:** Consume the frozen F1 names/atomicity; preserve all tool and catalogue counts.

- [ ] Create `task/schematic-instance-flags-docs` worktree from exact F0 base and verify clean status.

- [ ] Document `batch_edit_schematic_components` flag names, omission semantics, all-or-nothing behavior, byte-identical repeats, getter/list readback, and PCB-sync dry-run verification. Update only its existing tool-directory row; retain heading `sch_batch · 13 tools`.

- [ ] Run gates:

```bash
cargo test -p konnect --test asset_references -- --nocapture
cargo test -p konnect --test doc_tool_counts -- --nocapture
cargo test -p konnect-core registry_tool_counts_match_reality -- --nocapture
git diff --check
```

If asset validation identifies `in_bom` or `on_board` as prose-shaped phantom names, add exactly those parameter tokens to the existing allowlist. Do not add unrelated entries. Every filtered command must run at least one test.

- [ ] Commit/push `task/schematic-instance-flags-docs` as `docs(schematic): document atomic instance flags` with the exact trailer.

---

### Task F3: Encode the LH60 Contract Without Touching EDA Sources

**Files:** Modify `tools/lh60_design/schematic.py`, `tools/verify_schematic_contract.py`, `tools/verify_schematic_apply.py`, `tools/check_schematic_acceptance.py`, and `tools/verify_schematic_acceptance.py`.

**Interfaces:** Consume F1 schema/readback. Produce optional generator flags, one narrow production helper, a migration CLI, and pre/post identity/PCB evidence.

- [ ] Attach the clean detached worktree to `task/debug-connectors-r3c-flag-instance` only after proving exact base `940412322b4b7615384441f03c1e48e5812f37f2`, empty status, schematic SHA `7ae8a38afc453579f8f24de23e57772eff73056d12acd4fd9fcc6f0bf57533f9`, and PCB SHA `0a5722685ee378e9c9b240aa01a1f151f382cab83216edfa14a0663a1ac80664`.

- [ ] RED generator/apply tests:

```python
test_only_power_flags_have_explicit_instance_overrides
test_full_apply_finishes_with_one_exact_flag_batch
test_narrow_power_flag_migration_calls_only_one_batch_tool
test_capability_gate_requires_flag_item_schema_and_readback_tools
```

The full generator's final call, after conservative symbol refresh, is exactly one three-item flag-only batch. The narrow helper may call no other tool.

- [ ] Verify exactly four RED tests run:

```bash
PYTHONDONTWRITEBYTECODE=1 python -m unittest -v \
  tools.verify_schematic_contract.SchematicPlanContractTest.test_only_power_flags_have_explicit_instance_overrides \
  tools.verify_schematic_apply.SchematicApplyContractTest.test_full_apply_finishes_with_one_exact_flag_batch \
  tools.verify_schematic_apply.SchematicApplyContractTest.test_narrow_power_flag_migration_calls_only_one_batch_tool \
  tools.verify_schematic_apply.SchematicApplyContractTest.test_capability_gate_requires_flag_item_schema_and_readback_tools
```

- [ ] GREEN model/helper:

```python
class SchematicComponent:
    in_bom: bool | None = None
    on_board: bool | None = None
    dnp: bool | None = None

POWER_FLAG_INSTANCE_FLAGS = {
    "#FLG01": {"in_bom": True, "on_board": False, "dnp": False},
    "#FLG02": {"in_bom": True, "on_board": False, "dnp": False},
    "#FLG03": {"in_bom": True, "on_board": False, "dnp": False},
}

def _instance_flag_payload(
    component: SchematicComponent,
) -> dict[str, object] | None
def apply_power_flag_instance_flags(
    client: McpClient,
    schematic: Path = SCHEMATIC,
) -> dict[str, object]
```

Only the three flag records set values. The helper calls one batch tool and parses JSON fail-closed: `atomic is True`, exact reference set, exact final flags, and `updated_count + len(unchanged) == 3`. Call it after final symbol refresh in full generation.

- [ ] Harden `require_schematic_capabilities` by inspecting nested `edits.items.properties` for exact boolean inputs and requiring getter/list tool ownership. Runtime tests verify output fields.

- [ ] Run generator/apply GREEN:

```bash
PYTHONDONTWRITEBYTECODE=1 python -m unittest -v tools.verify_schematic_contract tools.verify_schematic_apply
```

Protected hashes must remain unchanged.

- [ ] RED migration acceptance tests:

```python
test_power_flag_migration_preserves_all_identities_and_pcb_bytes
test_power_flag_migration_rejects_any_non_flag_or_identity_drift
test_power_flag_migration_cli_is_narrow_and_requires_output
```

Add exact `POWER_FLAG_INSTANCE_CONTRACT`, reference/UUID plus wire/label fingerprint helpers, and mutually exclusive `--migrate-power-flag-instance-flags` requiring `--output`.

- [ ] Verify exactly three RED tests run, then implement the narrow flow:
  1. Fail closed on schema/read tools.
  2. Query component/wire/label UUIDs and source/PCB hashes.
  3. Call the helper exactly once.
  4. Query again and require identical fingerprints.
  5. Require exact three final flag states, unchanged frozen component/pin hashes, and unchanged PCB hash.
  6. Persist JSON evidence.

Do not call candidate generation, convergence, a PCB tool, or `save_project`.

- [ ] Complete F3 gates:

```bash
PYTHONDONTWRITEBYTECODE=1 python -m unittest -v \
  tools.verify_schematic_contract tools.verify_schematic_apply \
  tools.verify_schematic_acceptance tools.verify_pcb_sync tools.verify_pcb_placement
PYTHONDONTWRITEBYTECODE=1 python -m compileall -q tools
git diff --check
sha256sum lh60.kicad_sch lh60.kicad_pcb
```

Before F4, schematic remains `7ae8a38afc453579f8f24de23e57772eff73056d12acd4fd9fcc6f0bf57533f9` and PCB remains `0a5722685ee378e9c9b240aa01a1f151f382cab83216edfa14a0663a1ac80664`.

- [ ] Commit exactly five Python files as `feat(schematic): model non-board power flags` with the exact trailer; push `task/debug-connectors-r3c-flag-instance`.

---

### Task F1/F2 Integration and Reversible Deployment

- [ ] Independently review F1 and F2 diff packages against this plan. Require Spec PASS/Quality PASS/no Critical or Important; fixes stay on owning branches with scoped re-review.

- [ ] Cherry-pick reviewed F1 then F2 into clean `feat/pcb-schematic-identity-rebind`, preserving exactly one trailer; run focused gates and push after each commit.

- [ ] Full accepted-source gate:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p konnect
sha256sum target/release/konnect
git diff --check
```

Record accepted source/binary SHA in the existing SDD ledger.

- [ ] Reversible deployment: record current `~/.local/bin/konnect` target/SHA; audit every live Konnect PID via `/proc/<pid>/exe`; atomically repoint to the accepted binary; spawn a fresh non-TTY process; verify version, flag schema ownership, getter/list tool ownership, and unchanged catalogue counts. Preserve rollback target evidence.

---

### Task F4: Migrate Exactly Three Production Schematic Flags

**Files:** Modify only `lh60.kicad_sch` through deployed Konnect. `lh60.kicad_pcb` is forbidden.

- [ ] Independently review F3. Require Spec PASS/Quality PASS/no Critical or Important before an EDA write.

- [ ] Fresh pre-mutation gates: clean status, schematic `7ae8a38afc453579f8f24de23e57772eff73056d12acd4fd9fcc6f0bf57533f9`, PCB `0a5722685ee378e9c9b240aa01a1f151f382cab83216edfa14a0663a1ac80664`, no schematic writer, all Python contract suites green.

- [ ] Execute once:

```bash
PYTHONDONTWRITEBYTECODE=1 python tools/check_schematic_acceptance.py \
  --migrate-power-flag-instance-flags \
  --output /tmp/lh60-power-flag-instance-migration.json
```

On failure, do not patch/restore; discard and recreate the worktree from pushed F3.

- [ ] Prove exact delta: all 155 component UUID pairs, wire UUIDs, label UUIDs, functional hashes, and PCB bytes unchanged; exactly three final flag states; schematic diff contains only three `on_board yes -> no` atom changes; ERC/acceptance green.

```bash
PYTHONDONTWRITEBYTECODE=1 python -m unittest -v \
  tools.verify_schematic_contract tools.verify_schematic_apply \
  tools.verify_schematic_acceptance tools.verify_pcb_sync tools.verify_pcb_placement
PYTHONDONTWRITEBYTECODE=1 python tools/check_schematic_acceptance.py --preflight
git diff --check
sha256sum lh60.kicad_pcb
git diff -- lh60.kicad_pcb
```

- [ ] Independent schematic/evidence review receives the JSON evidence, complete schematic-only diff, hashes, and test output. Require Spec PASS/Quality PASS/no Critical or Important.

- [ ] Stage only `lh60.kicad_sch`. Commit as `fix(schematic): exclude power flags from board sync` with exact trailer and push.

---

### Task F5: Fresh Disposable 146-Rebind/6-Header Acceptance

**Files:** Task cache and fresh detached disposable worktree only; no committed source change.

- [ ] Remove the old disposable worktree only after confirming no KiCad process owns it. Recreate detached at exact pushed F4 tip and verify clean status/hashes.

- [ ] Start fresh isolated `TMPDIR`, cache, Konnect config, and socket under the task cache; open only the disposable PCB under Xvfb; require `kicad_ui_running=true`.

- [ ] Self-checking never-save transaction:
  - initial exact 169 board refs, 146 shared refs, zero zones/routing, saved PCB `0a5722685ee378e9c9b240aa01a1f151f382cab83216edfa14a0663a1ac80664`;
  - rebind dry-run `requested=eligible=planned=146`, no conflict, exactly 146 changes;
  - apply exact revision, `applied=146`;
  - rebind dry-run noop with `eligible=planned=applied=conflicts=0`;
  - normal sync dry-run exactly six J additions, zero updates/pad changes/conflicts, 23 board-only preserved, 3 skipped by flag, changes exactly J1..J6, diagnostics empty, undo null.

Do not delete TPs, apply normal sync, or save.

- [ ] Prove PCB bytes unchanged, kill without save, prove hash again, remove whole disposable worktree, retain only cache evidence.

- [ ] Append accepted Konnect source/binary SHAs, LH60 F3/F4 commits, exact counts, and hashes to the identity-rebind ledger. Mark R3c complete and resume original deployment/consumer work without altering the one-save production boundary.
