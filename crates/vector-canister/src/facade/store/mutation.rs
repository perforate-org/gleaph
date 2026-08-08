//! `vector_upsert` / `vector_remove` over the degenerate `ivf_flat` page store.
//!
//! Idempotence is decided **only** by `mutation_id` against the retained subject clock
//! (`VECTOR_SUBJECT_TO_ID`), never by comparing stored bytes — except the single
//! same-version-different-payload conflict guard on a live row. See ADR 0031 Slice 2.

use super::rebuild::rebuild_state_of;
use super::search::{assign_partition, read_centroids_at};
use super::{
    DEFAULT_MAX_PAGE_BYTES, DEGENERATE_PARTITION_ID, INITIAL_INDEX_VERSION, VectorCanisterStore,
};
use crate::encoding::EncodingRecord;
#[cfg(test)]
use crate::facade::stable::VECTOR_PARTITION_HEADS;
use crate::facade::stable::{
    IVF_CENTROID_META, PAGE_STORE, SHARD_CANISTER_CATALOG, VECTOR_INDEX_DEFS, VECTOR_SUBJECT_TO_ID,
};
#[cfg(test)]
use crate::records::PartitionKey;
use crate::records::{
    IvfCentroidMeta, SlotRef, SubjectKey, SubjectMapEntry, VectorIndexDef, VectorRebuildStateRecord,
};
use candid::Principal;
use gleaph_graph_kernel::vector_index::{
    VectorCanisterError, VectorEmbeddingSyncOp, VectorEncoding, VectorIndexKind, VectorMetric,
    VectorSubject,
};
use ic_stable_vector_page_store::{MAX_RUNS, PageLayout};

/// How a mutation must mirror its live effect across index versions during a rebuild (ADR 0031
/// Slice 7). Derived per-op from the durable rebuild state of `op.index_id`.
#[derive(Clone, Copy, Debug)]
enum RebuildMutationMode {
    /// No rebuild, or a phase with no shadow version yet / no longer (`Idle`/`Sampling`/`Training`/
    /// `Failed`/`Aborting`): operate only on the active version via `current_slot_for(active)`.
    /// Mutations during `Training` are active-only and are later shadowed when `Building` walks every
    /// live subject (ADR 0031 Slice 8).
    ActiveOnly,
    /// `Building`/`ReadyToPublish`: mirror the live effect into both the active and the shadow
    /// (`target`) version so the shadow stays publish-complete.
    DualWrite { target: u64, target_nlist: u32 },
    /// Post-publish `Cleaning`: the active version is already `target`; operate active-only via
    /// `current_slot_for(active)`. State-changing mutations collapse the touched subject
    /// (`slot = target, shadow = None`); pure idempotent no-ops are left to cleanup.
    Cleaning,
}

/// Resolves the per-op rebuild mutation mode from the durable rebuild state.
fn rebuild_mutation_mode(index_id: u32) -> RebuildMutationMode {
    match rebuild_state_of(index_id) {
        VectorRebuildStateRecord::Building {
            target_index_version,
            nlist,
            ..
        }
        | VectorRebuildStateRecord::ReadyToPublish {
            target_index_version,
            nlist,
        } => RebuildMutationMode::DualWrite {
            target: target_index_version,
            target_nlist: nlist,
        },
        VectorRebuildStateRecord::Cleaning { .. } => RebuildMutationMode::Cleaning,
        VectorRebuildStateRecord::Idle
        | VectorRebuildStateRecord::Sampling { .. }
        | VectorRebuildStateRecord::Training { .. }
        | VectorRebuildStateRecord::Aborting { .. }
        | VectorRebuildStateRecord::Failed { .. } => RebuildMutationMode::ActiveOnly,
    }
}

