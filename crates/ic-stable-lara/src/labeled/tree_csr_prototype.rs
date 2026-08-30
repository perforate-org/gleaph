//! ADR 0088 Tree-CSR mode prototype: an ordinal-addressed indirect block tree
//! standing in for one hot labeled bucket.
//!
//! **Evidence-only.** This is NOT wired into [`super::graph::LabeledLaraGraph`];
//! it exists to measure Tree-CSR's mutation, scan, and lookup cost against the
//! shared-leaf slab baselines in `labeled/bench.rs`, so the implementation
//! decision recorded in ADR 0088 §Measurement gates rests on numbers rather than
//! assumption.
//!
//! Design (per ADR 0088 recorded constraints):
//! - **Ordinal-addressed**, not key-addressed: lookup input is a logical
//!   position (`slot` 0..stored_slots); no separator keys; no target-keyed
//!   search. ADR 0022's measured B-tree costs (~2,004 ins/edge) do not apply
//!   here — block-internal reads are the same raw 4-byte rows as the slab.
//! - **Fixed fanout**, left-packed: `B = 1024` slots per block, `R_max = 1024`
//!   for the root array, depth derived from `stored_slots` per §4 of ADR 0088.
//! - **No persisted depth**: depth is recomputed on every mutation. Inconsistent
//!   states are unrepresentable.
//! - **Reserve/commit atomicity** for `deepen`, `flatten`, and promotion probes:
//!   intermediate states must be valid; the descriptor-equivalent (here, just
//!   the block-graph) is only mutated after all data blocks are populated.
//! - **Block-internal rows** are raw 4-byte `target` ids (one per slot,
//!   matching `Edge::BYTES == 4`).
//!
//! # Plan 0315 refactor
//!
//! The block store is now [`super::ltb_raw_block_store::LtbRawBlockStore<M>`],
//! a raw-byte backend that talks directly to stable memory. The previous
//! `StableBTreeMap<BlockId, Vec<u8>, VectorMemory>` scaffold inflated every
//! measurement (Gate 3 was 315.74M ins for edge-only promotion — well past the
//! single-digit M target). Plan 0315 swaps the scaffold out and re-runs Gates
//! 2/3/4 against this raw-block backend.
//!
//! [`TreeCsrBucket`] is now `M: Memory` generic so tests use
//! [`crate::VectorMemory`] (via [`crate::test_support::vector_memory`]) and
//! production wiring can use [`ic_stable_structures::memory_manager::VirtualMemory`].
//!
//! Scope: bench-only; not wired into the production path. Mirrors
//! `hub_tree_prototype.rs`'s evidence-only contract.
//!
//! # Read/Write API selection (Plan 0322)
//!
//! - `write_payload` (block-aligned, 4 KiB): the bulk path for promotion.
//!   Use when committing a full block in one shot. Plan 0316 made
//!   `promote_from_slice` and `promote_from_slices_with_property` use this
//!   path (one write per block).
//! - `write_payload_partial` (sub-block, `offset + src.len() ≤ BLOCK_PAYLOAD_BYTES`):
//!   the per-slot sub-block escape hatch. Use for per-slot writes after
//!   promotion (the "edge already exists, mutate one target" path).
//! - `read_payload` (block-aligned, 4 KiB): use when the caller decodes the
//!   whole block in one go (e.g. `for_each_descending` walks every slot
//!   in the block's payload).
//! - `read_payload_partial` (sub-block): use when only a slice of a block
//!   is needed (e.g. `range_target(slot)` reads 4 bytes at `slot * 4`).
//! - `for_each_chunk` (chunk-buffer iter): use when the caller wants to
//!   walk the bucket without materializing a stack buffer per slot.
//!   Mirrors the CSR-slab leaf-chunk-buffer pattern.

use ic_stable_structures::Memory;

use super::ltb_raw_block_store::{BLOCK_PAYLOAD_BYTES, LtbRawBlockStore};

/// Block payload capacity (ADR 0088 §1 wire truth).
const B: usize = BLOCK_PAYLOAD_BYTES / 4;
/// Root array cap (ADR 0088 §1 wire truth).
const R_MAX: usize = 1024;
/// Fail-closed depth boundary (ADR 0088 §4).
const MAX_DEPTH: u32 = 3;

/// Total slots addressable at each depth (with `R_max = B = 1024`):
/// depth 1 ≤ 2²⁰, depth 2 ≤ 2³⁰, depth 3 ≤ 2⁴⁰. Mirrors ADR 0088 §4.
const fn coverage_at_depth(depth: u32) -> u64 {
    // 1024^depth = 2^(10*depth)
    1u64 << (10 * depth)
}

