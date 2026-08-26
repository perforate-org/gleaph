# 0087. WASM ingestion operations model

Date: 2026-08-26
Status: accepted (operational model decided; implementation slices pending)
Last revised: 2026-08-26
Anchor timestamp: 2026-08-26 03:57:36 UTC +0000

## Context

[ADR 0036](0036-versioned-wasm-artifact-catalog.md) gave Provision an immutable,
content-addressed artifact catalog (kind + semantic version + SHA-256 identity, chunk-hash
verification, one full streaming verification, durable `verified` flag, atomic compatible-set
activation) and the chunked management-canister install path. The structure is implemented and
exercised end to end by PocketIC scenarios. What was never decided is the **operating model** on
top of that structure:

1. How artifact bytes enter the catalog outside PocketIC tests. No production or CLI upload
   surface exists; the dev CLI installs data-plane wasm directly via the management canister,
   bypassing the catalog.
2. Who executes which step under evolving governance. Today every catalog write requires the
   single bootstrap governance principal, coupling byte transfer to release approval.
3. Which tool owns which operation surface. The `gleaph` CLI is a developer (data-plane) tool;
   platform operations have no owner.

Internet Computer facts relevant to governance evolution (verified against official documentation
on 2026-08-26): an adopted SNS proposal is executed automatically by the SNS governance canister
as a declared method call (generic proposals carry validator + target methods); the SNS root
canister becomes sole controller of dapp canisters; large wasm upgrades use pre-uploaded chunks
in a store canister referenced by hash list inside the proposal, because ingress bounds exclude
embedding multi-megabyte wasm; the SNS framework's own upgrades pull approved versions from the
SNS-W catalog. Ingestion-by-prior-push plus hash-referencing decisions is therefore the platform's
own pattern, not a Gleaph invention.

## Problem

- Byte transfer (hundreds of bounded ingress calls per artifact) cannot be carried by DAO voting
  granularity; approval must attach to hash declaration and activation only.
- Governance key material must not migrate into the distributed developer CLI.
- Without a canonical pipeline, dev and production drift into different deploy paths.

## Existing architecture assessment

Existing concepts absorb the change without new subsystems:

- Catalog integrity is enforced independently of uploader identity (declared chunk hashes +
  server-side streaming full SHA-256 → durable `verified`). Role separation needs no storage
  change.
- Authorization resolves through the durable authority record at one checkpoint per handler, so
  governance evolution is a record swap, not a code rewrite.
- Install already streams from stable memory; no path requires reassembly.

## Alternatives considered

- **Extend the developer CLI with admin commands** — rejected: persona and key-material
  separation; consumer tooling would carry governance-key handling.
- **Dedicated chunk-store canisters now** — rejected until stable-memory growth or parallel-upload
  pressure is demonstrated; the burden of proof is on the new subsystem.
- **External archive as primary source (HTTPS-outcall retrieval)** — remains deferred per ADR
  0036; the IC's own large-wasm flow validates push-plus-hash-reference, and pull adds
  install-time failure modes and trust-root design without removing the push need.
- **Chosen: staged operating model on the existing catalog** (below).

## Decision

### Surfaces

| Surface | Persona | Scope |
| --- | --- | --- |
| `gleaph` CLI (unchanged) | app developer | data-plane only; local launcher convenience; no artifact commands |
| `gleaph-operator` (new binary, crate `crates/operator`) | platform operator | artifact publish-metadata / upload-chunk / get-status; release publish / activate / get-active; release-install to an explicit target; binding install; audit-history readback |
| SNS generic function (Stage 2) | DAO | proposal declares manifest hash set + activation; validator runs at submission, target revalidates at execution |

One shared ingestion client library is the single source of truth for protocol logic: chunk
splitting, SHA-256/chunk-hash computation, publish → upload → status → activate ordering, and
idempotent resume, with transport behind a trait so the logic is unit-testable off-IC. The crate
is `gleaph-artifact-api` (`crates/artifact-api`), following the neutral `gleaph-*-api` family
convention (candid + serde dependencies only). Consumers: `gleaph-operator` first; local launcher
seeding later.

### Canonical upload pipeline

