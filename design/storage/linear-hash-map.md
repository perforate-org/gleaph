# Stable Linear Hash Map

Status: **Partially Implemented** (the authoritative V1 map, bounded physical scan, Vector MemoryId
4 definition-store and MemoryId 7 SubjectStore cutovers, typed terminal delivery, and Graph
quarantine wiring are implemented; coordinated reset is fixture-only and production reset remains
pending)
Last updated: 2026-08-14
Anchor timestamp: 2026-08-14 09:04:32 UTC +0000

## Authority and status

[ADR 0067](../adr/0067-stable-linear-hash-map-production-contract.md) is the sole exact V1
persisted-format contract. This document intentionally does not repeat its byte-layout table or
define an alternative interpretation.

V1 is destructive and pre-release only. Strict create accepts zero-sized memory only; a nonempty
region must exact-open as V1 or be rejected unchanged. There is no alternate reader, conversion, or
reset-on-open-error path.

The map crate owns bytes, type-derived identity, address arithmetic, routing, split geometry,
mutation fencing, reset, physical scan, and scrub. A consuming facade owns its one MemoryId,
lifecycle state, seed authority, and domain-error translation. Bounded physical enumeration avoids
requiring a consumer-owned key catalog. Vector integrates the map through two private owners:
`DEFINITION_STORE` at MemoryId 4 and `SUBJECT_STORE` at MemoryId 7.

## Contract consequences

The exact byte ranges, fixed constants, and derivation formulas are specified only in ADR 0067.
This storage note records their consequences: ordinary nonempty open has fixed work and does not
scan map data; reset is an explicit owner-only destructive operation fenced by incarnation and
epoch; and scrub is bounded, handle-session integrity work rather than initialization or public
iteration. Its opaque cursor is replayable only on its originating open handle; reopen or upgrade
restarts at bucket zero. The separate physical-scan cursor is versioned and serializable across
exact reopen/upgrade. It persists schema/seed/incarnation/physical-bucket identity plus the next
slot and backward-relocation generation, never an epoch or derived geometry. Version 2 is 96 bytes;
an exact 88-byte version-1 cursor is accepted only as stale and requires restart. Each step has its
own even-epoch pre/post fence, so a scan does not claim a multi-call snapshot. The map derives layout
and geometry rather than persisting duplicate descriptors.

As of 2026-08-14 UTC, Vector owns MemoryIds 4 and 7 through separate private
`Uninitialized -> Ready | Unavailable` state machines. Fresh install strictly creates each LHM with
its own trusted install seed (`definition_map_seed` or `subject_map_seed`); `post_upgrade` seed-free
exact-opens each existing region. Every definition or subject point access is routed through its
owner, so unavailable state is never inferred to mean an empty catalog. Existing clustered-map or
unknown bytes are rejected unchanged and require a pre-release wipe/reinstall; there is no
migration, reader, or create-on-open-failure fallback.

`SubjectStore` is the only MemoryId 7 owner for `SubjectKey -> FixedSubjectMapEntry`, the canonical
subject freshness, deletion, and slot authority. Its durable `SubjectScanCursor` envelope carries a
version, one consumer scope, and the exact LHM physical-scan cursor bytes. It rejects an invalid
version, scope, length, or LHM cursor before a slot read. Detach plus rebuild Sampling, Building,
Cleaning, and Aborting use positive-budget physical `scan_step` pages; a short entry list is not EOF,
and split, reset, or a committed backward one-hop relocation invalidates the prior cursor. Direct
insert, overwrite, remove, and forward one-hop relocation leave that generation unchanged.

The Vector ownership cell at MemoryId 3, not LHM, owns the outer shard-detach lifecycle. Optional
config fields persist the checked-monotonic next generation and at most 64 active
`(shard_id, generation)` rows. The owner records/reuses the session before removing authorization,
rejects reattach until explicit subject-scan EOF, and validates the outer `ShardDetachCursor`
generation before decoding or stepping the inner `SubjectScanCursor` or touching subjects and
active/shadow rows. Legacy outer cursors decode with no generation but are rejected as stale. An
inner LHM restart retains the outer generation; LHM's backward-relocation generation prevents a
physical resident from moving behind a scan cursor, whereas the outer generation prevents
detach/reattach/detach ABA. This uses no new MemoryId and does not add a production reset path.

