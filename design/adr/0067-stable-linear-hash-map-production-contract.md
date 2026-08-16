# ADR 0067: stable linear hash map V1 production contract

Date: 2026-08-14
Status: **Implemented for pre-release use**
Anchor timestamp: 2026-08-14 21:52:20 UTC +0000

## Decision

Use one destructive, final V1 format for `ic-stable-linear-hash-map`. The map is created only on
zero-sized memory and exact-opened thereafter. Any nonempty region without this format is rejected
without writes. There is no migration reader, compatibility mode, fallback create, or V2 format.
Before first production deployment, an owner may wipe and recreate its region.

The map owns layout, schema validation, routing, split-pointer geometry, mutation fencing, bounded
maintenance, scan cursors, scrub, and map-local reset. Consumers own their memory bindings, seeds,
availability state, and domain error mapping.

## Persisted bytes

All integers are little-endian. The immutable header is 128 bytes:

| Range | Content |
|---|---|
| `0..3` | `LHM` magic |
| `3` | layout version `1` |
| `4..8` | fixed key width |
| `8..12` | fixed value width |
| `12..16` | zero reserved bytes |
| `16..32` | key storage schema ID |
| `32..48` | key routing schema ID |
| `48..64` | value storage schema ID |
| `64..72` | immutable hash seed |
| `72..88` | `GLEAPH-LHM-V1` fingerprint |
| `88..128` | zero reserved bytes |

The mutable control is 64 bytes at offset 128:

| Range | Content |
|---|---|
| `0..8` | logical length |
| `8..16` | physical bucket count |
| `16..24` | even/odd mutation epoch |
| `24..32` | incarnation |
| `32..40` | split debt |
| `40..48` | inline-overflow entry count |
| `48..64` | zero reserved bytes |

Level and split cursor are derived from the physical bucket count. A bucket block is five fixed
pages: one primary page plus two inline overflow pages. Each page is an occupancy word, a key
slab, and a value slab. The block is addressed from the header widths; no page map or free-list is
persisted.

## Admission and split debt

For an absent key, insertion first searches both candidate blocks, including their two overflow
pages. If both blocks are full, it plans the next standard linear-hash split, redistributes the
source block under the next geometry, and places the requested entry in the resulting images.
The complete plan, checked arithmetic, and required stable-memory extent are prepared before the
odd mutation epoch or any logical write.

An overflow insertion increments `split_debt`. `maintenance_step(entry_budget, byte_budget)`
performs multiple split-pointer splits while both budgets allow. It returns:

- `Idle { debt_remaining }` when no debt exists;
- `Progress { splits, moved_entries, moved_bytes, debt_remaining }` after one or more committed
  splits; or
- `Pending { debt_remaining, required_entries, required_bytes }` when the next complete source
  block does not fit the caller's budget and no split is written.

`TablePressure` is reserved for bounded admission failure after inline overflow and standard split
planning cannot place the key, or for a real capacity limit. Ordinary primary-bucket fullness is
not a terminal error. A prewrite error leaves logical bytes and the even epoch unchanged; an
unexpected post-write failure leaves an odd epoch and is trapped at the IC update boundary.

## Open, reset, scan, and scrub

`create` validates fixed widths and schema identities, grows the initial eight-bucket extent, and
writes the initial control. `open` validates the fingerprint, schemas, reserved bytes, control,
and extent in O(1) work. `init_with_hash_seed` creates only when memory size is zero.

`reset(expected_incarnation)` is an explicit owner operation. It preflights the expected
incarnation, successor arithmetic, epoch, and initial extent before clearing the initial bucket
blocks. It preserves the immutable header and never repairs an incompatible region.

`scan_start`/`scan_step` use an exact 88-byte cursor containing schema IDs, seed, incarnation,
physical bucket bound, and next physical slot. A positive physical-slot budget returns entries,
the next cursor, examined-slot count, and an explicit `exhausted` flag. Split or reset returns
`RestartRequired`; same-geometry mutations between completed calls are permitted without a
multi-call snapshot. There is no ordinary iterator and no external key catalog.

`scrub_snapshot`/`scrub_step` are separate handle-bound integrity operations. They validate
occupancy, fixed encodings, routing reachability, duplicate placement, and the captured length
under an exact epoch/incarnation fence.

## Gleaph ownership boundary

Vector binds MemoryId 4 (`DEFINITION_STORE`), MemoryId 7 (`SUBJECT_STORE`), and MemoryId 9
(`VECTOR_PARTITION_HEADS`) through private `Uninitialized -> Ready | Unavailable` owners. Fresh
install uses trusted definition/subject seeds with strict create; post-upgrade uses seed-free exact
open. Raw map handles do not escape the owner. Existing bytes from another format remain unavailable
and require the intentional pre-release reinstall described above.

`VECTOR_PARTITION_HEADS` is derived state (partition page chains + `next_page_id` allocator), so its
coordinated reset uses the map's `clear` (which preserves the incarnation and hash seed) rather than
`reset(expected_incarnation)`. The defs/subjects owners are canonical and use the incarnation-fenced
`reset` path.

The additive typed Vector batch maps real definition/subject admission pressure to terminal item
outcomes and owner availability failures to an outer result. Graph validates the outcome against
the submitted count and quarantines only a terminal failed item. The legacy batch endpoint remains
unchanged. The coordinated Vector reset is fixture-only; production reset and rollback proof are
not part of this V1 implementation.

## Verification

The LHM unit suite covers exact fingerprint rejection, inline overflow round-trip, split-debt
persistence and budget reporting, physical scan reopen/reset behavior, and scrub coverage. Vector
unit tests cover both owner lifecycles, rebuild/scan consumers, typed unavailable handling, and
coupled reset fixtures. The LHM benchmark compares Linear Hash Map and `StableBTreeMap` with the
same 4,096-entry `u64 -> u64` fixture for get, insert, and remove, plus one split-debt maintenance
case. Setup is isolated from measured memory and the persisted benchmark artifact is updated only
by an intentional unfiltered `canbench --persist` run.

The final V1 bucket block uses two inline overflow pages. A 4,096-entry comparison
against the previous four-page block measured Linear get 17.82M instructions (-33.78%), insert
61.96M (-40.61%), remove 20.77M (-33.08%), and maintenance 11.25M (+7.03%); BTree totals were
unchanged. The resulting numbers were written by the required unfiltered `canbench --persist` run
to the checked-in benchmark artifact.
