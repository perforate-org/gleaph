//! Edge property equality postings (ADR 0009 §1).

use super::IndexStore;
use crate::edge_key::EdgePostingKey;
use crate::facade::stable::INDEX_EDGE_POSTINGS;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::EdgePostingHit;

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
        self.assert_shard_owner(caller, shard_id)?;
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
        self.assert_shard_owner(caller, shard_id)?;
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
    ) -> Vec<EdgePostingHit> {
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
        INDEX_EDGE_POSTINGS.with_borrow(|postings| {
            postings
                .range(lo..=hi)
                .map(|k| EdgePostingHit {
                    shard_id: k.shard_id,
                    owner_vertex_id: k.owner_vertex_id,
                    label_id: k.label_id,
                    slot_index: k.slot_index,
                })
                .collect()
        })
    }
}