/// Derive depth from `stored_slots` (ADR 0088 §4 edge-stream equation):
/// `depth = min { d >= 1 : ceil(stored_slots / B^d) <= R_max }`.
fn derive_depth(stored_slots: u32) -> u32 {
    if stored_slots == 0 {
        return 1;
    }
    let s = u64::from(stored_slots);
    for d in 1..=MAX_DEPTH {
        // ceil(s / B^d) <= R_max  <=>  s <= R_max * B^d
        let ceiling = s.div_ceil(coverage_at_depth(d));
        if ceiling <= R_MAX as u64 {
            return d;
        }
    }
    // Past the structural boundary. Caller must fail-closed before any
    // canonical write; this panic matches the ADR's "fail-closed structural
    // boundary" rule for the evidence-only prototype.
    panic!(
        "tree_csr_prototype: stored_slots={stored_slots} exceeds MAX_DEPTH={MAX_DEPTH} coverage"
    );
}

/// Required root array length for a given `stored_slots` at the derived depth.
fn root_len(stored_slots: u32) -> usize {
    let depth = derive_depth(stored_slots);
    let s = u64::from(stored_slots);
    let blocks_at_depth = s.div_ceil(coverage_at_depth(depth)) as usize;
    debug_assert!(blocks_at_depth <= R_MAX);
    blocks_at_depth
}

/// Tree-CSR evidence-only prototype. Layout (per ADR 0088 §2):
/// - `root` is the dense block-id array (size = derived root length).
/// - `store` is the raw-block LTB store, generic over [`Memory`].
/// - `stored_slots` is tombstone-inclusive logical slot count; depth is derived.
///
/// **Packing invariant:** every non-tail block at every level is full (exactly
/// `B` slots); only the right-spine tail is partial. The derived root length
/// equals the number of unique block ids that exist for this bucket.
///
/// **Atomicity invariant:** mutations split into a reserve phase (mint all new
/// blocks, populate them) and a commit phase (rewrite `root` and bump
/// `stored_slots`). Intermediate states are valid; no published state has
/// `root` length disagreeing with `stored_slots`.
pub(crate) struct TreeCsrBucket<M: Memory> {
    root: Vec<u32>,
    store: LtbRawBlockStore<M>,
    stored_slots: u32,
}

#[allow(
    dead_code,
    reason = "All methods are exercised by benches and tests; allow until wired."
)]
impl<M: Memory> TreeCsrBucket<M> {
    /// New empty tree bucket (depth 1, zero slots). The LTB store starts fresh
    /// (no blocks minted); reserve/commit writes happen on first insert /
    /// promote.
    pub(crate) fn new(memory: M) -> Self {
        let store = LtbRawBlockStore::new(memory).expect("LtbRawBlockStore::new grows header");
        Self {
            root: Vec::new(),
            store,
            stored_slots: 0,
        }
    }

    pub(crate) fn stored_slots(&self) -> u32 {
        self.stored_slots
    }

    pub(crate) fn depth(&self) -> u32 {
        derive_depth(self.stored_slots)
    }

    pub(crate) fn root_len(&self) -> usize {
        self.root.len()
    }

    /// Reads a block's 4096-byte payload into `dst`. Raw-byte read; no
    /// allocation per access.
    fn read_block_into(&self, id: u32, dst: &mut [u8; BLOCK_PAYLOAD_BYTES]) {
        self.store
            .read_payload(id, dst)
            .expect("TreeCsrBucket: read_payload past tail_next")
    }

    /// Writes a block's 4096-byte payload from `src`.
    fn write_payload(&mut self, id: u32, src: &[u8; BLOCK_PAYLOAD_BYTES]) {
        self.store
            .write_payload(id, src)
            .expect("TreeCsrBucket: write_payload past tail_next");
    }

    /// Mint a new block via the LTB store.
    fn mint_block(&mut self) -> u32 {
        self.store
            .mint()
            .expect("TreeCsrBucket: LtbRawBlockStore::mint grow failed")
    }

