# 0036. Versioned WASM artifact catalog

Date: 2026-07-04
Status: partially implemented
Last revised: 2026-07-08
Anchor timestamp: 2026-07-08 15:19:47 UTC +0000

## Context

Provision must install mutually compatible Router, Graph, Property Index, and Vector Index code.
Independent “active version” pointers can assemble an untested mixed deployment. Artifact bytes are
larger than a normal ingress payload and must be uploaded, verified, activated, and transferred in
bounded chunks.

The Provision canister itself is a bootstrap/recovery component. It must not select and install its
own replacement through the normal provisioning path.

## Decision

Provision owns an immutable artifact catalog and immutable compatible release manifests.

```text
ArtifactId = (canister_kind, semantic_version, sha256)
ArtifactMetadata = { artifact_id, byte_length, chunk_hashes, created_at_ns }
ArtifactUpload = { artifact_id, state, received_chunks, verified_at_ns? }
ReleaseManifest = {
  release_id,
  router_artifact,
  graph_artifact,
  property_index_artifact,
  vector_index_artifact
}
```

`ArtifactMetadata` and `ReleaseManifest` never change after publication. Upload and verification
progress lives in `ArtifactUpload`; verification failure does not mutate immutable metadata. A single
atomic `active_release_id` selects the compatible set used for new issuance. Activation succeeds only
when every referenced artifact is fully present and SHA-256 verified.

Provision issuance receipts pin `release_id` plus every selected `ArtifactId`. Changing the active
release never changes an existing job or receipt.

### Bounded upload and install transfer

The governance/recovery principal configured at bootstrap uploads catalog chunks through a bounded,
idempotent API. Repeating an artifact id resumes or returns its existing upload; conflicting metadata
for the same id is rejected. Provision stores chunks in stable memory, checks each declared chunk
hash, then streams the whole logical artifact through a full SHA-256 verification before marking the
upload verified.

For target installation, Provision uses the IC management-canister chunk path:

1. send bounded bytes from the stable catalog with `upload_chunk` to the target canister's chunk
   store;
2. retain the returned chunk hashes in job state;
3. call `install_chunked_code` with the expected full WASM hash and pinned install arguments;
4. clear/reconcile chunk-store state as required by the platform API.

Per the management-canister contract, the `install_chunked_code` caller must control the
`store_canister` (or be that canister), and the `store_canister` and installation target must reside
on the same subnet. Provision also remains a controller of the issued target under ADR 0037. These
are store/target and controller constraints, not a claim that the ingress caller must share a subnet.
The implementation must not reassemble the entire WASM in one heap buffer merely to call
`install_code`. Exact chunk sizes and costs are implementation-time platform inputs, not fixed here.

Provision's own WASM is excluded from `ReleaseManifest`. Governance/recovery installs or upgrades
Provision through a separate bootstrap procedure.

External archives such as IPFS or Arweave may later back cold artifacts, but HTTP-outcall retrieval,
trust roots, and cache reconciliation remain planned. A release cannot activate from an external
reference alone.

## Ownership and invariants

| Invariant | Enforcer |
|---|---|
| Published artifact identity and release membership are immutable. | Provision catalog write API |
| Exactly one compatible release set is active for new jobs. | Atomic `active_release_id` update |
| Activation references only fully verified local artifacts. | Release activation validator |
| An issuance job uses the release and hashes captured at acceptance. | Provision job/receipt store |
| Provision cannot self-upgrade through normal issuance. | Catalog kind validation and bootstrap boundary |

## Alternatives

- **Independent active pointer per kind:** rejected because it permits unsupported mixed releases.
- **Mutable artifact record with verification states:** rejected because content identity and upload
  workflow have different lifecycles.
- **Embed all WASM in Provision:** rejected because upgrades require reinstalling the executor and
  duplicate artifact bytes.
- **External archive as the primary source:** deferred until retrieval and trust semantics are
  designed.

## Consequences

Release activation becomes a deliberate atomic operation, and receipts remain reproducible. Stable
chunk storage and target chunk-transfer bookkeeping add operational state, but avoid unbounded
reassembly and version skew.

## Implementation status

**Partially Implemented (2026-07-09).** Slice 8a added three stable regions (MemoryId 6, 7, 8) for the
artifact catalog, upload state, and verified canonical chunks. Slice 8b added two stable regions
(MemoryId 9, 10) for the release manifest and active release pointer, plus the `release_publish`,
`release_activate`, and `release_get_active` public ingress methods. Slice 8c (2026-07-09) adds
one stable region (MemoryId 11) for the artifact audit log using append-oriented (Principal,
sequence) keys (R5 strict), plus the `release_install` public ingress method that performs the
bounded cross-canister `upload_chunk` + `install_chunked_code` path against the IC management
canister, and 4 new PocketIC E2E scenarios. ADR 0036 is now Fully Implemented modulo external
archive / HTTP outcall (still deferred to a future slice).

## Cross-links

- [ADR 0035](0035-provision-canister-and-issuance-protocol.md) — jobs pin the selected release.
- [ADR 0037](0037-provisioned-canister-lifecycle-and-controllers.md) — target controller policy.
- [IC management canister](https://docs.internetcomputer.org/references/management-canister/) — authoritative chunk and install operations.
- Plan 0061b (release manifest + active-release pointer + activation) — builds on Slice 8a.
- Plan 0061c (install transfer + artifact audit log + PocketIC) — builds on Slices 8a and 8b.
