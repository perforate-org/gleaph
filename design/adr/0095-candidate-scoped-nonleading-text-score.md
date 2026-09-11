# 0095. Candidate-scoped non-leading text_score

Date: 2026-09-09
Status: **implemented (plan 0344) — awaiting independent review**
Evidence anchor: 2026-09-10 UTC — planner/text-canister/router libs focused-green,
`non_leading_text_candidate_topk_lifecycle` PocketIC green, candidate canbench arm
2.79M instructions (denser below). Policy-denied-scorer and 100M/16MiB headroom
remain explicitly deferred (see §Residual).

## Context and problem

`crates/gql-planner/src/text_scan.rs:141–246` lowers leading top-k only;
`crates/router/src/gql_text_scan.rs:335–429` rejects non-leading scans. The required query
traverses User → Project → Document and ranks only documents reached by that traversal.
Global TEXT top-100 followed by a graph filter cannot implement this contract.

`text_score` looks like a scalar, but its existing supported uses are index-backed search
lowerings, not a total per-row function (`design/gql/extension-syntax.md`, TEXT_SCORE).
The proposed extension explicitly preserves that distinction: nonmatching/unindexed documents
produce no search hit and are excluded, not assigned zero or NULL to fill LIMIT. General scalar
placement remains unsupported. This is a deliberate, review-required language contract, not
ordinary scalar evaluation disguised as an optimization. Existing leading behavior is unchanged.

## Proposed decision

For one graph, live shard, same-subnet deployment and one top-level non-leading score:

1. Router authorizes the user plan and lowers policies through the existing path. Graph executes
   the entire candidate-producing prefix, including all policy predicates, without the user's
   ranking LIMIT. Candidate membership comes from Graph, never TEXT.
2. Graph returns an internal projection of `ELEMENT_ID(d)` and the already-authorized ordinary
   result columns, using existing plan execution and GQL row wire. Router retains row multiplicity;
   only the TEXT request's document keys are deduplicated. Do not project a whole vertex merely
   to obtain identity: that materializes labels and all properties.
3. A controller-guarded TEXT query accepts bounded candidate keys and returns **all matching
   candidate scores**, using the existing analyzer, posting/tombstone readers and score parts.
   The proposed operation is `search_candidates(query, keys) -> Result<Vec<TextHit>, String>`:
   no request record, cursor, new stable state, or ranking formula. The stored controller is the
   Router in this deployment; the guard is not the provision dictionary-relay guard.
4. Router associates scores with retained prefix rows, drops rows without a hit, sorts by score
   descending then TEXT document key ascending, preserves duplicates and applies the user's
   **row** LIMIT. Integer scores become Float64 exactly. Equal-key duplicate rows retain prefix
   order; the initial projection scope makes those rows identical.
5. Return retained projected values rather than re-fetching them. This uses two sequential remote
   calls (Graph, TEXT), avoids score re-evaluation in Router and eliminates post-ranking hydration
   loss. Any later design using a second Graph hydration must fail on missing/changed identities
   or cardinality loss; it must never label a short answer a complete top-k.

Successful results are complete over the fully obtained authorized prefix and the TEXT index's
visible flushed state. No atomic snapshot across canisters, read-your-writes, certification, or
fresh-property/score snapshot equivalence is promised. All steps are read-only; there is no
irreversible mutation or durable callback state to reconcile.

## Bounds and failure ownership

The implementation plan specifies numeric admission limits and mandatory pre-implementation
measurement gates. Router owns prefix rows and bytes; TEXT owns the distinct candidate API cap
and query validation. Over-limit, incomplete prefix, malformed/out-of-set/duplicate hits, decode,
transport, readiness and authorization errors fail the query. There is no successful partial-window
fallback, silent clamp, paging service or budget-exhaustion success.

The existing unguarded `search(query,k)` remains an acknowledged pre-existing information surface,
not proof that a more powerful candidate-membership oracle may be public. The new operation must
reject anonymous, external and dictionary-relay callers before inspecting keys. General hardening
of existing TEXT endpoints is separate; do not claim this slice makes the whole canister private.

## Existing architecture and alternatives

- **Minimum change, selected:** existing Graph projection/wire, Router orchestration and retained
  projected rows, TEXT candidate restriction. Variable projection bytes require a strict cap.
