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

use crate::labeled::spill_run_store::{ArenaRows, SpillRunError, SpillRunId};
use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    VertexId,
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

use super::{DeleteTarget, EdgeLayout, EdgeStore};

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    // ---------------------------------------------------------------- typed wrappers

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
        if index >= self.spill_used_len(run) {
            return Err(LaraOperationError::SpillRowOutOfRange);
        }
        self.spill.read_row(run, index, out);
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
        let tombstone = E::tombstone_edge();
        if E::BYTES <= super::INLINE_EDGE_BYTES {
            let mut payload = [0u8; super::INLINE_EDGE_BYTES];
            tombstone.write_to(&mut payload[..E::BYTES]);
            self.spill_write_row(run, index, &payload[..E::BYTES])
        } else {
            let mut payload = vec![0u8; E::BYTES];
            tombstone.write_to(&mut payload);
            self.spill_write_row(run, index, &payload)
        }
    }

    /// The run's rows as replay entries (`None` = a tombstoned row), in ordinal order.
    pub(crate) fn spill_replay_entries(
        &self,
        run: SpillRunId,
    ) -> Result<Vec<Option<E>>, LaraOperationError> {
        let used = self.spill_used_len(run);
        let mut out = Vec::new();
        out.try_reserve_exact(used as usize)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        for index in 0..used {
            let edge = self.spill_edge_at(run, index)?;
            if edge.is_deleted_slot() {
                out.push(None);
            } else {
                out.push(Some(edge));
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
            SpillRunError::ArenaExhausted { .. } | SpillRunError::RunIdOutOfRange { .. } => {
                // One outcome for every way the arena refuses: the caller promotes the bucket.
                LaraOperationError::SpillNeedsPromotion
            }
            SpillRunError::Grow(err) => LaraOperationError::WriteLogFailed(err),
        }
    }
}
