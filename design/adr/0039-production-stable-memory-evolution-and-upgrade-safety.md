# 0039. Production stable-memory evolution and canister upgrade safety

Date: 2026-07-11
Status: proposed
Last revised: 2026-07-11
Anchor timestamp: 2026-07-11 04:55:51 UTC +0000

## Context

Gleaph is a database. Its durable core is canonical Graph data plus the minimum Router authority,
catalog, routing, idempotency, and recovery state required to locate, interpret, and safely mutate
that data. This core must remain readable and internally consistent across canister upgrades.
Preserving raw stable-memory pages is necessary but insufficient: a new Wasm must also understand
every durable-core layout and record encoding that it may encounter.

Not every current canister has the same maturity or durability contract. Property Index and Vector
Index are rebuildable projections whose designs may still change substantially. Provision and
related issuance/artifact workflows remain experimental control-plane work and may also be
redesigned or reinstalled. This ADR must protect the database core without prematurely freezing
those subsystems.

As of 2026-07-11 UTC, the repository has strong reopen checks for the LARA composite graph and the
vector row slab, a typed `MemoryId` registry, and same-Wasm upgrade tests for several critical
paths. It does not yet have a production migration protocol. ADR 0007 explicitly permits
development data to be wiped after an incompatible layout change, several stable values decode
lazily with `expect`, and not every canister performs an eager compatibility preflight in
`post_upgrade`.

There is also an immediate lifecycle mismatch: Router `post_upgrade` decodes the required
`RouterInitArgs`, while established upgrade callers and PocketIC tests pass Candid `()`. Therefore
the current empty-argument Router upgrade path traps before durable state can be reopened.

These gaps are release blockers for any deployment that promises durable core graph data. This ADR
does not claim that production compatibility is already implemented; it defines the required core
contract, keeps rebuildable and experimental subsystems flexible, and defines how a subsystem is
later promoted into the production durability boundary.

## Problem

The system needs one enforceable answer to each of the following questions:

1. How does a new Wasm determine whether it can read the installed stable-memory format?
2. How are canonical records migrated without an unbounded `post_upgrade` call?
3. How is a migration resumed after interruption without applying a row twice or skipping a row?
4. Which data is migrated, and which derived data is rebuilt from a canonical source?
5. How does an operator distinguish an incompatible binary from corrupt or partially initialized
   stable memory before serving traffic?
6. Which old binary and data versions must each release prove it can upgrade?
7. How are init-only bootstrap arguments kept out of routine upgrade requirements?

Without explicit answers, an upgrade may succeed but later trap on the first access to an old
record, or an operator may be forced to wipe database state to recover.

## Decision

### Production-readiness gate

Gleaph must not claim production durable-core compatibility until the Durable Core stages in the
implementation plan below are complete. Before that gate closes, deployments remain development
deployments whose stable state may require reinstall or wipe after an incompatible change.

After the gate closes, an incompatible Durable Core stable-format change without a reviewed
migration or an explicitly supported compatibility reader is prohibited. This prohibition does not
freeze rebuildable or experimental subsystems.

### Persistence maturity classes

Every stable region is assigned one persistence maturity class in addition to its existing
canonical/derived/maintenance classification:

1. **Durable Core** — canonical Graph state and the minimum Router state required to authorize,
   resolve, route, make idempotent, and recover operations on that canonical state. It receives the
   fixed-header, migration, preflight, and N-1 compatibility contract in this ADR.
2. **Rebuildable Derived** — Property Index and Vector Index data whose canonical source is Durable
   Core state. Its physical layout may be replaced without an old-format reader when the release
   provides a bounded reset/rebuild path and query behavior is safe while rebuilding.
3. **Experimental Control Plane** — Provision jobs, artifact/release state, and other subsystems
   explicitly marked experimental or design-stage. Their layout may change incompatibly and their
   development stable memory may be wiped or reinstalled until a later ADR promotes them.

