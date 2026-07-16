//! Federated property index integration with [`gleaph_graph_index`].

pub mod batch;
pub mod catalog_context;
pub mod edge_lookup;
pub mod edge_pending;
pub mod edge_property_backfill;
pub mod federation_routing;
pub mod ic;
#[cfg(test)]
mod inv_oracle;
pub mod label_backfill;
pub mod label_pending;
pub mod lookup;
pub mod pending;
pub mod repair_journal;
pub mod router;
mod router_call;
pub mod vector_catalog_context;
pub mod vector_dispatch;
pub mod vector_ic;
pub mod vector_lookup;
pub mod vector_pending;
pub mod vertex_embedding_backfill;
pub mod vertex_property_backfill;
