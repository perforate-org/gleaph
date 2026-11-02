//! EdgeStore spill API: the typed run wrappers and the run-based readers of a bucket's spill run.
//!
//! ADR 0097 §3 replaced the shared per-leaf overflow log with per-bucket spill runs, so this module
//! no longer has a head, a chain walk or any segment vocabulary: a bucket's rows beyond its slab
//! prefix are rows `0..used_len` of its own run, addressed by [`SpillRunId`].
//!
//! # Wrapper contracts
//!
//! * [`EdgeStore::spill_read_row`] rejects `index >= used_len` (fail closed): unused capacity is
//!   never exposed as if it were a row.
//! * [`EdgeStore::spill_append`] writes only within the run's capacity and returns the class-full
//!   signal; the caller grows or promotes, it never writes past capacity.
//! * [`EdgeStore::spill_grow`] reports **one** promotion outcome for both class-cap exhaustion and
//!   arena-limit exhaustion, and keeps the write/publish order allocate -> copy -> return ->
//!   publish, so a descriptor is never pointed at a freed run.
//! * [`EdgeStore::spill_release`] derives the run's class from the run's own header.

use crate::labeled::spill_run_store::{ArenaRows, MAX_ROWS, SpillRunError, SpillRunId};
use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    VertexId,
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