The class applies to a concrete state family, not merely a crate name. If a future Vector Index or
Provision record becomes the only source of an accepted user-visible fact, external effect, or
irreversible receipt, it must be promoted to Durable Core (or another explicit production-durable
class) before that behavior ships. Calling a state family "derived" or "experimental" cannot be
used to discard an obligation that no other durable source can reconstruct.

The region-level maturity inventory is a prerequisite to accepting this ADR, not merely an
implementation follow-up. The minimum Durable Core candidates are Graph existence, canonical
adjacency, labels, properties, inline edge payloads, graph metadata, and the Router auth, name/id
catalogs, shard routing, idempotency, and recovery facts required to access them. Graph-hosted
embeddings, Router vector catalogs/policies, Property Index, Vector Index, Provision, telemetry, and
feature-specific operational state are not Durable Core by location alone. Embeddings, uniqueness,
or another feature family enters the production compatibility matrix only after its owning ADR and
the maturity inventory explicitly promote it.

### Separate init and upgrade arguments

Each canister owns a distinct upgrade-argument type. Init-only authority, bootstrap principals,
initial catalogs, and fresh-install configuration must not be required on a routine upgrade.

Router introduces an optional `RouterUpgradeArgs` containing only explicit operator overrides.
Its upgrade boundary accepts the established empty Candid `()` form as `None`; a supplied override
is validated before any durable configuration is changed. With no override, Router reconstructs
heap state from its durable configuration. The issuing principal and initial administrators remain
init-only inputs and are never replayed during upgrade.

The same separation applies to future Graph, Property Index, Vector Index, and Provision upgrade
arguments. Absence of upgrade arguments always means "preserve durable configuration".

### One fixed Durable Core header per canister

Each canister that owns Durable Core state appends one dedicated, never-repurposed stable-memory
region containing a small fixed-width `StableCell<FixedCoreHeader>`. The header is the single source
of truth for the active production-durable physical epoch and migration fence:

```text
FixedCoreHeader = {
  magic: [u8; 3],          // b"GCH": Gleaph Core Header
  version: u8,             // FixedCoreHeader encoding version
  active_layout_epoch: u32,
  migration_id_or_none: u32,
  migration_generation: u64,
  reserved: fixed zero bytes,
}
```

`migration_id_or_none == 0` means no migration; nonzero ids identify one reviewed concrete
migration. Ids are never reused within a canister.

The V1 header follows the `ic-stable-structures` convention of a three-byte magic followed by a
one-byte version, so its first four bytes are `b"GCH"` plus `version = 1`. It has a fixed byte width,
offsets, endianness, and manual encoding. `version` describes the `FixedCoreHeader` encoding and is
independent from `active_layout_epoch`. Those facts are frozen once the production-readiness gate
closes. The decoder reads `magic` and `version` before interpreting later bytes, so the version
needed to decode the header is never hidden inside an incompatible Candid envelope.

`FixedCoreHeader` implements a fixed-size bounded `Storable`; it is not Candid-encoded. The outer
`StableCell` retains its own `SCL` header, while the inner `GCH` discriminator detects a wrong
domain value or miswired StableCell region. Reserved bytes permit a bounded future header extension
without moving fields; they must be written as zero and ignored by older compatible readers.

The header deliberately does not store a generic migration phase or cursor. A concrete migration's
owner stores its typed, versioned progress in the destination store's metadata or a justified
owner-specific control region. A new control region is not mandatory when existing destination
metadata can own the state without ambiguity. The header only fences which `migration_id` and
generation may advance or cut over. This keeps Graph record details inside Graph and Router catalog
details inside Router rather than turning the header into a central migration framework.

The typed layout registry remains the compile-time description of expected regions and ownership.
The durable header records which compatible epoch the installed data actually uses. The inclusive
reader range and output epoch supported by a candidate Wasm belong to its compiled capability and
artifact metadata, not to the installed header. Neither the inventory document nor crate/package
versions are runtime authority.

