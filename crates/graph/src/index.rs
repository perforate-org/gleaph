//! Federated property index integration with [`gleaph_graph_index`].

pub mod catalog_context;
pub mod edge_lookup;
pub mod edge_pending;
pub mod edge_property_backfill;
pub mod federation_routing;
pub mod ic;
pub mod label_backfill;
pub mod label_pending;
pub mod lookup;
pub mod pending;
pub mod router;
mod router_call;
pub mod vertex_property_backfill;
