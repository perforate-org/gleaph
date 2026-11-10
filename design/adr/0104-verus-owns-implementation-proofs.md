# 0104. Verus owns implementation proofs; Lean keeps models; Creusot is not adopted

Date: 2026-11-09
Status: accepted
Anchor timestamp: 2026-11-09 08:30:00 UTC +0900
Implementation status: no Verus proof exists in this repository at acceptance.
Plan 0370 (intake harness in this repository) and the independent verus-ic
project's four-gate plan gate adoption; this ADR owns
the tool ownership, evidence rules and retreat condition, not the implementation.

Related: [ADR 0086](0086-stable-vec-deque-segmented-block-ring.md) replaces the
vec-deque layout and constrains its restatement; the measured fixture drift and
per-cluster classification live in
[the Lean fixture classification record](../investigations/2026-11-09-lean-fixture-migration-classification.md).

## Context

The repository holds three kinds of formal evidence today:

- Lean fixtures beside three storage crates (`ic-stable-lara`,
  `ic-stable-vec-deque`, `ic-stable-linear-hash-map`), each a hand transcription
  of Rust semantics with a pinned revision and a stated scope. Only the
  linear-hash-map fixture is CI-gated.
- A Quint pilot under `formal/quint/`, explicitly experimental and non-normative,
  for bounded protocol state machines.
- Tests, PocketIC fixtures and canbench artifacts, which are the executable
  evidence the repository actually gates on.

The only implementation-level verifier previously exercised by the maintainer is
Creusot, in the separate, non-git `~/dev/creusot-ic` project (49 plans): a
Stable64 byte model, a Wasm gateway, MemoryManager layout helpers, map facades,
generated update handlers, native and actual-Wasm proof lanes, and PocketIC
runtime fixtures. That project proved the workflow, not this repository's code.

## Problem

Two failures are measured, not hypothetical:

1. **Transcription drift.** All three Lean fixtures describe older source.
   `ic-stable-lara` pinned `dbd76f3d` while 96 files changed afterwards;
   `ic-stable-vec-deque` pinned the contiguous ring that ADR 0086 replaced with
   the V1 segmented block-ring, so its proofs concern deleted code; and
   `ic-stable-linear-hash-map` has drifted in `control.rs` since its pin. The
   gate builds the model and cannot notice that the model stopped matching the
   source. The concrete instance is `LogHead::from_index`: the implementation now
   rejects index 169 while its own documentation, the Lean transcription and a
   vacuous unit test all describe 169 as valid.
2. **Unverifiable risk surface.** The repository's risk is concentrated in
   `unsafe` stable-memory code (40 sites in `ic-stable-vector-page-store`, 10 in
   the memory backend, and so on) and in canister handlers (345 `async fn` in
   `router`, 300 in `graph`). Creusot targets safe Rust, rejects unsafe
   operations, and rejects async functions, so it can only verify extracted pure
   arithmetic — the layer Lean already owns.

## Existing Architecture Assessment

- Lean is the right tool for abstract models and hard mathematics (induction,
  split repacking, epoch protocols, failure atomicity), and its kernel-checked,
  solver-independent proofs are stronger audit evidence than an SMT proof of the
  same claim. It cannot tie a proof to shipped code, which is the drift failure
  above.
- Quint answers bounded protocol questions and is deliberately non-normative. It
  is not a code verifier and stays as is.
- Creusot's unique strength is safe, `std`-heavy Rust, but both of its limits
  (unsafe, async) fall exactly on the repository's risk surface. Adopting it
  alongside another implementation verifier would add a second toolchain
  (nightly pin, opam/Why3/provers), a second specification language, and
  duplicated properties at the boundary.
- Verus covers unsafe code, pointers and permission-based cells, has documented
  bridge mechanisms for unverified code, supports attribute-based incremental
  adoption, and can express the pure arithmetic that Lean currently transcribes.
  Its async support is immature and wasm-target verification is unproven, which
  is why the independent verus-ic project runs those gates before any claim is made.

## Alternatives

1. **Status quo.** Keep Lean fixtures only. Rejected: it retains the measured
   drift and leaves the unsafe and handler layers unverified.
2. **Creusot-only.** Rejected: cannot touch unsafe or async code, adds heavy
   tooling, and duplicates the safe layer Lean already owns.