Adding a Durable Core header region is itself the final development-only layout cutover for that
canister. Its exact appended `MemoryId` must be recorded in ADR 0007, the typed registry, and the
stable-memory inventory in the same implementation patch. Existing Durable Core `MemoryId`s must
not be renumbered.

Rebuildable Derived and Experimental Control Plane canisters are not required to add this header
while they retain those maturity classes. They must instead carry an explicit reset/rebuild or
development-wipe policy in the inventory. Promotion to Durable Core requires adding and seeding the
header before accepting production-durable state.

### Header creation and legacy adoption

Fresh `init` is the only normal path that creates a Durable Core header. Once the production gate
closes, `post_upgrade` treats a missing, zeroed, truncated, or invalid header as a partial/corrupt
layout and fails closed; it must never infer "fresh" from an empty header region or reconstruct the
header from the other regions.

The one-time transition from a pre-header development canister is a separate explicit adoption
operation. It is permitted only before the production gate closes and must validate the complete
expected legacy region set, select one named legacy epoch, seed the fixed header once, and record a
non-reusable adoption generation. There is no general "repair/recreate header" API. After adoption,
header loss requires recovery from an external backup or a separately reviewed disaster-recovery
procedure, not automatic initialization.

### Version granularity at the owning boundary

Versioning is applied at the coarsest boundary that can preserve compatibility without mixing
ownership. A homogeneous region uses a region/header schema version. A fixed raw row uses its
owning store's layout version. A per-record version tag is used only when old and new records must
coexist during online migration, or when independently retained records such as journal entries
genuinely evolve at different times.

Per-record tags use the smallest explicit manual representation that fits the contract (normally a
single-byte version), not a Candid enum wrapper by default. Candid compatibility is helpful but is
not the version policy by itself. Adding `#[serde(default)]` to a field does not replace a
stable-format compatibility test. This avoids adding tag and decode overhead to every property,
edge, or other high-cardinality row merely because its schema could change someday.

For each stable value change, the implementation plan must classify it as exactly one of:

- byte-for-byte compatible with the existing version;
- readable through an explicit old-version decoder and converted in memory;
- migrated into a new record version or a new stable region; or
- derived and deliberately rebuilt from its named canonical source.

Unknown region or record versions fail closed with a typed compatibility error during preflight or
migration. Normal read APIs must not discover an expected old version for the first time through an
unclassified `expect` panic.

### Persist every datum required to interpret Durable Core bytes

A value that changes the meaning of persisted Durable Core bytes is durable interpretation
metadata, even when the current implementation supplies it as a constructor argument or compile-time
constant. Its owning stable store or canister metadata must persist it as a first-class datum and
validate it during reopen before any row is interpreted. Heap configuration, init arguments,
collection order, and a repeated binary constant are not independent sources of truth for persisted
meaning.

For each such datum, a concrete implementation plan must choose one of:

- encode it in the owning store header and include it in that header's compatibility check;
- persist it in an existing owner metadata record whose lifecycle is all-or-nothing with the store;
  or
- add a dedicated stable region only when neither existing owner can represent the invariant without
  mixing responsibilities.

Changing an interpretation datum is a stable-format change. It requires explicit compatibility or
migration classification under the preceding section; routine `post_upgrade` must not silently
substitute the new binary's default.

**Open Graph application (verified 2026-07-11 UTC):** `LabeledLaraGraph::default_label` determines
how default/bypass vertex rows are interpreted, but `LabeledLaraGraph::init` currently accepts it as
an argument and stores it only in heap state. Reopening the same memories with a different value is
not rejected. Before the production-readiness gate closes, the Graph/LARA owner must select its
durable representation, define the legacy adoption or migration rule, and add mismatch plus N-1
reopen coverage. The exact representation remains **planned**; this ADR does not preselect a new
`MemoryId`.

### Bounded, resumable canonical migration

