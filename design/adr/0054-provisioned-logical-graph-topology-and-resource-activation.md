# 0054. Provisioned logical-graph topology and on-demand resource activation

Date: 2026-07-29
Status: proposed
Last revised: 2026-07-29 23:18:48 UTC +0000

## Context

Gleaph is composed of a Router, one or more logical graphs, and optional auxiliary canisters.
The Router is the external API and execution boundary and may control multiple logical graphs.
The logical graph is the canonical graph data domain. It is currently represented by one Graph
canister and is intended to become a federation of Graph shard canisters.

The existing provisioning protocol is defined by [ADR 0035](0035-provision-canister-and-issuance-protocol.md):
one service-wide Provision canister owns durable issuance jobs and receipts, while Router remains
the source of truth for graph identity, topology, tenancy, and routing. This ADR does not change
that ownership model or the existing Property Index contracts in [ADR 0019](0019-graph-local-shard-id-and-index-clusters.md)
and [the Property Index design](../index/property-index.md).

The previous bootstrap shape created Router, Graph, and Index resources together. That makes an
optional derived index a prerequisite for creating a usable logical graph, despite Graph being the
canonical data domain and index canisters being independently introducible. Subnet memory and
canister-count limits also prevent a universal co-location guarantee.

## Decision

### Service-wide provisioning authority

Gleaph has one Provision canister for the service. Provision is the durable executor for canister
issuance, installation, receipts, and the existing deployment trust bindings. It does not own
logical graph topology or Router routing catalogs.

`deployment_id` retains the meaning established by ADR 0035: it identifies the Provision/Router
issuance and trust-binding scope. It is not a replacement for `GraphId`, and one Router may own
multiple logical graphs within that scope.

### Logical-graph topology

The conceptual topology is:

```text
Gleaph service
└── Provision (single service-wide canister)
    └── Router(s) registered by the existing deployment binding
        └── Logical graph(s), each identified by GraphId
            ├── Graph shard group(s)
            │   ├── Graph shard canister(s)
            │   └── Property index canister(s), when enabled
            ├── Vector index canister(s), optional
            ├── Text index canister(s), optional and under research
            └── Procedure canister(s), optional
```

The terms have these boundaries:

- **Router** owns external ingress, authentication and authorization for Gleaph APIs, graph
  resolution, planning, routing, and result orchestration.
- **Logical graph** is the canonical graph data domain. Its `GraphId` and topology remain Router
  state, including graph-local `ShardId` allocation and index routing configuration.
- **Graph shard group** is the placement and routing grouping used by federated Graph shards and
  their Property Index canisters. The existing `GROUP_SIZE` / `index_cluster` formula remains the
  Property Index routing contract.
- **Property Index** is optional derived state. It is created and attached independently of the
  initial Graph bootstrap and follows its existing ownership, posting, attach, detach, and
  backfill contracts.
- **Vector Index** and **Text Index** are optional derived search services associated with a
  logical graph. The current Vector Index implementation has one target canister per logical
  graph; its post-federation fan-out is a future decision. Text Index is not yet a provisionable
  resource and its implementation and partition strategy remain future research.
- **Procedure** is an ordinary user-provided canister that may use the Router's Graph API and may
  also expose its own direct canister API. Gleaph standardizes the Router-facing Graph API and may
  provide placement and client/macro support; it does not impose the security model of a
  procedure's direct API.

### Bootstrap and on-demand resources

The initial logical-graph bootstrap request creates only:

```text
Router + default logical graph + first Graph shard
```

It does not create a Property Index, Vector Index, Text Index, or Procedure unless a future
request explicitly includes one of those resources under the applicable resource contract.

The target model is that additional resources are provisioned through subsequent requests using
the existing ADR 0035 idempotent issuance protocol. The current implementation does not yet
provide all of these request paths: Router `provision_graph` currently requires a `GraphShard`
resource in every request, and `ProvisionableResourceKind` currently contains only `GraphShard`,
`PropertyIndex`, and `VectorIndex`. The following are target behaviors:

- Property Index creation is requested when an index is needed; existing Graph shards are then
  attached and backfilled according to the Property Index contract.
- Vector and Text Index creation is requested independently of Graph bootstrap.
- Procedure creation or registration is requested independently once a procedure resource or
  registration API is introduced; the procedure canister's own direct API remains outside Router
  authorization unless it is invoked through a Gleaph API.

