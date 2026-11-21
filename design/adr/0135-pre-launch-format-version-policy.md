# 0135. Pre-launch format version policy: one current version (1)

Date: 2026-11-21
Status: accepted
Last revised: 2026-11-21
Anchor timestamp: 2026-11-21 08:23:37 UTC +0900

## Context

Gleaph is not deployed and has no production data. Durable formats and cross-canister
contracts had nevertheless accumulated version numbers from pre-launch evolution:
`WIRE_VERSION` reached 7, the vector rebuild-pool header reached 4, the spill-run layout
reached 3, and several `V1`/`V2` stable envelopes carried readers that converted the older
bytes instead of rejecting them. The same layout was described with different numbers in
different documents (rebuild pool: 1, 2, 3, and 4; vector definition schema: 02, 03, and 04).

Those numbers do not describe deployed contracts. They make every reader carry migration
code for state that cannot exist, and they hide the current layout behind a history of
pre-launch slices. [ADR 0007](0007-stable-memory-layout.md) already requires reinstall when
the single canonical layout changes; [ADR 0039](0039-production-stable-memory-evolution-and-upgrade-safety.md)
is proposed and its production N-1 gate is not in force.

## Decision

1. Every durable format and cross-canister contract exposes exactly **one current version, `1`**.
   This includes stable headers, per-record schema bytes, Candid envelope variants, wire
   versions, and persisted codec versions.
2. A pre-launch layout or contract change keeps that version at `1` and requires a fresh
   install. It must not add a reader, decoder, migration, or fallback for the superseded bytes.
3. A pre-launch API shape change replaces the current shape. It must not keep an optional
   field, a legacy sender test, or a compatibility branch solely so an older sender still
   decodes.
4. After launch, a format or contract change bumps the version and adds the migration or
   compatibility path that the release claims. The version field or single-variant envelope
   is kept now precisely so that bump remains possible.
5. Monotone revision counters and data-model discriminants are **not** format versions and are
   never reset: `ServiceConfig.version`, `index_version`, `active_index_version`,
   `registry_version`, `model_version`, `policy_version`, graph mutation ordering stamps,
   `levels`, partition-level tags, and enum discriminants.
6. User-facing schema migrations (`migration-api`, Router `schema_migration`, inline value
   schema migration) are product features, not pre-launch format compatibility.

## Consequences

- Development state must be reinstalled after this reset; no migration is provided.
- The current implementation is the version-1 definition. The Router, inference canister,
  Graph, Graph-index, Vector, Text, and Provision canisters are deployed as one fresh set.
- Tests that proved an old version decodes are removed. Tests that prove an unknown version
  fails closed remain, using a value that cannot be the current version.
- Design contracts state the current version once. Dated investigation and evidence records
  are not rewritten; they quote the numbers that were current when they were produced.

## Alternatives considered

- **Keep the accumulated numbers and add migrations.** Rejected: it preserves readers for
  state that has never existed in a deployment and contradicts the repository's
  pre-production simplicity rule.
- **Remove version fields entirely.** Rejected: it would remove the post-launch evolution
  room that the same rule requires.
- **Reset only the storage headers and leave the wire version at 7.** Rejected: the wire
  contract has the same pre-launch status and the same absence of deployed peers.

## Migration

Fresh reinstall of every canister. No stable-memory migration, no dual decode, no compatibility
shim. The implementation change resets the constants and schema identifiers, collapses
`V1`/`V2` envelopes to `V1(current)`, and deletes old-version and old-sender tests.

## Design documentation impact

- [ADR 0007](0007-stable-memory-layout.md): revision row pointing here.
- [stable-memory-inventory.md](../storage/stable-memory-inventory.md): rebuild-pool and vector
  definition schema versions.
- [vector-index.md](../index/vector-index.md): definition record size and schema identifier.
- ADR 0079, 0082, 0097, 0114, 0116, 0117: current version statements.
- [security/rbac-and-prepared.md](../security/rbac-and-prepared.md): predicate encoding version.
