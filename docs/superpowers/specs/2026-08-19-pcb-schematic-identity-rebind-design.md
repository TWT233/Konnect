# PCB Schematic Identity Rebind Design

## Problem

`update_pcb_from_schematic` deliberately refuses to match a board footprint by
reference when both the PCB and the saved schematic already carry different
nonempty schematic identities. That guard prevents accidental reassignment of
an existing board footprint to an unrelated schematic symbol.

The LH60 A3 schematic convergence rebuilt the root-sheet symbol instances while
preserving every reference, value, footprint and electrical connection. The
saved PCB therefore still has the former `symbol_path` for 146 shared footprints
(`U1`, `D1..D70`, `SW1..SW58`, `SW60..SW76`), while the new schematic netlist has
new symbol UUIDs for the same logical components. A normal sync correctly reports
146 `reference_identity_conflict` diagnostics and performs no mutation.

The PCB itself remains unrouted and unchanged: 169 footprints, 23 legacy test
pads, 0 zones, and 0 segment/via/arc routing objects. The required next operation
is not a general conflict override. It is a narrowly proven identity migration.

## Decision

Add an independent live-IPC tool named
`rebind_pcb_schematic_identities` in the `sch_export` toolset. It plans and
atomically applies only `FootprintInstance.symbol_path` changes. It never adds,
deletes, moves, rotates, flips, reroutes, renumbers, changes values, modifies
DNP, changes pad nets, changes footprint library IDs, or saves the board.

The existing `update_pcb_from_schematic` conflict behavior remains unchanged.
Callers must explicitly choose rebind and supply the exact references they intend
to migrate.

## Alternatives Considered

### 1. Independent explicit rebind tool — chosen

This keeps the exceptional migration separate from routine schematic-to-PCB
sync, makes the evidence and undo boundary reviewable, and prevents a future
caller from enabling a broad permissive flag by accident.

### 2. `allow_identity_rebind` on `update_pcb_from_schematic`

This would require fewer public tools but mixes an exceptional identity-repair
policy into the normal sync state machine. A single Boolean could silently
weaken the principal conflict guard for every component in the design. Rejected.

### 3. Rebuild the PCB or revert the accepted schematic

Rebuilding risks losing reviewed placement, layer, field, footprint and geometry
state. Reverting the A3 schematic would reopen an accepted L4 artifact. Rejected.

## Public Interface

Toolset: `sch_export`

Tool: `rebind_pcb_schematic_identities`

Input schema:

```json
{
  "type": "object",
  "properties": {
    "schematic": { "type": "string" },
    "board": { "type": "string" },
    "references": {
      "type": "array",
      "items": { "type": "string" },
      "minItems": 1,
      "uniqueItems": true
    },
    "dry_run": { "type": "boolean", "default": true },
    "expected_plan_revision": { "type": "string" }
  },
  "required": ["schematic", "board", "references"]
}
```

Apply requires `dry_run=false` and the exact nonempty
`expected_plan_revision` returned by the latest equivalent dry run. References
are normalized to a sorted unique list for planning and revision hashing, but a
duplicate input is rejected rather than silently deduplicated.

## Match Contract

For each requested reference, the saved schematic netlist and current live PCB
must each contain exactly one component. The following fields must match before a
rebind can be planned:

- exact reference;
- exact value;
- exact footprint library ID;
- exact DNP state;
- exact set of pad numbers;
- exact pad-number-to-logical-net mapping after normalizing only the root-sheet
  KiCad prefix (`/VSYS` equals `VSYS`; nested names such as `/sheet/VSYS` are not
  normalized into root nets);
- both old and new `symbol_path` must be nonempty and syntactically valid. A
  valid path starts with exactly one `/`, has one or more nonempty `/`-separated
  segments, and every segment must parse with `uuid::Uuid::parse_str`; no empty,
  symbolic or trailing segment is accepted;
- old and new identities must differ.

The tool rejects missing references, duplicate references, duplicate identities,
board-only footprints, `not_in_schematic` footprints, ambiguous net names, value
drift, footprint drift, DNP drift, pad-set drift, pad-net drift, or a reference
whose identity already matches. A mixed request is all-or-nothing: one conflict
clears every planned change.

No routed-net exception exists. Even if copper is present, only identity can
change; nevertheless exact pad/net equality is mandatory.

## Dry-Run Response

Normal JSON response:

