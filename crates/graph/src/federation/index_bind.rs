//! Index posting hits → plan row bindings.

use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::PostingHit;
use ic_stable_lara::VertexId;

use crate::facade::GraphStore;
use crate::plan::{PlanBinding, query::PlanRow};

/// Bind hits on `local_shard_id` to local [`PlanBinding::Vertex`] rows.
///
/// Index postings are kept consistent on DML; read path does not filter tombstones.
pub(crate) fn bind_local_index_hits(
    store: &GraphStore,
    rows: &[PlanRow],
    variable: &str,
    hits: &[PostingHit],
    local_shard_id: ShardId,
) -> Vec<PlanRow> {
    let mut out = Vec::new();
    for row in rows {
        for hit in hits {
            if hit.shard_id != local_shard_id {
                continue;
            }
            let vertex_id = VertexId::from(hit.vertex_id);
            if store.vertex(vertex_id).is_none() {
                continue;
            }
            out.push(row.fork([(variable, PlanBinding::Vertex(vertex_id))]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::GraphStore;
    use crate::plan::query::empty_row_for_plan;
    use gleaph_gql::Value;

    #[test]
    fn bind_local_index_hits_skips_foreign_shard() {
        let store = GraphStore::new();
        let local_shard = 0u32;
        let vid = store
            .insert_vertex_named(["Local"], [("k", Value::Int64(1))])
            .expect("insert");
        let row = empty_row_for_plan(&gleaph_gql_planner::plan::PhysicalPlan::from_ops(vec![]));
        let hits = vec![
            PostingHit {
                shard_id: local_shard,
                vertex_id: u32::from(vid),
            },
            PostingHit {
                shard_id: local_shard + 1,
                vertex_id: 99,
            },
        ];
        let out =
            bind_local_index_hits(&store, std::slice::from_ref(&row), "n", &hits, local_shard);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0].get("n"),
            Some(PlanBinding::Vertex(id)) if *id == vid
        ));
    }
}