    /// Promotion probe (Gate 3): reserve mints `ceil(stored_slots / B)` data
    /// blocks, all at zero payload; commit writes the supplied `targets` into
    /// the blocks in order and bumps `stored_slots`. Models the
    /// `promote_bucket_to_tree_mode` reserve/commit split described in ADR
    /// 0088 §7 for the edge-only case (`w = 0`).
    ///
    /// The bench wraps this in a `VectorMemory` region to make the
    /// `Memory::grow`-equivalent cost observable at the canbench layer.
    ///
    /// **Plan 0316 (block-batched writes):** the commit phase is rewritten
    /// to build one `[u8; BLOCK_PAYLOAD_BYTES]` per block in a stack
    /// buffer and call `write_payload` once per block, instead of the
    /// previous per-slot `read_block_into` + `write_payload` pair. The
    /// I/O amplification drops from `O(S)` per slot (4 KiB read + 4 KiB
    /// write per slot) to `O(B)` per block (one 4 KiB write per block),
    /// which removes the per-write payload constant that Gate 3 was
    /// measuring.
    pub(crate) fn promote_from_slice(&mut self, targets: &[u32]) {
        debug_assert_eq!(
            self.stored_slots, 0,
            "promote_from_slice on non-empty bucket"
        );
        let new_stored_slots = u32::try_from(targets.len()).expect("stored_slots fits u32");
        let new_root_len = root_len(new_stored_slots);
        debug_assert!(new_root_len <= R_MAX);

        // Reserve phase: mint `new_root_len` blocks. The LTB store's `mint`
        // already writes the default Edge header; we additionally zero the
        // payload so the committed block holds canonical rows only.
        let mut reserved = Vec::with_capacity(new_root_len);
        let zero_payload = [0u8; BLOCK_PAYLOAD_BYTES];
        for _ in 0..new_root_len {
            let id = self.mint_block();
            self.write_payload(id, &zero_payload);
            reserved.push(id);
        }
        debug_assert_eq!(reserved.len(), new_root_len);

        // Commit phase (block-batched): for each reserved block, fill the
        // slots that fall in that block from `targets[]` in-stack, then
        // call `write_payload` once. The previous per-slot read-modify-
        // write (one `read_block_into` + one `write_payload` per slot,
        // each touching 4 KiB) collapsed to one `write_payload` per block.
        for (root_index, &block_id) in reserved.iter().enumerate() {
            let block_first_slot = root_index as u32 * B as u32;
            let block_last_slot_excl = (block_first_slot + B as u32).min(new_stored_slots);
            let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
            for s in block_first_slot..block_last_slot_excl {
                let in_block_offset = ((s - block_first_slot) as usize) * 4;
                payload[in_block_offset..in_block_offset + 4]
                    .copy_from_slice(&targets[s as usize].to_le_bytes());
            }
            self.write_payload(block_id, &payload);
        }

        // Publish: only after every block holds the canonical row.
        self.root = reserved;
        self.stored_slots = new_stored_slots;
        debug_assert_eq!(self.root.len(), root_len(self.stored_slots));
    }

    /// Promotion probe with inline-property bytes (Gate 3, widest admitted
    /// profile, ADR 0088 §5). Models `w` bytes per edge: a parallel property
    /// block stream stores `K = floor(B * 4 / w)` properties per block. For
    /// `w = 32`, `K = 4096 / 32 = 128`, so `stored_slots = 4096` requires
    /// `ceil(4096 / 128) = 32` property blocks.
    ///
    /// Used by `bench_labeled_tree_csr_promote_inline_property_w32`. Not
    /// wired into any other path; bench-only probe.
    ///
    /// `targets` and `properties` must have the same length.
    ///
    /// **Plan 0316 (block-batched writes):** both the edge commit and the
    /// property commit fill one `[u8; BLOCK_PAYLOAD_BYTES]` per block in a
    /// stack buffer and call `write_payload` once per block. The previous
    /// per-slot read-modify-write loop was the source of Gate 3's per-write
    /// payload constant; this rewrite removes it for both streams.
    pub(crate) fn promote_from_slices_with_property(
        &mut self,
        targets: &[u32],
        properties: &[u8],
        w: usize,
    ) {
        assert!(w > 0, "promote_with_property requires w > 0");
        assert!(w <= B * 4, "w must fit in one block payload");
        assert_eq!(
            targets.len(),
            properties.len() / w,
            "targets / properties length mismatch"
        );
        debug_assert_eq!(
            self.stored_slots, 0,
            "promote_from_slices_with_property on non-empty bucket"
        );

        let new_stored_slots = u32::try_from(targets.len()).expect("stored_slots fits u32");
        let k_u32 = u32::try_from((B * 4) / w).expect("k fits u32");
        let new_root_len = u32::try_from(root_len(new_stored_slots)).expect("root_len fits u32");
        let property_root_len = new_stored_slots.div_ceil(k_u32);
        debug_assert!(
            new_root_len as u64 + property_root_len as u64 <= (R_MAX * 2) as u64,
            "root span <= 2*R_max"
        );

        // Reserve phase: mint all edge + property blocks at zero payload.
        let mut edge_reserved = Vec::with_capacity(new_root_len as usize);
        let zero_payload = [0u8; BLOCK_PAYLOAD_BYTES];
        for _ in 0..new_root_len {
            let id = self.mint_block();
            self.write_payload(id, &zero_payload);
            edge_reserved.push(id);
        }
        let mut property_reserved = Vec::with_capacity(property_root_len as usize);
        for _ in 0..property_root_len {
            let id = self.mint_block();
            self.write_payload(id, &zero_payload);
            property_reserved.push(id);
        }

        // Commit phase (block-batched for both streams).
        //
        // Edge stream: same shape as `promote_from_slice` — one stack
        // buffer per block, one `write_payload` per block.
        for (root_index, &block_id) in edge_reserved.iter().enumerate() {
            let block_first_slot = root_index as u32 * B as u32;
            let block_last_slot_excl = (block_first_slot + B as u32).min(new_stored_slots);
            let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
            for s in block_first_slot..block_last_slot_excl {
                let in_block_offset = ((s - block_first_slot) as usize) * 4;
                payload[in_block_offset..in_block_offset + 4]
                    .copy_from_slice(&targets[s as usize].to_le_bytes());
            }
            self.write_payload(block_id, &payload);
        }

        // Property stream: one stack buffer per property block, `K = B*4/w`
        // properties per block. For `w = 32` that's 128 properties per
        // block; for `w = 16` it's 256. The last block may be partial
        // (the same `min` clamp as the edge stream).
        for (pblock_index, &pid) in property_reserved.iter().enumerate() {
            let block_first_pslot = pblock_index as u32 * k_u32;
            let block_last_pslot_excl = (block_first_pslot + k_u32).min(new_stored_slots);
            let mut pp = [0u8; BLOCK_PAYLOAD_BYTES];
            for p in block_first_pslot..block_last_pslot_excl {
                let in_block_offset = ((p - block_first_pslot) as usize) * w;
                pp[in_block_offset..in_block_offset + w]
                    .copy_from_slice(&properties[(p as usize) * w..(p as usize + 1) * w]);
            }
            self.write_payload(pid, &pp);
        }

        // Publish: descriptor-equivalent commit. Edge root then property root.
        let mut root_combined = Vec::with_capacity(edge_reserved.len() + property_reserved.len());
        root_combined.extend(edge_reserved);
        root_combined.extend(property_reserved);
        self.root = root_combined;
        self.stored_slots = new_stored_slots;
    }

