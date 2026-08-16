# Konnect Handoff: Safe Closed-Board Footprint Placement and Flip

## Goal

Finish, review, commit, push, and submit/update the upstream PR for the PCB
component capabilities required by the LH60 production layout:

1. safe closed-board `move_component`;
2. safe closed-board `rotate_component`;
3. safe closed-board `flip_component` between `F.Cu` and `B.Cu`.

Do not modify the LH60 production PCB from this Konnect task. Use temporary
coupon projects under `/tmp` for end-to-end verification.

## Repository State

- Worktree:
  `/data00/home/wangqiyilang/playground/.worktree/konnect-footprint-graphics/konnect`
- Branch: `feat/footprint-graphics`
- Origin: `https://github.com/TWT233/Konnect.git`
- Upstream: `https://github.com/mixelpixx/Konnect.git`
- Existing upstream PR from this branch: PR #205; verify that it is still open
  before creating another PR.
- Current committed HEAD:
  `3ef87f503124a969368fa4544c66a3bd3ca9b968`
- `origin/feat/footprint-graphics` matches that SHA.
- The user reports that upstream was synchronized immediately before this
  handoff. Re-check `git status`, `git log`, `git fetch upstream`, and PR state
  before integrating. Preserve the current uncommitted WIP; do not reset or
  stash it away.

## Already Committed and Pushed

Commit:

```text
3ef87f5 feat(pcb): support safe closed-board placement updates
```

It adds typed IPC failure gating and revision-aware atomic closed-board
fallbacks for `move_component` and `rotate_component`.

Behavior:

- `with_ipc_classified` distinguishes `Unreachable` from `Rejected`.
- IPC operations call `ensure_board_is_active`.
- `Rejected` fails closed and leaves the file untouched.
- `Unreachable` reads a consistent board, locates exactly one footprint by
  reference, updates its root placement, validates the resulting S-expression,
  and writes with `write_atomic_if_unchanged`.
- Move preserves the existing angle.
- Rotate sets an absolute target angle and applies the delta to pad/text child
  angles.
- Missing references and stale-source conflicts do not write.
- README and bundled `kicad-pcb` skill documentation were updated.

Validation already completed for `3ef87f5`:

```text
cargo test -p konnect-core
  442 unit tests passed
  4 conformance tests passed
  12 integration tests passed

cargo test -p konnect --test asset_references
  3 tests passed

cargo clippy -p konnect-core --all-targets -- -D warnings
  passed

cargo build --release -p konnect
  passed
```

An independent MCP smoke project under `/tmp` returned `source=file` for
place, move, and rotate, with final root placement `(at 30 40 270)`.

## Current Uncommitted WIP

Changed files:

```text
README.md
crates/konnect-core/src/router/registry.rs
crates/konnect-core/src/tools/pcb_components.rs
crates/konnect/assets/skills/kicad-pcb/SKILL.md
```

The WIP adds `flip_component` and currently changes roughly 697 lines.

### Public Contract

```json
{
  "board": "/path/to/board.kicad_pcb",
  "reference": "U1",
  "layer": "B.Cu"
}
```

- `layer` is restricted to `F.Cu` or `B.Cu`.
- The operation is idempotent when the footprint is already on the requested
  side.
- A reachable KiCad fails closed because the typed IPC API has no native
  footprint-flip command.
- An unreachable KiCad allows a revision-aware atomic closed-board transform.
- Unsupported footprint geometry fails closed.

### Geometry Basis

The implementation follows current KiCad `FOOTPRINT::Flip` semantics, checked
against upstream KiCad source:

- mirror children in the footprint library frame across the local X axis;
- negate the footprint root orientation and normalize to `(-180, 180]`;
- swap front/back layers;
- text local position: `(x, y) -> (x, -y)`;
- text local angle: `180 - angle`;
- toggle text `mirror` justification;
- pad local position: `(x, y) -> (x, -y)`;
- pad local angle: `-angle`;
- graphic points mirror Y;
- arc start/end swap after mirroring;
- model transforms remain unchanged.

Supported file-fallback child types currently include:

- `property`;
- `fp_text`;
- `fp_line`;
- `fp_rect`;
- `fp_circle`;
- `fp_arc`;
- `fp_poly`;
- standard non-custom `pad`;
- ordinary metadata/model/group blocks are preserved unchanged.

The implementation rejects custom pads and unsupported `fp_*`/zone geometry
rather than silently corrupting them.

### Current Automated Evidence

RED was observed first because `flip_footprint_block` and
`handle_flip_component` did not exist.

Current tests cover:

- KiCad-style library-frame transform;
- closed-board `F.Cu -> B.Cu`;
- idempotent repeat to the same side;
- reachable rejection with zero file writes;
- custom-pad rejection.

Latest full test run before the final clippy-only readability edit:

```text
cargo test -p konnect-core
  446 unit tests passed
  4 conformance tests passed
  12 integration tests passed

cargo test -p konnect --test asset_references
  3 tests passed
```

After the final readability-only edit:

```text
cargo clippy -p konnect-core --all-targets -- -D warnings
  passed

cargo build --release -p konnect
  passed

git diff --check
  passed
```

