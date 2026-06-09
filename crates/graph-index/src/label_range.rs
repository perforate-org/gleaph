//! Half-open `[low, high)` bounds over [`LabelPostingKey`] for label bucket scans.

use crate::label_key::LabelPostingKey;

fn label_min(vertex_label_id: u32) -> LabelPostingKey {
    LabelPostingKey::prefix_lower(vertex_label_id)
}

fn label_end_exclusive(vertex_label_id: u32) -> Option<LabelPostingKey> {
    Some(LabelPostingKey::prefix_lower(
        vertex_label_id.checked_add(1)?,
    ))
}

/// Half-open `[low, high)` range covering all label postings for one `vertex_label_id`.
pub fn label_posting_bucket(vertex_label_id: u32) -> Option<(LabelPostingKey, LabelPostingKey)> {
    let low = label_min(vertex_label_id);
    let high = label_end_exclusive(vertex_label_id)?;
    if low >= high {
        return None;
    }
    Some((low, high))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_bucket_bounds() {
        let (low, high) = label_posting_bucket(5).expect("bucket");
        assert_eq!(low.vertex_label_id, 5);
        assert_eq!(high.vertex_label_id, 6);
        assert!(low < high);
    }
}