- **Moderate change:** identity-only prefix, candidate search, then seed-based Graph projection.
  Smaller prefix payload, but a third remote call, strict identity/cardinality checks and a second
  observation of canonical state. Not selected as a hidden fallback when retained bytes exceed cap.
- **Larger redesign:** Graph pauses execution at a remote scoring operator or persistent paginated
  candidate sessions. Adds orchestration/persistence boundaries without a demonstrated need.
- **Rejected:** global-top-k post-filter; ADR0092 deepening unchanged. ADR0092 intentionally counts
  authorization survival independently of prefix joins and preserves vector join multiplicity
  without a final row cap. Neither its sparsity stop nor receipt establishes this candidate set.

The TEXT kernel orders equal scores by docid, while the Router contract orders by key. Returning
all bounded candidate hits avoids losing equal-score key winners at an intermediate k boundary.
Existing scoring is `Σ(WEIGHT_BASE + capped_tf)` over deduplicated analyzed query terms
(`text-canister/src/state.rs:1549–1632`), not BM25 and not candidate-normalized statistics.

## Required axes and consequences

- Encapsulation: Graph owns traversal/projection; TEXT owns keys/postings/scoring; no property export
  for Router re-analysis and no raw docid knowledge in Graph.
- Separation: Router composes existing results; portable GQL crates receive no ICP limits or policy
  rules. Existing TextScan placement can represent the boundary without a new persistent schema.
- Invariants: complete prefix before scoring; request-only dedup; exact row LIMIT; candidate-only
  hits; explicit failure on all incomplete paths.
- Consistency: canonical Graph and flushed derived TEXT state retain their existing distinct roles;
  retained projection avoids an additional Graph read, not all cross-canister skew.
- Fitness: bounded exact candidate matching is sufficient for the concrete query; arbitrary
  projections, nested/multiple search, aggregates, DISTINCT, OFFSET, extra sort keys and fan-out
  are out of scope. The cost is materializing bounded prefix rows before LIMIT.

## Migration and documentation

No stable-layout changes, migration, compatibility decoder, new dependency or new module is planned.
The candidate query adds a callable Candid method but reuses the hit result shape. Ship compatible
Router/TEXT artifacts together; do not add a fallback for an old TEXT canister missing the method.
On implementation, synchronize `design/index/text-index.md` and
`design/gql/extension-syntax.md` (including capability tables and limitations). They are deliberately
unchanged by this proposal. ADR0034/ADR0092 vector semantics are not superseded.

## Review gates

Approve explicitly: search-hit rather than total-scalar semantics; retained projection over third-call
hydration; conservative caps and preflight measurements; stored-controller access for candidate reads.
The draft implementation checklist is `plans/0344-nonleading-text-score-candidate-topk.md` (ignored
local planning artifact, not an implemented capability). No independent approval is recorded.

## Addendum: candidate-scoped compound threshold-top-k (implemented)

The compound form — `WHERE text_score(v.prop, Q) cmp bound` plus `ORDER BY` score
`DESC LIMIT k` on one triple after a traversal prefix — fuses into a single
`TextScanMode::ThresholdTopK` barrier, reusing the leading plan 0329 mode and its wire
form with no new variant, method, or cap. Fusion lives in
`apply_candidate_text_topk_lowering` (same-triple gate: single-predicate
`PropertyFilter` + `TopK`); mismatched halves stay residual and fail closed. The Router
accepts the barrier as `CandidateBarrierMode::Compound` (one new enum variant), reapplies
the 1..=1024 row LIMIT gate, retains the threshold on the complete candidate hit set
first, then truncates ranking — the 0329 order, candidate-scoped — reporting
exact `truncated=false`. The `property_projection` vertex-binding hook is mode-agnostic
and needed no change. TEXT, wire, and caps are unchanged.

## Addendum: candidate-scoped OFFSET (implemented)