Run the full suite again before committing.

## Mandatory Remaining Work

### 1. Review and Simplify the WIP

The flip implementation is intentionally fail-closed, but it is large. Review
the transform helpers for correctness and simplify duplication where possible
without weakening the safety gate.

Pay particular attention to:

- exact parsing of root versus nested `(at ...)` and `(layer ...)` blocks;
- angle normalization at `0`, `90`, `180`, `270`, negative angles, and
  non-cardinal angles;
- arc start/end reversal;
- text `justify mirror` insertion/removal;
- CRLF preservation;
- legacy `fp_text reference` footprints;
- duplicate references;
- stale-source conflict behavior;
- flip round trip `F -> B -> F`;
- behavior with hidden fields;
- through-hole pads using `"*.Cu"` and `"*.Mask"`;
- pad attributes that must be mirrored or rejected, especially `offset`,
  `rect_delta`, chamfered pads, and custom primitives;
- footprint-local zones and newer KiCad item types.

### 2. Add Missing Tests

At minimum add focused tests for:

- `B.Cu -> F.Cu` round trip returning the original supported footprint;
- stale-source conflict for flip;
- missing reference;
- invalid target layer as structured `invalid_argument`;
- CRLF board preservation;
- legacy `fp_text reference`;
- through-hole pad layer preservation;
- hidden property mirroring;
- non-cardinal root orientation;
- unsupported geometry zero-write behavior.

### 3. Real LH60 Coupon Verification

Use a fresh MCP process from the newly built release binary. Create a temporary
project under `/tmp`; do not copy or text-edit KiCad source files.

Register:

```text
/data00/home/wangqiyilang/playground/lh60/.worktree/lh60-rp2040-v2/lh60/lib/lh60-mcu/lh60-mcu.pretty
/data00/home/wangqiyilang/playground/lh60/.worktree/lh60-rp2040-v2/lh60/lib/lh60-core/lh60-core.pretty
```

Place and flip at least:

```text
lh60-mcu:MCU_RP2040-Tiny_SMD
lh60-core:D_SOD-323_Bottom
lh60-core:TestPoint_Pad_D1.5mm_Bottom
```

Verify with KiCad 10:

```bash
kicad-cli pcb drc --format json --output /tmp/<coupon>-drc.json /tmp/<coupon>.kicad_pcb
```

Required evidence:

- board parses;
- all three root layers are `B.Cu`;
- SMD pads use `B.Cu/B.Paste/B.Mask` as appropriate;
- front graphics moved to corresponding back layers;
- text is mirrored;
- models and attributes remain present;
- no `lib_footprint_mismatch`;
- no invalid-layer or malformed-footprint findings.

Expected unrelated errors from an outline-less/unconnected coupon must be
separated from flip-specific failures.

Then flip a second fresh coupon back to `F.Cu` and compare supported geometry
against the original placement.

On this host, run `kicad-cli` serially or use an isolated `TMPDIR` because
concurrent AppImage extraction can remove another process's `AppRun`.

### 4. Full Verification

Run:

```bash
cargo fmt --all --check
cargo test -p konnect-core
cargo test -p konnect --test asset_references
cargo clippy -p konnect-core --all-targets -- -D warnings
cargo build --release -p konnect
git diff --check
```

### 5. Commit, Push, and Upstream PR

Commit message must end exactly once with:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

Suggested commit subject:

```text
feat(pcb): add safe closed-board footprint flipping
```

Push `origin/feat/footprint-graphics`.

Check existing upstream PR #205 first. Prefer updating that PR if it still uses
this branch; do not create a duplicate. Its title/body must reflect the full
current scope, including closed-board move, rotate, and flip safety, not only
the original footprint-graphics feature.

If `gh pr edit` fails due to Classic Projects GraphQL deprecation, use:

```bash
gh api --method PATCH repos/mixelpixx/Konnect/pulls/205 --input -
```

Include:

- typed IPC failure gating;
- revision-aware atomic file writes;
- fail-closed reachable-KiCad behavior;
- supported/rejected geometry boundary;
- full Rust verification;
- real KiCad 10 LH60 coupon evidence.

## Local Deployment Note

`~/.local/bin/konnect` has previously been a symlink to this worktree's
`target/release/konnect`, but the binary hashes differed at handoff time.
Re-check the symlink and running processes after building:

```bash
ls -l ~/.local/bin/konnect
readlink -f ~/.local/bin/konnect
sha256sum target/release/konnect ~/.local/bin/konnect
pgrep -af '(^|/)konnect( |$)'
readlink /proc/<pid>/exe
```

Processes showing `(deleted)` are stale. Do not kill unrelated sessions
blindly. Synchronize the updated bundled skill to:

```text
~/.agents/skills/kicad-pcb/SKILL.md
```

only after the implementation is finalized.

## LH60 Continuation Gate

The LH60 session must not claim bottom-side placement complete until the
finalized `flip_component` build passes the real coupon checks and is deployed.
Socket center placement can proceed independently with committed
`move_component`/`rotate_component`, but MCU, all 70 diodes, and all 23 test
pads require a true geometry flip rather than a layer-name substitution.
