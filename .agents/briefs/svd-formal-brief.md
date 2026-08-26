# Brief: `svd_formal` — Lean verification project for ic-stable-vec-deque

Scope source of truth (read it first):
`/Users/yota/Library/Application Support/zerostack/agent/memory/projects/gleaph-1d2f63a8/notes/svd-formal-scope.md`

Reference methodology (copy structure and conventions; do not import code):
`crates/ic-stable-linear-hash-map/formal/` (`Lhm_formal`, Lean 4.33.1 core-only,
hand-transcription with line-range citations, pinned rev, SCOPE.md/REPORT.md,
`#print axioms` sorry-guard).

## Mission

Create standalone Lake project `crates/ic-stable-vec-deque/formal/`
(package `svd_formal`, root lib `Svd`) verifying the V1 ring layout of
`crates/ic-stable-vec-deque/src/vec_deque.rs`.

Hard constraints:

- Lean **core-only**, toolchain `leanprover/lean4:v4.33.1` (already installed locally;
  copy `lean-toolchain` / `lakefile.lean` shape from the lhm formal dir).
- Cargo-invisible: NO `Cargo.toml`; root workspace enumerates members explicitly, so no
  workspace/Cargo changes anywhere.
- Hand-transcription: every Lean definition cites `vec_deque.rs` name + line range it mirrors.
- `Svd.lean` ends with `#print axioms` for every headline theorem (sorry-guard).
- Delivered stages must be `sorry`-free; `lake build` must pass with zero errors.
- Lean-proof discipline: one tactic at a time, verify with `done`, hardest case first,
  minimal-proof cleanup afterwards. `by sorry` only for future-stage placeholders kept out
  of the build (or marked clearly in SCOPE.md as unproven follow-up).
- Do not modify any existing Rust code, tests, or design docs. New files under
  `crates/ic-stable-vec-deque/formal/` plus this brief's Progress section only.

## Deliverables this round: scaffold + Stages 1–3 (all proved)

1. Scaffold: `formal/lean-toolchain`, `formal/lakefile.lean` (package «svd_formal»,
   default_target lean_lib «Svd»). Run `lake build` there to generate manifest.
2. `Svd/Basic.lean`: modular-arithmetic foundations adapted from `Lhm/Basic.lean`,
   dropping power-of-two specifics (capacity here is an arbitrary positive u64/Nat).
3. `Svd/Ring.lean` (scope Stage 1 / P1):
   - model `physicalIndex head logical cap = (head + logical) % cap`
     (mirrors `physical_index`, vec_deque.rs);
   - extent: `0 < cap → logical < len → len ≤ cap → physicalIndex < cap`;
   - injection on `[0,len)`: `(head+a)%cap = (head+b)%cap → a < len → b < len → len ≤ cap → a = b`.
4. `Svd/Header.lean` (scope Stage 2, P4-analog):
   - predicate `HeaderValid h memSize` mirroring ALL checks in `VecDeque::init`
     (vec_deque.rs): magic/version/element bounds assumed as hypotheses; `cap = 0 →
     len = 0 ∧ head = 0`; `cap > 0 → len ≤ cap ∧ head < cap`; `len = 0 → head = 0`;
     `64 + cap * slotSize ≤ memSize`;
   - `initialHeader_valid`: the fresh header written by `new` (len=0, cap=0, head=0)
     satisfies `HeaderValid`.
5. Abstract layer `Svd/Abs/{State,Transfer,Ops,Preserve}.lean` (scope Stage 3):
   - `State.lean`: abstract state = function `slots : Nat → Option α` (unused slots
     arbitrary, A5-analog) plus `len/head/cap`; define `Inv`;
   - `Transfer.lean`: transfer principle between concrete slot contents and abstract
     `content' p = slots ((head + p) % cap)` for `p < len`;
   - `Ops.lean`: List-spec contracts for get/set/pushBack/pushFront/popBack/popFront
     (pop* reset head to 0 when the deque becomes empty; pushFront uses
     `(head + cap - 1) % cap` — cite exact source lines);
   - `Preserve.lean`: `Inv` preservation for each op.
6. Root `Svd.lean`: imports + `#print axioms` lines.
7. `formal/SCOPE.md`: mode (standing audit fixture, re-checked with `lake build`),
   location/form, method, component breakdown, properties verified (Stages 1–3),
   assumption list (A3 fail-closed for unreachable `saturating_mul` wrap; A5 unused-slot
   arbitrariness; element encoding out of scope), and explicit status: **Stage 4 (grow
   linearization via GCD cycle rotation) and Stage 5 (failure atomicity) are planned
   follow-up, NOT yet attempted**.
8. `formal/REPORT.md`: transcription findings (rotation permutes ALL cap slots incl.
   unused; `cap.saturating_mul(2)` saturation followed by non-saturating
   `DATA_OFFSET + new_cap * slot` would release-wrap — unreachable, declared under A3;
   `init`'s `len == 0 && head != 0` check partially redundant with the `cap == 0` branch,
   harmless; `push_front`/`pop_*` head-reset behaviors with citations).

Explicitly OUT of scope this round: grow linearization proof (GCD cycle rotation),
failure atomicity, `slot.rs` byte encoding/length prefixes, Memory page mechanics,
Iter plumbing, perf claims.

## Modeling facts already established