A fused `OFFSET n` in the top-k and compound forms lowers without any wire or TEXT
change (D2: trailing skip-`Limit`). The planner consumes `TopK{k, offset:n}` into a
`TextScan` carrying the checked `k + n` ranking window plus a pure
`Limit{count:None, offset:n}` parked after the late-projected RETURN (`OFFSET 0`
keeps the offset-free shape; literal-only offsets, parity with the limit). The Router
accepts the `[Project]` / `[Project, Limit]` tail, reapplies the existing 1..=1024
gate to the `k + n` window (no cap constant copied into the planner), and skips after
ranking on the fully ordered rows — exact `truncated=false`, past-end skip yields the
exact empty set. A skip after a threshold-only barrier, `OFFSET` without `LIMIT`, and
parameterized/negative offsets fail closed. No post-lowering pass assumes a trailing
`Project`: liveness/projection collectors treat `Limit` generically, and the Router
prefix dispatch slices `ops[..scan_idx]`, so the skip-`Limit` never reaches Graph.

## Addendum: candidate-scoped DISTINCT (implemented)

A `RETURN DISTINCT` tail is accepted in the top-k, threshold, and compound forms
with no planner, TEXT, wire, or cap change: the lowerings already ignore the
`Project` distinct flag, so the Router shape gate was the only blocker. The shape
carries `distinct: bool`; the execution adds one mode-agnostic stage —
rank → dedup → skip → take. Dedup is whole-projected-row first-occurrence
(`dedup_wire_rows`, mirroring Graph `dedup_rows`, O(n²) inside the 1024-row cap —
no hash index), so a shared title with different scores keeps both rows and only
byte-identical rows collapse. A DISTINCT tail bypasses the `k + n` ranking window
(a pre-dedup window is never exact) and takes `window − skip` after dedup
(`barrier_row_window`); threshold + DISTINCT opens together with no mode special
case, while threshold + skip stays rejected. Exact with `truncated=false`.

## Addendum: candidate-scoped same-variable dual-score (implemented, E2E happy path deferred)

Slice 1 of multi-placement covers `RETURN ..., text_score(d.a,$q) AS s1,
text_score(d.b,$q) AS s2 ORDER BY s1 DESC LIMIT k`: one triple feeds the ranking,
a second bare call on the same variable and query rides along as a projected
column. Same-variable needs no second id column — the second score joins on the
same document key — so there is no row-type or wire change; the
`SecondBarrierScore` carrier documents the second-key slot as the reserved
extension point for a future two-variable form (left unimplemented). The planner
needs no change (it already lowers the s1 triple; the bare s2 call never lowers).
The Router shape gate accepts the scanned call plus at most one distinct-property
bare call on the same variable (slice 1) and rejects wrapped calls, duplicate
scanned calls, third calls, non-TopK
modes, and DISTINCT tails; a second variable with no prefix-proven label is
rejected (see the slice-2 addendum for the bound-variable form). Execution resolves the second triple's
`(label, property)` index BEFORE any TEXT I/O — an uncovered second property
fails closed with function-unknown (`no ready TEXT index covers text_score`),
never a partial single-score frame — then runs the same barrier twice over the
same candidate key set (`TEXT(a)` → join → `TEXT(b)` → join → rank by `s1` →
project): no prefix re-run, no TEXT or wire change, per-call 256/32KiB caps
independent, rows missing either score dropped symmetrically, duplicate or
unrequested second keys rejected. Compound (`s1+s2` ordering), threshold-mixed,
different-variable, DISTINCT-mixed, second-key, and multi-shard duals stay
rejected with docs. Router unit tests cover the shape accepts/rejects; the live
E2E covers the unready fail-closed contract. The happy-path E2E initially needed
`#[ignore]` because TWO `CREATE TEXT INDEX` migrations on one graph could not
chain: the text backfill path converged without ever persisting terminal `Applied`
to the schema-migration ledger (`advance_text_backfill_migration` returned `Applied`
without a ledger write, and the crash-window short-circuit resumed `Applied` the
same way), so `pending_migration_exists()` stayed true forever. The same slice
lands the symmetric minimal fix in the text resume path — on re-drive, a terminal
(`Applied`/`Failed`) result's record is written back to `ROUTER_SCHEMA_MIGRATIONS`
(mirroring the generic index path's `finish_applied`/`finish_failed_migration`),
while non-terminal `Progress` leaves the ledger untouched — and the ignore is
removed with the happy path green.

## Addendum: candidate-scoped two-variable dual-score (implemented)

Slice 2 covers `RETURN ..., text_score(d.body,$q) AS s1, text_score(s.text,$q)
AS s2 ORDER BY s1 DESC LIMIT k`: the second call scores a second prefix-bound
variable with its own `(label, property)` triple and joins on its own document
key. Changes: (a) the prefix terminal projection carries `ELEMENT_ID(s)` as a
second internal identity column (the s2 call itself never reaches the graph —
both score columns stay excluded); (b) `CandidatePrefixRow` gains an internal
`key2` (struct extension only, wire unchanged); (c) the second round trip keys
TEXT off the deduplicated `key2` set and retains symmetrically (a row survives
only with both scores); ranking stays on s1. (d) The planner
`collect_text_barrier_bindings` hook now also protects variables of bare
`text_score` calls projected by later `Project` columns — the second variable
never lowers to a `TextScan`, so without this its binding could project to a
record and lose identity; only the bare-call form is collected (wrapped calls
never reach a barrier). The Router proves the second label from the
barrier-free prefix with the planner's own `proven_prefix_label` (now `pub`;
generic plan analysis, no execution assumptions) — unbound or ambiguously
labeled second variables fail closed. Per-call 256/32KiB caps stay independent;
an uncovered second triple fails closed before any I/O. Still rejected with
docs: `s1+s2` ordering, threshold/compound-mixed barriers, DISTINCT-mixed tails,
third calls, and multi-shard duals. Router unit tests cover the second-variable
shape accepts/rejects plus the pure `second_join_key` extract/join/drop/order;
the planner hook has its own bare-vs-wrapped unit test; the live E2E seeds one
Summary per document (`HAS_SUMMARY`, second TEXT index chained via the fixed
migration lane) and kills second-join-ignore, scoreless-remain,
s2-order-confusion, and replay.