Durable Core canonical state is never wiped or treated as rebuildable. Its migration is a bounded
admin workflow. The owning storage domain defines a typed migration record containing its phase,
cursor, source/destination identifiers, and verification evidence in destination metadata or an
owner-specific stable control region. The fixed core header supplies only the active `migration_id`
and generation fence.

Each migration step must:

1. validate the header's current `(migration_id, generation)` and the owner record's
   `(from_epoch, to_epoch, phase, cursor)` fence;
2. process at most the declared item and instruction budget;
3. write destination records idempotently;
4. validate the written batch before advancing the durable cursor; and
5. return observable progress without claiming completion early.

Each migration plan must choose and justify exactly one evolution strategy:

1. **Compatible/no copy** — the existing bytes remain valid.
2. **Lazy read-old/write-new** — mixed versions are explicitly tagged and converged on bounded
   reads or maintenance steps.
3. **Verified in-place** — allowed only when fixed-width bounds, interruption safety, and rollback
   behavior are proven for the concrete format.
4. **Shadow copy** — write newly appended regions, verify them, then cut over the header.
5. **Offline export/import** — an explicit fallback when online migration cannot fit safely.

Shadow copy is not the default for every change. Before starting one, the owner computes or
conservatively bounds source live bytes, destination bytes, writes expected during migration,
verification metadata, and safety margin. Insufficient headroom rejects the migration before the
first destination write. The plan also states how long rollback bytes are retained and whether old
pages can be reused, retired, or require a later export/import compaction. A sequence of migrations
must not grow unreachable stable regions without an explicit capacity lifecycle.

The headroom decision remains valid for the whole migration. The owner either reserves the required
logical capacity, gates ordinary growth against that reservation, or rechecks a conservative bound
before every step and enters a safe paused/write-limited state before exhaustion. A start-time
estimate alone is insufficient when concurrent writes can grow source, destination, or delta state.

For a shadow replacement, old canonical regions remain untouched until verification completes.
Cutover is a single fixed-header transition after the destination has been proven complete.
Destructive retirement of old regions is a later, explicit operation and is never part of the first
compatible release.

Public writes during migration are either rejected with a typed maintenance error or handled by a
reviewed dual-write/delta protocol. The chosen rule must be stated per migration; silent writes to
only one side are prohibited.

Every migration plan also declares read availability, write availability, maximum expected
write-stop duration, migration throughput target, delta/backlog bound, last safely abortable phase,
and post-cutover rollback contract. A generic dual-write framework is not introduced before two
concrete migrations demonstrate the same invariant and execution shape.

Migration authority is separate from ordinary graph mutation authority. Each Durable Core owner
exposes a narrow status surface plus guarded start, bounded step, verify, cutover, and abort
operations. The concrete plan identifies whether the caller is a controller, Router control-plane
administrator, or another durable authority; an unprivileged data writer can never start or advance
a migration. Status reports the migration id/generation, epochs, phase/cursor, capacity reservation
or headroom, delta/backlog, last error, and whether abort/cutover is currently legal.

### Migration completion proof

A migration cannot transition the fixed header to the target epoch merely because its work cursor
reached an expected final value. The owner-specific verification phase must prove:

- the source key/range has been exhausted under its declared ordering;
- every source identity has exactly one valid destination representation, or an explicitly recorded
  deletion;
- destination counts and any format-specific aggregate checks match the source evidence;
- the migration delta/backlog is empty;
- no stale worker can write under an earlier generation; and
- all cross-region invariants owned by the migrated store pass.

Verification is bounded and resumable. A schema-preserving upgrade needs only O(1) preflight;
bounded full-range verification is paid only for the record families being migrated.

If public writes continue, the final delta drain and cutover are one fenced update boundary: acquire
the source write fence, revalidate migration id/generation, drain or prove the final delta empty,
re-run the cutover-critical invariant checks, and change the fixed header before releasing the
fence. A writer that started under an older generation must fail its commit precondition. A prior
"delta empty" observation from another message is never sufficient for cutover.

