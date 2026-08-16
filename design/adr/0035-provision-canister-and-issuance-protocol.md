# 0035. Provision canister and issuance protocol

Date: 2026-07-04
Status: Partially Implemented
Last revised: 2026-08-16 00:23:25 UTC +0000
Anchor timestamp: 2026-08-16 00:23:25 UTC +0000

## Context

Router owns graph identity, tenancy, routing, and the stable catalogs that collectively describe a
Gleaph deployment. A `GraphRegistryEntry` cannot represent a graph before its first canister exists:
it requires a `canister_id`, and its current `ProvisioningState` is not a complete issuance journal.
Canister creation and installation are irreversible cross-canister effects that may succeed before a
later callback or Router registration fails.

Automated provisioning therefore needs an idempotent executor without turning Router into a
management-canister client or creating a second topology registry.

## Decision

Introduce a dedicated **Provision** canister. Router remains the sole owner of logical graph identity,
tenancy, and routing; Provision owns only durable issuance jobs and receipts.

### Router orchestration state

Router adds a provisioning-request catalog separate from `GraphRegistryEntry`:

```text
ProvisioningIntentKey = (deployment_id, resource_kind, logical_resource_key)
RouterProvisioningRequest = {
  request_id, request_fingerprint, caller, graph_name, reserved_graph_id?,
  requested_resources, state, provision_receipt?
}
```

This record can exist before a canister id exists. Router creates `GraphRegistryEntry` and related
shard/index catalog records only after Provision reports installed canisters. The existing
`GraphRegistryEntry.provisioning_state` is not used as the pre-creation journal; a later implementation
may remove it or retain it only as a derived compatibility projection, with that migration decided in
the implementation slice.

### Resolved request and acknowledgement

After authenticating the caller and reserving graph identity, Router sends a resolved envelope:

```text
ProvisionRequest = {
  deployment_id, request_id, request_fingerprint,
  intent_key, reserved_graph_id?, graph_name,
  requested_resources,
  authorized_caller, release_id,
  router_callback_principal
}
```

Provision accepts envelopes only from the Router principal registered for `deployment_id`. It does
not read Router tenancy state or re-derive authorization. The same `request_id` and fingerprint
returns the existing job/receipt; the same id with a different fingerprint returns `Conflict`.
A durable intent lock rejects or joins a distinct request id targeting the same unfinished
`ProvisioningIntentKey`.

The `deployment_id -> Router principal, governance principal` binding is canonical Provision
bootstrap configuration, written only by the governance/recovery principal. It is authentication
configuration, not graph topology or tenancy. Router owns every logical graph record; Provision owns
only this deployment trust binding and its job journals.

Provision reports:

```text
ProvisionResult = {
  request_id, request_fingerprint, release_id,
  created_resources[{kind, canister_id, artifact_hash}],
  terminal_outcome
}
RouterProvisionAck = { deployment_id, request_id, accepted_registry_version }  (P1-3: the only Slice 1 wire-shape change; makes ACK addressing unambiguous across deployments)
```

Router verifies the fingerprint and intent lock, atomically commits the affected Router catalogs,
then returns the acknowledgement. Provision records `Completed` only after receiving that ack.

### Durable job state

Provision persists the next state before each remote effect and the observed result after it:

```text
Submitted -> Reserved -> CreatePending -> CanisterCreated
          -> InstallPending -> Installed
          -> RouterRegistrationPending -> RouterAckPending -> Completed
```

If creation or installation succeeds but a later step fails, the job resumes from the persisted
canister id; it never issues a fresh create. Failures requiring removal transition to
`CleanupPending` and use ADR 0037. `Failed` is terminal only when no external resource remains or
cleanup has been reconciled.

Deployment bootstrap is out of band: governance installs Router and Provision and binds their
principals. Subsequent logical-graph, shard, and index issuance uses this protocol.

The resource selection policy for those requests is defined separately by [ADR 0054](0054-provisioned-logical-graph-topology-and-resource-activation.md): initial bootstrap creates Router, the default logical graph, and its first Graph shard; optional index and procedure resources are requested independently. ADR 0035 remains the source of truth for issuance idempotency, durable effect progress, receipts, and reconciliation.

## Ownership and invariants

