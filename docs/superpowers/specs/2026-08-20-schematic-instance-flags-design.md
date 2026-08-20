# Atomic Schematic Instance Flags Design

## Context

The disposable LH60 identity-rebind acceptance reached the intended
`ready -> applied -> noop` sequence for all 146 shared schematic/PCB
references.  The following normal PCB-sync dry run planned the expected six
debug-header additions and preserved all 23 existing test points, but reported
`skipped_by_flag.planned = 0` instead of the frozen value `3`.

The saved LH60 schematic explains the result: `#FLG01`, `#FLG02`, and
`#FLG03` are currently persisted as `in_bom=yes`, `on_board=yes`, and
`dnp=no`.  Konnect intentionally counts only saved instances with
`in_bom=yes` and `on_board=no` as `skipped_by_flag`.  The power flags have no
footprints and therefore do not appear in KiCad's exported netlist; their
saved instance flags are the only authoritative way to tell PCB sync that
they are schematic-only.

The repair must preserve the existing `skipped_by_flag = 3` safety gate.  It
must not weaken PCB-sync conflict behavior, reinterpret the counter, directly
edit a KiCad source file, or regenerate the complete production schematic.

## Approved outcome

Konnect extends the existing `batch_edit_schematic_components` tool with
optional per-edit `in_bom`, `on_board`, and `dnp` booleans.  The three LH60
power flags are then migrated in one narrow Konnect call to:

```text
in_bom=true
on_board=false
dnp=false
```

The same explicit state becomes part of the durable LH60 schematic generator
contract.  No other schematic instance receives an explicit override.

## Alternatives considered

### Extend the existing batch editor (selected)

This reuses the one-file transaction boundary already owned by `sch_batch`,
does not add a public tool, and lets a three-reference migration commit once.
The schematic editor already parses and serializes all three flags, so the
new surface exposes existing typed state rather than adding another storage
model.

### Add a dedicated flags tool

This would produce a second schematic-instance mutation path and another
public tool/count/documentation surface for state already owned by the batch
editor.  The extra interface is not justified.

### Accept `skipped_by_flag = 0` in LH60

This would hide incorrectly board-eligible schematic-only markers and remove a
load-bearing production guard.  It is rejected.

## Konnect public contract

`batch_edit_schematic_components` remains in `sch_batch` and retains its
existing top-level shape.  Each `edits[]` item gains these optional fields:

```json
{
  "reference": "#FLG01",
  "in_bom": true,
  "on_board": false,
  "dnp": false
}
```

- `in_bom`: whether the placed symbol instance is included in the BOM.
- `on_board`: whether the placed symbol instance participates in PCB sync.
- `dnp`: whether the placed symbol instance is do-not-populate.
- Omitted fields preserve the current value.
- The exact KiCad names are used; aliases are not accepted.
- Existing `value`, `footprint`, and custom `fields` edits remain supported.

`get_schematic_component` and `list_schematic_components` add the three
booleans to their JSON responses so callers can preflight and verify writes.
The singular `edit_schematic_component` does not gain write parameters; flag
writes have one public transaction implementation.

No tool is added or removed.  Registry and published tool totals therefore do
not change.

## Atomic mutation semantics

The batch editor becomes fail-closed for the complete request, including
mixed field and flag edits:

1. Read the schematic once with the existing consistent-read path.
2. Prevalidate every edit before constructing a write:
   - `reference` is a nonempty string;
   - references are unique within the request;
   - supplied flags are JSON booleans;
   - every target resolves to at least one placed symbol instance;
   - each requested flag exists exactly once as a direct child of every unit;
   - unrelated flags on a multi-unit symbol are internally consistent.
3. Target only placed symbol instances, never cached `lib_symbols`
   definitions.
4. Replace only the direct `yes`/`no` atom for each requested flag.
5. If any preflight check fails, return an error and leave the file
   byte-identical.
6. Otherwise perform exactly one revision-checked atomic write.
7. Reread the changed targets and return their final effective flags.

A valid request that already matches succeeds as a no-op without rewriting
the file.  The response distinguishes updated and unchanged targets and
reports `atomic: true`.

## Konnect verification

Focused tests lock these behaviors before implementation:

- three hidden `#FLG` references change only `on_board` from `yes` to `no`;
- all three explicit flags round-trip;
- multi-unit requested flags update every unit;
- duplicate reference, missing target, non-boolean input, missing direct flag
  token, or unrelated multi-unit inconsistency rejects the whole request and
  preserves complete source bytes;
- an exact repeat is a successful byte-identical no-op;
- getter/list responses expose the three booleans;
- applying saved flags to a local three-flag schematic moves the entries into
  `design.skipped` and produces `skipped_by_flag.planned = 3`;
- existing normal-sync and identity-rebind suites remain unchanged and green.

Bundled schematic Skill text and the existing tool-directory row document the
new fields, omission semantics, atomicity, and readback workflow.  Parameter
names referenced in prose are aligned with asset-reference validation.

## LH60 generator contract

`SchematicComponent` gains optional instance-flag fields.  Only
`#FLG01..03` set explicit values; every other component leaves them unset so
the generator cannot accidentally restate or change 152 unrelated instances.

Full disposable generation keeps the existing placement, field, connection,
visibility, and symbol-refresh stages.  A final flag-only
`batch_edit_schematic_components` call runs after
`update_symbols_from_library`, ensuring a later refresh cannot overwrite the
per-instance state.

A separate `apply_power_flag_instance_flags(client, schematic)` helper owns
the production migration.  It makes exactly one batch call containing the
three references and no placement, deletion, wiring, refresh, PCB-sync, or
save operation.

## LH60 production migration and invariants

The LH60 repair branch starts from composed tip
`940412322b4b7615384441f03c1e48e5812f37f2`.  A full schematic convergence is
forbidden for this migration because it would regenerate object identities.

Before the write, acceptance captures:

- reference-to-UUID fingerprints for U1, D1-D70, SW1-SW58, SW60-SW76,
  J1-J6, and all three flags;
- wire and label UUID sets;
- functional component and pin hashes;
- schematic and PCB SHA-256 values.

After the one batch call, all fingerprints, connectivity hashes, inventories,
and PCB bytes must be identical.  The only permitted serialized schematic
changes are three `on_board yes -> no` atoms.  ERC and existing schematic
acceptance remain green.  The production PCB is never opened or modified by
this migration and must not appear in its commit.

## R3c acceptance reset

After the Konnect capability and LH60 migration are independently committed,
reviewed, pushed, and the accepted Konnect binary is deployed:

1. Create a new detached disposable LH60 worktree from the new pushed tip.
2. Open only its PCB in a fresh task-owned Xvfb/KiCad session.
3. Require the exact 169-item initial inventory and unchanged protected-file
   hashes.
4. Rebind exactly the frozen 146 references: `ready -> applied -> noop`, with
   no conflicts and no save.
5. Run normal sync dry-run and require exactly:
   - six J1-J6 additions;
   - 23 board-only TP footprints preserved;
   - three schematic instances skipped by flag;
   - zero updates, pad reassignments, or conflicts.
6. Require the saved PCB file to remain byte-identical, then kill the unsaved
   session and remove the whole disposable worktree.

Only after this succeeds may the original identity-rebind plan proceed to
deployment, LH60 consumer orchestration, and the single production PCB save.

## Units, dependencies, and write ownership

| Unit | Deliverable | Dependency | Edge | Write owner |
|---|---|---|---|---|
| F0 | Approved design and implementation plan | R3c diagnosis | true blocking | Konnect docs |
| F1 | Atomic Konnect batch flag editing and readback | F0 | true blocking | Konnect Rust tests/code |
| F2 | Konnect Skill/tool-directory documentation | frozen F1 schema | shared interface | Konnect docs |
| F3 | LH60 generator contract and focused tests | frozen F1 schema | shared interface | LH60 Python files |
| F4 | Three-symbol production schematic migration | accepted/deployed F1 + F3 | true blocking | LH60 schematic |
| F5 | Fresh R3c disposable acceptance | F4 | true blocking | disposable PCB memory only |

F2 and F3 may run in parallel after the schema is frozen because their write
scopes do not overlap.  F1 has one Rust writer.  F4 and F5 are strictly
serial.

Every logical unit is independently verified, reviewed, committed with the
required TRAE trailer exactly once, and pushed immediately.