```json
{
  "status": "ready|noop|conflict|applied",
  "plan_revision": "<sha256>",
  "coverage": {
    "source": "saved_schematic_hierarchy",
    "hierarchy_files": 1,
    "transport": "live_kicad_ipc",
    "atomicity": "single_kicad_undo_commit",
    "requested": 146,
    "eligible": 146,
    "planned": 146,
    "applied": 0,
    "conflicts": 0
  },
  "changes": [
    {
      "reference": "D1",
      "kiid": "<board footprint UUID>",
      "old_symbol_path": "/11111111-1111-4111-8111-111111111111",
      "new_symbol_path": "/22222222-2222-4222-8222-222222222222",
      "value": "1N4148WS",
      "footprint_id": "lh60-core:D_SOD-323_Bottom",
      "dnp": false,
      "pad_nets": { "1": "KEY_00", "2": "ROW0" },
      "preserve": {
        "position": { "x": 0.0, "y": 0.0 },
        "rotation": 0.0,
        "layer": "F.Cu",
        "locked": false
      }
    }
  ],
  "diagnostics": [],
  "undo": null
}
```

`noop` is returned only when every requested identity already matches; it has no
changes or diagnostics. A partially matching request is rejected as conflict, not
partially applied.

The plan revision hashes the saved design-bearing netlist, live board identity and
field snapshot for every requested reference, sorted requested references, exact
planned changes, and stable board document identity. It excludes export timestamps.

## Apply Transaction

Apply repeats the full live snapshot and planning process. A stale or absent
revision is non-mutating. A ready plan is sent as one KiCad commit named
`Rebind PCB schematic identities`.

For each footprint, the outgoing protobuf is cloned from the exact live
`FootprintInstance`; only `symbol_path` is replaced. No library definition,
attributes, fields, pads, graphics, models or transforms are reconstructed.

After the commit, the tool rereads all requested footprints and requires:

- exact new symbol path;
- byte-equivalent canonical protobuf for every field except `symbol_path`;
- unchanged reference/value/footprint/DNP;
- unchanged pad UUIDs, numbers, nets, coordinates and geometry;
- unchanged position, rotation, layer and locked state;
- unchanged footprint graphics, fields, attributes and models.

Any gained/lost item, duplicate identity, or non-identity readback difference makes
the tool call fail. It does not save. The caller closes the unsaved KiCad document
without saving and recreates the isolated consumer worktree/session.

Successful apply reports `status=applied`, `applied=requested`, empty diagnostics,
and nonempty undo guidance. A following dry run for the same references must return
`noop`.

## LH60 Transaction Order

L5C uses one live, unsaved KiCad session:

1. Revalidate committed L5B baseline, hashes, exact 169 inventory, TP pads, zero
   zones, zero segment/via/arc count and 23 actual board-net trace queries.
2. Dry-run rebind for the exact 146 shared references and review all changes.
3. Apply the exact rebind revision.
4. Dry-run rebind again and require noop.
5. Run the existing normal schematic-to-PCB dry run and require exactly six J adds,
   23 board-only TP footprints, three skipped flags and no conflict/update/pad change.
6. Delete TP1..TP23, rerun dry-run, and apply the second exact sync revision.
7. Before saving, use live IPC to verify 152 references, exact J pad nets, 23
   actual board-net trace queries with zero segments, and a final normal-sync
   noop. These checks inspect the unsaved live board.
8. Save exactly once. `run_drc` and `validate_for_manufacturing` read the PCB
   file, so they are deliberately not used as proof of the new state before this
   save.
9. Reread the saved PCB and require unchanged schematic hash, changed PCB hash,
   exact 152 inventory, exact staged J attributes/pads/nets, zero zones,
   manufacturing `track_count=0` (segment + via + arc), 23 empty actual
   board-net trace queries, complete untruncated DRC, and a repeated normal-sync
   noop. Then commit only the PCB.

Any failure from step 3 through step 7 closes KiCad without saving and
discards/recreates the entire L5 consumer worktree from the last pushed commit.
Any failure after step 8 also discards/recreates the worktree, but never performs
a second save. No restore, stash or reset is allowed in either case.

## Verification

Konnect tests must include:

- pure planner tests for exact eligibility and every conflict family;
- dry-run determinism and revision drift;
- apply refusing absent/stale revision;
- one atomic commit and no save;
- protobuf/canonical readback proving only `symbol_path` changes;
- regression coverage for pad/graphic corruption;
- stdio protocol/schema coverage;
- a disposable live KiCad test;
- LH60 acceptance proving `146 ready -> 146 applied -> noop`, followed by the
  existing sync producing exactly the frozen six J additions.

The consumer repository adds FakeClient orchestration tests for the rebind-before-
sync ordering and the discard-without-save failure path.

## Documentation

Update the tool directory and bundled `kicad-pcb` skill with the exceptional use
case and the warning that this is not a general conflict override. Tool totals and
asset-reference guards must remain consistent.
