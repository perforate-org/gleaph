# 0086. ic-stable-vec-deque: segmented block-ring layout for bounded per-call operations

Date: 2026-08-26
Status: accepted (implemented same day; see "As built" amendments)
Last revised: 2026-08-26

Supersedes the direction explored in [ADR 0085] (rejected by erratum: relocating
wrapped elements on capacity growth is structurally unavoidable for a single
contiguous ring).

## Context

IC calls execute under per-message instruction limits and pay cycles for stable
memory. The current contiguous ring has bounded `get`/`set`/`pop_*`
(O(one slot)), but a full-ring push costs O(len × slot_size) of element I/O
(GCD cycle rotation, vec_deque.rs L319-L349) plus a memory grow whose page delta
scales with the current data region (memory.rs L102-L107). Past a size set by
the per-call budget, the doubling push cannot complete.

Gleaph is pre-production: the crate has never been deployed, and repo policy
requires fresh state when the canonical layout changes, with superseded paths
deleted. Migration design is explicitly out of scope.

## Problem

Per-operation work must be bounded by a constant that does not scale with
`len` — ideally O(one entry encode/decode) plus a constant amount of
metadata/growth I/O — so that no deque size leads to an uncompletable call.

## Existing architecture assessment

The routing formula `(head + i) % capacity` forces every storage byte into one
contiguous region; growing that region across its old boundary is what makes
element relocation unavoidable (ADR 0085 erratum). No existing abstraction
inside the crate can remove the relocation while keeping the single-region
layout; what must change is the storage organization itself. Everything above
the raw layout — public API shape, `Storable` bounds checks, the list-spec
contracts of the formal layer — is layout-independent and is preserved.

## Alternatives

- **A. Keep the contiguous ring, document an effective-size limit** — minimal
  effort, but leaves the trap-by-spiking failure mode as a permanent
  operational constraint.
- **B. Rotation-free doubling** — rejected by erratum (ADR 0085); impossible for
  a contiguous ring.
- **C. Incremental migration keeping linearization** — dropped: pre-production,
  and it exists only to serve compatibility this ADR does not need.
- **D. Segmented block-ring (this ADR)** — strictly bounded per-op work;
  replaces the layout outright.

## Decision

Replace the contiguous ring with a **block-ring**: elements live in fixed-size
blocks; a directory maps block positions to physical bases; both ends grow by
consuming blocks, never by relocating elements.

### Layout

Header grows to a **128-byte prefix** (`DATA_OFFSET := 128`); magic remains
`SVD`; **`LAYOUT_VERSION` remains `1`**. The crate has never been deployed, so
there is no prior generation to gate: version 1 denotes this block-ring layout
from the start, and `init`'s existing checks (bad magic, wrong version,
malformed geometry) keep their defensive role unchanged. The old 64-byte
header mirrored `ic_stable_structures::vec::Vec`; that prefix parity carried no
interop weight (magic/version already distinguish regions) and is dropped in
favor of dedicated, uniformly `u64` fields — no repurposing, no sub-word
packing:

| field | purpose |
|---|---|
| magic, version (= 1) | unchanged |
| len | unchanged |
| max_size, is_fixed_size | unchanged |
| headOff | virtual position of logical index 0 |
| virtCap | virtual capacity `numBlocks · B` |
| dirBase | byte offset of the directory |
| dirSlots | directory capacity in entries |
| numBlocks | blocks tracked by the directory |
| blockSlots | `B`: slots per block, fixed at creation |
| freeHead | intrusive free-block list head (`u64::MAX` = nil) |
| reserved (55 B) | future use |

Total: 73 of 128 bytes used; 55 bytes stay reserved for future fields.

### Operations

Element `i` lives at virtual position `r = (headOff + i) mod V`, `V =
numBlocks·B`, i.e. block `k = r / B`, slot `k' = r % B`, address
`dir[k] + k'·slot_size`. All addressing is arithmetic over persisted fields —
no heap structures.

- `push_back` / `push_front`: if `len == V`, obtain one new block — from the
  free list if non-empty, else allocate `B·slot_size` fresh bytes at the
  current end of stable memory (page-aligned via `grow`) and append its base to
  the directory (doubling the directory first if full). Then write the element
  at the tail/head virtual position and adjust `len` / `headOff`. Cost: one
  element write + O(1) metadata + at most one constant-sized block allocation.
- `pop_back` / `pop_front`: read the element; when the block at the consumed
  end drains, push its block index onto the intrusive free list. Cost:
  O(one slot) + O(1).
- `get` / `set`: routed read/write of one slot, as today.
- Empty convention: on transition to empty, `headOff := 0` (deterministic;
  mirrors the current behavior of `head = 0` when empty).

Initial parameters: `B = max(1, TARGET_BLOCK_BYTES / slot_size)` with
`TARGET_BLOCK_BYTES = 256 KiB` (tunable constants), `dirSlots = 16`, doubling
on demand.

### Boundedness statement

