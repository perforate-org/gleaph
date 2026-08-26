# 0085. ic-stable-vec-deque: rotation-free doubling for bounded per-call growth

Date: 2026-08-26
Status: rejected (erratum below — core claim was mathematically wrong)
Last revised: 2026-08-26

> **ERRATUM (same day, before implementation).** This ADR is rejected by its own
> author. The central claim — that exact doubling with `head` preserved maps
> every occupied slot to itself or to `slot + old_capacity`, allowing growth with
> zero element movement — conflated the *destination* of a wrapped element with
> its *current location*. Counterexample: `C = 4, head = 2, len = 3` places
> elements at physical slots `[2, 3, 0]` (logical index 2 wrapped). After
> doubling, the routing formula `(head + i) % 8` sends logical index 2 to slot
> **4**, but the element sits in slot **0**. Without copying, `get(2)` reads
> garbage. Relocating wrapped elements on capacity growth is structurally
> unavoidable for any single-contiguous-region ring; the GCD cycle rotation is
> exactly the collision-safe way to perform that relocation. The genuinely
> bounded alternatives are a segmented/chunked layout (a V2 change) or accepting
> the documented amortized behavior with an effective-size limit. See the
> follow-up discussion; the body below is retained unmodified as a record.

## Context

IC calls execute under per-message instruction limits and pay cycles for stable
memory operations. A data structure whose single-operation cost grows with the
size of the stored data therefore has a hard maximum working size: beyond it, the
operation cannot complete within one call and traps instead of degrading.

`crates/ic-stable-vec-deque` is a V1 ring buffer over contiguous stable memory.
Logical index `i ∈ [0, len)` maps to physical slot `(head + i) % capacity`
(vec_deque.rs L355-L359). When `len == capacity`, `grow_if_full` (L298-L353):

1. computes `new_cap = cap.saturating_mul(2).max(len.saturating_add(1))` (L309);
2. grows stable memory to `DATA_OFFSET + new_cap * slot_size` (L311-L312);
3. when `head != 0`, performs a GCD cycle rotation that reads and rewrites **every
   physical slot exactly once** (L319-L349), each move being a full
   decode/encode of `slot_size` bytes;
4. sets `head := 0` (L350) and `capacity := new_cap` (L351).

The crate documents this honestly (`src/lib.rs` L10-L13, README L9): pushes are
O(1) amortized, growth is O(len) plus stable-memory growth.

## Problem

A single `push_back`/`push_front` on a full ring costs:

- **element I/O proportional to the whole deque**: the rotation moves `cap`
  slots × `slot_size` bytes each (only skipped while `head == 0`);
- **a memory grow whose new-page count is proportional to the current data
  region** (`diff_pages = ceil(need − size)/64KiB`, memory.rs L102-L107; with
  doubling, `need` doubles, so the delta doubles every time);
- header writes and one element encode: O(slot_size).

The first term is pure CPU instructions scaling with stored data; the second
scales cycles with stored data. Past a size determined by the per-call budget,
the doubling push cannot complete — and because the cost spikes exactly at
doublings, the deque becomes permanently unpushable at that size rather than
degrading gracefully. All other operations (`get`, `set`, `pop_back`,
`pop_front`) are already bounded by one slot's `max_size`.

## Existing architecture assessment

The load-bearing invariant of V1 is purely the routing formula plus the validated
header (`init`: `len ≤ cap ∧ head < cap`, L212-L214; memory coverage,
L220-L226). Every read and write path routes through `physical_index`
(get L385, set L410, iter L561, push/pop compute their own routes at
L437/L466/L493/L528); **nothing depends on physical slots `0..len` holding
elements in logical order**.

That observation dissolves the reason for rotation. Under an exact doubling
`C' = 2C` with `head` kept at `H` (both reachable values: `len ≤ cap` makes
`len + 1 ≤ 2·cap`, so `new_cap = 2·cap` except u64 saturation, which is
unreachable per scope assumption A3; `cap == 0 → new_cap = 1` keeps `head = 0`):

- a non-wrapped element (`head + i < C`) sits at `p = head + i < C`; the new
  route `(head + i) % C' = head + i = p` — **it stays put**;
- a wrapped element (`head + i ≥ C`) sits at `p = head + i − C`; the new route
  is `head + i = p + C ∈ [C, 2C)` — **it shifts up by exactly the old capacity**;
- the two destination ranges `[H, C)` and `[C, 2C)` are disjoint, so no element
  collides with another, and grown stable memory covers every destination
  because stable memory extends contiguously — old slots remain valid in place.

So for the only growth step the implementation can actually take, the ring
mapping survives with **zero element movement**: growing is
`grow_memory_to_at_least_bytes(...)` followed by `set_capacity(new_cap)`. The
rotation block exists to restore a canonical form (slots `0..len` in logical
order, `head = 0`) that no code path consumes. The existing architecture can
absorb the fix by deleting code, not by adding a concept.

## Alternatives

### A. Rotation-free doubling (recommended)