Build (postprocess script) → split ≤1 MiB chunks → compute chunk hashes and the full SHA-256
locally → `artifact_publish_metadata` → `artifact_upload_chunk` × N (idempotent resume) → poll
`artifact_get_status` until server-side streaming verification marks the artifact verified →
`release_publish` → `release_activate`. No new candid endpoints are required.

### Stage model

| Stage | Governance form | Byte-push executor | Required changes |
| --- | --- | --- | --- |
| 0 (now) | single governance principal | same principal via `gleaph-operator` | slices: shared library, operator tool |
| 1 | committee/multisig canister principal | same | authority record swap only |
| 2 | SNS DAO | registered publisher principal | extend upload authorization to "authority OR registered publisher" (single checkpoint; deliberately deferred until multi-party operation starts); proposals declare hashes + activation only |

Native SNS `UpgradeSnsControlledCanister` proposals are not used for Gleaph kinds: they are
per-canister and permit unsupported mixed sets — the same failure mode ADR 0036 rejected for
independent per-kind active pointers.

### Invariants preserved across stages

1. Hash declaration plus activation is the atomic, release-scoped trust-critical act.
2. Byte-transfer integrity is enforced independently of uploader identity.
3. Authorization resolves only through durable authority indirection; no principal is hardcoded.
4. Upload authorization remains a single extensible checkpoint (authority OR future publisher set).

### Explicitly deferred

- Publisher-role implementation code (end-state recorded above only).
- Retention/GC policy for superseded artifact versions (separate decision).
- Upgrade orchestration body ([ADR 0037](0037-provisioned-canister-lifecycle-and-controllers.md)
  remains proposed; `release_install` is the landing spot).
- HTTPS-outcall retrieval from external archives.
- Bootstrap-tier commands (Account/Provision self-install/upgrade): excluded from slice 3 and
  delivered as their own slice **before the first production (mainnet) operation**. Slice 3's
  transport layer must be shaped so management-canister calls can later reuse it.

## Consequences

Production ingestion becomes executable end to end; dev/prod parity rests on one library and one
candid contract; DAO migration reduces to configuration plus one small recorded authorization
extension; receipts stay reproducible through hash pinning. Trade-offs: two binaries depend on one
new library crate (accepted — the shared library prevents path drift), Stage 2 requires a
recorded-but-unbuilt authorization extension, and `gleaph-operator` is a new maintenance surface.

## Migration

No storage-layout or candid changes. Slice order: (1) this ADR, no code; (2) shared ingestion
client library crate; (3) `gleaph-operator` binary; (4) local launcher convergence — `network
start` seeds the catalog through the library and direct management-canister install remains
bootstrap-tier only (Account/Provision initial deploy). Slice 4 is independent of the
GAP-2026-08-24-006 resolution.

## Design documentation impact

Updated in the same patch: [ADR 0036](0036-versioned-wasm-artifact-catalog.md) cross-link and
revision stamps; `design/architecture/account-and-provisioning.md` platform-layer tool pointer;
`design/implementation-gaps.md` GAP-2026-08-24-006(c) decision note.

## Implementation status

**Bootstrap tier implemented (2026-08-26).** `gleaph-operator` ships the deferred
Account/Provision self-install commands (`bootstrap deploy account|provision`,
`bootstrap upgrade … --target`, `bootstrap status --target`) on the slice-3 `IcIngress`
transport with destination `aaaaa-aa` — no second transport stack. Safety model: plan/confirm
(`--yes` gates execution; dry runs print the exact step list), pre-upgrade and post-upgrade
`module_hash` display next to the local wasm SHA-256, and no auto-start after a failed
upgrade (the target stays stopped with resume instructions). Management wire shapes come from
`ic-management-canister-types` (verified against the official management did); the
`canister_status` reply is a local hand mirror because current crate versions require reply
fields the validated replica generation does not send. Init arguments: Provision uses a JSON
mirror of `ProvisionInitArgs`/`DeploymentBinding` (`--init-args-hex` remains the universal
escape hatch); Account init takes no arguments. Known transport gap: ic-agent 0.49.2 cannot
attach cycles to ingress calls, so mainnet deploys fail fast (fee-free endpoints work);
PocketIC's E2E (`adr0087_bootstrap_tier`) drives create → chunked install → stop → upgrade →
start with exact module-hash verification through a management-transport adapter over real
replica ingress calls. Environment finding recorded there: PocketIC cannot route
ingress-level `create_canister` (the adapter's local equivalent is
`provisional_create_canister_with_cycles` with identical controllers).

