//! Half-open `[low, high)` bounds over [`PostingKey`] and [`EdgePostingKey`] for ordering
//! comparisons on encoded values.
//!
//! The upper bound is a [`Bound`] so the terminal `(u64::MAX, u32::MAX)` property bucket — whose
//! lexicographic successor does not exist — scans to `Bound::Unbounded` instead of being treated
//! as empty. The purge module (`facade/store/posting_purge.rs`) uses the same pattern.

use crate::edge_key::EdgePostingKey;
use crate::key::PostingKey;
use gleaph_graph_kernel::index::{PhysicalIndexId, PostingRangeRequest};
use std::ops::Bound;

/// Lexicographic successor of `b` as an unbounded-length byte sequence (`memcmp` order).
pub(crate) fn lex_succ_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = b.to_vec();
    for i in (0..out.len()).rev() {
        if out[i] < 255 {
            out[i] += 1;
            out.truncate(i + 1);
            return out;
        }
    }
    let mut v = b.to_vec();
    v.push(0);
    v
}

fn property_min(physical_index_id: PhysicalIndexId, property_id: u32) -> PostingKey {
    PostingKey::prefix_lower(physical_index_id, property_id, &[])
}

/// Exclusive upper bound for one `(physical_index_id, property_id)` bucket.
///
/// The terminal bucket `(u64::MAX, u32::MAX)` has no in-order successor, so its upper bound is
/// unbounded; every other bucket ends at the next property (or next physical namespace) prefix.
pub(crate) fn property_end_exclusive(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
) -> Bound<PostingKey> {
    match property_id.checked_add(1) {
        Some(next) => Bound::Excluded(PostingKey::prefix_lower(physical_index_id, next, &[])),
        None => physical_index_id
            .checked_next()
            .map_or(Bound::Unbounded, |next| {
                Bound::Excluded(PostingKey::prefix_lower(next, 0, &[]))
            }),
    }
}

/// Half-open `[low, high)` range covering all postings for one `property_id`.
///
/// The bucket is never empty: `low` is the property prefix and `high` either names the next
/// property/namespace prefix (strictly greater) or is unbounded at the terminal bucket.
pub fn property_posting_bucket(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
) -> (PostingKey, Bound<PostingKey>) {
    (
        property_min(physical_index_id, property_id),
        property_end_exclusive(physical_index_id, property_id),
    )
}

/// Half-open posting key range `[low, high)` covering encoded-value predicates for one `property_id`.
pub(crate) fn posting_key_half_open_range(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    req: &PostingRangeRequest,
) -> (PostingKey, Bound<PostingKey>) {
    let high_bucket = property_end_exclusive(physical_index_id, property_id);

    match req {
        PostingRangeRequest::Ge(b) => {
            let low = PostingKey::prefix_lower(physical_index_id, property_id, b);
            (low, high_bucket)
        }
        PostingRangeRequest::Gt(b) => {
            let low = PostingKey::prefix_lower(physical_index_id, property_id, &lex_succ_bytes(b));
            (low, high_bucket)
        }
        PostingRangeRequest::Le(b) => {
            let low = property_min(physical_index_id, property_id);
            let high = PostingKey::prefix_lower(physical_index_id, property_id, &lex_succ_bytes(b));
            (low, Bound::Excluded(high))
        }
        PostingRangeRequest::Lt(b) => {
            let low = property_min(physical_index_id, property_id);
            let high = PostingKey::prefix_lower(physical_index_id, property_id, b);
            (low, Bound::Excluded(high))
        }
        PostingRangeRequest::Between { low, high } => {
            let low_key = PostingKey::prefix_lower(physical_index_id, property_id, low);
            let high_key = PostingKey::prefix_lower(physical_index_id, property_id, high);
            (low_key, Bound::Excluded(high_key))
        }
    }
}

/// Exclusive upper bound for the contiguous `(physical_index_id, property_id)` range of edge keys.
pub(crate) fn edge_property_bucket_end_exclusive(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
) -> Bound<EdgePostingKey> {
    match property_id.checked_add(1) {
        Some(next) => Bound::Excluded(EdgePostingKey::prefix_lower(physical_index_id, next, &[])),
        None => physical_index_id
            .checked_next()
            .map_or(Bound::Unbounded, |next| {
                Bound::Excluded(EdgePostingKey::prefix_lower(next, 0, &[]))
            }),
    }
}

fn edge_property_min(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    label_id: Option<u16>,
) -> EdgePostingKey {
    match label_id {
        Some(label) => {
            EdgePostingKey::prefix_lower_labeled(physical_index_id, property_id, &[], label)
        }
        None => EdgePostingKey::prefix_lower(physical_index_id, property_id, &[]),
    }
}

