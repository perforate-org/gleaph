mod api;
#[cfg(feature = "canbench-rs")]
mod bench;
mod certification;
mod gql_bridge;
pub mod state;

use ic_cdk::export_candid;
use ic_cdk_macros::{init, post_upgrade, pre_upgrade, query, update};

#[init]
/// Initializes the graph canister state with optional capacity parameters.
fn init(initial_vertex_capacity: Option<u32>, initial_edge_capacity: Option<u64>) {
    // When canbench pre-loads stable memory with a benchmark snapshot, restore
    // from it instead of overwriting with a fresh graph.
    #[cfg(feature = "bench-ecom")]
    {
        use gleaph_pma::Memory;
        let mem = state::IcStableMemory::default();
        if mem.size_bytes() > 0
            && let Ok(()) = state::restore_state_uncertified()
        {
            return;
        }
    }
    #[cfg(feature = "bench-social")]
    {
        use gleaph_pma::Memory;
        let mem = state::IcStableMemory::default();
        if mem.size_bytes() > 0
            && let Ok(()) = state::restore_state_uncertified()
        {
            return;
        }
    }
    #[cfg(feature = "bench-timeline")]
    {
        use gleaph_pma::Memory;
        let mem = state::IcStableMemory::default();
        if mem.size_bytes() > 0
            && let Ok(()) = state::restore_state_uncertified()
        {
            return;
        }
    }
    certification::init_certification();
    state::init_state(
        initial_vertex_capacity.unwrap_or(1024),
        initial_edge_capacity.unwrap_or(0),
    )
    .expect("failed to initialize graph state");
    certification::certify_stats(api::get_stats());
}

#[pre_upgrade]
/// Persists graph metadata before a canister upgrade.
fn pre_upgrade() {
    state::persist_state_metadata().expect("failed to persist graph state for upgrade");
}

#[post_upgrade]
/// Restores graph state after a canister upgrade.
fn post_upgrade() {
    state::restore_state().expect("failed to restore graph state after upgrade");
    certification::certify_stats(api::get_stats());
}

// ── Stats & Monitoring ───────────────────────────────────────────────────

#[query]
/// Returns neighbors for a vertex.
fn get_neighbors(vertex_id: u32) -> Vec<gleaph_types::EdgeInfo> {
    api::get_neighbors(vertex_id)
}

#[query]
/// Returns graph statistics.
fn get_stats() -> gleaph_types::GraphStats {
    api::get_stats()
}

#[query]
fn get_stats_certified() -> gleaph_types::CertifiedResponse<gleaph_types::GraphStats> {
    api::get_stats_certified()
}

#[query]
/// Returns diagnostic information about the canister.
fn get_canister_info() -> gleaph_types::CanisterInfo {
    api::get_canister_info()
}

#[query]
/// Returns operational metrics: query/mutation/algorithm counters and memory usage.
fn get_metrics() -> gleaph_types::OperationalMetrics {
    api::get_metrics()
}

#[query]
/// Returns the current per-tenant usage quotas.
fn get_quota() -> gleaph_types::UsageQuota {
    api::get_quota()
}

#[update]
/// Configures per-tenant usage quotas. Controller-only in production.
fn set_quota(quota: gleaph_types::UsageQuota) {
    api::set_quota(quota)
}

#[query]
/// Returns cached planner statistics (label cardinality, property selectivity, etc.).
fn get_planner_stats() -> gleaph_types::PlannerStats {
    api::get_planner_stats()
}

#[update]
/// Recomputes sample-based property-selectivity estimates and persists them in the overlay.
fn compute_graph_stats() -> gleaph_types::PlannerStats {
    api::compute_graph_stats()
}

// ── GQL Query & Mutation ─────────────────────────────────────────────────

#[query(name = "query")]
/// Executes a read-only GQL statement with optional parameters.
/// Auto-pages large result sets via continuation tokens.
fn query_gql(
    gql: String,
    params: Option<gleaph_types::PropertyMap>,
) -> Result<gleaph_types::QueryResultWithContinuation, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Read)?;
    api::query_gql(gql, params)
}

#[query(name = "explain")]
/// Returns planner/semantic explain lines for a read-only GQL query.
fn explain_gql(gql: String) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Read)?;
    api::explain_gql(gql)
}

#[update(name = "mutate")]
/// Executes a GQL mutation with optional parameters.
/// Returns continuation token for large operations.
fn mutate_gql(
    gql: String,
    params: Option<gleaph_types::PropertyMap>,
) -> Result<gleaph_types::MutationResultWithContinuation, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Write)?;
    api::mutate_gql(gql, params)
}

