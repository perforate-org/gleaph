//! `vector_upsert` / `vector_remove` over the degenerate `ivf_flat` page store.
//!
//! Idempotence is decided **only** by `embedding_version` against the retained subject clock
//! (`VECTOR_SUBJECT_TO_ID`), never by comparing stored bytes — except the single
//! same-version-different-payload conflict guard on a live row. See ADR 0031 Slice 2.

use super::rebuild::rebuild_state_of;
use super::search::{assign_partition, read_centroids_at};
use super::{
    DEFAULT_MAX_PAGE_BYTES, DEGENERATE_PARTITION_ID, FIRST_ALLOCATION, INITIAL_INDEX_VERSION,
    PAGE_HEADER_BYTES, VectorIndexStore,
};
#[cfg(test)]
use crate::facade::stable::VECTOR_PARTITION_HEADS;
use crate::facade::stable::{
    IVF_CENTROID_META, PAGE_STORE, VECTOR_ID_TO_SLOT, VECTOR_ID_TO_SUBJECT, VECTOR_INDEX_DEFS,
    VECTOR_SUBJECT_TO_ID,
};
#[cfg(test)]
use crate::records::PartitionKey;
use crate::records::{
    IvfCentroidMeta, SlotRef, SubjectKey, SubjectMapEntry, VectorIdKey, VectorIndexDef,
    VectorRebuildStateRecord, VectorSubjectRecord,
};
use candid::Principal;
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorIndexError, VectorIndexKind, VectorMetric,
    VectorSubject,
};

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
fn slots_per_page_for(max_page_bytes: u32, stride_bytes: u32) -> Result<u32, VectorIndexError> {
    if stride_bytes == 0 {
        return Err(VectorIndexError::DimensionMismatch);
    }
    let usable = max_page_bytes.saturating_sub(PAGE_HEADER_BYTES);
    let slots = usable / stride_bytes;
    if slots < 1 {
        return Err(VectorIndexError::InvalidPageCapacity);
    }
    Ok(slots)
}

impl VectorIndexStore {
    /// Asserts the caller is the attached canister for some shard, and that shard owns the subject.
    fn assert_caller_owns_subject(
        &self,
        caller: Principal,
        subject_shard: gleaph_graph_kernel::federation::ShardId,
    ) -> Result<(), VectorIndexError> {
        let attached = crate::facade::stable::SHARD_CANISTER_CATALOG
            .with_borrow(|c| c.shard_for_canister(caller));
        let Some(shard) = attached else {
            return Err(VectorIndexError::ShardNotAttached);
        };
        if shard != subject_shard {
            return Err(VectorIndexError::ShardMismatch);
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
    ) -> Result<VectorIndexDef, VectorIndexError> {
        if let Some(def) = VECTOR_INDEX_DEFS.with_borrow(|defs| defs.get(&index_id)) {
            return Ok(def);
        }
        let stride_bytes = encoding.stride_bytes(dims);
        let slots_per_page = slots_per_page_for(DEFAULT_MAX_PAGE_BYTES, stride_bytes)?;
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding,
            dims,
            metric: VectorMetric::L2Squared,
            nlist: 1,
            active_index_version: INITIAL_INDEX_VERSION,
            stride_bytes,
            max_page_bytes: DEFAULT_MAX_PAGE_BYTES,
            slots_per_page,
            next_vector_id: FIRST_ALLOCATION,
        };
        VECTOR_INDEX_DEFS.with_borrow_mut(|defs| defs.insert(index_id, def));
        IVF_CENTROID_META.with_borrow_mut(|meta| meta.insert(index_id, IvfCentroidMeta::default()));
        Ok(def)
    }

    /// Allocates a fresh, never-reused `VectorId` from the durable defs allocator.
    fn alloc_vector_id(&self, index_id: u32) -> Result<u64, VectorIndexError> {
        VECTOR_INDEX_DEFS.with_borrow_mut(|defs| {
            let mut def = defs.get(&index_id).ok_or(VectorIndexError::UnknownIndex)?;
            let id = def.next_vector_id;
            def.next_vector_id = def
                .next_vector_id
                .checked_add(1)
                .ok_or(VectorIndexError::AllocatorOverflow)?;
            defs.insert(index_id, def);
            Ok(id)
        })
    }