## Addendum: nested OPTIONAL MATCH prefix with nulls-last keep (implemented)

The nested-keep slice covers `MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project)
OPTIONAL MATCH (p)-[:HAS_DOCUMENT]->(d:Document) RETURN d.rank AS rank, p.pno AS pno,
text_score(d.bio,$q) AS s ORDER BY s DESC LIMIT k`: documents may miss per project.
Changes: (a) the planner `extract_topk_order` rejects an explicit `NULLS FIRST`
(no barrier honors null-first ranking — the mention stays residual and fails
closed) and lowers bare/`NULLS LAST` with the nulls-last barrier contract (a
deliberate deviation from the GQL DESC default); `proven_prefix_label` descends
one `OptionalMatch` sub-plan level with shared label accumulators, so an
ambiguous or unlabeled optional binding stays unproven and fails closed. (b) The
Router gate keeps the shape TopK-only and single-score
(threshold/compound/second-call combinations stay rejected; DISTINCT opens in
the follow-up addendum below);
`CandidatePrefixRow.key` becomes `Option<u64>` (struct only, wire unchanged),
a `Null` identity decodes to `None` (any other non-bytes value is still a wire
break), null keys never reach TEXT, `rank_candidate_rows` keeps null rows after
the scored group in prefix order (null-null compares equal under the stable
sort), projects `Null` for their score, and skip/take apply to the final column
— `LIMIT k` consumes null slots while the TEXT window stays scored-only and
`truncated` stays TEXT-owned. Covered live by a doc-less-project fixture (one
empty project per user, `pno` 101/102 distinguishes the two null rows):
the keep test asserts 44 rows (42 in bio-frame order plus the null group in
barrier-free prefix order, NULL scores), LIMIT-43 slot consumption, and replay;
the fail-closed test asserts explicit `NULLS FIRST` rejects with the
did-not-lower diagnostic. A prepared-manifest covering this shape must declare
the score column nullable.

## Addendum: DISTINCT over an OPTIONAL MATCH prefix (implemented)