use super::{DeleteTarget, EdgeLayout, EdgeStore};

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    // ---------------------------------------------------------------- typed wrappers

    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn spill_allocator_inventory(
        &self,
    ) -> (u32, [u32; crate::labeled::spill_run_store::CLASS_COUNT]) {
        self.spill.allocator_inventory()
    }

    /// Used length of a run: the number of rows `0..used_len` that exist.
    #[inline]
    pub(crate) fn spill_used_len(&self, run: SpillRunId) -> u32 {
        self.spill.read_used_len(run)
    }

    /// Reads row `index` of `run` into `out`. Rejects `index >= used_len` fail-closed: unused
    /// capacity is never exposed as if it were a row.
    pub(crate) fn spill_read_row(
        &self,
        run: SpillRunId,
        index: u32,
        out: &mut [u8],
    ) -> Result<(), LaraOperationError> {
        assert_eq!(out.len(), E::BYTES, "spill row width mismatch");
        if !self.spill.read_rows(run, index, out) {
            return Err(LaraOperationError::SpillRowOutOfRange);
        }
        Ok(())
    }

    /// Writes row `index` of `run`, which must already exist (`index < used_len`): overwriting a
    /// tombstone hole or a slot the caller claimed. The used length does not change.
    pub(crate) fn spill_write_row(
        &self,
        run: SpillRunId,
        index: u32,
        row: &[u8],
    ) -> Result<(), LaraOperationError> {
        if index >= self.spill_used_len(run) {
            return Err(LaraOperationError::SpillRowOutOfRange);
        }
        self.spill
            .write_row(run, index, row)
            .map_err(LaraOperationError::from)
    }

    /// Appends `row` at the run's used end. `Ok(false)` means the run's class is full: the caller
    /// grows ([`Self::spill_grow`]) or promotes, and never writes past capacity.
    pub(crate) fn spill_append(
        &self,
        run: SpillRunId,
        row: &[u8],
    ) -> Result<bool, LaraOperationError> {
        self.spill
            .try_append_row(run, row)
            .map_err(LaraOperationError::from)
    }

    /// Grows `run` to hold `to_len` rows and returns the id the caller must publish.
    ///
    /// One outcome for both ways the arena can refuse - a class cap above `MAX_ROWS` and an arena
    /// limit - so the caller has a single promotion path; and the order is allocate -> copy ->
    /// return -> publish, so the descriptor is never pointed at a freed run.
    pub(crate) fn spill_grow(
        &self,
        run: SpillRunId,
        from_len: u32,
        to_len: u32,
    ) -> Result<SpillRunId, LaraOperationError> {
        self.spill
            .grow_to(run, from_len, to_len)
            .map_err(LaraOperationError::from)
    }

    /// Rows that may be dropped from the head of `run` before it would change class.
    pub(crate) fn spill_head_drop_capacity(&self, run: SpillRunId) -> u32 {
        self.spill.head_drop_capacity(run)
    }

    /// Drops `count` rows from the head of `run`. The caller folds them into the slab span first
    /// and publishes the new prefix length after, so a failure here leaves the rows readable.
    pub(crate) fn spill_drop_head(
        &self,
        run: SpillRunId,
        count: u32,
    ) -> Result<(), LaraOperationError> {
        self.spill
            .drop_head(run, count)
            .map_err(LaraOperationError::from)
    }

    /// Copies the rows at `keep` into a fresh run of their own class (the rebuild's store half).
    pub(crate) fn spill_repack(
        &self,
        run: SpillRunId,
        keep: &[u32],
    ) -> Result<SpillRunId, LaraOperationError> {
        self.spill
            .repack(run, keep)
            .map_err(LaraOperationError::from)
    }

    /// Releases `run` to its class's free list. The class comes from the run's own header.
    pub(crate) fn spill_release(&self, run: SpillRunId) -> Result<(), LaraOperationError> {
        self.spill.release(run).map_err(LaraOperationError::from)
    }

    /// Mints a run id for `run` against the shared arena's capacity, for callers that hold a raw
    /// offset from a descriptor field. Every other entry point takes a [`SpillRunId`].
    pub(crate) fn spill_mint(&self, run: u32) -> Result<SpillRunId, LaraOperationError> {
        SpillRunId::mint(run, ArenaRows::EDGE).map_err(LaraOperationError::from)
    }

    // ---------------------------------------------------------------- run-based readers

    /// Edge at row `index` of `run`.
    pub(crate) fn spill_edge_at(
        &self,
        run: SpillRunId,
        index: u32,
    ) -> Result<E, LaraOperationError> {
        if E::BYTES <= super::INLINE_EDGE_BYTES {
            let mut buf = [0u8; super::INLINE_EDGE_BYTES];
            self.spill_read_row(run, index, &mut buf[..E::BYTES])?;
            Ok(E::read_from(&buf[..E::BYTES]))
        } else {
            let mut buf = vec![0u8; E::BYTES];
            self.spill_read_row(run, index, &mut buf)?;
            Ok(E::read_from(&buf))
        }
    }

    /// Bounded scratch shared by streaming and full-replay readers. Range validation still
    /// belongs to the spill store on every payload read.
    fn spill_read_buffer(&self, run: SpillRunId) -> Result<(u32, Vec<u8>), LaraOperationError> {
        let used = self.spill_used_len(run);
        if used > MAX_ROWS {
            return Err(LaraOperationError::SpillRowOutOfRange);
        }
        let chunk = super::OUT_EDGE_SLAB_CHUNK_SLOTS;
        let size = (used.min(chunk) as usize)
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        bytes.resize(size, 0);
        Ok((used, bytes))
    }

    /// Lazily decodes rows in ordinal order from checked, bounded contiguous reads. Tombstones
    /// retain their positions; callers decide liveness and can stop before decoding a suffix.
    pub(crate) fn spill_edges_iter(
        &self,
        run: SpillRunId,
    ) -> Result<impl ExactSizeIterator<Item = Result<E, LaraOperationError>> + '_, LaraOperationError>
    {
        let (used, mut bytes) = self.spill_read_buffer(run)?;
        let chunk = super::OUT_EDGE_SLAB_CHUNK_SLOTS;
        let mut failed = false;
        Ok((0..used).map(move |index| {
            if failed {
                return Err(LaraOperationError::SpillRowOutOfRange);
            }
            if index % chunk == 0 {
                let count = (used - index).min(chunk) as usize;
                if !self
                    .spill
                    .read_rows(run, index, &mut bytes[..count * E::BYTES])
                {
                    failed = true;
                    return Err(LaraOperationError::SpillRowOutOfRange);
                }
            }
            let offset = (index % chunk) as usize * E::BYTES;
            Ok(E::read_from(&bytes[offset..offset + E::BYTES]))
        }))
    }

    /// Every row of `run`, in ordinal order, up to `max` rows. Bounded so a corrupt length cannot
    /// allocate unboundedly.
    pub(crate) fn spill_edges(
        &self,
        run: SpillRunId,
        max: u32,
    ) -> Result<Vec<E>, LaraOperationError> {
        let used = self.spill_used_len(run);
        if used > max {
            return Err(LaraOperationError::ReadLimitExceeded);
        }
        let mut out = Vec::new();
        out.try_reserve_exact(used as usize)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        for index in 0..used {
            out.push(self.spill_edge_at(run, index)?);
        }
        Ok(out)
    }

    /// The run's row indices in ordinal order (`0..used_len`).
    pub(crate) fn spill_asc_indices(&self, run: SpillRunId) -> Vec<u32> {
        (0..self.spill_used_len(run)).collect()
    }

    /// Writes a tombstone in place at row `index`, keeping the run's length unchanged.
    pub(crate) fn spill_tombstone_row(
        &self,
        run: SpillRunId,
        index: u32,
    ) -> Result<(), LaraOperationError>
    where
        E: crate::traits::CsrEdgeTombstone,
    {
        self.spill_write_edge_row(run, index, &E::tombstone_edge())
    }

    /// Encodes `edge` and writes it as row `index` of `run` (the row must already exist).
    fn spill_write_edge_row(
        &self,
        run: SpillRunId,
        index: u32,
        edge: &E,
    ) -> Result<(), LaraOperationError> {
        Self::with_encoded_edge_row(edge, |row| self.spill_write_row(run, index, row))
    }

    /// Encodes `edge` and appends it at the run's dense end. `Ok(false)` is the class-full signal.
    fn spill_append_edge_row(&self, run: SpillRunId, edge: &E) -> Result<bool, LaraOperationError> {
        Self::with_encoded_edge_row(edge, |row| self.spill_append(run, row))
    }

    /// Encodes `edge` and hands its row bytes to `write`. The buffer is stack-resident for inline
    /// edge widths and heap-allocated only for wider edge types, so the common width pays nothing.
    fn with_encoded_edge_row<T>(
        edge: &E,
        write: impl FnOnce(&[u8]) -> Result<T, LaraOperationError>,
    ) -> Result<T, LaraOperationError> {
        if E::BYTES <= super::INLINE_EDGE_BYTES {
            let mut payload = [0u8; super::INLINE_EDGE_BYTES];
            edge.write_to(&mut payload[..E::BYTES]);
            write(&payload[..E::BYTES])
        } else {
            let mut payload = vec![0u8; E::BYTES];
            edge.write_to(&mut payload);
            write(&payload)
        }
    }

    /// Appends `edge` to `v`'s spill run, publishes the grown row and returns the run that owns it.
    ///
    /// ADR 0097 §4/§7: the first row that does not fit the prefix allocates the smallest class for
    /// exactly one row (nothing is pre-allocated for growth); later rows append at the run's dense
    /// end. A full class grows by one row - in place at the arena tail, otherwise into the next
    /// class with a copy and a release - and the caller publishes the id `spill_grow` returns. A run
    /// already at [`MAX_ROWS`], the largest class, is the promotion outcome: a bigger bucket is a
    /// tree bucket (ADR 0097 §6), and so is an arena that cannot address the allocation. The row is
    /// written before the descriptor is published, so a published spill length is always backed
    /// (ADR 0097 §8).
    pub(super) fn insert_into_spill_with_layout<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
        v: V,
        edge: E,
    ) -> Result<SpillRunId, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let prepare = |run: SpillRunId| {
            v.try_with_spill_run(run.get())
                .and_then(CsrVertex::try_grow_packed_slab_by_one)
                .map_err(|()| LaraOperationError::RowDegreeOverflow)
        };
        let (run, prepared) = match self.spill_mint(v.spill_run()) {
            Ok(run) => {
                // Preflight before appending or growing/releasing the published run.
                let prepared = prepare(run)?;
                if self.spill_append_edge_row(run, &edge)? {
                    (run, prepared)
                } else {
                    let used = self.spill_used_len(run);
                    if used >= MAX_ROWS {
                        // The spill tops out at the largest class; beyond it the bucket promotes.
                        return Err(LaraOperationError::SpillNeedsPromotion);
                    }
                    let grown = self.spill_grow(run, used, used + 1)?;
                    self.spill_write_edge_row(grown, used, &edge)?;
                    (grown, prepared)
                }
            }
            Err(_) => {
                // Unset (`NO_SPILL_RUN`) or an id outside the arena: the row does not fit its
                // prefix, so the run is minted now, with room for exactly this row.
                let run = self.spill.allocate(1).map_err(LaraOperationError::from)?;
                let prepared = match prepare(run) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        self.spill_release(run)?;
                        return Err(error);
                    }
                };
                self.spill_write_edge_row(run, 0, &edge)?;
                (run, prepared)
            }
        };
        // Publish the preflighted width and the final run id. Arena bounds are the minimum
        // encodable bound of all referencing row types, including after run relocation.
        let published = prepared
            .try_with_spill_run(run.get())
            .map_err(|()| LaraOperationError::RowDegreeOverflow)?;
        vertices.set(vid, &published);
        Ok(run)
    }

    /// The run's rows as replay entries (`None` = a tombstoned row), in ordinal order.
    pub(crate) fn spill_replay_entries(
        &self,
        run: SpillRunId,
    ) -> Result<Vec<Option<E>>, LaraOperationError> {
        let (used, mut bytes) = self.spill_read_buffer(run)?;
        let chunk = super::OUT_EDGE_SLAB_CHUNK_SLOTS;
        let mut out = Vec::new();
        out.try_reserve_exact(used as usize)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        for start in (0..used).step_by(chunk as usize) {
            let count = (used - start).min(chunk) as usize;
            if !self
                .spill
                .read_rows(run, start, &mut bytes[..count * E::BYTES])
            {
                return Err(LaraOperationError::SpillRowOutOfRange);
            }
            for row in bytes[..count * E::BYTES].chunks_exact(E::BYTES) {
                let edge = E::read_from(row);
                if edge.is_deleted_slot() {
                    out.push(None);
                } else {
                    out.push(Some(edge));
                }
            }
        }
        Ok(out)
    }

    /// Deletes the spill row a delete target names, or the slab slot it names.
    pub(super) fn tombstone_spill_delete_target<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        vid: VertexId,
        v: V,
        target: DeleteTarget,
    ) -> Result<(), LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
        E: crate::traits::CsrEdgeTombstone,
    {
        let _ = edge_layout;
        match target {
            DeleteTarget::Slab(offset) => {
                let rm_slot = v
                    .base_slot_start()
                    .checked_add(u64::from(offset))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                self.write_slot(rm_slot, E::tombstone_edge())
                    .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
            }
            DeleteTarget::Spill { run, index } => {
                self.spill_tombstone_row(run, index)?;
            }
        }
        vertices.set(vid, &v.after_slab_tombstone_delete());
        Ok(())
    }
}

