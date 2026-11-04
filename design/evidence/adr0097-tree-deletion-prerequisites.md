# Property-bearing Tree-CSR deletion: blocked prerequisite

Status: **investigation complete; implementation paused, not fixed or approved**.\
Anchor: 2026-09-22 07:29:43 UTC +0000.\
Base: `7ca767bf3580674b9cf8b18d425d17f0ef0512be`; main remains
`9a64049f47fc07542446d651d4b09421636b73c5`, unmerged.

## Decision

Do not remove the width-zero assertion in `tree_mode_remove_edge_at_slot` yet.
A temporary assertion-only diagnostic reached two additional failures through real
public deletion: incorrect already-dead admission and a property-demotion panic after
the tombstone commit. All executable changes were removed byte-for-byte. GAP-2026-09-22-001
remains open, with prerequisites [003/004](../implementation-gaps.md#gap-2026-09-22-003--tree-demotion-does-not-preserve-property-and-movement-contracts).
No threshold, layout, policy, header, observer handler or benchmark changed.

This does not reopen the approved slab-compaction admission repair `90757825c`.
Its tests and fixtures remain unchanged. GAP-002 metadata validation is separate;
root IDs are still the sole addressing source. Overall ADR0097, atomicity, lifecycle,
PocketIC, performance and landing are not approved.

## Runtime evidence

All commands ran in `$REPO` with the normal target directory,
through one serialized 600-second ledger protecting 120 seconds before every non-final
command. No timeout, reset, process control, cold target, benchmark or persistence run occurred.
Seven recorded commands charged **36.611981041845866 / 600 seconds**, with zero startup
charge and **563.3880189581541 seconds unused**. Both witness-only formatting commands
and the draft/final plan validators exited 0. Final structural validation is not repair approval.

| Observation | Source | Command filter (`cargo test -p ic-stable-lara --lib … -- --nocapture`) | Exit / elapsed | Result |
|---|---|---|---|---|
| Public deletion red | Base + witness v1 | `property_tree_delete_public` | 101 / 11.183s | 0 passed, 1 failed, 668 filtered; stale width assertion, 33 versus 0 |
| Assertion-only diagnostic | Base minus that assertion + unchanged v1 | `property_tree_delete_` | 101 / 13.966s | 1 passed, 2 failed, 666 filtered; four owner cases pass, public no-op and demotion fail |
| Complete no-op discrimination | Same diagnostic + witness v2 | `property_tree_delete_public` | 101 / 7.712s | 0 passed, 1 failed, 668 filtered; all three no-op state checks complete, exact result assertion fails |

The second command is **not a green implementation**. Compilation completed in all three
commands (9.36s, 11.57s and 6.19s respectively); runtime was 0.03s, 0.68s and 0.05s.
The existing 15 test-target warnings remain, with no new diagnostic-module warning.
There was no compile/setup failure in this slice.

The initial saved setup failure remains at
[`adr0097-tree-compaction-setup-before.txt`](sources/adr0097-tree-compaction-setup-before.txt).
It was not overwritten or treated as a fresh run.

### Real backing and fixture cost

The diagnostic reuses `tree_compaction_fixture` and `InlinePropertyTestEdge` /
`VectorMemory`; there is no new complete-graph specialization. The fixture's real
1024-position tree at vertex 0 / raw label 3 has an edge leaf and multiple property
leaves, a slab sibling with 16 edges and a peer vertex with one edge. Promotion initially
transcribes holes at 0, 3, 511 and 1023. The diagnostic fills these holes with the
existing Unordered writer, checking each returned position and assigning distinct targets
`40000 + slot`. Thus the later deletion/reuse concerns a **newly created** hole, not an
old fixture hole. Other targets are `1000 + slot`.

Values encode `u64(target) + 1` in the first eight bytes and `0xa5` in the remainder;
8/33-byte values are injective and independent of destination positions. Expected native
rows enumerate all original slots from construction, not from the observed final traversal.
Public tests additionally maintain external identities at original slots, removing only
the deleted key and applying only reported moves. No expected-map reset is permitted.

Three diagnostic tests were added temporarily, then removed. The combined command
constructed eight native graphs: four owner-matrix graphs, plus populated forward and
empty reverse graphs for each of the two public-wrapper tests. Each separate public run
constructed two more graphs: **12 native constructions over the three commands**, zero
PocketIC constructions/installs. Empty reverse stores make this a forward API witness,
not a complete bidirectional/counterpart or Graph facade integration test.

### Above-threshold deletion and policy matrix

`property_tree_delete_public_preserves_native_rows` deletes slot 125 through
`remove_forward_edge_at_slot_with_move`. With only the stale assertion removed, before
its no-op assertion fails it verifies:

- Removed target 1125 / slot 125 and no survivor movement events.
- Degree 1024 → 1023, extent 1024 unchanged, global count 1041 → 1040;
  leaf slab-live count stays 17.
- Exact 1024 native rows in both directions, including the new tombstone's **retained
  nonzero property bytes**. The deletion owner writes no property bytes; zeroing them
  would be a new behavior, not this owner's current contract. Promotion's zero-filled
  properties for source holes are a different operation.
- Same root bytes and descriptor except degree. Every minted LTB header/payload equals
  its snapshot except the selected four-byte edge marker and the edge-leaf tombstone
  count. Actual producer metadata is preserved, not asserted to validate owner/ordinal.
- Distinct referenced block IDs, correct block kind/level/tombstone counts, root/free-span
  disjointness, unchanged peer/sibling vertices, descriptors, backing and properties.
- Every live canonical slot still resolves its independently assigned external identity.

`property_tree_delete_owner_policy_matrix` passes all four `(width, policy)` cases:
`(8, Insertion)`, `(8, Unordered)`, `(33, Insertion)`, `(33, Unordered)`.
Each deletes slot 125 through the single-orientation dispatch, rejects a one-byte
replacement with exact `InlinePropertyBytesWidthMismatch`, verifies unchanged source
snapshots, exercises out-of-range deletion and repeated deletion without state change,
then inserts a different target/value. Insertion appends at 1024 and retains the hole;
Unordered reuses 125 without extending the tree. Both directions, all properties,
counts, root ownership and neighbors remain exact. Width 8 appends at a property-leaf
boundary; width 33 appends within an existing property leaf, while both cross an
edge-leaf boundary.

Owner-level repeated deletion permits the existing `Some(tombstone)` result solely to
check state idempotency; it is **not** evidence of the public `None` contract. Rejection
snapshots cover descriptors, root/neighbor backing, all minted LTB bytes/allocation
metadata, edge header/counts and free spans, not every byte of all 16 memories. The
width rejection is not an allocation-failure or Graph sidecar transaction test.

### Public no-op failure

Witness v2 strengthens v1 by collecting all outcomes and checking unchanged source state
before asserting their results. It prints:

```text
TREE_DELETE_PUBLIC_NOOPS [(125, Some((4294967295, 0))), (1024, None), (4294967295, None)]
```

Already-dead 125 is reported as removed again, although backing/counts remain unchanged;
both out-of-range requests return `None`. `edge_exists_at_slot` calls the slab/run
`locate_bucket_edge_for_delete` without tree dispatch, so its probe reads LEG storage
instead of the LTB row. The tree delete owner then returns `Some(current)` for its
native tombstone. The v1 diagnostic comment claiming public wrappers filter this to
`None` was wrong; v2 corrects that comment. The failed public assertion was not weakened.

### Reachable demotion failure

`property_tree_delete_reachable_demotion_preserves_identities` uses the public forward
move-reporting API and a pre-existing `(slot → target/value)` map. After each completed
call it removes the requested key, replays only returned movements, and compares all live
canonical identities. The first 511 deletions pass. The 512th, deleting slot 511, reaches
actual `T_DEMOTE = 512` and panics:

```text
live property values must cover degree * w bytes
  left: 33792
 right: 16896
TREE_DELETE_DEMOTION slot=511 degree=513->512 extent=1024->1024 live=529 root_changed=false tail=10 free=0
```

The edge marker, degree and global count have already changed. The test catches the
panic only to record that state, then fails; it does not accept the panic as success.
The property-aware visitor correctly yields all 1024 positions, but demotion appends
all values while filtering only the edge vector to 512 live rows. No demoted descriptor
is published in this observation. Actual root bytes are unchanged; this is not evidence
of root aliasing or a measured root-overwrite failure.

Separately, source inspection shows that successful demotion packs survivors but
`remove_edge_at_slot_with_move` always returns an empty movement list for its tree arm.
The event oracle is ready to detect that, but the property panic preempts it here. No
property-filter patch, zero-width control, release-mode run or demotion redesign was
used to claim a second runtime result.

## Owner and error audit

### Caller map and duplicated addressing

- Forward/reverse public deletion, both with and without moves, share
  `edge_exists_at_slot` then the single-orientation removal dispatch. Forward/reverse
  property-update admission also uses that existence owner: **six call sites**.
- `GraphStore::commit_delete_edge_by_handle` resolves the source and counterpart before
  writing. It plans property-index admission, commits the fence when needed, removes
  inline index entries/sidecars, calls the forward removal, applies returned movements,
  then removes the undirected forward counterpart or directed reverse row. Its
  post-fence failures go through `trap_post_fence_commit`; that source structure is not
  a PocketIC rollback measurement.
- `counterpart.rs::scan_pair_ordinal{,_verified}` relies on `read_edge_state`.
  `read_edge_state_internal` groups Tree/Slab and `read_edge_state_at_slot` reads run-free
  tree positions as `edge_start + slot` from LEG. It cannot be reused as a correct tree
  existence helper without repairing its own mode contract.
- Directed/undirected deferred matching removals reach
  `remove_edge_matching_skip_leaf_cascade_with_move`. Its Tree/Slab arm scans LEG and
  calls the slab deletion owner; it does not reach the tree tombstone writer.
- These three addressing sites need one canonical tree-position/liveness contract, using
  existing root resolution rather than new caches/maps or independent caller guards.
  Only the public no-op manifestation ran here; counterpart, matching removal,
  reverse/facade and detach behavior are source-audited, not runtime validated.

### First write and reachable failures

| Owner | First destructive/published write | Subsequent failure/consistency boundary |
|---|---|---|
| `tree_mode_remove_edge_at_slot` | `ltb.write_payload_partial` writes the tombstone marker | Header-count increment and degree decrement are checked **after** the marker; global decrement is checked **after** descriptor publication. Valid live-row counts make those arithmetic failures unreachable, but malformed counters are not preflighted. Descriptor-write failure compensates marker/header; no injected late-write failure ran. |
| `tree_mode_reuse_tombstone_slot` | Writes the replacement target at a dead slot | Header decrement, property read/write, descriptor publication and global increment follow. Property/descriptor failures have compensation branches; checked degree/global increments occur after earlier writes. Width mismatch is rejected before writes and is covered through dispatch; exhaustion/rollback is not established. |
| `tree_mode_insert_edge` / tail append | May write an unpublished edge tail, mint LTB blocks, deepen or reserve/copy roots before the final descriptor | Exact append/reuse success is covered, not mint/deepen/tail-room allocation failure. Existing compensation/publication paths remain unchanged; passing the policy matrix is not blanket writer atomicity. |
| `tree_mode_demote_to_slab` | Reserves a new edge span and writes packed live targets, after deletion has committed | Property visitation/length/byte-span preparation occur afterward. The demonstrated length assertion panics; a property-read `?` can also bypass edge-span cleanup. Descriptor publication precedes fallible slab-count adjustment and old-resource collection/release. |

LTB partial I/O returns `NotMinted`/`OutOfBounds` before its write and does not grow;
normal deletion already read the same four-byte position. A canonical backed descriptor
write ordinarily needs no growth. Those facts bound the normal path, but do not prove
corrupt-state or allocation-failure atomicity. Native memory faults/panics are not typed
rejections. No properties are read or erased by the tree tombstone writer itself.

Demotion also computes its avoidance range from **edge-root width only**, rather than
the combined edge/property-root width; its property retirement iterates root entries as
leaves rather than traversing property interiors. These are source-audit obligations for
the prerequisite, not exercised overlap/depth failures in this depth-1 witness. Repair
must cover collection, combined-root ownership, publication, retirement and external
ordinal identities together. Merely filtering values would leave the empty movement
contract and later failure ordering unresolved.

### Obsolete combined maintenance tag

A repository search finds `CompactVertexEdgeAndValueSpanV1` only in its enum/tag, wire
encode/decode/key/priority matches, handler and handler requeues; no initial enqueue
producer was found. The handler dispatches Required compaction for either orientation
and discards both `EdgeMoved(_)` and `OverflowRewrite(_)`. It is **not observer-safe**.
GAP-2026-09-22-005 records this as an open pre-production removal/unification task; no
compatibility decoder, handler fix or queue behavior change was added here.

## Preservation and validation limits

- Assertion-only candidate and registration removed; tracked `crates/` and `Cargo.lock`
  match the base. No failing/ignored live test or temporary module remains.
- All accepted compaction sources/fixtures, benchmark YAML/config/source, foreign brief/CSV,
  main and historical evidence retained. The as-used lock is preserved separately with
  a raw reconstruction patch; the unchanged existing
  [`adr0097-crossover-as-used-lock.patch`](sources/adr0097-crossover-as-used-lock.patch)
  independently reconstructs the same as-used bytes and is reused rather than duplicated.
  Workspace lock restored.
- No final Rust/clippy rerun after restoring identical source, no full suite, canbench,
  native performance probe, PocketIC, release/Wasm, broad writer fault injection or merge.
  Lizard implementation comparison is inapplicable: no implementation functions changed.
- The bounded investigation/stop decision is complete, not the requested deletion repair.
  Plan `0336-property-tree-deletion-prerequisites.md` records that distinction.

## Artifacts and reproduction

Raw evidence: `$TMPDIR/spill-tree-delete-7ca-nrvy2ffs/`. Durable files use prefix
[`sources/adr0097-tree-delete-`](sources/adr0097-tree-delete-run.json).
The run metadata maps hashes, source revisions, commands, transforms and preserved files.
Raw logs are untouched; durable log copies only strip trailing line whitespace and terminal
blank lines. Witness v1/v2 are byte-identical copies; published source patches use
zero context to avoid patch-context whitespace, with the original diffs also kept raw.
Their reconstructed source hashes match the executed candidate.

To reconstruct **text only**, use the base commit, apply `registration.patch` with
`git apply --unidiff-zero`, and place
the selected witness at `crates/ic-stable-lara/src/labeled/bidirectional/tree_delete_test.rs`.
The first runtime used v1 with unchanged production. The second additionally applied
`assertion-only.patch`; the third kept that production patch and used v2. Neither patch
is retained production code. The demotion body is identical in v1/v2; v2 changes only
public no-op discrimination and the erroneous wrapper comment. Re-execution requires
its own authorization/budget, not a blind application to the current checkout.

Ledger totals and the final structural plan result are recorded in
[`adr0097-tree-delete-ledger.json`](sources/adr0097-tree-delete-ledger.json) and
[`adr0097-tree-delete-plan-final.txt`](sources/adr0097-tree-delete-plan-final.txt).