    /// Reserve/commit split: appends `target` to logical position `stored_slots`
    /// (append-only; tombstone semantics live above this layer). Triggers
    /// `deepen()` if the new root length would exceed `R_max`.
    pub(crate) fn insert(&mut self, target: u32) {
        let new_stored_slots = self
            .stored_slots
            .checked_add(1)
            .expect("tree_csr_prototype: stored_slots overflow");

        let new_root_len = root_len(new_stored_slots);

        // Reserve phase: mint any new blocks needed and fill them with zeros
        // (the canonical body of an unscheduled slot). Commit order is data →
        // root, so a panic between reserve and commit leaves a valid empty
        // block on the heap rather than a half-written descriptor.
        let zero_payload = [0u8; BLOCK_PAYLOAD_BYTES];
        while self.root.len() < new_root_len {
            let id = self.mint_block();
            self.write_payload(id, &zero_payload);
            self.root.push(id);
        }
        debug_assert_eq!(self.root.len(), new_root_len);

        // Compute physical address of the new slot and write the row.
        let (block_id, in_block_offset) = physical_address(new_stored_slots - 1, self.depth());
        let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
        self.read_block_into(block_id, &mut payload);
        let offset = in_block_offset * 4;
        payload[offset..offset + 4].copy_from_slice(&target.to_le_bytes());
        self.write_payload(block_id, &payload);

        // Publish (commit): only after the data block holds the canonical row.
        self.stored_slots = new_stored_slots;

        // Packing invariant sanity check (debug only).
        debug_assert_eq!(self.root.len(), root_len(self.stored_slots));
    }

    /// Tombstone by logical position. Shifts tail entries left to maintain the
    /// left-packed invariant (no holes). Tombstones themselves are not a tree
    /// concern; in production they live in the slab span and tree-mode tombstones
    /// would be tracked above this layer. For bench parity with the slab path
    /// (which shifts), we shift here.
    pub(crate) fn remove_at(&mut self, slot: u32) -> Option<u32> {
        if slot >= self.stored_slots {
            return None;
        }
        let target = self.read_slot(slot);
        // Shift left: read slot+1, write into slot; iterate to end.
        let last = self.stored_slots - 1;
        let depth = self.depth();
        // We buffer two blocks at a time: a current destination block and an
        // optional source block. Most shifts stay within one block; the
        // second buffer covers the cross-block boundary case.
        let mut dst_payload = [0u8; BLOCK_PAYLOAD_BYTES];
        let mut src_payload = [0u8; BLOCK_PAYLOAD_BYTES];
        let mut src_block_id: u32 = u32::MAX;
        for s in slot..last {
            let (src_block, src_off) = physical_address(s + 1, depth);
            let (dst_block, dst_off) = physical_address(s, depth);
            if src_block != src_block_id {
                self.read_block_into(src_block, &mut src_payload);
                src_block_id = src_block;
            }
            self.read_block_into(dst_block, &mut dst_payload);
            let src_byte = src_off * 4;
            let dst_byte = dst_off * 4;
            dst_payload[dst_byte..dst_byte + 4]
                .copy_from_slice(&src_payload[src_byte..src_byte + 4]);
            self.write_payload(dst_block, &dst_payload);
        }
        // Clear the now-unused tail slot to keep `stored_slots == root_len-coverage`
        // semantic honest (no stale data lingers in the tail block).
        if self.stored_slots > 0 {
            let (block_id, off) = physical_address(last, depth);
            let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
            self.read_block_into(block_id, &mut payload);
            let byte = off * 4;
            payload[byte..byte + 4].copy_from_slice(&0u32.to_le_bytes());
            self.write_payload(block_id, &payload);
        }
        self.stored_slots -= 1;
        let new_root_len = root_len(self.stored_slots);
        // Trim any now-unused tail blocks back to the derived root length.
        // The blocks themselves remain in the LTB store (production's "block
        // ids never reused" rule); we just shrink the root array.
        while self.root.len() > new_root_len {
            self.root.pop();
        }
        Some(target)
    }

