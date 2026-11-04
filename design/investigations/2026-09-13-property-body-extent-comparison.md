# Property bodies: native contiguous-extent comparison

Status: Experimental — native correctness/I/O comparison complete; production integration unselected.

Follow-up: the [managed IC comparison](2026-09-13-property-body-extent-canbench.md), verified on
2026-09-13 20:50:20 UTC +0000, adds91 query cases. The native measurements below remain unchanged;
the shared source also contains managed IC fixtures. No production acceptance follows.

Native reuse update (2026-09-14 01:10:06 UTC +0000): the [full PropertyStore comparison](2026-09-13-property-store-native-comparison.md)
reuses this allocator with a private prepare/publication seam and one shared ownership audit. Real
keys, Graph Values, directories and moves are measured separately; all36 extent and260 B-tree raw
rows reproduce. The changed source is not IC-remeasured codegen. The
frozen0357 sources (measurement dump, not retained) preserve exact managed
reproduction; the numbers below remain body-only evidence.

Design anchor: 2026-09-13 12:48:39 UTC +0000.
Native verified: 2026-09-13 14:02:33 UTC +0000.
Repository anchor: `95d7910878a5ee8af95af878f2b0df952edd70dc`.

## Decision

The dedicated body placement experiment removes the repeated body-node saves observed in the
[B-tree comparisons](2026-09-13-property-body-chunk-size-sensitivity.md). A same-size 1 MiB replacement
writes 1048592 bytes, including arena tags, versus 5559316/6860248/9047736 bytes with 4/16/64 KiB
chunks. Repeated hot shrink writes 260 bytes across arena and free index, **but still reads the full
old 1 MiB value**. These are native Memory-interface observations, not IC instruction or latency ratios.

The result supported the separately bounded IC follow-up, **not production adoption**. The extra free
index doubles the raw minimum backing in the short-body fixture; fragmentation still forces logical
growth; relocation retains old+new capacity; and the allocator adds substantial correctness code.
No directory threshold, MemoryId, production codec, allocator cap or Graph API is selected.

A short inventory of installed `ic-stable-structures = 0.7.2` found public `insert`, `remove`,
`pop_first` and `pop_last` operating per key, and `clear_new` resetting the entire map. There is no
public selected-node batch replacement/removal API to reuse for the measured full-body operations.
The inventory stopped there: no copied B-tree internals, fork or further chunk-width sweep.
[Source pinned to the installed revision](https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap.rs).

## Implemented boundary