See the scope note's "Key modeling facts found" section — they are authoritative here.

## Verification loop

```
cd crates/ic-stable-vec-deque/formal && lake build
```

Must end clean: no errors, no warnings, and the `#print axioms` output must show
no `sorryAx`.

## Progress

- 2026-08-26: Scaffold + Stages 1–3 delivered, sorry-free, `lake build` green.
  - `formal/lean-toolchain`, `formal/lakefile.lean`, manifest generated.
  - `Svd/Basic.lean`: modular foundations (`add_mul_mod`) adapted from Lhm,
    power-of-two specifics dropped.
  - `Svd/Ring.lean`: P1 extent (`physicalIndex_lt_cap`, `_in_extent`) and
    injection on `[0,len)` for arbitrary positive capacity
    (`physicalIndex_injective_of_lt_cap`, `physicalIndex_injective_on_window`,
    helper `eq_or_diff_mul_of_mod_eq`).
  - `Svd/Header.lean`: `HeaderValid` as single-constructor Prop structure mirroring
    all `init` arithmetic checks; `initialHeader_valid` from `new`.
  - `Svd/Abs/State.lean`: `DequeState`, `Inv` = {cap>0, len≤cap, head<cap,
    windowed occupancy (A5)}, `updSlot`, `contentOf`.
  - `Svd/Abs/Transfer.lean`: core-only list toolkit (`readAt`/`flatRead`/`putAt`),
    observable reading list (`contentUpTo`/`logicalList`), transfer principle
    (`logicalList_eq_of_contentOf`).
  - `Svd/Abs/Ops.lean`: op models with line citations + list-spec contracts for
    get/set/pushBack/pushFront/popBack/popFront; pop bodies split into
    nonempty helpers to keep the empty/head-reset branches faithful.
  - `Svd/Abs/Preserve.lean`: `inv_set` (requires Rust's `assert!(index < len)`),
    `inv_pushBack` / `inv_pushFront` (under `len < cap`, grow excluded),
    `inv_popBack` / `inv_popFront` (incl. head-reset-on-empty and dead `cap > 1`
    branch).
  - `Svd.lean`: imports + 16 `#print axioms` guards; dependencies limited to
    propext / Quot.sound / Classical.choice — no `sorryAx`.
  - `SCOPE.md`, `REPORT.md` written; Stage 4 (grow linearization) and Stage 5
    (failure atomicity) explicitly deferred.
- 2026-08-26 (same session): boundedness review. Pushes are O(1) amortized but
  worst-case O(len × slot) + size-proportional memory grow — documented in
  lib.rs/README, confirmed against grow_if_full/memory.rs. Drafted [ADR 0085]
  (rotation-free doubling), then **rejected it by erratum before implementation**:
  the zero-movement claim conflated a wrapped element's destination with its
  current location — relocating wrapped elements on capacity growth is
  structurally unavoidable for a contiguous ring.
- 2026-08-26: ADR 0086 accepted & implemented (block-ring V1 layout). Formal
  layer restatement **in progress**: `Svd/Basic.lean` rewritten (block-routing
  arithmetic: div/mod decomposition uniqueness, mod-window injectivity,
  add-cancel injectivity, rotated-directory injectivity) — green.
  `Svd/Abs/State.lean` rewritten (`DequeState` with blocks/dir/dirSlots/
  numBlocks/blockSlots/headOff/virtCap/len/free; two-level `routeBlock` /
  `routeSlot` / `contentOf`; `updBlock`; new `Inv` bundle with dirInj,
  freeDisj, occupied) — green. `Svd/Abs/Transfer.lean` reading-list toolkit
  carried over unchanged (layout-independent) — green. `Svd/Abs/Ops.lean`
  operation defs rewritten faithfully (incl. retireTop, wrapped drained guard)
  plus `routed_pair_inj(_len)` — green so far. REMAINING: Ops spec theorems
  (get/set/push×2/pop×2), `Svd/Abs/Grow.lean` (opGrow three-regime preservation
  + inv_grow), `Svd/Abs/Preserve.lean` rewrite, `Svd.lean` axiom-guard update,
  SCOPE.md stage restatement. NOTE: `lake build` is intentionally red mid-
  migration (old Abs/Preserve + old Svd.lean reference the superseded
  contiguous model).
- 2026-08-26 (same session): ADR 0086 **implemented** in the vecdeque-impl pane
  (`src/` consolidated engine, header grown to 128 B with dedicated u64 fields,
  block-ring + directory + intrusive free list, rotation deleted). Independently
  verified from the mgmt pane: `cargo test -p ic-stable-vec-deque` green
  (10 unit incl. wrapped-growth / recycling / directory-doubling / empty-reset /
  init-round-trip tests + 14 doctests), clippy `-D warnings` clean, fmt clean,
  dependents (`ic-stable-lara`, `text-canister`) compile. Four as-built
  deviations (directory rotation, one-block boundary migration, base-address
  free list, end-block retirement) are folded into ADR 0086 §As built.
  Formal/SCOPE.md now marks stages 1–3 as verifying superseded code; stage
  restatement for the block ring is the next major formal task.

## Completion signal

When everything above is done and `lake build` is clean, print this exact line as the
last line of your reply, followed by a short file-list summary:

SVD-FORMAL-BRIEF-COMPLETE
