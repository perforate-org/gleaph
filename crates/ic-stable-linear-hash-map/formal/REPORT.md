# Verification Report — ic-stable-linear-hash-map, Stage A (routing + control invariants)

Anchor timestamp: 2026-08-24 16:33:21 UTC +0000
Target revision: git `0da342d62b2a3c3b293fa7ff5ed21b9f577dd23d`
(`crates/ic-stable-linear-hash-map/` clean at this revision)
Mode: audit of an existing implementation, run as a permanent fixture (see SCOPE.md)

Post-audit remediation (anchored 2026-08-24 21:31:38 UTC +0000): findings 1 and 2
were fixed by consolidating the level/cursor derivation into control.rs
`derive_geometry`. Line citations for the affected functions were refreshed to the
post-remediation source; all other citations still match revision `0da342d62`
(map.rs line numbers are unchanged by the remediation).

## How to run

```
cd crates/ic-stable-linear-hash-map/formal
lake build
```

`lake build` re-checks every proof. The root module prints `#print axioms` for each
headline theorem; a regression to `sorry` appears as `sorryAx` in that output. The
project uses Lean core only (`leanprover/lean4:v4.33.1`, no Mathlib) and is invisible
to Cargo.

## Scope

Stage A of the staged roadmap in SCOPE.md: routing mathematics and control-region
invariants (properties P1–P5). Stages 3–5 (logical map spec, split preservation,
epoch fencing) are planned follow-up work and are not claimed here.

## Method

Hand-written Lean transcription of the referenced Rust functions, each citing its
source file and line range at the target revision, followed by proofs of the P1–P5
statements over those transcriptions. The Rust→Lean generators (Aeneas/Charon, hax)
were evaluated and not used: the crate's load-bearing paths are impure (`Memory`
trait I/O, thread-local scratch, external rapidhash), so automated extraction would
cover the wrong layer. Revisit if a pure-function cross-check is ever wanted.

## Verified results

| Property | Lean theorem | Mirrors |
|---|---|---|
| P1 route extent | `route_lt_base_plus_cursor` | map.rs L1963-L1971 `linear_bucket` |
| P2a split stability (level step) | `split_stability_level_up` | geometry transitions of map.rs L1693-L1709 |
| P2b split stability (cursor step) | `split_stability_cursor_adv` | geometry transitions of map.rs L1693-L1709 |
| P3 geometry-step validity | `next_geometry_shape` (+ `next_geometry_from_valid`) | map.rs L1693-L1709 `next_geometry`, L1719-L1721 `base_buckets` |
| P4 initial control validity | `initialControl_valid`, invariant set `ValidControl` | map.rs L401-L415 `create`, L1047-L1068 `validate_control`, header.rs L45-L56 |
| P5 threshold bounds | `split_threshold_mono`, `split_threshold_le_capacity`, `split_threshold_lt_capacity` | map.rs L1711-L1717 `split_threshold` |
| Corollary | `route_in_extent`: routed buckets < `physical_buckets` under `ValidControl` | composes P1 with P4 |

Notable strengthening discovered during proof work: P1 holds **unconditionally** — no
`cursor < 2^level` hypothesis is needed for the bound itself. That hypothesis is only
required to identify `2^level + cursor` with the persisted `physical_buckets`, which
is exactly what `ValidControl` provides.

## Assumption list (see SCOPE.md for full statements)

- A1 hash opacity — rapidhash is uninterpreted; P1–P5 hold for arbitrary hashes. No
  collision-resistance assumption was introduced.
- A2 sequential execution — IC canister message model.
- A3 arithmetic domain — naturals constrained to the u64 ranges the code enforces;
  Rust checked-overflow errors are modeled as explicit fail-closed variants
  (documented on `nextGeometry` / `splitThreshold`). Divergence direction matches:
  both fail closed near the u64 ceiling.
- A4 byte-layer modeling deferred to stage 3+; no byte-layout claims are made here.

No new axioms were introduced. All headline theorems depend only on Lean's standard
`propext` / `Quot.sound`; none depend on `sorryAx` or `Classical.choice`.

## `sorry` list

None. Stage A proofs are complete.

## Findings

Severity scale: Critical / High / Medium / Low / Info.

1. **[Low] Duplicated level/cursor derivation — three sites must stay in sync. Remediated.**
   At the audited revision, `control.rs` `decode`, `control.rs` `read_hot_with_epoch`,
   and `map.rs` `scrub_control` each independently re-derive
   `(level, split_cursor)` from `physical_buckets`. They agree today (the Lean model's
   single derivation is exactly what all three compute), but this is a
   single-source-of-truth gap inside the crate: a future edit to one site can silently
   desynchronize routing. Remediated on 2026-08-24 (21:31 UTC): all three sites now call
   one `fn derive_geometry(physical_buckets) -> (u8, u64)` at control.rs L48-L57;
   current anchors: `decode` control.rs L59-L73, `read_hot_with_epoch` control.rs
   L96-L116, `scrub_control` map.rs L1153-L1166.
2. **[Info] `read_hot_with_epoch` lacks the zero guard its sibling has. Remediated.**
   At the audited revision, `read_hot_with_epoch` computed `level = 63 -
   leading_zeros(pb)` and subtracted `1 << level` without checking; only `decode`
   special-cased `pb == 0`. With `pb == 0` the subtraction `63 - 64` underflows
   (debug panic; release wrap would yield a garbage level into a shift). Unreachable
   through public APIs because `open` → `validate_control` rejects
   `physical_buckets < INITIAL_BUCKETS = 8`, and the handle cannot observe `pb == 0`
   afterwards. Remediated on 2026-08-24 (21:31 UTC) together with finding 1:
   `derive_geometry` guards `pb == 0` once and degrades it to the empty geometry
   `(0, 0)` without trapping or wrapping; `read_hot_with_epoch` (control.rs L96-L116)
   routes through it. A unit test pins the contract, including the degraded input.
3. **[Info] Shift-safety obligation for `linear_bucket` is satisfied but implicit.**
   `hash & ((mask << 1) | 1)` requires `level < 64`. The chain that guarantees it
   (open validates `level < 63`; `next_geometry` caps increments below 63; hot-path
   levels are re-derived from validated `pb < 2^63`) is correct but spread across
   three files. The Lean side now states the obligation explicitly on `linearBucket`.
4. **[Info] `reset`/`clear` leave stale bytes beyond the initial extent.**
   map.rs L706-L724 / L766-L784 zero only the first `INITIAL_BUCKETS` blocks. Bytes
   written by earlier growth remain until reused. Safe today because a later
   `apply_split` writes complete block images before publishing the larger geometry
   (map.rs L1662-L1677), so stale bytes are never readable under a published control.
   Stage 5 should encode this write-before-publish ordering explicitly.
5. **[Info] `insert` retry-loop termination is informal.**
   map.rs L886-L964: after a maintenance split the loop re-reads control and retries.
   Termination relies on `physical_buckets` increasing monotonically toward the
   geometry cap where `next_geometry` fails closed. True, but worth a stage-5 lemma
   (progress measure) rather than prose.

## Conclusion

Stage A verifies cleanly: the linear-hashing routing mathematics and the
control-region contract enforced at open are internally consistent, and the routing
extent guarantee holds for arbitrary hash values under the documented invariant. The
audit produced no correctness defect in the verified scope; findings 1–2 are cheap
hygiene improvements (both applied post-audit, see above), and findings 3–5 define
the proof obligations that stages 3–5 must formalize next.

Per SCOPE.md, next stages: logical map specification (stage 3), split preservation
(stage 4), epoch fencing / failure atomicity (stage 5).
