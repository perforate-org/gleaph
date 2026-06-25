# 0033. Vector rebuild candidate pool storage and rebuild-state read cost

Date: 2026-06-25
Status: accepted (implementation deferred)
Last revised: 2026-06-25

> **Summary.** This ADR investigates how to cut the remaining serialization cost of the bounded vector
> rebuild after ADR 0031 Slice 7/8. The tempting fix — change how the `Sampling`/`Training` candidate
> pool is stored (a contiguous blob inside the record, or a dedicated raw slab region) — was measured
> and **rejected**: it does not address the demonstrated cost. The dominant cost is **re-reading the
> whole rebuild-state value (which embeds the candidate pool) out of stable memory on every bounded
> step**, not the per-element Candid decode and not the persist. The effective lever is a **transient
> heap memoization of the rebuild-state record** (stable memory remains the source of truth), which a
> throwaway probe showed removes ~99% of the read cost for a ~20-25% full-rebuild reduction with **no
> storage-layout change**. Implementation is deferred to a follow-up slice.

## Context

ADR 0031 makes the `graph-vector-index` canister a derived candidate-generation structure with a
bounded, resumable rebuild lifecycle persisted in `VECTOR_REBUILD_STATE` (MemoryId 12):
`Idle -> Sampling -> Training -> Building -> ReadyToPublish -> Cleaning -> Idle`. The `Sampling` and
`Training` phases carry a bounded distinct **candidate pool** (`Vec<Vec<u8>>`, every element a
fixed-width `stride_bytes` vector) inside the durable `VectorRebuildStateRecord`; `Training` runs one
k-means-lite iteration per bounded `admin_vector_rebuild_step` message (ADR 0031 Slice 8).

A prior optimization (single-encode persist via `RawRebuildState`) removed a redundant Candid encode
and clone on the write path, cutting full-rebuild `canbench` instructions ~17-21%. Phase-level
`bench_scope` instrumentation then showed where the *remaining* cost sits.

## Problem

For the heaviest rebuild benchmarks, after the single-encode persist fix, the residual **serialization**
cost is dominated by `rebuild_read_state` — `rebuild_state_of` reading the durable record (which embeds
the candidate pool) from stable memory once per bounded step:

- `bench_rebuild_full_d128_nlist16`: `rebuild_read_state` = 504.88M instructions over 10 steps
  (~25% of the 1.99B total).
- `bench_rebuild_full_d768_nlist64`: `rebuild_read_state` = 3.08B over 10 steps (~20% of the 15.03B
  total).

Because `Training` is k-means over the *immutable* pool and each iteration is a separate bounded
message, the pool is read out of stable memory again on every one of the ~8 training steps. The
dominant `bench_rebuild_full_d768_nlist64` cost overall is `rebuild_training_assign` (9.57B, ~64%),
which is inherent k-means L2 work (`candidate_count * nlist * dims`) and out of scope here.

## Existing Architecture Assessment

The Vector Index domain already owns the rebuild lifecycle, the candidate pool, and `VECTOR_REBUILD_STATE`.
Nothing about the *ownership* or *boundaries* is wrong. The question is purely whether a different
**storage layout** for the candidate pool would reduce the per-step read.