| Invariant | Enforcer |
|---|---|
| Router stable catalogs are the only topology and tenancy source of truth. | Router catalog transaction boundary |
| Provision owns request idempotency, effect progress, and receipts, but no graph RBAC or routing map. | Provision stable job store and API |
| A request cannot create twice after any successful management-canister call. | Persisted effect state and stored canister id |
| Concurrent requests cannot provision the same logical intent independently. | Provision intent lock |
| Completion means Router has acknowledged its canonical catalog update. | Provision `RouterAckPending -> Completed` transition |

## Alternatives

- **Router executes management calls:** rejected because it combines topology ownership with
  lifecycle execution and cycle/artifact policy.
- **Off-chain deployment only:** retained for local bootstrap, but rejected as the managed runtime
  protocol because it provides no durable idempotency or reconciliation.
- **Provision owns graph topology:** rejected because it duplicates Router state.

## Consequences

Provision adds a canister and a cross-canister saga, but each state has one owner and every
irreversible effect is resumable. Router APIs and stable layout require an implementation ADR/slice
before this proposal can be accepted.

## Implementation status

**Partially Implemented (2026-07-05).** This slice adds the Router-owned provisioning-request
catalog (three stable-memory regions and the `RouterProvisioningRequestStore` API) and all
ADR 0035 wire types (`ProvisionRequest`, `ProvisionResult`, `RouterProvisionAck`,
`ProvisionableResource`, etc.).

Slice 2 (2026-07-05) scaffolds the Provision canister: the `gleaph-provision` crate, the
deployment trust binding (`DeploymentBinding`), durable job/receipt state
(`ProvisionJobRecord`, `JobState`, `ResourceJobEntry`), Provision stable-memory regions 0–3
with the `PROVISION_STABLE_LAYOUT` registry, the `DeploymentTrustStore` and
`ProvisionJobStore` facades, and unit tests for idempotent insert, conflict detection,
state-machine transitions, intent locks, and governance authorization.

Slice 3 (2026-07-05) moves the six ADR 0035 Candid wire types into the neutral
`gleaph_graph_kernel::provisioning::wire` owner, adds the Provision ingress/query/ack handler
**foundation** (`accept_envelope_with_caller`, `query_job_with_caller`,
`router_ack_with_caller`) and a hand-written `provision.did` that defines the service surface.

Slice 4 (2026-07-06) implements the callable canister endpoints by adding `#[init]`,
`#[post_upgrade]`, `#[update]`, and `#[query]` annotations to
`crates/provision/src/lib.rs`; a thin `msg_caller()` shim in
`crates/provision/src/canister/handlers.rs`; `ic-cdk-macros` and `ic_cdk::export_candid!()`;
and a rewritten `provision.did` that declares `ProvisionIngressError`, `ProvisionInitArgs`,
and the named `ProvisionIngressResult` / `RouterAckResult` variant types. Durable bootstrap
persists across upgrades via the stable-memory-backed `DeploymentTrustStore` (StableBTreeMap
region 0); the durable bootstrap authority region for post-init installs is explicitly deferred
to a separate durable-authority slice.
`ProvisionJobRecord` gains `accepted_registry_version: Option<u64>` (round-trips inside the
existing `ProvisionJobStableRecord::V1` Candid body, no wrapper bump required for development
data). `ProvisionJobStore` extends `put`, `remove`, `intent_lock_count_for_record`,
`has_live_job_for_deployment`, and `insert_with_intent_locks`; the stale `get_by_request_id`
request-id-only scan is removed. Admin binding mutation via a public ingress surface is planned
for a separate durable-authority slice and is not implemented in this slice. Initial bindings are
seeded through `init(ProvisionInitArgs)` (durable-bootstrap model). `router_ack` uses the
exact canonical key `get_by_request(request_id, deployment_id)` and implements durable,
idempotent replay (`Completed` + matching version returns the ack; differing version returns
`AckConflict`; wrong state returns `InvalidState`).

