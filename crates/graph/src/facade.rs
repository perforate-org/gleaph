//! Public coordination layer over stable storage.

mod ic_budget;
mod ic_gql_extensions;
mod store;

pub mod mutation_executor;

pub use ic_budget::{
    GRAPH_TIMER_LARA_MAX_INSTRUCTIONS, GRAPH_TIMER_LARA_RESERVE_INSTRUCTIONS,
    IC_CANISTER_MESSAGE_INSTRUCTION_LIMIT, timer_lara_maintenance_budget,
};
pub use ic_gql_extensions::{ic_extension_type_names, init_ic_gql_extensions};
pub use store::{EdgeHandle, GraphStore, GraphStoreError};
