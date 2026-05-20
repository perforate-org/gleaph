//! Cross-canister GQL execution wire types (router → graph).
//!
//! IC surface rules (enforced by canister `#[query]` / `#[update]` attributes):
//! - **Query** programs use composite query on the router and `execute_*_query` on graph
//!   (read path; may call index / other canisters).
//! - **Update** programs use update on the router and `execute_*_update` on graph (DML and
//!   posting maintenance). A composite query must not invoke an update method.

use candid::CandidType;
use serde::{Deserialize, Serialize};

use crate::federation::ShardId;

/// Selects the IC call kind for a wired program/plan (must match the canister entrypoint).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GqlExecutionMode {
    /// Read-only execution (`gql_query` / `execute_plan_query` / composite where needed).
    Query,
    /// Write path (`gql_execute` / `execute_plan_update`).
    Update,
}

/// Router → graph: execute a pre-built physical plan on a target shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanArgs {
    pub target_shard_id: ShardId,
    pub plan_blob: Vec<u8>,
    pub params_blob: Vec<u8>,
    pub mode: GqlExecutionMode,
    /// When set, graph skips the first anchor `IndexScan` and binds these local vertex ids.
    pub seed_bindings_blob: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanResult {
    pub row_count: u64,
}

/// Phase-0 bridge: rkyv [`gleaph_gql::ast::GqlProgram`] (no parse on graph).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ExecuteProgramArgs {
    pub target_shard_id: ShardId,
    pub program_blob: Vec<u8>,
    pub params_blob: Vec<u8>,
    pub mode: GqlExecutionMode,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ExecuteProgramResult {
    pub row_count: u64,
}

/// Router → graph seed bindings for a single variable on the target shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SeedBindingEntry {
    pub variable: String,
    pub local_vertex_ids: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SeedBindingsWire {
    pub entries: Vec<SeedBindingEntry>,
}