A `RETURN DISTINCT` tail is accepted over an OPTIONAL MATCH prefix in the
TopK-only, single-score form with no planner, TEXT, wire, or cap change: the
Router shape gate was the only blocker, and the execution already runs the
mode-agnostic rank → dedup → skip → take stage. The null-row contract: the
dedup key is the whole projected row and is null-safe (`Null == Null`, SQL
DISTINCT semantics), so byte-identical miss rows collapse to one first
occurrence while scored rows keep their score-separated identity — a miss row
never merges with a scored row because the score column differs (`Null` vs
`Float64`). LIMIT/OFFSET slot consumption follows the existing post-dedup rule
(dedup runs before skip/take; a DISTINCT tail bypasses the ranking window).
Compound/second-call combinations with an optional prefix stay
rejected (threshold over an optional prefix takes the drop contract of the
threshold-nested addendum below). Covered live by a duplicate-empty-project fixture (two `pno`-101
misses plus one `pno`-102 miss): the lifecycle asserts the 45-row non-DISTINCT
baseline, the 23-row DISTINCT collapse (21 scored + 2 miss survivors, null
group trailing in prefix order with NULL scores), and replay.

Threshold over an optional prefix has since been accepted with the
drop contract (see the threshold-nested addendum below); the compound form
with the drop-then-truncate contract followed (see the compound-nested
addendum below); non-exact compound (DISTINCT/OFFSET) and second-
call combinations with an optional prefix stay rejected.
## Addendum: threshold over an OPTIONAL MATCH prefix (implemented 2026-09-10)

A threshold predicate over an optional-bound variable is accepted with a
null-**drop** contract (SQL three-valued logic), the deliberate counterpart to
the TopK null-keep contract:

|                        | TopK + optional (keep)              | Threshold + optional (drop)              |
|------------------------|-------------------------------------|------------------------------------------|
| Operation              | `ORDER BY s` (ordering)             | `WHERE s > t` (filtering)                |
| NULL rows              | Kept: NULLS LAST tail, prefix order | Dropped: `NULL cmp bound` is UNKNOWN     |
| `LIMIT`                | Nulls consume slots                 | Applies to the scored-only set           |
| `truncated`            | TEXT-owned                          | `Some(false)`: a drop is not truncation  |
| Score column           | `Null`-projected (nullable manifest)| No score column at all                  |

Why the drop preserves meaning: the surface `WHERE` arrives as a
`PropertyFilter` *inside* the `OptionalMatch` subplan (the grammar binds the
clause to the optional match), while the Router barrier must evaluate after
optional padding. The planner hoists the subplan-trailing single-predicate
threshold filter to a top-level `Threshold` barrier
(`try_optional_hoisted_threshold_barrier`: exact `[.., OptionalMatch,
Project]` tail, one optional level, score-free siblings, same prefix/label/
coverage guards as the top-level barrier). The hoist turns error into drop and
never keep into drop: a residual `text_score` call fails closed in Graph
execution (`UnsupportedExpression`), so no keep-semantics execution exists for
the shape — there is no coverage-dependent meaning flip.

Router-side the gate opens from TopK-only to TopK|Threshold (compound and
second calls stay rejected), and a mode-gated 3-line retain drops null-key
rows before TEXT and rank — beside the dual symmetric retain, so the
drop → retain → truncate order the compound follow-up needs is already in
place. Non-optional threshold plans are unaffected (every key decodes
non-null there). NaN bounds inherit the existing contract unchanged
(`resolve_scan_bound` has no NaN guard; every comparison is UNKNOWN, so a NaN
bound retains nothing — fail-safe, not fail-closed) and need no nested-
specific branch. `RETURN DISTINCT` rides the same mode-agnostic post-rank
dedup over the dropped set with no extra code.

Covered live by reusing the nested doc-less-project fixture: the lifecycle
asserts the 42-row drop (44 would mean keep) with all-`None` pno (a leaked
miss would read 101/102), exact frame equality with the non-nested threshold
frame (membership and order), an interior-score `>`/`>=` split, a NaN bound
retaining zero rows, a DISTINCT collapse to the 21 documents, and replay.

## Addendum: compound over an OPTIONAL MATCH prefix (implemented 2026-09-11)

The compound form (`WHERE text_score(d.bio,$q) > t ... ORDER BY s DESC LIMIT
k`) over an optional-bound variable is accepted exact-only, closing the
follow-up the threshold slice left open:

- **Planner**: the compound fusion only scans top-level ops and the plain TopK
  path refuses a score-mentioning prefix, so compound-over-optional lowered
  nothing and failed closed. The slice adds `try_optional_hoisted_compound_barrier`,
  the symmetric counterpart of the threshold hoist: an exact
  `[.., OptionalMatch, TopK, Project]` tail fuses the subplan-trailing
  single-predicate threshold filter with the TopK into ONE `ThresholdTopK`
  barrier in place (plan length unchanged). Same-triple, single-level,
  score-free-sibling, no-scan/no-cap, proven-label, and coverage guards all
  carry over; the TopK must be OFFSET-free (skip + compound + optional stays
  unlowered) and `NULLS FIRST` still refuses via the shared order extractor.
  The hoist keeps the error-into-drop meaning argument: no keep-semantics
  execution exists for the shape.
- **Router**: the gate opens from TopK|Threshold to TopK|Threshold|Compound,
  with compound + optional admitted only when exact (`!distinct`,
  `skip == 0`; second calls stay rejected by the earlier column loop). No
  execution change was needed: the threshold-slice retain already drops
  null-key rows before TEXT and rank, and `barrier_row_window` truncates the
  scored-only set after ranking — the drop → retain → truncate order the
  design predicted. `truncated` stays `Some(false)` (exact, like the
  non-nested compound).
- **Out of scope, still rejected**: DISTINCT + compound + optional (dedup
  bypasses the row cap), OFFSET + compound + optional (no stable meaning over
  a null-padded set; refused at both the hoist and the gate), dual and
  second-call combinations.

Covered live by reusing the nested doc-less-project fixture: LIMIT 5 returns 5
scored rows frame-equal to the non-nested compound calibration (a
null-slot-consumption misimplementation would return fewer or leak nulls),
LIMIT 45 returns the 42 scored rows with no null padding, and OFFSET/DISTINCT
variants fail closed on their respective layers (planner residual vs Router
gate) with replay determinism.

## Addendum: candidate-scoped ascending top-k (implemented 2026-09-11)

`ORDER BY` score `ASC` now lowers into the same barrier with an explicit rank
instead of refusing: the ASC slice carries only direction, no new TEXT, wire
type, cap, or module.

- **Plan/wire**: new `TextTopkRank::{ScoreDescNullsLast, ScoreAscNullsLast,
  ScoreAscNullsFirst}` on `TextScanMode::{TopK, ThresholdTopK}` (bare
  `ORDER BY` and `DESC NULLS FIRST` still extract to `None` and fail closed;
  all four direction spellings normalize, absorbing the old lone-`DESCENDING`
  refusal). The leading seed and fused leading compound stay DESC-only; the
  candidate TopK/compound hoists (top-level and optional) thread the rank
  through. The wire adds `TextTopkRankWire::{DescNullsLast, AscNullsLast,
  AscNullsFirst}` (shortened names — the lint rejects the mirrored `Score`
  prefix on the derive span) with old-bytes rejection, no shim.
- **Router**: the second-score join admits only DESC TopK (an ASC barrier
  carrying a second call fails closed — the join keys off the s1 frame order);
  `rank_candidate_rows` orders `(score asc, key asc)` under ascending ranks
  with the same key-ascending tie-break both directions, nulls tailing in
  prefix order or heading under `AscNullsFirst`, and skip/take consuming the
  placed side's slots. Threshold keeps its DESC default dead-explicit.
- **Placement note**: a leading-shape ASC (bare `NodeScan` prefix) lowers
  through the *candidate* barrier by prefix enumeration — exact or fail-closed
  via the 1024-row admission cap — so the separate bottom-k TEXT driver
  remains a scale optimization for larger labels, never a correctness gap.

Covered live by two PocketIC tests reusing the candidate and nested fixtures:
bare-ASC over the full 42-row frame ascends with the DESC frame's score
multiset, and nested `ASC NULLS FIRST` heads the two prefix-ordered misses
before the ascending scored tail, both with replay determinism.

## Addendum: aggregates over text_score (deferred 2026-09-11)

Aggregates over `text_score` (global `AVG`/`MAX`/`MIN`/`SUM`/`COUNT`) stay
deferred: no demand exists in-repo, and industry demand (e.g.
Elasticsearch per-bucket average score) points at the grouped form, which
would need a separate score-by-group-key slice — not the cheap global
window fold. Current shape fails closed safely (no barrier, residual
rejected). If demand appears, design from the grouped form.