impl From<SpillRunError> for LaraOperationError {
    fn from(err: SpillRunError) -> Self {
        match err {
            // The fold matches on this error itself and rebuilds the run; the conversion is only
            // reached by the append/grow paths, which never drop rows from the head.
            SpillRunError::ArenaExhausted { .. }
            | SpillRunError::RunIdOutOfRange { .. }
            | SpillRunError::HeadDropWouldReclass { .. } => {
                // One outcome for every way the arena refuses: the caller promotes the bucket.
                LaraOperationError::SpillNeedsPromotion
            }
            SpillRunError::Grow(err) => LaraOperationError::WriteLogFailed(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use crate::test_support::{TestEdge, vector_memory};

    type TestStore = EdgeStore<TestEdge, VectorMemory>;

    fn store() -> TestStore {
        TestStore::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            8,
            1,
            0,
        )
        .expect("edge store")
    }

    /// One class-8 run holding one used row, minted through the store the wrappers wrap.
    fn store_with_run() -> (TestStore, SpillRunId) {
        let store = store();
        let run = store.spill.allocate(1).expect("spill run");
        (store, run)
    }

    #[test]
    fn spill_read_row_rejects_an_index_at_the_used_length() {
        let (store, run) = store_with_run();
        let mut row = [0u8; 4];
        assert!(store.spill_read_row(run, 0, &mut row).is_ok());
        // The class has room, but only `used_len` rows exist: unused capacity is never exposed as
        // a row (fail closed), so a reader cannot address the next run's header or a stale row.
        row.fill(99);
        assert!(matches!(
            store.spill_read_row(run, 1, &mut row),
            Err(LaraOperationError::SpillRowOutOfRange)
        ));
        assert_eq!(row, [99; 4]);
        assert_eq!(store.spill_used_len(run), 1);
    }

    #[test]
    fn spill_append_signals_a_full_class_without_writing() {
        let (store, run) = store_with_run();
        for index in 1..8u32 {
            assert!(
                store
                    .spill_append(run, &index.to_le_bytes())
                    .expect("append"),
                "row {index} fits class 8"
            );
        }
        assert_eq!(store.spill_used_len(run), 8);
        // Class full: the caller's signal to grow or promote, and the length is unchanged.
        assert!(!store.spill_append(run, &99u32.to_le_bytes()).expect("full"));
        assert_eq!(store.spill_used_len(run), 8);
    }

    #[test]
    fn every_arena_refusal_is_the_single_promotion_outcome() {
        // `spill_grow` is one `.map_err(Into::into)` over the store's `grow_to`, so both ways the
        // arena refuses reach the caller as the same outcome: promote the bucket to tree mode
        // (ADR 0097 section 6). A class cap above `MAX_ROWS` is the producer's own check; the
        // arena limit and an unaddressable run id are this error path.
        let arena_limit = SpillRunError::ArenaExhausted {
            arena_rows: 8,
            requested_rows: 9,
            arena_row_limit: 8,
        };
        let unaddressable_id = SpillRunError::RunIdOutOfRange {
            id: u32::MAX - 1,
            arena_id_capacity: 8,
        };
        assert!(matches!(
            LaraOperationError::from(arena_limit),
            LaraOperationError::SpillNeedsPromotion
        ));
        assert!(matches!(
            LaraOperationError::from(unaddressable_id),
            LaraOperationError::SpillNeedsPromotion
        ));
        // A genuine growth failure is not a promotion: the caller reports it instead.
        let grow = SpillRunError::Grow(crate::GrowFailed {
            current_size: 0,
            delta: 0,
        });
        assert!(!matches!(
            LaraOperationError::from(grow),
            LaraOperationError::SpillNeedsPromotion
        ));
    }
}
