//! Shared helpers for batched federated index updates.

use candid::Encode;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::IndexPostingMutation;
use gleaph_message_sizing::{FitError, SizingPolicy, adaptive_fitting_prefix};

/// Find the largest sub-slice of `operations[start..]` whose encoded `(ShardId, sub_slice)`
/// payload still fits inside the safe inter-canister request payload limit. The shared sizing
/// helper uses a target-sized probe and retries below the hard limit when the estimate is high.
///
/// Returns at least `start + 1` so the caller always makes progress, even if a
/// single operation somehow exceeds the limit (the target canister will reject
/// it and the caller's error path can journal the op for repair).
pub(crate) fn posting_batch_chunk_end(
    shard_id: ShardId,
    operations: &[IndexPostingMutation],
    start: usize,
) -> usize {
    let remaining = operations.len().saturating_sub(start);
    let result =
        adaptive_fitting_prefix(remaining, None, SizingPolicy::inter_canister(), |count| {
            let candidate = operations[start..start + count].to_vec();
            Encode!(&(shard_id, &candidate))
                .map(|encoded| encoded.len())
                .map_err(|error| error.to_string())
        });
    match result {
        Ok(Some(fitted)) => start + fitted.entry_count,
        Ok(None) | Err(FitError::Measure(_)) | Err(FitError::NoEntryFits { .. }) => {
            // Preserve the queue's forward-progress contract. The downstream canister remains
            // authoritative for a pathological single-entry rejection, and the caller journals
            // that suffix on the normal call error path.
            start.saturating_add(1).min(operations.len())
        }
    }
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