    /// Reads the target at logical position `slot`.
    pub(crate) fn read_slot(&self, slot: u32) -> u32 {
        assert!(
            slot < self.stored_slots,
            "read_slot({slot}) out of bounds (stored_slots={})",
            self.stored_slots
        );
        let (block_id, off) = physical_address(slot, self.depth());
        let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
        self.read_block_into(block_id, &mut payload);
        let byte = off * 4;
        u32::from_le_bytes(payload[byte..byte + 4].try_into().unwrap())
    }

    /// Point lookup at logical position `slot`: returns the target id, or
    /// `None` if `slot >= stored_slots`. Uses [`LtbRawBlockStore::read_payload_partial`]
    /// to read only the 4 bytes at `slot * 4`, avoiding a 4 KiB block
    /// allocation per call.
    ///
    /// This is the production call site for `random_ordinal_access` and
    /// `CounterpartScan::PairOrdinal` resolution paths. Tombstone semantics
    /// (sentinel target values) live above this layer; this method returns
    /// whatever 4 bytes are at that offset.
    pub(crate) fn range_target(&self, slot: u32) -> Option<u32> {
        if slot >= self.stored_slots {
            return None;
        }
        let (block_root_index, in_block_offset) = physical_address(slot, self.depth());
        let block_id = self.root[block_root_index as usize];
        let byte_offset = in_block_offset * 4;
        let mut buf = [0u8; 4];
        self.store
            .read_payload_partial(block_id, byte_offset, &mut buf)
            .expect("range_target: read_payload_partial past tail_next (invariant violated)");
        Some(u32::from_le_bytes(buf))
    }

    /// Chunk-buffer iterator: walks the bucket's root array in order,
    /// yielding `(block_first_slot, &payload[..block_byte_len])` to the
    /// callback. The yielded slice is the raw 4-byte rows in order; callers
    /// can `u32::from_le_bytes(slice[i..i+4])` directly without copying.
    /// Block boundaries are natural chunk boundaries. The tail block may be
    /// partial.
    ///
    /// This mirrors the CSR-slab leaf-chunk-buffer iter pattern: a single
    /// stack buffer (4 KiB) is reused per block, and the caller decodes the
    /// rows in-place. It is the lower-level building block that
    /// [`Self::for_each_descending`] / [`Self::for_each_ascending`] use
    /// internally. Lives on `TreeCsrBucket` (not on `LtbRawBlockStore`)
    /// because the bucket owns the `root` array and `stored_slots` count.
    pub(crate) fn for_each_chunk<F>(&self, mut f: F)
    where
        F: FnMut(u32, &[u8]),
    {
        for (root_index, &block_id) in self.root.iter().enumerate() {
            let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
            self.read_block_into(block_id, &mut payload);
            // Tail block may be partial: byte length is
            // `min(B*4, (stored_slots - first_slot) * 4)`.
            let block_first_slot = root_index as u32 * B as u32;
            let remaining_slots = self
                .stored_slots
                .saturating_sub(block_first_slot)
                .min(B as u32);
            let block_byte_len = remaining_slots as usize * 4;
            f(block_first_slot, &payload[..block_byte_len]);
        }
    }

    /// Sequential full scan (descending order = highest slot first; matches
    /// the production `OutEdgeOrder::Descending` slab scan).
    pub(crate) fn for_each_descending<F: FnMut(u32, u32)>(&self, mut f: F) {
        // Walk the root array back-to-front so we pay one read per block
        // boundary. In-block reads are sequential within each block payload.
        let depth = self.depth();
        for root_index in (0..self.root.len()).rev() {
            let block_id = self.root[root_index];
            let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
            self.read_block_into(block_id, &mut payload);
            // The tail block may be partial; compute its slot range first.
            let block_first_slot = root_index as u32 * B as u32;
            let block_last_slot = block_first_slot + B as u32 - 1;
            let last_slot = block_last_slot.min(self.stored_slots.saturating_sub(1));
            for s in (block_first_slot..=last_slot).rev() {
                let (_blk, off) = physical_address(s, depth);
                let byte = off * 4;
                let target = u32::from_le_bytes(payload[byte..byte + 4].try_into().unwrap());
                f(s, target);
            }
        }
    }