[Extent source](../../crates/graph/src/facade/stable/property_store_probe/body_extent_probe.rs) implements the
[experimental extent design](2026-09-13-property-body-extent-design.md#4-experimental-arena-format).
That document owns the format and allocation rules; this report records implementation and evidence.

- One private `Arena` owns raw body memory and a separate existing B-tree free index. The index
  contains fixed size/offset keys and unit values, never body bytes or per-size vectors.
- A caller-owned `BodyAddress` supplies location/encoded length. Tags own capacity/state; the index
  supplies fit lookup. The heap owner map is only an oracle/descriptor stand-in, **not a persisted
  directory or a correctness mechanism for a future canister**. There is no duplicate stable length map.
- `change` admits combined old/new lengths before metadata/body reads, acquires the exact previous
  bytes, plans bounded span/index edits and validates their coverage before commit. Empty replacement
  deletes; absent-plus-empty and stored-empty descriptors reject.
- In-place reuse, shrink/split/absorb, right-free growth and frontier extension precede relocation.
  Retirement resolves post-allocation neighbors, including a selected old-left free span. The plan
  holds at most four original/three final spans and three removed/two inserted free keys. Those are
  logical edits, not five physical B-tree writes or a complete instruction bound.
- The first grow/index/arena write begins commit. There is no recoverable `Err` afterward; unexpected
  failures panic. New bytes precede the caller descriptor, then retirement. **Native panic is not
  rollback**, and this infallible descriptor publication does not establish directory/Graph atomicity.
- Pair creation/reopen rejects aliased or half-initialized memories. Reopen checks the owning arena
  header and calls upstream `load`, without scanning/rebuilding the free index. Full scans exist only
  in oracles. Touched corruption is rejected; forged interior aliases, stale reused addresses, a wrong
  caller length still within capacity, and arbitrary upstream corruption are not universally detected.

At this native checkpoint, the extent module was a native test child of the existing
[B-tree probe](../../crates/graph/src/facade/stable/property_store_probe/body_probe.rs). Only its scope comment and child
inclusion changed; all reference algorithms, Wasm exports and fixture controls remain unchanged.
Both share `CountedMemory`, `Io` and the position-bearing payload generator. No production Rust,
workspace dependency/lock, LARA storage or Graph path changed.

## Measurement contract

The retained extent CSV (measurement dump, not retained) has 36 rows:
32 matched density/neighbor/hot rows and four fragmentation rows. All 260
reference rows (measurement dump, not retained) reproduce exactly.
The `case_input` column is the byte-size axis for density/neighbor/hot rows and the initial hole
count F for fragmentation rows; it is not universally a byte length.
The four missing-prefix controls per B-tree configuration have no allocator equivalent: absence
lookup belongs to the excluded directory. Whole ordinary point-get controls remain visible.

Operations use the same sizes, key order and changing payloads as the native reference. Replacement
always obtains the full old value and changes every new byte; it does not elide unchanged chunks.
Reads return the caller-declared bytes after the owner's checks. The extent owner does not perform
a B-tree prefix/ordinal walk; that work is intrinsic to the reference, not silently charged to an
excluded directory. Raw body lengths are not Graph Value-envelope lengths.

Counters bracket operations only. Setup, payload generation, full partition/index/value audits,
reopen and reporting are outside the captured counters. Oracle updates/assertions inside closures
make no Memory calls, but this is **not a timing or peak-heap benchmark**: their allocations and the
lifetime of returned vectors would matter in an IC port. `body_buffer_bytes` counts previous/read
body buffers only, not all allocator, B-tree, output, oracle or input heap use. Memory size calls
are not included in the read/write counters. The previous-value contract is matched; there is no
claim that allocator descriptors and B-tree body-prefix lookup have identical APIs.

### Large body and neighboring body

Native neighbor-series state, one selected 1 MiB body beside a 65-byte guard:

| Owner | Large read calls / bytes | Large rewrite read calls / bytes | Large rewrite write calls / bytes |
| --- | ---: | ---: | ---: |
| Whole | 2076 / 1060002 | 4154 / 2120087 | 1045 / 1048708 |
| C4KiB/default | 2925 / 1066618 | 33878 / 6861313 | 10472 / 5559316 |
| C16KiB/default | 2270 / 1061530 | 25936 / 8093525 | 8241 / 6860248 |
| C64KiB/default | 2123 / 1060356 | 21467 / 10217406 | 9407 / 9047736 |
| Extent | 4 / 1048656 | 4 / 1048656 | 3 / 1048592 |

All extent numbers in this table are arena I/O; the free index has zero I/O for these operations.
The body occupies the frontier in this native history. Equal-size replacement changes no allocation
geometry. Body payload is one contiguous read/write; remaining calls read/check the arena header and
tags or write tags. This does not predict the cost of fragmented allocation or a relocating mutation.

For the selected 65-byte body beside 1 MiB:

| Owner | Read bytes | Rewrite read bytes | Rewrite write bytes |
| --- | ---: | ---: | ---: |
| Whole | 11509 | 1071594 | 1048708 |
| C4KiB/default | 1100 | 18620 | 16582 |
| Extent | 145 | 161 | 81 |

The extent's 4-read get and 6-read/3-write replacement touch no free-index records and no neighboring
payload. They remain unchanged as the neighboring body grows through 65/4096/65536/1048576 bytes.
Whole ordinary point-get has the same native read calls/bytes in these two-body fixtures, but no
assembly buffer. Its much faster **IC** result from the earlier experiment is not erased by the
native byte table; no extent IC point-get comparison has run.

### Hot growth/shrink and retained capacity

After the initial 65 B → 1 MiB → 65 B sequence, reopen the pair and repeat four growth/shrink pairs.
The following write bytes include both extent memories where applicable:

| Owner | Retained backing bytes | Grow write bytes | Shrink write bytes |
| --- | ---: | ---: | ---: |
| Whole | 1114112 | 1134526 | 66350 |
| C4KiB/default | 1179648 | 12455003 | 10409385 |
| C16KiB/default | 1179648 | 10867263 | 9582735 |
| C64KiB/default | 1114112 | 7814173 | 9605237 |
| Extent: arena + index | 1179648 | 1048684 | 260 |

The extent arena retains 1114112 bytes (17 pages), and its index retains 65536 bytes (one page).
All eight repeated operations have zero growth in both memories. Growth reads 282 bytes across
20 calls; shrink reads **1048672 bytes** across five calls, including the old 1 MiB body. Shrink's
260 written bytes are 97 arena bytes plus 163 index bytes; they are not its total execution cost.
Final deletion leaves no owners and one coalesced Free span, without reducing physical memory.

This history avoids replacement-body doubling, but not every history can reuse capacity. The
relocation contract fixture blocks a 64 KiB body's frontier with a larger-than-hole guard, grows
the body to 1 MiB, and asserts allocation at the previous logical end. Old+new space exists before
retirement; afterward the old capacity stays free below the new frontier. No universal footprint
advantage or reclaim-to-the-platform claim follows.

### Short-body density and free-index cost

For 256 bodies of 65 bytes (16640 payload bytes), all default-geometry B-tree controls retain one
65536-byte memory. The extent owns **two** 65536-byte memories, even when its free index is empty.
Its live span bytes are 22528, and logical arena end is 22592; caller/directory storage is excluded.
A future small-cell threshold can remove these bodies from the lane, but no threshold is selected.

| Operation | Whole read / write bytes | Extent arena read / write bytes | Extent index read / write bytes |
| --- | ---: | ---: | ---: |
| Insert256 | 301162 / 282299 | 22520 / 22776 | 121 / 92 |
| Get256 | 107514 / 0 | 37120 / 0 | 0 / 0 |
| Rewrite256 | 294417 / 124136 | 41200 / 20736 | 0 / 0 |
| Remove128 alternating | 286082 / 262933 | 23656 / 2048 | 157370 / 40244 |
| Refill128 after reopen | 116039 / 103034 | 10240 / 10368 | 144621 / 38783 |

Free-index traffic dominates the extent's remove/refill metadata work, despite no second growth.
The earlier declared-max density penalty (2 pages for C256, 27 for C4096) remains in the reference;
it is not necessary to choose that unfavorable geometry to show the extra extent-region floor.
Native raw floors exclude MemoryManager. The conditional production region-floor estimate remains
in the [design's accounting](2026-09-13-property-body-extent-design.md#6-work-and-space-accounting),
not a measured production allocation.

### Fragmentation: sufficient total free space is not a fit

Prepare alternating 33-byte bodies and 65-byte guards, then remove the small bodies and reopen.
There are F separate 56-byte holes. A new 65-byte body needs a larger contiguous span, so it appends
even though aggregate free capacity is sufficient. Release the first guard outside counters to
coalesce the first two holes, then allocate a 160-byte body from the resulting 200-byte span.
The unusably small remainder is absorbed; the final allocation does not extend logical end.

| F | Initial free span bytes | No-fit index reads / bytes | Coalesced-fit index reads / bytes | Coalesced-fit index writes / bytes |
| ---: | ---: | ---: | ---: | ---: |
| 8 | 448 | 18 / 175 | 55 / 465 | 23 / 171 |
| 128 | 7168 | 46 / 501 | 144 / 1479 | 29 / 211 |

No-fit arena I/O is 4 reads/88 bytes and 4 writes/89 bytes in both cases; logical end increases by
88 bytes to 1304/18584. Coalesced-fit arena I/O is 3 reads/80 bytes and 3 writes/176 bytes; final
free spans are 6/126. Neither case grows a physical page because existing capacity suffices.
These observations distinguish **logical fragmentation growth** from physical page growth and show
nonconstant metadata cost. They do not measure worst-case fragmentation, latency or tree rebalance.

## Correctness and implementation cost

Four new tests run alongside the two unmodified B-tree tests:

- `extent_lifecycle_preserves_partition_and_reuse`: exact same-address reuse; split/absorb;
  right-free consumption; an 8-byte frontier extension; trailing-Free extension rather than append;
  old plus insufficient trailing-Free growth; coalescing; lowest-offset exact-fit tie; reopen;
  and 256 deterministic mixed operations with exact partition/index/owner audits after each step.
- `extent_relocation_uses_post_allocation_neighbors`: old-left allocation both with a split and
  exact consumption; exact resulting free keys and preserved guard bytes; forced later relocation
  with old+new retained capacity; reopen and full audit.
- `extent_rejects_before_effects_and_reopens_without_rebuild`: exact/one-short combined budgets,
  deletion/create admission, representability, bad descriptor/header/tag/free key, double release,
  alias and both half-pair directions. Rejections preserve both full memory vectors and caller
  descriptor; budget rejection performs no I/O, and successful reopen performs no writes/growth.
- `extent_native_comparison_and_fragmentation`: the 36 retained measurements, full oracles, separated
  holes, repeated post-reopen capacity reuse and zero growth on reads.

One complete suite creates nine raw arenas/nine free indexes and reopens nine pairs. Together with
27/21 reference B-tree creations/loads: **36 B-tree creations, 30 successful same-type loads**.
Failed header/pair attempts are separate; no canister/federation installation occurs. Fixtures stay
within 256 live bodies, 1 MiB per body and 8 MiB live payload, with no shared global live fixture.

The draft plan/test boundary was recorded before source edits. Supplementary detailed notes were
written after core source and before the first test execution. Initial three contracts passed;
then all six tests and all measurements passed. Review strengthened frontier, tie, relocation and
malformed-input cases. A new relocation assertion initially failed because its tiny guard reused
an existing hole rather than blocking the frontier. The fixture now uses a larger-than-hole guard
and asserts its address. This was a fixture-assumption repair, not a production defect or weakened
assertion. An isolated deliberate stale-left-neighbor mutant fails the relocation contract with
`Err(Span)`; the checked implementation retains the post-allocation lookup. This is an oracle check,
not evidence of native rollback or a previously deployed bug.

Final native suite: six passed, 1.31s build + 2.96s runtime. All 260 reference and 36 extent rows match
the first complete successful run. Scoped Clippy passed in 0.36s after two ordinary lint repairs;
rustfmt passes. Final review also made the missing-free-key rejection assert preservation of the
actual guard descriptor passed to `change`, rather than an unrelated outer descriptor. No timeout,
thread override, production build or workspace test.

Lizard1.23.0, explicit two-artifact scope: the added extent file has 36 functions, maxCCN22,
sumCCN185 and function NLOC953 (including fixtures). Owner code accounts for 25 functions/NLOC423;
`validate` CCN22/NLOC54 and `change` CCN18/NLOC47 are the main review points. `commit` has five
parameters including self. Bounded staged geometry is justified by the stale-neighbor case and
pre-effect consistency; no generic overlay, policy flags, public transaction API or metric-only
helper splitting was added. The existing reference has unchanged 32 functions/CCNsum68/NLOC649.
This maintenance burden must be justified alongside I/O gains before production selection.

## Reproduction and provenance

Run from the repository root, using a temporary standalone manifest; do not add it to the workspace:

```sh
root="$PWD"
probe="$(mktemp -d $TMPDIR/gleaph-extent-native-repro-XXXXXX)"
cat > "$probe/Cargo.toml" <<EOF
[package]
name = "gleaph-property-body-extent-probe"
version = "0.0.0"
edition = "2024"
publish = false

[lib]
path = "$root/design/investigations/artifacts/2026-09-13-property-body-btree-probe.rs"

[dependencies]
ic-stable-structures = "=0.7.2"

[profile.dev]
debug = "line-tables-only"

[profile.test]
debug = "line-tables-only"
EOF
cargo test --offline --manifest-path "$probe/Cargo.toml" --target-dir "$root/target" --lib -- --nocapture
cargo clippy --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$root/target" --lib --tests -- -D warnings
rustfmt --edition 2024 --check design/investigations/artifacts/2026-09-13-property-body-{btree,extent}-probe.rs
```

The measured isolated lock resolves ic-stable-structures0.7.2 and ic_principal0.1.5; verify those
versions for exact replay (the generated lock remains in the evidence directory). Compiler:
`1.100.0-nightly (c656540d6 2026-08-21)`, aarch64-apple-darwin. No Wasm artifact was built/measured
in this native checkpoint. The following hashes identify that revision, before managed IC additions;
all36/260 native numeric rows are reproduced by the follow-up source.

- Shared source SHA256: `f63f3139d0b277e7d8700209b678947482eda28e60a59ba3c90b31c42862d362`.
- Extent source SHA256: `5979ffd82f454563736cf8e2852c3c8664268ffb0f9081d6dbe3e910b1bc3a9a`.
- Evidence: `$TMPDIR/gleaph-extent-native-slan7c4t/`: draft/contract/native/lint logs, failed fixture and
  deliberate mutant logs, Lizard, initial/final snapshots, arithmetic/scope checks and review.
- Same-assistant implementation/review only; no independent or security approval.

The managed IC follow-up uses the same allocator with matched manager/policy conditions and
old-value acquisition; setup, validation, oracle allocations and returned-vector destruction stay
outside timing. Its91-case result is separate evidence, not an IC interpretation of the native
counters above. The prior48-case raw B-tree IC results, including shrink-time+1 stable page, remain
unchanged; those earlier results are not extent measurements.

Production additionally requires persistent directory/Value/move ownership, whole-row interacting
allocation preflight, numerical tree/allocator/workspace limits, the Graph fence/outbox and durable
positive/zero/rejected outcomes, coherent fresh-state activation and actual lifecycle/rollback/upgrade
validation. Bulk edge SET remains unavailable. Static demo counts still do not supply workload
frequencies or justify treating the 143-byte sample maximum as a product limit.