The owner keeps LHM `TablePressure` distinct from unavailable and other mutation failures. The
legacy `vector_sync_batch` Candid method still returns its unchanged progress shape. The additive
`vector_sync_batch_outcome` method admits a lazy definition and, for a new subject, the subject
record before any coupled row, page, tombstone, or deleted-list write. Only real owner pressure
returns `Terminal { applied, failed_index: applied }`, distinguished as
`IndexDefinitionTablePressure` or `SubjectTablePressure`; unavailable owners return the matching
outer lifecycle error before a committed prefix. Any later nonterminal failure traps so IC rollback
cannot hide a prefix behind an outer error. Graph validates the decoded outcome against the submitted
count, removes exactly the acknowledged prefix, and quarantines exactly the failed row with the
matching fixed reason without a legacy fallback after unavailable, malformed, transport, or reject
failures. The coordinated reset is internal and owner-only, but currently compiled only for
`cfg(test)` / `cfg(feature = "canbench")` fixtures. It preflights both LHM owners and resets the
definition-dependent regions while preserving router authority, graph ownership, shard catalog,
watermarks, and the GC cursor. Production reset and its rollback proof remain pending; no public
reset endpoint exists.

## Roadmap

1. **Phase 1a — implemented format/open slice:** exact V1 header/control, type-owned schema identity,
   strict create, immutable seed, and O(1) normal open.
2. **Phase 1b — implemented map-local lifecycle slice:** destructive owner-only reset and
   incarnation fencing. Consumer owners are responsible for coordinating their coupled regions;
   the Vector consumer demonstrates that coordination in test/canbench fixtures; production owner
   wiring remains pending.
3. **Phase 2 — partially implemented bounded maintenance:** bounded scrub and serializable bounded
   physical scan are implemented; a focused reopen benchmark remains planned with consumer
   lifecycle work.
4. **Phase 3a — implemented Vector ownership slice:** MemoryId 4 definition and MemoryId 7 subject
   destructive CHM-to-LHM cutovers, type-owned schema identities, private staged owners, distinct
   strict install seeds, seed-free post-upgrade open, and migration of production point accesses.
5. **Phase 3b — implemented Vector/Graph outcome slice:** additive typed
   `vector_sync_batch_outcome`, exact definition/subject `TablePressure` terminal mapping, Graph
   exact-prefix quarantine wiring, decoded-outcome validation, no-fallback failure handling, and focused
   terminal-at-zero/nonempty-prefix and malformed/transport unit tests are implemented. PocketIC
   proves live typed `Progress` delivery, legacy wire compatibility, and the unavailable no-write
   upgrade/rebind path in `unavailable_vector_owner_keeps_graph_delete_outbox_until_upgrade_rebind`
   (1 passed, 0 failed; focused runtime, 2026-08-14 UTC). Definition pressure remains proven at the
   owning Vector and Graph unit layers: a fixed-count sequential-key PocketIC fixture cannot
   deterministically force a specific definition collision after bounded one-hop relocation.
   `real_subject_table_pressure_is_terminal_and_graph_quarantines_exact_prefix_without_retry`
   provides the live real-pressure PocketIC proof for typed terminal-at-zero, exact Graph prefix
   acknowledgement, durable failed-item quarantine, and no pending retry. Coordinated reset is
   fixture-only; production reset and rollback coverage remain pending.
6. **Phase 4 — later work:** generic recovery only when a demonstrated multi-call mutation requires
   it. The bounded one-hop relocation is part of the current production map contract; an ordinary
   iterator and external key catalog remain intentionally absent.

## Current implementation and remaining boundary

The live crate implements the authoritative V1 header/control layout, strict create, seed-free exact
nonempty open, serializable bounded physical scan, and opaque handle-session bounded scrub. Physical
scan returns entries, next cursor, examined slots, and explicit exhausted state under a per-call
epoch fence; split/reset requires restart, while exact reopen/upgrade under the same incarnation,
physical bucket bound, and backward-relocation generation can continue. Exact legacy version-1
cursor bytes decode as stale and require restart. Scrub validates
reserved occupancy bits, canonical fixed encodings, routing reachability, duplicate candidate
placement, and the captured length under an exact pre/post schema/seed/epoch/incarnation/geometry
fence. Fixed-width canonical mismatches return typed scrub errors. User-defined decode, encode,
hash, or equality panics trap under the wasm panic-abort build and fail closed at the IC boundary;
they are not promised as typed errors. The map-local reset/incarnation lifecycle, both Vector owner
lifecycle boundaries, and fixture-only coordinated reset are implemented. Production reset ownership
remains pending. The additive endpoint exposes real definition and subject `TablePressure`, and
Graph's live IC client consumes those typed outcomes without a legacy fallback before applying the
owner-level outbox transition.
Within the map, an absent insert first attempts the bounded next-in-order split required by the
current threshold; a rejected prospective split then uses the same current-geometry deterministic
one-hop relocation as a below-threshold full pair. The planner scans no more than the two target
buckets, moves only a resident to its other candidate, and prepares all resident routing/page bytes
before the mutation epoch becomes odd. TablePressure remains the no-write result when neither path
can admit the key. A one-hop move to a lower physical slot checked-increments the persisted
backward-relocation generation before either resident page write while the epoch is odd; u64::MAX
rejects before any write. Reset publishes generation zero under its successor incarnation.