#[update(name = "batch_mutate")]
/// Executes multiple GQL mutations, each with optional parameters.
fn batch_mutate_gql(
    gqls: Vec<(String, Option<gleaph_types::PropertyMap>)>,
) -> Vec<Result<gleaph_types::MutationResult, gleaph_types::GleaphError>> {
    if let Err(e) = api::check_caller_permission(&gleaph_types::AccessLevel::Write) {
        return gqls.iter().map(|_| Err(e.clone())).collect();
    }
    api::batch_mutate_gql(gqls)
}

#[query(name = "query_continue")]
/// Resumes a paginated query or algorithm from a continuation token.
fn query_continue(
    token: gleaph_types::ContinuationToken,
) -> Result<gleaph_types::ContinuationResult, gleaph_types::GleaphError> {
    api::query_continue(token)
}

#[update(name = "mutate_continue")]
/// Resumes a suspended mutation from a continuation token.
fn mutate_continue(
    token: gleaph_types::ContinuationToken,
) -> Result<gleaph_types::MutationResultWithContinuation, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Write)?;
    api::mutate_continue(token)
}

// ── Prepared Statements ──────────────────────────────────────────────────

#[update]
/// Prepares a GQL statement for repeated execution under the given name.
fn prepare(
    name: String,
    gql: String,
    options: Option<gleaph_types::PreparedOptions>,
) -> Result<gleaph_types::PreparedStatementInfo, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Admin)?;
    api::prepare_gql(name, gql, options)
}

#[query(name = "execute_prepared")]
/// Executes a previously prepared read-only GQL statement with parameters.
fn execute_prepared_gql(
    name: String,
    params: gleaph_types::PropertyMap,
    sort: Option<Vec<gleaph_types::PreparedSortSpec>>,
) -> Result<gleaph_types::QueryResultWithContinuation, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Execute)?;
    api::execute_prepared_gql(name, params, sort)
}

#[update(name = "execute_prepared_mutation")]
/// Executes a previously prepared mutation GQL statement with parameters.
fn execute_prepared_mutation_gql(
    name: String,
    params: gleaph_types::PropertyMap,
) -> Result<gleaph_types::MutationResult, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Execute)?;
    api::execute_prepared_mutation_gql(name, params)
}

#[update]
/// Drops a prepared statement by name. Returns true if it existed.
fn drop_prepared(name: String) -> Result<bool, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Read)?;
    api::drop_prepared_gql(name)
}

#[query]
/// Lists all prepared statements with metadata.
fn list_prepared() -> Result<Vec<gleaph_types::PreparedStatementInfo>, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Read)?;
    api::list_prepared_gql()
}

// ── Algorithms ───────────────────────────────────────────────────────────

#[query(name = "bfs")]
/// Runs BFS with automatic continuation support for large graphs.
fn bfs_query(
    start: u32,
    config: gleaph_algo::bfs::BfsConfig,
) -> Result<gleaph_types::BfsResultWithContinuation, gleaph_types::GleaphError> {
    api::bfs_query(start, config)
}

#[query(name = "recommend")]
/// Collaborative filtering recommendations.
fn recommend_query(
    user: u32,
    config: gleaph_algo::recommend::RecommendConfig,
) -> Result<Vec<gleaph_types::Recommendation>, gleaph_types::GleaphError> {
    api::recommend_query(user, config)
}

#[update]
/// Computes PageRank with caching for certified retrieval. Supports continuation.
fn compute_pagerank(
    config: gleaph_algo::pagerank::PageRankConfig,
) -> Result<gleaph_types::PageRankResultWithContinuation, gleaph_types::GleaphError> {
    api::compute_pagerank(config)
}

#[update]
/// Computes single-source shortest paths with caching. Supports continuation.
fn compute_sssp(
    start: u32,
    config: gleaph_algo::sssp::SsspConfig,
) -> Result<gleaph_types::SsspResultWithContinuation, gleaph_types::GleaphError> {
    api::compute_sssp(start, config)
}

#[query]
/// Returns a cached, IC-certified PageRank result.
fn get_pagerank_certified(
    config_hash: Vec<u8>,
) -> Result<gleaph_types::CertifiedResponse<gleaph_types::PageRankResult>, gleaph_types::GleaphError>
{
    api::get_pagerank_certified(config_hash)
}

// ── Index ────────────────────────────────────────────────────────────────

