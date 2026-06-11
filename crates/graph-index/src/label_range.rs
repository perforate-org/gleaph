//! Half-open `[low, high)` bounds over [`LabelPostingKey`] for label bucket scans.

use crate::label_key::LabelPostingKey;
use gleaph_graph_kernel::federation::ShardId;

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

/// Half-open `[low, high)` range for one `(vertex_label_id, shard_id)` prefix.
pub fn label_shard_posting_bucket(
    vertex_label_id: u32,
    shard_id: ShardId,
) -> Option<(LabelPostingKey, LabelPostingKey)> {
    let (_, label_high) = label_posting_bucket(vertex_label_id)?;
    let low = LabelPostingKey {
        vertex_label_id,
        shard_id,
        vertex_id: 0,
    };
    let high = shard_id
        .checked_add(1)
        .map(|next_shard| LabelPostingKey {
            vertex_label_id,
            shard_id: next_shard,
            vertex_id: 0,
        })
        .unwrap_or(label_high);
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

    #[test]
    fn label_shard_bucket_bounds() {
        let (low, high) = label_shard_posting_bucket(5, ShardId::new(0)).expect("shard bucket");
        assert_eq!(
            low,
            LabelPostingKey {
                vertex_label_id: 5,
                shard_id: ShardId::new(0),
                vertex_id: 0,
            }
        );
        assert_eq!(
            high,
            LabelPostingKey {
                vertex_label_id: 5,
                shard_id: ShardId::new(1),
                vertex_id: 0,
            }
        );
    }
}