3. **Creusot and Verus.** Rejected for now: two toolchains and two specification
   languages for one layer, with the same property represented twice. Revisit
   only under the retreat condition below.
4. **Verus alone, retire Lean.** Rejected: loses kernel-checked, solver-
   independent evidence for the hardest proofs, and forces SMT-hostile induction
   into Z3.
5. **Verus for implementation, Lean for models, Quint for protocols.** Chosen.

## Decision

1. **Verus owns implementation-level proofs** in this repository: contracts on
   shipped functions, proofs about `unsafe` helpers, and permission-based bridges
   to unverified dependencies.
2. **Lean keeps the model layer**: abstract algorithm specifications, hard
   inductive/number-theoretic proofs, and the independent-transcription audit
   where it still pays.
3. **Quint keeps bounded protocol models** and stays non-normative.
4. **Creusot is not adopted in this repository.** The external `~/dev/creusot-ic`
   project is frozen as historical evidence and is not a dependency of any crate
   here.

   The shared IC-facing implementation asset is the independent `~/dev/verus-ic`
   project. It is not a subproject of this repository and contains no Gleaph-specific
   types; this repository consumes it at a pinned tag.
5. **One owner per property.** A property proved in Lean is not also proved in
   Verus (and vice versa); during a transition the Lean fixture is a reference
   model, not a second gate.
6. **Proof claims carry their scope.** Every result is labeled proved,
   trusted-and-tested, or assumed. Every assumed site has a trust card naming the
   assumption, its dependents and the test that checks it. Every proved clause
   has a recorded mutant that the verifier rejects, so a vacuously true spec is
   visible.
7. **Implementation proofs live with the code.** The default is to extract a
   verified leaf crate that production calls, so one implementation is both
   proved and shipped. Mirrors are permitted only as audit fixtures with a pinned
   revision and an enforced freshness check.
8. **Verus is pinned per release**, compatible with the repository's 14-day
   publish-age gate, and gated in a path-filtered CI job once a pilot passes.
9. **Retreat condition.** If Verus cannot express or discharge a required
   boundary after a documented attempt, that property alone may be retried in the
   frozen Creusot stack; general re-adoption requires a new ADR.
10. **Dependency form for the shared asset.** Until `verus-ic` is published to
    crates.io, this repository consumes it as a git dependency pinned by immutable
    rev. The registry 14-day publish-age gate does not apply to git revs, so that
    exception is recorded here; after publication, consumed versions must satisfy
    the gate.

## Consequences

Positive:

- Migrated properties become tied to shipped code; the drift class above dies at
  compile time instead of accumulating silently.
- One implementation verifier, one proof language, one toolchain to provision and
  pin, and no duplicated specifications across two verifiers.
- The unsafe layer becomes verifiable, which no existing approach in the
  repository could do.
- Retiring Lean fixtures per the classification removes stale evidence and its
  maintenance cost.

## Trade-offs

- Why3's multiple provers and goal-level interactive debugging are not available;
  Verus debugging is SMT-oriented (`assert ... by`, `calc`, `reveal`/`hide`,
  rlimit tuning), and hard goals are expected to stay in Lean.
- Verus proofs depend on Z3 and the pinned release; a weekly upgrade can break
  proofs, so the upgrade procedure and rlimit budget must be recorded.
- A single implementation verifier is a single point of failure; the retreat
  condition above is the recorded mitigation, not a second adopted toolchain.
- The repository accepts that some current Lean fixtures will be retired rather
  than kept as independent transcriptions; that is an explicit evidence trade.

## Migration

- Plan 0370 provisions the pinned Verus release and launcher and proves one
  frozen mirror with mutant controls and trust cards.
- The independent `~/dev/verus-ic` project (not a subproject of this repository) runs the
  four IC gates (wasm target, Stable64 model with bridges, a proved unsafe helper, sync core
  plus async handler).
- The classification record orders Lean retirement: `ic-stable-lara` Stage 1
  first; `ic-stable-vec-deque` restated for ADR 0086 rather than migrated; the
  linear-hash-map model kept and freshly anchored.
- The `LogHead` 169 contract question is resolved: both constructors accept
  `0..=169`, regression tests pin the boundary and the all-negative rule, and the
  Verus mirror is re-frozen to the corrected blob (plan 0371;
  `design/evidence/log-head-169-boundary-2026-11-09.md`).

## Design Documentation Impact