#[update]
/// Creates a property index for faster query execution.
fn create_index(
    entity_type: gleaph_types::EntityType,
    property_name: String,
    index_type: gleaph_types::IndexType,
) -> Result<(), gleaph_types::GleaphError> {
    api::create_index(entity_type, property_name, index_type)
}

// ── Legacy Data Operations ───────────────────────────────────────────────

#[update]
/// Ensures a vertex exists and returns the updated vertex count.
fn add_vertex(vertex: gleaph_types::VertexData) -> Result<u64, gleaph_types::GleaphError> {
    api::add_vertex(vertex)
}

#[update]
/// Inserts one edge and returns the updated edge count.
fn add_edge(edge: gleaph_types::EdgeData) -> Result<u64, gleaph_types::GleaphError> {
    api::add_edge(edge)
}

#[update]
/// Inserts multiple vertices and returns the updated vertex count.
fn bulk_insert_vertices(
    vertices: Vec<gleaph_types::VertexData>,
) -> Result<u64, gleaph_types::GleaphError> {
    api::bulk_insert_vertices(vertices)
}

#[update]
/// Inserts multiple edges and returns the updated edge count.
fn bulk_insert_edges(edges: Vec<gleaph_types::EdgeData>) -> Result<u64, gleaph_types::GleaphError> {
    api::bulk_insert_edges(edges)
}

// ── ACL ──────────────────────────────────────────────────────────────────

#[update]
/// Grants or updates access for a principal. Caller must be Admin or controller.
fn set_acl_entry(
    principal: candid::Principal,
    level: gleaph_types::AccessLevel,
) -> Result<(), gleaph_types::GleaphError> {
    api::set_acl_entry(principal, level)
}

#[update]
/// Revokes access for a principal. Caller must be Admin or controller.
fn remove_acl_entry(principal: candid::Principal) -> Result<(), gleaph_types::GleaphError> {
    api::remove_acl_entry(principal)
}

#[query]
/// Lists all ACL entries. Caller must have at least Read access.
fn list_acl_entries() -> Result<Vec<gleaph_types::AclEntry>, gleaph_types::GleaphError> {
    api::list_acl_entries()
}

// ── Registry Delegation ──────────────────────────────────────────────────

#[update]
/// Sets the registry canister principal for CREATE/DROP GRAPH delegation.
fn set_registry_principal(p: candid::Principal) -> Result<(), gleaph_types::GleaphError> {
    api::set_registry_principal(p)
}

#[query]
/// Returns the currently configured registry canister principal.
fn get_registry_principal() -> Result<Option<candid::Principal>, gleaph_types::GleaphError> {
    api::get_registry_principal()
}

#[update(name = "execute_gql")]
/// Unified async GQL endpoint: handles USE GRAPH NEXT routing, CREATE/DROP GRAPH,
/// and falls back to local query/mutation execution for all other statements.
async fn execute_gql(
    gql: String,
) -> Result<gleaph_types::ExecuteGqlResult, gleaph_types::GleaphError> {
    api::execute_gql(gql).await
}

// ── Graph Alias ──────────────────────────────────────────────────────────

#[update]
/// Registers or updates a graph alias (name → canister_id). Admin-only.
fn set_graph_alias(
    name: String,
    canister_id: candid::Principal,
) -> Result<(), gleaph_types::GleaphError> {
    api::set_graph_alias(name, canister_id)
}

#[update]
/// Removes a graph alias. Returns true if the alias existed. Admin-only.
fn remove_graph_alias(name: String) -> Result<bool, gleaph_types::GleaphError> {
    api::remove_graph_alias(name)
}

#[query]
/// Lists all graph aliases. Requires Read access.
fn list_graph_aliases() -> Result<Vec<gleaph_types::GraphAlias>, gleaph_types::GleaphError> {
    api::list_graph_aliases()
}

// ── Cross-canister Forwarding ────────────────────────────────────────────

#[query(composite = true, name = "query_via")]
/// Forwards a GQL read query to a remote graph canister by alias name.
async fn query_via(
    graph_name: String,
    gql: String,
) -> Result<gleaph_types::QueryResultWithContinuation, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Read)?;
    api::query_via(graph_name, gql).await
}

#[update(name = "mutate_via")]
/// Forwards a GQL mutation to a remote graph canister by alias name.
async fn mutate_via(
    graph_name: String,
    gql: String,
) -> Result<gleaph_types::MutationResult, gleaph_types::GleaphError> {
    api::check_caller_permission(&gleaph_types::AccessLevel::Write)?;
    api::mutate_via(graph_name, gql).await
}

export_candid!();
