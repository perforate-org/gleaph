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
    IndexSubject, LookupEqualPageForLabelRequest, LookupEqualPageRequest,
    LookupIntersectionPageRequest, LookupRangeIntersectionPageRequest,
    LookupRangePageForLabelRequest, LookupRangePageRequest, MAX_EQUALITY_INTERSECTION_ARMS,
    PostingHit, PostingHitPage, PostingRangeRequest, PropertyPostingCursor, ValuePostingCount,
};
use nohash_hasher::IntSet;
use std::ops::Bound;

/// Decide whether the equality sieve should use a dense range scan or point lookups.
///
/// Preconditions: `hits` is sorted by `(shard_id, vertex_id)` and is non-empty.
/// A dense scan is used when all hits are on the same shard and the `(shard_id, vertex_id)` span
/// is at most four times the page length. Otherwise point lookups keep work bounded by the page
/// size.
pub(crate) fn equal_sieve_dense_threshold_met(hits: &[PostingHit]) -> bool {
    debug_assert!(!hits.is_empty());
    let first = &hits[0];
    let last = &hits[hits.len() - 1];
    let span = last
        .vertex_id
        .saturating_sub(first.vertex_id)
        .saturating_add(1) as usize;
    first.shard_id == last.shard_id && span <= hits.len().saturating_mul(4)
}

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

    /// Paginated equality export with label membership sieved inside the index canister.
    /// The cursor advances over scanned property postings, including non-matching rows.
    pub fn lookup_equal_page_for_label(
        &self,
        req: &LookupEqualPageForLabelRequest,
    ) -> Result<PostingHitPage, IndexError> {
        let mut page = self.lookup_equal_page(&LookupEqualPageRequest {
            property_id: req.property_id,
            value: req.value.clone(),
            after: req.after.clone(),
            limit: req.limit,
        })?;
        page.hits = self.filter_hits_by_label(req.vertex_label_id, &page.hits);
        Ok(page)
    }

    /// Paginated range export with label membership sieved inside the index canister.
    pub fn lookup_range_page_for_label(
        &self,
        req: &LookupRangePageForLabelRequest,
    ) -> Result<PostingHitPage, IndexError> {
        let mut page = self.lookup_range_page(&LookupRangePageRequest {
            property_id: req.property_id,
            range: req.range.clone(),
            after: req.after.clone(),
            limit: req.limit,
        })?;
        page.hits = self.filter_hits_by_label(req.vertex_label_id, &page.hits);
        Ok(page)
    }

    /// Paginated all-vertex equality intersection: walk `specs[0]` one page at a time and sieve each
    /// page against the remaining arms in-heap. Server-side composition of [`Self::lookup_equal_page`]
    /// and [`Self::filter_hits_by_equal`], so one inter-canister call returns a bounded page of
    /// survivors plus the walk-arm cursor — no per-arm set is ever materialized, and the walk + sieve
    /// fold into a single message per page instead of one message per arm per page.
    ///
    /// Execution contract:
    /// - Requires between 2 and [`MAX_EQUALITY_INTERSECTION_ARMS`] inclusive specs, all targeting
    ///   [`IndexSubject::VertexProperty`]. Requests with fewer than two specs return an empty terminal
    ///   page for backward compatibility with callers that branch on arm count. Requests with more than
    ///   [`MAX_EQUALITY_INTERSECTION_ARMS`] specs return [`IndexError::TooManyEqualityIntersectionArms`].
    ///   Requests containing a non-vertex spec return [`IndexError::InvalidIntersectionSubject`].
    /// - The walk arm is selected deterministically by canonical `(property_id, value)` ordering, so
    ///   repeated calls with the same spec set use the same walk plan and resume cursor.
    pub fn lookup_intersection_page(
        &self,
        req: &LookupIntersectionPageRequest,
    ) -> Result<PostingHitPage, IndexError> {
        if req.specs.len() < 2 {
            return Ok(empty_posting_page());
        }
        if req.specs.len() > MAX_EQUALITY_INTERSECTION_ARMS {
            return Err(IndexError::TooManyEqualityIntersectionArms);
        }
        if !req
            .specs
            .iter()
            .all(|spec| matches!(spec.subject, IndexSubject::VertexProperty))
        {
            return Err(IndexError::InvalidIntersectionSubject);
        }
        let mut sorted_specs = req.specs.clone();
        sorted_specs.sort_by(|a, b| {
            a.property_id
                .cmp(&b.property_id)
                .then_with(|| a.value.cmp(&b.value))
        });
        let walk = &sorted_specs[0];
        let sieves = &sorted_specs[1..];
        let walk_page = self.lookup_equal_page(&LookupEqualPageRequest {
            property_id: walk.property_id,
            value: walk.value.clone(),
            after: req.after.clone(),
            limit: req.limit,
        })?;
        let mut survivors = walk_page.hits;
        for arm in sieves {
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

    /// Point-lookup equality sieve bounded by the number of input hits.
    ///
    /// For each hit this performs one `BTreeSet::contains` lookup for the exact
    /// `(property_id, value, shard_id, vertex_id)` posting. Work is therefore proportional to the
    /// page size and safe for range-walk pages whose hits may be scattered across arbitrary
    /// `(shard_id, vertex_id) intervals. Used by `Self::lookup_range_intersection_page`; callers
    /// that know their hits are densely packed in `(shard_id, vertex_id)` order can use
    /// `Self::filter_hits_by_equal` instead.
    fn filter_hits_by_equal_point_lookup(
        &self,
        property_id: u32,
        value: &[u8],
        hits: Vec<PostingHit>,
    ) -> Result<Vec<PostingHit>, IndexError> {
        ensure_index_value_key(value)?;
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let mut key = PostingKey {
            property_id,
            value: value.to_vec(),
            shard_id: ShardId::new(0),
            vertex_id: 0,
        };
        Ok(INDEX_VERTEX_POSTINGS.with_borrow(|postings| {
            hits.into_iter()
                .filter(|hit| {
                    key.shard_id = hit.shard_id;
                    key.vertex_id = hit.vertex_id;
                    postings.contains(&key)
                })
                .collect()
        }))
    }

    /// Keep only hits whose `(shard_id, vertex_id)` has a posting for `(property_id, value)`.
    ///
    /// Equality-arm sieve for streaming property intersection: the caller walks one arm in pages
    /// ([`Self::lookup_equal_page`]) and sieves each page against the other arms here, so the index
    /// never materializes a full posting bucket for any arm.
    ///
    /// The implementation picks a dense range scan when the `(shard_id, vertex_id)` span of the
    /// input page is small relative to its length (the common case for `lookup_equal_page`), and
    /// falls back to per-hit point lookups when the span is large (e.g. range-walk pages whose
    /// hits may be scattered across arbitrary subject ids). In both cases the work is bounded by
    /// the page size, not by the equality bucket. `hits` need not be pre-sorted; the returned
    /// survivors are in `(shard_id, vertex_id)` order. The result is at most `len(hits)`, so
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

        if equal_sieve_dense_threshold_met(&hits) {
            self.filter_hits_by_equal_dense(property_id, value, hits)
        } else {
            self.filter_hits_by_equal_point_lookup(property_id, value, hits)
        }
    }

    /// Dense-path equality sieve: single sorted merge over the bounded subject range.
    fn filter_hits_by_equal_dense(
        &self,
        property_id: u32,
        value: &[u8],
        hits: Vec<PostingHit>,
    ) -> Result<Vec<PostingHit>, IndexError> {
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

    /// Bounded range-walk plus equality sieves: walk `range_property_id` over `[low, high)`
    /// one page at a time and keep only hits that also have every equality `(property_id, value)`
    /// posting supplied in `equal_specs`. The returned `next`/`done` always describe the range
    /// walk, so an empty survivor page is not terminal when the range walk has more pages.
    ///
    /// Execution contract:
    /// - `equal_specs` must contain between 1 and [`MAX_EQUALITY_INTERSECTION_ARMS`] vertex specs
    ///   inclusive. Zero specs is rejected with [`IndexError::MissingEqualityIntersectionArms`].
    ///   More than [`MAX_EQUALITY_INTERSECTION_ARMS`] specs return
    ///   [`IndexError::TooManyEqualityIntersectionArms`].
    /// - All specs must target [`IndexSubject::VertexProperty`]. Edge or mixed specs return
    ///   [`IndexError::InvalidIntersectionSubject`].
    /// - The sieve arms are applied in canonical `(property_id, value)` order for deterministic
    ///   paging; repeated requests with the same spec set converge to the same walk plan and resume
    ///   cursor.
    pub fn lookup_range_intersection_page(
        &self,
        req: &LookupRangeIntersectionPageRequest,
    ) -> Result<PostingHitPage, IndexError> {
        if req.equal_specs.is_empty() {
            return Err(IndexError::MissingEqualityIntersectionArms);
        }
        if req.equal_specs.len() > MAX_EQUALITY_INTERSECTION_ARMS {
            return Err(IndexError::TooManyEqualityIntersectionArms);
        }
        let mut equal_specs = req.equal_specs.clone();
        equal_specs.sort_by(|a, b| {
            a.property_id
                .cmp(&b.property_id)
                .then_with(|| a.value.cmp(&b.value))
        });
        for spec in &equal_specs {
            if !matches!(spec.subject, IndexSubject::VertexProperty) {
                return Err(IndexError::InvalidIntersectionSubject);
            }
            ensure_index_value_key(&spec.value)?;
        }
        let walk_page = self.lookup_range_page(&LookupRangePageRequest {
            property_id: req.range_property_id,
            range: PostingRangeRequest::Between {
                low: req.low.clone(),
                high: req.high.clone(),
            },
            after: req.after.clone(),
            limit: req.limit,
        })?;
        let mut survivors = walk_page.hits;
        for spec in &equal_specs {
            if survivors.is_empty() {
                break;
            }
            survivors = self.filter_hits_by_equal(spec.property_id, &spec.value, survivors)?;
        }
        Ok(PostingHitPage {
            hits: survivors,
            next: walk_page.next,
            done: walk_page.done,
        })
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