Delete the GCD cycle rotation and the `set_head(0)`; keep `head` unchanged.
Growth becomes: grow memory, then persist `capacity := 2·cap`.

- Benefits: removes the O(len × slot_size) instruction spike entirely; deletes
  ~30 lines of the hardest-to-verify code; collapses the planned stage-4 Lean
  obligation from a GCD cycle argument to an identity-plus-shift lemma.
- Drawbacks: stale bytes persist longer in never-reread slots (already covered
  by scope assumption A5); the canonical "linearized" form disappears (nothing
  consumes it).
- Complexity impact: negative (code shrinks). Boundary impact: none — V1 header
  fields, `init` checks, and public API unchanged.

### B. Fixed-increment (page-budgeted) growth

Replace doubling with `new_cap = cap + K` for a constant slot count `K` sized to
a page budget, making even the memory-grow term O(1) per call.

- Drawback discovered during evaluation: the no-movement argument of A is specific
  to `C' = 2C`. For `C' ≠ 2C`, a wrapped element's destination
  `(head + i) % C'` is neither `p` nor `p + C`, so elements must physically move
  again — reintroducing either O(len) copying or a two-generation migration
  cursor (a V2 header change).
- Verdict: incompatible with A's zero-movement property; only viable together
  with alternative C. Rejected for now.

### C. Incremental migration, keeping linearization (V2)

Keep rotation semantics but spread it: two-generation layout with a migration
cursor, migrating K slots per operation.

- Benefits: preserves canonical layout; strictly bounded per-op work.
- Drawbacks: header/layout version bump, migration state machine, far more
  complex invariants and tests — disproportionate while alternative A already
  removes the dominant scaling term.
- Verdict: rejected unless measured evidence later shows the remaining
  memory-grow term breaches per-call budgets at realistic sizes.

### D. Documentation-only mitigation

Document a safe maximum length and require callers to shard.

- Verdict: insufficient as the sole response (the failure mode is a trap, not an
  error), but the effective-size table belongs in the README regardless.

## Decision

Adopt alternative A:

1. In `grow_if_full`, delete the GCD cycle rotation (L319-L349) and the
   `set_head(0)` (L350). Growth reduces to: grow memory to
   `DATA_OFFSET + new_cap * slot_size`, then `set_capacity(new_cap)`; `head` is
   left untouched.
2. Keep `new_cap = cap == 0 ? 1 : cap.saturating_mul(2)` (the `.max(len + 1)`
   term is implied dead by the maintained invariant `len ≤ cap` but is retained
   or dropped as a cleanup detail during implementation; saturation remains
   covered by scope assumption A3).
3. No header, layout-version, API, or persisted-data change. Data written by the
   current code remains valid (`init` enforces `head < capacity`, which holds),
   and vice versa: the routing formula is identical, and unused/stale slots were
   never assumed clean.
4. Defer alternative B/C decisions until a measurement task compares the
   remaining scaling term (one `stable64_grow` of ≤ ⌈old-data-bytes / 64KiB⌉ + 1
   pages) against current IC per-call pricing at the intended deployment sizes.

## Consequences

Positive:

- Every deque operation's instruction work becomes bounded by O(max entry size)
  plus header I/O; the only residual scaling term is the memory-grow call's
  cycle cost, which no longer involves touching element bytes.
- ~30 lines including the GCD cycle walker are deleted; the hardest future proof
  obligation (stage 4) shrinks to a short algebraic lemma.
- The empty-head discipline (`pop_*` reset, `push_front` from empty leaves
  `head = cap − 1`) is unchanged; all stage-1–3 Lean results remain valid
  verbatim, since they quantify over arbitrary `head < cap` already.

Negative / accepted:

- After growth, physical order no longer equals logical order even immediately
  post-grow; any future feature wanting that canonical form must rebuild it
  explicitly.
- Stale element images survive longer in unwritten slots (A5 already declares
  them arbitrary; defense-in-depth zeroization is deliberately not added).

## Trade-offs

Canonical-layout simplicity is traded for bounded per-call work. The deleted
rotation was the only component whose cost scaled with total stored bytes in
CPU instructions; what remains scales in cycles through page allocation only.

## Migration

None. The on-disk format is byte-compatible in both directions: `magic`,
`version`, field offsets, `init` checks, and the routing formula are untouched.
Existing stable regions open unchanged under the new code, and regions written
by the new code open unchanged under the old code.

## Design documentation impact

- `src/lib.rs` and `README.md` complexity notes: drop "linearizes the ring";
  describe rotation-free doubling and the residual grow term.
- `formal/SCOPE.md`: stage 4 redefined as "growth mapping": prove that under
  head-preserving doubling the routed reading list is preserved with zero slot
  movement (`contentOf` invariance lemma) and that the moved-slot count is 0;
  stage numbering otherwise unchanged.
- `formal/REPORT.md`: findings 1 and 9 gain a pointer to this ADR (finding 1's
  "permutes all slots" behavior is what the ADR removes).
- `.agents/briefs/svd-formal-brief.md`: stage 4 deliverable replaced by the
  growth-mapping proof once the Rust change lands.