Two storage-layout hypotheses were evaluated against measured evidence (a throwaway prototype run
through `canbench`; results below are the prototype's change vs the committed baseline):

- **Hypothesis: the cost is per-element Candid decode of `Vec<Vec<u8>>`.** Tested by changing the pool
  to a single contiguous `Vec<u8>` blob in the record (Alternative B). Result: `rebuild_read_state`
  moved only -2.16% (d128) and was not significant (d768); totals -0.67% / -0.12% (noise). **Refuted.**
  The per-element decode is not the cost; reading the value bytes out of stable memory is.

- **Hypothesis: the cost is re-reading the pool bytes from stable each step.** Tested by a transient
  heap memo of the rebuild record (read-through + write-through), so steps after the first hit the heap
  instead of stable. Result: `rebuild_read_state` -99.14% (504.88M -> 4.32M, d128) and -99.13%
  (3.08B -> 26.79M, d768); **totals -24.95% / -20.16%**. **Confirmed.**

Therefore the demonstrated cost is the repeated stable-memory read of the rebuild value. A
storage-layout change does not remove a stable read that the rebuild still has to perform every step;
only *not re-reading* removes it.

## Alternatives

### A. Keep the current layout (minimum change)

Do nothing further. The single-encode persist fix already captured ~17-21%. No new concepts, no risk.

- Benefits: zero complexity, zero risk.
- Drawbacks: leaves the ~20-25% read-side cost on the table.

### B. Contiguous-blob candidate pool in the record (moderate change)

Store the pool as one `Vec<u8>` (`count = len / stride_bytes`) instead of `Vec<Vec<u8>>`, still inside
`VectorRebuildStateRecord`. No new region, no MemoryId, transient/derived so no migration.

- Measured effect: `rebuild_read_state` -2.16% (d128), not significant (d768); total ~0. The blob also
  made `rebuild_sampling_dedup` modestly slower (hashing fixed-width slices).
- Verdict: **does not address the cost.** Rejected by measurement.

### C. Dedicated raw fixed-stride candidate region (large change)

Move the pool out of the record into a new raw slab region (e.g. MemoryId 14), analogous to ADR 0032's
`VECTOR_ROW_SLAB`. `rebuild_state_of` would then read only the small lifecycle record (cheap), and
`Training` would read candidates directly from the region.

- Analysis: this *relocates* rather than *eliminates* the per-step pool read. `Training` still needs
  the entire immutable pool each iteration, so it reads `~pool_bytes` from the region every step — the
  same stable bytes that `read_state` reads today, just under a different scope. Net is ~neutral unless
  combined with memoization of the pool across steps. A raw slab read may be marginally cheaper than a
  chunked `BTreeMap` unbounded-value read, but that delta is unverified and small relative to the cost.
- Cost: new stable region, `VECTOR_INDEX_STABLE_LAYOUT` baseline bump (0-13 -> 0-14), fail-closed reopen
  validation, and explicit teardown of the region on `Training -> Building`, `Sampling -> Failed`, and
  abort (today abort from `Sampling`/`Training` is O(1) to `Idle` with "nothing durable outside the
  state row"; a separate region would break that invariant and add a teardown path).
- Verdict: **high complexity for a benefit the measurement does not support.** Rejected.

### D. Transient heap memoization of the rebuild record (the effective lever)

Add a heap (`thread_local`) read-through + write-through cache of `VectorRebuildStateRecord`, keyed by
`index_id` (at most one active rebuild per index, enforced by `RebuildAlreadyActive`). `rebuild_state_of`
returns the cached record on hit. `VECTOR_REBUILD_STATE` (stable) remains the source of truth; the heap
cache is a non-durable mirror that is empty after an upgrade and repopulated by the next read. No
storage-layout change, no MemoryId change, no persistence-format change, no migration.

- Measured effect (throwaway probe): `rebuild_read_state` -99% on both d-sizes; total -24.95% (d128) /
  -20.16% (d768). The probe also reduces the mutation-path `rebuild_state_of` reads (dual-write phase
  checks) for free.
- **Implementation requirement — no write-through misses.** This is the highest-risk part: a stale cache
  is read as if durable. There is more than one stable writer of the rebuild row. Today they are
  `put_rebuild_state` (used by start / publish / abort / cleanup transitions) **and the inline persist in
  `rebuild_step_inner`** (`crates/graph-vector-index/src/facade/store/rebuild.rs`, the per-step
  Training/Sampling/Building persist that bypasses `put_rebuild_state` to share the single
  `RawRebuildState` encode). The implementation **must route every stable write of `VECTOR_REBUILD_STATE`
  through a single cache-aware helper** (insert/remove that updates or clears the cache in lockstep) so a
  new write path cannot silently skip the cache. If the inline persist is kept separate for the
  single-encode optimization, it **must update the cache explicitly** at the same point it writes stable.
  A direct `VECTOR_REBUILD_STATE.insert/remove` outside that helper is a defect.
- Other caveats: avoid a write-time deep clone of the pool into the cache (the probe cloned, inflating
  `persist_state`; cache the encoded `RawRebuildState` bytes — decode is cheap, per Alternative B — or
  move the record into the cache and derive the bytes once); confirm coherence given that only the
  canister itself writes rebuild state (the mutation path is read-only against it); confirm upgrade
  behavior (cold cache, stable fallback).

## Decision

1. **Reject the candidate-pool storage-layout change (Alternatives B and C).** Measurement shows it does
   not address the demonstrated cost (B) or only relocates it at high structural cost (C). The candidate
   pool stays inside `VectorRebuildStateRecord`; no new stable region is introduced.

2. **Adopt transient heap memoization of the rebuild-state record (Alternative D)** as the optimization
   for the read-side cost, with stable memory remaining the single source of truth. This ADR records the
   accepted decision and the measured justification; the implementation is a separate follow-up slice
   (status `accepted (implementation deferred)`).

## Consequences

- Confirms the existing `VECTOR_REBUILD_STATE` storage layout is correct and avoids adding a stable
  region that measurement shows would not pay off (preserves the O(1) `Sampling`/`Training` abort
  invariant and the "nothing durable outside the state row" property).
- Identifies a no-storage-change path to a ~20-25% full-rebuild reduction and documents the measured
  cost structure so future work targets the right lever.
- Records that the dominant `d768` rebuild cost (`rebuild_training_assign`, k-means L2) is inherent and
  not a serialization problem.

## Trade-offs

- Memoization (D) adds a consistency surface (a heap mirror of durable state) that must be updated on
  every rebuild-state write and must tolerate being empty after an upgrade. This is acceptable because
  the cache is non-durable and the canister is the sole writer of rebuild state, but it is real
  maintenance burden and is the reason it is split into its own implementation slice rather than landed
  opportunistically.
- Choosing not to pursue B/C means the rebuild record continues to embed the pool; if memoization is not
  implemented, the read-side cost remains.

## Migration

None. `VECTOR_REBUILD_STATE` layout, MemoryId, and on-disk format are unchanged by this decision.
Memoization (when implemented) is a heap-only addition with no durable footprint.

## Design Documentation Impact

- `design/index/vector-index.md`: linked this ADR from the rebuild section (done); when memoization is
  implemented, note that `rebuild_state_of` is served from a transient heap cache backed by
  `VECTOR_REBUILD_STATE`.
- `design/storage/stable-memory-inventory.md`: fixed the pre-existing ADR 0031 Slice 7 drift in the
  MemoryId 12 entry — the lifecycle list was missing `Training`, which this ADR depends on — and noted
  that the `Sampling`/`Training` candidate pool deliberately stays inside the MemoryId 12 record (no new
  region) per this decision (done). No layout change.
- `design/adr/README.md`: index this ADR (done with this change).

## Alternatives considered

See **Alternatives** (A keep-as-is, B contiguous blob, C dedicated raw region, D heap memoization). B and
C were rejected on measured/structural grounds; D was adopted.

## Required Axes Impact

- **Encapsulation:** preserved. No internal state is exposed across an API; the (future) cache is a
  private heap detail of the Vector Index canister.
- **Separation of concerns:** preserved. The change stays entirely within the Vector Index domain that
  already owns rebuild state.
- **Invariants:** preserved. Rejecting C keeps the O(1) `Sampling`/`Training` abort and the
  "nothing durable outside the rebuild-state row" invariant. D keeps stable memory as the sole durable
  source of truth.
- **Consistency:** D introduces a derived heap mirror with a single write-through update path from the
  canonical stable record; coherence relies on the canister being the only writer of rebuild state.
- **Fitness for purpose:** the chosen fix matches the demonstrated problem (repeated stable reads)
  exactly, without over-generalizing into a new storage subsystem.
