//! Instruction/size guards for federated index fast paths and seed routing.

use gleaph_graph_kernel::index::PostingHit;

/// Maximum `(shard_id, vertex_id)` pairs shipped as a C1 aggregate vertex filter.
///
/// Seed routing no longer falls back to unseeded shard execution when this budget is exceeded;
/// large seed lists are preferred over all-shard local scans per ADR 0004.
pub const FAST_PATH_MAX_VERTEX_FILTER_HITS: usize = 10_000;

#[inline]
pub fn posting_hits_exceed_fast_path_budget(hits: &[PostingHit]) -> bool {
    hits.len() > FAST_PATH_MAX_VERTEX_FILTER_HITS
}

#[inline]
pub fn packed_vertices_exceed_fast_path_budget(packed: &[u64]) -> bool {
    packed.len() > FAST_PATH_MAX_VERTEX_FILTER_HITS
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::PostingHit;

    #[test]
    fn budget_allows_at_limit() {
        let hits = vec![
            PostingHit {
                shard_id: ShardId::new(1),
                vertex_id: 0,
            };
            FAST_PATH_MAX_VERTEX_FILTER_HITS
        ];
        assert!(!posting_hits_exceed_fast_path_budget(&hits));
    }

    #[test]
    fn budget_rejects_over_limit() {
        let hits = vec![
            PostingHit {
                shard_id: ShardId::new(1),
                vertex_id: 0,
            };
            FAST_PATH_MAX_VERTEX_FILTER_HITS + 1
        ];
        assert!(posting_hits_exceed_fast_path_budget(&hits));
    }
}
