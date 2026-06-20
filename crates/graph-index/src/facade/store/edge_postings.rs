//! Edge property equality postings (ADR 0009 §1).

use super::{IndexStore, clamp_posting_page_limit, ensure_index_value_key};
use crate::edge_key::EdgePostingKey;
use crate::facade::stable::INDEX_EDGE_POSTINGS;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    EdgePostingCursor, EdgePostingHit, EdgePostingHitPage, LookupEdgeEqualPageRequest,
};
use std::ops::Bound;

impl IndexStore {
    pub(super) fn commit_edge_posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), IndexError> {
        ensure_index_value_key(&value)?;
        self.assert_shard_canister(caller, shard_id)?;
        let key = EdgePostingKey {
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
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), IndexError> {
        self.assert_shard_canister(caller, shard_id)?;
        let key = EdgePostingKey {
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
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), IndexError> {
        self.commit_edge_posting_insert(
            caller,
            shard_id,
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
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), IndexError> {
        self.commit_edge_posting_remove(
            caller,
            shard_id,
            property_id,
            value,
            label_id,
            owner_vertex_id,
            slot_index,
        )
    }

    pub fn lookup_edge_equal(
        &self,
        property_id: u32,
        value: &[u8],
        label_id: Option<u16>,
    ) -> Result<Vec<EdgePostingHit>, IndexError> {
        ensure_index_value_key(value)?;
        let (lo, hi) = match label_id {
            Some(label) => (
                EdgePostingKey::prefix_lower_labeled(property_id, value, label),
                EdgePostingKey::prefix_upper_labeled(property_id, value, label),
            ),
            None => (
                EdgePostingKey::prefix_lower(property_id, value),
                EdgePostingKey::prefix_upper(property_id, value),
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
                EdgePostingKey::prefix_lower_labeled(req.property_id, &req.value, label),
                EdgePostingKey::prefix_upper_labeled(req.property_id, &req.value, label),
            ),
            None => (
                EdgePostingKey::prefix_lower(req.property_id, &req.value),
                EdgePostingKey::prefix_upper(req.property_id, &req.value),
            ),
        };
        let upper = Bound::Included(hi);
        let lower = match &req.after {
            Some(cursor) => Bound::Excluded(EdgePostingKey {
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
}