    /// Appends a vector row into the given partition's page chain via the slab page store
    /// ([`crate::facade::stable::page_store`], ADR 0032), rolling a new page when the mutable page is
    /// full. Fallible because the slab can fail to `grow`; the store commits write-then-commit so a
    /// failed grow leaves no head/meta pointing at unwritten bytes.
    ///
    /// Production callers pass `DEGENERATE_PARTITION_ID` (every production def is `nlist == 1`);
    /// the `partition_id` parameter is what lets the Slice 6 seed helpers populate `nlist > 1`
    /// partition chains and is forward-useful for the Slice 7 rebuild.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn append_slot(
        &self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        def: &VectorIndexDef,
        vector_id: u64,
        generation: u64,
        subject: VectorSubject,
        bytes: &[u8],
    ) -> Result<SlotRef, VectorIndexError> {
        PAGE_STORE.with_borrow_mut(|store| {
            store.append_row(
                index_id,
                index_version,
                partition_id,
                def,
                vector_id,
                generation,
                subject,
                bytes,
            )
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

    /// Applies an upsert, ordered by the pair `(embedding_incarnation, embedding_version)` against
    /// the retained subject clock (ADR 0031 Slice 4):
    ///
    /// - **Older incarnation** (`op.inc < clock.inc`): stale no-op — a stale replay can never
    ///   resurrect or mutate a subject whose identity has already moved on.
    /// - **Newer incarnation** (`op.inc > clock.inc`): **resurrect** with a *fresh* `VectorId`. This
    ///   is the only resurrection path; it requires a strictly greater incarnation, which the graph
    ///   canonical store allocates on each delete/reinsert. Any live slot of the older incarnation is
    ///   tombstoned first so it cannot orphan.
    /// - **Same incarnation** (`op.inc == clock.inc`): version rules within the incarnation. If the
    ///   subject is already deleted at this incarnation the upsert is a stale replay (no-op, since a
    ///   genuine reinsert carries a greater incarnation). On a live subject: stale `<` no-op; `==`
    ///   identical no-op / different `EmbeddingVersionConflict`; `>` appends a new slot reusing the
    ///   live `VectorId`.
    pub fn vector_upsert(
        &self,
        caller: Principal,
        op: &VectorEmbeddingSyncOp,
    ) -> Result<(), VectorIndexError> {
        if op.remove {
            return Err(VectorIndexError::MutationKindMismatch);
        }
        self.assert_caller_owns_subject(caller, op.subject.shard_id())?;
        let def = self.ensure_def_for_upsert(op.index_id, op.encoding, op.dims)?;
        if op.encoding != def.encoding || op.dims != def.dims {
            return Err(VectorIndexError::DimensionMismatch);
        }
        if op.bytes.len() != def.stride_bytes as usize {
            return Err(VectorIndexError::ByteWidthMismatch);
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

        match op.embedding_incarnation.cmp(&entry.embedding_incarnation) {
            std::cmp::Ordering::Less => Ok(()), // stale older-incarnation replay: no-op
            std::cmp::Ordering::Greater => {
                // Fresh incarnation: resurrect with a brand-new VectorId. Tombstone any live slot of
                // the older incarnation (active, and the shadow while dual-writing) so it does not
                // orphan.
                if !entry.deleted {
                    if let Some(active_slot) = entry.current_slot_for(active) {
                        self.tombstone_slot(op.index_id, active_slot);
                    }
                    if let RebuildMutationMode::DualWrite { .. } = mode
                        && let Some(shadow_slot) = entry.shadow_slot
                    {
                        self.tombstone_slot(op.index_id, shadow_slot);
                    }
                    if let Some(vector_id) = entry.vector_id {
                        let id_key = VectorIdKey::new(op.index_id, vector_id);
                        VECTOR_ID_TO_SLOT.with_borrow_mut(|m| m.remove(&id_key));
                        VECTOR_ID_TO_SUBJECT.with_borrow_mut(|m| m.remove(&id_key));
                    }
                }
                self.insert_new_subject(op, &def, mode, key)?;
                Ok(())
            }
            std::cmp::Ordering::Equal => {
                if entry.deleted {
                    // Same incarnation already tombstoned: a genuine reinsert would carry a greater
                    // incarnation, so this is a stale replay.
                    return Ok(());
                }
                let clock = entry.stored_embedding_version;
                if op.embedding_version < clock {
                    return Ok(()); // stale replay within the live incarnation
                }
                if op.embedding_version == clock {
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
                    return Err(VectorIndexError::EmbeddingVersionConflict);
                }
                // newer version within the live incarnation: append a new slot, reuse the live id.
                let old_slot = entry
                    .current_slot_for(active)
                    .expect("live entry has a slot");
                let vector_id = entry.vector_id.expect("live entry has a vector_id");
                let generation = old_slot.generation + 1;
                let active_partition = self.active_partition(&def, op.index_id, &op.bytes);
                let new_slot = self.append_slot(
                    op.index_id,
                    active,
                    active_partition,
                    &def,
                    vector_id,
                    generation,
                    op.subject,
                    &op.bytes,
                )?;
                // Append the shadow mirror (while dual-writing) BEFORE any tombstone / id-map /
                // subject-map commit. `append_slot` is the only fallible step (slab `grow`), so doing
                // both appends first means a failed shadow grow returns `Err` with the subject clock
                // and id map still pointing at the old, valid live slot — never at a tombstoned slot
                // or a missing shadow row. On shadow failure we also tombstone the just-appended
                // active row, so the residual is a tombstoned dead row rather than a live-counted
                // orphan that would inflate `VectorPageMeta.live_count` / `PartitionHead.live_len`
                // (and thus partition-health accounting).
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
                            vector_id,
                            generation,
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
                // All fallible appends succeeded; the remaining mutations are infallible. Tombstone
                // the superseded slots (active, and the old shadow while dual-writing — collapse to
                // shadow = None otherwise so a `Cleaning`-window touch normalizes to the target slot).
                self.tombstone_slot(op.index_id, old_slot);
                if let RebuildMutationMode::DualWrite { .. } = mode
                    && let Some(old_shadow) = entry.shadow_slot
                {
                    self.tombstone_slot(op.index_id, old_shadow);
                }
                VECTOR_ID_TO_SLOT.with_borrow_mut(|m| {
                    m.insert(VectorIdKey::new(op.index_id, vector_id), new_slot)
                });
                VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                    m.insert(
                        key,
                        SubjectMapEntry {
                            embedding_incarnation: op.embedding_incarnation,
                            stored_embedding_version: op.embedding_version,
                            deleted: false,
                            slot: Some(new_slot),
                            shadow_slot,
                            vector_id: Some(vector_id),
                        },
                    )
                });
                Ok(())
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
    ) -> Result<(), VectorIndexError> {
        let active = def.active_index_version;
        let vector_id = self.alloc_vector_id(op.index_id)?;
        let active_partition = self.active_partition(def, op.index_id, &op.bytes);
        let slot = self.append_slot(
            op.index_id,
            active,
            active_partition,
            def,
            vector_id,
            FIRST_ALLOCATION,
            op.subject,
            &op.bytes,
        )?;
        // Append the shadow mirror (while dual-writing) BEFORE committing the id/subject maps so a
        // fallible slab grow cannot orphan those maps against a missing shadow row. On shadow failure
        // we tombstone the just-appended active row before returning, so the residual is a tombstoned
        // dead row (live counters restored) rather than a live-counted orphan; the id/subject maps
        // stay untouched.
        let shadow_slot = match mode {
            RebuildMutationMode::DualWrite {
                target,
                target_nlist,
            } => {
                let partition =
                    self.shadow_partition(op.index_id, target, target_nlist, def.dims, &op.bytes);
                match self.append_slot(
                    op.index_id,
                    target,
                    partition,
                    def,
                    vector_id,
                    FIRST_ALLOCATION,
                    op.subject,
                    &op.bytes,
                ) {
                    Ok(shadow) => Some(shadow),
                    Err(err) => {
                        self.tombstone_slot(op.index_id, slot);
                        return Err(err);
                    }
                }
            }
            RebuildMutationMode::ActiveOnly | RebuildMutationMode::Cleaning => None,
        };
        let id_key = VectorIdKey::new(op.index_id, vector_id);
        VECTOR_ID_TO_SLOT.with_borrow_mut(|m| m.insert(id_key, slot));
        VECTOR_ID_TO_SUBJECT.with_borrow_mut(|m| m.insert(id_key, VectorSubjectRecord(op.subject)));
        VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
            m.insert(
                key,
                SubjectMapEntry {
                    embedding_incarnation: op.embedding_incarnation,
                    stored_embedding_version: op.embedding_version,
                    deleted: false,
                    slot: Some(slot),
                    shadow_slot,
                    vector_id: Some(vector_id),
                },
            )
        });
        Ok(())
    }

    /// Applies a remove, ordered by the pair `(embedding_incarnation, embedding_version)` against the
    /// retained subject clock (ADR 0031 Slice 4):
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
    /// (maximum) `embedding_version` so it supersedes any live slot of the same incarnation.
    pub fn vector_remove(
        &self,
        caller: Principal,
        op: &VectorEmbeddingSyncOp,
    ) -> Result<(), VectorIndexError> {
        if !op.remove {
            return Err(VectorIndexError::MutationKindMismatch);
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
                        embedding_incarnation: op.embedding_incarnation,
                        stored_embedding_version: op.embedding_version,
                        deleted: true,
                        slot: None,
                        shadow_slot: None,
                        vector_id: None,
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

        match op.embedding_incarnation.cmp(&entry.embedding_incarnation) {
            std::cmp::Ordering::Less => Ok(()), // stale older-incarnation remove: no-op (fenced)
            std::cmp::Ordering::Greater => {
                // Authoritative remove for a newer, as-yet-unseen incarnation: tombstone any live
                // slot (active, and the shadow while dual-writing) and record the deleted clock at
                // the op's incarnation.
                if !entry.deleted {
                    if let Some(slot) = active_live_slot {
                        self.tombstone_slot(op.index_id, slot);
                    }
                    if let RebuildMutationMode::DualWrite { .. } = mode
                        && let Some(shadow_slot) = entry.shadow_slot
                    {
                        self.tombstone_slot(op.index_id, shadow_slot);
                    }
                    if let Some(vector_id) = entry.vector_id {
                        let id_key = VectorIdKey::new(op.index_id, vector_id);
                        VECTOR_ID_TO_SLOT.with_borrow_mut(|m| m.remove(&id_key));
                        VECTOR_ID_TO_SUBJECT.with_borrow_mut(|m| m.remove(&id_key));
                    }
                }
                VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                    m.insert(
                        key,
                        SubjectMapEntry {
                            embedding_incarnation: op.embedding_incarnation,
                            stored_embedding_version: op.embedding_version,
                            deleted: true,
                            slot: None,
                            shadow_slot: None,
                            vector_id: None,
                        },
                    )
                });
                Ok(())
            }
            std::cmp::Ordering::Equal => {
                let clock = entry.stored_embedding_version;
                if op.embedding_version < clock {
                    return Ok(()); // stale repair replay after a newer upsert
                }
                if entry.deleted {
                    if op.embedding_version > clock {
                        VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                            let mut e = m.get(&key).expect("entry present");
                            e.stored_embedding_version = op.embedding_version;
                            m.insert(key, e);
                        });
                    }
                    return Ok(());
                }
                // live, op.embedding_version >= clock: tombstone the active slot (and shadow while
                // dual-writing).
                let slot = active_live_slot.expect("live entry has a slot");
                let vector_id = entry.vector_id.expect("live entry has a vector_id");
                self.tombstone_slot(op.index_id, slot);
                if let RebuildMutationMode::DualWrite { .. } = mode
                    && let Some(shadow_slot) = entry.shadow_slot
                {
                    self.tombstone_slot(op.index_id, shadow_slot);
                }
                let id_key = VectorIdKey::new(op.index_id, vector_id);
                VECTOR_ID_TO_SLOT.with_borrow_mut(|m| m.remove(&id_key));
                VECTOR_ID_TO_SUBJECT.with_borrow_mut(|m| m.remove(&id_key));
                VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                    m.insert(
                        key,
                        SubjectMapEntry {
                            embedding_incarnation: op.embedding_incarnation,
                            stored_embedding_version: op.embedding_version,
                            deleted: true,
                            slot: None,
                            shadow_slot: None,
                            vector_id: None,
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
    ) -> Result<(), VectorIndexError> {
        let stride_bytes = encoding.stride_bytes(dims);
        let slots_per_page = slots_per_page_for(max_page_bytes, stride_bytes)?;
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding,
            dims,
            metric: VectorMetric::L2Squared,
            nlist: 1,
            active_index_version: INITIAL_INDEX_VERSION,
            stride_bytes,
            max_page_bytes,
            slots_per_page,
            next_vector_id: FIRST_ALLOCATION,
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

    #[cfg(test)]
    pub(crate) fn id_to_slot_for_test(&self, index_id: u32, vector_id: u64) -> Option<SlotRef> {
        VECTOR_ID_TO_SLOT.with_borrow(|m| m.get(&VectorIdKey::new(index_id, vector_id)))
    }

    #[cfg(test)]
    pub(crate) fn id_to_subject_for_test(
        &self,
        index_id: u32,
        vector_id: u64,
    ) -> Option<gleaph_graph_kernel::vector_index::VectorSubject> {
        VECTOR_ID_TO_SUBJECT
            .with_borrow(|m| m.get(&VectorIdKey::new(index_id, vector_id)))
            .map(|r| r.0)
    }
}
