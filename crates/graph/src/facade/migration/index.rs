//! Federated index posting maintenance during vertex migration.

use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_ic::IcExtensionBinaryDecode;
use gleaph_graph_kernel::federation::{ExportedProperty, ExportedVertex, ShardId};

fn index_key_bytes(prop: &ExportedProperty) -> Result<Option<Vec<u8>>, PlanQueryError> {
    let value = Value::from_binary_bytes_with_extensions(
        &prop.value_bytes,
        &IcExtensionBinaryDecode::INSTANCE,
    )
    .map_err(|e| PlanQueryError::FederatedIndexCall {
        op: "migration_property_decode",
        detail: e.to_string(),
    })?;
    Ok(value_to_index_key_bytes(&value).ok().flatten())
}

/// Removes source-shard postings, then inserts destination-shard postings.
///
/// Call after the destination vertex row exists and router placement has been committed.
pub async fn sync_migration_index_postings(
    index: &dyn PropertyIndexLookup,
    bundle: &ExportedVertex,
    destination_shard_id: ShardId,
    destination_local_vertex_id: u32,
) -> Result<(), PlanQueryError> {
    for prop in &bundle.properties {
        let Some(key_bytes) = index_key_bytes(prop)? else {
            continue;
        };
        let pid = prop.property_id.raw();
        index
            .posting_remove_at(
                bundle.source_shard_id,
                pid,
                key_bytes.clone(),
                bundle.source_local_vertex_id,
            )
            .await?;
        index
            .posting_insert_at(
                destination_shard_id,
                pid,
                key_bytes,
                destination_local_vertex_id,
            )
            .await?;
    }
    Ok(())
}