`ProvisionableResourceKind` and `ProvisioningIntentKey` are single-sourced in
`gleaph_graph_kernel::provisioning` and re-exported by both `gleaph-router` and
`gleaph-provision`; `ProvisioningIntentKey::new` is public so both canisters can construct
the shared key. The `completed_effect_count` increment rule is provisional pending ADR 0035
implementation notes.
Slice 5 (2026-07-06) adds the Router outbound accept_envelope send (Router -> Provision cross-canister call), moving ProvisionAcceptResponse, ProvisionJobSummary, ProvisionIngressError, and ProvisionIngressResult into the shared gleaph_graph_kernel::provisioning::wire module and adding a Router-side provision_graph ingress endpoint with durable ROUTER_PROVISION_CONFIG stable rehydration. Slice 6 (2026-07-07) implements the Router-side receiver for the Provision -> Router `router_ack` callback, adds the `RouterAckResponse` wire type, extends `RouterError` with `AckConflict` and `InvalidState`, advances the Router-side `RouterProvisioningRequest` catalog from `AwaitingAck` to `Completed` with durable `accepted_registry_version`, replaces the zero-byte intent-lock marker with owner-identity-bound `IntentLockOwner` so preflight and release are owner-scoped, releases Router-side intent locks symmetrically with the Provision side, and adds four-branch invocation-owned rollback of the `AwaitingAck` record when `provision_graph`'s outbound `send_accept_envelope` fails (rollback only if the current operation inserted the record AND it is still in `AwaitingAck`; pre-existing `AwaitingAck`, `Completed`, and all other states are preserved). The Provision canister outbound cross-canister `router_ack` call remains deferred to Slice 6+; artifact catalog, lifecycle controller policy, and cycle algebra remain proposed.

Slice 7 (2026-07-07) implements the durable bootstrap authority region (`PROVISION_BOOTSTRAP_AUTH`, MemoryId 4) as a true `StableCell<Option<BootstrapAuthorityRecord>>` singleton and a separate per-governance audit log (`PROVISION_BOOTSTRAP_AUDIT_LOG`, MemoryId 5) as a `StableBTreeMap<Principal, BootstrapAuthHistory>`, adds the `ProvisionBootstrapAuthStore` facade, the `DeploymentTrustStore::admin_upsert` governance-agnostic overwrite method, the `admin_install_deployment_binding` #[update] ingress endpoint with the bootstrap-or-stored-governance decision tree, and the 10 unit tests plus 2 PocketIC scenarios that prove audit-before-return and upgrade durability. The deferred durable-authority slice from Slice 4 is now implemented.

Slice 8 (2026-08-16) implements Provision-side canister deployment execution. `accept_envelope`
is now async: after atomically reserving the job and its intent locks, it drives
`Reserved -> CreatePending -> CanisterCreated -> InstallPending -> Installed ->
RouterRegistrationPending` per resource, calling `create_canister` (controllers `[Provision,
governance]`) and `install_chunked_code` from the active release's verified artifact chunks.
`ProvisionRequest` gains `install_args: Vec<Vec<u8>>` (one Candid-encoded init args blob per
requested resource, constructed by the Router — the sole owner of logical topology);
`ProvisionAcceptResponse::Accepted`/`Replay` gain `created_resources: Vec<CreatedResource>` so
the Router receives each installed canister id and artifact hash. `RouterInitArgs`,
`GraphInitArgs`, and `IndexInitArgs` move to `gleaph_graph_kernel::provisioning::init_args` so
the Router (and Account, for first-Router issuance) can construct install args without depending
on the graph/graph-index crates. A no-active-release guard aborts before any management call,
leaving the job `Reserved` (the path PocketIC admission tests exercise). The Provision -> Router
outbound `router_ack` call remains deferred to a later slice; the Router advances to
`RouterAckPending` only after it has durably registered the returned canisters. ADR 0037
lifecycle (stop/delete/reconciliation) and ADR 0038 cycle reservation remain proposed.

Slice 8 also wires the Router side of the created-resources handoff. `provision_graph` registers
the provisioned graph and its shards into the Router catalog from `created_resources`:
`ProvisionGraphArgs` gains `owner`/`admins`, the graph registry entry is committed via
`admin_register_graph_with_random_key`, and each created Graph shard is registered via
`admin_register_shard`. Indexless shards (ADR 0054) are now expressible: `admin_register_shard`
accepts an anonymous index target (skipping the attach handshake and `index_cluster` entry), and
`verify_registry_invariants_after_commit` skips cluster-membership validation for anonymous-index
shards. `register_provisioned_graph` runs only on a fresh `Accepted` with non-empty
`created_resources`; a `Replay`/`Completed` does not re-register. The Property Index canister from
`created_resources` is paired with the Graph shard during registration. The `register_graph`
provisioned-mode fold (ADR 0056 Slice B) remains a separate integration surface; this slice
implements the seam's registration behavior.