fn edge_value_bound(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    value: &[u8],
    label_id: Option<u16>,
) -> EdgePostingKey {
    match label_id {
        Some(label) => {
            EdgePostingKey::prefix_lower_labeled(physical_index_id, property_id, value, label)
        }
        None => EdgePostingKey::prefix_lower(physical_index_id, property_id, value),
    }
}

/// Half-open edge posting key range `[low, high)` covering an encoded-value predicate for one
/// edge property bucket. When `label_id` is set the bounds pin that wire label; postings with
/// other labels inside the value interval are sieved by the caller during iteration.
pub(crate) fn edge_posting_key_half_open_range(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    req: &PostingRangeRequest,
    label_id: Option<u16>,
) -> (EdgePostingKey, Bound<EdgePostingKey>) {
    let high_bucket = edge_property_bucket_end_exclusive(physical_index_id, property_id);

    match req {
        PostingRangeRequest::Ge(b) => {
            let low = edge_value_bound(physical_index_id, property_id, b, label_id);
            (low, high_bucket)
        }
        PostingRangeRequest::Gt(b) => {
            let low =
                edge_value_bound(physical_index_id, property_id, &lex_succ_bytes(b), label_id);
            (low, high_bucket)
        }
        PostingRangeRequest::Le(b) => {
            let low = edge_property_min(physical_index_id, property_id, label_id);
            let high =
                edge_value_bound(physical_index_id, property_id, &lex_succ_bytes(b), label_id);
            (low, Bound::Excluded(high))
        }
        PostingRangeRequest::Lt(b) => {
            let low = edge_property_min(physical_index_id, property_id, label_id);
            let high = edge_value_bound(physical_index_id, property_id, b, label_id);
            (low, Bound::Excluded(high))
        }
        PostingRangeRequest::Between { low, high } => {
            let low_key = edge_value_bound(physical_index_id, property_id, low, label_id);
            let high_key = edge_value_bound(physical_index_id, property_id, high, label_id);
            (low_key, Bound::Excluded(high_key))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_succ_smoke() {
        assert_eq!(lex_succ_bytes(&[]), vec![0]);
        assert_eq!(lex_succ_bytes(&[0]), vec![1]);
        assert_eq!(lex_succ_bytes(&[255]), vec![255, 0]);
        assert_eq!(lex_succ_bytes(&[1, 255]), vec![2]);
    }

    #[test]
    fn ge_range_low_includes_exact_bound() {
        let b = vec![1u8, 2u8];
        let physical_index_id = PhysicalIndexId::new(1).unwrap();
        let (low, _) =
            posting_key_half_open_range(physical_index_id, 7, &PostingRangeRequest::Ge(b.clone()));
        assert_eq!(low, PostingKey::prefix_lower(physical_index_id, 7, &b));
    }

    #[test]
    fn lt_range_excludes_bound_value() {
        let b = vec![10u8];
        let physical_index_id = PhysicalIndexId::new(1).unwrap();
        let (_, high) =
            posting_key_half_open_range(physical_index_id, 3, &PostingRangeRequest::Lt(b.clone()));
        assert_eq!(
            high,
            Bound::Excluded(PostingKey::prefix_lower(physical_index_id, 3, &b))
        );
    }

    #[test]
    fn terminal_bucket_upper_bound_is_unbounded() {
        let max_physical = PhysicalIndexId::new(u64::MAX).unwrap();
        let (low, high) = property_posting_bucket(max_physical, u32::MAX);
        assert_eq!(low, PostingKey::prefix_lower(max_physical, u32::MAX, &[]));
        assert_eq!(high, Bound::Unbounded);
    }

    #[test]
    fn max_property_before_last_namespace_bounds_at_next_namespace() {
        let physical = PhysicalIndexId::new(u64::MAX - 1).unwrap();
        let max_physical = PhysicalIndexId::new(u64::MAX).unwrap();
        let (_, high) = property_posting_bucket(physical, u32::MAX);
        assert_eq!(
            high,
            Bound::Excluded(PostingKey::prefix_lower(max_physical, 0, &[]))
        );
    }

    #[test]
    fn ge_range_at_terminal_bucket_is_unbounded() {
        let max_physical = PhysicalIndexId::new(u64::MAX).unwrap();
        let (low, high) = posting_key_half_open_range(
            max_physical,
            u32::MAX,
            &PostingRangeRequest::Ge(b"v".to_vec()),
        );
        assert_eq!(low, PostingKey::prefix_lower(max_physical, u32::MAX, b"v"));
        assert_eq!(high, Bound::Unbounded);
    }
}