An indexless Graph shard is therefore a target logical-graph state. It is not representable by the
current `ShardRegistryEntry`, whose `index_canister` is mandatory and whose registration path
performs the Property Index attach handshake. Implementing this decision requires a Router
registry/registration change. Once an index is enabled, the existing attach and synchronization
contracts remain applicable.

This ADR defines the resource activation policy, not a new provisioning state machine. The
durable effect ordering and reconciliation rules remain those of ADR 0035. More detailed
resource-specific lifecycle transitions are deferred to the existing lifecycle and funding ADRs
and to the implementation slice for each resource kind.

### Placement policy

Provision attempts to place the following resources on the same subnet whenever the IC permits:

1. Graph shard group and its Property Index canister(s)
2. The complete logical graph, where capacity permits
3. Router and the default logical graph

Co-location is a target provisioning preference, not an unconditional invariant. The current
Provision implementation does not yet select or verify subnets. The current IC does not provide
a canister-group primitive that guarantees this placement, and subnet memory and canister-count
limits can make it impossible. Cross-subnet calls remain valid but may incur a substantial
latency cost.

## Ownership and invariants

| Concern | Owner |
|---|---|
| Service-wide issuance jobs and receipts | Provision |
| Deployment trust binding | Provision, under ADR 0035 governance rules |
| Logical graph identity and tenancy | Router |
| Graph shard registry and graph-local shard ids | Router |
| Property Index definitions and routing | Router; index canister owns postings |
| Canonical graph data | Graph shard canisters |
| Vector/Text derived search state | Respective index canisters |
| Procedure direct API security | Procedure implementation |
| Procedure calls through Gleaph Graph API | Router's existing security model |
| Same-subnet placement attempt | Provisioning policy; not a permanent IC invariant |

## Consequences

- After the required Router registry and provisioning changes, a logical graph can be created and
  used without paying the storage and canister cost of an optional index.
- Index creation becomes an explicit lifecycle operation and can be aligned with capacity and
  query requirements.
- Router and Graph registry invariants must support an indexless first shard; `index_cluster` may
  remain empty until a Property Index is activated.
- Existing index attach, posting synchronization, detach, and backfill contracts remain the
  source of truth and are not duplicated here.
- Provisioning is still a cross-canister workflow. Partial effects, retries, and reconciliation
  remain governed by ADR 0035 rather than by this topology document.
- Text Index partitioning, Vector Index federation, Graph federation strategy, and future
  canister-group placement guarantees remain open research topics.

## Implementation gaps

The following gaps must be closed before this ADR can describe the implemented deployment path:

- Make Router shard registration able to represent an absent Property Index and avoid an
  unconditional attach handshake.
- Add Router admission for Property Index or Vector Index requests that do not contain a new
  GraphShard.
- Extend or separate the provisioning model for Text Index and Procedure; neither is currently
  a `ProvisionableResourceKind` or artifact `CanisterKind`.
- Keep the current Vector Index invariant of one target per logical graph until federation policy
  is decided.

## Alternatives considered

- **Always provision Router, Graph, and Property Index together:** rejected because optional
  derived state becomes a bootstrap prerequisite and forces unnecessary resource allocation.
- **Let Provision own graph topology:** rejected by ADR 0035; it would duplicate Router's
  canonical graph and routing catalogs.
- **Treat Procedure as a special trusted execution domain:** rejected because a procedure is an
  ordinary canister; only Router-facing Gleaph APIs receive Gleaph's security contract.
- **Require same-subnet placement as an invariant:** rejected because current IC subnet capacity
  and canister placement primitives cannot guarantee it.

## Open research

- Vector Index federation and placement after Graph federation.
- Text Index implementation and partition strategy, including whether FST is appropriate.
- Cross-subnet result orchestration and latency policy.
- Detailed lifecycle transitions for resource addition, replacement, removal, and recovery.
- Router-visible procedure schema and GQL `CALL` registration contract.

## Cross-links

- [ADR 0035](0035-provision-canister-and-issuance-protocol.md) — issuance protocol and Provision ownership.
- [ADR 0019](0019-graph-local-shard-id-and-index-clusters.md) — graph-local shards and Property Index cluster routing.
- [ADR 0031](0031-vertex-embedding-store-and-derived-vector-index.md) — derived Vector Index direction.
- [Property Index design](../index/property-index.md) — existing posting and attach contracts.