Rollback is classified explicitly. Pre-cutover abort restores normal use of the old source.
Post-cutover binary rollback is allowed only while the old binary supports the new active epoch.
Data rollback after accepting a new-format write requires a reviewed reverse migration or reverse
delta; retaining old shadow bytes alone does not make them current. Otherwise recovery after
cutover is forward-fix only.

### Mixed-epoch cluster rollout

Durable Core is not upgraded atomically across Router and all Graph shards. Every release therefore
declares and tests the intermediate combinations that can occur during its rollout, including at
least `Router N × Graph N-1`, `Router N × Graph N`, and, when rollback could create it,
`Router N-1 × Graph N`. The matrix covers both stable-layout compatibility local to each canister
and the Router↔Graph wire/protocol behavior needed to operate that combination.

Router tracks or queries each shard's reported header epoch and migration readiness; it must not
assume all shards share one epoch. A rollout plan states which side upgrades first, which operations
remain enabled in every intermediate state, whether fan-out may combine results from different
epochs, and the fence that prevents target-epoch-only writes before every affected shard can accept
them. A shard failure leaves the cluster in a supported mixed state or halts behind an explicit
maintenance gate; it never relies on distributed atomic upgrade.

Property/Vector rebuild activation follows the same principle: a new derived generation is not
query-ready until its canonical source epoch range is supported, its bounded rebuild completes, and
Router activates that generation. Rollout ordering alone is not evidence of compatibility.

### Derived and maintenance state

Rebuildable Derived state is rebuilt from the Durable Core canonical source identified in the typed
stable-layout registry. It is not copied merely to preserve physical representation. A release may
replace its entire physical format, including Vector Index and Property Index layouts, without
migrating old derived bytes. It must provide a bounded reset/rebuild driver, generation fencing so
stale work cannot repopulate the new layout, and safe query behavior while the projection is
incomplete.

Maintenance state may be restarted only when its owner proves that doing so cannot lose a canonical
effect, release a live reservation, reuse an allocated identity, or bypass a recovery obligation.
Otherwise it is migrated as canonical operational state even if it is not query truth.

### Upgrade preflight on every Durable Core canister

Every canister that owns Durable Core state implements `post_upgrade`. Before serving normal
traffic, each hook:

1. decodes only its optional upgrade arguments;
2. opens and validates the fixed Durable Core header;
3. checks that this Wasm supports the installed epoch and active migration fence;
4. eagerly opens composite stores and validates their O(1) headers and cross-region shape; and
5. reconstructs heap-only caches, decode hooks, and timers from durable state.

Preflight must remain bounded. It validates the fixed core header, store headers, and migration
fences, not every row in a large map. Region/row versions and migration verification cover row-level
compatibility. A failed preflight traps the upgrade with a diagnostic naming the canister, region or
record family, installed epoch, and supported epoch range; it must not mutate canonical data first.

Rebuildable Derived canisters need only reject corrupt partial layouts that could make reset or
rebuild unsafe; a planned incompatible format may start empty and rebuild. Experimental Control
Plane canisters retain their documented development reinstall/wipe procedure until promotion.

### Compatibility window and release gate

Every production release must support a Durable Core upgrade from at least the immediately
preceding production stable-layout epoch (`N-1`). A longer window may be declared, but a shorter one
requires an explicit export/import or stepped-upgrade procedure before release.

CI keeps the previous production Wasm and a small versioned stable-state fixture for every Durable
Core canister. The required test sequence is:

1. install the previous Wasm;
2. write representative canonical, derived, maintenance, and non-terminal recovery state;
3. upgrade to the candidate Wasm with empty upgrade arguments;
4. finish any bounded migration/rebuild;
5. verify reads, invariants, and recovery convergence; and
6. perform new writes and a second upgrade to prove the result remains reopenable.

