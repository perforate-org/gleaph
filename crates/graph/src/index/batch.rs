//! Shared helpers for batched federated index updates.

use candid::Encode;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::IndexPostingMutation;

/// Binary-search the largest sub-slice of `operations[start..]` whose encoded
/// `(ShardId, sub_slice)` payload still fits inside the safe inter-canister
/// request payload limit.
///
/// Returns at least `start + 1` so the caller always makes progress, even if a
/// single operation somehow exceeds the limit (the target canister will reject
/// it and the caller's error path can journal the op for repair).
pub(crate) fn posting_batch_chunk_end(
    shard_id: ShardId,
    operations: &[IndexPostingMutation],
    start: usize,
) -> usize {
    let mut low = start.saturating_add(1);
    let mut high = operations.len();
    let mut best = operations.len();
    while low <= high {
        let end = low + (high - low) / 2;
        let candidate = operations[start..end].to_vec();
        let Ok(encoded) = Encode!(&(shard_id, &candidate)) else {
            high = end.saturating_sub(1);
            continue;
        };
        if encoded.len() <= gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            best = end;
            low = end.saturating_add(1);
        } else {
            high = end.saturating_sub(1);
        }
    }
    best.max(start.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::IndexPostingMutation;

    #[test]
    fn chunk_end_fits_single_small_operation() {
        let ops = vec![IndexPostingMutation::Label {
            remove: false,
            label_id: 1,
            vertex_id: 2,
        }];
        let shard_id = ShardId::from(0);
        assert_eq!(posting_batch_chunk_end(shard_id, &ops, 0), 1);
    }

    #[test]
    fn chunk_end_splits_large_batch_by_payload_size() {
        // Each VertexProperty op carries a 2 KiB payload, so a handful of them
        // already exceed the 2 MiB safe limit and must be chunked.
        let payload = vec![0u8; 2 * 1024];
        let ops: Vec<IndexPostingMutation> = (0..2000u32)
            .map(|i| IndexPostingMutation::VertexProperty {
                remove: false,
                property_id: 1,
                value: payload.clone(),
                vertex_id: i,
            })
            .collect();
        let shard_id = ShardId::from(0);
        let end = posting_batch_chunk_end(shard_id, &ops, 0);
        assert!(
            end < ops.len(),
            "expected a size-based chunk, but got end={end} for {} ops",
            ops.len()
        );
        assert!(end > 0);
    }
}
