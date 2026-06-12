//! Federated property index integration with [`gleaph_graph_index`].

pub mod edge_lookup;
pub mod edge_pending;
pub mod edge_property_backfill;
pub mod ic;
pub mod label_backfill;
pub mod label_pending;
pub mod lookup;
pub mod pending;
pub mod placement;
pub mod property_backfill;
pub mod registry;
pub mod router;
mod router_call;