Each fixture carries a coverage manifest proving that it contains every Durable Core region and
stable record/enum variant written by the old release, including `None`/`Some` optional forms,
empty/non-empty composite shapes, boundary sizes, tombstones/deletions, and every non-terminal
maintenance/recovery phase. Fixtures stay small by covering variants and boundaries rather than
production volume.

Subject to the prerequisite maturity inventory, the Durable Core matrix covers at minimum:

- Graph existence, canonical adjacency, labels, properties, inline payloads, metadata, and the
  maintenance/recovery records proven necessary to preserve those facts;
- Router auth, graph/name catalogs, idempotency, graph/shard routing, and graph-mutation recovery
  state; and
- the Router state required to discover and safely rebuild Property Index and Vector Index
  projections.

Property Index and Vector Index use separate destructive-format-change tests: install the candidate
layout empty, rebuild from the upgraded Durable Core source, fence stale generation work, and prove
query readiness only after rebuild completion. Provision needs same-Wasm or cross-version upgrade
tests only for behavior its current implementation claims to preserve; full N-1 compatibility is
not a production gate while Provision remains Experimental Control Plane.

Same-Wasm upgrade tests remain useful for heap-loss and reopen behavior but do not satisfy the
version-compatibility gate.

Candidate artifact metadata for Durable Core Wasm records the canister kind, supported fixed-header
versions, the inclusive range of installed layout epochs it can read, the epoch it writes, and the
Router↔Graph protocol versions/intermediate combinations it supports. Release tooling validates
that metadata and the required cross-canister rollout order. Adding these fields requires ADR 0036
and its current immutable `ArtifactMetadata`/release schema to be updated in the same implementation
patch; ADR 0039 does not pretend they already exist. The metadata prevents an obviously incompatible
core release from being selected; the target canister's fixed core header and `post_upgrade`
preflight remain the runtime authority. Provision is not required to freeze its own stable schema
merely because it stores or transfers these artifact facts.

Reserved header bytes may acquire meaning without a `FixedCoreHeader.version` bump only when every
older supported reader can safely ignore that meaning. Any new field that changes validation,
cutover, or write safety bumps `FixedCoreHeader.version`, and artifacts must declare support for it
explicitly.

### Performance and capacity gates

The fixed header is a lifecycle/admin concern and must not be read on normal query or mutation hot
paths. Its implementation adds focused wasm canbench coverage for Graph and Router reopen so the
extra stable region and decode work are measured against the ADR 0007 baselines.

Any migration or encoding change measures, as separate signals:

- source scan and destination write instructions per item;
- stable bytes read and written per item;
- sustained bounded-step throughput;
- peak source + destination + delta stable-memory amplification;
- verification scan cost;
- hot query/mutation cost before and after any row encoding change; and
- post-migration reopen cost.

High-cardinality row tags or wrappers require direct hot-path and encoded-size evidence. A safety
change may justify a regression, but the ADR/plan must state the measured cost and why a smaller
region-level or row-layout version cannot preserve the same invariant. Final benchmark artifacts
are updated only for implemented changes, not for this proposed design.

Before measurement, each concrete implementation plan declares its acceptance thresholds: maximum
affected hot-path regression, per-step instruction ceiling and minimum useful items/step, maximum
peak stable-byte amplification, maximum write-stop duration, and allowed reopen regression. A
benchmark result without a predeclared threshold is evidence but not a release gate.

### Documentation and change control

Any Durable Core stable-format change must update, in one patch:

- the owning code and versioned record or migration;
- the typed stable-layout registry;
- `design/storage/stable-memory-inventory.md`;
- ADR 0007 when a region is added, removed, or repurposed;
- the N-1 upgrade fixture and compatibility test; and
- the affected domain ADR or design contract.

For Rebuildable Derived or Experimental Control Plane changes, the patch instead records the new
layout and its reset/rebuild/wipe policy and must not claim compatibility that was not tested.

