//! Vertex label membership postings: shard-canister writes and posting-local reads.

use super::{DEFAULT_LABEL_LOOKUP_PAGE_LIMIT, IndexStore, pack_posting_vertex};
use crate::facade::stable::INDEX_VERTEX_LABEL_POSTINGS;
use crate::label_key::LabelPostingKey;
use crate::label_range::label_shard_posting_bucket;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    LabelIntersectionPageRequest, LabelLookupPageRequest, LabelLookupPageResult,
    LabelPostingCursor, PostingHit,
};
use nohash_hasher::IntSet;

impl IndexStore {
    pub(super) fn commit_label_posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        vertex_label_id: u32,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        self.assert_shard_canister(caller, shard_id)?;
        let key = LabelPostingKey {
            vertex_label_id,
            shard_id,
            vertex_id,
        };
        INDEX_VERTEX_LABEL_POSTINGS.with_borrow_mut(|postings| {
            postings.insert(key);
        });
        Ok(())
    }

    pub(super) fn commit_label_posting_remove(
        &self,
        caller: Principal,
        shard_id: ShardId,
        vertex_label_id: u32,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        self.assert_shard_canister(caller, shard_id)?;
        let key = LabelPostingKey {
            vertex_label_id,
            shard_id,
            vertex_id,
        };
        INDEX_VERTEX_LABEL_POSTINGS.with_borrow_mut(|postings| {
            postings.remove(&key);
        });
        Ok(())
    }

    pub fn label_posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        vertex_label_id: u32,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        self.commit_label_posting_insert(caller, shard_id, vertex_label_id, vertex_id)
    }

    pub fn label_posting_remove(
        &self,
        caller: Principal,
        shard_id: ShardId,
        vertex_label_id: u32,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        self.commit_label_posting_remove(caller, shard_id, vertex_label_id, vertex_id)
    }

    pub fn lookup_label(&self, vertex_label_id: u32) -> Vec<PostingHit> {
        let lo = LabelPostingKey::prefix_lower(vertex_label_id);
        let hi = LabelPostingKey::prefix_upper(vertex_label_id);
        INDEX_VERTEX_LABEL_POSTINGS.with_borrow(|postings| {
            postings
                .range(lo..=hi)
                .map(|k| PostingHit {
                    shard_id: k.shard_id,
                    vertex_id: k.vertex_id,
                })
                .collect()
        })
    }

    /// All label postings for one `(vertex_label_id, shard_id)` prefix.
    pub fn lookup_label_for_shard(
        &self,
        vertex_label_id: u32,
        shard_id: ShardId,
    ) -> Vec<PostingHit> {
        let Some((low, high)) = label_shard_posting_bucket(vertex_label_id, shard_id) else {
            return Vec::new();
        };
        INDEX_VERTEX_LABEL_POSTINGS.with_borrow(|postings| {
            postings
                .range(low..high)
                .map(|k| PostingHit {
                    shard_id: k.shard_id,
                    vertex_id: k.vertex_id,
                })
                .collect()
        })
    }

    /// Paginated label export for one shard-local prefix.
    pub fn lookup_label_page(&self, req: &LabelLookupPageRequest) -> LabelLookupPageResult {
        let limit = usize::try_from(req.limit)
            .unwrap_or(DEFAULT_LABEL_LOOKUP_PAGE_LIMIT)
            .clamp(1, DEFAULT_LABEL_LOOKUP_PAGE_LIMIT);
        let Some((mut low, high)) = label_shard_posting_bucket(req.vertex_label_id, req.shard_id)
        else {
            return LabelLookupPageResult {
                hits: Vec::new(),
                next: None,
                done: true,
            };
        };
        if let Some(after) = req.after {
            let cursor_key = LabelPostingKey {
                vertex_label_id: req.vertex_label_id,
                shard_id: after.shard_id,
                vertex_id: after.vertex_id,
            };
            low = match cursor_key.successor() {
                Some(next) if next < high => next,
                _ => {
                    return LabelLookupPageResult {
                        hits: Vec::new(),
                        next: None,
                        done: true,
                    };
                }
            };
        }
        if low >= high {
            return LabelLookupPageResult {
                hits: Vec::new(),
                next: None,
                done: true,
            };
        }

        let mut hits = Vec::with_capacity(limit.min(256));
        INDEX_VERTEX_LABEL_POSTINGS.with_borrow(|postings| {
            for key in postings.range(low..high).take(limit + 1) {
                hits.push(PostingHit {
                    shard_id: key.shard_id,
                    vertex_id: key.vertex_id,
                });
            }
        });

        let done = hits.len() <= limit;
        if hits.len() > limit {
            hits.truncate(limit);
        }
        let next = hits.last().map(|hit| LabelPostingCursor {
            shard_id: hit.shard_id,
            vertex_id: hit.vertex_id,
        });
        LabelLookupPageResult { hits, next, done }
    }

    /// Paginated label intersection with all sieve memberships checked in this canister.
    pub fn lookup_label_intersection_page(
        &self,
        req: &LabelIntersectionPageRequest,
    ) -> LabelLookupPageResult {
        let mut page = self.lookup_label_page(&LabelLookupPageRequest {
            vertex_label_id: req.walk_label_id,
            shard_id: req.shard_id,
            after: req.after,
            limit: req.limit,
        });
        for &label_id in &req.sieve_label_ids {
            page.hits = self.filter_hits_by_label(label_id, &page.hits);
            if page.hits.is_empty() {
                break;
            }
        }
        page
    }

    /// Intersect label membership across at least two `vertex_label_id` buckets.
    pub fn lookup_label_intersection(&self, vertex_label_ids: &[u32]) -> Vec<PostingHit> {
        if vertex_label_ids.len() < 2 {
            return Vec::new();
        }
        let mut sets: Vec<IntSet<u64>> = Vec::with_capacity(vertex_label_ids.len());
        for &label_id in vertex_label_ids {
            let lo = LabelPostingKey::prefix_lower(label_id);
            let hi = LabelPostingKey::prefix_upper(label_id);
            let mut set = IntSet::default();
            INDEX_VERTEX_LABEL_POSTINGS.with_borrow(|postings| {
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
                shard_id: ShardId::new((packed >> 32) as u32),
                vertex_id: (packed & 0xFFFF_FFFF) as u32,
            })
            .collect()
    }

    /// Keep only hits whose `(shard_id, vertex_id)` has a label posting for `vertex_label_id`.
    pub fn filter_hits_by_label(
        &self,
        vertex_label_id: u32,
        hits: &[PostingHit],
    ) -> Vec<PostingHit> {
        INDEX_VERTEX_LABEL_POSTINGS.with_borrow(|labels| {
            hits.iter()
                .copied()
                .filter(|hit| {
                    labels.contains(&LabelPostingKey {
                        vertex_label_id,
                        shard_id: hit.shard_id,
                        vertex_id: hit.vertex_id,
                    })
                })
                .collect()
        })
    }
}
