//! Federated property index integration with [`gleaph_graph_index`].

pub mod edge_equal;
pub mod federation;
pub mod ic;
pub mod lookup;
pub mod pending;
pub mod placement;
pub mod router;
#[cfg(target_family = "wasm")]
mod router_call;
