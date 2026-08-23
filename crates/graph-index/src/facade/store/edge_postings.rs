//! Edge property equality postings (ADR 0009 §1).
//!
//! Label identity (GAP-2026-08-22-001): stored keys carry LARA wire-tagged labels while
//! lookup requests name catalog ids; every catalog-id request scans both bucket packings
//! through [`wire_packings`], owned by `gleaph_graph_kernel::entry`.

use super::{
    IndexStore, clamp_posting_page_limit, ensure_index_value_key, ensure_posting_range_request,
};
use crate::edge_key::EdgePostingKey;
use crate::facade::stable::INDEX_EDGE_POSTINGS;
use crate::posting_range::edge_posting_key_half_open_range;
use crate::state::IndexError;
use candid::{Encode, Principal};
use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    EdgePostingCursor, EdgePostingHit, EdgePostingHitPage, IndexSubject,
    LookupEdgeEqualBatchRequest, LookupEdgeEqualBatchResult, LookupEdgeEqualPageRequest,
    LookupEdgeRangePageRequest, PhysicalIndexId,
};
use std::ops::Bound;

/// Both LARA bucket packings of one catalog label, in stored-key order: the undirected
/// packing (the bare catalog id) sorts before the directed packing (`catalog id | MSB`).
fn wire_packings(catalog_label: u16) -> [u16; 2] {
    let label = EdgeLabelId::from_raw(catalog_label);
    [
        label.pack(EdgeDirectedness::Undirected).raw(),
        label.pack(EdgeDirectedness::Directed).raw(),
    ]
}

/// One contiguous key walk plus its exact-label sieve: `Some(wire)` keeps only postings
/// stored under that packing; `None` accepts every label in the segment.
type PageSegment = (Bound<EdgePostingKey>, Bound<EdgePostingKey>, Option<u16>);