**GAP-2026-08-24-006(a) resolved (2026-08-26).** The launcher gateway *can* route
management-canister updates; the observed failures were operator-side effective-canister-id
routing. `gleaph-operator bootstrap deploy` now uses `provisional_create_canister_with_cycles`
for local/PocketIC endpoints and sets the effective canister id per call: the target canister
for `upload_chunk`/`install_chunked_code`/`stop`/`start`/`canister_status`, and the network's
default effective canister id (read from the `/_/topology` endpoint, the same source dfx's
`dfx info default-effective-canister-id` uses) for the provisional create, whose response
certification requires the effective id to fall within the target subnet's canister ranges.
Verified end to end against a locally launched launcher network.

**Upgrade-durability defect found by this slice, root cause verified
(GAP-2026-08-26-005).** Provision's seeded authority (MemoryId 4) and active-release
pointer (MemoryId 10) do not survive any real wasm upgrade. The cause is provision-side,
not the replica: eager thread-local constructors (`stable/bootstrap_auth.rs:26-38`,
`stable/memory.rs:111-116`) build their cells with `StableCell::new(memory, None)`, which
writes on construction (`ic-stable-structures` cell.rs:174-176), so every process restart
rewrites the durable cells empty; stable memory itself is preserved byte-for-byte
(25,231,360 bytes measured unchanged across upgrade). The read-or-create form
`StableCell::init(memory, default)` — already used safely for the storage-id cell in the
same module set — is the confined fix, plus post-upgrade survival assertions in
`adr0087_bootstrap_tier`. Must land before Provision's first production upgrade;
upgrade orchestration otherwise remains ADR 0037 territory.

**Defect fixed (2026-08-26): both Provision cell constructors now use `StableCell::init`
(read-or-create), and `adr0087_bootstrap_tier` asserts survival — a pre-upgrade published +
activated release and its audit rows survive the chunked upgrade, governance readback stays
authorized, anonymous stays rejected. See GAP-2026-08-26-005 for pinned tests.**

**Slice 3 implemented (2026-08-26).** `gleaph-operator` (`crates/operator`) ships the platform
operator binary on top of the slice-2 library: the clap command surface (`artifact ingest` /
`artifact status`, `release publish` / `release activate` / `release get-active`,
`canister install` to an explicit target, `binding install`, `audit history`), a generic
any-canister/any-method IC ingress layer whose endpoint, root-key, and PEM identity handling
follow the dev CLI conventions, and the `ArtifactTransport` implementation over that layer
that feeds the shared ingest driver. A PocketIC E2E (`adr0087_operator_ingestion`) drives the
driver against the real Provision canister through a transport adapter and round-trips every
operator mirror type, proving wire compatibility end to end. Bootstrap-tier management-canister
commands stay excluded per §Explicitly deferred; slice 4 (launcher convergence) remains
pending.

**Slice 2 implemented (2026-08-26).** `gleaph-artifact-api` (`crates/artifact-api`) ships the
neutral candid+serde+sha2 wire mirror, the bounds-mirror constants, the pure planning pipeline,
the AFIT transport trait, and the idempotent ingest driver with resume from every observed
upload state.

## Cross-links

- [ADR 0035](0035-provision-canister-and-issuance-protocol.md) — jobs pin the selected release.
- [ADR 0036](0036-versioned-wasm-artifact-catalog.md) — the catalog this model operates on.
- [ADR 0037](0037-provisioned-canister-lifecycle-and-controllers.md) — upgrade orchestration is
  its own future decision; `release_install` is the landing spot.
- [ADR 0038](0038-provisioning-authorization-and-cycles-funding.md) — cycle funding; catalog
  storage funding remains open there.
- IC references, verified 2026-08-26: [SNS proposals](https://internetcomputer.org/docs/building-apps/governing-apps/managing/making-proposals),
  [System canisters (SNS-W, root)](https://internetcomputer.org/docs/references/system-canisters),
  [Launching an SNS](https://internetcomputer.org/docs/guides/governance/launching/).