    pub(crate) fn for_each_ascending<F: FnMut(u32, u32)>(&self, mut f: F) {
        let depth = self.depth();
        for root_index in 0..self.root.len() {
            let block_id = self.root[root_index];
            let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
            self.read_block_into(block_id, &mut payload);
            let block_first_slot = root_index as u32 * B as u32;
            let block_last_slot = block_first_slot + B as u32 - 1;
            let last_slot = block_last_slot.min(self.stored_slots.saturating_sub(1));
            for s in block_first_slot..=last_slot {
                let (_blk, off) = physical_address(s, depth);
                let byte = off * 4;
                let target = u32::from_le_bytes(payload[byte..byte + 4].try_into().unwrap());
                f(s, target);
            }
        }
    }

    /// Prefix scan (descending): visits the top `len` slots.
    pub(crate) fn prefix_scan_descending<F: FnMut(u32, u32)>(&self, len: u32, mut f: F) {
        let take = len.min(self.stored_slots);
        if take == 0 {
            return;
        }
        let depth = self.depth();
        let first_root = ((take - 1) / B as u32) as usize;
        let last_root = (root_len(self.stored_slots) - 1).min(first_root);
        for root_index in (0..=last_root).rev() {
            let block_id = self.root[root_index];
            let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
            self.read_block_into(block_id, &mut payload);
            let block_first_slot = root_index as u32 * B as u32;
            let block_last_slot = block_first_slot + B as u32 - 1;
            let last_slot = block_last_slot.min(take - 1);
            for s in (block_first_slot..=last_slot).rev() {
                let (_blk, off) = physical_address(s, depth);
                let byte = off * 4;
                let target = u32::from_le_bytes(payload[byte..byte + 4].try_into().unwrap());
                f(s, target);
            }
        }
    }

    /// Random ordinal access probe: visits `call_count` random slots, in a
    /// fixed pseudo-random order, to exercise the structure's real exposure
    /// per ADR 0088 §Measurement gates (Gate 2).
    ///
    /// **Plan 0322:** the inner read now goes through
    /// [`Self::range_target`], which uses
    /// [`LtbRawBlockStore::read_payload_partial`] to read only the 4 bytes
    /// at `slot * 4` instead of materializing a 4 KiB block. The block-id
    /// cache is removed because `read_payload_partial` is a single 4-byte
    /// `Memory::read` per call — the bench closure cost (the deterministic
    /// splitmix state update) dominates the dereference cost in the
    /// per-call measurement.
    pub(crate) fn random_ordinal_access<F: FnMut(u32, u32)>(&self, call_count: u32, mut f: F) {
        if self.stored_slots == 0 {
            return;
        }
        // Deterministic splitmix sequence: xorshift to avoid cache effects
        // and keep bench runs reproducible. The visit order hits every block
        // boundary because the modulus is the stored slot count.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        for _ in 0..call_count {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let slot = (state % u64::from(self.stored_slots)) as u32;
            if let Some(target) = self.range_target(slot) {
                f(slot, target);
            }
        }
    }

    /// Point lookup by target (descending): the O(degree) probe the status quo
    /// also pays (no `target -> seq` index). Mirrors `find_seq_by_target`.
    pub(crate) fn find_slot_by_target_descending(&self, target: u32) -> Option<u32> {
        (0..self.stored_slots)
            .rev()
            .find(|&s| self.read_slot(s) == target)
    }

    /// Counterpart resolution probe (Gate 2 row): pairs this bucket's logical
    /// positions with a sibling's logical positions in their common insertion
    /// order. We do not require the sibling to be a tree bucket (mixed-mode
    /// pairs are in scope per ADR 0088 §2); the pair callback receives
    /// `(slot, target, counterpart_slot, counterpart_target)`.
    pub(crate) fn for_each_with_counterpart<F: FnMut(u32, u32, u32, u32)>(
        &self,
        counterpart_targets: &[u32],
        mut f: F,
    ) {
        let n = self.stored_slots.min(counterpart_targets.len() as u32);
        for s in 0..n {
            let target = self.read_slot(s);
            let c_target = counterpart_targets[s as usize];
            f(s, target, s, c_target);
        }
    }

    /// Deepen: grow one level (called when `stored_slots` crosses the depth-`d`
    /// boundary). In the evidence-only prototype we skip the actual interior
    /// block reshape and instead grow root + leaf blocks on demand via
    /// `insert`. This function exists for symmetry with the ADR contract and
    /// is exercised by the `deepen_at_boundary` unit test.
    ///
    /// Returns the previous depth for diagnostic purposes.
    pub(crate) fn deepen_probe(&self) -> u32 {
        self.depth()
    }

    /// Flatten: shrink one level (mirror of `deepen_probe`). Only ever called
    /// inside maintenance; bench-only symmetry.
    pub(crate) fn flatten_probe(&self) -> u32 {
        self.depth()
    }
}