- This ADR and `design/adr/README.md` (index row).
- `design/investigations/2026-11-09-lean-fixture-migration-classification.md`
  (analysis record; append-only).
- Per-crate `formal/SCOPE.md` files when a fixture is retired, restated or
  freshness-gated.
- Plan file 0370 (`plans/` is gitignored in this repository); the verus-ic plan lives in that
  project.

## Implementation note (plan 0372, 2026-11-09)

The `log_head` Verus mirror package under `crates/ic-stable-lara/formal/verus/` is retired together
with its subject. Plan 0372 deletes `LogHead` and the descriptor's i32 log-head view, and the mirror
transcribed exactly that deleted codec (plus a stale row-validation shape), so keeping it would have
been a false contract: the freshness gate would have pinned a blob nobody ships, and the drift this
ADR was written to remove would have deepened with every later edit. The package (its
`Cargo.toml`/`Cargo.lock`, `SCOPE.md`, `src/lib.rs` and `stub/`) goes away as a unit; it is not
re-frozen. The `LogHead` 169 resolution recorded under Migration above was that mirror's last accepted
state, so that bullet is history rather than a live contract.

**What stays:** the decision (Verus owns implementation-level proofs, Lean keeps models, one owner
per property), the pinned Verus release, launcher and harness scripts
(`scripts/setup-verus-stack.sh`, `scripts/verus-0.2026.10.18`, the `.tools/` ignore). The harness is
provisioned but idle: no active proof target exists until production adoption resumes (Plan 0370 and
the independent `~/dev/verus-ic` project still gate adoption), and re-targeting it at a surviving
`ic-stable-lara` module is a later slice, not this retirement. This slice adds no replacement spec
and no new trust card; it only removes the mirror of deleted code.

## Implementation note (plan 0374, 2026-11-09)

The Lean freshness gate proposed by the classification record is implemented, and its open question
("blocking now or advisory until the ADR follow-up?") is answered by a per-fixture status in
`crates/<crate>/formal/SOURCES.sha256`:

- `anchored` — the model was re-anchored to the manifest revision; any `HEAD` source change in the
  cited paths is **blocking** (`scripts/check-lean-sources.py` exits 1).
- `pending-reanchor` — the known anchor and drift are recorded; the checker lists changed paths as
  advisories and exits 0, so existing drift does not turn CI red before the model is re-anchored.
  Flipping the status to `anchored` activates blocking without a CI change.
- `obsolete` — the fixture models deleted code; it is reported and skipped.

Initial assignment: `ic-stable-lara` and `ic-stable-linear-hash-map` are `pending-reanchor` (their
measured drift is listed by the checker), and `ic-stable-vec-deque` is `obsolete` (ADR 0086). The
new `.github/workflows/lean-freshness.yml` runs the checker on formal/source changes and needs no Lean
toolchain; `lean-verify.yml` remains the lhm `lake build`/axiom gate. Re-anchoring lara and lhm (and
restating vec-deque) are separate slices; the gate makes their drift visible until then.

## Implementation note (plan 0375, 2026-11-09)

The lara Lean fixture is re-anchored and its freshness status is now `anchored`
(blocking). Plan 0375 updated the surviving `Lar/Basic`/`SlotIndex`/
`BucketLabelKey` vocabulary and citations from the retired log-head naming to
the shipped spill-run naming without changing any theorem statement or proof
obligation, rebuilt the project, and replayed the `Lar.lean` axiom guard
(same axiom set, no `sorryAx`). The manifest now pins `src/slab_index.rs` and
`src/labeled/bucket_label_key.rs` at `d75d2467…`, so drift in either source is
blocking; lhm remains `pending-reanchor` and vec-deque remains `obsolete`.
Modelling the newer exclusive-end/validity helpers — or retiring this fixture
under the Verus migration — is a later slice.

## Implementation note (plan 0376, 2026-11-09)

The lhm Lean fixture is re-anchored and its freshness status is now `anchored`
(blocking). The drift since its recorded anchor was a pure refactor — the
`control.rs` `derive_geometry` single-source helper (with a fail-closed
`physical_buckets = 0` case) and the `map.rs` scrub call site — while
`next_geometry`, `split_threshold`, `base_buckets` and `validate_control` were
unchanged, so P3–P5 were not invalidated. Plan 0376 models the helper and
proves it reconstructs the `ValidControl` level/cursor equation, rebuilds the
Stage 1–5 proof tree under the `lean-verify.yml` axiom gate, and pins
`control.rs`/`header.rs`/`map.rs`/`memory.rs` at `9803f522…`. Only the
vec-deque fixture remains `obsolete` pending the ADR 0086 restatement.