/// Computes `slots_per_page` from a page byte budget and stride, rejecting a `< 1` capacity.
/// Largest `slots_per_page` (capacity) whose page fits `max_page_bytes` under the new two-table page
/// geometry (ADR 0064 §7). Fails closed with [`VectorCanisterError::InvalidPageCapacity`] when even a
/// single-row page does not fit.
fn slots_per_page_for(
    max_page_bytes: u32,
    pad_stride_bytes: u32,
    meta_stride_bytes: u32,
    run_capacity: u32,
) -> Result<u32, VectorCanisterError> {
    let capacity = PageLayout::max_capacity_for(
        max_page_bytes as usize,
        pad_stride_bytes,
        meta_stride_bytes,
        run_capacity,
    )
    .ok_or(VectorCanisterError::InvalidPageCapacity)?;
    Ok(capacity)
}

/// Run-table width for a def: `min(owned_shards, MAX_RUNS)`, floored at 1. Owned shards come from the
/// shard↔canister catalog at def creation; the value is frozen into the def (ADR 0064 §7).
fn owned_run_capacity() -> u32 {
    let owned = SHARD_CANISTER_CATALOG.with_borrow(|c| c.owned_shard_count());
    (owned as u32).clamp(1, MAX_RUNS)
}

impl VectorCanisterStore {
    /// Asserts the caller is the attached canister for some shard, and that shard owns the subject.
    fn assert_caller_owns_subject(
        &self,
        caller: Principal,
        subject_shard: gleaph_graph_kernel::federation::ShardId,
    ) -> Result<(), VectorCanisterError> {
        let attached = crate::facade::stable::SHARD_CANISTER_CATALOG
            .with_borrow(|c| c.shard_for_canister(caller));
        let Some(shard) = attached else {
            return Err(VectorCanisterError::ShardNotAttached);
        };
        if shard != subject_shard {
            return Err(VectorCanisterError::ShardMismatch);
        }
        Ok(())
    }

