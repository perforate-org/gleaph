# Stable Linear Hash Map storage

Status: **V1 implemented for pre-release use**
Anchor timestamp: 2026-08-14 21:52:20 UTC +0000

[ADR 0067](../adr/0067-stable-linear-hash-map-production-contract.md) is the sole byte-level
authority. This note records ownership and operating consequences; it defines no alternate format.

## V1 boundary

The map is fixed-width, two-choice, and linear-hashed. A fresh map writes a 128-byte header and a
64-byte control record. The header contains the literal V1 fingerprint and type-owned schema IDs.
The control stores length, physical bucket count, mutation epoch, incarnation, split debt, and
inline-overflow count. A logical bucket is one primary page plus two overflow pages in the same
map-owned stable-memory region. There is no migration reader, compatibility interpretation, second
arena, free-list, or external key catalog.

Strict create is zero-memory-only. Nonempty open validates the fingerprint, schema IDs, widths,
reserved bytes, control invariants, and allocated extent without scanning entries. An incompatible
region is unavailable to its owner and is not cleared or recreated. The only supported pre-release
recovery is an intentional destructive reinstall.

## Admission and maintenance

Point operations inspect the two candidate bucket blocks. Overflow absorbs ordinary bucket fullness
and increments persistent `split_debt`. `maintenance_step` services several standard split-pointer
splits under independent entry and byte budgets. It returns `Idle`, `Progress`, or `Pending`; the
latter means the next source block does not fit the caller's budget and no split was written.

The split planner builds complete source/new images and reserves the final stable extent before the
first logical write. Geometry is published after those images are written. `TablePressure` is
therefore an admission failure after bounded overflow and split planning are exhausted, not the
normal result of a full primary page.

## Cursors and integrity

The serializable physical scan cursor is exactly 88 bytes in final V1. It carries schema identity,
seed, incarnation, physical bucket bound, and next slot. A positive slot budget returns entries,
the next cursor, an examined-slot count, and explicit EOF. Split and reset make a cursor stale;
same-geometry mutations are allowed between completed steps without snapshot guarantees. The
handle-bound scrub cursor is separate and validates occupancy, fixed encodings, routing, duplicate
placement, and captured length.

## Consumers

Vector owns MemoryId 4 (`DEFINITION_STORE`) and MemoryId 7 (`SUBJECT_STORE`) through private staged
owners. Install uses each trusted seed and strict create; post-upgrade uses seed-free exact open.
All point and scan access goes through the owner. The Vector typed batch surface maps owner
`TablePressure` to a terminal item outcome and owner unavailability to an outer result; Graph
quarantines only the failed terminal item. The existing legacy batch endpoint remains a separate
wire contract.

Vector's coordinated reset remains an internal test/canbench fixture. Production reset and its
rollback proof are intentionally deferred until an administrative owner contract is approved; no
public reset endpoint or map-format migration is part of V1.

## Benchmark contract

The LHM canbench compares Linear Hash Map and `StableBTreeMap` using identical 4,096-entry `u64`
fixtures for get, insert, and remove. Setup and assertions are outside measured closures, and a
separate maintenance case measures split-debt service. The fixture never constructs two maps over
the same memory, which avoids the `NonEmptyMemory` initialization trap.

The selected V1 block has two inline overflow pages. In the persisted 4,096-entry run this
reduced Linear get/insert/remove instructions by 33.78%/40.61%/33.08% versus the previous
four-page block; maintenance increased 7.03%. The BTree comparison was unchanged. These values
are now the checked-in baseline from the required unfiltered `canbench --persist` run.