CI must fail when the typed region count differs from the inventory summary. Prose remains useful
for ownership and recovery rationale, but executable registry checks own the numeric layout facts.

## Ownership and invariants

| Invariant | Owner / enforcer |
|---|---|
| Installed Durable Core stable epoch is unambiguous. | Fixed Durable Core header |
| A Durable Core canister serves traffic only when its Wasm supports the installed epoch. | Owning canister's `post_upgrade` preflight |
| Durable Core canonical state is never discarded as a migration shortcut. | Owning storage domain and migration state machine |
| Migration progress is resumable and fenced. | Owner-specific migration record plus fixed-header generation |
| Old canonical bytes remain available through first shadow cutover. | Concrete shadow-migration owner |
| Final delta drain cannot race a concurrent writer. | Source write fence plus same-message cutover |
| Missing headers are never mistaken for fresh state. | Init-only creation and explicit legacy adoption |
| Migration operations require stronger authority than data writes. | Owner-specific guarded migration API |
| Mixed Router/Graph epochs remain a supported cluster state. | Release compatibility matrix and Router rollout gate |
| Derived state has one named canonical rebuild source. | Typed layout registry and owning backfill driver |
| Heap state and timers are not authority. | Durable owner plus lifecycle reconstruction |
| Every datum required to interpret Durable Core bytes is persisted and checked on reopen. | Owning store header or owner metadata; fixed-header epoch fences incompatible changes |
| Init authority is not replayed on upgrade. | Separate init and optional upgrade argument types |
| A release proves N-1 data compatibility. | Versioned Wasm/fixture upgrade matrix in CI |
| Region numbers have one mechanical source of truth. | Typed stable-layout registry |
| Derived layout replacement cannot lose user graph data. | Canonical rebuild source plus generation-fenced rebuild |
| Experimental state is not mistaken for production-durable state. | Inventory maturity class and promotion ADR |

Router coordinates cross-canister rollout order but does not own Graph record migrations or derived
index rebuilds. Each Durable Core canister migrates the state and invariants it owns; each
Rebuildable Derived canister resets and rebuilds through its owning boundary. Provision release
manifests may select compatible Wasm sets, but they do not replace Durable Core stable-epoch checks.

## Alternatives considered

### Continue wiping development stable memory

Rejected for the production Durable Core. It remains acceptable for Rebuildable Derived and
Experimental Control Plane layouts under their explicit reset/rebuild/wipe contracts, and for all
development deployments until the core production-readiness gate closes.

### Depend on Candid's structural compatibility

Rejected as the sole policy. Candid can make selected record changes compatible, but it does not
version raw layouts, `MemoryId` assignments, manual byte formats, or cross-region invariants, and it
does not prove that every stored record remains decodable.

### Migrate all state synchronously in `post_upgrade`

Rejected because database-sized state can exceed one message's instruction budget and leave no
durable, observable progress model. `post_upgrade` performs bounded preflight and lifecycle
reconstruction only.

### Export and re-import the whole database for every change

Rejected as the default because it creates long downtime, duplicates operational tooling, and
turns routine schema evolution into disaster recovery. Export/import remains a fallback for an
explicitly unsupported epoch jump.

### One global coordinator migrates every canister

Rejected because it moves record invariants away from their owning storage domains. Router may
sequence a compatible release, but each canister owns its local migration and verification.

### Require shadow copy for every change

Rejected because it can double a large Graph and accumulate unreachable stable regions. Shadow copy
is retained when its rollback benefit justifies the measured headroom; compatible, lazy, proven
in-place, and offline export/import strategies remain available for concrete migrations.

## Consequences

Production Durable Core releases gain an explicit compatibility window, early failure for
unsupported binaries, and a resumable path for database-sized migrations. Core stable-format
changes become more expensive: they require version classification, fixtures, migration logic,
documentation, and temporary storage for non-destructive cutovers.