Slice 9 (2026-08-16) wires on-demand vector provisioning into `CREATE VECTOR INDEX`. The vector DDL
path (`execute_vector_index_ddl_for_graph`) is now async. In provisioned mode, a `CREATE VECTOR
INDEX` on a graph with no vector target provisions a vector canister through the shared admission
flow (`requested_resources = [VectorIndex(0)]`) and registers the definition against that canister;
a graph that already has a vector target just reuses it. `provision_graph_flow` relaxes its
"GraphShard required" canonical-intent rule so a vector-only add-on provision is admitted without a
graph bootstrap; graph/shard registration runs only when the batch actually created a GraphShard.
Dev mode (no `provision_canister`) still registers a targetless `Registered` definition.

Slice 10 (2026-08-16) wires on-demand property-index provisioning into `CREATE INDEX`. The property
DDL path (`create_index`) is now async. In provisioned mode, a `CREATE INDEX` on a graph whose live
shard groups have no index canister provisions one index canister per unassigned group through the
shared admission flow (`requested_resources = [PropertyIndex(IndexClusterId(g)) for g in
unassigned_groups]`), assigns each to `index_cluster`, and retrofit-attaches every live shard in the
affected groups to its group's canister. Unlike vector (which separates provision from a manual
`attach_vector_shard` admin API), property-index attach runs inside `CREATE INDEX` because there is
no existing retrofit attach path for property index and the index canister is unusable until its
shards are attached. Dev mode (no `provision_canister`) still registers the definition indexless.
`unassigned_index_groups` keys off the shards' own `index_canister` (not `index_cluster`) so a group
whose canister was assigned but whose shards were not yet attached is re-provisioned idempotently on
retry; `attach_provisioned_index_canisters` is idempotent per group.

## Cross-links

- [ADR 0036](0036-versioned-wasm-artifact-catalog.md) — compatible release selection and artifacts.
- [ADR 0037](0037-provisioned-canister-lifecycle-and-controllers.md) — cleanup and controllers.
- [ADR 0038](0038-provisioning-authorization-and-cycles-funding.md) — admission and cycle reservation.
- [ADR 0054](0054-provisioned-logical-graph-topology-and-resource-activation.md) — bootstrap resource selection, logical-graph topology, and placement policy.
- Plan 0061b (release manifest + active-release pointer + activation) and Plan 0061c (install transfer + artifact audit log + PocketIC) build on the artifact catalog.

## Amendment: Account as bootstrap trust subject (planned)

**Status of this amendment: planned, not implemented.** The original ADR above is accepted and
partially implemented as written. This section records the changes required by the Account-canister
design ([ADR 0068](0068-account-canister-and-per-developer-router-issuance.md)) for the first-Router
bootstrap handover. Until implemented, the original sections remain authoritative.

### 1. Account as a transient trust subject

Today Provision accepts envelopes only from the Router principal registered for `deployment_id`
(above, §"Provision accepts envelopes only from the Router principal"). Under ADR 0068, for the
**first Router only**, the **Account** canister acts as the deployment's issuance authority:

- Provision must accept `accept_envelope` from the **Account principal** bound to the deployment as
  a bootstrap trust subject, in addition to the Router principal.
- This trust is **transient**: after the first Router is issued, the deployment trust binding is
  handed over to the issued Router principal, and the ADR 0035 model (Router owns the binding)
  applies unchanged.

### 2. First-issuance result callback to Account

Provision must deliver the first-issuance result to **Account** (so Account can
`register_router`), not only via the existing Router-bound `router_ack`. This is a new callback
surface on Provision. The existing `router_ack` to the Router continues to apply to subsequent
graph / shard / index issuance under ADR 0035.

### 3. `deployment_id` derivation

`deployment_id` is derived from `account_id` (Personal principal or Org generated id) per
[ADR 0068](0068-account-canister-and-per-developer-router-issuance.md). It remains the issuance and
trust-binding scope; it is not a new user-configured concept.
