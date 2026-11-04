# Sidecar pre-read bounds: dependency and storage options

Status: Investigation complete — property-value separation design prioritized; no production change selected or implemented.

Design direction revised: 2026-09-13 03:57:44 UTC +0000. See the
[value-separation design and experiment](2026-09-13-property-value-separation-design.md).
The fixed-block experiment completed at 2026-09-13 06:23:23 UTC +0000 without selecting a production layout.

Verified: 2026-09-13 01:52:13 UTC +0000.
Repository anchor: `95d791087`, with the uncommitted bounded LARA readers described in
[ADR 0050](../adr/0050-lara-traverse-read-api.md#bounded-complete-inline-property-collection-implemented).

## Finding

The installed `ic-stable-structures` 0.7.2 API cannot enforce a caller's sidecar read allowance before
materializing values. The obstacle is not only the absence of an encoded-value length accessor:
**loading a node reconstructs its whole overflow-page list before searching its keys**. A large
unselected value can therefore increase work even for a missing key or a key-only iterator.

A small native experiment reproduced this with valid storage and a fixed ten-byte selected value.
It does not show corruption or a violation of the ordinary B-tree API, which promises no caller read
budget. It identifies an additional prerequisite for bounded bulk edge SET; that feature remains
unavailable under the [selected first-execution/all-match contract](2026-09-12-bounded-edge-membership-witnesses.md#selected-direction-and-storage-accounting).

## Existing owners and call path

- [EdgePropertyStore](../../crates/graph/src/facade/stable/edge_properties.rs) stores fixed 14-byte
  `EdgePropertyKey`s and unbounded `StoredPropertyValue`s. Graph owns key identity and value meaning;
  it should not parse upstream B-tree pages to recover hidden lengths.
- [StoredPropertyValue](../../crates/graph/src/facade/stable/vertex_properties.rs) owns the shared
  vertex/edge value envelope: version byte 1 followed by binary Value bytes. Vertex keys are eight
  bytes; the same unbounded value codec is used. `ensure_persistable` in
  [property/persisted.rs](../../crates/graph/src/property/persisted.rs) checks encoding, not a byte cap.
- In upstream [`BTreeMap::get` and `traverse`][map], a node is loaded before key search, and the found
  value is extracted before `V::from_bytes`. A check inside `StoredPropertyValue::from_bytes` is too
  late to bound that encoded buffer's allocation or stable-memory read.
- [`LazyEntry`][iter] exposes `key`, `value`, and `into_pair`, not a value-length admission operation.
  Calling only `key()` avoids loading the value body, but not node loading.
- [`Node::load_v2`][v2] calls `read_overflows` first. That helper reads the initial overflow address,
  then [one packed 11-byte magic/next header per linked page][overflow] into a growing address vector until NULL.
  These pages belong to the **whole node**, not just the requested value.
- [`load_value_from_memory`][node] allocates from the stored length before reading the body. V2 node
  I/O uses the reconstructed page list to translate offsets. A length accessor added only after node
  loading would leave the earlier work unbounded by the caller.

The primary sources above are pinned to commit `72b02ab65b6c6fbf53fbabf752732d2726539377`, identified
by the installed release's `.cargo_vcs_info.json`. The experiment compiled the exact 0.7.2 package.
No claim about a later release or upstream willingness to accept an extension is made.

## Native read experiment

[Probe source](artifacts/2026-09-13-sidecar-read-probe.rs): one diagnostic test, four independent
maps, four initializations and four reopens. Each map has two `[u8;14]` keys and unbounded `Vec<u8>`
values. The selected value is always ten bytes; only the other value's length changes. Reopen and
fixture writes happen before instrumentation is reset.

The three measured operations are an exact tiny-value `get`, a missing-key `get`, and one key-only
`range(...).next()`. Exact returned bytes/absence/key are asserted. A full large-value `get` is the
positive instrumentation control. The observer delegates reads unchanged; it does not reject,
truncate, return synthetic bytes, catch traps, or parse storage headers.

| Unselected body bytes | Tiny get calls / bytes | Missing get calls / bytes | Key-only calls / bytes | Full-body control bytes |
| ---: | ---: | ---: | ---: | ---: |
| 16 | 7 / 61 | 6 / 51 | 6 / 51 | 67 |
| 4,096 | 11 / 105 | 10 / 95 | 10 / 95 | 4,191 |
| 65,536 | 71 / 765 | 70 / 755 | 70 / 755 | 66,291 |
| 1,048,576 | 1,042 / 11,446 | 1,041 / 11,436 | 1,041 / 11,436 | 1,060,012 |

The tiny reads' largest individual read stayed **14 bytes**. The larger total is node metadata
work, not accidental reading of the neighboring payload. The missing/key-only results make the same
point without any selected value body. Backing VectorMemory sizes were 1/1/2/17 Wasm-sized pages;
these are fixture sizes, not allocation high-water or production storage estimates.

Limits of the evidence: this is a native upstream primitive specimen, not Graph's facade or Value
codec. It measures Memory-interface calls/bytes, not actual IC syscalls, instructions, time, allocator
usage, policy execution or whole-row SET. A single two-entry node proves neighbor dependence; it
does not measure multi-level search, mutation/rebalance costs, or worst-case graph workloads.

## Alternatives

| Option | Benefit | Required cost / limitation |
| --- | --- | --- |
| **A. Bounded read in the existing B-tree owner** | Can preserve the stored layout and ordinary property-size contract; adds no per-edge/vertex identity bytes | Requires an upstream release or maintained dependency patch. Must bound node/overflow discovery **and** encoded payload allocation/read, not only expose value length. Small requests can still hit a work limit because of large neighboring values. |
| **B. Global persisted-value size cap** | Writer-enforced cap can provide a conservative bound on valid node sizes without a separate length map | Changes ordinary property writes, not only bulk SET. Needs a complete writer audit and explicit activation/fresh-state decision; existing data cannot simply be assumed compliant. Charging a worst-case body/node for every small lookup can reject useful work. No numerical cap is selected. |
| **C. Out-of-line property bodies** | A fixed-size key-to-body reference can expose length before body reads and isolate key lookup from unrelated body size | New canonical property layout, body allocator/reclamation and co-write invariants; explicit fresh-state/reinstall contract. Adds reference/allocation costs per stored property, even though adjacency need not widen. Size/throughput benefits have not been measured. |

The local `StablePagedOrderedMap` accepts u64/u64, and `StableLinearHashMap` requires fixed-width
entries. Neither is an existing drop-in owner for variable property bodies. Adapting one would be a
storage redesign, not a wrapper around the existing getter.

Rejected shortcuts:

- A separate per-property length map duplicates knowledge and creates write/GC/move consistency work;
  it also does not bound overflow discovery in the unchanged body map.
- A custom `Storable` check executes after extraction. Switching to key-only iteration avoids the
  body, not the node's overflow traversal.
- A Memory wrapper with synthetic reads changes storage semantics. `Memory::read` cannot return a
  typed limit error, and allocation can precede the read call. A trap/catch strategy is not a retained
  deterministic rejection contract for Wasm canister execution.
- Parsing the dependency's format in Graph creates a second schema owner. Vendoring only a duplicate
  read decoder in a new Graph helper would have the same problem.
- Capping the entire memory region would bound a much larger scope than one request and make bulk
  availability depend on total graph size; it is not a useful substitute for bounded local work.

## Recommendation and remaining gates

The investigation at the verification anchor recommended **A** as the minimum layout-preserving
change. A correct extension would need to charge node discovery and payload extraction before work,
without putting an upstream page parser or GQL semantics into Graph.

The design direction revised above prioritizes **C: independent property bodies behind bounded
B-tree cells**, as specified in the [value-separation design](2026-09-13-property-value-separation-design.md).
The native experiment demonstrates removal of neighbor-body coupling from both reads and node
rewrites, rather than merely rejecting expensive work. It does not establish Graph/Wasm performance
or whole-row atomicity. Its fixed-block/replacement-body candidate is not selected for production
because of short-body waste and retained high-water; common-path preparation cost also needs careful
integration. See that design's results for the measured trade-offs and remaining allocator decisions.

No dependency fork, patch section, published PR, new MemoryId or persistent value format has been
introduced. An ADR and explicit fresh-Graph activation are required if the storage redesign is
adopted. A remains an alternative, not an already authorized workspace dependency change.

Even a correct bounded encoded read is not whole-row mutation preflight:

1. Graph must bound Value decoding/workspace and all eligibility/policy reads, using its concrete
   types and numerical allowances. A generic `Storable` callback is not automatically heap-bounded.
2. [`Node::save_v2`][write] loads every entry and rewrites all values in the node. Read-side admission
   alone does not bound insertion/rebalance, outbox growth or total write work. The write owner must
   establish those preconditions before the canonical fence; returning Err after writes is not rollback.
3. Positive/zero/deterministic-rejected outcomes, retained replay, row-versus-edge counts, and Router
   prefix/Abort integration still need the existing journal/receipt owners. No identity registry is
   needed for first-execution all-match selection.

## Reproduce

From the repository root, create an isolated manifest; do not add the probe to the workspace:

```sh
repo="$PWD"
probe="$(mktemp -d /tmp/gleaph-sidecar-read-XXXXXX)"
printf '%s\n' \
  '[package]' 'name = "gleaph-sidecar-read-probe"' 'version = "0.0.0"' \
  'edition = "2024"' 'publish = false' '[lib]' \
  "path = \"$repo/design/investigations/artifacts/2026-09-13-sidecar-read-probe.rs\"" \
  '[dependencies]' 'ic-stable-structures = "=0.7.2"' \
  '[profile.dev]' 'debug = "line-tables-only"' \
  '[profile.test]' 'debug = "line-tables-only"' > "$probe/Cargo.toml"
cargo test --offline --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --lib -- --nocapture
cargo clippy --offline --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --lib --tests -- -D warnings
rustfmt --edition 2024 --check design/investigations/artifacts/2026-09-13-sidecar-read-probe.rs
```

The run at the verification anchor passed one test and scoped Clippy. No production Rust was changed,
so Graph/LARA/PocketIC/workspace tests and canbench were not rerun. The workspace lock is unchanged;
the probe's lock is confined to its temporary manifest directory.

[map]: https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap.rs#L651-L683
[iter]: https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap/iter.rs#L454-L485
[v2]: https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap/node/v2.rs#L112-L139
[node]: https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap/node.rs#L255-L279
[overflow]: https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap/node/v2.rs#L301-L319
[write]: https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap/node/v2.rs#L217-L298