Only canisters owning Durable Core state necessarily gain a fixed-header region and `post_upgrade`
responsibility. The final core development cutover must wipe or deliberately seed existing
development state once; after that cutover, core stable region identities and the fixed-header
contract are frozen under this ADR. Property Index, Vector Index, and Provision retain substantial
design freedom under their declared maturity classes.

Region/header versioning keeps ordinary high-cardinality rows at their existing compact shape.
Shadow migrations temporarily amplify storage only when chosen for a concrete format and only after
capacity preflight; safe compatible, lazy, or verified in-place strategies avoid unconditional
double storage. The fixed header is read on lifecycle/admin paths, never on the normal query or
mutation hot path.

Maintaining previous production Wasm fixtures adds CI and artifact-retention cost. That cost is
accepted because same-Wasm reopen tests cannot prove upgrade compatibility.

## Implementation plan

1. **Lifecycle repair:** make Router empty-argument upgrades valid, introduce optional upgrade args,
   repair all affected PocketIC tests, and verify recovery timers re-arm.
2. **Maturity inventory:** classify every stable state family as Durable Core, Rebuildable Derived,
   or Experimental Control Plane, name its canonical source or allowed wipe policy, and remove
   embeddings/vector/uniqueness feature state from the core matrix unless explicitly promoted.
3. **Core header foundation:** append and register fixed headers only for Durable Core canisters;
   freeze the V1 manual encoding, implement init-only creation, explicit legacy adoption, bounded
   preflight, and typed diagnostics, and perform the final core development cutover.
4. **Core stable-record inventory:** enumerate each Durable Core `Storable` value and raw layout,
   assign an owner and compatibility classification, choose region/row/record version granularity,
   and add tags only where mixed-version coexistence requires them.
5. **First concrete migration support:** implement owner-specific phase/cursor state, capacity
   reservation/guard, guarded status/control APIs, availability contract, completion proof, final
   write-fenced cutover, and rollback classification for the first demonstrated core migration.
   Generalize helpers only after a second migration proves shared semantics.
6. **Core N-1 fixtures:** retain versioned production Wasm/data fixtures and build the Durable Core
   variant-coverage and mixed Router/Graph epoch matrix; separately prove destructive rebuilds for
   derived indexes.
7. **Release enforcement:** add core artifact stable-epoch compatibility metadata and make N-1
   evidence plus synchronized inventory/registry changes mandatory before a production release can
   be activated.
8. **Measured acceptance:** run focused Graph/Router reopen canbench plus the first migration's
   throughput, encoded-size, amplification, verification, and affected hot-path benchmarks before
   accepting its implementation ADR.

Each step requires its own bounded implementation plan and review. Step 1 fixes a current lifecycle
regression; Steps 2-8 establish the production core compatibility contract and must not be presented
as implemented until their tests pass. Promotion of Vector Index, Provision, or another subsystem
to Durable Core is a separate future decision, not an implicit consequence of this ADR.

## Implementation status

**Proposed (2026-07-11).** Existing reopen checks, typed layout registries, and selected same-Wasm
upgrade tests are foundations only. Router empty-argument upgrade compatibility, fixed Durable Core
headers, owner-specific bounded migration support, core record-version classification, and core
N-1 fixtures are not yet implemented. Durable Graph interpretation metadata, including LARA's
`default_label`, is also not yet bound to persisted state. Property Index and Vector Index remain
Rebuildable Derived; Provision remains Experimental Control Plane.

## Cross-links

- [ADR 0007](0007-stable-memory-layout.md) — physical region allocation and inventory policy.
- [ADR 0023](0023-federated-index-consistency-upgrade-compaction.md) — derived index recovery across
  upgrade and compaction.
- [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md) — canonical local commit
  and durable cross-canister recovery state.
- [ADR 0036](0036-versioned-wasm-artifact-catalog.md) — compatible multi-canister release sets.
- [Stable-memory inventory](../storage/stable-memory-inventory.md) — current region ownership and
  rebuild paths.