Every operation performs ≤ one element encode/decode plus O(64 bytes) of
metadata writes, plus — on a push into a full structure — exactly one block
allocation of `B·slot_size` bytes and at most one directory copy of
`8·dirSlots ≤ 8·(len/B + 1)` bytes. Both terms are constants for a fixed `T`
and a fixed moment in time and are documented as such; they do not grow within
a single call beyond those envelopes.

### Superseded code paths (deleted)

The contiguous-ring storage engine — single-capacity region math
(`physical_index` over one capacity), the GCD cycle rotation, and the header
field usage tied to them — is deleted rather than kept behind a flag. The
crate ships one canonical layout from this ADR onward; `init` continues to
reject bad magic, wrong `version`, and malformed geometry exactly as before,
which is defensive validation, not legacy support.

## Consequences

Positive:

- Strictly bounded per-call work for all six operations; no trap-by-growth
  failure mode.
- No rotation/relocation code at all; the hardest planned formal obligation
  (GCD cycle linearization) disappears instead of being proven.
- Steady-state memory footprint tracks peak occupancy via block recycling,
  matching the reuse behavior the contiguous ring provided implicitly.

Negative / accepted:

- Directory copies at doubling moments cost `O(8·numBlocks)` bytes once per
  doubling — divided by block size relative to V1's whole-payload rotation and
  documented; eliminating them entirely would need a tree directory
  (rejected as premature).
- Partially filled head/tail blocks waste up to `2·B·slot_size − 2·entry`
  bytes versus a perfectly packed contiguous region.
- One more indirection per access (directory load) versus the flat formula.

## Trade-offs

Layout sophistication (block allocator + directory) is traded for the removal
of both the unbounded spike and all element-relocation code paths. Complexity
moves from proof-hard algorithmics (cycle rotation) into routine bookkeeping
with local invariants.

## Migration

None — and none is needed: nothing is deployed. The version byte keeps the
value 1 because it never denoted anything else; there are no old regions to
convert, reject, or read. Fresh state follows ordinary usage (`new`, or first
use on empty memory).

## Design documentation impact

- `src/lib.rs`, crate docs, `README.md`: describe the block-ring layout and
  bounded per-op costs; drop "linearizes" language; the V1 ASCII layout diagram
  and the "same 64-byte header prefix as `ic_stable_structures::vec::Vec`"
  claim are replaced by the 128-byte block-ring header.
- `formal/SCOPE.md` + stage plan restated for the block-ring layout (see
  below); `REPORT.md` findings remain as a record of the superseded contiguous
  implementation, annotated.
- `.agents/briefs/svd-formal-brief.md`: replaced by a block-ring brief once
  this ADR is accepted.

## Formal-layer impact (planning)

- List-level op contracts from stages 3 (observable reading-list
  transformations) carry over: they are layout-independent.
- Stage 1 becomes block-routing arithmetic (`r = B·k + k'` decomposition, mod
  over `V`); stage 2 becomes block-ring header/fresh-header validity (with
  `DATA_OFFSET` moving from 64 to 128, `HeaderValid.memFit` becomes
  `128 + virtCap·slotSize ≤ memSize`); `Inv` gains directory/free-list
  well-formedness conjuncts.
- Stage 4 becomes: appending one block preserves every existing routed reading
  verbatim (virtual positions of live elements are unchanged because `headOff`
  and existing directory entries are untouched) — the zero-movement property,
  now true by construction.

## As built (amendments, 2026-08-26)

Implementation landed in `crates/ic-stable-vec-deque` (single-engine
`src/vec_deque.rs`, contiguous-ring code deleted; `cargo test -p
ic-stable-vec-deque` 10 unit + 14 doctests green, clippy `-D warnings`
clean, dependents `ic-stable-lara` / `text-canister` compile). Four
deviations from the text above are accepted as part of the design:

1. **Directory rotation on growth.** Because routing depends on `virtCap`,
   appending a new block for a wrapped window requires rotating the directory
   entries by `headOff / B` first; otherwise any capacity change would corrupt
   wrapped windows. The ADR's "append at the tail" is this rotate-then-append.
2. **Bounded boundary migration.** After rotation, up to one block's worth of
   slots (`headOff % B`) migrates from post-rotation `dir[0]` into the newly
   acquired block. Relocation stays bounded by construction — the O(len) spike
   this ADR set out to remove remains removed.
3. **Free list stores base addresses**, not block indices, so directory
   rotation cannot invalidate free-list links.
4. **Retirement policy:** only the block containing the popped element drains
   to the free list when it empties and it is the current end block
   (`headOff + len` crosses the top boundary exactly when the last element in
   the top block is popped); interior drained blocks keep their directory entry
   and are reclaimed by wraparound. This preserves the O(one slot) pop bound.
   Growth with `headOff == 0` appends an empty top block, so the previous top
   retires only after the window re-enters and re-drains it — correctness-
   neutral capacity tracking. An earlier retirement criterion was fixed after
   a differential stress test caught a full wrapped window (`N = 3, B = 4,
   headOff = 9`) retiring its top block while elements still lived there;
   the shipped criterion retires iff the popped element is in the top block
   and `headOff + newLen ≤ (numBlocks − 1)·B` (or `newLen == 0`).
