//! Property equality postings: shard-canister writes and posting-local reads.

use super::{
    IndexStore, clamp_posting_page_limit, ensure_index_value_key, ensure_posting_range_request,
    pack_posting_vertex,
};
use crate::facade::stable::{INDEX_VERTEX_LABEL_POSTINGS, INDEX_VERTEX_POSTINGS};
use crate::key::PostingKey;
use crate::label_key::LabelPostingKey;
use crate::posting_range::{posting_key_half_open_range, property_posting_bucket};
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    IndexSubject, LookupEqualPageRequest, LookupIntersectionPageRequest, LookupRangePageRequest,
    PostingHit, PostingHitPage, PostingRangeRequest, PropertyPostingCursor, ValuePostingCount,
};
use nohash_hasher::IntSet;
use std::ops::Bound;

impl IndexStore {
    pub(super) fn commit_posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        ensure_index_value_key(&value)?;
        self.assert_shard_canister(caller, shard_id)?;
        let key = PostingKey {
            property_id,
            value,
            shard_id,
            vertex_id,
        };
        INDEX_VERTEX_POSTINGS.with_borrow_mut(|postings| {
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
        self.assert_shard_canister(caller, shard_id)?;
        let key = PostingKey {
            property_id,
            value,
            shard_id,
            vertex_id,
        };
        INDEX_VERTEX_POSTINGS.with_borrow_mut(|postings| {
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

    pub fn lookup_equal(
        &self,
        property_id: u32,
        value: &[u8],
    ) -> Result<Vec<PostingHit>, IndexError> {
        ensure_index_value_key(value)?;
        let lo = PostingKey::prefix_lower(property_id, value);
        let hi = PostingKey::prefix_upper(property_id, value);
        Ok(INDEX_VERTEX_POSTINGS.with_borrow(|postings| {
            postings
                .range(lo..=hi)
                .map(|k| PostingHit {
                    shard_id: k.shard_id,
                    vertex_id: k.vertex_id,
                })
                .collect()
        }))
    }

    /// Bounded equality export for one `(property_id, value)` bucket (no full-bucket heap
    /// materialization). Returns at most `limit` hits plus a resume cursor.
    pub fn lookup_equal_page(
        &self,
        req: &LookupEqualPageRequest,
    ) -> Result<PostingHitPage, IndexError> {
        ensure_index_value_key(&req.value)?;
        let limit = clamp_posting_page_limit(req.limit);
        let upper = Bound::Included(PostingKey::prefix_upper(req.property_id, &req.value));
        let lower = match &req.after {
            Some(cursor) => Bound::Excluded(PostingKey {
                property_id: req.property_id,
                value: cursor.value.clone(),
                shard_id: cursor.shard_id,
                vertex_id: cursor.vertex_id,
            }),
            None => Bound::Included(PostingKey::prefix_lower(req.property_id, &req.value)),
        };
        Ok(collect_vertex_posting_page(lower, upper, limit))
    }

    /// Paginated all-vertex equality intersection: walk `specs[0]` one page at a time and sieve each
    /// page against the remaining arms in-heap. Server-side composition of [`Self::lookup_equal_page`]
    /// and [`Self::filter_hits_by_equal`], so one inter-canister call returns a bounded page of
    /// survivors plus the walk-arm cursor — no per-arm set is ever materialized, and the walk + sieve
    /// fold into a single message per page instead of one message per arm per page.
    ///
    /// Returns an empty terminal page when given fewer than two specs or any non-vertex spec; the
    /// router only routes all-vertex requests here and falls back to the materializing
    /// [`Self::lookup_intersection`] otherwise.
    pub fn lookup_intersection_page(
        &self,
        req: &LookupIntersectionPageRequest,
    ) -> Result<PostingHitPage, IndexError> {
        if req.specs.len() < 2
            || !req
                .specs
                .iter()
                .all(|spec| matches!(spec.subject, IndexSubject::VertexProperty))
        {
            return Ok(empty_posting_page());
        }
        let walk = &req.specs[0];
        let walk_page = self.lookup_equal_page(&LookupEqualPageRequest {
            property_id: walk.property_id,
            value: walk.value.clone(),
            after: req.after.clone(),
            limit: req.limit,
        })?;
        let mut survivors = walk_page.hits;
        for arm in &req.specs[1..] {
            if survivors.is_empty() {
                break;
            }
            survivors = self.filter_hits_by_equal(arm.property_id, &arm.value, survivors)?;
        }
        Ok(PostingHitPage {
            hits: survivors,
            next: walk_page.next,
            done: walk_page.done,
        })
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
        vertex_filter: Option<&IntSet<u64>>,
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

        INDEX_VERTEX_POSTINGS.with_borrow(|postings| {
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

        INDEX_VERTEX_POSTINGS.with_borrow(|postings| {
            INDEX_VERTEX_LABEL_POSTINGS.with_borrow(|labels| {
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

    /// Keep only hits whose `(shard_id, vertex_id)` has a posting for `(property_id, value)`.
    ///
    /// Equality-arm sieve for streaming property intersection: the caller walks one arm in pages
    /// ([`Self::lookup_equal_page`]) and sieves each page against the other arms here, so the index
    /// never materializes a full posting bucket for any arm.
    ///
    /// Both the input `hits` and the `(property_id, value)` bucket are ordered by
    /// `(shard_id, vertex_id)`, so this runs a single bounded sorted merge over the bucket range
    /// `[min(hits), max(hits)]` instead of one stable point lookup per hit — turning random tree
    /// descents into a sequential scan. `hits` need not be pre-sorted (sorted in place); the
    /// returned survivors are in `(shard_id, vertex_id)` order. The result is at most `len(hits)`, so
    /// heap stays bounded by the page.
    pub fn filter_hits_by_equal(
        &self,
        property_id: u32,
        value: &[u8],
        mut hits: Vec<PostingHit>,
    ) -> Result<Vec<PostingHit>, IndexError> {
        ensure_index_value_key(value)?;
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        hits.sort_unstable_by_key(|hit| (hit.shard_id, hit.vertex_id));

        let first = hits[0];
        let last = hits[hits.len() - 1];
        let lower = Bound::Included(PostingKey {
            property_id,
            value: value.to_vec(),
            shard_id: first.shard_id,
            vertex_id: first.vertex_id,
        });
        let upper = Bound::Included(PostingKey {
            property_id,
            value: value.to_vec(),
            shard_id: last.shard_id,
            vertex_id: last.vertex_id,
        });

        let mut out = Vec::new();
        INDEX_VERTEX_POSTINGS.with_borrow(|postings| {
            let mut bucket = postings.range((lower, upper));
            let mut current = bucket.next();
            for hit in hits {
                let hit_key = (hit.shard_id, hit.vertex_id);
                while let Some(key) = &current {
                    let bucket_key = (key.shard_id, key.vertex_id);
                    if bucket_key < hit_key {
                        current = bucket.next();
                    } else if bucket_key == hit_key {
                        out.push(hit);
                        current = bucket.next();
                        break;
                    } else {
                        break;
                    }
                }
            }
        });
        Ok(out)
    }

    /// Half-open `[low, high)` scan over postings for `property_id` using encoded-value [`PostingRangeRequest`].
    pub fn lookup_range(
        &self,
        property_id: u32,
        req: &PostingRangeRequest,
    ) -> Result<Vec<PostingHit>, IndexError> {
        ensure_posting_range_request(req)?;
        let Some((low, high)) = posting_key_half_open_range(property_id, req) else {
            return Ok(Vec::new());
        };
        if low >= high {
            return Ok(Vec::new());
        }
        Ok(INDEX_VERTEX_POSTINGS.with_borrow(|postings| {
            postings
                .range(low..high)
                .map(|k| PostingHit {
                    shard_id: k.shard_id,
                    vertex_id: k.vertex_id,
                })
                .collect()
        }))
    }

    /// Bounded range export over encoded values for one property (no full-bucket heap
    /// materialization). Returns at most `limit` hits plus a resume cursor.
    pub fn lookup_range_page(
        &self,
        req: &LookupRangePageRequest,
    ) -> Result<PostingHitPage, IndexError> {
        ensure_posting_range_request(&req.range)?;
        let limit = clamp_posting_page_limit(req.limit);
        let Some((low, high)) = posting_key_half_open_range(req.property_id, &req.range) else {
            return Ok(empty_posting_page());
        };
        if low >= high {
            return Ok(empty_posting_page());
        }
        let upper = Bound::Excluded(high.clone());
        let lower = match &req.after {
            Some(cursor) => {
                ensure_index_value_key(&cursor.value)
                    .map_err(|_| IndexError::IndexValueKeyTooLarge)?;
                let cursor_key = PostingKey {
                    property_id: req.property_id,
                    value: cursor.value.clone(),
                    shard_id: cursor.shard_id,
                    vertex_id: cursor.vertex_id,
                };
                // A cursor outside the requested range would silently change the interval. Clamp it
                // to the interval boundary; if it is already at or beyond `high` the page is empty.
                if cursor_key >= high {
                    return Ok(empty_posting_page());
                }
                if cursor_key < low {
                    Bound::Included(low)
                } else {
                    Bound::Excluded(cursor_key)
                }
            }
            None => Bound::Included(low),
        };
        Ok(collect_vertex_posting_page(lower, upper, limit))
    }
}

fn empty_posting_page() -> PostingHitPage {
    PostingHitPage {
        hits: Vec::new(),
        next: None,
        done: true,
    }
}

/// Collect at most `limit` vertex posting hits over `(lower, upper)`, reading one extra key to
/// detect a further page. The resume cursor is the last retained hit's full key.
fn collect_vertex_posting_page(
    lower: Bound<PostingKey>,
    upper: Bound<PostingKey>,
    limit: usize,
) -> PostingHitPage {
    let mut hits = Vec::with_capacity(limit.min(256));
    let mut next: Option<PropertyPostingCursor> = None;
    let mut overflow = false;
    INDEX_VERTEX_POSTINGS.with_borrow(|postings| {
        for key in postings.range((lower, upper)).take(limit + 1) {
            if hits.len() == limit {
                overflow = true;
                break;
            }
            let shard_id = key.shard_id;
            let vertex_id = key.vertex_id;
            hits.push(PostingHit {
                shard_id,
                vertex_id,
            });
            next = Some(PropertyPostingCursor {
                value: key.value,
                shard_id,
                vertex_id,
            });
        }
    });
    if overflow {
        PostingHitPage {
            hits,
            next,
            done: false,
        }
    } else {
        PostingHitPage {
            hits,
            next: None,
            done: true,
        }
    }
}