## Implementation note (plan 0377, 2026-11-09)

The vec-deque fixture is no longer classified `obsolete`: the block-ring restatement
has started and its freshness manifest now anchors `src/vec_deque.rs` at
`997debce…` with status `pending-reanchor` (advisory). Stage 1' block-routing
arithmetic and Stage 2' header validity are verified; the Stage 3' operation
contracts/preservation and the Stage 4' zero-movement proof are the remaining
obligations, after which the fixture flips to `anchored` and is wired into
`lean-verify.yml`. lara and lhm remain `anchored`/blocking.

## Implementation note (plan 0378, 2026-11-09)

The vec-deque block-ring restatement now covers Stage 1' (routing), Stage 2' (header validity) and
Stage 4' (zero-movement growth); the fixture builds sorry-free and the new proof is guarded in
`Svd.lean`. Its freshness manifest stays `pending-reanchor` (advisory) because the Stage 3'
observable operation contracts and `Inv` preservation are still open. After those land (plus the
`Classical.choice` cleanup) the manifest flips to `anchored` and the fixture is wired into
`lean-verify.yml`; lara and lhm remain anchored/blocking.

## Implementation note (plan 0379, 2026-11-09)

The vec-deque block-ring restatement is complete for the shipped layout: Stage 1' (routing), Stage
2' (header validity), Stage 3' (observable operation contracts and `Inv` preservation) and Stage 4'
(zero-movement growth) are verified, the fixture builds sorry-free, and its freshness manifest is
now `anchored` (blocking). Stage 5' (failure atomicity) and the `Classical.choice` cleanup that
precedes `lean-verify.yml` wiring remain. lara and lhm are anchored/blocking.

## Implementation note (plan 0380, 2026-11-09)

The vec-deque fixture's proof tree is now free of `Classical.choice` and is rebuilt plus
axiom-gated in the `lean-verify.yml` matrix (lhm and vec-deque). lara remains anchored but outside
that matrix because its surviving `unpack_pack_canonical` theorem still depends on
`Classical.choice`; cleaning that up and adding lara is a later slice. Stage 5' failure atomicity is
the remaining vec-deque proof work.

## Implementation note (plan 0381, 2026-11-09)

The lara Lean fixture is choice-free and now runs in the `lean-verify.yml` matrix
(lara, lhm, vec-deque), so all three anchored fixtures are rebuilt and axiom-gated
in CI. Its remaining audit value is the Stage-1 arithmetic transcription; lara
remains the first fixture to retire when the Verus migration criteria are met.
vec-deque Stage 5' failure atomicity remains the only open Lean proof obligation.

## Implementation note (plan 0382, 2026-11-09)

Verus production adoption has started with a bounded verified leaf:
`crates/verus-stable64` owns absolute-target Stable64 growth once, with a plain bridge
implementation for release builds and a token-based verified implementation that proves
`final_pages >= target_pages`. It consumes `verus-ic` v0.1.0 via a git tag, is checked by
`cargo verus verify` with the pinned Verus `0.2026.10.18.7d82994` stack, and has a path-filtered
`verus-verify.yml` CI job. The leaf is not yet a stable-memory owner in any canister: verus-ic's
single-owner rule makes that migration an atomic later slice. The private verus-ic repository means
the CI job needs the `VERUS_IC_TOKEN` secret and skips the token-gated steps (with a warning) until
it is configured; moving to crates.io or a read-only deploy key removes that friction. The leaf also
declares a standalone workspace as a bootstrap exception while the root Cargo files are being
reconciled.

## Implementation note (plan 0383, 2026-11-10)

The adoption leaf now has a runtime fixture: `crates/verus-stable64/fixtures/canister` is a minimal
wasm canister whose only durable state is Stable64 memory owned through the leaf's plain mirror, and
`fixtures/pocketic-tests` verifies install → absolute growth → stable write/read → canister upgrade
persistence (plus a no-shrink absolute target) on PocketIC 15.0.0. The `verus-verify` job builds the
wasm and runs the PocketIC test in addition to the host test and `cargo verus verify`. This is a
runtime check, not a new Verus claim; the atomic production-owner migration remains the next slice.
