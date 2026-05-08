//! Public coordination layer over stable storage.

mod ic_budget;
mod store;

pub mod mutation_executor;

pub use ic_budget::{
    GRAPH_TIMER_LARA_MAX_INSTRUCTIONS, GRAPH_TIMER_LARA_RESERVE_INSTRUCTIONS,
    IC_CANISTER_MESSAGE_INSTRUCTION_LIMIT, timer_lara_maintenance_budget,
};
pub use store::{EdgeHandle, GraphStore, GraphStoreError};
