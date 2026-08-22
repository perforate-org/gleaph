//! Edge property equality postings (ADR 0009 §1).

use super::{
    IndexStore, clamp_posting_page_limit, ensure_index_value_key, ensure_posting_range_request,
};
use crate::edge_key::EdgePostingKey;
use crate::facade::stable::INDEX_EDGE_POSTINGS;
use crate::posting_range::edge_posting_key_half_open_range;
use crate::state::IndexError;
use candid::{Encode, Principal};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    EdgePostingCursor, EdgePostingHit, EdgePostingHitPage, IndexSubject,
    LookupEdgeEqualBatchRequest, LookupEdgeEqualBatchResult, LookupEdgeEqualPageRequest,
    LookupEdgeRangePageRequest, PhysicalIndexId,
};
use std::ops::Bound;

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
        let (lo, hi) = match label_id {
            Some(label) => (
                EdgePostingKey::prefix_lower_labeled(physical_index_id, property_id, value, label),
                EdgePostingKey::prefix_upper_labeled(physical_index_id, property_id, value, label),
            ),
            None => (
                EdgePostingKey::prefix_lower(physical_index_id, property_id, value),
                EdgePostingKey::prefix_upper(physical_index_id, property_id, value),
            ),
        };
        Ok(INDEX_EDGE_POSTINGS.with_borrow(|postings| {
            postings
                .range(lo..=hi)
                .map(|k| EdgePostingHit {
                    shard_id: k.shard_id,
                    owner_vertex_id: k.owner_vertex_id,
                    label_id: k.label_id,
                    slot_index: k.slot_index,
                })
                .collect()
        }))
    }

    /// Bounded equality export for one edge property `(property_id, value[, label_id])` bucket (no
    /// full-bucket heap materialization). Returns at most `limit` hits plus a resume cursor.
    pub fn lookup_edge_equal_page(
        &self,
        req: &LookupEdgeEqualPageRequest,
    ) -> Result<EdgePostingHitPage, IndexError> {
        ensure_index_value_key(&req.value)?;
        let limit = clamp_posting_page_limit(req.limit);
        let (lo, hi) = match req.label_id {
            Some(label) => (
                EdgePostingKey::prefix_lower_labeled(
                    req.physical_index_id,
                    req.property_id,
                    &req.value,
                    label,
                ),
                EdgePostingKey::prefix_upper_labeled(
                    req.physical_index_id,
                    req.property_id,
                    &req.value,
                    label,
                ),
            ),
            None => (
                EdgePostingKey::prefix_lower(req.physical_index_id, req.property_id, &req.value),
                EdgePostingKey::prefix_upper(req.physical_index_id, req.property_id, &req.value),
            ),
        };
        let upper = Bound::Included(hi);
        let lower = match &req.after {
            Some(cursor) => Bound::Excluded(EdgePostingKey {
                physical_index_id: req.physical_index_id,
                property_id: req.property_id,
                value: cursor.value.clone(),
                label_id: cursor.label_id,
                shard_id: cursor.shard_id,
                owner_vertex_id: cursor.owner_vertex_id,
                slot_index: cursor.slot_index,
            }),
            None => Bound::Included(lo),
        };

        let mut hits = Vec::with_capacity(limit.min(256));
        let mut next: Option<EdgePostingCursor> = None;
        let mut overflow = false;
        INDEX_EDGE_POSTINGS.with_borrow(|postings| {
            for key in postings.range((lower, upper)).take(limit + 1) {
                if hits.len() == limit {
                    overflow = true;
                    break;
                }
                hits.push(EdgePostingHit {
                    shard_id: key.shard_id,
                    owner_vertex_id: key.owner_vertex_id,
                    label_id: key.label_id,
                    slot_index: key.slot_index,
                });
                next = Some(EdgePostingCursor {
                    value: key.value,
                    label_id: key.label_id,
                    shard_id: key.shard_id,
                    owner_vertex_id: key.owner_vertex_id,
                    slot_index: key.slot_index,
                });
            }
        });
        Ok(if overflow {
            EdgePostingHitPage {
                hits,
                next,
                done: false,
            }
        } else {
            EdgePostingHitPage {
                hits,
                next: None,
                done: true,
            }
        })
    }

    /// Bounded ordered range export over encoded values for one edge property bucket (no
    /// full-bucket heap materialization). Returns at most `limit` hits plus a resume cursor.
    /// Postings whose label differs from `label_id`, when set, are sieved during iteration.
    pub fn lookup_edge_range_page(
        &self,
        req: &LookupEdgeRangePageRequest,
    ) -> Result<EdgePostingHitPage, IndexError> {
        ensure_posting_range_request(&req.range)?;
        let limit = clamp_posting_page_limit(req.limit);
        let (low, high) = edge_posting_key_half_open_range(
            req.physical_index_id,
            req.property_id,
            &req.range,
            req.label_id,
        );
        if let Bound::Excluded(high_key) = &high
            && low >= *high_key
        {
            return Ok(EdgePostingHitPage {
                hits: Vec::new(),
                next: None,
                done: true,
            });
        }
        let lower = match &req.after {
            Some(cursor) => {
                ensure_index_value_key(&cursor.value)
                    .map_err(|_| IndexError::IndexValueKeyTooLarge)?;
                let cursor_key = EdgePostingKey {
                    physical_index_id: req.physical_index_id,
                    property_id: req.property_id,
                    value: cursor.value.clone(),
                    label_id: cursor.label_id,
                    shard_id: cursor.shard_id,
                    owner_vertex_id: cursor.owner_vertex_id,
                    slot_index: cursor.slot_index,
                };
                // A cursor outside the requested range would silently change the interval. Clamp it
                // to the interval boundary; if it is already at or beyond `high` the page is empty.
                if let Bound::Excluded(high_key) = &high
                    && cursor_key >= *high_key
                {
                    return Ok(EdgePostingHitPage {
                        hits: Vec::new(),
                        next: None,
                        done: true,
                    });
                }
                if cursor_key < low {
                    Bound::Included(low)
                } else {
                    Bound::Excluded(cursor_key)
                }
            }
            None => Bound::Included(low),
        };

        let mut hits = Vec::with_capacity(limit.min(256));
        let mut next: Option<EdgePostingCursor> = None;
        let mut overflow = false;
        INDEX_EDGE_POSTINGS.with_borrow(|postings| {
            for key in postings.range((lower, high.clone())) {
                if let Some(label) = req.label_id
                    && key.label_id != label
                {
                    continue;
                }
                if hits.len() == limit {
                    overflow = true;
                    break;
                }
                hits.push(EdgePostingHit {
                    shard_id: key.shard_id,
                    owner_vertex_id: key.owner_vertex_id,
                    label_id: key.label_id,
                    slot_index: key.slot_index,
                });
                next = Some(EdgePostingCursor {
                    value: key.value.clone(),
                    label_id: key.label_id,
                    shard_id: key.shard_id,
                    owner_vertex_id: key.owner_vertex_id,
                    slot_index: key.slot_index,
                });
            }
        });
        Ok(if overflow {
            EdgePostingHitPage {
                hits,
                next,
                done: false,
            }
        } else {
            EdgePostingHitPage {
                hits,
                next: None,
                done: true,
            }
        })
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