    /// Returns the existing def, or lazily creates a degenerate `ivf_flat` def for an upsert.
    ///
    /// Slice 2 has no admin create-index endpoint; `kind`/`metric` have a single variant each, so a
    /// def created from the first op's `encoding`/`dims` is lossless. The Router will own definition
    /// creation in a later slice.
    fn ensure_def_for_upsert(
        &self,
        index_id: u32,
        encoding: VectorEncoding,
        dims: u16,
        metric: VectorMetric,
    ) -> Result<VectorIndexDef, VectorCanisterError> {
        if let Some(def) = VECTOR_INDEX_DEFS.with_borrow(|defs| defs.get(&index_id)) {
            if def.metric != metric {
                return Err(VectorCanisterError::MetricMismatch);
            }
            return Ok(def);
        }
        // The encoding record is the single source of width truth (ADR 0064 §8): it validates the
        // (encoding, dims, metric) combination and derives the stored stride before any def or row
        // is written. `dimension_mismatch` maps any invalid combination (wire-unchanged).
        let record = EncodingRecord::from_parts(encoding, dims)
            .map_err(|_| VectorCanisterError::DimensionMismatch)?;
        let stride_bytes = record.stride_bytes;
        let pad_stride_bytes = record.pad_stride_bytes;
        let meta_stride_bytes = record.meta_stride();
        let run_capacity = owned_run_capacity();
        let slots_per_page = slots_per_page_for(
            DEFAULT_MAX_PAGE_BYTES,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
        )?;
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding,
            dims,
            metric,
            nlist: 1,
            active_index_version: INITIAL_INDEX_VERSION,
            stride_bytes,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
            max_page_bytes: DEFAULT_MAX_PAGE_BYTES,
            slots_per_page,
        };
        VECTOR_INDEX_DEFS.with_borrow_mut(|defs| defs.insert(index_id, def));
        IVF_CENTROID_META.with_borrow_mut(|meta| meta.insert(index_id, IvfCentroidMeta::default()));
        Ok(def)
    }

    /// Appends a vector row into the given partition's page chain via the slab page store
    /// ([`crate::facade::stable::page_store`], ADR 0064 §7), rolling a new page when the mutable page
    /// is full or its run table would overflow. Fallible because the slab can fail to `grow`; the
    /// store commits write-then-commit so a failed grow leaves no head/meta pointing at unwritten
    /// bytes.
    ///
    /// Production callers pass `DEGENERATE_PARTITION_ID` (every production def is `nlist == 1`); the
    /// `partition_id` parameter is what lets the Slice 6 seed helpers populate `nlist > 1` partition
    /// chains and is forward-useful for the Slice 7 rebuild.
    pub(super) fn append_slot(
        &self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        def: &VectorIndexDef,
        subject: VectorSubject,
        bytes: &[u8],
    ) -> Result<SlotRef, VectorCanisterError> {
        PAGE_STORE.with_borrow_mut(|store| {
            store.append_row(index_id, index_version, partition_id, def, subject, bytes)
        })
    }

    /// Marks a slot tombstoned via the slab page store, which owns the `VectorPageMeta` live/
    /// tombstone counts and the `VECTOR_PARTITION_HEADS.live_len` decrement. Idempotent.
    pub(super) fn tombstone_slot(&self, index_id: u32, slot: SlotRef) {
        PAGE_STORE.with_borrow_mut(|store| {
            store.tombstone_row(index_id, slot);
        });
    }

    pub(super) fn read_slot_bytes(&self, index_id: u32, slot: SlotRef) -> Option<Vec<u8>> {
        PAGE_STORE.with_borrow(|store| store.read_row_bytes(index_id, slot).map(|(_, bytes)| bytes))
    }

    /// Partition for an append on the **active** version: degenerate partition `0` when `nlist <= 1`,
    /// otherwise the nearest active centroid (ADR 0031 Slice 6/7). A missing/incomplete active
    /// centroid set falls back to partition `0` (the same fail-soft the search path uses). This is
    /// what makes a published `nlist > 1` index mutable.
    fn active_partition(&self, def: &VectorIndexDef, index_id: u32, bytes: &[u8]) -> u32 {
        if def.nlist <= 1 {
            return DEGENERATE_PARTITION_ID;
        }
        match read_centroids_at(index_id, def.active_index_version, def.nlist, def.dims) {
            Some(centroids) => assign_partition(&centroids, bytes),
            None => DEGENERATE_PARTITION_ID,
        }
    }

    /// Partition for an append into the rebuild's **shadow** (`target`) version: nearest target
    /// centroid (the shadow always has `nlist > 1` ready centroids by construction).
    fn shadow_partition(
        &self,
        index_id: u32,
        target: u64,
        target_nlist: u32,
        dims: u16,
        bytes: &[u8],
    ) -> u32 {
        match read_centroids_at(index_id, target, target_nlist, dims) {
            Some(centroids) => assign_partition(&centroids, bytes),
            None => DEGENERATE_PARTITION_ID,
        }
    }

    /// Applies an upsert, ordered by the single `mutation_id` stamp against the retained subject
    /// clock (ADR 0064 §5):
    ///
    /// - **Older incarnation** (`op.inc < clock.inc`): stale no-op — a stale replay can never
    ///   resurrect or mutate a subject whose identity has already moved on.
    /// - **Newer incarnation** (`op.inc > clock.inc`): **resurrect** with a *fresh* `VectorId`. This
    ///   is the only resurrection path; it requires a strictly greater incarnation, which the graph
    ///   canonical store allocates on each delete/reinsert. Any live slot of the older incarnation is
    ///   tombstoned first so it cannot orphan.
    /// - **Same incarnation** (`op.inc == clock.inc`): version rules within the incarnation. If the
    ///   subject is already deleted at this stamp the upsert is a stale replay (no-op, since a
    ///   genuine reinsert carries a greater stamp). On a live subject: stale `<` no-op; `==`
    ///   identical no-op / different `MutationStampConflict`; `>` appends a new slot.
    pub fn vector_upsert(
        &self,
        caller: Principal,
        op: &VectorEmbeddingSyncOp,
    ) -> Result<(), VectorCanisterError> {
        if op.remove {
            return Err(VectorCanisterError::MutationKindMismatch);
        }
        self.assert_caller_owns_subject(caller, op.subject.shard_id())?;
        let def = self.ensure_def_for_upsert(op.index_id, op.encoding, op.dims, op.metric)?;
        if op.encoding != def.encoding || op.dims != def.dims {
            return Err(VectorCanisterError::DimensionMismatch);
        }
        if op.bytes.len() != def.stride_bytes as usize {
            return Err(VectorCanisterError::ByteWidthMismatch);
        }
        let active = def.active_index_version;
        let mode = rebuild_mutation_mode(op.index_id);
        let key = SubjectKey::new(op.index_id, op.subject);
        let existing = VECTOR_SUBJECT_TO_ID.with_borrow(|m| m.get(&key));

        let Some(entry) = existing else {
            // New subject: allocate a fresh VectorId and create a live slot.
            self.insert_new_subject(op, &def, mode, key)?;
            return Ok(());
        };

        match op.mutation_id.cmp(&entry.stamp) {
            std::cmp::Ordering::Less => Ok(()), // stale replay: no-op
            std::cmp::Ordering::Greater => {
                // Newer stamp: append a fresh slot (and shadow while dual-writing) FIRST (fallible),
                // then tombstone any live slot of the older stamp (infallible), then commit. This
                // preserves the write-then-commit atomicity: a failed shadow append leaves the old
                // slot live.
                let active_partition = self.active_partition(&def, op.index_id, &op.bytes);
                let new_slot = self.append_slot(
                    op.index_id,
                    active,
                    active_partition,
                    &def,
                    op.subject,
                    &op.bytes,
                )?;
                let shadow_slot = match mode {
                    RebuildMutationMode::DualWrite {
                        target,
                        target_nlist,
                    } => {
                        let partition = self.shadow_partition(
                            op.index_id,
                            target,
                            target_nlist,
                            def.dims,
                            &op.bytes,
                        );
                        match self.append_slot(
                            op.index_id,
                            target,
                            partition,
                            &def,
                            op.subject,
                            &op.bytes,
                        ) {
                            Ok(shadow) => Some(shadow),
                            Err(err) => {
                                self.tombstone_slot(op.index_id, new_slot);
                                return Err(err);
                            }
                        }
                    }
                    RebuildMutationMode::ActiveOnly | RebuildMutationMode::Cleaning => None,
                };
                // Infallible: tombstone the superseded live slots (active, and the old shadow while
                // dual-writing).
                if !entry.deleted {
                    if let Some(active_slot) = entry.current_slot_for(active) {
                        self.tombstone_slot(op.index_id, active_slot);
                    }
                    if let RebuildMutationMode::DualWrite { .. } = mode
                        && let Some(old_shadow) = entry.shadow_slot
                    {
                        self.tombstone_slot(op.index_id, old_shadow);
                    }
                }
                VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                    m.insert(
                        key,
                        SubjectMapEntry {
                            stamp: op.mutation_id,
                            deleted: false,
                            slot: Some(new_slot),
                            shadow_slot,
                        },
                    )
                });
                Ok(())
            }
            std::cmp::Ordering::Equal => {
                if entry.deleted {
                    // Same stamp already tombstoned: a genuine reinsert would carry a greater stamp,
                    // so this is a stale replay.
                    return Ok(());
                }
                // Same stamp on a live subject: idempotent no-op if the bytes match, else a conflict.
                let slot = entry
                    .current_slot_for(active)
                    .expect("live entry has a slot");
                let stored = self.read_slot_bytes(op.index_id, slot).unwrap_or_default();
                if stored == op.bytes {
                    // Pure idempotent no-op: nothing changes. During `Cleaning` this intentionally
                    // does *not* collapse `shadow_slot -> slot` (collapse-on-touch only applies to
                    // state-changing mutations); search stays correct via `current_slot_for` and
                    // the subject is collapsed later by `cleanup_step`.
                    return Ok(());
                }
                Err(VectorCanisterError::MutationStampConflict)
            }
        }
    }

    /// Inserts a brand-new (or resurrected) live subject. The active row is assigned to its active
    /// partition; while dual-writing, a mirror row is also appended into the shadow `target` version
    /// and recorded in `shadow_slot` (ADR 0031 Slice 7).
    fn insert_new_subject(
        &self,
        op: &VectorEmbeddingSyncOp,
        def: &VectorIndexDef,
        mode: RebuildMutationMode,
        key: SubjectKey,
    ) -> Result<(), VectorCanisterError> {
        let active = def.active_index_version;
        let active_partition = self.active_partition(def, op.index_id, &op.bytes);
        let slot = self.append_slot(
            op.index_id,
            active,
            active_partition,
            def,
            op.subject,
            &op.bytes,
        )?;
        // Append the shadow mirror (while dual-writing) BEFORE committing the subject map so a
        // fallible slab grow cannot orphan it against a missing shadow row. On shadow failure
        // we tombstone the just-appended active row before returning, so the residual is a tombstoned
        // dead row (live counters restored) rather than a live-counted orphan; the subject map
        // stays untouched.
        let shadow_slot = match mode {
            RebuildMutationMode::DualWrite {
                target,
                target_nlist,
            } => {
                let partition =
                    self.shadow_partition(op.index_id, target, target_nlist, def.dims, &op.bytes);
                match self.append_slot(op.index_id, target, partition, def, op.subject, &op.bytes) {
                    Ok(shadow) => Some(shadow),
                    Err(err) => {
                        self.tombstone_slot(op.index_id, slot);
                        return Err(err);
                    }
                }
            }
            RebuildMutationMode::ActiveOnly | RebuildMutationMode::Cleaning => None,
        };
        VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
            m.insert(
                key,
                SubjectMapEntry {
                    stamp: op.mutation_id,
                    deleted: false,
                    slot: Some(slot),
                    shadow_slot,
                },
            )
        });
        Ok(())
    }

    /// Applies a remove, ordered by the single `mutation_id` stamp against the retained subject
    /// clock (ADR 0064 §5):
    ///
    /// - **Older incarnation** (`op.inc < clock.inc`): stale no-op. This closes the reverse-orphan
    ///   race — a late repair-drain remove for a deleted incarnation can never tombstone a newer
    ///   reinsert that already advanced the clock.
    /// - **Newer incarnation** (`op.inc > clock.inc`): authoritative remove for an as-yet-unseen
    ///   incarnation; tombstone any live slot and record the deleted clock at the op's incarnation.
    /// - **Same incarnation** (`op.inc == clock.inc`): stale `<` version no-op; on a deleted subject
    ///   bump the clock if `>`; on a live subject tombstone the active slot.
    ///
    /// A `remove` for a never-inserted subject still **writes a tombstone clock** (not a pure no-op).
    /// The clock no longer *blocks* resurrection by itself: a delivered upsert with a greater
    /// incarnation resurrects (see [`Self::vector_upsert`]). Stale-replay protection is the
    /// incarnation fence plus the graph repair-drain's canonical re-derivation
    /// ([`crate::index::repair_journal`]); a canonical-wins removal arrives with an authoritative
    /// (maximum) `mutation_id` so it supersedes any live slot of the same stamp.
    pub fn vector_remove(
        &self,
        caller: Principal,
        op: &VectorEmbeddingSyncOp,
    ) -> Result<(), VectorCanisterError> {
        if !op.remove {
            return Err(VectorCanisterError::MutationKindMismatch);
        }
        self.assert_caller_owns_subject(caller, op.subject.shard_id())?;
        let mode = rebuild_mutation_mode(op.index_id);
        let active = VECTOR_INDEX_DEFS
            .with_borrow(|defs| defs.get(&op.index_id))
            .map(|def| def.active_index_version);
        let key = SubjectKey::new(op.index_id, op.subject);
        let existing = VECTOR_SUBJECT_TO_ID.with_borrow(|m| m.get(&key));

        let Some(entry) = existing else {
            VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                m.insert(
                    key,
                    SubjectMapEntry {
                        stamp: op.mutation_id,
                        deleted: true,
                        slot: None,
                        shadow_slot: None,
                    },
                )
            });
            return Ok(());
        };

        // Live active slot resolved against the active version (`shadow_slot` once published into
        // `Cleaning`); falls back to `entry.slot` only if the def somehow vanished.
        let active_live_slot = active
            .and_then(|a| entry.current_slot_for(a))
            .or(entry.slot);

        match op.mutation_id.cmp(&entry.stamp) {
            std::cmp::Ordering::Less => Ok(()), // stale remove: no-op (fenced)
            std::cmp::Ordering::Greater => {
                // Authoritative remove for a newer stamp: tombstone any live slot (active, and the
                // shadow while dual-writing) and record the deleted clock at the op's stamp.
                if !entry.deleted {
                    if let Some(slot) = active_live_slot {
                        self.tombstone_slot(op.index_id, slot);
                    }
                    if let RebuildMutationMode::DualWrite { .. } = mode
                        && let Some(shadow_slot) = entry.shadow_slot
                    {
                        self.tombstone_slot(op.index_id, shadow_slot);
                    }
                }
                VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                    m.insert(
                        key,
                        SubjectMapEntry {
                            stamp: op.mutation_id,
                            deleted: true,
                            slot: None,
                            shadow_slot: None,
                        },
                    )
                });
                Ok(())
            }
            std::cmp::Ordering::Equal => {
                if entry.deleted {
                    // Same stamp already tombstoned: no-op.
                    return Ok(());
                }
                // Same stamp on a live subject: tombstone the active slot (and shadow while
                // dual-writing) and record the deleted clock.
                let slot = active_live_slot.expect("live entry has a slot");
                self.tombstone_slot(op.index_id, slot);
                if let RebuildMutationMode::DualWrite { .. } = mode
                    && let Some(shadow_slot) = entry.shadow_slot
                {
                    self.tombstone_slot(op.index_id, shadow_slot);
                }
                VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                    m.insert(
                        key,
                        SubjectMapEntry {
                            stamp: op.mutation_id,
                            deleted: true,
                            slot: None,
                            shadow_slot: None,
                        },
                    )
                });
                Ok(())
            }
        }
    }

    // --- Test-only inspection / setup helpers ---

    /// Creates an index def with an explicit page byte budget (test-only; production creates defs
    /// lazily on first upsert with [`DEFAULT_MAX_PAGE_BYTES`]).
    #[cfg(test)]
    pub(crate) fn create_index_for_test(
        &self,
        index_id: u32,
        encoding: VectorEncoding,
        dims: u16,
        max_page_bytes: u32,
    ) -> Result<(), VectorCanisterError> {
        let record = EncodingRecord::from_parts(encoding, dims)
            .map_err(|_| VectorCanisterError::DimensionMismatch)?;
        let stride_bytes = record.stride_bytes;
        let pad_stride_bytes = record.pad_stride_bytes;
        let meta_stride_bytes = record.meta_stride();
        let run_capacity = 1; // isolated single-shard tests
        let slots_per_page = slots_per_page_for(
            max_page_bytes,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
        )?;
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding,
            dims,
            metric: VectorMetric::L2Squared,
            nlist: 1,
            active_index_version: INITIAL_INDEX_VERSION,
            stride_bytes,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
            max_page_bytes,
            slots_per_page,
        };
        VECTOR_INDEX_DEFS.with_borrow_mut(|defs| defs.insert(index_id, def));
        IVF_CENTROID_META.with_borrow_mut(|meta| meta.insert(index_id, IvfCentroidMeta::default()));
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn subject_entry_for_test(
        &self,
        index_id: u32,
        subject: gleaph_graph_kernel::vector_index::VectorSubject,
    ) -> Option<SubjectMapEntry> {
        VECTOR_SUBJECT_TO_ID.with_borrow(|m| m.get(&SubjectKey::new(index_id, subject)))
    }

    #[cfg(test)]
    pub(crate) fn def_for_test(&self, index_id: u32) -> Option<VectorIndexDef> {
        VECTOR_INDEX_DEFS.with_borrow(|defs| defs.get(&index_id))
    }

    #[cfg(test)]
    pub(crate) fn partition_head_for_test(
        &self,
        index_id: u32,
        index_version: u64,
    ) -> Option<crate::records::PartitionHead> {
        VECTOR_PARTITION_HEADS.with_borrow(|heads| {
            heads.get(&PartitionKey::new(
                index_id,
                index_version,
                DEGENERATE_PARTITION_ID,
            ))
        })
    }
}
