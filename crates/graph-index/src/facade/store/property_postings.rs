//! Property equality postings: shard-owner writes and posting-local reads.

use super::{IndexStore, pack_posting_vertex};
use crate::facade::stable::{INDEX_LABEL_POSTINGS, INDEX_POSTINGS};
use crate::key::PostingKey;
use crate::label_key::LabelPostingKey;
use crate::posting_range::{posting_key_half_open_range, property_posting_bucket};
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    IndexIntersectionRequest, PostingHit, PostingRangeRequest, ValuePostingCount,
};

impl IndexStore {
    pub(super) fn commit_posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        self.assert_shard_owner(caller, shard_id)?;
        let key = PostingKey {
            property_id,
            value,
            shard_id,
            vertex_id,
        };
        INDEX_POSTINGS.with_borrow_mut(|postings| {
            postings.insert(key);
        });
        Ok(())
    }

    pub(super) fn commit_posting_remove(
        &self,
        caller: Principal,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        self.assert_shard_owner(caller, shard_id)?;
        let key = PostingKey {
            property_id,
            value,
            shard_id,
            vertex_id,
        };
        INDEX_POSTINGS.with_borrow_mut(|postings| {
            postings.remove(&key);
        });
        Ok(())
    }

    pub fn posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        self.commit_posting_insert(caller, shard_id, property_id, value, vertex_id)
    }

    pub fn posting_remove(
        &self,
        caller: Principal,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        self.commit_posting_remove(caller, shard_id, property_id, value, vertex_id)
    }

    pub fn lookup_equal(&self, property_id: u32, value: &[u8]) -> Vec<PostingHit> {
        let lo = PostingKey::prefix_lower(property_id, value);
        let hi = PostingKey::prefix_upper(property_id, value);
        INDEX_POSTINGS.with_borrow(|postings| {
            postings
                .range(lo..=hi)
                .map(|k| PostingHit {
                    shard_id: k.shard_id,
                    vertex_id: k.vertex_id,
                })
                .collect()
        })
    }

    pub fn lookup_intersection(&self, req: &IndexIntersectionRequest) -> Vec<PostingHit> {
        if req.specs.len() < 2 {
            return Vec::new();
        }
        let mut sets: Vec<std::collections::HashSet<u64>> = Vec::with_capacity(req.specs.len());
        for spec in &req.specs {
            let lo = PostingKey::prefix_lower(spec.property_id, &spec.value);
            let hi = PostingKey::prefix_upper(spec.property_id, &spec.value);
            let mut set = std::collections::HashSet::new();
            INDEX_POSTINGS.with_borrow(|postings| {
                for key in postings.range(lo..=hi) {
                    let packed = pack_posting_vertex(key.shard_id, key.vertex_id);
                    set.insert(packed);
                }
            });
            sets.push(set);
        }
        sets.sort_by_key(|set| set.len());
        let mut intersection = sets[0].clone();
        for set in sets.iter().skip(1) {
            intersection = intersection.intersection(set).copied().collect();
            if intersection.is_empty() {
                return Vec::new();
            }
        }
        intersection
            .into_iter()
            .map(|packed| PostingHit {
                shard_id: (packed >> 32) as u32,
                vertex_id: (packed & 0xFFFF_FFFF) as u32,
            })
            .collect()
    }

    /// Walk the property bucket and return `(encoded_value, global_count)` groups with `count >= min_count`.
    ///
    /// When `vertex_filter` is set, only postings whose `(shard_id, vertex_id)` appear in the set
    /// are counted (packed as `(shard_id as u64) << 32 | vertex_id as u64`).
    pub fn count_postings_by_value(
        &self,
        property_id: u32,
        min_count: u64,
        max_groups: usize,
        vertex_filter: Option<&std::collections::HashSet<u64>>,
    ) -> Vec<ValuePostingCount> {
        let Some((low, high)) = property_posting_bucket(property_id) else {
            return Vec::new();
        };
        let max_groups = max_groups.max(1);
        let mut out = Vec::new();
        let mut current_value: Option<Vec<u8>> = None;
        let mut current_count: u64 = 0;

        let flush = |value: Vec<u8>, count: u64, out: &mut Vec<ValuePostingCount>| {
            if count >= min_count {
                out.push(ValuePostingCount {
                    encoded_value: value,
                    count,
                });
            }
        };

        INDEX_POSTINGS.with_borrow(|postings| {
            for key in postings.range(low..high) {
                if let Some(filter) = vertex_filter {
                    let packed = pack_posting_vertex(key.shard_id, key.vertex_id);
                    if !filter.contains(&packed) {
                        continue;
                    }
                }
                match current_value.as_ref() {
                    None => {
                        current_value = Some(key.value.clone());
                        current_count = 1;
                    }
                    Some(value) if value == &key.value => {
                        current_count = current_count.saturating_add(1);
                    }
                    Some(value) => {
                        flush(value.clone(), current_count, &mut out);
                        if out.len() >= max_groups {
                            return;
                        }
                        current_value = Some(key.value.clone());
                        current_count = 1;
                    }
                }
            }
        });

        if let Some(value) = current_value
            && out.len() < max_groups
        {
            flush(value, current_count, &mut out);
        }
        out
    }

    /// Walk one property bucket and count groups whose postings belong to `vertex_label_id`.
    pub fn count_postings_by_value_for_label(
        &self,
        property_id: u32,
        vertex_label_id: u32,
        min_count: u64,
        max_groups: usize,
    ) -> Vec<ValuePostingCount> {
        let Some((low, high)) = property_posting_bucket(property_id) else {
            return Vec::new();
        };
        let max_groups = max_groups.max(1);
        let mut out = Vec::new();
        let mut current_value: Option<Vec<u8>> = None;
        let mut current_count: u64 = 0;

        let flush = |value: Vec<u8>, count: u64, out: &mut Vec<ValuePostingCount>| {
            if count >= min_count {
                out.push(ValuePostingCount {
                    encoded_value: value,
                    count,
                });
            }
        };

        INDEX_POSTINGS.with_borrow(|postings| {
            INDEX_LABEL_POSTINGS.with_borrow(|labels| {
                for key in postings.range(low..high) {
                    if !labels.contains(&LabelPostingKey {
                        vertex_label_id,
                        shard_id: key.shard_id,
                        vertex_id: key.vertex_id,
                    }) {
                        continue;
                    }
                    match current_value.as_ref() {
                        None => {
                            current_value = Some(key.value.clone());
                            current_count = 1;
                        }
                        Some(value) if value == &key.value => {
                            current_count = current_count.saturating_add(1);
                        }
                        Some(value) => {
                            flush(value.clone(), current_count, &mut out);
                            if out.len() >= max_groups {
                                return;
                            }
                            current_value = Some(key.value.clone());
                            current_count = 1;
                        }
                    }
                }
            });
        });

        if let Some(value) = current_value
            && out.len() < max_groups
        {
            flush(value, current_count, &mut out);
        }
        out
    }

    /// Half-open `[low, high)` scan over postings for `property_id` using encoded-value [`PostingRangeRequest`].
    pub fn lookup_range(&self, property_id: u32, req: &PostingRangeRequest) -> Vec<PostingHit> {
        let Some((low, high)) = posting_key_half_open_range(property_id, req) else {
            return Vec::new();
        };
        if low >= high {
            return Vec::new();
        }
        INDEX_POSTINGS.with_borrow(|postings| {
            postings
                .range(low..high)
                .map(|k| PostingHit {
                    shard_id: k.shard_id,
                    vertex_id: k.vertex_id,
                })
                .collect()
        })
    }
}
