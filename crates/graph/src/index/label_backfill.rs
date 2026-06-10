//! Cursor-based backfill of label postings from shard-local vertex label state.

use crate::facade::GraphStore;
use crate::index::lookup::PropertyIndexLookup;
use gleaph_graph_kernel::federation::{PostingBackfillArgs, PostingBackfillResult};
use ic_stable_lara::VertexId;

pub async fn backfill_label_postings(
    store: &GraphStore,
    index: &dyn PropertyIndexLookup,
    args: PostingBackfillArgs,
) -> Result<PostingBackfillResult, String> {
    if !store.federation_configured() {
        return Err("federation not configured".into());
    }
    let shard_id = index.local_shard_id();
    let vertex_cap = u32::from(store.vertex_count());
    let max_vertices = args.max_vertices.max(1);
    let mut cursor = args.start_vertex_id.min(vertex_cap);
    let mut vertices_processed = 0u32;
    let mut postings_synced = 0u32;

    while vertices_processed < max_vertices && cursor < vertex_cap {
        let vertex_id = VertexId::from(cursor);
        cursor = cursor.saturating_add(1);
        vertices_processed = vertices_processed.saturating_add(1);

        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone() {
            continue;
        }
        let labels = store.vertex_labels(vertex_id, vertex);
        let local_raw = u32::from_le_bytes(vertex_id.to_le_bytes());
        for label in labels {
            index
                .label_posting_insert_at(shard_id, u32::from(label.raw()), local_raw)
                .await
                .map_err(|e| e.to_string())?;
            postings_synced = postings_synced.saturating_add(1);
        }
    }

    Ok(PostingBackfillResult {
        next_vertex_id: cursor,
        vertices_processed,
        postings_synced,
        done: cursor >= vertex_cap,
    })
}