impl IndexStore {
    pub(super) fn commit_edge_posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), IndexError> {
        ensure_index_value_key(&value)?;
        self.assert_shard_canister(caller, shard_id)?;
        let key = EdgePostingKey {
            physical_index_id,
            property_id,
            value,
            label_id,
            shard_id,
            owner_vertex_id,
            slot_index,
        };
        INDEX_EDGE_POSTINGS.with_borrow_mut(|postings| {
            postings.insert(key);
        });
        Ok(())
    }

    pub(super) fn commit_edge_posting_remove(
        &self,
        caller: Principal,
        shard_id: ShardId,
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), IndexError> {
        self.assert_shard_canister(caller, shard_id)?;
        let key = EdgePostingKey {
            physical_index_id,
            property_id,
            value,
            label_id,
            shard_id,
            owner_vertex_id,
            slot_index,
        };
        INDEX_EDGE_POSTINGS.with_borrow_mut(|postings| {
            postings.remove(&key);
        });
        Ok(())
    }

    pub fn edge_posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), IndexError> {
        self.commit_edge_posting_insert(
            caller,
            shard_id,
            physical_index_id,
            property_id,
            value,
            label_id,
            owner_vertex_id,
            slot_index,
        )
    }

    pub fn edge_posting_remove(
        &self,
        caller: Principal,
        shard_id: ShardId,
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), IndexError> {
        self.commit_edge_posting_remove(
            caller,
            shard_id,
            physical_index_id,
            property_id,
            value,
            label_id,
            owner_vertex_id,
            slot_index,
        )
    }

    pub fn lookup_edge_equal(
        &self,
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        value: &[u8],
        label_id: Option<u16>,
    ) -> Result<Vec<EdgePostingHit>, IndexError> {
        ensure_index_value_key(value)?;
        let segments = Self::equal_prefix_segments(physical_index_id, property_id, value, label_id);
        Ok(INDEX_EDGE_POSTINGS.with_borrow(|postings| {
            let mut hits = Vec::new();
            for (lo, hi, exact_label) in segments {
                for k in postings.range((lo, hi)) {
                    if let Some(wire) = exact_label
                        && k.label_id != wire
                    {
                        continue;
                    }
                    hits.push(EdgePostingHit {
                        shard_id: k.shard_id,
                        owner_vertex_id: k.owner_vertex_id,
                        label_id: k.label_id,
                        slot_index: k.slot_index,
                    });
                }
            }
            hits
        }))
    }

    /// Bounded equality export for one edge property `(property_id, value[, label_id])` bucket
    /// (no full-bucket heap materialization). Returns at most `limit` hits plus a resume cursor.
    /// `label_id` is a catalog edge label id; the walk covers both of its LARA bucket packings.
    pub fn lookup_edge_equal_page(
        &self,
        req: &LookupEdgeEqualPageRequest,
    ) -> Result<EdgePostingHitPage, IndexError> {
        ensure_index_value_key(&req.value)?;
        let limit = clamp_posting_page_limit(req.limit);
        let segments = Self::equal_prefix_segments(
            req.physical_index_id,
            req.property_id,
            &req.value,
            req.label_id,
        );
        self.page_edge_segments(
            req.physical_index_id,
            req.property_id,
            segments,
            req.after.as_ref(),
            limit,
        )
    }

    /// Prefix key segments covering every stored packing of the requested label space.
    ///
    /// A catalog label maps to both LARA bucket packings (GAP-2026-08-22-001); `None` scans
    /// the whole label dimension once. Segments come back in stored-key order.
    fn equal_prefix_segments(
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        value: &[u8],
        label_id: Option<u16>,
    ) -> Vec<PageSegment> {
        match label_id {
            Some(catalog) => wire_packings(catalog)
                .into_iter()
                .map(|wire| {
                    (
                        Bound::Included(EdgePostingKey::prefix_lower_labeled(
                            physical_index_id,
                            property_id,
                            value,
                            wire,
                        )),
                        Bound::Included(EdgePostingKey::prefix_upper_labeled(
                            physical_index_id,
                            property_id,
                            value,
                            wire,
                        )),
                        Some(wire),
                    )
                })
                .collect(),
            None => vec![(
                Bound::Included(EdgePostingKey::prefix_lower(
                    physical_index_id,
                    property_id,
                    value,
                )),
                Bound::Included(EdgePostingKey::prefix_upper(
                    physical_index_id,
                    property_id,
                    value,
                )),
                None,
            )],
        }
    }

    /// Pages one bounded walk over precomputed key segments, resuming from an exact-key cursor.
    ///
    /// The resume cursor belongs to the segment that contains it: earlier segments are already
    /// drained and later ones start at their own lower bound. A cursor at or beyond a segment's
    /// upper bound skips that segment; a cursor below its lower bound clamps the segment to its
    /// boundary, so an out-of-range cursor can never silently widen the requested interval.
    fn page_edge_segments(
        &self,
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        segments: Vec<PageSegment>,
        after: Option<&EdgePostingCursor>,
        limit: usize,
    ) -> Result<EdgePostingHitPage, IndexError> {
        if let Some(cursor) = after {
            ensure_index_value_key(&cursor.value).map_err(|_| IndexError::IndexValueKeyTooLarge)?;
        }
        let cursor_key = after.map(|cursor| EdgePostingKey {
            physical_index_id,
            property_id,
            value: cursor.value.clone(),
            label_id: cursor.label_id,
            shard_id: cursor.shard_id,
            owner_vertex_id: cursor.owner_vertex_id,
            slot_index: cursor.slot_index,
        });
        let mut hits: Vec<EdgePostingHit> = Vec::with_capacity(limit.min(256));
        let mut next: Option<EdgePostingCursor> = None;
        let mut overflow = false;
        INDEX_EDGE_POSTINGS.with_borrow(|postings| {
            'segments: for (low, high, exact_label) in segments {
                let high_key = match &high {
                    Bound::Included(key) | Bound::Excluded(key) => key.clone(),
                    Bound::Unbounded => unreachable!("segment bounds are keyed"),
                };
                if let Some(key) = &cursor_key
                    && key >= &high_key
                {
                    continue;
                }
                let lower = match &cursor_key {
                    Some(key) => {
                        let low_key = match &low {
                            Bound::Included(k) | Bound::Excluded(k) => k.clone(),
                            Bound::Unbounded => unreachable!("segment bounds are keyed"),
                        };
                        if key < &low_key {
                            low
                        } else {
                            Bound::Excluded(key.clone())
                        }
                    }
                    None => low,
                };
                for key in postings.range((lower, high)) {
                    if let Some(wire) = exact_label
                        && key.label_id != wire
                    {
                        continue;
                    }
                    if hits.len() == limit {
                        overflow = true;
                        break 'segments;
                    }
                    next = Some(EdgePostingCursor {
                        value: key.value.clone(),
                        label_id: key.label_id,
                        shard_id: key.shard_id,
                        owner_vertex_id: key.owner_vertex_id,
                        slot_index: key.slot_index,
                    });
                    hits.push(EdgePostingHit {
                        shard_id: key.shard_id,
                        owner_vertex_id: key.owner_vertex_id,
                        label_id: key.label_id,
                        slot_index: key.slot_index,
                    });
                }
            }
        });
        if overflow {
            Ok(EdgePostingHitPage {
                hits,
                next,
                done: false,
            })
        } else {
            Ok(EdgePostingHitPage {
                hits,
                next: None,
                done: true,
            })
        }
    }

    /// Bounded ordered range export over encoded values for one edge property bucket (no
    /// full-bucket heap materialization). Returns at most `limit` hits plus a resume cursor.
    /// `label_id` is a catalog edge label id; the walk covers both of its LARA bucket packings.
    pub fn lookup_edge_range_page(
        &self,
        req: &LookupEdgeRangePageRequest,
    ) -> Result<EdgePostingHitPage, IndexError> {
        ensure_posting_range_request(&req.range)?;
        let limit = clamp_posting_page_limit(req.limit);
        let segments: Vec<PageSegment> = match req.label_id {
            Some(catalog) => wire_packings(catalog)
                .into_iter()
                .map(|wire| {
                    let (low, high) = edge_posting_key_half_open_range(
                        req.physical_index_id,
                        req.property_id,
                        &req.range,
                        Some(wire),
                    );
                    // A labeled value interval spans foreign labels between its bounds, so
                    // each segment sieves on its exact packing during iteration.
                    (Bound::Included(low), high, Some(wire))
                })
                .collect(),
            None => {
                let (low, high) = edge_posting_key_half_open_range(
                    req.physical_index_id,
                    req.property_id,
                    &req.range,
                    None,
                );
                vec![(Bound::Included(low), high, None)]
            }
        };
        self.page_edge_segments(
            req.physical_index_id,
            req.property_id,
            segments,
            req.after.as_ref(),
            limit,
        )
    }

    /// Batch paginated equality export for many edge `(property_id, value[, label_id])` buckets.
    /// Same bucket-granularity, response-budget, and resume contract as
    /// [`super::property_postings::IndexStore::lookup_equal_batch`].
    pub fn lookup_edge_equal_batch(
        &self,
        req: &LookupEdgeEqualBatchRequest,
    ) -> Result<LookupEdgeEqualBatchResult, IndexError> {
        if req
            .specs
            .iter()
            .any(|spec| !matches!(spec.subject, IndexSubject::EdgeProperty { .. }))
        {
            return Err(IndexError::InvalidBatchSubject);
        }
        let limit = clamp_posting_page_limit(req.limit);
        let base_bytes = Encode!(&LookupEdgeEqualBatchResult {
            pages: Vec::new(),
            next: None,
        })
        .map_err(|e| IndexError::BatchEncodeFailed(e.to_string()))?
        .len();
        let (pages, next) = super::answer_batch_pages(
            req.specs.len(),
            base_bytes,
            super::LOOKUP_BATCH_RESPONSE_BUDGET_BYTES,
            |index| {
                let spec = &req.specs[index];
                let IndexSubject::EdgeProperty { label_id } = spec.subject else {
                    unreachable!("subject validated above")
                };
                self.lookup_edge_equal_page(&LookupEdgeEqualPageRequest {
                    physical_index_id: spec.physical_index_id,
                    property_id: spec.property_id,
                    value: spec.value.clone(),
                    label_id,
                    after: None,
                    limit: limit as u32,
                })
            },
        )?;
        Ok(LookupEdgeEqualBatchResult { pages, next })
    }
}