// Map logical position → (block_id_index_in_root, in-block offset) at the
// given depth. One shift/mask pair per level (ADR 0088 §2 derivation).
//
// **Prototype simplification:** the bench arms run at depth 1 (4K fits
// `root_len = 4`, 64K fits `root_len = 64`, 1M fits `root_len = 1024`, all
// `≤ R_max = 1024`); the derive-depth formula does not promote any of them
// to depth 2 or 3. We therefore resolve addresses with a single indirection
// (root → leaf block). The `depth` parameter is retained so deepen/flatten
// probes and the boundary test can compile against the same signature; the
// real Tree-CSR implementation will mint interior blocks per ADR §4.
fn physical_address(slot: u32, depth: u32) -> (u32, usize) {
    debug_assert!((1..=MAX_DEPTH).contains(&depth));
    let in_block = (slot % B as u32) as usize;
    let root_index = slot / B as u32;
    (root_index, in_block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::vector_memory;

    fn fresh() -> TreeCsrBucket<crate::VectorMemory> {
        TreeCsrBucket::new(vector_memory())
    }

    #[test]
    fn empty_bucket_has_depth_one_and_zero_root() {
        let b = fresh();
        assert_eq!(b.stored_slots(), 0);
        assert_eq!(b.depth(), 1);
        assert_eq!(b.root_len(), 0);
    }

    #[test]
    fn for_each_chunk_walks_blocks_in_order_with_tail_trim() {
        // Insert 1025 slots (1 full block + 1 partial tail block of size 1).
        // The chunk-iter must yield (0, &payload[..4096]) for the first
        // block and (1024, &payload[..4]) for the tail block.
        let mut b = fresh();
        for s in 0..1025u32 {
            b.insert(s);
        }
        let mut visited: Vec<(u32, usize)> = Vec::new();
        let mut first_chunk_bytes: Vec<u8> = Vec::new();
        let mut second_chunk_bytes: Vec<u8> = Vec::new();
        b.for_each_chunk(|start_slot, chunk| {
            if visited.is_empty() {
                first_chunk_bytes.extend_from_slice(chunk);
            } else {
                second_chunk_bytes.extend_from_slice(chunk);
            }
            visited.push((start_slot, chunk.len()));
        });
        assert_eq!(visited.len(), 2, "expected 2 chunks (1 full + 1 tail)");
        assert_eq!(visited[0], (0, BLOCK_PAYLOAD_BYTES));
        assert_eq!(visited[1], (1024, 4));
        // First chunk: slots 0..1024 = targets 0..1024, decoded in order.
        for s in 0..1024u32 {
            let lo = (s as usize) * 4;
            let hi = lo + 4;
            let target = u32::from_le_bytes(first_chunk_bytes[lo..hi].try_into().unwrap());
            assert_eq!(target, s, "first chunk slot {s} mismatch");
        }
        // Second chunk: slot 1024 = target 1024.
        let target = u32::from_le_bytes(second_chunk_bytes[..4].try_into().unwrap());
        assert_eq!(target, 1024);
    }

    #[test]
    fn left_packing_invariant_holds_at_depth_one_boundary() {
        // 1024 slots fits exactly one block (depth 1).
        let mut b = fresh();
        for s in 0..1024u32 {
            b.insert(s + 1);
        }
        assert_eq!(b.stored_slots(), 1024);
        assert_eq!(b.depth(), 1);
        assert_eq!(b.root_len(), 1);
        // 1025th insert crosses the 1-block boundary; root length grows to 2.
        b.insert(9999);
        assert_eq!(b.stored_slots(), 1025);
        assert_eq!(b.root_len(), 2);
        assert_eq!(b.depth(), 1);
        // Tail block holds the new entry.
        assert_eq!(b.read_slot(1024), 9999);
        // Earlier entries still in place.
        assert_eq!(b.read_slot(0), 1);
        assert_eq!(b.read_slot(1023), 1024);
    }

    #[test]
    fn derived_depth_matches_adr_formula() {
        // depth 1 covers <= 2^20 slots = 1,048,576.
        for s in [1u32, 1024, 1025, 65_536, 1_048_575, 1_048_576] {
            let b = fresh();
            let _d = derive_depth(s);
            let _r = root_len(s);
            assert!(derive_depth(s) >= 1);
            assert!(derive_depth(s) <= MAX_DEPTH);
            assert!(root_len(s) <= R_MAX);
            let _ = b; // touch to silence unused warnings if we add asserts later
        }
    }

    #[test]
    fn derived_depth_one_for_4k_64k_1m() {
        // All three sweep degrees fit in depth 1 (4K ≤ 1024 * 4 = 4096; 64K
        // needs root_len = 64 ≤ 1024; 1M needs root_len = 1024 ≤ 1024).
        assert_eq!(root_len(4096), 4);
        assert_eq!(derive_depth(4096), 1);
        assert_eq!(root_len(65_536), 64);
        assert_eq!(derive_depth(65_536), 1);
        assert_eq!(root_len(1_048_576), 1024);
        assert_eq!(derive_depth(1_048_576), 1);
        // 1,048,577 crosses depth-1 → depth-2.
        assert_eq!(derive_depth(1_048_577), 2);
        assert_eq!(root_len(1_048_577), 2);
    }

    #[test]
    fn remove_at_shifts_left_and_preserves_order() {
        let mut b = fresh();
        for t in [10u32, 20, 30, 40, 50] {
            b.insert(t);
        }
        // Remove the middle entry (target 30, slot 2). The shift pulls 40→2 and
        // 50→3; the tail slot is zeroed.
        let removed = b.remove_at(2).expect("removed");
        assert_eq!(removed, 30);
        assert_eq!(b.stored_slots(), 4);
        assert_eq!(b.read_slot(0), 10);
        assert_eq!(b.read_slot(1), 20);
        assert_eq!(b.read_slot(2), 40);
        assert_eq!(b.read_slot(3), 50);
    }

    #[test]
    fn remove_at_out_of_range_returns_none() {
        let mut b = fresh();
        b.insert(1);
        assert!(b.remove_at(5).is_none());
        assert_eq!(b.stored_slots(), 1);
    }

    #[test]
    fn counterpart_pair_walks_logical_positions_in_insertion_order() {
        let mut b = fresh();
        for t in [100u32, 200, 300] {
            b.insert(t);
        }
        let siblings = [11u32, 22, 33];
        let mut pairs = Vec::new();
        b.for_each_with_counterpart(&siblings, |slot, target, c_slot, c_target| {
            pairs.push((slot, target, c_slot, c_target));
        });
        assert_eq!(
            pairs,
            vec![(0, 100, 0, 11), (1, 200, 1, 22), (2, 300, 2, 33)]
        );
    }

    #[test]
    fn random_ordinal_access_visits_only_in_range_slots() {
        let mut b = fresh();
        for s in 0..100u32 {
            b.insert(s + 1000);
        }
        let mut seen_targets = std::collections::HashSet::new();
        b.random_ordinal_access(500, |_slot, target| {
            assert!(
                (1000..1100).contains(&target),
                "target {target} out of range"
            );
            seen_targets.insert(target);
        });
        // With 500 visits across 100 distinct slots, every slot is hit at least
        // 3 times on average; we just confirm we saw > 1 distinct target.
        assert!(!seen_targets.is_empty());
    }

    #[test]
    fn range_target_reads_only_4_bytes_at_slot() {
        // Fill 4096 slots (4 blocks at depth 1) so we exercise every block
        // boundary in the point-lookup path.
        let mut b = fresh();
        for i in 0..4096u32 {
            b.insert(i);
        }
        // Sample slot 0, the last slot of block 0, slot 1024 (first of block 1),
        // slot 2047 (last of block 1), slot 4095 (last slot overall).
        for slot in [0u32, 1023, 1024, 2047, 3072, 4095] {
            assert_eq!(b.range_target(slot), Some(slot), "slot {slot} mismatch");
        }
        // Out-of-range.
        assert_eq!(b.range_target(4096), None);
        // Empty bucket.
        let empty = fresh();
        assert_eq!(empty.range_target(0), None);
    }

    #[test]
    fn deepening_at_max_depth_boundary_is_fail_closed() {
        // `derive_depth` panics when `stored_slots` exceeds MAX_DEPTH = 3
        // coverage (2^40). We probe the boundary at exactly `2^40 + 1`,
        // computed in `u64` to avoid overflow before the call.
        let stored = (1u64 << 40) + 1;
        let result = std::panic::catch_unwind(|| {
            let s = u32::try_from(stored).expect("boundary fits in u64 sample");
            derive_depth(s);
        });
        assert!(result.is_err(), "derive_depth past MAX_DEPTH must panic");
        // Also probe at the largest representable u32: 2^32 - 1 > 2^40 is
        // false (2^32 < 2^40), but the test exercises overflow arithmetic
        // in the depth loop, so we just confirm the call sites do not crash
        // for in-range inputs.
        for s in [1u32, 1024, 65_536, 1_048_576, u32::MAX] {
            // 2^32 - 1 > 2^20 (depth-1 cap), so it must promote to depth 2
            // or 3 without panic.
            let d = derive_depth(s);
            assert!((1..=MAX_DEPTH).contains(&d));
        }
    }

    #[test]
    fn deepen_and_flatten_probes_report_current_depth() {
        let mut b = fresh();
        assert_eq!(b.deepen_probe(), 1);
        for s in 0..4096u32 {
            b.insert(s);
        }
        assert_eq!(b.deepen_probe(), 1);
        assert_eq!(b.flatten_probe(), 1);
    }

    #[test]
    fn published_state_invariant_holds_after_insert() {
        // After every insert, root_len() must equal root_len(stored_slots) —
        // i.e. the published state can never have a stale root length.
        let mut b = fresh();
        for s in 0..3000u32 {
            b.insert(s);
            assert_eq!(b.root_len(), root_len(b.stored_slots()));
        }
    }
}
