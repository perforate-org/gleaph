//! Half-open `[low, high)` bounds over [`PostingKey`] for ordering comparisons on encoded values.

use crate::key::PostingKey;
use gleaph_graph_kernel::index::PostingRangeRequest;

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

fn property_min(property_id: u32) -> PostingKey {
    PostingKey::prefix_lower(property_id, &[])
}

/// First [`PostingKey`] not belonging to `property_id` (half-open property bucket upper bound).
fn property_end_exclusive(property_id: u32) -> Option<PostingKey> {
    Some(PostingKey::prefix_lower(property_id.checked_add(1)?, &[]))
}

/// Half-open posting key range `[low, high)` covering encoded-value predicates for one `property_id`.
pub(crate) fn posting_key_half_open_range(
    property_id: u32,
    req: &PostingRangeRequest,
) -> Option<(PostingKey, PostingKey)> {
    let high_bucket = property_end_exclusive(property_id)?;

    match req {
        PostingRangeRequest::Ge(b) => {
            let low = PostingKey::prefix_lower(property_id, b);
            Some((low, high_bucket))
        }
        PostingRangeRequest::Gt(b) => {
            let low = PostingKey::prefix_lower(property_id, &lex_succ_bytes(b));
            Some((low, high_bucket))
        }
        PostingRangeRequest::Le(b) => {
            let low = property_min(property_id);
            let high = PostingKey::prefix_lower(property_id, &lex_succ_bytes(b));
            Some((low, high))
        }
        PostingRangeRequest::Lt(b) => {
            let low = property_min(property_id);
            let high = PostingKey::prefix_lower(property_id, b);
            Some((low, high))
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
        let (low, _) = posting_key_half_open_range(7, &PostingRangeRequest::Ge(b.clone())).unwrap();
        assert_eq!(low, PostingKey::prefix_lower(7, &b));
    }

    #[test]
    fn lt_range_excludes_bound_value() {
        let b = vec![10u8];
        let (_, high) =
            posting_key_half_open_range(3, &PostingRangeRequest::Lt(b.clone())).unwrap();
        assert_eq!(high, PostingKey::prefix_lower(3, &b));
    }
}
