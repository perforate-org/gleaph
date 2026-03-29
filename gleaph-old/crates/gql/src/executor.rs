use crate::ast::{
    AggFunc, BinaryOp, CmpOp, CreateStmt, DeleteStmt, Direction, Expr, LabelExpr, MatchChain,
    MatchClause, MatchMode, MergeStmt, NodePattern, PathLength, PathMode, PatternElement,
    QueryStmt, RemoveItem, RemoveStmt, ReturnClause, ReturnItem, SetItem, SetOp, SetStmt,
    Statement, StringPredicateKind, TruthValue, UnaryOp, ValueType, WhereClause,
};
use crate::plan::{ConditionalCmpOp, ConditionalScanInfo, PhysicalPlan, PlanOp};
use crate::value::compare_values;
use gleaph_algo::{
    GraphView,
    bfs::{BfsConfig, bfs, bfs_bidirectional},
    budget::CountingBudget,
};
use gleaph_pma::{
    Memory, PmaGraph, RevEntry, reset_debug_read_counters, snapshot_debug_read_counters,
};
use gleaph_types::{
    EdgeEntry, EntityType, GleaphError, IndexType, MutationResult, PathElement,
    QueryExecutionBreakdown, QueryResult, QueryStats, TimestampRange, Value, VertexIdSet,
};
use rapidhash::fast::RapidHashMap;
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet, HashMap as StdHashMap, HashSet};
use std::hash::BuildHasher;
use std::rc::Rc;
use std::sync::Arc;

// Thread-local query parameter store.
// Set before execution via `execute_plan_with_params`; cleared afterward.
thread_local! {
    static QUERY_PARAMS: RefCell<RapidHashMap<String, Value>> = RefCell::new(RapidHashMap::default());
}

// Thread-local caller principal injection.
// Set by the graph canister before execution; cleared afterward.
// The caller() built-in function reads from this.
thread_local! {
    static CALLER_PRINCIPAL: RefCell<Option<Value>> = const { RefCell::new(None) };
}

/// Injects the caller principal for the `caller()` GQL built-in function.
pub fn set_caller(principal: Value) {
    CALLER_PRINCIPAL.with(|c| *c.borrow_mut() = Some(principal));
}

/// Clears the injected caller principal.
pub fn clear_caller() {
    CALLER_PRINCIPAL.with(|c| *c.borrow_mut() = None);
}

// Thread-local IC time injection.
// When non-zero, `temporal_now_nanos()` returns this value instead of SystemTime/0.
thread_local! {
    static CURRENT_TIME_NANOS: Cell<u64> = const { Cell::new(0) };
}

/// Injects the current IC time (nanoseconds) for use by `CURRENT_TIMESTAMP` and friends.
pub fn set_current_time(ns: u64) {
    CURRENT_TIME_NANOS.with(|c| c.set(ns));
}

/// Clears the injected IC time (reverts to default behavior).
pub fn clear_current_time() {
    CURRENT_TIME_NANOS.with(|c| c.set(0));
}

// Thread-local node type definitions for type annotation resolution.
// Maps type name → label list. Set by the graph canister before query execution.
thread_local! {
    static NODE_TYPE_DEFS: RefCell<RapidHashMap<String, Vec<String>>> = RefCell::new(RapidHashMap::default());
}

// Thread-local CHAR(n) property constraints for read-time padding.
// Maps property_name → fixed char length.  Set by the graph canister bridge
// before query execution when a graph type with CHAR(n) columns is active.
thread_local! {
    static CHAR_PAD_DEFS: RefCell<RapidHashMap<String, u32>> = RefCell::new(RapidHashMap::default());
}

/// Registers CHAR(n) property constraints for read-time space-padding.
pub fn set_char_pad_defs(defs: StdHashMap<String, u32>) {
    CHAR_PAD_DEFS.with(|d| *d.borrow_mut() = defs.into_iter().collect());
}

/// Clears CHAR(n) property constraints.
pub fn clear_char_pad_defs() {
    CHAR_PAD_DEFS.with(|d| d.borrow_mut().clear());
}

// Thread-local BINARY(n) property constraints for read-time zero-padding.
thread_local! {
    static BINARY_PAD_DEFS: RefCell<RapidHashMap<String, u32>> = RefCell::new(RapidHashMap::default());
}

/// Registers BINARY(n) property constraints for read-time zero-padding.
pub fn set_binary_pad_defs(defs: StdHashMap<String, u32>) {
    BINARY_PAD_DEFS.with(|d| *d.borrow_mut() = defs.into_iter().collect());
}

/// Clears BINARY(n) property constraints.
pub fn clear_binary_pad_defs() {
    BINARY_PAD_DEFS.with(|d| d.borrow_mut().clear());
}

// Thread-local stack of compiled WHERE clauses.
// A stack is used so nested query execution (subqueries) can safely override the active predicate.
thread_local! {
    static COMPILED_WHERE_STACK: RefCell<Vec<Option<Rc<CompiledWhereClause>>>> = const { RefCell::new(Vec::new()) };
}

/// Registers node type definitions for type annotation resolution during execution.
///
/// Called by the graph canister bridge before query execution when a graph type is active.
pub fn set_node_type_defs(defs: StdHashMap<String, Vec<String>>) {
    NODE_TYPE_DEFS.with(|d| *d.borrow_mut() = defs.into_iter().collect());
}

/// Clears registered node type definitions.
pub fn clear_node_type_defs() {
    NODE_TYPE_DEFS.with(|d| d.borrow_mut().clear());
}

/// Injects query parameters into the thread-local store (for mutation paths
/// that don't go through `execute_plan_with_params`).
pub fn set_query_params(params: StdHashMap<String, Value>) {
    QUERY_PARAMS.with(|p| *p.borrow_mut() = params.into_iter().collect());
}

/// Clears query parameters from the thread-local store.
pub fn clear_query_params() {
    QUERY_PARAMS.with(|p| p.borrow_mut().clear());
}

/// Returns a single query parameter by name, or `None` if not set.
pub fn get_query_param(name: &str) -> Option<Value> {
    QUERY_PARAMS.with(|p| p.borrow().get(name).cloned())
}

// ── VarRegistry and BindingRow: Dense Indexed Bindings ─────────────────
//
// Maps variable names to dense slot indices (built once per query from AST).
// BindingRow replaces BTreeMap<String, Binding> — zero String allocations on clone/insert.

struct VarRegistry {
    slot_to_name: Vec<String>,
    /// Pre-sorted slot indices by variable name (for RETURN */WITH * ordering).
    sorted_slots: Vec<usize>,
}

impl VarRegistry {
    fn new() -> Self {
        VarRegistry {
            slot_to_name: Vec::new(),
            sorted_slots: Vec::new(),
        }
    }

    /// Register a variable name, returning its slot index.
    /// If already registered, returns the existing slot.
    fn register(&mut self, name: &str) -> usize {
        if let Some(pos) = self.slot_to_name.iter().position(|n| n == name) {
            pos
        } else {
            let slot = self.slot_to_name.len();
            self.slot_to_name.push(name.to_string());
            slot
        }
    }

    /// Finalize: compute sorted_slots for RETURN */WITH * iteration order.
    fn finalize(&mut self) {
        let mut sorted: Vec<(usize, &str)> = self
            .slot_to_name
            .iter()
            .enumerate()
            .map(|(i, n)| (i, n.as_str()))
            .collect();
        sorted.sort_by_key(|(_, name)| *name);
        self.sorted_slots = sorted.into_iter().map(|(i, _)| i).collect();
    }

    fn slot_count(&self) -> usize {
        self.slot_to_name.len()
    }

    /// Look up a variable name's slot index. For typical registries (5-15 vars),
    /// linear scan is faster than HashMap due to no hashing overhead.
    fn slot_opt(&self, name: &str) -> Option<usize> {
        self.slot_to_name.iter().position(|n| n == name)
    }
}

thread_local! {
    static VAR_REGISTRY_STACK: RefCell<Vec<Rc<VarRegistry>>> = const { RefCell::new(Vec::new()) };
}

fn push_registry(reg: VarRegistry) {
    VAR_REGISTRY_STACK.with(|s| s.borrow_mut().push(Rc::new(reg)));
}

fn pop_registry() {
    VAR_REGISTRY_STACK.with(|s| {
        s.borrow_mut().pop();
    });
}

/// Get the current registry as an Rc (one TLS access, then all future ops go through the Rc).
fn current_registry_rc() -> Rc<VarRegistry> {
    VAR_REGISTRY_STACK.with(|s| {
        s.borrow()
            .last()
            .expect("VarRegistry stack is empty")
            .clone()
    })
}

fn registry_is_active() -> bool {
    VAR_REGISTRY_STACK.with(|s| !s.borrow().is_empty())
}

/// RAII guard that pops the registry on drop.
struct RegistryGuard;
impl Drop for RegistryGuard {
    fn drop(&mut self) {
        pop_registry();
    }
}

/// Push a registry for `stmt` if none is active yet. Returns a guard that pops on drop.
fn ensure_registry(stmt: &Statement) -> Option<RegistryGuard> {
    if registry_is_active() {
        return None;
    }
    push_registry(build_var_registry(stmt));
    Some(RegistryGuard)
}

/// Push a registry for a query if none is active yet. Returns a guard that pops on drop.
fn ensure_registry_for_query(q: &QueryStmt) -> Option<RegistryGuard> {
    if registry_is_active() {
        return None;
    }
    push_registry(build_var_registry_from_query(q));
    Some(RegistryGuard)
}

/// The runtime binding row: a dense `Vec<Option<Binding>>` indexed by slot IDs.
///
/// Replaces `BTreeMap<String, Binding>` — zero String allocations on clone/insert.
/// Stores an `Rc<VarRegistry>` so that get/insert/keys bypass TLS on every call.
/// TLS is accessed only once, in `BindingRow::new()`.
#[derive(Clone)]
struct BindingRow {
    slots: Vec<Option<Binding>>,
    reg: Rc<VarRegistry>,
}

impl std::fmt::Debug for BindingRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(
                self.reg
                    .sorted_slots
                    .iter()
                    .filter(|&&s| s < self.slots.len() && self.slots[s].is_some())
                    .map(|&s| (&self.reg.slot_to_name[s], self.slots[s].as_ref().unwrap())),
            )
            .finish()
    }
}

impl BindingRow {
    fn new() -> Self {
        let reg = current_registry_rc();
        let cap = reg.slot_count();
        BindingRow {
            slots: vec![None; cap],
            reg,
        }
    }

    fn get(&self, name: &str) -> Option<&Binding> {
        self.reg
            .slot_opt(name)
            .and_then(|s| self.slots.get(s).and_then(|o| o.as_ref()))
    }

    fn insert(&mut self, name: String, val: Binding) {
        if let Some(s) = self.reg.slot_opt(&name)
            && s < self.slots.len()
        {
            self.slots[s] = Some(val);
        }
    }

    fn contains_key(&self, name: &str) -> bool {
        self.reg
            .slot_opt(name)
            .is_some_and(|s| s < self.slots.len() && self.slots[s].is_some())
    }

    fn remove(&mut self, name: &str) {
        if let Some(s) = self.reg.slot_opt(name)
            && s < self.slots.len()
        {
            self.slots[s] = None;
        }
    }

    fn is_empty(&self) -> bool {
        self.slots.iter().all(|opt| opt.is_none())
    }

    /// Iterate all non-None bindings (values only).
    fn values(&self) -> impl Iterator<Item = &Binding> {
        self.slots.iter().filter_map(|opt| opt.as_ref())
    }

    /// Return sorted variable names that have a non-None binding.
    /// Order matches BTreeMap's lexicographic iteration.
    fn keys(&self) -> Vec<String> {
        self.reg
            .sorted_slots
            .iter()
            .filter(|&&s| s < self.slots.len() && self.slots[s].is_some())
            .map(|&s| self.reg.slot_to_name[s].clone())
            .collect()
    }

    // ── Fast-path method with pre-resolved slot ──

    fn set_slot(&mut self, slot: usize, val: Binding) {
        if slot < self.slots.len() {
            self.slots[slot] = Some(val);
        }
    }
}

impl FromIterator<(String, Binding)> for BindingRow {
    fn from_iter<I: IntoIterator<Item = (String, Binding)>>(iter: I) -> Self {
        let mut row = BindingRow::new();
        for (name, val) in iter {
            row.insert(name, val);
        }
        row
    }
}

/// Extended mutation result that includes affected vertex IDs for incremental
/// secondary index maintenance.
pub struct MutationOutcome {
    pub result: MutationResult,
    pub affected_vertex_ids: Vec<u32>,
}

/// Progress of a resumable mutation.
pub enum MutationProgress {
    Done(MutationOutcome),
    Suspended {
        partial: MutationOutcome,
        checkpoint: gleaph_types::MutationCheckpoint,
    },
}

/// A projected result row: one `Value` per column in the RETURN clause.
pub type Row = Vec<Value>;

/// Streaming interface for result rows.
pub trait RowIterator {
    /// Returns the next result row, or `Ok(None)` when exhausted.
    fn next_row(&mut self) -> Result<Option<Row>, GleaphError>;
}

/// A no-op [`RowIterator`] that immediately signals end-of-stream.
#[derive(Default)]
pub struct EmptyExecutor;

impl RowIterator for EmptyExecutor {
    fn next_row(&mut self) -> Result<Option<Row>, GleaphError> {
        Ok(None)
    }
}

/// The runtime value of a variable bound by a MATCH clause.
#[derive(Clone, Debug)]
enum Binding {
    /// A graph vertex identified by its numeric ID.
    Vertex(u32),
    /// A graph edge identified by source, destination, optional label, and stable edge id.
    ///
    /// `edge_id` is the monotonically-assigned PMA identifier (non-zero for PMA-stored edges;
    /// `0` for overlay-only edges that pre-date the edge-id scheme).
    ///
    /// `weight` and `timestamp` are cached from `EdgeEntry` at bind time so that
    /// property lookups for these hot fields avoid the O(degree) `edge_record()` call.
    Edge {
        src: u32,
        dst: u32,
        label: Option<Arc<str>>,
        edge_id: u32,
        weight: f32,
        timestamp: u64,
    },
    Value(Value),
}

/// Map from variable name to its current binding in one result row.
/// Dense indexed: variable names are resolved to slot indices at query start.
type Bindings = BindingRow;
const MAX_GROUPS: usize = 10_000;

thread_local! {
    static MAX_GROUPS_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
}

#[inline]
fn effective_max_groups() -> usize {
    MAX_GROUPS_OVERRIDE.with(|v| v.get()).unwrap_or(MAX_GROUPS)
}

/// Overrides the aggregate group cap for the current thread.
///
/// Passing `None` resets to the default cap (`MAX_GROUPS`).
pub fn set_max_groups_override(max_groups: Option<usize>) {
    MAX_GROUPS_OVERRIDE.with(|v| v.set(max_groups.filter(|m| *m > 0)));
}

struct ReverseView<'a, G>(&'a G);

/// Undirected view: BFS discovers neighbors in both forward and reverse directions.
struct BothWaysView<'a, G>(&'a G);

impl<G: GraphView> GraphView for BothWaysView<'_, G> {
    fn vertex_count(&self) -> u64 {
        self.0.vertex_count()
    }
    fn edge_count(&self) -> u64 {
        self.0.edge_count()
    }
    fn neighbors(&self, vertex_id: u32) -> Vec<gleaph_algo::Neighbor> {
        let mut n = self.0.neighbors(vertex_id);
        n.extend(self.0.reverse_neighbors(vertex_id));
        n
    }
    fn neighbors_filtered(
        &self,
        vertex_id: u32,
        ts_range: Option<gleaph_types::TimestampRange>,
    ) -> Vec<gleaph_algo::Neighbor> {
        let fwd = self.0.neighbors_filtered(vertex_id, ts_range.clone());
        let mut rev = self
            .0
            .reverse_neighbors(vertex_id)
            .into_iter()
            .filter(|(_, _, ts)| {
                let r = ts_range.as_ref();
                r.is_none_or(|r| r.start.is_none_or(|s| *ts >= s) && r.end.is_none_or(|e| *ts <= e))
            })
            .collect::<Vec<_>>();
        let mut all = fwd;
        all.append(&mut rev);
        all
    }
    fn reverse_neighbors(&self, target: u32) -> Vec<gleaph_algo::Neighbor> {
        let mut n = self.0.reverse_neighbors(target);
        n.extend(self.0.neighbors(target));
        n
    }
    fn is_vertex_active(&self, vertex_id: u32) -> bool {
        self.0.is_vertex_active(vertex_id)
    }
    fn vertex_has_label(&self, vertex_id: u32, label: &str) -> bool {
        self.0.vertex_has_label(vertex_id, label)
    }
    fn edge_has_label(&self, src: u32, dst: u32, label: &str) -> bool {
        // Either direction qualifies
        self.0.edge_has_label(src, dst, label) || self.0.edge_has_label(dst, src, label)
    }
    fn edge_label_ref(&self, src: u32, dst: u32) -> Option<&str> {
        self.0
            .edge_label_ref(src, dst)
            .or_else(|| self.0.edge_label_ref(dst, src))
    }
    fn label_name_by_id(&self, label_id: u32) -> Option<&str> {
        self.0.label_name_by_id(label_id)
    }
    fn all_vertices(&self) -> Vec<u32> {
        self.0.all_vertices()
    }
}

impl<G: GraphView> GraphView for ReverseView<'_, G> {
    fn vertex_count(&self) -> u64 {
        self.0.vertex_count()
    }
    fn edge_count(&self) -> u64 {
        self.0.edge_count()
    }
    fn neighbors(&self, vertex_id: u32) -> Vec<gleaph_algo::Neighbor> {
        self.0.reverse_neighbors(vertex_id)
    }
    fn neighbors_filtered(
        &self,
        vertex_id: u32,
        ts_range: Option<gleaph_types::TimestampRange>,
    ) -> Vec<gleaph_algo::Neighbor> {
        // `bfs` only uses `neighbors_filtered` + `edge_has_label`; reverse traversal uses reverse neighbors.
        self.0
            .reverse_neighbors(vertex_id)
            .into_iter()
            .filter(|(_, _, ts)| {
                let r = ts_range.as_ref();
                r.is_none_or(|r| r.start.is_none_or(|s| *ts >= s) && r.end.is_none_or(|e| *ts <= e))
            })
            .collect()
    }
    fn reverse_neighbors(&self, target: u32) -> Vec<gleaph_algo::Neighbor> {
        self.0.neighbors(target)
    }
    fn is_vertex_active(&self, vertex_id: u32) -> bool {
        self.0.is_vertex_active(vertex_id)
    }
    fn vertex_has_label(&self, vertex_id: u32, label: &str) -> bool {
        self.0.vertex_has_label(vertex_id, label)
    }
    fn edge_has_label(&self, src: u32, dst: u32, label: &str) -> bool {
        self.0.edge_has_label(dst, src, label)
    }
    fn edge_label_ref(&self, src: u32, dst: u32) -> Option<&str> {
        self.0.edge_label_ref(dst, src)
    }
    fn label_name_by_id(&self, label_id: u32) -> Option<&str> {
        self.0.label_name_by_id(label_id)
    }
    fn all_vertices(&self) -> Vec<u32> {
        self.0.all_vertices()
    }
}

/// Optional hard limits applied during query execution to protect against
/// runaway queries.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecutionLimits {
    /// Maximum number of result rows before returning an error.
    pub max_rows: Option<usize>,
    /// Maximum number of internal execution steps before returning an error.
    pub max_execution_steps: Option<u64>,
}

fn merge_breakdown(dst: &mut QueryExecutionBreakdown, src: &QueryExecutionBreakdown) {
    dst.index_fast_path_attempted |= src.index_fast_path_attempted;
    dst.index_fast_path_used |= src.index_fast_path_used;
    dst.aggregate_fast_path_attempted |= src.aggregate_fast_path_attempted;
    dst.aggregate_fast_path_used |= src.aggregate_fast_path_used;
    dst.aggregate_compiled_fast_path_used |= src.aggregate_compiled_fast_path_used;
    dst.shortest_fast_path_attempted |= src.shortest_fast_path_attempted;
    dst.shortest_fast_path_used |= src.shortest_fast_path_used;
    dst.recent_two_hop_projection_fast_path_used |= src.recent_two_hop_projection_fast_path_used;
    dst.var_len_terminal_projection_fast_path_used |=
        src.var_len_terminal_projection_fast_path_used;
    dst.two_hop_top_k_count_fast_path_used |= src.two_hop_top_k_count_fast_path_used;
    dst.rows_after_match = dst.rows_after_match.saturating_add(src.rows_after_match);
    dst.rows_after_with = dst.rows_after_with.saturating_add(src.rows_after_with);
    dst.rows_before_projection = dst
        .rows_before_projection
        .saturating_add(src.rows_before_projection);
    dst.groups_formed = dst.groups_formed.saturating_add(src.groups_formed);
    dst.top_k_calls = dst.top_k_calls.saturating_add(src.top_k_calls);
    dst.full_sort_calls = dst.full_sort_calls.saturating_add(src.full_sort_calls);
    dst.limit_truncate_calls = dst
        .limit_truncate_calls
        .saturating_add(src.limit_truncate_calls);
    dst.selectivity_refresh_ran |= src.selectivity_refresh_ran;
    dst.edge_label_calls = dst.edge_label_calls.saturating_add(src.edge_label_calls);
    dst.edge_record_calls = dst.edge_record_calls.saturating_add(src.edge_record_calls);
    dst.is_edge_tombstoned_calls = dst
        .is_edge_tombstoned_calls
        .saturating_add(src.is_edge_tombstoned_calls);
    dst.reverse_neighbor_callbacks = dst
        .reverse_neighbor_callbacks
        .saturating_add(src.reverse_neighbor_callbacks);
    dst.var_len_dfs_calls = dst.var_len_dfs_calls.saturating_add(src.var_len_dfs_calls);
    dst.compiled_match_records = dst
        .compiled_match_records
        .saturating_add(src.compiled_match_records);
    dst.var_len_binding_clones = dst
        .var_len_binding_clones
        .saturating_add(src.var_len_binding_clones);
    dst.var_len_path_contains_checks = dst
        .var_len_path_contains_checks
        .saturating_add(src.var_len_path_contains_checks);
    dst.var_len_node_match_checks = dst
        .var_len_node_match_checks
        .saturating_add(src.var_len_node_match_checks);
    dst.reverse_row_clones = dst.reverse_row_clones.saturating_add(src.reverse_row_clones);
    dst.reverse_node_match_checks = dst
        .reverse_node_match_checks
        .saturating_add(src.reverse_node_match_checks);
    dst.compiled_group_key_evals = dst
        .compiled_group_key_evals
        .saturating_add(src.compiled_group_key_evals);
    dst.compiled_group_bucket_probes = dst
        .compiled_group_bucket_probes
        .saturating_add(src.compiled_group_bucket_probes);
    dst.compiled_agg_updates = dst
        .compiled_agg_updates
        .saturating_add(src.compiled_agg_updates);
    dst.compiled_projection_fast_calls = dst
        .compiled_projection_fast_calls
        .saturating_add(src.compiled_projection_fast_calls);
    dst.compiled_projection_input_rows = dst
        .compiled_projection_input_rows
        .saturating_add(src.compiled_projection_input_rows);
    dst.compiled_projection_empty_returns = dst
        .compiled_projection_empty_returns
        .saturating_add(src.compiled_projection_empty_returns);
    dst.with_continuation_match_calls = dst
        .with_continuation_match_calls
        .saturating_add(src.with_continuation_match_calls);
    dst.with_continuation_match_input_rows = dst
        .with_continuation_match_input_rows
        .saturating_add(src.with_continuation_match_input_rows);
    dst.with_continuation_match_output_rows = dst
        .with_continuation_match_output_rows
        .saturating_add(src.with_continuation_match_output_rows);
    dst.joined_match_start_candidates = dst
        .joined_match_start_candidates
        .saturating_add(src.joined_match_start_candidates);
    dst.joined_match_local_rows_before_inline_where = dst
        .joined_match_local_rows_before_inline_where
        .saturating_add(src.joined_match_local_rows_before_inline_where);
    dst.joined_match_local_rows_after_inline_where = dst
        .joined_match_local_rows_after_inline_where
        .saturating_add(src.joined_match_local_rows_after_inline_where);
    dst.with_continuation_joined_match_start_candidates = dst
        .with_continuation_joined_match_start_candidates
        .saturating_add(src.with_continuation_joined_match_start_candidates);
    dst.with_continuation_joined_local_rows_before_inline_where = dst
        .with_continuation_joined_local_rows_before_inline_where
        .saturating_add(src.with_continuation_joined_local_rows_before_inline_where);
    dst.with_continuation_joined_local_rows_after_inline_where = dst
        .with_continuation_joined_local_rows_after_inline_where
        .saturating_add(src.with_continuation_joined_local_rows_after_inline_where);
    dst.with_continuation_scanned_edges = dst
        .with_continuation_scanned_edges
        .saturating_add(src.with_continuation_scanned_edges);
    dst.with_continuation_execution_steps = dst
        .with_continuation_execution_steps
        .saturating_add(src.with_continuation_execution_steps);
    dst.outgoing_hop_candidates = dst
        .outgoing_hop_candidates
        .saturating_add(src.outgoing_hop_candidates);
    dst.incoming_hop_candidates = dst
        .incoming_hop_candidates
        .saturating_add(src.incoming_hop_candidates);
    dst.hop_label_rejects = dst.hop_label_rejects.saturating_add(src.hop_label_rejects);
    dst.outgoing_hop_label_rejects = dst
        .outgoing_hop_label_rejects
        .saturating_add(src.outgoing_hop_label_rejects);
    dst.incoming_hop_label_rejects = dst
        .incoming_hop_label_rejects
        .saturating_add(src.incoming_hop_label_rejects);
    dst.hop_node_rejects = dst.hop_node_rejects.saturating_add(src.hop_node_rejects);
    dst.hop_edge_property_rejects = dst
        .hop_edge_property_rejects
        .saturating_add(src.hop_edge_property_rejects);
    dst.hop_where_pushdown_rejects = dst
        .hop_where_pushdown_rejects
        .saturating_add(src.hop_where_pushdown_rejects);
    dst.var_len_cycle_rejects = dst
        .var_len_cycle_rejects
        .saturating_add(src.var_len_cycle_rejects);
    dst.with_continuation_hop_label_rejects = dst
        .with_continuation_hop_label_rejects
        .saturating_add(src.with_continuation_hop_label_rejects);
    dst.with_continuation_hop_node_rejects = dst
        .with_continuation_hop_node_rejects
        .saturating_add(src.with_continuation_hop_node_rejects);
    dst.with_continuation_hop_edge_property_rejects = dst
        .with_continuation_hop_edge_property_rejects
        .saturating_add(src.with_continuation_hop_edge_property_rejects);
    dst.with_continuation_hop_where_pushdown_rejects = dst
        .with_continuation_hop_where_pushdown_rejects
        .saturating_add(src.with_continuation_hop_where_pushdown_rejects);
    dst.with_continuation_var_len_cycle_rejects = dst
        .with_continuation_var_len_cycle_rejects
        .saturating_add(src.with_continuation_var_len_cycle_rejects);
    dst.with_continuation_outgoing_hop_candidates = dst
        .with_continuation_outgoing_hop_candidates
        .saturating_add(src.with_continuation_outgoing_hop_candidates);
    dst.with_continuation_incoming_hop_candidates = dst
        .with_continuation_incoming_hop_candidates
        .saturating_add(src.with_continuation_incoming_hop_candidates);
    dst.with_continuation_outgoing_hop_label_rejects = dst
        .with_continuation_outgoing_hop_label_rejects
        .saturating_add(src.with_continuation_outgoing_hop_label_rejects);
    dst.with_continuation_incoming_hop_label_rejects = dst
        .with_continuation_incoming_hop_label_rejects
        .saturating_add(src.with_continuation_incoming_hop_label_rejects);
}

fn attach_debug_read_counters(result: &mut QueryResult) {
    let counters = snapshot_debug_read_counters();
    result.stats.breakdown.edge_label_calls = counters.edge_label_calls;
    result.stats.breakdown.edge_record_calls = counters.edge_record_calls;
    result.stats.breakdown.is_edge_tombstoned_calls = counters.is_edge_tombstoned_calls;
}

fn merge_query_stats(dst: &mut QueryStats, src: &QueryStats) {
    dst.scanned_vertices = dst.scanned_vertices.saturating_add(src.scanned_vertices);
    dst.scanned_edges = dst.scanned_edges.saturating_add(src.scanned_edges);
    dst.execution_steps = dst.execution_steps.saturating_add(src.execution_steps);
    merge_breakdown(&mut dst.breakdown, &src.breakdown);
}

// ── AST Variable Discovery ─────────────────────────────────────────────

fn build_var_registry_from_query(q: &QueryStmt) -> VarRegistry {
    let mut reg = VarRegistry::new();
    collect_vars_query(q, &mut reg);
    reg.register(INTERNAL_PATH_VAR);
    reg.finalize();
    reg
}

fn build_var_registry(stmt: &Statement) -> VarRegistry {
    let mut reg = VarRegistry::new();
    collect_vars_stmt(stmt, &mut reg);
    reg.register(INTERNAL_PATH_VAR);
    reg.finalize();
    reg
}

fn collect_vars_stmt(stmt: &Statement, reg: &mut VarRegistry) {
    match stmt {
        Statement::Query(q) => collect_vars_query(q, reg),
        Statement::Compound { left, right, .. } => {
            collect_vars_stmt(left, reg);
            collect_vars_stmt(right, reg);
        }
        Statement::Create(cs) => {
            for c in cs {
                collect_vars_create(c, reg);
            }
        }
        Statement::Merge(m) => {
            collect_vars_create(&m.create, reg);
            for item in &m.on_create_set {
                collect_vars_set_item(item, reg);
            }
            for item in &m.on_match_set {
                collect_vars_set_item(item, reg);
            }
        }
        Statement::Delete(d) => {
            collect_vars_match_clause(&d.match_clause, reg);
            for var in &d.target_vars {
                reg.register(var);
            }
            if let Some(w) = &d.where_clause {
                collect_vars_expr(w, reg);
            }
        }
        Statement::Set(s) => {
            collect_vars_match_clause(&s.match_clause, reg);
            if let Some(w) = &s.where_clause {
                collect_vars_expr(w, reg);
            }
            for item in &s.set_clause.items {
                collect_vars_set_item(item, reg);
            }
        }
        Statement::Remove(r) => {
            collect_vars_match_clause(&r.match_clause, reg);
            if let Some(w) = &r.where_clause {
                collect_vars_expr(w, reg);
            }
        }
        Statement::Let(l) => {
            collect_vars_match_clause(&l.match_clause, reg);
            if let Some(w) = &l.where_clause {
                collect_vars_expr(w, reg);
            }
            for (var, expr) in &l.bindings {
                reg.register(var);
                collect_vars_expr(expr, reg);
            }
            collect_vars_return_clause(&l.return_clause, reg);
        }
        Statement::Filter(f) => {
            collect_vars_match_clause(&f.match_clause, reg);
            if let Some(w) = &f.where_clause {
                collect_vars_expr(w, reg);
            }
            collect_vars_expr(&f.filter_expr, reg);
        }
        Statement::For(f) => {
            reg.register(&f.var);
            if let Some(ord) = &f.ordinality_var {
                reg.register(ord);
            }
            collect_vars_expr(&f.list_expr, reg);
            collect_vars_return_clause(&f.return_clause, reg);
        }
        Statement::Call(c) => {
            for v in &c.scope_vars {
                reg.register(v);
            }
            collect_vars_stmt(&c.body, reg);
        }
        Statement::Finish
        | Statement::UseGraph(_)
        | Statement::CreateGraph { .. }
        | Statement::DropGraph { .. }
        | Statement::CreateGraphType { .. }
        | Statement::DropGraphType { .. }
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. }
        | Statement::DescribeGraphType(_)
        | Statement::CreateIndex { .. }
        | Statement::DropIndex { .. }
        | Statement::Show(_)
        | Statement::Grant { .. }
        | Statement::Revoke { .. }
        | Statement::Analyze
        | Statement::CallProcedure(_)
        | Statement::SetTypeCheck(_)
        | Statement::CreateConstraint(_)
        | Statement::DropConstraint(_) => {}
    }
}

fn collect_vars_query(q: &QueryStmt, reg: &mut VarRegistry) {
    for entry in &q.match_clauses {
        if let Some(pv) = &entry.path_variable {
            reg.register(pv);
        }
        collect_vars_match_clause(&entry.pattern, reg);
    }
    if let Some(w) = &q.where_clause {
        collect_vars_expr(w, reg);
    }
    collect_vars_return_clause(&q.return_clause, reg);
    for w in &q.with_clauses {
        for item in &w.items {
            // Register alias or inferred name (used as new binding name after WITH projection).
            if let Some(alias) = &item.alias {
                reg.register(alias);
            } else {
                match &item.expr {
                    Expr::Variable(v) => {
                        reg.register(v);
                    }
                    Expr::PropertyAccess { target, property } => {
                        if let Expr::Variable(v) = target.as_ref() {
                            reg.register(&format!("{v}.{property}"));
                        } else {
                            reg.register(property);
                        }
                    }
                    _ => {}
                }
            }
            collect_vars_expr(&item.expr, reg);
        }
        if let Some(w_expr) = &w.where_clause {
            collect_vars_expr(w_expr, reg);
        }
        if let Some(pw) = &w.post_match_where {
            collect_vars_expr(pw, reg);
        }
        for entry in &w.match_clauses {
            if let Some(pv) = &entry.path_variable {
                reg.register(pv);
            }
            collect_vars_match_clause(&entry.pattern, reg);
        }
        if let Some(ob) = &w.order_by {
            for item in &ob.items {
                collect_vars_expr(&item.expr, reg);
            }
        }
    }
    if let Some(gb) = &q.group_by {
        for e in gb {
            collect_vars_expr(e, reg);
        }
    }
    if let Some(h) = &q.having {
        collect_vars_expr(h, reg);
    }
    if let Some(ob) = &q.order_by {
        for item in &ob.items {
            collect_vars_expr(&item.expr, reg);
        }
    }
}

fn collect_vars_match_clause(m: &MatchClause, reg: &mut VarRegistry) {
    if let Some(v) = &m.start.var {
        reg.register(v);
    }
    if let Some(w) = &m.start.where_clause {
        collect_vars_expr(w, reg);
    }
    collect_vars_elements(&m.elements, reg);
}

fn collect_vars_elements(elements: &[PatternElement], reg: &mut VarRegistry) {
    for elem in elements {
        match elem {
            PatternElement::Hop(chain) => {
                if let Some(v) = &chain.edge.var {
                    reg.register(v);
                }
                if let Some(v) = &chain.node.var {
                    reg.register(v);
                }
                if let Some(w) = &chain.edge.where_clause {
                    collect_vars_expr(w, reg);
                }
                if let Some(w) = &chain.node.where_clause {
                    collect_vars_expr(w, reg);
                }
            }
            PatternElement::SubPath {
                inner_start,
                inner_elements,
                trailing_node,
                var,
                ..
            } => {
                // Register internal path variable for subpath path accumulation
                // (enables path mode filtering across repetition boundaries).
                reg.register("__subpath_path__");
                if let Some(v) = &inner_start.var {
                    reg.register(v);
                }
                if let Some(w) = &inner_start.where_clause {
                    collect_vars_expr(w, reg);
                }
                if let Some(v) = var {
                    reg.register(v);
                }
                collect_vars_elements(inner_elements, reg);
                if let Some(tn) = trailing_node {
                    if let Some(v) = &tn.var {
                        reg.register(v);
                    }
                    if let Some(w) = &tn.where_clause {
                        collect_vars_expr(w, reg);
                    }
                }
            }
        }
    }
}

fn collect_vars_return_clause(ret: &ReturnClause, reg: &mut VarRegistry) {
    for item in &ret.items {
        // Register alias or inferred column name (used in NEXT pipeline, ORDER BY on aggs, etc.)
        if let Some(alias) = &item.alias {
            reg.register(alias);
        } else {
            match &item.expr {
                Expr::Variable(v) => {
                    reg.register(v.as_str());
                }
                Expr::PropertyAccess { target, property } => {
                    if let Expr::Variable(v) = target.as_ref() {
                        reg.register(&format!("{v}.{property}"));
                    } else {
                        reg.register(property.as_str());
                    }
                }
                Expr::Aggregate(_) => {
                    reg.register("aggregate");
                }
                _ => {
                    reg.register("expr");
                }
            }
        }
        collect_vars_expr(&item.expr, reg);
    }
}

fn collect_vars_create(c: &CreateStmt, reg: &mut VarRegistry) {
    match c {
        CreateStmt::Node(n) => {
            if let Some(v) = &n.node.var {
                reg.register(v);
            }
        }
        CreateStmt::Edge(e) => {
            if let Some(v) = &e.left.var {
                reg.register(v);
            }
            if let Some(v) = &e.edge.var {
                reg.register(v);
            }
            if let Some(v) = &e.right.var {
                reg.register(v);
            }
        }
    }
}

fn collect_vars_set_item(item: &SetItem, reg: &mut VarRegistry) {
    match item {
        SetItem::Property { var, value, .. } => {
            reg.register(var);
            collect_vars_expr(value, reg);
        }
        SetItem::AllProperties { var, properties } => {
            reg.register(var);
            for (_, value) in properties {
                collect_vars_expr(value, reg);
            }
        }
        SetItem::Label { var, .. } => {
            reg.register(var);
        }
    }
}

fn collect_vars_expr(expr: &Expr, reg: &mut VarRegistry) {
    match expr {
        Expr::Variable(v) | Expr::PathVar(v) => {
            reg.register(v);
        }
        Expr::Parameter { .. } => {
            // §21.3: parameters are in a separate namespace; do not register as graph variables.
        }
        Expr::Literal(_) => {}
        Expr::PropertyAccess { target, .. } => collect_vars_expr(target, reg),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right) => {
            collect_vars_expr(left, reg);
            collect_vars_expr(right, reg);
        }
        Expr::UnaryOp { expr: e, .. }
        | Expr::Not(e)
        | Expr::IsNull(e)
        | Expr::IsNotNull(e)
        | Expr::PathLength(e)
        | Expr::Cast { expr: e, .. }
        | Expr::IsTruth { expr: e, .. }
        | Expr::IsLabeled { expr: e, .. }
        | Expr::IsType { expr: e, .. }
        | Expr::IsDirected { expr: e, .. }
        | Expr::PropertyExists { target: e, .. } => {
            collect_vars_expr(e, reg);
        }
        Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
            collect_vars_expr(node, reg);
            collect_vars_expr(edge, reg);
        }
        Expr::InList { expr, list, .. } => {
            collect_vars_expr(expr, reg);
            for e in list {
                collect_vars_expr(e, reg);
            }
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            collect_vars_expr(expr, reg);
            collect_vars_expr(pattern, reg);
        }
        Expr::Case(c) => {
            if let Some(op) = &c.operand {
                collect_vars_expr(op, reg);
            }
            for wt in &c.when_then {
                collect_vars_expr(&wt.when, reg);
                collect_vars_expr(&wt.then, reg);
            }
            if let Some(e) = &c.else_expr {
                collect_vars_expr(e, reg);
            }
        }
        Expr::Coalesce(items)
        | Expr::ListLiteral(items)
        | Expr::AllDifferent(items)
        | Expr::Same(items)
        | Expr::PathConstructor(items) => {
            for e in items {
                collect_vars_expr(e, reg);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for e in args {
                collect_vars_expr(e, reg);
            }
        }
        Expr::Aggregate(agg) => {
            if let Some(e) = &agg.expr {
                collect_vars_expr(e, reg);
            }
        }
        Expr::Exists(stmt) => collect_vars_stmt(stmt, reg),
        Expr::ValueSubquery(stmt) => collect_vars_stmt(stmt, reg),
        Expr::RecordLiteral(pairs) => {
            for (_, e) in pairs {
                collect_vars_expr(e, reg);
            }
        }
        Expr::LetIn { bindings, body } => {
            for (name, e) in bindings {
                reg.register(name);
                collect_vars_expr(e, reg);
            }
            collect_vars_expr(body, reg);
        }
    }
}

/// Executes a [`PhysicalPlan`] against `graph` with no execution limits.
///
/// This is a convenience wrapper around [`execute_plan_with_limits`].
pub fn execute_plan<M: Memory + Clone>(
    plan: &PhysicalPlan,
    graph: &PmaGraph<M>,
) -> Result<QueryResult, GleaphError> {
    execute_plan_with_limits(plan, graph, ExecutionLimits::default())
}

/// Executes a [`PhysicalPlan`] with query parameters injected.
///
/// Parameters are keyed by their name (without the `$` prefix). They can be
/// referenced in a GQL query as `$name` and are resolved during expression
/// evaluation (before graph pattern matching bindings take precedence).
pub fn execute_plan_with_params<M: Memory + Clone>(
    plan: &PhysicalPlan,
    graph: &PmaGraph<M>,
    params: &StdHashMap<String, Value>,
    limits: ExecutionLimits,
) -> Result<QueryResult, GleaphError> {
    execute_plan_with_params_and_hasher(plan, graph, params, limits, &RandomState::new())
}

/// Like [`execute_plan_with_params`] but uses a custom [`BuildHasher`] for
/// aggregation group-key hashing.  Pass `&rapidhash::fast::RandomState::default()`
/// (re-exported as `gleaph_pma::RapidBuildHasher`) for faster hashing on IC.
pub fn execute_plan_with_params_and_hasher<M: Memory + Clone, S: BuildHasher>(
    plan: &PhysicalPlan,
    graph: &PmaGraph<M>,
    params: &StdHashMap<String, Value>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<QueryResult, GleaphError> {
    // §21.3: Pre-check that all referenced parameters are present in the params map.
    if let Some(query) = &plan.query {
        let mut required = BTreeSet::new();
        collect_param_names_from_query(query, &mut required);
        for name in &required {
            if !params.contains_key(name.as_str()) {
                return Err(GleaphError::ValidationError(format!(
                    "undefined parameter '${name}'"
                )));
            }
        }
    }
    QUERY_PARAMS.with(|p| *p.borrow_mut() = params.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
    let result = execute_plan_with_limits_and_hasher(plan, graph, limits, build_hasher);
    QUERY_PARAMS.with(|p| p.borrow_mut().clear());
    result
}

/// Executes a [`PhysicalPlan`] against `graph`, aborting early if `limits` are
/// exceeded.
pub fn execute_plan_with_limits<M: Memory + Clone>(
    plan: &PhysicalPlan,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<QueryResult, GleaphError> {
    execute_plan_with_limits_and_hasher(plan, graph, limits, &RandomState::new())
}

/// Like [`execute_plan_with_limits`] but uses a custom [`BuildHasher`] for
/// aggregation group-key hashing.
pub fn execute_plan_with_limits_and_hasher<M: Memory + Clone, S: BuildHasher>(
    plan: &PhysicalPlan,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<QueryResult, GleaphError> {
    reset_debug_read_counters();
    if let (Some(cap), Some(est)) = (
        limits.max_execution_steps,
        plan.annotations.estimated_instructions,
    ) && est > cap as f64
    {
        return Err(GleaphError::ExecutionError(format!(
            "estimated execution steps {:.0} exceed hard cap {}",
            est, cap
        )));
    }
    let query = plan.query.as_ref().ok_or_else(|| {
        GleaphError::ExecutionError("physical plan is missing query payload".into())
    })?;
    let _reg_guard = ensure_registry_for_query(query);
    let _compiled_where_guard = push_compiled_where_scope(query.where_clause.as_ref());
    let index_planned = plan.ops.iter().any(|op| matches!(op, PlanOp::IndexScan));
    let edge_index_planned = plan
        .ops
        .iter()
        .any(|op| matches!(op, PlanOp::EdgeIndexScan));
    let aggregate_planned = plan.ops.iter().any(|op| matches!(op, PlanOp::Aggregate));
    let shortest_planned = plan.ops.iter().any(|op| matches!(op, PlanOp::ShortestPath));
    if let Some(mut r) =
        execute_two_hop_top_k_count_by_terminal_key_query(query, graph, limits, build_hasher)?
    {
        r.stats.breakdown.aggregate_fast_path_attempted = true;
        r.stats.breakdown.aggregate_fast_path_used = true;
        if index_planned {
            r.stats.breakdown.index_fast_path_attempted = true;
        }
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if let Some(mut r) = execute_two_hop_count_by_middle_vertex_query(query, graph, limits)? {
        r.stats.breakdown.aggregate_fast_path_attempted = true;
        r.stats.breakdown.aggregate_fast_path_used = true;
        if index_planned {
            r.stats.breakdown.index_fast_path_attempted = true;
        }
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if let Some(mut r) =
        execute_seeded_top_k_count_by_terminal_key_query(query, graph, limits, build_hasher)?
    {
        r.stats.breakdown.aggregate_fast_path_attempted = true;
        r.stats.breakdown.aggregate_fast_path_used = true;
        if index_planned {
            r.stats.breakdown.index_fast_path_attempted = true;
        }
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if let Some(mut r) = execute_seeded_segmentation_query(query, graph, limits, build_hasher)? {
        r.stats.breakdown.aggregate_fast_path_attempted = true;
        r.stats.breakdown.aggregate_fast_path_used = true;
        if index_planned {
            r.stats.breakdown.index_fast_path_attempted = true;
        }
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if let Some(mut r) =
        execute_seeded_verified_influence_query(query, graph, limits, build_hasher)?
    {
        r.stats.breakdown.aggregate_fast_path_attempted = true;
        r.stats.breakdown.aggregate_fast_path_used = true;
        if index_planned {
            r.stats.breakdown.index_fast_path_attempted = true;
        }
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if let Some(mut r) = execute_reverse_two_hop_top_k_count_by_terminal_key_query(
        query,
        graph,
        limits,
        build_hasher,
    )? {
        r.stats.breakdown.aggregate_fast_path_attempted = true;
        r.stats.breakdown.aggregate_fast_path_used = true;
        if index_planned {
            r.stats.breakdown.index_fast_path_attempted = true;
        }
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if let Some(r) = execute_var_len_terminal_projection_query(query, graph, limits)? {
        let mut r = r;
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if let Some(mut r) = execute_recent_two_hop_top_k_projection_query(query, graph, limits)? {
        r.stats.breakdown.top_k_calls = r.stats.breakdown.top_k_calls.saturating_add(1);
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if index_planned
        && let Some(mut r) = execute_index_plan_query(plan, query, graph, limits, build_hasher)?
    {
        r.stats.breakdown.index_fast_path_attempted = true;
        r.stats.breakdown.index_fast_path_used = true;
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if edge_index_planned
        && let Some(mut r) =
            execute_edge_index_plan_query(plan, query, graph, limits, build_hasher)?
    {
        r.stats.breakdown.index_fast_path_attempted = true;
        r.stats.breakdown.index_fast_path_used = true;
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    let conditional_planned = plan
        .ops
        .iter()
        .any(|op| matches!(op, PlanOp::ConditionalIndexScan));
    if conditional_planned
        && let Some(ref cond) = plan.annotations.conditional_scan
        && let Some(mut r) =
            execute_conditional_index_plan_query(plan, query, graph, limits, build_hasher, cond)?
    {
        r.stats.breakdown.index_fast_path_attempted = true;
        r.stats.breakdown.index_fast_path_used = true;
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if aggregate_planned
        && let Some(mut r) = execute_aggregate_plan_query(plan, query, graph, limits, build_hasher)?
    {
        r.stats.breakdown.aggregate_fast_path_attempted = true;
        r.stats.breakdown.aggregate_fast_path_used = true;
        if index_planned {
            r.stats.breakdown.index_fast_path_attempted = true;
        }
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    if shortest_planned
        && let Some(mut r) = execute_shortest_plan_query(plan, query, graph, limits)?
    {
        r.stats.breakdown.shortest_fast_path_attempted = true;
        r.stats.breakdown.shortest_fast_path_used = true;
        if index_planned {
            r.stats.breakdown.index_fast_path_attempted = true;
        }
        if aggregate_planned {
            r.stats.breakdown.aggregate_fast_path_attempted = true;
        }
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    // Label-anchor fast path: non-start anchor chosen by label cardinality.
    if plan.annotations.chosen_anchor.is_some()
        && let Some(r) = execute_label_anchor_plan_query(plan, query, graph, limits, build_hasher)?
    {
        let mut r = r;
        attach_debug_read_counters(&mut r);
        return Ok(r);
    }
    let mut result = execute_query(query, graph, limits, build_hasher)?;
    if index_planned || edge_index_planned || conditional_planned {
        result.stats.breakdown.index_fast_path_attempted = true;
    }
    if aggregate_planned {
        result.stats.breakdown.aggregate_fast_path_attempted = true;
    }
    if shortest_planned {
        result.stats.breakdown.shortest_fast_path_attempted = true;
    }
    attach_debug_read_counters(&mut result);
    Ok(result)
}

fn execute_index_plan_query<M: Memory + Clone, S: BuildHasher>(
    plan: &PhysicalPlan,
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    let Some(first_entry) = q.match_clauses.first() else {
        return Ok(None);
    };
    if first_entry.optional || first_entry.shortest {
        return Ok(None);
    }
    let start_var = first_entry
        .pattern
        .start
        .var
        .as_deref()
        .unwrap_or("__anon_start__");
    if plan.annotations.chosen_anchor.as_deref() == Some(start_var)
        && let Some(result) = execute_aggregate_query_fast(q, graph, limits, build_hasher)?
    {
        return Ok(Some(result));
    }
    let Some(_chosen_anchor) = plan.annotations.chosen_anchor.as_deref() else {
        return Ok(None);
    };
    // Determine scan type: range or equality.
    let range_cmp_op = plan.annotations.index_scan_cmp_op;

    // Try WHERE-based predicate first, then fall back to inline props_hint.
    let (pred_var, pred_prop, pred_value) = if let Some(cmp_op) = range_cmp_op {
        // Range index scan: extract the range predicate.
        let _ = cmp_op; // used below for scan dispatch
        if let Some((v, p, val, _)) = range_property_literal_predicate(q.where_clause.as_ref()) {
            (v, p, val)
        } else {
            return Ok(None);
        }
    } else if let Some((v, p, val)) = equality_property_literal_predicate(q.where_clause.as_ref()) {
        (v, p, val)
    } else if let Some((v, p, val)) = inline_props_hint_literal_predicate(q) {
        (v, p, val)
    } else {
        return Ok(None);
    };
    let required_index_type = if range_cmp_op.is_some() {
        IndexType::Range
    } else {
        IndexType::Equality
    };
    let has_registered_index = graph.list_property_indexes().into_iter().any(|idx| {
        idx.entity_type == EntityType::Vertex
            && idx.index_type == required_index_type
            && idx.property_name == pred_prop
    });
    if !has_registered_index {
        return Ok(None);
    }

    // Determine anchor position: start node or mid-chain target.
    let anchor_chain_idx: Option<usize> = if pred_var == start_var {
        None // anchor is the start node
    } else {
        let found = first_entry
            .pattern
            .elements
            .iter()
            .enumerate()
            .position(|(i, elem)| {
                let PatternElement::Hop(c) = elem else {
                    return false;
                };
                c.node.var.as_deref() == Some(&pred_var)
                    || (c.node.var.is_none() && pred_var == format!("__anon_chain_{i}__"))
            });
        match found {
            Some(idx) => Some(idx),
            None => return Ok(None), // pred_var not in pattern
        }
    };

    let mut stats = QueryStats::default();

    // Check if the anchor is at the end of the first MATCH clause's chain,
    // meaning reverse traversal fully resolves the entire first MATCH.
    let anchor_covers_all_chains =
        anchor_chain_idx.is_some_and(|k| k == first_entry.pattern.elements.len() - 1);

    // When anchor covers all chains, push down WITH LIMIT into reverse traversal
    // (only safe when no ORDER BY precedes the LIMIT, since ORDER BY needs all rows).
    let reverse_max_rows = if anchor_covers_all_chains {
        q.with_clauses.first().and_then(|w| {
            if w.order_by.is_none() {
                w.limit.map(|l| l.0 as usize)
            } else {
                None
            }
        })
    } else {
        None
    };

    // Scan helper: dispatches to equality or range scan based on plan annotation.
    let scan_vertices =
        |graph: &PmaGraph<M>, prop: &str, val: &Value| -> Result<VertexIdSet, GleaphError> {
            if let Some(cmp_op) = range_cmp_op {
                use gleaph_pma::property_store::RangeOp;
                let range_op = match cmp_op {
                    ConditionalCmpOp::Ge => RangeOp::Ge,
                    ConditionalCmpOp::Gt => RangeOp::Gt,
                    ConditionalCmpOp::Le => RangeOp::Le,
                    ConditionalCmpOp::Lt => RangeOp::Lt,
                    ConditionalCmpOp::Eq => unreachable!(),
                };
                graph.scan_vertices_by_property_range_auto(prop, val, range_op)
            } else {
                graph.scan_vertices_by_property_eq_auto(prop, val)
            }
        };

    let mut rows = if let Some(k) = anchor_chain_idx {
        // ── Non-start-anchor: seed from index, reverse-traverse to start ──
        let anchor_node = &first_entry.pattern.chain(k).node;
        let seed_vertices: Vec<u32> = scan_vertices(graph, &pred_prop, &pred_value)?
            .iter()
            .filter(|v| node_matches(anchor_node, *v, graph))
            .collect();
        if seed_vertices.is_empty() {
            return Ok(Some(QueryResult {
                columns: if q.return_clause.star {
                    Vec::new()
                } else {
                    q.return_clause.items.iter().map(column_name).collect()
                },
                rows: Vec::new(),
                stats,
                warnings: vec![],
            }));
        }
        let mut all_rows = Vec::new();
        for anchor_v in seed_vertices {
            let mut seed_rows = reverse_traverse_to_start(
                anchor_v,
                k,
                &first_entry.pattern,
                graph,
                &mut stats,
                q.where_clause.as_ref(),
                limits,
                reverse_max_rows,
            )?;
            // Bind the anchor variable in each seed row.
            for row in &mut seed_rows {
                row.insert(pred_var.clone(), Binding::Vertex(anchor_v));
            }
            all_rows.extend(seed_rows);
        }
        all_rows
    } else {
        // ── Start-anchor (existing path) ──
        let seed_vertices: Vec<u32> = scan_vertices(graph, &pred_prop, &pred_value)?
            .iter()
            .filter(|v| node_matches(&first_entry.pattern.start, *v, graph))
            .collect();
        seed_vertices
            .into_iter()
            .map(|v| {
                let mut b = Bindings::new();
                b.insert(start_var.to_string(), Binding::Vertex(v));
                b
            })
            .collect::<Vec<_>>()
    };
    if rows.is_empty() {
        return Ok(Some(QueryResult {
            columns: if q.return_clause.star {
                Vec::new()
            } else {
                q.return_clause.items.iter().map(column_name).collect()
            },
            rows: Vec::new(),
            stats,
            warnings: vec![],
        }));
    }
    if anchor_covers_all_chains {
        // Reverse traversal fully resolved the first MATCH clause.
        // Apply any_paths truncation that would have been done in forward path.
        if let Some(k) = first_entry.any_paths {
            rows.truncate(k as usize);
        }
        if let Some(ref keep) = first_entry.keep_clause {
            apply_keep_clause(&mut rows, keep);
        }
        // Skip forward re-scan of the first MATCH (already fully bound).
        // Still need to process remaining MATCH clauses (2nd, 3rd, ...) if any.
        if q.match_clauses.len() > 1 {
            let _compiled_where_guard = push_compiled_where_scope(q.where_clause.as_ref());
            let pushdown_limit = if q.order_by.is_none() {
                q.limit.map(|l| l.0 as usize)
            } else {
                None
            };
            // Use planner-determined clause order for remaining MATCH clauses.
            let default_remaining: Vec<usize> = (1..q.match_clauses.len()).collect();
            let remaining_order = plan
                .annotations
                .match_clause_order
                .as_ref()
                .map(|o| o.iter().copied().filter(|&i| i != 0).collect::<Vec<_>>())
                .unwrap_or(default_remaining);
            let total_remaining = remaining_order.len();
            for (step, &clause_idx) in remaining_order.iter().enumerate() {
                let entry = &q.match_clauses[clause_idx];
                let apply_where = if step + 1 == total_remaining {
                    q.where_clause.as_ref()
                } else {
                    None
                };
                rows = execute_match_clause_joined(
                    &entry.pattern,
                    entry.shortest,
                    entry.shortest_mode,
                    entry.path_variable.as_deref(),
                    entry.path_mode,
                    &rows,
                    graph,
                    &mut stats,
                    apply_where,
                    pushdown_limit,
                    limits,
                    entry.optional,
                )?;
                if let Some(k) = entry.any_paths {
                    rows.truncate(k as usize);
                }
                if let Some(ref keep) = entry.keep_clause {
                    apply_keep_clause(&mut rows, keep);
                }
            }
        }
    } else {
        let pushdown_limit = if q.order_by.is_none() {
            q.limit.map(|l| l.0 as usize)
        } else {
            None
        };
        rows = execute_query_match_entries_from_seed_rows(
            q,
            graph,
            &mut stats,
            q.where_clause.as_ref(),
            pushdown_limit,
            limits,
            rows,
            plan.annotations.match_clause_order.as_deref(),
        )?;
    }
    stats.breakdown.rows_after_match = rows.len() as u64;
    rows = apply_with_clauses(q, rows, graph, &mut stats, limits)?;
    stats.breakdown.rows_after_with = rows.len() as u64;

    let is_agg = query_has_aggregate(q);
    let compiled_return_exprs = if !q.return_clause.star && !is_agg {
        compile_value_exprs(q.return_clause.items.iter().map(|item| &item.expr))
    } else {
        None
    };
    if let Some(order_by) = &q.order_by
        && !is_agg
    {
        let compiled_order_exprs =
            compile_value_exprs(order_by.items.iter().map(|item| &item.expr));
        bump_steps(&mut stats, rows.len() as u64, limits)?;
        if let Some(limit) = q.limit {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            rows = top_k_rows(
                rows,
                order_by,
                compiled_order_exprs.as_deref(),
                limit.0 as usize,
                graph,
            );
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            rows.sort_by(|a, b| {
                compare_rows_for_order(order_by, a, b, compiled_order_exprs.as_deref(), graph)
            });
        }
    }
    if let Some(limit) = q.limit
        && !is_agg
    {
        rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.breakdown.rows_before_projection = rows.len() as u64;
    let mut projected_rows = if q.return_clause.star {
        rows.iter().map(project_star_row).collect::<Vec<_>>()
    } else if is_agg {
        project_aggregated_rows(q, &rows, graph, build_hasher, Some(&mut stats))?
    } else if let Some(compiled_exprs) = compiled_return_exprs.as_deref() {
        rows.iter()
            .map(|bindings| {
                compiled_exprs
                    .iter()
                    .map(|expr| eval_compiled_value_expr(expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    } else {
        rows.iter()
            .map(|bindings| {
                q.return_clause
                    .items
                    .iter()
                    .map(|item| eval_expr(&item.expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let columns = if q.return_clause.star {
        star_columns(&rows)
    } else {
        q.return_clause
            .items
            .iter()
            .map(column_name)
            .collect::<Vec<_>>()
    };
    if q.return_clause.distinct {
        let mut seen = BTreeSet::new();
        if q.order_by.is_none()
            && let Some(limit) = q.limit
        {
            // Early termination: stop once k distinct rows collected.
            let k = limit.0 as usize;
            let mut deduped = Vec::with_capacity(k);
            for row in projected_rows.into_iter() {
                if seen.insert(format!("{row:?}")) {
                    deduped.push(row);
                    if deduped.len() >= k {
                        break;
                    }
                }
            }
            projected_rows = deduped;
        } else {
            projected_rows.retain(|row| seen.insert(format!("{row:?}")));
        }
    }
    if let Some(order_by) = &q.order_by
        && is_agg
    {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit
        && is_agg
    {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    if let Some(offset) = q.offset {
        let off = offset as usize;
        if off >= projected_rows.len() {
            projected_rows.clear();
        } else {
            projected_rows.drain(0..off);
        }
    }
    let projection_cells = projected_rows
        .len()
        .saturating_mul(if q.return_clause.star {
            columns.len()
        } else {
            q.return_clause.items.len()
        });
    bump_steps(&mut stats, projection_cells as u64, limits)?;
    stats.rows_emitted = projected_rows.len() as u64;
    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

/// Label-anchor fast path: when the planner picks a non-start mid-chain node as
/// anchor purely based on label cardinality (no index), scan anchor candidates
/// via label, reverse-traverse to the start, then forward-expand remaining hops.
fn execute_label_anchor_plan_query<M: Memory + Clone, S: BuildHasher>(
    plan: &PhysicalPlan,
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    let Some(first_entry) = q.match_clauses.first() else {
        return Ok(None);
    };
    if first_entry.optional || first_entry.shortest {
        return Ok(None);
    }
    let anchor_var = match plan.annotations.chosen_anchor.as_deref() {
        Some(v) => v,
        None => return Ok(None),
    };
    let start_var = first_entry
        .pattern
        .start
        .var
        .as_deref()
        .unwrap_or("__anon_start__");
    // Only activate for non-start anchors.
    if anchor_var == start_var {
        return Ok(None);
    }
    // All hops must be fixed-length(1) for reverse_traverse_to_start.
    let all_fixed_1 = first_entry
        .pattern
        .hops()
        .all(|c| matches!(c.edge.length, crate::ast::PathLength::Fixed(1)));
    if !all_fixed_1 {
        return Ok(None);
    }
    // Find the anchor hop index.
    let anchor_chain_idx = first_entry
        .pattern
        .elements
        .iter()
        .enumerate()
        .position(|(i, elem)| {
            let PatternElement::Hop(c) = elem else {
                return false;
            };
            c.node.var.as_deref() == Some(anchor_var)
                || (c.node.var.is_none() && anchor_var == format!("__anon_chain_{i}__"))
        });
    let Some(k) = anchor_chain_idx else {
        return Ok(None);
    };

    let mut stats = QueryStats::default();
    let anchor_node = &first_entry.pattern.chain(k).node;
    // Label-scan for anchor candidates.
    let seed_vertices = initial_candidates(anchor_node, graph, &mut stats, limits)?;
    if seed_vertices.is_empty() {
        return Ok(Some(QueryResult {
            columns: if q.return_clause.star {
                Vec::new()
            } else {
                q.return_clause.items.iter().map(column_name).collect()
            },
            rows: Vec::new(),
            stats,
            warnings: vec![],
        }));
    }

    let anchor_covers_all_chains = k == first_entry.pattern.elements.len() - 1;
    let reverse_max_rows = if anchor_covers_all_chains {
        q.with_clauses.first().and_then(|w| {
            if w.order_by.is_none() {
                w.limit.map(|l| l.0 as usize)
            } else {
                None
            }
        })
    } else {
        None
    };

    let mut rows = Vec::new();
    for anchor_v in seed_vertices {
        let mut seed_rows = reverse_traverse_to_start(
            anchor_v,
            k,
            &first_entry.pattern,
            graph,
            &mut stats,
            q.where_clause.as_ref(),
            limits,
            reverse_max_rows,
        )?;
        for row in &mut seed_rows {
            row.insert(anchor_var.to_string(), Binding::Vertex(anchor_v));
        }
        rows.extend(seed_rows);
    }
    if rows.is_empty() {
        return Ok(Some(QueryResult {
            columns: if q.return_clause.star {
                Vec::new()
            } else {
                q.return_clause.items.iter().map(column_name).collect()
            },
            rows: Vec::new(),
            stats,
            warnings: vec![],
        }));
    }

    if anchor_covers_all_chains {
        if let Some(k) = first_entry.any_paths {
            rows.truncate(k as usize);
        }
        if let Some(ref keep) = first_entry.keep_clause {
            apply_keep_clause(&mut rows, keep);
        }
        if q.match_clauses.len() > 1 {
            let _compiled_where_guard = push_compiled_where_scope(q.where_clause.as_ref());
            let pushdown_limit = if q.order_by.is_none() {
                q.limit.map(|l| l.0 as usize)
            } else {
                None
            };
            let default_remaining: Vec<usize> = (1..q.match_clauses.len()).collect();
            let remaining_order = plan
                .annotations
                .match_clause_order
                .as_ref()
                .map(|o| o.iter().copied().filter(|&i| i != 0).collect::<Vec<_>>())
                .unwrap_or(default_remaining);
            let total_remaining = remaining_order.len();
            for (step, &clause_idx) in remaining_order.iter().enumerate() {
                let entry = &q.match_clauses[clause_idx];
                let apply_where = if step + 1 == total_remaining {
                    q.where_clause.as_ref()
                } else {
                    None
                };
                rows = execute_match_clause_joined(
                    &entry.pattern,
                    entry.shortest,
                    entry.shortest_mode,
                    entry.path_variable.as_deref(),
                    entry.path_mode,
                    &rows,
                    graph,
                    &mut stats,
                    apply_where,
                    pushdown_limit,
                    limits,
                    entry.optional,
                )?;
                if let Some(k) = entry.any_paths {
                    rows.truncate(k as usize);
                }
                if let Some(ref keep) = entry.keep_clause {
                    apply_keep_clause(&mut rows, keep);
                }
            }
        }
    } else {
        let pushdown_limit = if q.order_by.is_none() {
            q.limit.map(|l| l.0 as usize)
        } else {
            None
        };
        rows = execute_query_match_entries_from_seed_rows(
            q,
            graph,
            &mut stats,
            q.where_clause.as_ref(),
            pushdown_limit,
            limits,
            rows,
            plan.annotations.match_clause_order.as_deref(),
        )?;
    }
    stats.breakdown.rows_after_match = rows.len() as u64;
    rows = apply_with_clauses(q, rows, graph, &mut stats, limits)?;
    stats.breakdown.rows_after_with = rows.len() as u64;

    let is_agg = query_has_aggregate(q);
    let compiled_return_exprs = if !q.return_clause.star && !is_agg {
        compile_value_exprs(q.return_clause.items.iter().map(|item| &item.expr))
    } else {
        None
    };
    if let Some(order_by) = &q.order_by
        && !is_agg
    {
        let compiled_order_exprs =
            compile_value_exprs(order_by.items.iter().map(|item| &item.expr));
        bump_steps(&mut stats, rows.len() as u64, limits)?;
        if let Some(limit) = q.limit {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            rows = top_k_rows(
                rows,
                order_by,
                compiled_order_exprs.as_deref(),
                limit.0 as usize,
                graph,
            );
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            rows.sort_by(|a, b| {
                compare_rows_for_order(order_by, a, b, compiled_order_exprs.as_deref(), graph)
            });
        }
    }
    if let Some(limit) = q.limit
        && !is_agg
    {
        rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.breakdown.rows_before_projection = rows.len() as u64;
    let mut projected_rows = if q.return_clause.star {
        rows.iter().map(project_star_row).collect::<Vec<_>>()
    } else if is_agg {
        project_aggregated_rows(q, &rows, graph, build_hasher, Some(&mut stats))?
    } else if let Some(compiled_exprs) = compiled_return_exprs.as_deref() {
        rows.iter()
            .map(|bindings| {
                compiled_exprs
                    .iter()
                    .map(|expr| eval_compiled_value_expr(expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    } else {
        rows.iter()
            .map(|bindings| {
                q.return_clause
                    .items
                    .iter()
                    .map(|item| eval_expr(&item.expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let columns = if q.return_clause.star {
        star_columns(&rows)
    } else {
        q.return_clause
            .items
            .iter()
            .map(column_name)
            .collect::<Vec<_>>()
    };
    if q.return_clause.distinct {
        let mut seen = BTreeSet::new();
        if q.order_by.is_none()
            && let Some(limit) = q.limit
        {
            let k = limit.0 as usize;
            let mut deduped = Vec::with_capacity(k);
            for row in projected_rows.into_iter() {
                if seen.insert(format!("{row:?}")) {
                    deduped.push(row);
                    if deduped.len() >= k {
                        break;
                    }
                }
            }
            projected_rows = deduped;
        } else {
            projected_rows.retain(|row| seen.insert(format!("{row:?}")));
        }
    }
    if let Some(order_by) = &q.order_by
        && is_agg
    {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit
        && is_agg
    {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    if let Some(offset) = q.offset {
        let off = offset as usize;
        if off >= projected_rows.len() {
            projected_rows.clear();
        } else {
            projected_rows.drain(0..off);
        }
    }
    let projection_cells = projected_rows
        .len()
        .saturating_mul(if q.return_clause.star {
            columns.len()
        } else {
            q.return_clause.items.len()
        });
    bump_steps(&mut stats, projection_cells as u64, limits)?;
    stats.rows_emitted = projected_rows.len() as u64;
    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

/// Conditional index scan: checks parameter values at runtime and branches.
///
/// Iterates through candidates in order; the first candidate whose parameter is
/// non-NULL is used for an index-seeded scan. If all parameters are NULL, returns
/// `Ok(None)` to fall back to the generic NodeScan path.
/// Routes a conditional scan candidate to the appropriate index scan method.
fn conditional_scan_vertices<M: Memory + Clone>(
    graph: &PmaGraph<M>,
    candidate: &crate::plan::ConditionalScanCandidate,
    param_val: &Value,
) -> Result<VertexIdSet, GleaphError> {
    use gleaph_pma::property_store::RangeOp;
    match candidate.cmp_op {
        ConditionalCmpOp::Eq => {
            graph.scan_vertices_by_property_eq_auto(&candidate.property, param_val)
        }
        ConditionalCmpOp::Ge => {
            graph.scan_vertices_by_property_range_auto(&candidate.property, param_val, RangeOp::Ge)
        }
        ConditionalCmpOp::Gt => {
            graph.scan_vertices_by_property_range_auto(&candidate.property, param_val, RangeOp::Gt)
        }
        ConditionalCmpOp::Le => {
            graph.scan_vertices_by_property_range_auto(&candidate.property, param_val, RangeOp::Le)
        }
        ConditionalCmpOp::Lt => {
            graph.scan_vertices_by_property_range_auto(&candidate.property, param_val, RangeOp::Lt)
        }
    }
}

/// Attempts a compound range scan when a complementary candidate exists on the
/// same (variable, property). Falls back to single-bound scan otherwise.
fn conditional_scan_with_compound<M: Memory + Clone>(
    graph: &PmaGraph<M>,
    chosen: &crate::plan::ConditionalScanCandidate,
    param_val: &Value,
    all_candidates: &[crate::plan::ConditionalScanCandidate],
) -> Result<VertexIdSet, GleaphError> {
    use gleaph_pma::property_store::RangeOp;

    fn cmp_op_to_range(op: ConditionalCmpOp) -> Option<RangeOp> {
        match op {
            ConditionalCmpOp::Ge => Some(RangeOp::Ge),
            ConditionalCmpOp::Gt => Some(RangeOp::Gt),
            ConditionalCmpOp::Le => Some(RangeOp::Le),
            ConditionalCmpOp::Lt => Some(RangeOp::Lt),
            ConditionalCmpOp::Eq => None,
        }
    }

    fn is_lower_bound(op: ConditionalCmpOp) -> bool {
        matches!(op, ConditionalCmpOp::Ge | ConditionalCmpOp::Gt)
    }
    fn is_upper_bound(op: ConditionalCmpOp) -> bool {
        matches!(op, ConditionalCmpOp::Le | ConditionalCmpOp::Lt)
    }

    // Only attempt compound for range operators.
    if matches!(chosen.cmp_op, ConditionalCmpOp::Eq) {
        return conditional_scan_vertices(graph, chosen, param_val);
    }

    // Find a complementary candidate on the same (variable, property).
    let complement = all_candidates.iter().find(|c| {
        !std::ptr::eq(*c, chosen)
            && c.variable == chosen.variable
            && c.property == chosen.property
            && ((is_lower_bound(chosen.cmp_op) && is_upper_bound(c.cmp_op))
                || (is_upper_bound(chosen.cmp_op) && is_lower_bound(c.cmp_op)))
    });

    let Some(complement) = complement else {
        return conditional_scan_vertices(graph, chosen, param_val);
    };

    // Check if the complement's parameter is non-NULL.
    let comp_val = QUERY_PARAMS
        .with(|p| p.borrow().get(&complement.param_name).cloned())
        .unwrap_or(Value::Null);
    if matches!(comp_val, Value::Null) {
        return conditional_scan_vertices(graph, chosen, param_val);
    }

    // Determine lower and upper bounds.
    let (lower_val, lower_op, upper_val, upper_op) = if is_lower_bound(chosen.cmp_op) {
        (
            param_val,
            cmp_op_to_range(chosen.cmp_op).unwrap(),
            &comp_val,
            cmp_op_to_range(complement.cmp_op).unwrap(),
        )
    } else {
        (
            &comp_val,
            cmp_op_to_range(complement.cmp_op).unwrap(),
            param_val,
            cmp_op_to_range(chosen.cmp_op).unwrap(),
        )
    };

    graph.scan_vertices_by_property_range_between_auto(
        &chosen.property,
        lower_val,
        lower_op,
        upper_val,
        upper_op,
    )
}

fn execute_conditional_index_plan_query<M: Memory + Clone, S: BuildHasher>(
    plan: &PhysicalPlan,
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
    cond: &ConditionalScanInfo,
) -> Result<Option<QueryResult>, GleaphError> {
    // Find the first candidate with a non-NULL parameter that has a registered index.
    let chosen = cond.candidates.iter().find(|c| {
        let val = QUERY_PARAMS
            .with(|p| p.borrow().get(&c.param_name).cloned())
            .unwrap_or(Value::Null);
        if matches!(val, Value::Null) {
            return false;
        }
        let required_index_type = match c.cmp_op {
            ConditionalCmpOp::Eq => IndexType::Equality,
            _ => IndexType::Range,
        };
        graph.list_property_indexes().into_iter().any(|idx| {
            idx.entity_type == EntityType::Vertex
                && idx.index_type == required_index_type
                && idx.property_name == c.property
        })
    });

    let Some(chosen) = chosen else {
        // All parameters are NULL or no index available → fall back to generic path.
        return Ok(None);
    };

    let param_val = QUERY_PARAMS
        .with(|p| p.borrow().get(&chosen.param_name).cloned())
        .unwrap_or(Value::Null);

    let Some(first_entry) = q.match_clauses.first() else {
        return Ok(None);
    };
    if first_entry.optional || first_entry.shortest {
        return Ok(None);
    }

    let start_var = first_entry
        .pattern
        .start
        .var
        .as_deref()
        .unwrap_or("__anon_start__");

    // Determine anchor position: start node or mid-chain target.
    let anchor_chain_idx: Option<usize> = if chosen.variable == start_var {
        None
    } else {
        let found = first_entry
            .pattern
            .elements
            .iter()
            .enumerate()
            .position(|(i, elem)| {
                let PatternElement::Hop(c) = elem else {
                    return false;
                };
                c.node.var.as_deref() == Some(chosen.variable.as_str())
                    || (c.node.var.is_none() && chosen.variable == format!("__anon_chain_{i}__"))
            });
        match found {
            Some(idx) => Some(idx),
            None => return Ok(None),
        }
    };

    let mut stats = QueryStats::default();

    // Determine anchor position: does it cover all chains?
    let anchor_covers_all_chains =
        anchor_chain_idx.is_some_and(|k| k == first_entry.pattern.elements.len() - 1);

    // Push down WITH LIMIT into reverse traversal when safe (no ORDER BY).
    let reverse_max_rows = if anchor_covers_all_chains {
        q.with_clauses.first().and_then(|w| {
            if w.order_by.is_none() {
                w.limit.map(|l| l.0 as usize)
            } else {
                None
            }
        })
    } else {
        None
    };

    let mut rows = if let Some(k) = anchor_chain_idx {
        // Non-start-anchor: seed from index, reverse-traverse to start.
        let anchor_node = &first_entry.pattern.chain(k).node;
        let seed_vertices: Vec<u32> =
            conditional_scan_with_compound(graph, chosen, &param_val, &cond.candidates)?
                .iter()
                .filter(|v| node_matches(anchor_node, *v, graph))
                .collect();
        if seed_vertices.is_empty() {
            return Ok(Some(QueryResult {
                columns: if q.return_clause.star {
                    Vec::new()
                } else {
                    q.return_clause.items.iter().map(column_name).collect()
                },
                rows: Vec::new(),
                stats,
                warnings: vec![],
            }));
        }
        let mut all_rows = Vec::new();
        for anchor_v in seed_vertices {
            let mut seed_rows = reverse_traverse_to_start(
                anchor_v,
                k,
                &first_entry.pattern,
                graph,
                &mut stats,
                q.where_clause.as_ref(),
                limits,
                reverse_max_rows,
            )?;
            for row in &mut seed_rows {
                row.insert(chosen.variable.clone(), Binding::Vertex(anchor_v));
            }
            all_rows.extend(seed_rows);
        }
        all_rows
    } else {
        // Start-anchor: seed from index.
        let seed_vertices: Vec<u32> =
            conditional_scan_with_compound(graph, chosen, &param_val, &cond.candidates)?
                .iter()
                .filter(|v| node_matches(&first_entry.pattern.start, *v, graph))
                .collect();
        seed_vertices
            .into_iter()
            .map(|v| {
                let mut b = Bindings::new();
                b.insert(start_var.to_string(), Binding::Vertex(v));
                b
            })
            .collect::<Vec<_>>()
    };
    if rows.is_empty() {
        return Ok(Some(QueryResult {
            columns: if q.return_clause.star {
                Vec::new()
            } else {
                q.return_clause.items.iter().map(column_name).collect()
            },
            rows: Vec::new(),
            stats,
            warnings: vec![],
        }));
    }

    // Forward-extend remaining chains (same as execute_index_plan_query).
    if anchor_covers_all_chains {
        if let Some(k) = first_entry.any_paths {
            rows.truncate(k as usize);
        }
        if let Some(ref keep) = first_entry.keep_clause {
            apply_keep_clause(&mut rows, keep);
        }
        if q.match_clauses.len() > 1 {
            let _compiled_where_guard = push_compiled_where_scope(q.where_clause.as_ref());
            let pushdown_limit = if q.order_by.is_none() {
                q.limit.map(|l| l.0 as usize)
            } else {
                None
            };
            let default_remaining: Vec<usize> = (1..q.match_clauses.len()).collect();
            let remaining_order = plan
                .annotations
                .match_clause_order
                .as_ref()
                .map(|o| o.iter().copied().filter(|&i| i != 0).collect::<Vec<_>>())
                .unwrap_or(default_remaining);
            let total_remaining = remaining_order.len();
            for (step, &clause_idx) in remaining_order.iter().enumerate() {
                let entry = &q.match_clauses[clause_idx];
                let apply_where = if step + 1 == total_remaining {
                    q.where_clause.as_ref()
                } else {
                    None
                };
                rows = execute_match_clause_joined(
                    &entry.pattern,
                    entry.shortest,
                    entry.shortest_mode,
                    entry.path_variable.as_deref(),
                    entry.path_mode,
                    &rows,
                    graph,
                    &mut stats,
                    apply_where,
                    pushdown_limit,
                    limits,
                    entry.optional,
                )?;
                if let Some(k) = entry.any_paths {
                    rows.truncate(k as usize);
                }
                if let Some(ref keep) = entry.keep_clause {
                    apply_keep_clause(&mut rows, keep);
                }
            }
        }
    } else {
        let pushdown_limit = if q.order_by.is_none() {
            q.limit.map(|l| l.0 as usize)
        } else {
            None
        };
        rows = execute_query_match_entries_from_seed_rows(
            q,
            graph,
            &mut stats,
            q.where_clause.as_ref(),
            pushdown_limit,
            limits,
            rows,
            plan.annotations.match_clause_order.as_deref(),
        )?;
    }
    stats.breakdown.rows_after_match = rows.len() as u64;
    rows = apply_with_clauses(q, rows, graph, &mut stats, limits)?;
    stats.breakdown.rows_after_with = rows.len() as u64;

    let is_agg = query_has_aggregate(q);
    let compiled_return_exprs = if !q.return_clause.star && !is_agg {
        compile_value_exprs(q.return_clause.items.iter().map(|item| &item.expr))
    } else {
        None
    };
    if let Some(order_by) = &q.order_by
        && !is_agg
    {
        let compiled_order_exprs =
            compile_value_exprs(order_by.items.iter().map(|item| &item.expr));
        bump_steps(&mut stats, rows.len() as u64, limits)?;
        if let Some(limit) = q.limit {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            rows = top_k_rows(
                rows,
                order_by,
                compiled_order_exprs.as_deref(),
                limit.0 as usize,
                graph,
            );
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            rows.sort_by(|a, b| {
                compare_rows_for_order(order_by, a, b, compiled_order_exprs.as_deref(), graph)
            });
        }
    }
    if let Some(limit) = q.limit
        && !is_agg
    {
        rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.breakdown.rows_before_projection = rows.len() as u64;
    let mut projected_rows = if q.return_clause.star {
        rows.iter().map(project_star_row).collect::<Vec<_>>()
    } else if is_agg {
        project_aggregated_rows(q, &rows, graph, build_hasher, Some(&mut stats))?
    } else if let Some(compiled_exprs) = compiled_return_exprs.as_deref() {
        rows.iter()
            .map(|bindings| {
                compiled_exprs
                    .iter()
                    .map(|expr| eval_compiled_value_expr(expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    } else {
        rows.iter()
            .map(|bindings| {
                q.return_clause
                    .items
                    .iter()
                    .map(|item| eval_expr(&item.expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let columns = if q.return_clause.star {
        star_columns(&rows)
    } else {
        q.return_clause
            .items
            .iter()
            .map(column_name)
            .collect::<Vec<_>>()
    };
    if q.return_clause.distinct {
        let mut seen = BTreeSet::new();
        if q.order_by.is_none()
            && let Some(limit) = q.limit
        {
            let k = limit.0 as usize;
            let mut deduped = Vec::with_capacity(k);
            for row in projected_rows.into_iter() {
                if seen.insert(format!("{row:?}")) {
                    deduped.push(row);
                    if deduped.len() >= k {
                        break;
                    }
                }
            }
            projected_rows = deduped;
        } else {
            projected_rows.retain(|row| seen.insert(format!("{row:?}")));
        }
    }
    if let Some(order_by) = &q.order_by
        && is_agg
    {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit
        && is_agg
    {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    if let Some(offset) = q.offset {
        let off = offset as usize;
        if off >= projected_rows.len() {
            projected_rows.clear();
        } else {
            projected_rows.drain(0..off);
        }
    }
    let projection_cells = projected_rows
        .len()
        .saturating_mul(if q.return_clause.star {
            columns.len()
        } else {
            q.return_clause.items.len()
        });
    bump_steps(&mut stats, projection_cells as u64, limits)?;
    stats.rows_emitted = projected_rows.len() as u64;
    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

/// Edge-index-seeded query: scans `scan_edges_by_property_eq` for matching
/// edge pairs, then filters node patterns and forward-extends remaining chains.
#[allow(clippy::too_many_arguments)]
fn execute_edge_index_plan_query<M: Memory + Clone, S: BuildHasher>(
    plan: &PhysicalPlan,
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    let Some(first_entry) = q.match_clauses.first() else {
        return Ok(None);
    };
    if first_entry.optional || first_entry.shortest {
        return Ok(None);
    }
    let m = &first_entry.pattern;
    if m.elements.is_empty() {
        return Ok(None);
    }

    // Only handle first chain for now.
    let chain = m.chain(0);

    // Find edge property predicate: inline edge properties or WHERE clause.
    let pred: Option<(String, Value)> = {
        // Inline edge property hints.
        let mut found = None;
        for (prop, expr) in &chain.edge.properties {
            if let Expr::Literal(val) = expr {
                found = Some((prop.clone(), val.clone()));
                break;
            }
        }
        // WHERE clause: e.prop = literal.
        if found.is_none()
            && let Some((var, prop, val)) =
                equality_property_literal_predicate(q.where_clause.as_ref())
            && chain.edge.var.as_deref() == Some(&var)
        {
            found = Some((prop, val));
        }
        found
    };
    let Some((pred_prop, pred_value)) = pred else {
        return Ok(None);
    };

    // Verify the property is indexed.
    let has_index = graph.list_property_indexes().into_iter().any(|idx| {
        idx.entity_type == EntityType::Edge
            && idx.index_type == IndexType::Equality
            && idx.property_name == pred_prop
    });
    if !has_index {
        return Ok(None);
    }

    let mut stats = QueryStats::default();
    let _compiled_where_guard = push_compiled_where_scope(q.where_clause.as_ref());

    // Scan edge index for all matching edges with cached label and identity metadata.
    let edge_pairs = graph.scan_edges_by_property_eq_rich(&pred_prop, &pred_value);
    stats.breakdown.index_fast_path_attempted = true;

    // Resolve edge label constraint.
    let resolved_label = resolve_edge_label(&chain.edge, graph);

    let start_var = m.start.var.as_deref().unwrap_or("__anon_start__");
    let end_var = chain.node.var.as_deref();

    let has_more_chains = m.elements.len() > 1;

    let mut seed_rows = Vec::new();
    for edge_match in &edge_pairs {
        bump_steps(&mut stats, 1, limits)?;
        stats.scanned_edges += 1;
        let src = edge_match.src;
        let dst = edge_match.dst;
        let label_id = edge_match.label_id;

        // Determine start/end vertices based on edge direction.
        let (start_v, end_v) = match chain.edge.direction {
            Direction::Outgoing => (src, dst),
            Direction::Incoming => (dst, src),
            Direction::Either => (src, dst), // will also try reversed below
        };

        // Check edge label matches.
        let label_ref = graph.label_name_by_id(label_id);
        if !resolved_label.matches(label_id) {
            // For Direction::Either, try the other direction.
            if chain.edge.direction == Direction::Either {
                let rev_label_id = graph.edge_label(dst, src).map_or(0, |label| {
                    graph.label_index.label_id(&label).unwrap_or(0)
                });
                if resolved_label.matches(rev_label_id)
                    && !graph.is_vertex_tombstoned(dst)
                    && !graph.is_vertex_tombstoned(src)
                    && node_matches(&m.start, dst, graph)
                    && node_matches(&chain.node, src, graph)
                {
                    let rev_label_ref = graph.label_name_by_id(rev_label_id);
                    let mut bindings = Bindings::new();
                    bindings.insert(start_var.to_string(), Binding::Vertex(dst));
                    if let Some(ev) = &chain.edge.var {
                        let edge_entry = graph
                            .collect_neighbors(dst)
                            .ok()
                            .and_then(|n| n.into_iter().find(|e| e.target == src));
                        if let Some(ee) = edge_entry {
                            bindings.insert(
                                ev.clone(),
                                Binding::Edge {
                                    src: dst,
                                    dst: src,
                                    label: rev_label_ref.map(Arc::from),
                                    edge_id: ee.edge_id,
                                    weight: ee.weight,
                                    timestamp: ee.timestamp,
                                },
                            );
                        }
                    }
                    if let Some(nv) = end_var {
                        bindings.insert(nv.to_string(), Binding::Vertex(src));
                    }
                    if eval_where_partial_pushdown(q.where_clause.as_ref(), &bindings, graph) {
                        seed_rows.push(bindings);
                    }
                }
            }
            continue;
        }

        // Check tombstone status.
        if graph.is_vertex_tombstoned(start_v) || graph.is_vertex_tombstoned(end_v) {
            continue;
        }

        // Check start node pattern.
        if !node_matches(&m.start, start_v, graph) {
            continue;
        }
        // Check end node pattern.
        if !node_matches(&chain.node, end_v, graph) {
            continue;
        }

        let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
        let mut bindings = Bindings::new();
        bindings.insert(start_var.to_string(), Binding::Vertex(start_v));
        if let Some(ev) = &chain.edge.var {
            let edge_entry = graph
                .collect_neighbors(src)
                .ok()
                .and_then(|n| n.into_iter().find(|e| e.target == dst));
            if let Some(ee) = edge_entry {
                bindings.insert(
                    ev.clone(),
                    Binding::Edge {
                        src,
                        dst,
                        label: label_arc,
                        edge_id: ee.edge_id,
                        weight: ee.weight,
                        timestamp: ee.timestamp,
                    },
                );
            }
        }
        if let Some(nv) = end_var {
            bindings.insert(nv.to_string(), Binding::Vertex(end_v));
        }

        // Evaluate inline WHERE on edge.
        if let Some(w) = chain.edge.where_clause.as_deref()
            && !truthy(&eval_expr(w, &bindings, graph))
        {
            continue;
        }
        // Evaluate inline WHERE on end node.
        if let Some(w) = chain.node.where_clause.as_deref()
            && !truthy(&eval_expr(w, &bindings, graph))
        {
            continue;
        }

        if !eval_where_partial_pushdown(q.where_clause.as_ref(), &bindings, graph) {
            continue;
        }

        seed_rows.push(bindings);
    }

    if seed_rows.is_empty() {
        return Ok(Some(QueryResult {
            columns: if q.return_clause.star {
                Vec::new()
            } else {
                q.return_clause.items.iter().map(column_name).collect()
            },
            rows: Vec::new(),
            stats,
            warnings: vec![],
        }));
    }

    // Forward-extend remaining chains within the first MATCH clause.
    if has_more_chains {
        let mut extended_rows = Vec::new();
        let pushdown_limit = if q.order_by.is_none() {
            q.limit.map(|l| l.0 as usize)
        } else {
            None
        };
        for bindings in seed_rows {
            let end_v = match end_var {
                Some(v) => match bindings.get(v) {
                    Some(Binding::Vertex(id)) => *id,
                    _ => continue,
                },
                None => continue,
            };
            let path_elems = vec![PathElement::Node(end_v)];
            extend_match(
                1, // start from chain index 1
                end_v,
                &bindings,
                &path_elems,
                first_entry.path_variable.as_deref(),
                m,
                graph,
                &mut stats,
                &mut extended_rows,
                q.where_clause.as_ref(),
                pushdown_limit,
                limits,
            )?;
        }
        seed_rows = extended_rows;
    }

    // Apply any_paths / keep clause from first MATCH entry.
    if let Some(k) = first_entry.any_paths {
        seed_rows.truncate(k as usize);
    }
    if let Some(ref keep) = first_entry.keep_clause {
        apply_keep_clause(&mut seed_rows, keep);
    }

    // Process remaining MATCH clauses (2nd, 3rd, ...).
    let mut rows = seed_rows;
    if q.match_clauses.len() > 1 {
        let pushdown_limit = if q.order_by.is_none() {
            q.limit.map(|l| l.0 as usize)
        } else {
            None
        };
        let default_remaining: Vec<usize> = (1..q.match_clauses.len()).collect();
        let remaining_order = plan
            .annotations
            .match_clause_order
            .as_ref()
            .map(|o| o.iter().copied().filter(|&i| i != 0).collect::<Vec<_>>())
            .unwrap_or(default_remaining);
        let total_remaining = remaining_order.len();
        for (step, &clause_idx) in remaining_order.iter().enumerate() {
            let entry = &q.match_clauses[clause_idx];
            let apply_where = if step + 1 == total_remaining {
                q.where_clause.as_ref()
            } else {
                None
            };
            rows = execute_match_clause_joined(
                &entry.pattern,
                entry.shortest,
                entry.shortest_mode,
                entry.path_variable.as_deref(),
                entry.path_mode,
                &rows,
                graph,
                &mut stats,
                apply_where,
                pushdown_limit,
                limits,
                entry.optional,
            )?;
            if let Some(k) = entry.any_paths {
                rows.truncate(k as usize);
            }
            if let Some(ref keep) = entry.keep_clause {
                apply_keep_clause(&mut rows, keep);
            }
        }
    } else {
        // Single MATCH: apply WHERE filter now.
        if let Some(w) = q.where_clause.as_ref() {
            rows.retain(|b| eval_where(w, b, graph));
        }
    }
    stats.breakdown.rows_after_match = rows.len() as u64;

    // Apply WITH clauses.
    rows = apply_with_clauses(q, rows, graph, &mut stats, limits)?;
    stats.breakdown.rows_after_with = rows.len() as u64;

    // ORDER BY + LIMIT + projection — delegate to common path.
    let is_agg = query_has_aggregate(q);
    if let Some(order_by) = &q.order_by
        && !is_agg
    {
        bump_steps(&mut stats, rows.len() as u64, limits)?;
        if let Some(limit) = q.limit {
            rows = top_k_rows(rows, order_by, None, limit.0 as usize, graph);
        } else {
            rows.sort_by(|a, b| compare_rows_for_order(order_by, a, b, None, graph));
        }
    }
    if let Some(limit) = q.limit
        && !is_agg
    {
        rows.truncate(limit.0 as usize);
    }
    stats.breakdown.rows_before_projection = rows.len() as u64;
    let columns = if q.return_clause.star {
        star_columns(&rows)
    } else {
        q.return_clause.items.iter().map(column_name).collect()
    };
    let projected_rows = if q.return_clause.star {
        rows.iter().map(project_star_row).collect()
    } else if is_agg {
        project_aggregated_rows(q, &rows, graph, build_hasher, Some(&mut stats))?
    } else {
        rows.iter()
            .map(|bindings| {
                q.return_clause
                    .items
                    .iter()
                    .map(|item| eval_expr(&item.expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    stats.rows_emitted = projected_rows.len() as u64;
    if let Some(offset) = q.offset {
        let mut pr = projected_rows;
        let off = offset as usize;
        if off >= pr.len() {
            pr.clear();
        } else {
            pr.drain(0..off);
        }
        return Ok(Some(QueryResult {
            columns,
            rows: pr,
            stats,
            warnings: vec![],
        }));
    }
    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

fn flip_direction(dir: Direction) -> Direction {
    match dir {
        Direction::Outgoing => Direction::Incoming,
        Direction::Incoming => Direction::Outgoing,
        Direction::Either => Direction::Either,
    }
}

/// Reverse-traverse chains from the anchor vertex back to the start node.
///
/// Given an anchor at `pattern.elements[anchor_chain_idx]` (Hop node), this walks
/// backward through elements[anchor_chain_idx] → ... → elements[0] → start,
/// reversing edge directions at each step.  Returns fully-bound rows that
/// include the start variable and all intermediate chain variables (but NOT
/// the anchor variable itself — the caller binds that).
#[allow(clippy::too_many_arguments)]
fn reverse_traverse_to_start<M: Memory>(
    anchor_vertex: u32,
    anchor_chain_idx: usize,
    pattern: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    limits: ExecutionLimits,
    max_rows: Option<usize>,
) -> Result<Vec<Bindings>, GleaphError> {
    // Begin with a single row containing only the anchor vertex (the caller
    // will bind the anchor variable afterwards).
    let mut current_rows = vec![Bindings::new()];
    // The "current vertex" for each row at each reverse step.
    // Initially every row's current vertex is the anchor.
    let mut current_verts: Vec<u32> = vec![anchor_vertex];

    // Walk backward: step k, k-1, ..., 0
    for step in (0..=anchor_chain_idx).rev() {
        let chain = pattern.chain(step);
        let target_node_pattern = if step == 0 {
            &pattern.start
        } else {
            &pattern.chain(step - 1).node
        };
        let flipped = flip_direction(chain.edge.direction);
        let resolved_label = resolve_edge_label(&chain.edge, graph);
        let reverse_label_filter = match &resolved_label {
            ResolvedEdgeLabel::Exact(id) => Some(*id),
            _ => None,
        };

        let mut next_rows = Vec::new();
        let mut next_verts = Vec::new();

        for (row_idx, row) in current_rows.iter().enumerate() {
            if let Some(limit) = max_rows
                && next_rows.len() >= limit
            {
                break;
            }
            let cv = current_verts[row_idx];
            bump_steps(stats, 1, limits)?;

            // Collect candidate edges based on flipped direction.
            // Flipped Outgoing means the original was Incoming, so we now go outgoing.
            // Flipped Incoming means the original was Outgoing, so we now go incoming (reverse_neighbors).
            match flipped {
                Direction::Outgoing | Direction::Either => {
                    // Outgoing neighbors of cv
                    graph.for_each_neighbor_filtered(cv, reverse_label_filter, None, &mut |edge| {
                        bump_steps(stats, 1, limits)?;
                        stats.scanned_edges += 1;
                        stats.breakdown.outgoing_hop_candidates = stats
                            .breakdown
                            .outgoing_hop_candidates
                            .saturating_add(1);
                        if graph.is_vertex_tombstoned(edge.target) {
                            return Ok::<(), GleaphError>(());
                        }
                        if !resolved_label.matches(edge.label_id()) {
                            stats.breakdown.hop_label_rejects = stats
                                .breakdown
                                .hop_label_rejects
                                .saturating_add(1);
                            stats.breakdown.outgoing_hop_label_rejects = stats
                                .breakdown
                                .outgoing_hop_label_rejects
                                .saturating_add(1);
                            return Ok::<(), GleaphError>(());
                        }
                        if edge.is_tombstoned() {
                            return Ok::<(), GleaphError>(());
                        }
                        let label_ref = graph.label_name_by_id(edge.label_id());
                        stats.breakdown.reverse_node_match_checks = stats
                            .breakdown
                            .reverse_node_match_checks
                            .saturating_add(1);
                        if !node_matches(target_node_pattern, edge.target, graph) {
                            return Ok::<(), GleaphError>(());
                        }
                        let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
                        let mut next_b = row.clone();
                        stats.breakdown.reverse_row_clones = stats
                            .breakdown
                            .reverse_row_clones
                            .saturating_add(1);
                        if let Some(edge_var) = &chain.edge.var {
                            // For the reverse direction the physical (src,dst) in the
                            // original pattern is (target_node → cv) for outgoing,
                            // but we're traversing cv → target_node.  Keep the original
                            // direction's src/dst: original Incoming means src=target, dst=cv.
                            let (esrc, edst) = match chain.edge.direction {
                                Direction::Outgoing => (edge.target, cv),
                                Direction::Incoming => (cv, edge.target),
                                Direction::Either => (cv, edge.target),
                            };
                            next_b.insert(
                                edge_var.clone(),
                                Binding::Edge {
                                    src: esrc,
                                    dst: edst,
                                    label: label_arc,
                                    edge_id: edge.edge_id,
                                    weight: edge.weight,
                                    timestamp: edge.timestamp,
                                },
                            );
                        }
                        if let Some(node_var) = &target_node_pattern.var {
                            next_b.insert(node_var.clone(), Binding::Vertex(edge.target));
                        }
                        if !eval_where_partial_pushdown(where_clause, &next_b, graph) {
                            return Ok::<(), GleaphError>(());
                        }
                        next_verts.push(edge.target);
                        next_rows.push(next_b);
                        if let Some(limit) = max_rows && next_rows.len() >= limit {
                            return Ok::<(), GleaphError>(());
                        }
                        Ok::<(), GleaphError>(())
                    })?;
                    // For Either, also check incoming.
                    if flipped == Direction::Either {
                        let mut hit_cap = false;
                        graph.for_each_reverse_neighbor(
                            cv,
                            reverse_label_filter,
                            None,
                            &mut |rev| {
                                bump_steps(stats, 1, limits)?;
                                stats.scanned_edges += 1;
                                stats.breakdown.incoming_hop_candidates = stats
                                    .breakdown
                                    .incoming_hop_candidates
                                    .saturating_add(1);
                                if !resolved_label.matches(rev.label_id()) {
                                    stats.breakdown.hop_label_rejects = stats
                                        .breakdown
                                        .hop_label_rejects
                                        .saturating_add(1);
                                    stats.breakdown.incoming_hop_label_rejects = stats
                                        .breakdown
                                        .incoming_hop_label_rejects
                                        .saturating_add(1);
                                    return Ok(());
                                }
                                let label_ref = graph.label_name_by_id(rev.label_id());
                                stats.breakdown.reverse_node_match_checks = stats
                                    .breakdown
                                    .reverse_node_match_checks
                                    .saturating_add(1);
                                if !node_matches(target_node_pattern, rev.src, graph) {
                                    return Ok(());
                                }
                                let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
                                let mut next_b = row.clone();
                                stats.breakdown.reverse_row_clones = stats
                                    .breakdown
                                    .reverse_row_clones
                                    .saturating_add(1);
                                if let Some(edge_var) = &chain.edge.var {
                                    next_b.insert(
                                        edge_var.clone(),
                                        Binding::Edge {
                                            src: rev.src,
                                            dst: cv,
                                            label: label_arc,
                                            edge_id: rev.edge_id,
                                            weight: rev.weight,
                                            timestamp: rev.timestamp,
                                        },
                                    );
                                }
                                if let Some(node_var) = &target_node_pattern.var {
                                    next_b.insert(node_var.clone(), Binding::Vertex(rev.src));
                                }
                                if !eval_where_partial_pushdown(where_clause, &next_b, graph) {
                                    return Ok(());
                                }
                                next_verts.push(rev.src);
                                next_rows.push(next_b);
                                if let Some(limit) = max_rows
                                    && next_rows.len() >= limit
                                {
                                    hit_cap = true;
                                }
                                Ok(())
                            },
                        )?;
                        if hit_cap {
                            break;
                        }
                    }
                }
                Direction::Incoming => {
                    // Incoming edges to cv (reverse neighbors) — rich path avoids redundant checks
                    let mut hit_cap = false;
                    graph.for_each_reverse_neighbor(
                        cv,
                        reverse_label_filter,
                        None,
                        &mut |rev| {
                            bump_steps(stats, 1, limits)?;
                            stats.scanned_edges += 1;
                            stats.breakdown.incoming_hop_candidates = stats
                                .breakdown
                                .incoming_hop_candidates
                                .saturating_add(1);
                            if !resolved_label.matches(rev.label_id()) {
                                stats.breakdown.hop_label_rejects = stats
                                    .breakdown
                                    .hop_label_rejects
                                    .saturating_add(1);
                                stats.breakdown.incoming_hop_label_rejects = stats
                                    .breakdown
                                    .incoming_hop_label_rejects
                                    .saturating_add(1);
                                return Ok(());
                            }
                            let label_ref = graph.label_name_by_id(rev.label_id());
                            stats.breakdown.reverse_node_match_checks = stats
                                .breakdown
                                .reverse_node_match_checks
                                .saturating_add(1);
                            if !node_matches(target_node_pattern, rev.src, graph) {
                                return Ok(());
                            }
                            let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
                            let mut next_b = row.clone();
                            stats.breakdown.reverse_row_clones = stats
                                .breakdown
                                .reverse_row_clones
                                .saturating_add(1);
                            if let Some(edge_var) = &chain.edge.var {
                                // Original direction was Outgoing (flipped = Incoming),
                                // so src/dst in the original pattern: (target → cv)
                                next_b.insert(
                                    edge_var.clone(),
                                    Binding::Edge {
                                        src: rev.src,
                                        dst: cv,
                                        label: label_arc,
                                        edge_id: rev.edge_id,
                                        weight: rev.weight,
                                        timestamp: rev.timestamp,
                                    },
                                );
                            }
                            if let Some(node_var) = &target_node_pattern.var {
                                next_b.insert(node_var.clone(), Binding::Vertex(rev.src));
                            }
                            if !eval_where_partial_pushdown(where_clause, &next_b, graph) {
                                return Ok(());
                            }
                            next_verts.push(rev.src);
                            next_rows.push(next_b);
                            if let Some(limit) = max_rows
                                && next_rows.len() >= limit
                            {
                                hit_cap = true;
                            }
                            Ok(())
                        },
                    )?;
                    if hit_cap {
                        break;
                    }
                }
            }
        }
        current_rows = next_rows;
        current_verts = next_verts;
    }
    Ok(current_rows)
}

fn execute_shortest_plan_query<M: Memory>(
    plan: &PhysicalPlan,
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<Option<QueryResult>, GleaphError> {
    let _ = plan;
    let supported = q.with_clauses.is_empty()
        && q.group_by.is_none()
        && q.having.is_none()
        && !q.return_clause.distinct
        && q.order_by.is_none()
        && q.limit.is_none()
        && q.offset.is_none()
        && q.match_clauses.len() == 1
        && q.match_clauses[0].shortest;
    if !supported {
        return Ok(None);
    }

    let mut stats = QueryStats::default();
    let rows =
        execute_query_match_entries(q, graph, &mut stats, q.where_clause.as_ref(), None, limits)?;
    stats.breakdown.rows_after_match = rows.len() as u64;
    stats.breakdown.rows_after_with = rows.len() as u64;
    stats.breakdown.rows_before_projection = rows.len() as u64;
    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();
    let projected_rows = rows
        .iter()
        .map(|bindings| {
            q.return_clause
                .items
                .iter()
                .map(|item| eval_expr(&item.expr, bindings, graph))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    stats.rows_emitted = projected_rows.len() as u64;
    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

fn execute_aggregate_plan_query<M: Memory, S: BuildHasher>(
    plan: &PhysicalPlan,
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    let _ = plan;
    let supported =
        q.with_clauses.is_empty() && query_has_aggregate(q) && q.match_clauses.len() == 1;
    if !supported {
        return Ok(None);
    }
    if let Some(result) =
        execute_two_hop_top_k_count_by_terminal_key_query(q, graph, limits, build_hasher)?
    {
        return Ok(Some(result));
    }
    if let Some(result) = execute_top_k_count_by_endpoint_key_query(q, graph, limits, build_hasher)? {
        return Ok(Some(result));
    }
    if let Some(result) = execute_aggregate_query_fast(q, graph, limits, build_hasher)? {
        return Ok(Some(result));
    }

    let mut stats = QueryStats::default();
    let rows =
        execute_query_match_entries(q, graph, &mut stats, q.where_clause.as_ref(), None, limits)?;
    stats.breakdown.rows_after_match = rows.len() as u64;
    stats.breakdown.rows_after_with = rows.len() as u64;
    stats.breakdown.rows_before_projection = rows.len() as u64;
    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();

    let mut projected_rows =
        project_aggregated_rows(q, &rows, graph, build_hasher, Some(&mut stats))?;
    if q.return_clause.distinct {
        let mut seen = BTreeSet::new();
        if q.order_by.is_none()
            && let Some(limit) = q.limit
        {
            let k = limit.0 as usize;
            let mut deduped = Vec::with_capacity(k);
            for row in projected_rows.into_iter() {
                if seen.insert(format!("{row:?}")) {
                    deduped.push(row);
                    if deduped.len() >= k {
                        break;
                    }
                }
            }
            projected_rows = deduped;
        } else {
            projected_rows.retain(|row| seen.insert(format!("{row:?}")));
        }
    }
    if let Some(order_by) = &q.order_by {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    if let Some(offset) = q.offset {
        let off = offset as usize;
        if off >= projected_rows.len() {
            projected_rows.clear();
        } else {
            projected_rows.drain(0..off);
        }
    }
    stats.rows_emitted = projected_rows.len() as u64;
    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

fn extract_terminal_key_ne_literal(
    where_clause: Option<&WhereClause>,
    terminal_var: &str,
    key_property: &str,
) -> Option<Option<Value>> {
    let Some(expr) = where_clause else {
        return Some(None);
    };
    let matches_key_prop = |expr: &Expr| {
        matches!(
            expr,
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == terminal_var)
                    && property == key_property
        )
    };
    match expr {
        Expr::Compare { left, op: CmpOp::Ne, right } if matches_key_prop(left) => {
            eval_literal_expr(right).map(Some)
        }
        Expr::Compare { left, op: CmpOp::Ne, right } if matches_key_prop(right) => {
            eval_literal_expr(left).map(Some)
        }
        _ => None,
    }
}

/// Fast path for 2-hop terminal-grouped top-k count queries.
///
/// Supported shape:
/// `MATCH (a)-[label1?]->(b)-[label2?]->(c) RETURN c.prop, COUNT(*) ORDER BY COUNT(*) DESC LIMIT k`
/// Optionally with a single terminal-key exclusion predicate:
/// `WHERE c.prop <> <literal>`
struct TwoHopTopKCountSpec<'a> {
    entry: &'a crate::ast::MatchEntry,
    first: &'a MatchChain,
    second: &'a MatchChain,
    key_idx: usize,
    agg_idx: usize,
    key_property: &'a str,
    excluded_key: Option<Value>,
}

struct TwoHopCountByMiddleSpec<'a> {
    entry: &'a crate::ast::MatchEntry,
    first: &'a MatchChain,
    second: &'a MatchChain,
    prop_items: Vec<(usize, &'a str)>,
    agg_idx: usize,
}

struct SeededTopKCountByTerminalSpec<'a> {
    seed_match: &'a crate::ast::MatchEntry,
    with_clause: &'a crate::ast::WithClause,
    continuation: &'a crate::ast::MatchEntry,
    key_idx: usize,
    agg_idx: usize,
    key_property: &'a str,
    edge_ts_range: Option<TimestampRange>,
}

struct ReverseTwoHopTopKCountByTerminalSpec<'a> {
    entry: &'a crate::ast::MatchEntry,
    first: &'a MatchChain,
    second: &'a MatchChain,
    key_idx: usize,
    agg_idx: usize,
    key_property: &'a str,
    excluded_property: Option<&'a str>,
    excluded_value: Option<Value>,
}

struct SeededSegmentationSpec<'a> {
    seed_match: &'a crate::ast::MatchEntry,
    with_clause: &'a crate::ast::WithClause,
    continuation: &'a crate::ast::MatchEntry,
    group_idx: usize,
    group_property: &'a str,
    distinct_seed_idx: usize,
    distinct_target_idx: usize,
    case_sum_idx: usize,
    case_cmp_property: &'a str,
    case_cmp_op: CmpOp,
    case_cmp_literal: Value,
    case_then_literal: Value,
    case_else_literal: Value,
    avg_idx: usize,
    avg_property: &'a str,
}

struct SeededVerifiedInfluenceSpec<'a> {
    seed_match: &'a crate::ast::MatchEntry,
    with_clause: &'a crate::ast::WithClause,
    first: &'a MatchChain,
    second: &'a MatchChain,
    group_idx: usize,
    group_property: &'a str,
    collect_idx: usize,
    collect_property: &'a str,
    count_idx: usize,
}

fn classify_two_hop_top_k_count_by_terminal_key_query<'a>(
    q: &'a QueryStmt,
) -> Result<TwoHopTopKCountSpec<'a>, &'static str> {
    if q.return_clause.star {
        return Err("return_star");
    }
    if q.return_clause.distinct {
        return Err("return_distinct");
    }
    if !q.with_clauses.is_empty() {
        return Err("with_clauses");
    }
    if q.having.is_some() {
        return Err("having");
    }
    if q.offset.is_some() {
        return Err("offset");
    }
    if q.group_by.is_some() {
        return Err("group_by");
    }
    if q.match_clauses.len() != 1 {
        return Err("match_clause_count");
    }
    let entry = &q.match_clauses[0];
    if entry.optional {
        return Err("optional");
    }
    if entry.shortest {
        return Err("shortest");
    }
    if entry.pattern.elements.len() != 2 {
        return Err("pattern_len");
    }
    let first = entry.pattern.chain(0);
    let second = entry.pattern.chain(1);
    if first.edge.direction != Direction::Outgoing || second.edge.direction != Direction::Outgoing {
        return Err("direction");
    }
    if first.edge.length != PathLength::Fixed(1) || second.edge.length != PathLength::Fixed(1) {
        return Err("path_length");
    }
    if !first.edge.properties.is_empty() || !second.edge.properties.is_empty() {
        return Err("edge_properties");
    }
    if first.edge.where_clause.is_some() || second.edge.where_clause.is_some() {
        return Err("edge_where");
    }
    let Some(terminal_var) = second.node.var.as_deref() else {
        return Err("terminal_var");
    };
    if q.return_clause.items.len() != 2 {
        return Err("return_item_count");
    }

    let mut key_idx = None;
    let mut agg_idx = None;
    let mut key_property = None;
    for (idx, item) in q.return_clause.items.iter().enumerate() {
        match &item.expr {
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Count
                    && agg.count_all
                    && !agg.distinct
                    && agg.expr.is_none() =>
            {
                agg_idx = Some(idx);
            }
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == terminal_var) =>
            {
                key_idx = Some(idx);
                key_property = Some(property.as_str());
            }
            _ => return Err("return_item_shape"),
        }
    }
    let (Some(key_idx), Some(agg_idx), Some(key_property)) = (key_idx, agg_idx, key_property)
    else {
        return Err("return_projection");
    };
    let excluded_key =
        match extract_terminal_key_ne_literal(q.where_clause.as_ref(), terminal_var, key_property) {
            Some(value) => value,
            None => return Err("where_shape"),
        };

    Ok(TwoHopTopKCountSpec {
        entry,
        first,
        second,
        key_idx,
        agg_idx,
        key_property,
        excluded_key,
    })
}

pub fn debug_two_hop_top_k_count_by_terminal_key_query_shape(q: &QueryStmt) -> &'static str {
    match classify_two_hop_top_k_count_by_terminal_key_query(q) {
        Ok(_) => "supported",
        Err(reason) => reason,
    }
}

fn classify_two_hop_count_by_middle_vertex_query<'a>(
    q: &'a QueryStmt,
) -> Result<TwoHopCountByMiddleSpec<'a>, &'static str> {
    if q.return_clause.star {
        return Err("return_star");
    }
    if q.return_clause.distinct {
        return Err("return_distinct");
    }
    if !q.with_clauses.is_empty() {
        return Err("with_clauses");
    }
    if q.where_clause.is_some() {
        return Err("where_clause");
    }
    if q.having.is_some() {
        return Err("having");
    }
    if q.offset.is_some() {
        return Err("offset");
    }
    if q.group_by.is_some() {
        return Err("group_by");
    }
    if q.match_clauses.len() != 1 {
        return Err("match_clause_count");
    }
    let entry = &q.match_clauses[0];
    if entry.optional {
        return Err("optional");
    }
    if entry.shortest {
        return Err("shortest");
    }
    if entry.pattern.elements.len() != 2 {
        return Err("pattern_len");
    }
    let first = entry.pattern.chain(0);
    let second = entry.pattern.chain(1);
    if first.edge.direction != Direction::Outgoing || second.edge.direction != Direction::Outgoing {
        return Err("direction");
    }
    if first.edge.length != PathLength::Fixed(1) || second.edge.length != PathLength::Fixed(1) {
        return Err("path_length");
    }
    if !first.edge.properties.is_empty() || !second.edge.properties.is_empty() {
        return Err("edge_properties");
    }
    if first.edge.where_clause.is_some() || second.edge.where_clause.is_some() {
        return Err("edge_where");
    }
    let Some(middle_var) = first.node.var.as_deref() else {
        return Err("middle_var");
    };
    if q.return_clause.items.len() < 2 {
        return Err("return_item_count");
    }

    let mut prop_items = Vec::new();
    let mut agg_idx = None;
    for (idx, item) in q.return_clause.items.iter().enumerate() {
        match &item.expr {
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Count
                    && agg.count_all
                    && !agg.distinct
                    && agg.expr.is_none() =>
            {
                if agg_idx.is_some() {
                    return Err("multiple_aggregates");
                }
                agg_idx = Some(idx);
            }
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == middle_var) =>
            {
                prop_items.push((idx, property.as_str()));
            }
            _ => return Err("return_item_shape"),
        }
    }
    let Some(agg_idx) = agg_idx else {
        return Err("missing_aggregate");
    };
    if prop_items.is_empty() {
        return Err("missing_projection");
    }

    Ok(TwoHopCountByMiddleSpec {
        entry,
        first,
        second,
        prop_items,
        agg_idx,
    })
}

fn classify_seeded_top_k_count_by_terminal_key_query<'a>(
    q: &'a QueryStmt,
) -> Result<SeededTopKCountByTerminalSpec<'a>, &'static str> {
    if q.return_clause.star {
        return Err("return_star");
    }
    if q.return_clause.distinct {
        return Err("return_distinct");
    }
    if q.where_clause.is_some() {
        return Err("where_clause");
    }
    if q.having.is_some() {
        return Err("having");
    }
    if q.offset.is_some() {
        return Err("offset");
    }
    if q.group_by.is_some() {
        return Err("group_by");
    }
    if q.match_clauses.len() != 1 || q.with_clauses.len() != 1 {
        return Err("seed_shape");
    }
    let seed_match = &q.match_clauses[0];
    if seed_match.optional || seed_match.shortest || !seed_match.pattern.elements.is_empty() {
        return Err("seed_match");
    }
    let Some(seed_var) = seed_match.pattern.start.var.as_deref() else {
        return Err("seed_var");
    };

    let with_clause = &q.with_clauses[0];
    if with_clause.distinct
        || with_clause.star
        || with_clause.where_clause.is_some()
        || with_clause.order_by.is_some()
        || with_clause.offset.is_some()
    {
        return Err("with_shape");
    }
    if with_clause.items.len() != 1 {
        return Err("with_items");
    }
    match &with_clause.items[0].expr {
        Expr::Variable(var) if var == seed_var && with_clause.items[0].alias.is_none() => {}
        _ => return Err("with_projection"),
    }
    let Some(_seed_limit) = with_clause.limit else {
        return Err("with_limit");
    };
    if with_clause.match_clauses.len() != 1 {
        return Err("continuation_count");
    }
    let continuation = &with_clause.match_clauses[0];
    if continuation.optional || continuation.shortest {
        return Err("continuation_shape");
    }
    if continuation.pattern.start.var.as_deref() != Some(seed_var) {
        return Err("continuation_seed_var");
    }
    if continuation.pattern.elements.len() != 1 {
        return Err("continuation_pattern");
    }
    let chain = continuation.pattern.chain(0);
    if chain.edge.direction != Direction::Outgoing || chain.edge.length != PathLength::Fixed(1) {
        return Err("continuation_edge_shape");
    }
    if !chain.edge.properties.is_empty() || chain.edge.where_clause.is_some() {
        return Err("continuation_edge_filter");
    }
    let Some(target_var) = chain.node.var.as_deref() else {
        return Err("target_var");
    };
    if q.return_clause.items.len() != 2 {
        return Err("return_item_count");
    }

    let mut key_idx = None;
    let mut agg_idx = None;
    let mut key_property = None;
    for (idx, item) in q.return_clause.items.iter().enumerate() {
        match &item.expr {
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Count
                    && agg.count_all
                    && !agg.distinct
                    && agg.expr.is_none() =>
            {
                agg_idx = Some(idx);
            }
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == target_var) =>
            {
                key_idx = Some(idx);
                key_property = Some(property.as_str());
            }
            _ => return Err("return_item_shape"),
        }
    }
    let (Some(key_idx), Some(agg_idx), Some(key_property)) = (key_idx, agg_idx, key_property)
    else {
        return Err("return_projection");
    };

    let edge_ts_range = if let Some(expr) = with_clause.post_match_where.as_ref() {
        let Some(edge_var) = chain.edge.var.as_deref() else {
            return Err("post_match_where_without_edge_var");
        };
        if strip_edge_ts_predicates(expr, &[edge_var]).is_some() {
            return Err("post_match_where_shape");
        }
        extract_edge_ts_range_from_expr(expr, edge_var)
    } else {
        None
    };

    Ok(SeededTopKCountByTerminalSpec {
        seed_match,
        with_clause,
        continuation,
        key_idx,
        agg_idx,
        key_property,
        edge_ts_range,
    })
}

fn classify_reverse_two_hop_top_k_count_by_terminal_key_query<'a>(
    q: &'a QueryStmt,
) -> Result<ReverseTwoHopTopKCountByTerminalSpec<'a>, &'static str> {
    if q.return_clause.star {
        return Err("return_star");
    }
    if q.return_clause.distinct {
        return Err("return_distinct");
    }
    if !q.with_clauses.is_empty() {
        return Err("with_clauses");
    }
    if q.having.is_some() {
        return Err("having");
    }
    if q.offset.is_some() {
        return Err("offset");
    }
    if q.group_by.is_some() {
        return Err("group_by");
    }
    if q.match_clauses.len() != 1 {
        return Err("match_clause_count");
    }
    let entry = &q.match_clauses[0];
    if entry.optional || entry.shortest {
        return Err("match_shape");
    }
    if entry.pattern.elements.len() != 2 {
        return Err("pattern_len");
    }
    let first = entry.pattern.chain(0);
    let second = entry.pattern.chain(1);
    if first.edge.direction != Direction::Incoming || second.edge.direction != Direction::Outgoing {
        return Err("direction");
    }
    if first.edge.length != PathLength::Fixed(1) || second.edge.length != PathLength::Fixed(1) {
        return Err("path_length");
    }
    if !first.edge.properties.is_empty() || !second.edge.properties.is_empty() {
        return Err("edge_properties");
    }
    if first.edge.where_clause.is_some() || second.edge.where_clause.is_some() {
        return Err("edge_where");
    }
    let Some(terminal_var) = second.node.var.as_deref() else {
        return Err("terminal_var");
    };
    if q.return_clause.items.len() != 2 {
        return Err("return_item_count");
    }

    let mut key_idx = None;
    let mut agg_idx = None;
    let mut key_property = None;
    for (idx, item) in q.return_clause.items.iter().enumerate() {
        match &item.expr {
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Count
                    && agg.count_all
                    && !agg.distinct
                    && agg.expr.is_none() =>
            {
                agg_idx = Some(idx);
            }
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == terminal_var) =>
            {
                key_idx = Some(idx);
                key_property = Some(property.as_str());
            }
            _ => return Err("return_item_shape"),
        }
    }
    let (Some(key_idx), Some(agg_idx), Some(key_property)) = (key_idx, agg_idx, key_property)
    else {
        return Err("return_projection");
    };

    let (excluded_property, excluded_value) = match q.where_clause.as_ref() {
        None => (None, None),
        Some(Expr::Compare { left, op: CmpOp::Ne, right }) => {
            if let Expr::PropertyAccess { target, property } = left.as_ref()
                && matches!(target.as_ref(), Expr::Variable(var) if var == terminal_var)
            {
                (Some(property.as_str()), eval_literal_expr(right))
            } else if let Expr::PropertyAccess { target, property } = right.as_ref()
                && matches!(target.as_ref(), Expr::Variable(var) if var == terminal_var)
            {
                (Some(property.as_str()), eval_literal_expr(left))
            } else {
                return Err("where_shape");
            }
        }
        Some(_) => return Err("where_shape"),
    };

    Ok(ReverseTwoHopTopKCountByTerminalSpec {
        entry,
        first,
        second,
        key_idx,
        agg_idx,
        key_property,
        excluded_property,
        excluded_value,
    })
}

fn classify_seeded_segmentation_query<'a>(
    q: &'a QueryStmt,
) -> Result<SeededSegmentationSpec<'a>, &'static str> {
    if q.return_clause.star
        || q.return_clause.distinct
        || q.where_clause.is_some()
        || q.having.is_some()
        || q.offset.is_some()
        || q.group_by.is_some()
        || q.match_clauses.len() != 1
        || q.with_clauses.len() != 1
    {
        return Err("query_shape");
    }
    let seed_match = &q.match_clauses[0];
    if seed_match.optional || seed_match.shortest || !seed_match.pattern.elements.is_empty() {
        return Err("seed_match");
    }
    let Some(seed_var) = seed_match.pattern.start.var.as_deref() else {
        return Err("seed_var");
    };
    let with_clause = &q.with_clauses[0];
    if with_clause.distinct
        || with_clause.star
        || with_clause.where_clause.is_some()
        || with_clause.order_by.is_some()
        || with_clause.offset.is_some()
        || with_clause.post_match_where.is_some()
        || with_clause.items.len() != 1
    {
        return Err("with_shape");
    }
    match &with_clause.items[0].expr {
        Expr::Variable(var) if var == seed_var && with_clause.items[0].alias.is_none() => {}
        _ => return Err("with_projection"),
    }
    if with_clause.limit.is_none() || with_clause.match_clauses.len() != 1 {
        return Err("with_limit_or_continuation");
    }
    let continuation = &with_clause.match_clauses[0];
    if continuation.optional
        || continuation.shortest
        || continuation.pattern.start.var.as_deref() != Some(seed_var)
        || continuation.pattern.elements.len() != 1
    {
        return Err("continuation_shape");
    }
    let chain = continuation.pattern.chain(0);
    if chain.edge.direction != Direction::Outgoing
        || chain.edge.length != PathLength::Fixed(1)
        || !chain.edge.properties.is_empty()
        || chain.edge.where_clause.is_some()
    {
        return Err("continuation_edge");
    }
    let Some(target_var) = chain.node.var.as_deref() else {
        return Err("target_var");
    };
    if q.return_clause.items.len() != 5 {
        return Err("return_item_count");
    }

    let mut group_idx = None;
    let mut group_property = None;
    let mut distinct_seed_idx = None;
    let mut distinct_target_idx = None;
    let mut case_sum_idx = None;
    let mut case_cmp_property = None;
    let mut case_cmp_op = None;
    let mut case_cmp_literal = None;
    let mut case_then_literal = None;
    let mut case_else_literal = None;
    let mut avg_idx = None;
    let mut avg_property = None;

    for (idx, item) in q.return_clause.items.iter().enumerate() {
        match &item.expr {
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == target_var) =>
            {
                if group_idx.is_some() {
                    return Err("multiple_group_keys");
                }
                group_idx = Some(idx);
                group_property = Some(property.as_str());
            }
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Count
                    && agg.distinct
                    && matches!(agg.expr.as_deref(), Some(Expr::Variable(var)) if var == seed_var) =>
            {
                distinct_seed_idx = Some(idx);
            }
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Count
                    && agg.distinct
                    && matches!(agg.expr.as_deref(), Some(Expr::Variable(var)) if var == target_var) =>
            {
                distinct_target_idx = Some(idx);
            }
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Avg
                    && !agg.distinct
                    && matches!(agg.expr.as_deref(), Some(Expr::PropertyAccess { target, .. }) if matches!(target.as_ref(), Expr::Variable(var) if var == target_var)) =>
            {
                if let Some(Expr::PropertyAccess { property, .. }) = agg.expr.as_deref() {
                    avg_idx = Some(idx);
                    avg_property = Some(property.as_str());
                }
            }
            Expr::Aggregate(agg) if agg.func == AggFunc::Sum && !agg.distinct => {
                let Some(Expr::Case(case_expr)) = agg.expr.as_deref() else {
                    return Err("sum_shape");
                };
                if case_expr.operand.is_some() || case_expr.when_then.len() != 1 {
                    return Err("sum_case_shape");
                }
                let wt = &case_expr.when_then[0];
                let Expr::Compare { left, op, right } = &wt.when else {
                    return Err("sum_case_when");
                };
                let (cmp_property, cmp_literal) = match (left.as_ref(), right.as_ref()) {
                    (
                        Expr::PropertyAccess { target, property },
                        other,
                    ) if matches!(target.as_ref(), Expr::Variable(var) if var == target_var) => {
                        (property.as_str(), eval_literal_expr(other))
                    }
                    (
                        other,
                        Expr::PropertyAccess { target, property },
                    ) if matches!(target.as_ref(), Expr::Variable(var) if var == target_var) => {
                        let flipped = match op {
                            CmpOp::Gt => CmpOp::Lt,
                            CmpOp::Ge => CmpOp::Le,
                            CmpOp::Lt => CmpOp::Gt,
                            CmpOp::Le => CmpOp::Ge,
                            x => *x,
                        };
                        case_cmp_op = Some(flipped);
                        (property.as_str(), eval_literal_expr(other))
                    }
                    _ => return Err("sum_case_when"),
                };
                case_sum_idx = Some(idx);
                case_cmp_property = Some(cmp_property);
                if case_cmp_op.is_none() {
                    case_cmp_op = Some(*op);
                }
                case_cmp_literal = cmp_literal;
                case_then_literal = eval_literal_expr(&wt.then);
                case_else_literal = case_expr.else_expr.as_deref().and_then(eval_literal_expr);
            }
            _ => return Err("return_item_shape"),
        }
    }

    Ok(SeededSegmentationSpec {
        seed_match,
        with_clause,
        continuation,
        group_idx: group_idx.ok_or("missing_group")?,
        group_property: group_property.ok_or("missing_group")?,
        distinct_seed_idx: distinct_seed_idx.ok_or("missing_distinct_seed")?,
        distinct_target_idx: distinct_target_idx.ok_or("missing_distinct_target")?,
        case_sum_idx: case_sum_idx.ok_or("missing_case_sum")?,
        case_cmp_property: case_cmp_property.ok_or("missing_case_cmp_property")?,
        case_cmp_op: case_cmp_op.ok_or("missing_case_cmp_op")?,
        case_cmp_literal: case_cmp_literal.ok_or("missing_case_cmp_literal")?,
        case_then_literal: case_then_literal.ok_or("missing_case_then_literal")?,
        case_else_literal: case_else_literal.ok_or("missing_case_else_literal")?,
        avg_idx: avg_idx.ok_or("missing_avg")?,
        avg_property: avg_property.ok_or("missing_avg_property")?,
    })
}

fn classify_seeded_verified_influence_query<'a>(
    q: &'a QueryStmt,
) -> Result<SeededVerifiedInfluenceSpec<'a>, &'static str> {
    if q.return_clause.star
        || q.return_clause.distinct
        || q.where_clause.is_some()
        || q.having.is_some()
        || q.offset.is_some()
        || q.group_by.is_some()
        || q.match_clauses.len() != 1
        || q.with_clauses.len() != 1
    {
        return Err("query_shape");
    }
    let seed_match = &q.match_clauses[0];
    if seed_match.optional || seed_match.shortest || !seed_match.pattern.elements.is_empty() {
        return Err("seed_match");
    }
    let Some(seed_var) = seed_match.pattern.start.var.as_deref() else {
        return Err("seed_var");
    };
    let with_clause = &q.with_clauses[0];
    if with_clause.distinct
        || with_clause.star
        || with_clause.where_clause.is_some()
        || with_clause.order_by.is_some()
        || with_clause.offset.is_some()
        || with_clause.post_match_where.is_some()
        || with_clause.items.len() != 1
        || with_clause.limit.is_none()
        || with_clause.match_clauses.len() != 1
    {
        return Err("with_shape");
    }
    match &with_clause.items[0].expr {
        Expr::Variable(var) if var == seed_var && with_clause.items[0].alias.is_none() => {}
        _ => return Err("with_projection"),
    }
    let continuation = &with_clause.match_clauses[0];
    if continuation.optional || continuation.shortest || continuation.pattern.start.var.as_deref() != Some(seed_var) {
        return Err("continuation_shape");
    }
    if continuation.pattern.elements.len() != 2 {
        return Err("continuation_len");
    }
    let first = continuation.pattern.chain(0);
    let second = continuation.pattern.chain(1);
    if first.edge.direction != Direction::Outgoing
        || second.edge.direction != Direction::Outgoing
        || first.edge.length != PathLength::Fixed(1)
        || second.edge.length != PathLength::Fixed(1)
        || !first.edge.properties.is_empty()
        || !second.edge.properties.is_empty()
        || first.edge.where_clause.is_some()
        || second.edge.where_clause.is_some()
    {
        return Err("continuation_edges");
    }
    let Some(mid_var) = first.node.var.as_deref() else {
        return Err("mid_var");
    };
    let Some(term_var) = second.node.var.as_deref() else {
        return Err("term_var");
    };
    if q.return_clause.items.len() != 3 {
        return Err("return_item_count");
    }

    let mut group_idx = None;
    let mut group_property = None;
    let mut collect_idx = None;
    let mut collect_property = None;
    let mut count_idx = None;
    for (idx, item) in q.return_clause.items.iter().enumerate() {
        match &item.expr {
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == seed_var) =>
            {
                group_idx = Some(idx);
                group_property = Some(property.as_str());
            }
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Collect
                    && agg.distinct
                    && matches!(agg.expr.as_deref(), Some(Expr::PropertyAccess { target, .. }) if matches!(target.as_ref(), Expr::Variable(var) if var == term_var)) =>
            {
                if let Some(Expr::PropertyAccess { property, .. }) = agg.expr.as_deref() {
                    collect_idx = Some(idx);
                    collect_property = Some(property.as_str());
                }
            }
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Count
                    && agg.distinct
                    && matches!(agg.expr.as_deref(), Some(Expr::Variable(var)) if var == term_var) =>
            {
                count_idx = Some(idx);
            }
            _ => return Err("return_item_shape"),
        }
    }
    let _ = mid_var;
    Ok(SeededVerifiedInfluenceSpec {
        seed_match,
        with_clause,
        first,
        second,
        group_idx: group_idx.ok_or("missing_group")?,
        group_property: group_property.ok_or("missing_group_property")?,
        collect_idx: collect_idx.ok_or("missing_collect")?,
        collect_property: collect_property.ok_or("missing_collect_property")?,
        count_idx: count_idx.ok_or("missing_count")?,
    })
}


fn execute_two_hop_top_k_count_by_terminal_key_query<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    let Ok(spec) = classify_two_hop_top_k_count_by_terminal_key_query(q) else {
        return Ok(None);
    };
    let entry = spec.entry;
    let first = spec.first;
    let second = spec.second;
    let key_idx = spec.key_idx;
    let agg_idx = spec.agg_idx;
    let key_property = spec.key_property;
    let excluded_key = spec.excluded_key;

    let mut stats = QueryStats::default();
    let start_candidates = initial_candidates(&entry.pattern.start, graph, &mut stats, limits)?;
    let first_label = resolve_edge_label(&first.edge, graph);
    let second_label = resolve_edge_label(&second.edge, graph);
    let resolved_mid = resolve_node_match(&first.node, graph);
    let resolved_terminal = resolve_node_match(&second.node, graph);
    let mut key_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut groups: Vec<(Value, u64)> = Vec::new();
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    let max_groups = effective_max_groups();

    for start_vertex in start_candidates {
        let first_total = graph.for_each_neighbor(start_vertex, None, &mut |edge1| {
            bump_steps(&mut stats, 1, limits)?;
            if edge1.is_tombstoned()
                || graph.is_vertex_tombstoned(edge1.target)
                || !first_label.matches(edge1.label_id())
                || !resolved_mid.matches_no_tombstone(&first.node, edge1.target, graph)
            {
                return Ok(());
            }

            let second_total = graph.for_each_neighbor(edge1.target, None, &mut |edge2| {
                bump_steps(&mut stats, 1, limits)?;
                if edge2.is_tombstoned()
                    || graph.is_vertex_tombstoned(edge2.target)
                    || !second_label.matches(edge2.label_id())
                    || !resolved_terminal.matches_no_tombstone(&second.node, edge2.target, graph)
                {
                    return Ok(());
                }

                let key_value = key_cache
                    .entry(edge2.target)
                    .or_insert_with(|| {
                        vertex_property_value_or_null(graph, edge2.target, key_property)
                    })
                    .clone();
                if excluded_key
                    .as_ref()
                    .is_some_and(|excluded| compare_values(excluded, &key_value) == Some(Ordering::Equal))
                {
                    return Ok(());
                }
                let h = hash_value_slice(std::slice::from_ref(&key_value), build_hasher);
                let bucket = group_index.entry(h).or_default();
                for &idx in bucket.iter() {
                    if groups[idx].0 == key_value {
                        groups[idx].1 = groups[idx].1.saturating_add(1);
                        return Ok(());
                    }
                }
                if groups.len() >= max_groups {
                    return Err(GleaphError::ExecutionError(format!(
                        "MAX_GROUPS exceeded ({max_groups})"
                    )));
                }
                let idx = groups.len();
                bucket.push(idx);
                groups.push((key_value, 1));
                Ok(())
            })?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(second_total);
            Ok(())
        })?;
        stats.scanned_edges = stats.scanned_edges.saturating_add(first_total);
    }

    stats.breakdown.rows_after_match = groups.len() as u64;
    stats.breakdown.rows_after_with = groups.len() as u64;
    stats.breakdown.rows_before_projection = groups.len() as u64;
    stats.breakdown.groups_formed = stats
        .breakdown
        .groups_formed
        .saturating_add(groups.len() as u64);

    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();
    let mut projected_rows = groups
        .into_iter()
        .map(|(key_value, count)| {
            let mut row = vec![Value::Null; 2];
            row[key_idx] = key_value;
            row[agg_idx] = Value::Int64(count as i64);
            row
        })
        .collect::<Vec<_>>();

    if let Some(order_by) = &q.order_by {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.rows_emitted = projected_rows.len() as u64;
    stats.breakdown.two_hop_top_k_count_fast_path_used = true;

    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

fn execute_two_hop_count_by_middle_vertex_query<M: Memory>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<Option<QueryResult>, GleaphError> {
    let Ok(spec) = classify_two_hop_count_by_middle_vertex_query(q) else {
        return Ok(None);
    };

    let mut stats = QueryStats::default();
    let start_candidates = initial_candidates(&spec.entry.pattern.start, graph, &mut stats, limits)?;
    let first_label = resolve_edge_label(&spec.first.edge, graph);
    let second_label = resolve_edge_label(&spec.second.edge, graph);
    let resolved_mid = resolve_node_match(&spec.first.node, graph);
    let resolved_terminal = resolve_node_match(&spec.second.node, graph);
    let mut groups: Vec<(u32, u64)> = Vec::new();
    let mut group_index: RapidHashMap<u32, usize> = RapidHashMap::default();
    let max_groups = effective_max_groups();

    for start_vertex in start_candidates {
        let first_total = graph.for_each_neighbor(start_vertex, None, &mut |edge1| {
            bump_steps(&mut stats, 1, limits)?;
            if edge1.is_tombstoned()
                || graph.is_vertex_tombstoned(edge1.target)
                || !first_label.matches(edge1.label_id())
                || !resolved_mid.matches_no_tombstone(&spec.first.node, edge1.target, graph)
            {
                return Ok(());
            }

            let second_total = graph.for_each_neighbor(edge1.target, None, &mut |edge2| {
                bump_steps(&mut stats, 1, limits)?;
                if edge2.is_tombstoned()
                    || graph.is_vertex_tombstoned(edge2.target)
                    || !second_label.matches(edge2.label_id())
                    || !resolved_terminal.matches_no_tombstone(&spec.second.node, edge2.target, graph)
                {
                    return Ok(());
                }

                if let Some(&idx) = group_index.get(&edge1.target) {
                    groups[idx].1 = groups[idx].1.saturating_add(1);
                } else {
                    if groups.len() >= max_groups {
                        return Err(GleaphError::ExecutionError(format!(
                            "MAX_GROUPS exceeded ({max_groups})"
                        )));
                    }
                    let idx = groups.len();
                    group_index.insert(edge1.target, idx);
                    groups.push((edge1.target, 1));
                }
                Ok(())
            })?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(second_total);
            Ok(())
        })?;
        stats.scanned_edges = stats.scanned_edges.saturating_add(first_total);
    }

    stats.breakdown.rows_after_match = groups.len() as u64;
    stats.breakdown.rows_after_with = groups.len() as u64;
    stats.breakdown.rows_before_projection = groups.len() as u64;
    stats.breakdown.groups_formed = stats
        .breakdown
        .groups_formed
        .saturating_add(groups.len() as u64);

    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();
    let mut projected_rows = groups
        .into_iter()
        .map(|(middle_vertex, count)| {
            let mut row = vec![Value::Null; q.return_clause.items.len()];
            for (idx, property) in &spec.prop_items {
                row[*idx] = vertex_property_value_or_null(graph, middle_vertex, property);
            }
            row[spec.agg_idx] = Value::Int64(count as i64);
            row
        })
        .collect::<Vec<_>>();

    if let Some(order_by) = &q.order_by {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.rows_emitted = projected_rows.len() as u64;

    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

fn execute_seeded_top_k_count_by_terminal_key_query<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    let Ok(spec) = classify_seeded_top_k_count_by_terminal_key_query(q) else {
        return Ok(None);
    };

    let mut stats = QueryStats::default();
    let mut start_candidates =
        initial_candidates(&spec.seed_match.pattern.start, graph, &mut stats, limits)?;
    start_candidates.truncate(spec.with_clause.limit.expect("validated").0 as usize);

    let chain = spec.continuation.pattern.chain(0);
    let resolved_label = resolve_edge_label(&chain.edge, graph);
    let resolved_terminal = resolve_node_match(&chain.node, graph);
    let mut key_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut groups: Vec<(Value, u64)> = Vec::new();
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    let max_groups = effective_max_groups();

    for start_vertex in start_candidates {
        let total = graph.for_each_neighbor(start_vertex, None, &mut |edge| {
            bump_steps(&mut stats, 1, limits)?;
            if edge.is_tombstoned()
                || graph.is_vertex_tombstoned(edge.target)
                || !resolved_label.matches(edge.label_id())
                || !timestamp_matches_range(spec.edge_ts_range.as_ref(), edge.timestamp)
                || !resolved_terminal.matches_no_tombstone(&chain.node, edge.target, graph)
            {
                return Ok(());
            }
            let key_value = key_cache
                .entry(edge.target)
                .or_insert_with(|| {
                    vertex_property_value_or_null(graph, edge.target, spec.key_property)
                })
                .clone();
            let h = hash_value_slice(std::slice::from_ref(&key_value), build_hasher);
            let bucket = group_index.entry(h).or_default();
            for &idx in bucket.iter() {
                if groups[idx].0 == key_value {
                    groups[idx].1 = groups[idx].1.saturating_add(1);
                    return Ok(());
                }
            }
            if groups.len() >= max_groups {
                return Err(GleaphError::ExecutionError(format!(
                    "MAX_GROUPS exceeded ({max_groups})"
                )));
            }
            let idx = groups.len();
            bucket.push(idx);
            groups.push((key_value, 1));
            Ok(())
        })?;
        stats.scanned_edges = stats.scanned_edges.saturating_add(total);
    }

    stats.breakdown.rows_after_match = groups.len() as u64;
    stats.breakdown.rows_after_with = groups.len() as u64;
    stats.breakdown.rows_before_projection = groups.len() as u64;
    stats.breakdown.groups_formed = stats
        .breakdown
        .groups_formed
        .saturating_add(groups.len() as u64);

    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();
    let mut projected_rows = groups
        .into_iter()
        .map(|(key_value, count)| {
            let mut row = vec![Value::Null; 2];
            row[spec.key_idx] = key_value;
            row[spec.agg_idx] = Value::Int64(count as i64);
            row
        })
        .collect::<Vec<_>>();

    if let Some(order_by) = &q.order_by {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.rows_emitted = projected_rows.len() as u64;

    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

fn execute_reverse_two_hop_top_k_count_by_terminal_key_query<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    let Ok(spec) = classify_reverse_two_hop_top_k_count_by_terminal_key_query(q) else {
        return Ok(None);
    };

    let mut stats = QueryStats::default();
    let start_candidates = initial_candidates(&spec.entry.pattern.start, graph, &mut stats, limits)?;
    let first_label = resolve_edge_label(&spec.first.edge, graph);
    let second_label = resolve_edge_label(&spec.second.edge, graph);
    let resolved_mid = resolve_node_match(&spec.first.node, graph);
    let resolved_terminal = resolve_node_match(&spec.second.node, graph);
    let mut key_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut excluded_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut groups: Vec<(Value, u64)> = Vec::new();
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    let max_groups = effective_max_groups();

    for start_vertex in start_candidates {
        graph.for_each_reverse_neighbor(start_vertex, None, None, &mut |rev| {
            bump_steps(&mut stats, 1, limits)?;
            if !first_label.matches(rev.label_id())
                || !resolved_mid.matches_no_tombstone(&spec.first.node, rev.src, graph)
            {
                return Ok(());
            }

            let second_total = graph.for_each_neighbor(rev.src, None, &mut |edge| {
                bump_steps(&mut stats, 1, limits)?;
                if edge.is_tombstoned()
                    || graph.is_vertex_tombstoned(edge.target)
                    || !second_label.matches(edge.label_id())
                    || !resolved_terminal.matches_no_tombstone(&spec.second.node, edge.target, graph)
                {
                    return Ok(());
                }
                if let (Some(property), Some(excluded)) =
                    (spec.excluded_property, spec.excluded_value.as_ref())
                {
                    let excluded_value = excluded_cache
                        .entry(edge.target)
                        .or_insert_with(|| vertex_property_value_or_null(graph, edge.target, property))
                        .clone();
                    if compare_values(excluded, &excluded_value) == Some(Ordering::Equal) {
                        return Ok(());
                    }
                }
                let key_value = key_cache
                    .entry(edge.target)
                    .or_insert_with(|| {
                        vertex_property_value_or_null(graph, edge.target, spec.key_property)
                    })
                    .clone();
                let h = hash_value_slice(std::slice::from_ref(&key_value), build_hasher);
                let bucket = group_index.entry(h).or_default();
                for &idx in bucket.iter() {
                    if groups[idx].0 == key_value {
                        groups[idx].1 = groups[idx].1.saturating_add(1);
                        return Ok(());
                    }
                }
                if groups.len() >= max_groups {
                    return Err(GleaphError::ExecutionError(format!(
                        "MAX_GROUPS exceeded ({max_groups})"
                    )));
                }
                let idx = groups.len();
                bucket.push(idx);
                groups.push((key_value, 1));
                Ok(())
            })?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(second_total);
            Ok(())
        })?;
    }

    stats.breakdown.rows_after_match = groups.len() as u64;
    stats.breakdown.rows_after_with = groups.len() as u64;
    stats.breakdown.rows_before_projection = groups.len() as u64;
    stats.breakdown.groups_formed = stats
        .breakdown
        .groups_formed
        .saturating_add(groups.len() as u64);

    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();
    let mut projected_rows = groups
        .into_iter()
        .map(|(key_value, count)| {
            let mut row = vec![Value::Null; 2];
            row[spec.key_idx] = key_value;
            row[spec.agg_idx] = Value::Int64(count as i64);
            row
        })
        .collect::<Vec<_>>();

    if let Some(order_by) = &q.order_by {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.rows_emitted = projected_rows.len() as u64;

    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

fn execute_seeded_segmentation_query<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    let Ok(spec) = classify_seeded_segmentation_query(q) else {
        return Ok(None);
    };

    struct SegGroup {
        key_value: Value,
        seed_seen: HashSet<u32>,
        target_seen: HashSet<u32>,
        case_sum: f64,
        avg_total: f64,
        avg_count: u64,
    }

    let mut stats = QueryStats::default();
    let mut start_candidates =
        initial_candidates(&spec.seed_match.pattern.start, graph, &mut stats, limits)?;
    start_candidates.truncate(spec.with_clause.limit.expect("validated").0 as usize);

    let chain = spec.continuation.pattern.chain(0);
    let resolved_label = resolve_edge_label(&chain.edge, graph);
    let resolved_terminal = resolve_node_match(&chain.node, graph);
    let mut group_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut cmp_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut avg_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut groups: Vec<SegGroup> = Vec::new();
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    let max_groups = effective_max_groups();

    for start_vertex in start_candidates {
        let total = graph.for_each_neighbor(start_vertex, None, &mut |edge| {
            bump_steps(&mut stats, 1, limits)?;
            if edge.is_tombstoned()
                || graph.is_vertex_tombstoned(edge.target)
                || !resolved_label.matches(edge.label_id())
                || !resolved_terminal.matches_no_tombstone(&chain.node, edge.target, graph)
            {
                return Ok(());
            }

            let key_value = group_cache
                .entry(edge.target)
                .or_insert_with(|| {
                    vertex_property_value_or_null(graph, edge.target, spec.group_property)
                })
                .clone();
            let h = hash_value_slice(std::slice::from_ref(&key_value), build_hasher);
            let bucket = group_index.entry(h).or_default();
            let group_idx = if let Some(&idx) = bucket.iter().find(|&&idx| groups[idx].key_value == key_value) {
                idx
            } else {
                if groups.len() >= max_groups {
                    return Err(GleaphError::ExecutionError(format!(
                        "MAX_GROUPS exceeded ({max_groups})"
                    )));
                }
                let idx = groups.len();
                bucket.push(idx);
                groups.push(SegGroup {
                    key_value: key_value.clone(),
                    seed_seen: HashSet::new(),
                    target_seen: HashSet::new(),
                    case_sum: 0.0,
                    avg_total: 0.0,
                    avg_count: 0,
                });
                idx
            };

            let group = &mut groups[group_idx];
            group.seed_seen.insert(start_vertex);
            group.target_seen.insert(edge.target);

            let cmp_value = cmp_cache
                .entry(edge.target)
                .or_insert_with(|| {
                    vertex_property_value_or_null(graph, edge.target, spec.case_cmp_property)
                })
                .clone();
            let case_value = if compare_cmp(spec.case_cmp_op, &cmp_value, &spec.case_cmp_literal) {
                &spec.case_then_literal
            } else {
                &spec.case_else_literal
            };
            if let Some(v) = numeric_as_f64(case_value) {
                group.case_sum += v;
            }

            let avg_value = avg_cache
                .entry(edge.target)
                .or_insert_with(|| {
                    vertex_property_value_or_null(graph, edge.target, spec.avg_property)
                })
                .clone();
            if let Some(v) = numeric_as_f64(&avg_value) {
                group.avg_total += v;
                group.avg_count = group.avg_count.saturating_add(1);
            }

            Ok(())
        })?;
        stats.scanned_edges = stats.scanned_edges.saturating_add(total);
    }

    stats.breakdown.rows_after_match = groups.len() as u64;
    stats.breakdown.rows_after_with = groups.len() as u64;
    stats.breakdown.rows_before_projection = groups.len() as u64;
    stats.breakdown.groups_formed = stats
        .breakdown
        .groups_formed
        .saturating_add(groups.len() as u64);

    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();
    let mut projected_rows = groups
        .into_iter()
        .map(|group| {
            let mut row = vec![Value::Null; q.return_clause.items.len()];
            row[spec.group_idx] = group.key_value;
            row[spec.distinct_seed_idx] = Value::Int64(group.seed_seen.len() as i64);
            row[spec.distinct_target_idx] = Value::Int64(group.target_seen.len() as i64);
            row[spec.case_sum_idx] = Value::Float64(group.case_sum);
            row[spec.avg_idx] = if group.avg_count == 0 {
                Value::Null
            } else {
                Value::Float64(group.avg_total / group.avg_count as f64)
            };
            row
        })
        .collect::<Vec<_>>();

    if let Some(order_by) = &q.order_by {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.rows_emitted = projected_rows.len() as u64;

    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

fn execute_seeded_verified_influence_query<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    let Ok(spec) = classify_seeded_verified_influence_query(q) else {
        return Ok(None);
    };

    struct InfluenceGroup {
        key_value: Value,
        categories: BTreeSet<String>,
        hashtags: HashSet<u32>,
    }

    let mut stats = QueryStats::default();
    let mut start_candidates =
        initial_candidates(&spec.seed_match.pattern.start, graph, &mut stats, limits)?;
    start_candidates.truncate(spec.with_clause.limit.expect("validated").0 as usize);

    let first_label = resolve_edge_label(&spec.first.edge, graph);
    let second_label = resolve_edge_label(&spec.second.edge, graph);
    let resolved_mid = resolve_node_match(&spec.first.node, graph);
    let resolved_term = resolve_node_match(&spec.second.node, graph);
    let mut group_key_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut category_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut groups: Vec<InfluenceGroup> = Vec::new();
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    let max_groups = effective_max_groups();

    for start_vertex in start_candidates {
        let first_total = graph.for_each_neighbor(start_vertex, None, &mut |edge1| {
            bump_steps(&mut stats, 1, limits)?;
            if edge1.is_tombstoned()
                || graph.is_vertex_tombstoned(edge1.target)
                || !first_label.matches(edge1.label_id())
                || !resolved_mid.matches_no_tombstone(&spec.first.node, edge1.target, graph)
            {
                return Ok(());
            }

            let second_total = graph.for_each_neighbor(edge1.target, None, &mut |edge2| {
                bump_steps(&mut stats, 1, limits)?;
                if edge2.is_tombstoned()
                    || graph.is_vertex_tombstoned(edge2.target)
                    || !second_label.matches(edge2.label_id())
                    || !resolved_term.matches_no_tombstone(&spec.second.node, edge2.target, graph)
                {
                    return Ok(());
                }

                let key_value = group_key_cache
                    .entry(start_vertex)
                    .or_insert_with(|| {
                        vertex_property_value_or_null(graph, start_vertex, spec.group_property)
                    })
                    .clone();
                let h = hash_value_slice(std::slice::from_ref(&key_value), build_hasher);
                let bucket = group_index.entry(h).or_default();
                let group_idx =
                    if let Some(&idx) = bucket.iter().find(|&&idx| groups[idx].key_value == key_value)
                    {
                        idx
                    } else {
                        if groups.len() >= max_groups {
                            return Err(GleaphError::ExecutionError(format!(
                                "MAX_GROUPS exceeded ({max_groups})"
                            )));
                        }
                        let idx = groups.len();
                        bucket.push(idx);
                        groups.push(InfluenceGroup {
                            key_value: key_value.clone(),
                            categories: BTreeSet::new(),
                            hashtags: HashSet::new(),
                        });
                        idx
                    };

                let group = &mut groups[group_idx];
                group.hashtags.insert(edge2.target);
                if let Value::Text(category) = category_cache
                    .entry(edge2.target)
                    .or_insert_with(|| {
                        vertex_property_value_or_null(graph, edge2.target, spec.collect_property)
                    })
                    .clone()
                {
                    group.categories.insert(category);
                }
                Ok(())
            })?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(second_total);
            Ok(())
        })?;
        stats.scanned_edges = stats.scanned_edges.saturating_add(first_total);
    }

    stats.breakdown.rows_after_match = groups.len() as u64;
    stats.breakdown.rows_after_with = groups.len() as u64;
    stats.breakdown.rows_before_projection = groups.len() as u64;
    stats.breakdown.groups_formed = stats
        .breakdown
        .groups_formed
        .saturating_add(groups.len() as u64);

    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();
    let mut projected_rows = groups
        .into_iter()
        .map(|group| {
            let mut row = vec![Value::Null; q.return_clause.items.len()];
            row[spec.group_idx] = group.key_value;
            row[spec.collect_idx] =
                Value::List(group.categories.into_iter().map(Value::Text).collect());
            row[spec.count_idx] = Value::Int64(group.hashtags.len() as i64);
            row
        })
        .collect::<Vec<_>>();

    if let Some(order_by) = &q.order_by {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.rows_emitted = projected_rows.len() as u64;

    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

/// If a CHAR(n) or BINARY(n) constraint is registered for `property`, pad the value.
fn apply_char_padding(val: Value, property: &str) -> Value {
    match &val {
        Value::Text(s) => {
            let pad_len = CHAR_PAD_DEFS.with(|d| d.borrow().get(property).copied());
            if let Some(n) = pad_len {
                let char_len = s.chars().count() as u32;
                if char_len < n {
                    return Value::Text(format!("{:<width$}", s, width = n as usize));
                }
            }
        }
        Value::Bytes(b) => {
            let pad_len = BINARY_PAD_DEFS.with(|d| d.borrow().get(property).copied());
            if let Some(n) = pad_len {
                if (b.len() as u32) < n {
                    let mut padded = b.clone();
                    padded.resize(n as usize, 0u8);
                    return Value::Bytes(padded);
                }
            }
        }
        _ => {}
    }
    val
}

fn vertex_property_value_or_null<M: Memory>(
    graph: &PmaGraph<M>,
    vertex_id: u32,
    property: &str,
) -> Value {
    let val = graph
        .get_single_vertex_property(vertex_id, property)
        .unwrap_or(Value::Null);
    apply_char_padding(val, property)
}

/// Position-based variable kind for compiled aggregate expressions.
#[derive(Clone, Copy, Debug)]
enum VarKind {
    /// Vertex at position `pos` in the chain (0 = start, 1+ = chain nodes).
    Vertex(usize),
    /// Edge at position `pos` (0 = first chain edge, 1 = second, etc.).
    Edge(usize),
}

/// Per-edge metadata carried through aggregate evaluation.
#[derive(Clone, Copy, Debug)]
struct AggEdgeMeta {
    weight: f32,
    timestamp: u64,
}

/// Maximum number of hops supported by the aggregate fast-path.
const MAX_AGG_HOPS: usize = 10;

/// Aggregate evaluation row: holds all vertices and edges in the match chain.
/// Uses fixed-size arrays to avoid heap allocation — `Copy` for trivial cloning.
#[derive(Clone, Copy, Debug)]
struct AggEvalRow {
    /// Vertex IDs: index 0 = start, index i+1 = chain\[i\].node.
    vertices: [u32; MAX_AGG_HOPS + 1],
    /// Edge metadata: index i = chain\[i\].edge.
    edges: [AggEdgeMeta; MAX_AGG_HOPS],
}

#[derive(Clone, Debug)]
struct AggFnCall {
    name: String,
    args: Vec<CompiledAggExpr>,
}

#[derive(Clone, Debug)]
struct AggCaseExpr {
    operand: Option<Box<CompiledAggExpr>>,
    when_then: Vec<(CompiledAggExpr, CompiledAggExpr)>,
    else_expr: Option<Box<CompiledAggExpr>>,
}

#[derive(Clone, Debug)]
enum CompiledAggExpr {
    Literal(Value),
    /// Vertex ID at the given chain position.
    VertexId(usize),
    /// Vertex property at (chain_position, property_cache_index).
    VertexProperty(usize, usize),
    /// Edge timestamp at the given chain index.
    EdgeTimestamp(usize),
    /// Edge weight at the given chain index.
    EdgeWeight(usize),
    /// Edge user-defined property at (hop_index, property_cache_index).
    EdgeProperty(usize, usize),
    /// Arithmetic binary operation on two compiled sub-expressions.
    BinaryOp {
        op: BinaryOp,
        left: Box<CompiledAggExpr>,
        right: Box<CompiledAggExpr>,
    },
    /// Arithmetic unary operation on a compiled sub-expression.
    UnaryOp {
        op: UnaryOp,
        inner: Box<CompiledAggExpr>,
    },
    /// Pure scalar function call (abs, round, floor, ceil, tointeger, tofloat, coalesce).
    FunctionCall(Box<AggFnCall>),
    /// CASE expression.
    Case(Box<AggCaseExpr>),
    /// COALESCE(expr, expr, ...)
    Coalesce(Box<[CompiledAggExpr]>),
    /// NULLIF(left, right)
    NullIf {
        left: Box<CompiledAggExpr>,
        right: Box<CompiledAggExpr>,
    },
    /// Reference to a finalized accumulator value (used in HAVING).
    AccumResult(usize),
}

#[derive(Clone, Copy, Debug)]
enum AggAccumKind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
    StringAgg,
    PercentileCont,
    PercentileDisc,
}

impl From<AggFunc> for AggAccumKind {
    fn from(f: AggFunc) -> Self {
        match f {
            AggFunc::Count => AggAccumKind::Count,
            AggFunc::Sum => AggAccumKind::Sum,
            AggFunc::Avg => AggAccumKind::Avg,
            AggFunc::Min => AggAccumKind::Min,
            AggFunc::Max => AggAccumKind::Max,
            AggFunc::Collect => AggAccumKind::Collect,
            AggFunc::StringAgg => AggAccumKind::StringAgg,
            AggFunc::PercentileCont => AggAccumKind::PercentileCont,
            AggFunc::PercentileDisc => AggAccumKind::PercentileDisc,
        }
    }
}

#[derive(Clone, Debug)]
struct CompiledAgg {
    kind: AggAccumKind,
    operand: Option<CompiledAggExpr>,
    distinct: bool,
    /// Second argument for STRING_AGG (separator) / PERCENTILE (percentile value).
    param: Option<Value>,
}

#[derive(Clone, Debug)]
enum AggAccum {
    Count(u64),
    Sum {
        total: f64,
        any: bool,
    },
    Avg {
        total: f64,
        count: u64,
    },
    Min(Option<Value>),
    Max(Option<Value>),
    Collect(Vec<Value>),
    StringAgg {
        parts: Vec<String>,
        separator: String,
    },
    PercentileCont {
        values: Vec<f64>,
        p: f64,
    },
    PercentileDisc {
        values: Vec<f64>,
        p: f64,
    },
    /// Wraps an inner accumulator with deduplication.
    Distinct {
        inner: Box<AggAccum>,
        seen: BTreeSet<String>,
    },
}

impl AggAccum {
    fn new(kind: AggAccumKind) -> Self {
        match kind {
            AggAccumKind::Count => AggAccum::Count(0),
            AggAccumKind::Sum => AggAccum::Sum {
                total: 0.0,
                any: false,
            },
            AggAccumKind::Avg => AggAccum::Avg {
                total: 0.0,
                count: 0,
            },
            AggAccumKind::Min => AggAccum::Min(None),
            AggAccumKind::Max => AggAccum::Max(None),
            AggAccumKind::Collect
            | AggAccumKind::StringAgg
            | AggAccumKind::PercentileCont
            | AggAccumKind::PercentileDisc => Self::new_parameterized(kind, None),
        }
    }

    fn new_parameterized(kind: AggAccumKind, param: Option<&Value>) -> Self {
        match kind {
            AggAccumKind::Collect => AggAccum::Collect(Vec::new()),
            AggAccumKind::StringAgg => {
                let separator = param
                    .and_then(|v| {
                        if let Value::Text(t) = v {
                            Some(t.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                AggAccum::StringAgg {
                    parts: Vec::new(),
                    separator,
                }
            }
            AggAccumKind::PercentileCont => {
                let p = param
                    .and_then(numeric_as_f64)
                    .unwrap_or(0.5)
                    .clamp(0.0, 1.0);
                AggAccum::PercentileCont {
                    values: Vec::new(),
                    p,
                }
            }
            AggAccumKind::PercentileDisc => {
                let p = param
                    .and_then(numeric_as_f64)
                    .unwrap_or(0.5)
                    .clamp(0.0, 1.0);
                AggAccum::PercentileDisc {
                    values: Vec::new(),
                    p,
                }
            }
            other => Self::new(other),
        }
    }

    fn new_distinct_parameterized(kind: AggAccumKind, param: Option<&Value>) -> Self {
        AggAccum::Distinct {
            inner: Box::new(Self::new_parameterized(kind, param)),
            seen: BTreeSet::new(),
        }
    }

    fn accumulate(&mut self, val: &Value) {
        match self {
            AggAccum::Count(c) => {
                if !matches!(val, Value::Null) {
                    *c = c.saturating_add(1);
                }
            }
            AggAccum::Sum { total, any } => {
                if let Some(n) = numeric_as_f64(val) {
                    *total += n;
                    *any = true;
                }
            }
            AggAccum::Avg { total, count } => {
                if let Some(n) = numeric_as_f64(val) {
                    *total += n;
                    *count = count.saturating_add(1);
                }
            }
            AggAccum::Min(cur) => match cur {
                Some(c) => {
                    if !matches!(val, Value::Null)
                        && compare_values(val, c).is_some_and(|o| o == Ordering::Less)
                    {
                        *cur = Some(val.clone());
                    }
                }
                None => {
                    if !matches!(val, Value::Null) {
                        *cur = Some(val.clone());
                    }
                }
            },
            AggAccum::Max(cur) => match cur {
                Some(c) => {
                    if !matches!(val, Value::Null)
                        && compare_values(val, c).is_some_and(|o| o == Ordering::Greater)
                    {
                        *cur = Some(val.clone());
                    }
                }
                None => {
                    if !matches!(val, Value::Null) {
                        *cur = Some(val.clone());
                    }
                }
            },
            AggAccum::Collect(list) => {
                if !matches!(val, Value::Null) {
                    list.push(val.clone());
                }
            }
            AggAccum::StringAgg { parts, .. } => {
                if let Value::Text(t) = val {
                    parts.push(t.clone());
                }
            }
            AggAccum::PercentileCont { values, .. } | AggAccum::PercentileDisc { values, .. } => {
                if let Some(n) = numeric_as_f64(val) {
                    values.push(n);
                }
            }
            AggAccum::Distinct { inner, seen } => {
                if matches!(val, Value::Null) {
                    return;
                }
                let key = format!("{val:?}");
                if seen.insert(key) {
                    inner.accumulate(val);
                }
            }
        }
    }

    fn finalize(self) -> Value {
        match self {
            AggAccum::Count(c) => Value::Int64(c as i64),
            AggAccum::Sum { total, any } => {
                if any {
                    Value::Float64(total)
                } else {
                    Value::Null
                }
            }
            AggAccum::Avg { total, count } => {
                if count > 0 {
                    Value::Float64(total / count as f64)
                } else {
                    Value::Null
                }
            }
            AggAccum::Min(v) => v.unwrap_or(Value::Null),
            AggAccum::Max(v) => v.unwrap_or(Value::Null),
            AggAccum::Collect(list) => Value::List(list),
            AggAccum::StringAgg { parts, separator } => {
                if parts.is_empty() {
                    Value::Null
                } else {
                    Value::Text(parts.join(&separator))
                }
            }
            AggAccum::PercentileCont { mut values, p } => {
                if values.is_empty() {
                    return Value::Null;
                }
                values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
                let n = values.len() as f64;
                let rank = p * (n - 1.0);
                let lo = rank.floor() as usize;
                let hi = rank.ceil() as usize;
                if lo == hi {
                    Value::Float64(values[lo])
                } else {
                    let frac = rank - lo as f64;
                    Value::Float64(values[lo] + frac * (values[hi] - values[lo]))
                }
            }
            AggAccum::PercentileDisc { mut values, p } => {
                if values.is_empty() {
                    return Value::Null;
                }
                values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
                let n = values.len() as f64;
                let idx = (p * n).ceil() as usize;
                let idx = if idx == 0 {
                    0
                } else {
                    idx.min(values.len()) - 1
                };
                Value::Float64(values[idx])
            }
            AggAccum::Distinct { inner, .. } => inner.finalize(),
        }
    }

    fn default_empty(kind: AggAccumKind) -> Value {
        match kind {
            AggAccumKind::Count => Value::Int64(0),
            AggAccumKind::Collect => Value::List(Vec::new()),
            _ => Value::Null,
        }
    }
}

#[derive(Clone, Debug)]
enum CompiledAggPredicate {
    And(Box<CompiledAggPredicate>, Box<CompiledAggPredicate>),
    Or(Box<CompiledAggPredicate>, Box<CompiledAggPredicate>),
    Not(Box<CompiledAggPredicate>),
    Compare {
        left: CompiledAggExpr,
        op: CmpOp,
        right: CompiledAggExpr,
    },
    IsNull(CompiledAggExpr),
    IsNotNull(CompiledAggExpr),
    Truthy(CompiledAggExpr),
    StringPredicate {
        expr: CompiledAggExpr,
        kind: StringPredicateKind,
        pattern: CompiledAggExpr,
    },
    InList {
        expr: CompiledAggExpr,
        values: Vec<CompiledAggExpr>,
        negated: bool,
    },
}

#[derive(Default)]
struct VertexPropertyCache {
    names: Vec<String>,
    values: Vec<RapidHashMap<u32, Value>>,
}

impl VertexPropertyCache {
    fn intern_property(&mut self, property: &str) -> usize {
        if let Some(idx) = self.names.iter().position(|name| name == property) {
            return idx;
        }
        let idx = self.names.len();
        self.names.push(property.to_string());
        self.values.push(RapidHashMap::default());
        idx
    }

    fn vertex_property<M: Memory>(
        &mut self,
        graph: &PmaGraph<M>,
        vertex_id: u32,
        property_idx: usize,
    ) -> Value {
        if let Some(cached) = self.values[property_idx].get(&vertex_id) {
            return cached.clone();
        }
        let value = vertex_property_value_or_null(graph, vertex_id, &self.names[property_idx]);
        self.values[property_idx].insert(vertex_id, value.clone());
        value
    }
}

#[derive(Default)]
struct EdgePropertyCache {
    names: Vec<String>,
    values: Vec<RapidHashMap<(u32, u32), Value>>,
}

impl EdgePropertyCache {
    fn intern_property(&mut self, property: &str) -> usize {
        if let Some(idx) = self.names.iter().position(|name| name == property) {
            return idx;
        }
        let idx = self.names.len();
        self.names.push(property.to_string());
        self.values.push(RapidHashMap::default());
        idx
    }

    fn edge_property<M: Memory>(
        &mut self,
        graph: &PmaGraph<M>,
        src: u32,
        dst: u32,
        property_idx: usize,
    ) -> Value {
        let key = (src, dst);
        if let Some(cached) = self.values[property_idx].get(&key) {
            return cached.clone();
        }
        let label = graph.edge_label_ref(src, dst);
        let value = graph
            .edge_record(src, dst, label)
            .and_then(|rec| {
                rec.props.into_iter().find_map(|(k, v)| {
                    if k == self.names[property_idx] {
                        Some(v)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(Value::Null);
        self.values[property_idx].insert(key, value.clone());
        value
    }
}

fn resolve_var_kind(
    var: &str,
    vertex_vars: &[Option<&str>],
    edge_vars: &[Option<&str>],
) -> Option<VarKind> {
    for (pos, v) in vertex_vars.iter().enumerate() {
        if v.is_some_and(|v| v == var) {
            return Some(VarKind::Vertex(pos));
        }
    }
    for (pos, v) in edge_vars.iter().enumerate() {
        if v.is_some_and(|v| v == var) {
            return Some(VarKind::Edge(pos));
        }
    }
    None
}

fn compile_agg_expr(
    expr: &Expr,
    vertex_vars: &[Option<&str>],
    edge_vars: &[Option<&str>],
    property_cache: &mut VertexPropertyCache,
    edge_property_cache: &mut EdgePropertyCache,
) -> Option<CompiledAggExpr> {
    match expr {
        Expr::Literal(v) => Some(CompiledAggExpr::Literal(v.clone())),
        Expr::Parameter { name, .. } => QUERY_PARAMS
            .with(|p| p.borrow().get(name.as_str()).cloned())
            .map(CompiledAggExpr::Literal),
        Expr::Variable(var) | Expr::PathVar(var) => {
            match resolve_var_kind(var, vertex_vars, edge_vars)? {
                VarKind::Vertex(pos) => Some(CompiledAggExpr::VertexId(pos)),
                VarKind::Edge(_) => None,
            }
        }
        Expr::PropertyAccess { target, property } => {
            let (Expr::Variable(var) | Expr::PathVar(var)) = target.as_ref() else {
                return None;
            };
            match resolve_var_kind(var, vertex_vars, edge_vars)? {
                VarKind::Vertex(pos) => {
                    let idx = property_cache.intern_property(property);
                    Some(CompiledAggExpr::VertexProperty(pos, idx))
                }
                VarKind::Edge(pos) => {
                    let idx = edge_property_cache.intern_property(property);
                    Some(CompiledAggExpr::EdgeProperty(pos, idx))
                }
            }
        }
        Expr::BinaryOp { op, left, right } => {
            let l = compile_agg_expr(
                left,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?;
            let r = compile_agg_expr(
                right,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?;
            Some(CompiledAggExpr::BinaryOp {
                op: *op,
                left: Box::new(l),
                right: Box::new(r),
            })
        }
        Expr::UnaryOp { op, expr: inner } => {
            let compiled = compile_agg_expr(
                inner,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?;
            Some(CompiledAggExpr::UnaryOp {
                op: *op,
                inner: Box::new(compiled),
            })
        }
        Expr::FunctionCall { name, args } => {
            let lower = name.to_ascii_lowercase();
            // gleaph_weight(e) / weight(e) → EdgeWeight fast path
            if (lower == "gleaph_weight" || lower == "weight")
                && args.len() == 1
                && let Expr::Variable(var) | Expr::PathVar(var) = &args[0]
                && let Some(VarKind::Edge(pos)) = resolve_var_kind(var, vertex_vars, edge_vars)
            {
                return Some(CompiledAggExpr::EdgeWeight(pos));
            }
            // gleaph_timestamp(e) / timestamp(e) → EdgeTimestamp fast path
            if (lower == "gleaph_timestamp" || lower == "timestamp")
                && args.len() == 1
                && let Expr::Variable(var) | Expr::PathVar(var) = &args[0]
                && let Some(VarKind::Edge(pos)) = resolve_var_kind(var, vertex_vars, edge_vars)
            {
                return Some(CompiledAggExpr::EdgeTimestamp(pos));
            }
            // id(n) → VertexId fast path
            if lower == "id"
                && args.len() == 1
                && let Expr::Variable(var) | Expr::PathVar(var) = &args[0]
                && let Some(VarKind::Vertex(pos)) = resolve_var_kind(var, vertex_vars, edge_vars)
            {
                return Some(CompiledAggExpr::VertexId(pos));
            }
            const SAFE_SCALARS: &[&str] = &[
                "abs",
                "round",
                "floor",
                "ceil",
                "ceiling",
                "tointeger",
                "tofloat",
                "coalesce",
            ];
            if !SAFE_SCALARS.contains(&lower.as_str()) {
                return None;
            }
            let compiled_args: Option<Vec<_>> = args
                .iter()
                .map(|a| {
                    compile_agg_expr(
                        a,
                        vertex_vars,
                        edge_vars,
                        property_cache,
                        edge_property_cache,
                    )
                })
                .collect();
            Some(CompiledAggExpr::FunctionCall(Box::new(AggFnCall {
                name: lower,
                args: compiled_args?,
            })))
        }
        Expr::Case(case_expr) => {
            let operand = match &case_expr.operand {
                Some(e) => Some(Box::new(compile_agg_expr(
                    e,
                    vertex_vars,
                    edge_vars,
                    property_cache,
                    edge_property_cache,
                )?)),
                None => None,
            };
            let when_then: Option<Vec<_>> = case_expr
                .when_then
                .iter()
                .map(|wt| {
                    let w = compile_agg_expr(
                        &wt.when,
                        vertex_vars,
                        edge_vars,
                        property_cache,
                        edge_property_cache,
                    )?;
                    let t = compile_agg_expr(
                        &wt.then,
                        vertex_vars,
                        edge_vars,
                        property_cache,
                        edge_property_cache,
                    )?;
                    Some((w, t))
                })
                .collect();
            let else_expr = match &case_expr.else_expr {
                Some(e) => Some(Box::new(compile_agg_expr(
                    e,
                    vertex_vars,
                    edge_vars,
                    property_cache,
                    edge_property_cache,
                )?)),
                None => None,
            };
            Some(CompiledAggExpr::Case(Box::new(AggCaseExpr {
                operand,
                when_then: when_then?,
                else_expr,
            })))
        }
        Expr::Coalesce(exprs) => {
            let compiled: Option<Vec<_>> = exprs
                .iter()
                .map(|e| {
                    compile_agg_expr(
                        e,
                        vertex_vars,
                        edge_vars,
                        property_cache,
                        edge_property_cache,
                    )
                })
                .collect();
            Some(CompiledAggExpr::Coalesce(compiled?.into_boxed_slice()))
        }
        Expr::NullIf { left, right } => {
            let l = compile_agg_expr(
                left,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?;
            let r = compile_agg_expr(
                right,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?;
            Some(CompiledAggExpr::NullIf {
                left: Box::new(l),
                right: Box::new(r),
            })
        }
        _ => None,
    }
}

fn compile_agg_predicate(
    expr: &Expr,
    vertex_vars: &[Option<&str>],
    edge_vars: &[Option<&str>],
    property_cache: &mut VertexPropertyCache,
    edge_property_cache: &mut EdgePropertyCache,
) -> Option<CompiledAggPredicate> {
    match expr {
        Expr::And(left, right) => Some(CompiledAggPredicate::And(
            Box::new(compile_agg_predicate(
                left,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?),
            Box::new(compile_agg_predicate(
                right,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?),
        )),
        Expr::Or(left, right) => Some(CompiledAggPredicate::Or(
            Box::new(compile_agg_predicate(
                left,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?),
            Box::new(compile_agg_predicate(
                right,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?),
        )),
        Expr::Not(inner) => Some(CompiledAggPredicate::Not(Box::new(compile_agg_predicate(
            inner,
            vertex_vars,
            edge_vars,
            property_cache,
            edge_property_cache,
        )?))),
        Expr::Compare { left, op, right } => Some(CompiledAggPredicate::Compare {
            left: compile_agg_expr(
                left,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?,
            op: *op,
            right: compile_agg_expr(
                right,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?,
        }),
        Expr::IsNull(inner) => Some(CompiledAggPredicate::IsNull(compile_agg_expr(
            inner,
            vertex_vars,
            edge_vars,
            property_cache,
            edge_property_cache,
        )?)),
        Expr::IsNotNull(inner) => Some(CompiledAggPredicate::IsNotNull(compile_agg_expr(
            inner,
            vertex_vars,
            edge_vars,
            property_cache,
            edge_property_cache,
        )?)),
        Expr::StringPredicate {
            expr: e,
            kind,
            pattern,
        } => Some(CompiledAggPredicate::StringPredicate {
            expr: compile_agg_expr(
                e,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?,
            kind: *kind,
            pattern: compile_agg_expr(
                pattern,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?,
        }),
        Expr::InList {
            expr: e,
            list,
            negated,
        } => {
            let compiled_expr = compile_agg_expr(
                e,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?;
            let compiled_values: Option<Vec<_>> = list
                .iter()
                .map(|item| {
                    compile_agg_expr(
                        item,
                        vertex_vars,
                        edge_vars,
                        property_cache,
                        edge_property_cache,
                    )
                })
                .collect();
            Some(CompiledAggPredicate::InList {
                expr: compiled_expr,
                values: compiled_values?,
                negated: *negated,
            })
        }
        _ => Some(CompiledAggPredicate::Truthy(compile_agg_expr(
            expr,
            vertex_vars,
            edge_vars,
            property_cache,
            edge_property_cache,
        )?)),
    }
}

/// Compile an expression for HAVING evaluation. Aggregate sub-expressions are
/// mapped to `AccumResult(idx)` referencing the finalized accumulator at that
/// return-item position. Non-aggregate sub-expressions delegate to `compile_agg_expr`.
fn compile_having_expr(
    expr: &Expr,
    compiled_aggs: &[Option<CompiledAgg>],
    return_items: &[crate::ast::ReturnItem],
    vertex_vars: &[Option<&str>],
    edge_vars: &[Option<&str>],
    property_cache: &mut VertexPropertyCache,
    edge_property_cache: &mut EdgePropertyCache,
) -> Option<CompiledAggExpr> {
    if let Expr::Aggregate(agg) = expr {
        // Find matching accumulator in the RETURN clause.
        for (idx, item) in return_items.iter().enumerate() {
            if compiled_aggs[idx].is_some()
                && let Expr::Aggregate(ret_agg) = &item.expr
                && ret_agg.func == agg.func
                && ret_agg.count_all == agg.count_all
                && ret_agg.distinct == agg.distinct
                && ret_agg.expr == agg.expr
            {
                return Some(CompiledAggExpr::AccumResult(idx));
            }
        }
        // HAVING references an aggregate not in RETURN — bail.
        return None;
    }
    compile_agg_expr(
        expr,
        vertex_vars,
        edge_vars,
        property_cache,
        edge_property_cache,
    )
}

/// Compile a HAVING predicate. Aggregate sub-expressions become `AccumResult`.
fn compile_having_predicate(
    expr: &Expr,
    compiled_aggs: &[Option<CompiledAgg>],
    return_items: &[crate::ast::ReturnItem],
    vertex_vars: &[Option<&str>],
    edge_vars: &[Option<&str>],
    property_cache: &mut VertexPropertyCache,
    edge_property_cache: &mut EdgePropertyCache,
) -> Option<CompiledAggPredicate> {
    match expr {
        Expr::And(left, right) => Some(CompiledAggPredicate::And(
            Box::new(compile_having_predicate(
                left,
                compiled_aggs,
                return_items,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?),
            Box::new(compile_having_predicate(
                right,
                compiled_aggs,
                return_items,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?),
        )),
        Expr::Or(left, right) => Some(CompiledAggPredicate::Or(
            Box::new(compile_having_predicate(
                left,
                compiled_aggs,
                return_items,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?),
            Box::new(compile_having_predicate(
                right,
                compiled_aggs,
                return_items,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?),
        )),
        Expr::Not(inner) => Some(CompiledAggPredicate::Not(Box::new(
            compile_having_predicate(
                inner,
                compiled_aggs,
                return_items,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?,
        ))),
        Expr::Compare { left, op, right } => Some(CompiledAggPredicate::Compare {
            left: compile_having_expr(
                left,
                compiled_aggs,
                return_items,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?,
            op: *op,
            right: compile_having_expr(
                right,
                compiled_aggs,
                return_items,
                vertex_vars,
                edge_vars,
                property_cache,
                edge_property_cache,
            )?,
        }),
        Expr::IsNull(inner) => Some(CompiledAggPredicate::IsNull(compile_having_expr(
            inner,
            compiled_aggs,
            return_items,
            vertex_vars,
            edge_vars,
            property_cache,
            edge_property_cache,
        )?)),
        Expr::IsNotNull(inner) => Some(CompiledAggPredicate::IsNotNull(compile_having_expr(
            inner,
            compiled_aggs,
            return_items,
            vertex_vars,
            edge_vars,
            property_cache,
            edge_property_cache,
        )?)),
        _ => Some(CompiledAggPredicate::Truthy(compile_having_expr(
            expr,
            compiled_aggs,
            return_items,
            vertex_vars,
            edge_vars,
            property_cache,
            edge_property_cache,
        )?)),
    }
}

fn eval_agg_expr<M: Memory>(
    expr: &CompiledAggExpr,
    row: &AggEvalRow,
    graph: &PmaGraph<M>,
    property_cache: &mut VertexPropertyCache,
    edge_property_cache: &mut EdgePropertyCache,
    finalized: &[Value],
) -> Value {
    match expr {
        CompiledAggExpr::Literal(v) => v.clone(),
        CompiledAggExpr::VertexId(pos) => Value::Int64(i64::from(row.vertices[*pos])),
        CompiledAggExpr::VertexProperty(pos, property_idx) => {
            property_cache.vertex_property(graph, row.vertices[*pos], *property_idx)
        }
        CompiledAggExpr::EdgeTimestamp(pos) => Value::Timestamp(row.edges[*pos].timestamp),
        CompiledAggExpr::EdgeWeight(pos) => Value::Float64(row.edges[*pos].weight as f64),
        CompiledAggExpr::EdgeProperty(pos, property_idx) => {
            let src = row.vertices[*pos];
            let dst = row.vertices[*pos + 1];
            edge_property_cache.edge_property(graph, src, dst, *property_idx)
        }
        CompiledAggExpr::BinaryOp { op, left, right } => {
            let l = eval_agg_expr(
                left,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            let r = eval_agg_expr(
                right,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            eval_binary_op(*op, &l, &r)
        }
        CompiledAggExpr::UnaryOp { op, inner } => {
            let v = eval_agg_expr(
                inner,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            eval_unary_op(*op, &v)
        }
        CompiledAggExpr::FunctionCall(fc) => {
            let vals: Vec<Value> = fc
                .args
                .iter()
                .map(|a| {
                    eval_agg_expr(
                        a,
                        row,
                        graph,
                        property_cache,
                        edge_property_cache,
                        finalized,
                    )
                })
                .collect();
            eval_function_call(&fc.name, &vals)
        }
        CompiledAggExpr::Case(case) => {
            if let Some(op) = &case.operand {
                let op_val = eval_agg_expr(
                    op,
                    row,
                    graph,
                    property_cache,
                    edge_property_cache,
                    finalized,
                );
                for (w, t) in &case.when_then {
                    let w_val = eval_agg_expr(
                        w,
                        row,
                        graph,
                        property_cache,
                        edge_property_cache,
                        finalized,
                    );
                    if compare_values(&op_val, &w_val) == Some(Ordering::Equal) {
                        return eval_agg_expr(
                            t,
                            row,
                            graph,
                            property_cache,
                            edge_property_cache,
                            finalized,
                        );
                    }
                }
            } else {
                for (w, t) in &case.when_then {
                    let w_val = eval_agg_expr(
                        w,
                        row,
                        graph,
                        property_cache,
                        edge_property_cache,
                        finalized,
                    );
                    if truthy(&w_val) {
                        return eval_agg_expr(
                            t,
                            row,
                            graph,
                            property_cache,
                            edge_property_cache,
                            finalized,
                        );
                    }
                }
            }
            match &case.else_expr {
                Some(e) => eval_agg_expr(
                    e,
                    row,
                    graph,
                    property_cache,
                    edge_property_cache,
                    finalized,
                ),
                None => Value::Null,
            }
        }
        CompiledAggExpr::Coalesce(exprs) => {
            for e in exprs {
                let v = eval_agg_expr(
                    e,
                    row,
                    graph,
                    property_cache,
                    edge_property_cache,
                    finalized,
                );
                if !matches!(v, Value::Null) {
                    return v;
                }
            }
            Value::Null
        }
        CompiledAggExpr::NullIf { left, right } => {
            let l = eval_agg_expr(
                left,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            let r = eval_agg_expr(
                right,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            if compare_values(&l, &r) == Some(Ordering::Equal) {
                Value::Null
            } else {
                l
            }
        }
        CompiledAggExpr::AccumResult(idx) => finalized[*idx].clone(),
    }
}

fn eval_agg_predicate<M: Memory>(
    pred: &CompiledAggPredicate,
    row: &AggEvalRow,
    graph: &PmaGraph<M>,
    property_cache: &mut VertexPropertyCache,
    edge_property_cache: &mut EdgePropertyCache,
    finalized: &[Value],
) -> bool {
    match pred {
        CompiledAggPredicate::And(left, right) => {
            eval_agg_predicate(
                left,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            ) && eval_agg_predicate(
                right,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            )
        }
        CompiledAggPredicate::Or(left, right) => {
            eval_agg_predicate(
                left,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            ) || eval_agg_predicate(
                right,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            )
        }
        CompiledAggPredicate::Not(inner) => !eval_agg_predicate(
            inner,
            row,
            graph,
            property_cache,
            edge_property_cache,
            finalized,
        ),
        CompiledAggPredicate::Compare { left, op, right } => {
            let l = eval_agg_expr(
                left,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            let r = eval_agg_expr(
                right,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            compare_cmp(*op, &l, &r)
        }
        CompiledAggPredicate::IsNull(inner) => {
            matches!(
                eval_agg_expr(
                    inner,
                    row,
                    graph,
                    property_cache,
                    edge_property_cache,
                    finalized
                ),
                Value::Null
            )
        }
        CompiledAggPredicate::IsNotNull(inner) => !matches!(
            eval_agg_expr(
                inner,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized
            ),
            Value::Null
        ),
        CompiledAggPredicate::Truthy(inner) => truthy(&eval_agg_expr(
            inner,
            row,
            graph,
            property_cache,
            edge_property_cache,
            finalized,
        )),
        CompiledAggPredicate::StringPredicate {
            expr,
            kind,
            pattern,
        } => {
            let val = eval_agg_expr(
                expr,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            let pat = eval_agg_expr(
                pattern,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            match (val, pat) {
                (Value::Text(s), Value::Text(p)) => match kind {
                    StringPredicateKind::StartsWith => s.starts_with(p.as_str()),
                    StringPredicateKind::EndsWith => s.ends_with(p.as_str()),
                    StringPredicateKind::Contains => s.contains(p.as_str()),
                    StringPredicateKind::Like => like_match(&s, &p, false),
                    StringPredicateKind::ILike => like_match(&s, &p, true),
                },
                _ => false,
            }
        }
        CompiledAggPredicate::InList {
            expr,
            values,
            negated,
        } => {
            let needle = eval_agg_expr(
                expr,
                row,
                graph,
                property_cache,
                edge_property_cache,
                finalized,
            );
            let found = values.iter().any(|v| {
                let val = eval_agg_expr(
                    v,
                    row,
                    graph,
                    property_cache,
                    edge_property_cache,
                    finalized,
                );
                if let Value::List(items) = &val {
                    items
                        .iter()
                        .any(|item| compare_values(&needle, item) == Some(Ordering::Equal))
                } else {
                    compare_values(&needle, &val) == Some(Ordering::Equal)
                }
            });
            if *negated { !found } else { found }
        }
    }
}

/// Recursively extend aggregate match one hop at a time.  When all hops are
/// complete, `record_match` is called with the fully-populated `AggEvalRow`.
#[allow(clippy::too_many_arguments)]
#[inline]
fn extend_agg_hop<M: Memory, F: FnMut(&AggEvalRow) -> Result<(), GleaphError>>(
    hop_idx: usize,
    current_vertex: u32,
    row: &mut AggEvalRow,
    m: &MatchClause,
    ts_ranges: &[Option<TimestampRange>],
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    limits: ExecutionLimits,
    record_match: &mut F,
) -> Result<(), GleaphError> {
    if hop_idx >= m.elements.len() {
        return record_match(row);
    }
    let chain = m.chain(hop_idx);
    let (min_depth, max_depth) = match chain.edge.length {
        PathLength::Fixed(n) => (n, n),
        PathLength::Range { min, max } => (min, max),
    };

    if min_depth == 1 && max_depth == 1 {
        // Single-hop fast path (original logic).
        extend_agg_hop_single(
            hop_idx,
            current_vertex,
            row,
            m,
            ts_ranges,
            graph,
            stats,
            limits,
            record_match,
        )
    } else {
        // Variable-length: DFS over depths min..max.
        let default_edge = AggEdgeMeta {
            weight: 0.0,
            timestamp: 0,
        };
        extend_agg_hop_var(
            hop_idx,
            current_vertex,
            0,
            min_depth,
            max_depth,
            default_edge,
            row,
            m,
            ts_ranges,
            graph,
            stats,
            limits,
            record_match,
        )
    }
}

/// Single-hop extension (the original `extend_agg_hop` logic).
#[allow(clippy::too_many_arguments)]
#[inline]
fn extend_agg_hop_single<M: Memory, F: FnMut(&AggEvalRow) -> Result<(), GleaphError>>(
    hop_idx: usize,
    current_vertex: u32,
    row: &mut AggEvalRow,
    m: &MatchClause,
    ts_ranges: &[Option<TimestampRange>],
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    limits: ExecutionLimits,
    record_match: &mut F,
) -> Result<(), GleaphError> {
    stats.breakdown.var_len_dfs_calls = stats.breakdown.var_len_dfs_calls.saturating_add(1);
    let chain = m.chain(hop_idx);
    let ts_range = ts_ranges[hop_idx].as_ref();
    let last_hop = hop_idx + 1 >= m.elements.len();
    let resolved_node = resolve_node_match(&chain.node, graph);

    macro_rules! advance {
        ($next_vertex:expr) => {
            if last_hop {
                stats.breakdown.compiled_match_records = stats
                    .breakdown
                    .compiled_match_records
                    .saturating_add(1);
                record_match(row)?;
            } else {
                extend_agg_hop(
                    hop_idx + 1,
                    $next_vertex,
                    row,
                    m,
                    ts_ranges,
                    graph,
                    stats,
                    limits,
                    record_match,
                )?;
            }
        };
    }

    // Pre-resolve edge label pattern to integer filter (once per hop, not per edge).
    let resolved_label = resolve_edge_label(&chain.edge, graph);
    let label_filter = match &resolved_label {
        ResolvedEdgeLabel::Exact(id) => Some(*id),
        _ => None,
    };

    // Opt A: Only look up label names and check edge tombstones when needed.
    let need_label_name = graph.has_tombstoned_edges() || !chain.edge.properties.is_empty();
    // Opt C: Cache label name for Exact labels (all matching edges share the same label).
    let cached_label_name: Option<&str> = if need_label_name {
        match &resolved_label {
            ResolvedEdgeLabel::Exact(id) => graph.label_name_by_id(*id),
            _ => None,
        }
    } else {
        None
    };

    // Shared closure for processing outgoing edges (used by Outgoing and Either).
    macro_rules! process_outgoing {
        ($edge:expr) => {{
            let edge = $edge;
            bump_steps(stats, 1, limits)?;
            stats.breakdown.outgoing_hop_candidates = stats
                .breakdown
                .outgoing_hop_candidates
                .saturating_add(1);
            if !graph.is_vertex_tombstoned(edge.target) && resolved_label.matches(edge.label_id()) {
                if !need_label_name || {
                    let label_ref =
                        cached_label_name.or_else(|| graph.label_name_by_id(edge.label_id()));
                    !edge.is_tombstoned()
                        && edge_literal_properties_match(
                            graph,
                            &chain.edge,
                            current_vertex,
                            edge.target,
                            label_ref,
                        )
                } {
                    // Opt B: Use no-tombstone variant since we already checked above.
                    if resolved_node.matches_no_tombstone(&chain.node, edge.target, graph) {
                        row.vertices[hop_idx + 1] = edge.target;
                        row.edges[hop_idx] = AggEdgeMeta {
                            weight: edge.weight,
                            timestamp: edge.timestamp,
                        };
                        advance!(edge.target);
                    }
                }
            } else if !resolved_label.matches(edge.label_id()) {
                stats.breakdown.hop_label_rejects = stats
                    .breakdown
                    .hop_label_rejects
                    .saturating_add(1);
                stats.breakdown.outgoing_hop_label_rejects = stats
                    .breakdown
                    .outgoing_hop_label_rejects
                    .saturating_add(1);
            }
            Ok::<(), GleaphError>(())
        }};
    }

    // Shared closure for processing incoming edges (used by Incoming and Either).
    // Note: label and timestamp filtering already done in PMA layer.
    // Note: vertex tombstone + edge tombstone checks done in for_each_reverse_neighbor.
    macro_rules! process_incoming {
        ($rev:expr) => {{
            let rev = $rev;
            bump_steps(stats, 1, limits)?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(1);
            stats.breakdown.incoming_hop_candidates = stats
                .breakdown
                .incoming_hop_candidates
                .saturating_add(1);
            stats.breakdown.reverse_neighbor_callbacks = stats
                .breakdown
                .reverse_neighbor_callbacks
                .saturating_add(1);
            if resolved_label.matches(rev.label_id()) {
                if !chain.edge.properties.is_empty() {
                    let label_ref =
                        cached_label_name.or_else(|| graph.label_name_by_id(rev.label_id()));
                    if !edge_literal_properties_match(
                        graph,
                        &chain.edge,
                        rev.src,
                        current_vertex,
                        label_ref,
                    ) {
                        return Ok::<(), GleaphError>(());
                    }
                }
                // Opt B: for_each_reverse_neighbor already filters tombstoned vertices.
                if resolved_node.matches_no_tombstone(&chain.node, rev.src, graph) {
                    row.vertices[hop_idx + 1] = rev.src;
                    row.edges[hop_idx] = AggEdgeMeta {
                        weight: rev.weight,
                        timestamp: rev.timestamp,
                    };
                    advance!(rev.src);
                }
            } else {
                stats.breakdown.hop_label_rejects = stats
                    .breakdown
                    .hop_label_rejects
                    .saturating_add(1);
                stats.breakdown.incoming_hop_label_rejects = stats
                    .breakdown
                    .incoming_hop_label_rejects
                    .saturating_add(1);
            }
            Ok::<(), GleaphError>(())
        }};
    }

    match chain.edge.direction {
        Direction::Outgoing => {
            let total = graph.for_each_neighbor(current_vertex, ts_range, &mut |edge| {
                process_outgoing!(edge)
            })?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(total);
        }
        Direction::Incoming => {
            graph.for_each_reverse_neighbor(
                current_vertex,
                label_filter,
                ts_range,
                &mut |rev| process_incoming!(rev),
            )?;
        }
        Direction::Either => {
            let total = graph.for_each_neighbor(current_vertex, ts_range, &mut |edge| {
                process_outgoing!(edge)
            })?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(total);
            graph.for_each_reverse_neighbor(
                current_vertex,
                label_filter,
                ts_range,
                &mut |rev| process_incoming!(rev),
            )?;
        }
    }
    Ok(())
}

/// Variable-length hop: recursively expand edges from `current_vertex` at
/// `depth` (0-indexed). At depths `>= min_depth`, if the target node matches,
/// record/advance. At depths `< max_depth`, expand to neighbors.
#[allow(clippy::too_many_arguments)]
fn extend_agg_hop_var<M: Memory, F: FnMut(&AggEvalRow) -> Result<(), GleaphError>>(
    hop_idx: usize,
    current_vertex: u32,
    depth: u32,
    min_depth: u32,
    max_depth: u32,
    last_edge: AggEdgeMeta,
    row: &mut AggEvalRow,
    m: &MatchClause,
    ts_ranges: &[Option<TimestampRange>],
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    limits: ExecutionLimits,
    record_match: &mut F,
) -> Result<(), GleaphError> {
    let chain = m.chain(hop_idx);
    let last_hop = hop_idx + 1 >= m.elements.len();
    let resolved_node = resolve_node_match(&chain.node, graph);

    // At sufficient depth, check target node pattern and record match.
    if depth >= min_depth && resolved_node.matches(&chain.node, current_vertex, graph) {
        row.vertices[hop_idx + 1] = current_vertex;
        row.edges[hop_idx] = last_edge;
        stats.breakdown.compiled_match_records = stats
            .breakdown
            .compiled_match_records
            .saturating_add(1);
        if last_hop {
            record_match(row)?;
        } else {
            extend_agg_hop(
                hop_idx + 1,
                current_vertex,
                row,
                m,
                ts_ranges,
                graph,
                stats,
                limits,
                record_match,
            )?;
        }
    }

    // Expand further if below max depth.
    if depth >= max_depth {
        return Ok(());
    }

    let ts_range = ts_ranges[hop_idx].as_ref();

    /// Helper: expand one neighbor edge for variable-length DFS.
    macro_rules! expand_neighbor {
        ($target:expr, $edge_meta:expr) => {
            extend_agg_hop_var(
                hop_idx,
                $target,
                depth + 1,
                min_depth,
                max_depth,
                $edge_meta,
                row,
                m,
                ts_ranges,
                graph,
                stats,
                limits,
                record_match,
            )?;
        };
    }

    // Pre-resolve edge label pattern to integer filter.
    let resolved_label = resolve_edge_label(&chain.edge, graph);
    let label_filter = match &resolved_label {
        ResolvedEdgeLabel::Exact(id) => Some(*id),
        _ => None,
    };

    // Opt A: Only look up label names and check edge tombstones when needed.
    let has_tombstoned_edges = graph.has_tombstoned_edges();

    macro_rules! process_outgoing_var {
        ($edge:expr) => {{
            let edge = $edge;
            bump_steps(stats, 1, limits)?;
            stats.breakdown.outgoing_hop_candidates = stats
                .breakdown
                .outgoing_hop_candidates
                .saturating_add(1);
            if !graph.is_vertex_tombstoned(edge.target) && resolved_label.matches(edge.label_id()) {
                if !has_tombstoned_edges || !edge.is_tombstoned() {
                    expand_neighbor!(
                        edge.target,
                        AggEdgeMeta {
                            weight: edge.weight,
                            timestamp: edge.timestamp
                        }
                    );
                }
            } else if !resolved_label.matches(edge.label_id()) {
                stats.breakdown.hop_label_rejects = stats
                    .breakdown
                    .hop_label_rejects
                    .saturating_add(1);
                stats.breakdown.outgoing_hop_label_rejects = stats
                    .breakdown
                    .outgoing_hop_label_rejects
                    .saturating_add(1);
            }
            Ok::<(), GleaphError>(())
        }};
    }

    // Note: label and timestamp filtering already done in PMA layer.
    // Note: vertex tombstone + edge tombstone checks done in for_each_reverse_neighbor.
    macro_rules! process_incoming_var {
        ($rev:expr) => {{
            let rev = $rev;
            bump_steps(stats, 1, limits)?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(1);
            stats.breakdown.incoming_hop_candidates = stats
                .breakdown
                .incoming_hop_candidates
                .saturating_add(1);
            stats.breakdown.reverse_neighbor_callbacks = stats
                .breakdown
                .reverse_neighbor_callbacks
                .saturating_add(1);
            if resolved_label.matches(rev.label_id()) {
                expand_neighbor!(
                    rev.src,
                    AggEdgeMeta {
                        weight: rev.weight,
                        timestamp: rev.timestamp
                    }
                );
            } else {
                stats.breakdown.hop_label_rejects = stats
                    .breakdown
                    .hop_label_rejects
                    .saturating_add(1);
                stats.breakdown.incoming_hop_label_rejects = stats
                    .breakdown
                    .incoming_hop_label_rejects
                    .saturating_add(1);
            }
            Ok::<(), GleaphError>(())
        }};
    }

    match chain.edge.direction {
        Direction::Outgoing => {
            let total = graph.for_each_neighbor(current_vertex, ts_range, &mut |edge| {
                process_outgoing_var!(edge)
            })?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(total);
        }
        Direction::Incoming => {
            graph.for_each_reverse_neighbor(
                current_vertex,
                label_filter,
                ts_range,
                &mut |rev| process_incoming_var!(rev),
            )?;
        }
        Direction::Either => {
            let total = graph.for_each_neighbor(current_vertex, ts_range, &mut |edge| {
                process_outgoing_var!(edge)
            })?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(total);
            graph.for_each_reverse_neighbor(
                current_vertex,
                label_filter,
                ts_range,
                &mut |rev| process_incoming_var!(rev),
            )?;
        }
    }
    Ok(())
}

fn execute_aggregate_query_compiled<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    if !q
        .return_clause
        .items
        .iter()
        .any(|item| expr_contains_aggregate(&item.expr))
    {
        return Ok(None);
    }
    let explicit_group_by = q.group_by.as_deref();
    let non_agg_indices = q
        .return_clause
        .items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| (!expr_contains_aggregate(&item.expr)).then_some(idx))
        .collect::<Vec<_>>();
    let has_group_keys = explicit_group_by.is_some() || !non_agg_indices.is_empty();

    // Build position-based variable maps from all chains.
    let mut vertex_vars: Vec<Option<&str>> = vec![m.start.var.as_deref()];
    let mut edge_vars: Vec<Option<&str>> = Vec::with_capacity(m.elements.len());
    for chain in m.hops() {
        edge_vars.push(chain.edge.var.as_deref());
        vertex_vars.push(chain.node.var.as_deref());
    }
    let mut property_cache = VertexPropertyCache::default();
    let mut edge_property_cache = EdgePropertyCache::default();

    let group_exprs: Vec<&Expr> = if let Some(group_by) = explicit_group_by {
        group_by.iter().collect()
    } else {
        non_agg_indices
            .iter()
            .map(|idx| &q.return_clause.items[*idx].expr)
            .collect()
    };
    let mut compiled_group_exprs = Vec::with_capacity(group_exprs.len());
    for expr in group_exprs {
        let Some(compiled) = compile_agg_expr(
            expr,
            &vertex_vars,
            &edge_vars,
            &mut property_cache,
            &mut edge_property_cache,
        ) else {
            return Ok(None);
        };
        compiled_group_exprs.push(compiled);
    }

    let mut compiled_return_exprs = Vec::with_capacity(q.return_clause.items.len());
    let mut compiled_aggs: Vec<Option<CompiledAgg>> =
        Vec::with_capacity(q.return_clause.items.len());
    for item in &q.return_clause.items {
        if let Expr::Aggregate(agg) = &item.expr {
            let operand = if agg.count_all {
                None
            } else {
                let Some(compiled) = compile_agg_expr(
                    agg.expr.as_ref().unwrap(),
                    &vertex_vars,
                    &edge_vars,
                    &mut property_cache,
                    &mut edge_property_cache,
                ) else {
                    return Ok(None);
                };
                Some(compiled)
            };
            let param = match agg.func {
                AggFunc::StringAgg | AggFunc::PercentileCont | AggFunc::PercentileDisc => {
                    match agg.separator.as_ref().and_then(|s| {
                        compile_agg_expr(
                            s,
                            &vertex_vars,
                            &edge_vars,
                            &mut property_cache,
                            &mut edge_property_cache,
                        )
                    }) {
                        Some(CompiledAggExpr::Literal(v)) => Some(v),
                        Some(_) => return Ok(None), // non-literal param → bail
                        None => None,
                    }
                }
                _ => None,
            };
            compiled_aggs.push(Some(CompiledAgg {
                kind: agg.func.into(),
                operand,
                distinct: agg.distinct,
                param,
            }));
            compiled_return_exprs.push(None);
        } else {
            compiled_aggs.push(None);
            let Some(compiled) = compile_agg_expr(
                &item.expr,
                &vertex_vars,
                &edge_vars,
                &mut property_cache,
                &mut edge_property_cache,
            ) else {
                return Ok(None);
            };
            compiled_return_exprs.push(Some(compiled));
        }
    }

    let mut stats = QueryStats::default();
    let start_candidates = initial_candidates(&m.start, graph, &mut stats, limits)?;
    // Pre-compute timestamp ranges per edge from the WHERE clause.
    let ts_ranges: Vec<Option<TimestampRange>> = edge_vars
        .iter()
        .map(|ev| extract_edge_ts_range(q.where_clause.as_ref(), *ev))
        .collect();
    // Strip edge-timestamp predicates already covered by ts_ranges, then compile
    // the remaining WHERE clause (if any) into a fast-eval predicate.
    let compiled_where = if let Some(where_clause) = q.where_clause.as_ref() {
        let active_edge_vars: Vec<&str> = edge_vars
            .iter()
            .zip(&ts_ranges)
            .filter_map(|(ev, ts)| if ts.is_some() { ev.as_deref() } else { None })
            .collect();
        let stripped = strip_edge_ts_predicates(where_clause, &active_edge_vars);
        if let Some(stripped_expr) = stripped {
            let Some(compiled) = compile_agg_predicate(
                &stripped_expr,
                &vertex_vars,
                &edge_vars,
                &mut property_cache,
                &mut edge_property_cache,
            ) else {
                return Ok(None);
            };
            Some(compiled)
        } else {
            None
        }
    } else {
        None
    };

    // Compile HAVING clause (if present) into a predicate over finalized accumulators.
    let compiled_having = if let Some(having_expr) = q.having.as_ref() {
        let Some(compiled) = compile_having_predicate(
            having_expr,
            &compiled_aggs,
            &q.return_clause.items,
            &vertex_vars,
            &edge_vars,
            &mut property_cache,
            &mut edge_property_cache,
        ) else {
            return Ok(None);
        };
        Some(compiled)
    } else {
        None
    };

    let mut groups: Vec<(Vec<Value>, AggEvalRow, Vec<AggAccum>)> = Vec::new();
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    let mut matched_rows: u64 = 0;
    let mut compiled_group_key_evals: u64 = 0;
    let mut compiled_group_bucket_probes: u64 = 0;
    let mut compiled_agg_updates: u64 = 0;

    let mut record_match = |row: &AggEvalRow| -> Result<(), GleaphError> {
        if let Some(where_pred) = &compiled_where
            && !eval_agg_predicate(
                where_pred,
                row,
                graph,
                &mut property_cache,
                &mut edge_property_cache,
                &[],
            )
        {
            return Ok(());
        }
        matched_rows = matched_rows.saturating_add(1);
        if let Some(cap) = limits.max_rows
            && matched_rows as usize > cap
        {
            return Err(GleaphError::ExecutionError(format!(
                "result row count {} exceeds default cap {}",
                matched_rows, cap
            )));
        }
        let key_values = compiled_group_exprs
            .iter()
            .map(|expr| {
                compiled_group_key_evals = compiled_group_key_evals.saturating_add(1);
                eval_agg_expr(
                    expr,
                    row,
                    graph,
                    &mut property_cache,
                    &mut edge_property_cache,
                    &[],
                )
            })
            .collect::<Vec<_>>();
        let h = hash_value_slice(&key_values, build_hasher);
        let bucket = group_index.entry(h).or_default();
        for &idx in bucket.iter() {
            compiled_group_bucket_probes = compiled_group_bucket_probes.saturating_add(1);
            if groups[idx].0 == key_values {
                for (accum, ca) in groups[idx].2.iter_mut().zip(&compiled_aggs) {
                    if let Some(ca) = ca {
                        let val = match &ca.operand {
                            Some(expr) => eval_agg_expr(
                                expr,
                                row,
                                graph,
                                &mut property_cache,
                                &mut edge_property_cache,
                                &[],
                            ),
                            None => Value::Bool(true), // COUNT(*): non-NULL sentinel
                        };
                        accum.accumulate(&val);
                        compiled_agg_updates = compiled_agg_updates.saturating_add(1);
                    }
                }
                return Ok(());
            }
        }
        let max_groups = effective_max_groups();
        if groups.len() >= max_groups {
            return Err(GleaphError::ExecutionError(format!(
                "MAX_GROUPS exceeded ({max_groups})"
            )));
        }
        let idx = groups.len();
        bucket.push(idx);
        let mut accums: Vec<AggAccum> = compiled_aggs
            .iter()
            .map(|ca| match ca {
                Some(ca) if ca.distinct => {
                    AggAccum::new_distinct_parameterized(ca.kind, ca.param.as_ref())
                }
                Some(ca) => AggAccum::new_parameterized(ca.kind, ca.param.as_ref()),
                None => AggAccum::Count(0), // placeholder for non-agg items
            })
            .collect();
        // Accumulate the first row
        for (accum, ca) in accums.iter_mut().zip(&compiled_aggs) {
            if let Some(ca) = ca {
                let val = match &ca.operand {
                    Some(expr) => eval_agg_expr(
                        expr,
                        row,
                        graph,
                        &mut property_cache,
                        &mut edge_property_cache,
                        &[],
                    ),
                    None => Value::Bool(true), // COUNT(*): non-NULL sentinel
                };
                accum.accumulate(&val);
                compiled_agg_updates = compiled_agg_updates.saturating_add(1);
            }
        }
        groups.push((key_values, *row, accums));
        Ok(())
    };

    let default_edge = AggEdgeMeta {
        weight: 0.0,
        timestamp: 0,
    };
    let mut row = AggEvalRow {
        vertices: [0u32; MAX_AGG_HOPS + 1],
        edges: [default_edge; MAX_AGG_HOPS],
    };
    for start_vertex in start_candidates {
        row.vertices[0] = start_vertex;
        extend_agg_hop(
            0,
            start_vertex,
            &mut row,
            m,
            &ts_ranges,
            graph,
            &mut stats,
            limits,
            &mut record_match,
        )?;
    }

    stats.breakdown.aggregate_compiled_fast_path_used = true;
    stats.breakdown.rows_after_match = matched_rows;
    stats.breakdown.rows_after_with = matched_rows;
    stats.breakdown.rows_before_projection = matched_rows;
    stats.breakdown.compiled_group_key_evals = stats
        .breakdown
        .compiled_group_key_evals
        .saturating_add(compiled_group_key_evals);
    stats.breakdown.compiled_group_bucket_probes = stats
        .breakdown
        .compiled_group_bucket_probes
        .saturating_add(compiled_group_bucket_probes);
    stats.breakdown.compiled_agg_updates = stats
        .breakdown
        .compiled_agg_updates
        .saturating_add(compiled_agg_updates);
    stats.breakdown.groups_formed = stats
        .breakdown
        .groups_formed
        .saturating_add(groups.len() as u64);

    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();
    let mut projected_rows = if matched_rows == 0 && !has_group_keys {
        let default_row: Vec<Value> = compiled_aggs
            .iter()
            .map(|ca| match ca {
                Some(ca) => AggAccum::default_empty(ca.kind),
                None => Value::Null,
            })
            .collect();
        // Apply HAVING even to the empty-group default row.
        if let Some(having_pred) = &compiled_having {
            let default_edge = AggEdgeMeta {
                weight: 0.0,
                timestamp: 0,
            };
            let dummy_row = AggEvalRow {
                vertices: [0u32; MAX_AGG_HOPS + 1],
                edges: [default_edge; MAX_AGG_HOPS],
            };
            if eval_agg_predicate(
                having_pred,
                &dummy_row,
                graph,
                &mut property_cache,
                &mut edge_property_cache,
                &default_row,
            ) {
                vec![default_row]
            } else {
                Vec::new()
            }
        } else {
            vec![default_row]
        }
    } else {
        groups
            .into_iter()
            .filter_map(|(_key_values, ref rep, accums)| {
                // Finalize all accumulators for this group.
                let finalized: Vec<Value> = accums
                    .iter()
                    .enumerate()
                    .map(|(idx, accum)| {
                        if compiled_aggs[idx].is_some() {
                            accum.clone().finalize()
                        } else {
                            Value::Null // placeholder; non-agg items projected below
                        }
                    })
                    .collect();
                // Apply HAVING filter.
                if let Some(having_pred) = &compiled_having
                    && !eval_agg_predicate(
                        having_pred,
                        rep,
                        graph,
                        &mut property_cache,
                        &mut edge_property_cache,
                        &finalized,
                    )
                {
                    return None;
                }
                // Project return items.
                Some(
                    q.return_clause
                        .items
                        .iter()
                        .enumerate()
                        .map(|(idx, _item)| {
                            if compiled_aggs[idx].is_some() {
                                finalized[idx].clone()
                            } else {
                                eval_agg_expr(
                                    compiled_return_exprs[idx]
                                        .as_ref()
                                        .expect("non-aggregate return expression is compiled"),
                                    rep,
                                    graph,
                                    &mut property_cache,
                                    &mut edge_property_cache,
                                    &[],
                                )
                            }
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };

    if q.return_clause.distinct {
        let mut seen = HashSet::new();
        if q.order_by.is_none()
            && let Some(limit) = q.limit
        {
            // Early termination: stop once k distinct rows collected.
            let k = limit.0 as usize;
            let mut deduped = Vec::with_capacity(k);
            for row in projected_rows.into_iter() {
                if seen.insert(format!("{row:?}")) {
                    deduped.push(row);
                    if deduped.len() >= k {
                        break;
                    }
                }
            }
            projected_rows = deduped;
        } else {
            projected_rows.retain(|row| {
                let key = format!("{row:?}");
                seen.insert(key)
            });
        }
    }

    if let Some(order_by) = &q.order_by {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    if let Some(offset) = q.offset {
        let off = offset as usize;
        if off >= projected_rows.len() {
            projected_rows.clear();
        } else {
            projected_rows.drain(0..off);
        }
    }
    stats.rows_emitted = projected_rows.len() as u64;

    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

/// Fast path for 1-hop endpoint-grouped top-k count queries.
///
/// Supported shape:
/// `MATCH (a)-[label?]->(b) RETURN endpoint.prop, COUNT(*) ORDER BY COUNT(*) DESC LIMIT k`
///
/// The grouping key may come from either the start or terminal endpoint, but it must be a
/// single property access and the aggregate must be an exact `COUNT(*)`.
fn execute_top_k_count_by_endpoint_key_query<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    if q.return_clause.star
        || q.return_clause.distinct
        || !q.with_clauses.is_empty()
        || q.where_clause.is_some()
        || q.having.is_some()
        || q.offset.is_some()
        || q.group_by.is_some()
        || q.match_clauses.len() != 1
    {
        return Ok(None);
    }
    let entry = &q.match_clauses[0];
    if entry.optional || entry.shortest || entry.pattern.elements.len() != 1 {
        return Ok(None);
    }
    let chain = entry.pattern.chain(0);
    if chain.edge.direction != Direction::Outgoing
        || chain.edge.length != PathLength::Fixed(1)
        || !chain.edge.properties.is_empty()
    {
        return Ok(None);
    }
    let start_var = entry.pattern.start.var.as_deref();
    let Some(target_var) = chain.node.var.as_deref() else {
        return Ok(None);
    };
    if q.return_clause.items.len() != 2 {
        return Ok(None);
    }
    let mut key_idx = None;
    let mut agg_idx = None;
    let mut key_property = None;
    let mut key_from_start = false;
    for (idx, item) in q.return_clause.items.iter().enumerate() {
        match &item.expr {
            Expr::Aggregate(agg)
                if agg.func == AggFunc::Count
                    && agg.count_all
                    && !agg.distinct
                    && agg.expr.is_none() =>
            {
                agg_idx = Some(idx);
            }
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if Some(var.as_str()) == start_var) =>
            {
                key_idx = Some(idx);
                key_property = Some(property.as_str());
                key_from_start = true;
            }
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == target_var) =>
            {
                key_idx = Some(idx);
                key_property = Some(property.as_str());
            }
            _ => return Ok(None),
        }
    }
    let (Some(key_idx), Some(agg_idx), Some(key_property)) = (key_idx, agg_idx, key_property) else {
        return Ok(None);
    };

    let mut stats = QueryStats::default();
    let start_candidates = initial_candidates(&entry.pattern.start, graph, &mut stats, limits)?;
    let resolved_label = resolve_edge_label(&chain.edge, graph);
    let mut key_cache: RapidHashMap<u32, Value> = RapidHashMap::default();
    let mut groups: Vec<(Value, u64)> = Vec::new();
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    let max_groups = effective_max_groups();

    for start_vertex in start_candidates {
        let start_key_value = key_from_start.then(|| {
            key_cache
                .entry(start_vertex)
                .or_insert_with(|| vertex_property_value_or_null(graph, start_vertex, key_property))
                .clone()
        });
        let total = graph.for_each_neighbor(start_vertex, None, &mut |edge| {
            bump_steps(&mut stats, 1, limits)?;
            if edge.is_tombstoned()
                || graph.is_vertex_tombstoned(edge.target)
                || !resolved_label.matches(edge.label_id())
                || !node_matches_no_tombstone(&chain.node, edge.target, graph)
            {
                return Ok(());
            }
            let key_value = if let Some(value) = &start_key_value {
                value.clone()
            } else {
                key_cache
                    .entry(edge.target)
                    .or_insert_with(|| {
                        vertex_property_value_or_null(graph, edge.target, key_property)
                    })
                    .clone()
            };
            let h = hash_value_slice(std::slice::from_ref(&key_value), build_hasher);
            let bucket = group_index.entry(h).or_default();
            for &idx in bucket.iter() {
                if groups[idx].0 == key_value {
                    groups[idx].1 = groups[idx].1.saturating_add(1);
                    return Ok(());
                }
            }
            if groups.len() >= max_groups {
                return Err(GleaphError::ExecutionError(format!(
                    "MAX_GROUPS exceeded ({max_groups})"
                )));
            }
            let idx = groups.len();
            bucket.push(idx);
            groups.push((key_value, 1));
            Ok(())
        })?;
        stats.scanned_edges = stats.scanned_edges.saturating_add(total);
    }

    stats.breakdown.rows_after_match = groups.len() as u64;
    stats.breakdown.rows_after_with = groups.len() as u64;
    stats.breakdown.rows_before_projection = groups.len() as u64;
    stats.breakdown.groups_formed = stats
        .breakdown
        .groups_formed
        .saturating_add(groups.len() as u64);

    let columns = q
        .return_clause
        .items
        .iter()
        .map(column_name)
        .collect::<Vec<_>>();
    let mut projected_rows = groups
        .into_iter()
        .map(|(key_value, count)| {
            let mut row = vec![Value::Null; 2];
            row[key_idx] = key_value;
            row[agg_idx] = Value::Int64(count as i64);
            row
        })
        .collect::<Vec<_>>();

    if let Some(order_by) = &q.order_by {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.rows_emitted = projected_rows.len() as u64;

    Ok(Some(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    }))
}

/// Fast path for anchored 2-hop projections ordered by the second edge timestamp.
///
/// Supported shape:
/// `MATCH (a {...})-[label1?]->(b)-[e:label2?]->(c) WHERE gleaph_timestamp(e) ...`
/// `RETURN c.prop..., gleaph_timestamp(e) AS ts ORDER BY ts DESC LIMIT k`
///
/// This avoids building binding rows and generic ORDER BY evaluation for the common
/// "recent feed" pattern where only terminal properties plus the second-hop timestamp
/// are projected.
fn execute_recent_two_hop_top_k_projection_query<M: Memory>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<Option<QueryResult>, GleaphError> {
    if q.return_clause.star
        || q.return_clause.distinct
        || !q.with_clauses.is_empty()
        || q.having.is_some()
        || q.group_by.is_some()
        || q.offset.is_some()
        || q.match_clauses.len() != 1
    {
        return Ok(None);
    }
    let limit = match q.limit {
        Some(limit) if limit.0 > 0 => limit.0 as usize,
        Some(_) => {
            return Ok(Some(QueryResult {
                columns: q.return_clause.items.iter().map(column_name).collect(),
                rows: Vec::new(),
                stats: QueryStats::default(),
                warnings: vec![],
            }));
        }
        None => return Ok(None),
    };
    let entry = &q.match_clauses[0];
    if entry.optional || entry.shortest || !entry.pattern.is_flat() || entry.pattern.elements.len() != 2
    {
        return Ok(None);
    }
    let first = entry.pattern.chain(0);
    let second = entry.pattern.chain(1);
    if first.edge.direction != Direction::Outgoing
        || second.edge.direction != Direction::Outgoing
        || first.edge.length != PathLength::Fixed(1)
        || second.edge.length != PathLength::Fixed(1)
        || !first.edge.properties.is_empty()
        || !second.edge.properties.is_empty()
    {
        return Ok(None);
    }
    let Some(terminal_var) = second.node.var.as_deref() else {
        return Ok(None);
    };
    let Some(edge_var) = second.edge.var.as_deref() else {
        return Ok(None);
    };
    let Some(ts_range) = extract_edge_ts_range(q.where_clause.as_ref(), Some(edge_var)) else {
        return Ok(None);
    };
    if q.where_clause.as_ref().and_then(|expr| strip_edge_ts_predicates(expr, &[edge_var])).is_some()
    {
        return Ok(None);
    }

    let mut ts_return_idx = None;
    let mut ts_alias = None;
    let mut terminal_properties = Vec::new();
    for (idx, item) in q.return_clause.items.iter().enumerate() {
        match &item.expr {
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == terminal_var) =>
            {
                terminal_properties.push((idx, property.clone()));
            }
            Expr::FunctionCall { name, args }
                if args.len() == 1
                    && matches!(args[0], Expr::Variable(ref var) if var == edge_var)
                    && matches!(name.to_ascii_lowercase().as_str(), "gleaph_timestamp" | "timestamp") =>
            {
                if ts_return_idx.replace(idx).is_some() {
                    return Ok(None);
                }
                ts_alias = item.alias.as_deref();
            }
            _ => return Ok(None),
        }
    }
    let Some(ts_return_idx) = ts_return_idx else {
        return Ok(None);
    };
    let Some(order_by) = &q.order_by else {
        return Ok(None);
    };
    if order_by.items.len() != 1 {
        return Ok(None);
    }
    let order_item = &order_by.items[0];
    if !order_item.descending || order_item.nulls_first.is_some() {
        return Ok(None);
    }
    let order_matches_ts = match &order_item.expr {
        Expr::Variable(var) => ts_alias.is_some_and(|alias| var.eq_ignore_ascii_case(alias)),
        Expr::FunctionCall { name, args }
            if args.len() == 1
                && matches!(args[0], Expr::Variable(ref var) if var == edge_var)
                && matches!(name.to_ascii_lowercase().as_str(), "gleaph_timestamp" | "timestamp") =>
        {
            true
        }
        _ => false,
    };
    if !order_matches_ts {
        return Ok(None);
    }

    let mut stats = QueryStats::default();
    let start_candidates = initial_candidates(&entry.pattern.start, graph, &mut stats, limits)?;
    let first_label = resolve_edge_label(&first.edge, graph);
    let second_label = resolve_edge_label(&second.edge, graph);
    let mut property_cache: RapidHashMap<(u32, String), Value> = RapidHashMap::default();
    let mut best: Vec<(usize, u64, Vec<Value>)> = Vec::new();
    let mut ordinal = 0usize;

    for start_vertex in start_candidates {
        let first_total = graph.for_each_neighbor(start_vertex, None, &mut |edge1| {
            bump_steps(&mut stats, 1, limits)?;
            if edge1.is_tombstoned()
                || graph.is_vertex_tombstoned(edge1.target)
                || !first_label.matches(edge1.label_id())
                || !node_matches_no_tombstone(&first.node, edge1.target, graph)
            {
                return Ok(());
            }

            let second_total = graph.for_each_neighbor(edge1.target, None, &mut |edge2| {
                bump_steps(&mut stats, 1, limits)?;
                if edge2.is_tombstoned()
                    || graph.is_vertex_tombstoned(edge2.target)
                    || !second_label.matches(edge2.label_id())
                    || !node_matches_no_tombstone(&second.node, edge2.target, graph)
                    || !timestamp_matches_range(Some(&ts_range), edge2.timestamp)
                {
                    return Ok(());
                }

                let mut row = vec![Value::Null; q.return_clause.items.len()];
                for (idx, property) in terminal_properties.iter() {
                    let cache_key = (edge2.target, property.clone());
                    let value = property_cache
                        .entry(cache_key)
                        .or_insert_with(|| {
                            vertex_property_value_or_null(graph, edge2.target, property.as_str())
                        })
                        .clone();
                    row[*idx] = value;
                }
                row[ts_return_idx] = Value::Timestamp(edge2.timestamp);

                let insert_pos = best
                    .iter()
                    .position(|existing| {
                        existing.1 < edge2.timestamp
                            || (existing.1 == edge2.timestamp && existing.0 > ordinal)
                    })
                    .unwrap_or(best.len());
                if best.len() < limit {
                    best.insert(insert_pos, (ordinal, edge2.timestamp, row));
                } else if insert_pos < limit {
                    best.insert(insert_pos, (ordinal, edge2.timestamp, row));
                    best.pop();
                }
                ordinal = ordinal.saturating_add(1);
                Ok(())
            })?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(second_total);
            Ok(())
        })?;
        stats.scanned_edges = stats.scanned_edges.saturating_add(first_total);
    }

    let rows = best.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>();
    stats.breakdown.rows_after_match = rows.len() as u64;
    stats.breakdown.rows_after_with = rows.len() as u64;
    stats.breakdown.rows_before_projection = rows.len() as u64;
    stats.breakdown.limit_truncate_calls = stats.breakdown.limit_truncate_calls.saturating_add(1);
    stats.breakdown.recent_two_hop_projection_fast_path_used = true;
    stats.rows_emitted = rows.len() as u64;
    Ok(Some(QueryResult {
        columns: q.return_clause.items.iter().map(column_name).collect(),
        rows,
        stats,
        warnings: vec![],
    }))
}

/// Fast path for a single outgoing variable-length hop that only projects terminal node properties.
///
/// Supported shape:
/// `MATCH (a {...})-[:L1|L2*min..max]->(b:Label) RETURN b.prop... LIMIT k`
///
/// This preserves the current DFS traversal order while avoiding per-hop binding/path
/// materialization when the query does not reference edge bindings, ORDER BY, or WHERE.
fn execute_var_len_terminal_projection_query<M: Memory>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<Option<QueryResult>, GleaphError> {
    if q.return_clause.star
        || q.return_clause.distinct
        || !q.with_clauses.is_empty()
        || q.having.is_some()
        || q.group_by.is_some()
        || q.order_by.is_some()
        || q.offset.is_some()
        || q.where_clause.is_some()
        || q.match_clauses.len() != 1
    {
        return Ok(None);
    }
    let limit = match q.limit {
        Some(limit) if limit.0 > 0 => limit.0 as usize,
        Some(_) => {
            return Ok(Some(QueryResult {
                columns: q.return_clause.items.iter().map(column_name).collect(),
                rows: Vec::new(),
                stats: QueryStats::default(),
                warnings: vec![],
            }));
        }
        None => return Ok(None),
    };
    let entry = &q.match_clauses[0];
    if entry.optional || entry.shortest || !entry.pattern.is_flat() || entry.pattern.elements.len() != 1
    {
        return Ok(None);
    }
    let chain = entry.pattern.chain(0);
    let (min_hops, max_hops) = match chain.edge.length {
        PathLength::Range { min, max } if min > 0 && max >= min => (min, max),
        _ => return Ok(None),
    };
    if chain.edge.direction != Direction::Outgoing
        || chain.edge.var.is_some()
        || !chain.edge.properties.is_empty()
        || chain.edge.where_clause.is_some()
        || chain.node.where_clause.is_some()
    {
        return Ok(None);
    }
    let Some(terminal_var) = chain.node.var.as_deref() else {
        return Ok(None);
    };
    let mut terminal_properties = Vec::new();
    for (idx, item) in q.return_clause.items.iter().enumerate() {
        match &item.expr {
            Expr::PropertyAccess { target, property }
                if matches!(target.as_ref(), Expr::Variable(var) if var == terminal_var) =>
            {
                terminal_properties.push((idx, property.clone()));
            }
            _ => return Ok(None),
        }
    }

    let mut stats = QueryStats::default();
    let start_candidates = initial_candidates(&entry.pattern.start, graph, &mut stats, limits)?;
    let resolved_label = resolve_edge_label(&chain.edge, graph);
    let mut property_cache: RapidHashMap<(u32, String), Value> = RapidHashMap::default();
    let mut rows = Vec::new();

    fn dfs_var_len_projection<M: Memory>(
        current_vertex: u32,
        depth: u32,
        visited: &mut Vec<u32>,
        chain: &crate::ast::MatchChain,
        resolved_label: &ResolvedEdgeLabel,
        terminal_properties: &[(usize, String)],
        property_cache: &mut RapidHashMap<(u32, String), Value>,
        rows: &mut Vec<Vec<Value>>,
        limit: usize,
        graph: &PmaGraph<M>,
        stats: &mut QueryStats,
        limits: ExecutionLimits,
        min_hops: u32,
        max_hops: u32,
        width: usize,
    ) -> Result<(), GleaphError> {
        if rows.len() >= limit {
            return Ok(());
        }
        if depth >= min_hops && depth <= max_hops && depth > 0 && node_matches(&chain.node, current_vertex, graph)
        {
            let mut row = vec![Value::Null; width];
            for (idx, property) in terminal_properties.iter() {
                let cache_key = (current_vertex, property.clone());
                row[*idx] = property_cache
                    .entry(cache_key)
                    .or_insert_with(|| {
                        vertex_property_value_or_null(graph, current_vertex, property.as_str())
                    })
                    .clone();
            }
            rows.push(row);
            if rows.len() >= limit {
                return Ok(());
            }
        }
        if depth == max_hops {
            return Ok(());
        }
        for edge in graph.collect_neighbors(current_vertex)? {
            bump_steps(stats, 1, limits)?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(1);
            if edge.is_tombstoned()
                || graph.is_vertex_tombstoned(edge.target)
                || !resolved_label.matches(edge.label_id())
                || visited.contains(&edge.target)
            {
                continue;
            }
            visited.push(edge.target);
            dfs_var_len_projection(
                edge.target,
                depth + 1,
                visited,
                chain,
                resolved_label,
                terminal_properties,
                property_cache,
                rows,
                limit,
                graph,
                stats,
                limits,
                min_hops,
                max_hops,
                width,
            )?;
            visited.pop();
            if rows.len() >= limit {
                return Ok(());
            }
        }
        Ok(())
    }

    for start_vertex in start_candidates {
        let mut visited = vec![start_vertex];
        dfs_var_len_projection(
            start_vertex,
            0,
            &mut visited,
            chain,
            &resolved_label,
            &terminal_properties,
            &mut property_cache,
            &mut rows,
            limit,
            graph,
            &mut stats,
            limits,
            min_hops,
            max_hops,
            q.return_clause.items.len(),
        )?;
        if rows.len() >= limit {
            break;
        }
    }

    stats.breakdown.rows_after_match = rows.len() as u64;
    stats.breakdown.rows_after_with = rows.len() as u64;
    stats.breakdown.rows_before_projection = rows.len() as u64;
    stats.breakdown.limit_truncate_calls = stats.breakdown.limit_truncate_calls.saturating_add(1);
    stats.breakdown.var_len_terminal_projection_fast_path_used = true;
    stats.rows_emitted = rows.len() as u64;
    Ok(Some(QueryResult {
        columns: q.return_clause.items.iter().map(column_name).collect(),
        rows,
        stats,
        warnings: vec![],
    }))
}

fn edge_literal_properties_match<M: Memory>(
    graph: &PmaGraph<M>,
    edge: &crate::ast::EdgePattern,
    src: u32,
    dst: u32,
    label_ref: Option<&str>,
) -> bool {
    if edge.properties.is_empty() {
        return true;
    }
    let edge_props = graph
        .edge_record(src, dst, label_ref)
        .map(|e| e.props)
        .unwrap_or_default();
    edge.properties.iter().all(|(key, expected_expr)| {
        if let Expr::Literal(expected) = expected_expr {
            let actual = edge_props
                .iter()
                .find_map(|(k, v)| if k == key { Some(v.clone()) } else { None })
                .unwrap_or(Value::Null);
            compare_values(&actual, expected) == Some(Ordering::Equal)
        } else {
            true
        }
    })
}

fn execute_aggregate_query_fast<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<Option<QueryResult>, GleaphError> {
    if q.return_clause.star {
        return Ok(None);
    }
    let Some(entry) = q.match_clauses.first() else {
        return Ok(None);
    };
    if entry.optional || entry.shortest {
        return Ok(None);
    }
    let m = &entry.pattern;
    if m.elements.is_empty() || m.elements.len() > MAX_AGG_HOPS {
        return Ok(None);
    }
    // All chains must be within MAX_AGG_HOPS total depth.
    // Variable-length and multi-hop fixed chains are supported.
    let total_max_hops: u32 = m
        .hops()
        .map(|c| match c.edge.length {
            PathLength::Fixed(n) => n,
            PathLength::Range { max, .. } => max,
        })
        .sum();
    if total_max_hops as usize > MAX_AGG_HOPS {
        return Ok(None);
    }
    if q.return_clause
        .items
        .iter()
        .any(|item| expr_contains_aggregate(&item.expr) && !is_fast_path_aggregate(&item.expr))
    {
        return Ok(None);
    }
    if !q
        .return_clause
        .items
        .iter()
        .any(|item| expr_contains_aggregate(&item.expr))
    {
        return Ok(None);
    }
    // Try compiled fast path (works for any number of fixed-hop chains).
    if let Some(result) = execute_aggregate_query_compiled(q, m, graph, limits, build_hasher)? {
        return Ok(Some(result));
    }
    // Fallback: use general extend_match, then aggregate results.
    Ok(None)
}

fn execute_read_statement<M: Memory>(
    stmt: &Statement,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<QueryResult, GleaphError> {
    let _reg_guard = ensure_registry(stmt);
    match stmt {
        Statement::Query(q) => execute_query(q, graph, limits, &RandomState::new()),
        Statement::Compound { op, left, right } => {
            // §9.2: NEXT pipeline — execute left, use its rows as seed bindings for right.
            if let SetOp::Next(ref yield_cols) = *op {
                return execute_next_pipeline(left, right, yield_cols.as_deref(), graph, limits);
            }
            let left_result = execute_read_statement(left, graph, limits)?;
            let right_result = execute_read_statement(right, graph, limits)?;
            if left_result.columns.len() != right_result.columns.len() {
                return Err(GleaphError::ValidationError(format!(
                    "compound query column count mismatch: left={}, right={}",
                    left_result.columns.len(),
                    right_result.columns.len()
                )));
            }
            let mut rows = match op {
                SetOp::UnionAll => {
                    let mut out = left_result.rows.clone();
                    out.extend(right_result.rows.clone());
                    out
                }
                SetOp::Union => {
                    let mut out = left_result.rows.clone();
                    out.extend(right_result.rows.clone());
                    let mut seen = BTreeSet::new();
                    out.retain(|row| seen.insert(format!("{row:?}")));
                    out
                }
                SetOp::Except => {
                    let right_set = right_result
                        .rows
                        .iter()
                        .map(|r| format!("{r:?}"))
                        .collect::<BTreeSet<_>>();
                    left_result
                        .rows
                        .into_iter()
                        .filter(|r| !right_set.contains(&format!("{r:?}")))
                        .collect::<Vec<_>>()
                }
                SetOp::Intersect => {
                    let right_set = right_result
                        .rows
                        .iter()
                        .map(|r| format!("{r:?}"))
                        .collect::<BTreeSet<_>>();
                    let mut seen = BTreeSet::new();
                    left_result
                        .rows
                        .into_iter()
                        .filter(|r| {
                            let key = format!("{r:?}");
                            right_set.contains(&key) && seen.insert(key)
                        })
                        .collect::<Vec<_>>()
                }
                SetOp::Otherwise => {
                    // Execute left; if empty, use right
                    if !left_result.rows.is_empty() {
                        left_result.rows.clone()
                    } else {
                        right_result.rows.clone()
                    }
                }
                SetOp::Next(_) => unreachable!("handled above"),
            };
            let mut stats = left_result.stats;
            merge_query_stats(&mut stats, &right_result.stats);
            stats.rows_emitted = rows.len() as u64;
            Ok(QueryResult {
                columns: left_result.columns,
                rows: std::mem::take(&mut rows),
                stats,
                warnings: vec![],
            })
        }
        Statement::Finish => Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            stats: QueryStats::default(),
            warnings: vec![],
        }),
        Statement::Let(l) => execute_let_statement(l, graph, limits),
        Statement::Filter(f) => execute_filter_statement(f, graph, limits),
        Statement::For(f) => execute_for_statement(f, graph),
        Statement::Call(c) => execute_call_statement(c, &Bindings::new(), graph, limits),
        // §16.2/§12: Catalog DDL — no-op in native context (IC routing in gql_bridge).
        Statement::UseGraph(_)
        | Statement::CreateGraph { .. }
        | Statement::DropGraph { .. }
        | Statement::CreateGraphType { .. }
        | Statement::DropGraphType { .. }
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. }
        | Statement::DescribeGraphType(_) => Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            stats: QueryStats::default(),
            warnings: vec![],
        }),
        _ => Err(GleaphError::ValidationError(
            "read execution only accepts query/compound statements".into(),
        )),
    }
}

fn execute_let_statement<M: Memory>(
    l: &crate::ast::LetStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<QueryResult, GleaphError> {
    let mut stats = QueryStats::default();
    let mut rows = execute_match_clause(
        &l.match_clause,
        graph,
        &mut stats,
        l.where_clause.as_ref(),
        None,
        limits,
    )?;
    // Add computed bindings from LET clause
    for row in &mut rows {
        for (var, expr) in &l.bindings {
            let val = eval_expr(expr, row, graph);
            row.insert(var.clone(), Binding::Value(val));
        }
    }
    let (columns, projected) = if l.return_clause.star {
        let cols = star_columns(&rows);
        let proj = rows.iter().map(project_star_row).collect::<Vec<_>>();
        (cols, proj)
    } else {
        let cols = l
            .return_clause
            .items
            .iter()
            .map(column_name)
            .collect::<Vec<_>>();
        let proj = rows
            .iter()
            .map(|bindings| {
                l.return_clause
                    .items
                    .iter()
                    .map(|item| eval_expr(&item.expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        (cols, proj)
    };
    stats.rows_emitted = projected.len() as u64;
    Ok(QueryResult {
        columns,
        rows: projected,
        stats,
        warnings: vec![],
    })
}

fn execute_filter_statement<M: Memory>(
    f: &crate::ast::FilterStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<QueryResult, GleaphError> {
    let mut stats = QueryStats::default();
    let rows = execute_match_clause(
        &f.match_clause,
        graph,
        &mut stats,
        f.where_clause.as_ref(),
        None,
        limits,
    )?;
    // Apply filter_expr
    let filtered: Vec<Bindings> = rows
        .into_iter()
        .filter(|bindings| truthy(&eval_expr(&f.filter_expr, bindings, graph)))
        .collect();
    // Return column names from filter_expr bindings
    let columns: Vec<String> = f
        .match_clause
        .start
        .var
        .iter()
        .cloned()
        .chain(
            f.match_clause
                .hops()
                .flat_map(|c| c.edge.var.iter().cloned().chain(c.node.var.iter().cloned())),
        )
        .collect();
    let projected = filtered
        .iter()
        .map(|bindings| {
            columns
                .iter()
                .map(|col| binding_value(col, bindings))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    stats.rows_emitted = projected.len() as u64;
    Ok(QueryResult {
        columns,
        rows: projected,
        stats,
        warnings: vec![],
    })
}

/// §14.8: Execute a `FOR item IN list_expr RETURN ...` statement.
///
/// Evaluates `list_expr` against an empty binding set to get a list, then for each element
/// yields a row with `var` bound to that element (and optionally `ordinality_var` bound to
/// the 1-based element index). Projects the RETURN clause for each resulting binding row.
fn execute_for_statement<M: Memory>(
    f: &crate::ast::ForStmt,
    graph: &PmaGraph<M>,
) -> Result<QueryResult, GleaphError> {
    let empty_bindings = Bindings::new();
    let list_val = eval_expr(&f.list_expr, &empty_bindings, graph);
    let elements = match list_val {
        Value::List(elems) => elems,
        other => vec![other], // treat a non-list value as a one-element list
    };
    let mut binding_rows: Vec<Bindings> = Vec::new();
    for (idx, elem) in elements.into_iter().enumerate() {
        let mut bindings = Bindings::new();
        bindings.insert(f.var.clone(), Binding::Value(elem));
        if let Some(ord_var) = &f.ordinality_var {
            bindings.insert(
                ord_var.clone(),
                Binding::Value(Value::Int64((idx as i64) + 1)),
            );
        }
        binding_rows.push(bindings);
    }
    let columns = if f.return_clause.star {
        star_columns(&binding_rows)
    } else {
        f.return_clause
            .items
            .iter()
            .map(column_name)
            .collect::<Vec<_>>()
    };
    let projected: Vec<Row> = if f.return_clause.star {
        binding_rows.iter().map(project_star_row).collect()
    } else {
        binding_rows
            .iter()
            .map(|bindings| {
                f.return_clause
                    .items
                    .iter()
                    .map(|item| eval_expr(&item.expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    let stats = QueryStats {
        rows_emitted: projected.len() as u64,
        ..Default::default()
    };
    Ok(QueryResult {
        columns,
        rows: projected,
        stats,
        warnings: vec![],
    })
}

/// §15.2: Execute a `CALL (<scope_vars>) { <body> }` inline subquery.
///
/// Variables in `scope_vars` are extracted from `outer_bindings` and pre-seeded into the
/// inner query's binding table. For a `Query` body, uses `execute_query_match_entries_from_seed_rows`.
/// The result rows are returned independently (not joined with outer scope for now).
fn execute_call_statement<M: Memory>(
    c: &crate::ast::CallStmt,
    outer_bindings: &Bindings,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<QueryResult, GleaphError> {
    let result = execute_call_statement_inner(c, outer_bindings, graph, limits);
    if c.optional {
        result.or_else(|_| {
            Ok(QueryResult {
                columns: vec![],
                rows: vec![],
                stats: QueryStats::default(),
                warnings: vec![],
            })
        })
    } else {
        result
    }
}

fn execute_call_statement_inner<M: Memory>(
    c: &crate::ast::CallStmt,
    outer_bindings: &Bindings,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<QueryResult, GleaphError> {
    // Build a seed row from outer scope variables listed in scope_vars.
    let seed: Bindings = c
        .scope_vars
        .iter()
        .filter_map(|var| outer_bindings.get(var).map(|b| (var.clone(), b.clone())))
        .collect();
    let seed_rows = if seed.is_empty() {
        vec![Bindings::new()]
    } else {
        vec![seed]
    };
    match c.body.as_ref() {
        Statement::Query(q) => {
            let mut stats = QueryStats::default();
            let mut rows = execute_query_match_entries_from_seed_rows(
                q,
                graph,
                &mut stats,
                q.where_clause.as_ref(),
                None,
                limits,
                seed_rows,
                None,
            )?;
            if q.match_mode == Some(MatchMode::DifferentEdges) {
                rows.retain(different_edges_allows);
            }
            stats.breakdown.rows_after_match = rows.len() as u64;
            rows = apply_with_clauses(q, rows, graph, &mut stats, limits)?;
            stats.breakdown.rows_after_with = rows.len() as u64;
            let is_agg = query_has_aggregate(q);
            let default_hasher = RandomState::new();
            stats.breakdown.rows_before_projection = rows.len() as u64;
            let projected_rows = if q.return_clause.star {
                rows.iter().map(project_star_row).collect::<Vec<_>>()
            } else if is_agg {
                project_aggregated_rows(q, &rows, graph, &default_hasher, Some(&mut stats))?
            } else {
                rows.iter()
                    .map(|bindings| {
                        q.return_clause
                            .items
                            .iter()
                            .map(|item| eval_expr(&item.expr, bindings, graph))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            };
            let columns = if q.return_clause.star {
                star_columns(&rows)
            } else {
                q.return_clause
                    .items
                    .iter()
                    .map(column_name)
                    .collect::<Vec<_>>()
            };
            stats.rows_emitted = projected_rows.len() as u64;
            Ok(QueryResult {
                columns,
                rows: projected_rows,
                stats,
                warnings: vec![],
            })
        }
        // For nested CALL or other statements, execute them with outer scope.
        other => execute_read_statement(other, graph, limits),
    }
}

/// §9.2: Execute a `stmt1 NEXT stmt2` pipeline.
///
/// Executes `left`, converts its result rows into binding maps (column_name → Value),
/// then uses those maps as seed bindings for `right`. If `right` is a `Query`, runs
/// `execute_query_match_entries_from_seed_rows`. If it's a compound pipeline, recurses.
fn execute_next_pipeline<M: Memory>(
    left: &Statement,
    right: &Statement,
    yield_cols: Option<&[String]>,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<QueryResult, GleaphError> {
    let left_result = execute_read_statement(left, graph, limits)?;
    // Convert left result rows to Bindings maps, using column names as variable names.
    let seed_rows: Vec<Bindings> = left_result
        .rows
        .iter()
        .map(|row| {
            let all_bindings = left_result
                .columns
                .iter()
                .zip(row.iter())
                .map(|(col, val)| (col.clone(), Binding::Value(val.clone())));
            // S1: When YIELD projects specific columns, filter seed bindings.
            if let Some(cols) = yield_cols {
                all_bindings
                    .filter(|(col, _)| cols.iter().any(|c| c == col))
                    .collect::<Bindings>()
            } else {
                all_bindings.collect::<Bindings>()
            }
        })
        .collect();
    // Execute right with seed rows.
    match right {
        Statement::Query(q) => {
            let mut stats = QueryStats::default();
            let mut rows = execute_query_match_entries_from_seed_rows(
                q,
                graph,
                &mut stats,
                q.where_clause.as_ref(),
                None,
                limits,
                seed_rows,
                None,
            )?;
            if q.match_mode == Some(MatchMode::DifferentEdges) {
                rows.retain(different_edges_allows);
            }
            stats.breakdown.rows_after_match = rows.len() as u64;
            rows = apply_with_clauses(q, rows, graph, &mut stats, limits)?;
            stats.breakdown.rows_after_with = rows.len() as u64;
            let is_agg = query_has_aggregate(q);
            let default_hasher = RandomState::new();
            stats.breakdown.rows_before_projection = rows.len() as u64;
            let projected_rows = if q.return_clause.star {
                rows.iter().map(project_star_row).collect::<Vec<_>>()
            } else if is_agg {
                project_aggregated_rows(q, &rows, graph, &default_hasher, Some(&mut stats))?
            } else {
                rows.iter()
                    .map(|bindings| {
                        q.return_clause
                            .items
                            .iter()
                            .map(|item| eval_expr(&item.expr, bindings, graph))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            };
            let columns = if q.return_clause.star {
                star_columns(&rows)
            } else {
                q.return_clause
                    .items
                    .iter()
                    .map(column_name)
                    .collect::<Vec<_>>()
            };
            stats.rows_emitted = projected_rows.len() as u64;
            Ok(QueryResult {
                columns,
                rows: projected_rows,
                stats,
                warnings: vec![],
            })
        }
        // §15.2: CALL on the right — execute inner body for each seed row.
        Statement::Call(c) => {
            let mut all_rows: Vec<Row> = Vec::new();
            let mut combined_stats = QueryStats::default();
            let mut columns: Vec<String> = vec![];
            for seed in &seed_rows {
                let result = execute_call_statement(c, seed, graph, limits)?;
                if columns.is_empty() {
                    columns = result.columns;
                }
                all_rows.extend(result.rows);
                merge_query_stats(&mut combined_stats, &result.stats);
            }
            combined_stats.rows_emitted = all_rows.len() as u64;
            Ok(QueryResult {
                columns,
                rows: all_rows,
                stats: combined_stats,
                warnings: vec![],
            })
        }
        // For nested NEXT pipelines, recursively call with seeds passed through.
        Statement::Compound {
            op: SetOp::Next(inner_yield),
            left: inner_left,
            right: inner_right,
        } => execute_next_pipeline(
            inner_left,
            inner_right,
            inner_yield.as_deref(),
            graph,
            limits,
        ),
        // Fallback: execute right independently (seeds lost).
        _ => execute_read_statement(right, graph, limits),
    }
}

/// Executes a read-only statement (single query or compound UNION/EXCEPT) against
/// `graph` with no execution limits.
///
/// Unlike [`execute_plan`], this path bypasses the physical planner and handles
/// compound statements directly.  Intended for host-native integration tests.
pub fn execute_query_statement<M: Memory + Clone>(
    stmt: &Statement,
    graph: &PmaGraph<M>,
) -> Result<QueryResult, GleaphError> {
    execute_read_statement(stmt, graph, ExecutionLimits::default())
}

/// Like [`execute_query_statement`] but with execution limits.
///
/// Handles both single queries and compound statements (UNION/EXCEPT/INTERSECT).
pub fn execute_query_statement_with_limits<M: Memory>(
    stmt: &Statement,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<QueryResult, GleaphError> {
    execute_read_statement(stmt, graph, limits)
}

/// Executes a `CREATE` or `DELETE` statement against `graph` with no limits.
///
/// This is a convenience wrapper around [`execute_mutation_tracked`].
pub fn execute_mutation<M: Memory>(
    stmt: &Statement,
    graph: &mut PmaGraph<M>,
) -> Result<MutationResult, GleaphError> {
    execute_mutation_tracked(stmt, graph, ExecutionLimits::default(), 0).map(|o| o.result)
}

/// Executes a `CREATE` or `DELETE` statement against `graph`, aborting early
/// if `limits` are exceeded during the MATCH phase of a DELETE.
///
/// Returns [`GleaphError::ValidationError`] if `stmt` is not a mutation.
pub fn execute_mutation_with_limits<M: Memory>(
    stmt: &Statement,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationResult, GleaphError> {
    execute_mutation_tracked(stmt, graph, limits, 0).map(|o| o.result)
}

/// Like [`execute_mutation_with_limits`] but returns a [`MutationOutcome`]
/// that includes the affected vertex IDs for incremental index maintenance.
pub fn execute_mutation_tracked<M: Memory>(
    stmt: &Statement,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
    edge_timestamp: u64,
) -> Result<MutationOutcome, GleaphError> {
    let _reg_guard = ensure_registry(stmt);
    match stmt {
        Statement::Create(cs) => execute_create_multi(cs, graph, edge_timestamp),
        Statement::Merge(m) => execute_merge(m, graph, limits, edge_timestamp),
        Statement::Delete(d) => execute_delete(d, graph, limits),
        Statement::Set(s) => execute_set(s, graph, limits),
        Statement::Remove(r) => execute_remove(r, graph, limits),
        _ => Err(GleaphError::ValidationError(
            "mutate endpoint only accepts CREATE/DELETE/SET/REMOVE/MERGE".into(),
        )),
    }
}

fn execute_query<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    limits: ExecutionLimits,
    build_hasher: &S,
) -> Result<QueryResult, GleaphError> {
    let _reg_guard = ensure_registry_for_query(q);
    let mut stats = QueryStats::default();
    let pushdown_limit = if q.order_by.is_none() {
        q.limit.map(|l| l.0 as usize)
    } else {
        None
    };
    let mut rows = execute_query_match_entries(
        q,
        graph,
        &mut stats,
        q.where_clause.as_ref(),
        pushdown_limit,
        limits,
    )?;
    // §16.4: MATCH DIFFERENT EDGES — reject rows where any edge appears more than once.
    if q.match_mode == Some(MatchMode::DifferentEdges) {
        rows.retain(different_edges_allows);
    }
    stats.breakdown.rows_after_match = rows.len() as u64;
    rows = apply_with_clauses(q, rows, graph, &mut stats, limits)?;
    stats.breakdown.rows_after_with = rows.len() as u64;

    let is_agg = query_has_aggregate(q);
    let compiled_return_exprs = if !q.return_clause.star && !is_agg {
        compile_value_exprs(q.return_clause.items.iter().map(|item| &item.expr))
    } else {
        None
    };

    if let Some(order_by) = &q.order_by
        && !is_agg
    {
        let compiled_order_exprs =
            compile_value_exprs(order_by.items.iter().map(|item| &item.expr));
        bump_steps(&mut stats, rows.len() as u64, limits)?;
        if let Some(limit) = q.limit {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            rows = top_k_rows(
                rows,
                order_by,
                compiled_order_exprs.as_deref(),
                limit.0 as usize,
                graph,
            );
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            rows.sort_by(|a, b| {
                compare_rows_for_order(order_by, a, b, compiled_order_exprs.as_deref(), graph)
            });
        }
    }

    if let Some(limit) = q.limit
        && !is_agg
    {
        rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    stats.breakdown.rows_before_projection = rows.len() as u64;

    let mut projected_rows = if q.return_clause.star {
        rows.iter().map(project_star_row).collect::<Vec<_>>()
    } else if is_agg {
        project_aggregated_rows(q, &rows, graph, build_hasher, Some(&mut stats))?
    } else if let Some(compiled_exprs) = compiled_return_exprs.as_deref() {
        rows.iter()
            .map(|bindings| {
                compiled_exprs
                    .iter()
                    .map(|expr| eval_compiled_value_expr(expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    } else {
        rows.iter()
            .map(|bindings| {
                q.return_clause
                    .items
                    .iter()
                    .map(|item| eval_expr(&item.expr, bindings, graph))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let columns = if q.return_clause.star {
        star_columns(&rows)
    } else {
        q.return_clause
            .items
            .iter()
            .map(column_name)
            .collect::<Vec<_>>()
    };
    if q.return_clause.distinct {
        let mut seen = BTreeSet::new();
        if q.order_by.is_none()
            && let Some(limit) = q.limit
        {
            // Early termination: stop once k distinct rows collected.
            let k = limit.0 as usize;
            let mut deduped = Vec::with_capacity(k);
            for row in projected_rows.into_iter() {
                if seen.insert(format!("{row:?}")) {
                    deduped.push(row);
                    if deduped.len() >= k {
                        break;
                    }
                }
            }
            projected_rows = deduped;
        } else {
            projected_rows.retain(|row| seen.insert(format!("{row:?}")));
        }
    }
    if let Some(order_by) = &q.order_by
        && is_agg
    {
        if let Some(limit) = q.limit
            && q.offset.is_none()
        {
            stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
            projected_rows = top_k_projected_aggregate_rows(
                q,
                order_by,
                projected_rows,
                limit.0 as usize,
                graph,
            )?;
        } else {
            stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
            sort_projected_aggregate_rows(q, order_by, &mut projected_rows, graph)?;
        }
    }
    if let Some(limit) = q.limit
        && is_agg
    {
        projected_rows.truncate(limit.0 as usize);
        stats.breakdown.limit_truncate_calls =
            stats.breakdown.limit_truncate_calls.saturating_add(1);
    }
    if let Some(offset) = q.offset {
        let off = offset as usize;
        if off >= projected_rows.len() {
            projected_rows.clear();
        } else {
            projected_rows.drain(0..off);
        }
    }

    let projection_cells = projected_rows
        .len()
        .saturating_mul(if q.return_clause.star {
            columns.len()
        } else {
            q.return_clause.items.len()
        });
    bump_steps(&mut stats, projection_cells as u64, limits)?;
    stats.rows_emitted = projected_rows.len() as u64;
    Ok(QueryResult {
        columns,
        rows: projected_rows,
        stats,
        warnings: vec![],
    })
}

fn apply_with_clauses<M: Memory>(
    q: &QueryStmt,
    mut rows: Vec<Bindings>,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    limits: ExecutionLimits,
) -> Result<Vec<Bindings>, GleaphError> {
    for w in &q.with_clauses {
        let is_agg = with_clause_has_aggregate(w);
        if let Some(order_by) = &w.order_by
            && !is_agg
        {
            let compiled_order_exprs =
                compile_value_exprs(order_by.items.iter().map(|item| &item.expr));
            bump_steps(stats, rows.len() as u64, limits)?;
            if let Some(limit) = w.limit {
                stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
                rows = top_k_rows(
                    rows,
                    order_by,
                    compiled_order_exprs.as_deref(),
                    limit.0 as usize,
                    graph,
                );
            } else {
                stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
                rows.sort_by(|a, b| {
                    compare_rows_for_order(order_by, a, b, compiled_order_exprs.as_deref(), graph)
                });
            }
        }
        if let Some(limit) = w.limit
            && !is_agg
        {
            rows.truncate(limit.0 as usize);
            stats.breakdown.limit_truncate_calls =
                stats.breakdown.limit_truncate_calls.saturating_add(1);
        }

        rows = if is_agg {
            project_with_aggregated_rows(w, &rows, graph, stats)?
        } else {
            rows.iter()
                .map(|bindings| project_with_row(w, bindings, graph))
                .collect::<Result<Vec<_>, _>>()?
        };

        if w.distinct {
            let mut seen = BTreeSet::new();
            rows.retain(|row| seen.insert(format!("{row:?}")));
        }
        if let Some(order_by) = &w.order_by
            && is_agg
        {
            if let Some(limit) = w.limit
                && w.offset.is_none()
            {
                stats.breakdown.top_k_calls = stats.breakdown.top_k_calls.saturating_add(1);
                rows = top_k_binding_rows_for_with_aggregate(
                    w,
                    order_by,
                    rows,
                    limit.0 as usize,
                    graph,
                )?;
            } else {
                stats.breakdown.full_sort_calls = stats.breakdown.full_sort_calls.saturating_add(1);
                sort_binding_rows_for_with_aggregate(w, order_by, &mut rows, graph)?;
            }
        }
        if let Some(limit) = w.limit
            && is_agg
        {
            rows.truncate(limit.0 as usize);
            stats.breakdown.limit_truncate_calls =
                stats.breakdown.limit_truncate_calls.saturating_add(1);
        }
        if let Some(offset) = w.offset {
            let off = offset as usize;
            if off >= rows.len() {
                rows.clear();
            } else {
                rows.drain(0..off);
            }
        }
        if let Some(where_clause) = &w.where_clause {
            rows.retain(|b| truthy(&eval_expr(where_clause, b, graph)));
        }

        // Execute follow-on MATCH clauses in the WITH continuation.
        // Pass post_match_where to the last MATCH entry for pushdown
        // (timestamp pushdown, partial predicate evaluation) — matching
        // the pattern in execute_query_match_entries_from_seed_rows.
        if !w.match_clauses.is_empty() {
            let last_idx = w.match_clauses.len() - 1;
            for (idx, entry) in w.match_clauses.iter().enumerate() {
                bump_steps(stats, rows.len() as u64, limits)?;
                stats.breakdown.with_continuation_match_calls = stats
                    .breakdown
                    .with_continuation_match_calls
                    .saturating_add(1);
                stats.breakdown.with_continuation_match_input_rows = stats
                    .breakdown
                    .with_continuation_match_input_rows
                    .saturating_add(rows.len() as u64);
                let before = stats.breakdown.clone();
                let before_scanned_edges = stats.scanned_edges;
                let before_execution_steps = stats.execution_steps;
                let apply_where = if idx == last_idx {
                    w.post_match_where.as_ref()
                } else {
                    None
                };
                rows = execute_match_clause_joined(
                    &entry.pattern,
                    entry.shortest,
                    entry.shortest_mode,
                    entry.path_variable.as_deref(),
                    entry.path_mode,
                    &rows,
                    graph,
                    stats,
                    apply_where,
                    limits.max_rows,
                    limits,
                    entry.optional,
                )?;
                let cont_joined_match_start_candidates = stats
                    .breakdown
                    .joined_match_start_candidates
                    .saturating_sub(before.joined_match_start_candidates);
                let cont_joined_local_rows_before_inline_where = stats
                    .breakdown
                    .joined_match_local_rows_before_inline_where
                    .saturating_sub(before.joined_match_local_rows_before_inline_where);
                let cont_joined_local_rows_after_inline_where = stats
                    .breakdown
                    .joined_match_local_rows_after_inline_where
                    .saturating_sub(before.joined_match_local_rows_after_inline_where);
                let cont_hop_label_rejects = stats
                    .breakdown
                    .hop_label_rejects
                    .saturating_sub(before.hop_label_rejects);
                let cont_outgoing_hop_candidates = stats
                    .breakdown
                    .outgoing_hop_candidates
                    .saturating_sub(before.outgoing_hop_candidates);
                let cont_incoming_hop_candidates = stats
                    .breakdown
                    .incoming_hop_candidates
                    .saturating_sub(before.incoming_hop_candidates);
                let cont_outgoing_hop_label_rejects = stats
                    .breakdown
                    .outgoing_hop_label_rejects
                    .saturating_sub(before.outgoing_hop_label_rejects);
                let cont_incoming_hop_label_rejects = stats
                    .breakdown
                    .incoming_hop_label_rejects
                    .saturating_sub(before.incoming_hop_label_rejects);
                let cont_hop_node_rejects = stats
                    .breakdown
                    .hop_node_rejects
                    .saturating_sub(before.hop_node_rejects);
                let cont_hop_edge_property_rejects = stats
                    .breakdown
                    .hop_edge_property_rejects
                    .saturating_sub(before.hop_edge_property_rejects);
                let cont_hop_where_pushdown_rejects = stats
                    .breakdown
                    .hop_where_pushdown_rejects
                    .saturating_sub(before.hop_where_pushdown_rejects);
                let cont_var_len_cycle_rejects = stats
                    .breakdown
                    .var_len_cycle_rejects
                    .saturating_sub(before.var_len_cycle_rejects);
                stats.breakdown.with_continuation_match_output_rows = stats
                    .breakdown
                    .with_continuation_match_output_rows
                    .saturating_add(rows.len() as u64);
                stats.breakdown.with_continuation_joined_match_start_candidates = stats
                    .breakdown
                    .with_continuation_joined_match_start_candidates
                    .saturating_add(cont_joined_match_start_candidates);
                stats.breakdown.with_continuation_joined_local_rows_before_inline_where = stats
                    .breakdown
                    .with_continuation_joined_local_rows_before_inline_where
                    .saturating_add(cont_joined_local_rows_before_inline_where);
                stats.breakdown.with_continuation_joined_local_rows_after_inline_where = stats
                    .breakdown
                    .with_continuation_joined_local_rows_after_inline_where
                    .saturating_add(cont_joined_local_rows_after_inline_where);
                stats.breakdown.with_continuation_scanned_edges = stats
                    .breakdown
                    .with_continuation_scanned_edges
                    .saturating_add(stats.scanned_edges.saturating_sub(before_scanned_edges));
                stats.breakdown.with_continuation_execution_steps = stats
                    .breakdown
                    .with_continuation_execution_steps
                    .saturating_add(
                        stats.execution_steps
                            .saturating_sub(before_execution_steps),
                    );
                stats.breakdown.with_continuation_hop_label_rejects = stats
                    .breakdown
                    .with_continuation_hop_label_rejects
                    .saturating_add(cont_hop_label_rejects);
                stats.breakdown.with_continuation_outgoing_hop_candidates = stats
                    .breakdown
                    .with_continuation_outgoing_hop_candidates
                    .saturating_add(cont_outgoing_hop_candidates);
                stats.breakdown.with_continuation_incoming_hop_candidates = stats
                    .breakdown
                    .with_continuation_incoming_hop_candidates
                    .saturating_add(cont_incoming_hop_candidates);
                stats.breakdown.with_continuation_outgoing_hop_label_rejects = stats
                    .breakdown
                    .with_continuation_outgoing_hop_label_rejects
                    .saturating_add(cont_outgoing_hop_label_rejects);
                stats.breakdown.with_continuation_incoming_hop_label_rejects = stats
                    .breakdown
                    .with_continuation_incoming_hop_label_rejects
                    .saturating_add(cont_incoming_hop_label_rejects);
                stats.breakdown.with_continuation_hop_node_rejects = stats
                    .breakdown
                    .with_continuation_hop_node_rejects
                    .saturating_add(cont_hop_node_rejects);
                stats.breakdown.with_continuation_hop_edge_property_rejects = stats
                    .breakdown
                    .with_continuation_hop_edge_property_rejects
                    .saturating_add(cont_hop_edge_property_rejects);
                stats.breakdown.with_continuation_hop_where_pushdown_rejects = stats
                    .breakdown
                    .with_continuation_hop_where_pushdown_rejects
                    .saturating_add(cont_hop_where_pushdown_rejects);
                stats.breakdown.with_continuation_var_len_cycle_rejects = stats
                    .breakdown
                    .with_continuation_var_len_cycle_rejects
                    .saturating_add(cont_var_len_cycle_rejects);
            }
        }
    }
    Ok(rows)
}

fn execute_query_match_entries<M: Memory>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<Vec<Bindings>, GleaphError> {
    execute_query_match_entries_from_seed_rows(
        q,
        graph,
        stats,
        where_clause,
        max_rows,
        limits,
        vec![Bindings::new()],
        None,
    )
}

fn execute_query_match_entries_from_seed_rows<M: Memory>(
    q: &QueryStmt,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
    mut rows: Vec<Bindings>,
    clause_order: Option<&[usize]>,
) -> Result<Vec<Bindings>, GleaphError> {
    let _compiled_where_guard = push_compiled_where_scope(where_clause);
    let default_order: Vec<usize> = (0..q.match_clauses.len()).collect();
    let order = clause_order.unwrap_or(&default_order);
    let total = q.match_clauses.len();
    for (step, &clause_idx) in order.iter().enumerate() {
        let entry = &q.match_clauses[clause_idx];
        // Apply WHERE only after the last clause is processed.
        let apply_where = if step + 1 == total {
            where_clause
        } else {
            None
        };
        rows = execute_match_clause_joined(
            &entry.pattern,
            entry.shortest,
            entry.shortest_mode,
            entry.path_variable.as_deref(),
            entry.path_mode,
            &rows,
            graph,
            stats,
            apply_where,
            max_rows,
            limits,
            entry.optional,
        )?;
        if let Some(k) = entry.any_paths {
            rows.truncate(k as usize);
        }
        if let Some(ref keep) = entry.keep_clause {
            apply_keep_clause(&mut rows, keep);
        }
    }
    Ok(rows)
}

/// Apply KEEP clause filtering to restrict bindings to specified variables.
fn apply_keep_clause(rows: &mut [Bindings], keep: &crate::ast::KeepClause) {
    match keep {
        crate::ast::KeepClause::All => {} // no-op
        crate::ast::KeepClause::Vars(vars) => {
            for row in rows.iter_mut() {
                // Collect names to remove (those not in `vars`).
                let to_remove: Vec<String> = row
                    .keys()
                    .into_iter()
                    .filter(|k| !vars.iter().any(|v| v == k))
                    .collect();
                for name in &to_remove {
                    row.remove(name);
                }
            }
        }
    }
}

fn equality_property_literal_predicate(
    where_clause: Option<&WhereClause>,
) -> Option<(String, String, Value)> {
    /// Resolve a value from a literal or parameter expression.
    fn resolve_value(expr: &Expr) -> Option<Value> {
        match expr {
            Expr::Literal(v) => Some(v.clone()),
            Expr::Parameter { name, .. } => {
                let val = QUERY_PARAMS
                    .with(|p| p.borrow().get(name).cloned())
                    .unwrap_or(Value::Null);
                if matches!(val, Value::Null) {
                    None
                } else {
                    Some(val)
                }
            }
            _ => None,
        }
    }
    fn walk(expr: &Expr) -> Option<(String, String, Value)> {
        match expr {
            Expr::Compare { left, op, right } if *op == CmpOp::Eq => {
                // Try prop = value_source
                if let Expr::PropertyAccess { target, property } = left.as_ref() {
                    if let Expr::Variable(var) = target.as_ref() {
                        if let Some(val) = resolve_value(right) {
                            return Some((var.clone(), property.clone(), val));
                        }
                    }
                }
                // Try value_source = prop
                if let Expr::PropertyAccess { target, property } = right.as_ref() {
                    if let Expr::Variable(var) = target.as_ref() {
                        if let Some(val) = resolve_value(left) {
                            return Some((var.clone(), property.clone(), val));
                        }
                    }
                }
                None
            }
            Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => walk(l).or_else(|| walk(r)),
            Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => walk(e),
            _ => None,
        }
    }
    where_clause.and_then(walk)
}

/// Extracts the first range comparison predicate (`var.prop >= literal/param`, etc.)
/// from a WHERE clause. Returns `(variable, property, value, ConditionalCmpOp)`.
fn range_property_literal_predicate(
    where_clause: Option<&WhereClause>,
) -> Option<(String, String, Value, ConditionalCmpOp)> {
    /// Resolve a value from a literal or parameter expression.
    fn resolve_value(expr: &Expr) -> Option<Value> {
        match expr {
            Expr::Literal(v) => Some(v.clone()),
            Expr::Parameter { name, .. } => {
                let val = QUERY_PARAMS
                    .with(|p| p.borrow().get(name).cloned())
                    .unwrap_or(Value::Null);
                if matches!(val, Value::Null) {
                    None
                } else {
                    Some(val)
                }
            }
            _ => None,
        }
    }
    fn walk(expr: &Expr) -> Option<(String, String, Value, ConditionalCmpOp)> {
        match expr {
            Expr::Compare { left, op, right }
                if matches!(op, CmpOp::Ge | CmpOp::Gt | CmpOp::Le | CmpOp::Lt) =>
            {
                let (var, prop, val, reversed) =
                    if let Expr::PropertyAccess { target, property } = left.as_ref() {
                        if let Expr::Variable(v) = target.as_ref() {
                            if let Some(val) = resolve_value(right) {
                                (v.clone(), property.clone(), val, false)
                            } else {
                                return None;
                            }
                        } else {
                            return None;
                        }
                    } else if let Expr::PropertyAccess { target, property } = right.as_ref() {
                        if let Expr::Variable(v) = target.as_ref() {
                            if let Some(val) = resolve_value(left) {
                                (v.clone(), property.clone(), val, true)
                            } else {
                                return None;
                            }
                        } else {
                            return None;
                        }
                    } else {
                        return None;
                    };
                let cmp_op = match (op, reversed) {
                    (CmpOp::Ge, false) | (CmpOp::Le, true) => ConditionalCmpOp::Ge,
                    (CmpOp::Gt, false) | (CmpOp::Lt, true) => ConditionalCmpOp::Gt,
                    (CmpOp::Le, false) | (CmpOp::Ge, true) => ConditionalCmpOp::Le,
                    (CmpOp::Lt, false) | (CmpOp::Gt, true) => ConditionalCmpOp::Lt,
                    _ => return None,
                };
                Some((var, prop, val, cmp_op))
            }
            Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => walk(l).or_else(|| walk(r)),
            Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => walk(e),
            _ => None,
        }
    }
    where_clause.and_then(walk)
}

/// Extracts the first `(variable, property, literal_value)` triple from inline
/// `props_hint` on node patterns in the first MATCH clause.
fn inline_props_hint_literal_predicate(q: &QueryStmt) -> Option<(String, String, Value)> {
    let first_entry = q.match_clauses.first()?;
    let m = &first_entry.pattern;
    for (prop, expr) in &m.start.props_hint {
        if let Expr::Literal(val) = expr {
            let var = m
                .start
                .var
                .clone()
                .unwrap_or_else(|| "__anon_start__".to_string());
            return Some((var, prop.clone(), val.clone()));
        }
    }
    for (i, elem) in m.elements.iter().enumerate() {
        let PatternElement::Hop(chain) = elem else {
            continue;
        };
        for (prop, expr) in &chain.node.props_hint {
            if let Expr::Literal(val) = expr {
                let var = chain
                    .node
                    .var
                    .clone()
                    .unwrap_or_else(|| format!("__anon_chain_{i}__"));
                return Some((var, prop.clone(), val.clone()));
            }
        }
    }
    None
}

/// Hidden path variable used when SHORTEST is requested without a named path variable.
/// Allows path length tracking internally; stripped from output rows after selection.
const INTERNAL_PATH_VAR: &str = "__shortest_internal__";

#[allow(clippy::too_many_arguments)]
fn execute_match_clause_joined<M: Memory>(
    m: &MatchClause,
    shortest: bool,
    shortest_mode: Option<crate::ast::ShortestMode>,
    path_variable: Option<&str>,
    path_mode: Option<PathMode>,
    input_rows: &[Bindings],
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
    optional: bool,
) -> Result<Vec<Bindings>, GleaphError> {
    // §16.6: When SHORTEST is requested without a named path variable, use a hidden internal
    // variable to track path length, then strip it from output rows after shortest selection.
    // Use an internal path variable to track traversal for:
    // 1. SHORTEST without a named path variable (need path length)
    // 2. Non-Walk path mode with SubPath elements (need edge/vertex tracking across repetitions)
    let has_subpath = m
        .elements
        .iter()
        .any(|e| matches!(e, PatternElement::SubPath { .. }));
    let needs_internal_path = (shortest && path_variable.is_none())
        || (has_subpath
            && path_mode.is_some_and(|pm| !matches!(pm, PathMode::Walk))
            && path_variable.is_none());
    let effective_path_var: Option<&str> = if needs_internal_path {
        Some(INTERNAL_PATH_VAR)
    } else {
        path_variable
    };

    let mut out = Vec::new();
    for seed in input_rows {
        let mut local_rows = Vec::new();

        // Bound-chain-target reverse anchor (checked FIRST to skip start_candidates
        // computation entirely).  When the start is unbound but the last chain's
        // target IS already bound in seed, reverse the traversal direction — walking
        // chains from last to first — to iterate only the target's edges.
        //
        // Works for single-hop (1 chain) and multi-hop (N chains, all Fixed(1)):
        //   OPTIONAL MATCH (liker:User)-[:Liked]->(p)                    -- 1-chain
        //   OPTIONAL MATCH (author:User)-[:Posted]->(page)-[:Contains]->(p)  -- 2-chain
        let start_is_bound = m
            .start
            .var
            .as_ref()
            .is_some_and(|v| matches!(seed.get(v), Some(Binding::Vertex(_))));
        let all_chains_fixed_1 = !m.elements.is_empty()
            && m.hops()
                .all(|c| matches!(c.edge.length, PathLength::Fixed(1)));
        let last_chain_target_bound = m
            .hops()
            .last()
            .and_then(|c| c.node.var.as_ref())
            .and_then(|v| seed.get(v))
            .is_some_and(|b| matches!(b, Binding::Vertex(_)));
        let used_reverse_chain_anchor =
            !shortest && !start_is_bound && all_chains_fixed_1 && last_chain_target_bound;

        if used_reverse_chain_anchor {
            local_rows = execute_reverse_chain_anchor(
                seed,
                m,
                graph,
                stats,
                where_clause,
                max_rows,
                limits,
            )?;
        } else {
            // Compute start candidates (only when reverse-chain anchor is not used).
            let start_candidates = if let Some(var) = &m.start.var {
                match seed.get(var) {
                    Some(Binding::Vertex(id)) => vec![*id],
                    _ => initial_candidates(&m.start, graph, stats, limits)?,
                }
            } else if let Some(Binding::Vertex(id)) = seed.get("__anon_start__") {
                // Anonymous start node with a synthetic binding from the index scan path.
                vec![*id]
            } else {
                initial_candidates(&m.start, graph, stats, limits)?
            };
            stats.breakdown.joined_match_start_candidates = stats
                .breakdown
                .joined_match_start_candidates
                .saturating_add(start_candidates.len() as u64);

            // Reverse-anchor optimization for SHORTEST single-chain outgoing.
            // When target candidates are significantly fewer than start candidates,
            // iterate targets with reverse BFS instead of starts with forward BFS.
            let used_reverse_anchor = if shortest
                && m.elements.len() == 1
                && matches!(m.chain(0).edge.direction, Direction::Outgoing)
                && !matches!(
                    shortest_mode,
                    Some(crate::ast::ShortestMode::All) | Some(crate::ast::ShortestMode::Group)
                )
                && start_candidates.len() > 1
            {
                let chain = m.chain(0);
                if let Some(pv) = effective_path_var
                    && chain.node.var.is_some()
                {
                    let target_cands = initial_candidates(&chain.node, graph, stats, limits)?;
                    if !target_cands.is_empty()
                        && target_cands.len() <= 128
                        && target_cands.len() * 4 < start_candidates.len()
                    {
                        local_rows = try_shortest_reversed_anchor(
                            &target_cands,
                            &start_candidates,
                            seed,
                            pv,
                            m,
                            chain,
                            graph,
                            stats,
                            where_clause,
                            max_rows,
                            limits,
                        )?;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if !used_reverse_anchor {
                for v in start_candidates {
                    let mut bindings = seed.clone();
                    if let Some(var) = &m.start.var {
                        bindings.insert(var.clone(), Binding::Vertex(v));
                    }
                    // GQL §16.7: evaluate inline WHERE on start node per-candidate.
                    if let Some(w) = m.start.where_clause.as_deref()
                        && !truthy(&eval_expr(w, &bindings, graph))
                    {
                        continue;
                    }
                    // Predicate pushdown: skip start vertices that fail conjuncts referencing only
                    // already-bound variables (seed + start-node variables).
                    if !eval_where_partial_pushdown(where_clause, &bindings, graph) {
                        continue;
                    }
                    let path_elems = vec![PathElement::Node(v)];
                    // For ALL SHORTEST and SHORTEST GROUP we need all paths (not just the BFS-first),
                    // so skip BFS and fall through to extend_match which enumerates all paths.
                    if shortest
                        && !matches!(
                            shortest_mode,
                            Some(crate::ast::ShortestMode::All)
                                | Some(crate::ast::ShortestMode::Group)
                        )
                        && let Some(rows_via_bfs) = try_shortest_via_bfs(
                            v,
                            &bindings,
                            &path_elems,
                            effective_path_var,
                            m,
                            graph,
                            stats,
                            where_clause,
                            max_rows,
                            limits,
                        )?
                    {
                        local_rows.extend(rows_via_bfs);
                        continue;
                    }
                    extend_match(
                        0,
                        v,
                        &bindings,
                        &path_elems,
                        effective_path_var,
                        m,
                        graph,
                        stats,
                        &mut local_rows,
                        where_clause,
                        max_rows,
                        limits,
                    )?;
                }
            }
        }
        if shortest && !local_rows.is_empty() {
            // §16.6: select based on shortest mode.
            local_rows = match shortest_mode {
                Some(crate::ast::ShortestMode::All) => {
                    select_all_shortest_rows(local_rows, effective_path_var)?
                }
                Some(crate::ast::ShortestMode::K(k)) => {
                    select_k_shortest_rows(local_rows, effective_path_var, k as usize)?
                }
                Some(crate::ast::ShortestMode::Group) => {
                    select_shortest_group_rows(local_rows, effective_path_var)?
                }
                _ => select_shortest_rows(local_rows, effective_path_var)?,
            };
        }
        // §16.6: Apply path mode filtering BEFORE stripping internal path var,
        // so that the full traversal path is visible for TRAIL/SIMPLE/ACYCLIC checks.
        if let Some(mode) = path_mode {
            local_rows.retain(|row| path_mode_allows(row, mode));
        }
        // Strip internal path variable from rows if we used it as a proxy.
        if needs_internal_path {
            for row in &mut local_rows {
                row.remove(INTERNAL_PATH_VAR);
            }
        }
        // Safety net: apply inline WHERE conditions from node/edge patterns post-hoc.
        // Retained for BFS-driven SHORTEST paths that bypass extend_hop and thus
        // may not have evaluated inline WHERE per-hop.
        stats.breakdown.joined_match_local_rows_before_inline_where = stats
            .breakdown
            .joined_match_local_rows_before_inline_where
            .saturating_add(local_rows.len() as u64);
        apply_pattern_inline_where(m, &mut local_rows, graph);
        stats.breakdown.joined_match_local_rows_after_inline_where = stats
            .breakdown
            .joined_match_local_rows_after_inline_where
            .saturating_add(local_rows.len() as u64);
        if local_rows.is_empty() {
            if optional {
                out.push(seed.clone());
            }
        } else {
            out.extend(local_rows);
        }
        if max_rows.is_some_and(|cap| out.len() >= cap) {
            out.truncate(max_rows.unwrap());
            break;
        }
    }
    Ok(out)
}

/// Checks if a row satisfies the given path mode constraint (§16.6).
///
/// Collects all vertex IDs (from `Binding::Vertex` values and `PathElement::Node` elements in
/// path variables) and edge keys (from `Binding::Edge` values and `PathElement::Edge` elements)
/// and applies the appropriate uniqueness check.
fn path_mode_allows(row: &Bindings, mode: PathMode) -> bool {
    use std::collections::HashSet;
    if matches!(mode, PathMode::Walk) {
        return true;
    }
    let mut vertex_ids: Vec<u32> = Vec::new();
    let mut edge_keys: Vec<(u32, u32, Option<String>)> = Vec::new();

    for binding in row.values() {
        match binding {
            Binding::Vertex(id) => vertex_ids.push(*id),
            Binding::Edge {
                src, dst, label, ..
            } => edge_keys.push((*src, *dst, label.as_deref().map(str::to_string))),
            Binding::Value(Value::Path(elements)) => {
                for elem in elements {
                    match elem {
                        PathElement::Node(id) => vertex_ids.push(*id),
                        PathElement::Edge { src, dst, label } => {
                            edge_keys.push((*src, *dst, label.clone()))
                        }
                    }
                }
            }
            _ => {}
        }
    }

    match mode {
        PathMode::Walk => true,
        PathMode::Trail => {
            // No repeated edges.
            let unique: HashSet<_> = edge_keys.iter().collect();
            unique.len() == edge_keys.len()
        }
        PathMode::Simple => {
            // No repeated vertices.
            let unique: HashSet<_> = vertex_ids.iter().collect();
            unique.len() == vertex_ids.len()
        }
        PathMode::Acyclic => {
            // No repeated vertices AND start vertex ≠ end vertex (no cycle).
            let unique: HashSet<_> = vertex_ids.iter().collect();
            if unique.len() != vertex_ids.len() {
                return false;
            }
            // If we have at least two vertices, first and last must differ.
            if vertex_ids.len() >= 2 {
                let first = vertex_ids.first().unwrap();
                let last = vertex_ids.last().unwrap();
                first != last
            } else {
                true
            }
        }
    }
}

/// §16.4: Returns `true` if no edge appears more than once across all bindings in a row.
///
/// Used for `MATCH DIFFERENT EDGES` mode: each edge (src, dst, label) must be unique.
fn different_edges_allows(row: &Bindings) -> bool {
    use std::collections::HashSet;
    let mut edge_keys: Vec<(u32, u32, Option<String>)> = Vec::new();
    for binding in row.values() {
        match binding {
            Binding::Edge {
                src, dst, label, ..
            } => edge_keys.push((*src, *dst, label.as_deref().map(str::to_string))),
            Binding::Value(Value::Path(elements)) => {
                for elem in elements {
                    if let PathElement::Edge { src, dst, label } = elem {
                        edge_keys.push((*src, *dst, label.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    let unique: HashSet<_> = edge_keys.iter().collect();
    unique.len() == edge_keys.len()
}

fn execute_match_clause<M: Memory>(
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<Vec<Bindings>, GleaphError> {
    let _compiled_where_guard = push_compiled_where_scope(where_clause);
    let start_candidates = initial_candidates(&m.start, graph, stats, limits)?;
    let mut rows = Vec::new();
    for v in start_candidates {
        if max_rows.is_some_and(|cap| rows.len() >= cap) {
            break;
        }
        let mut bindings = Bindings::new();
        if let Some(var) = &m.start.var {
            bindings.insert(var.clone(), Binding::Vertex(v));
        }
        // GQL §16.7: evaluate inline WHERE on start node per-candidate.
        if let Some(w) = m.start.where_clause.as_deref()
            && !truthy(&eval_expr(w, &bindings, graph))
        {
            continue;
        }
        // Predicate pushdown: skip start vertices that fail conjuncts referencing only
        // start-node variables (no expansion cost incurred for mismatches).
        if !eval_where_partial_pushdown(where_clause, &bindings, graph) {
            continue;
        }
        let path_elems = vec![PathElement::Node(v)];
        extend_match(
            0,
            v,
            &bindings,
            &path_elems,
            None,
            m,
            graph,
            stats,
            &mut rows,
            where_clause,
            max_rows,
            limits,
        )?;
    }
    // Post-hoc inline WHERE removed: inline WHERE is now evaluated per-candidate
    // and per-hop in extend_hop / traverse_var_len. The safety net in
    // execute_match_clause_joined is retained for BFS-driven SHORTEST paths.
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
fn try_shortest_via_bfs<M: Memory>(
    start_vertex: u32,
    seed_bindings: &Bindings,
    seed_path: &[PathElement],
    path_variable: Option<&str>,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<Option<Vec<Bindings>>, GleaphError> {
    let Some(path_var) = path_variable else {
        return Ok(None);
    };
    if m.elements.is_empty() {
        return Ok(None);
    }
    // §16.6: multi-chain SHORTEST — chained BFS across all pattern segments.
    if m.elements.len() > 1 {
        return try_shortest_via_bfs_multi_chain(
            start_vertex,
            seed_bindings,
            seed_path,
            path_var,
            m,
            graph,
            stats,
            where_clause,
            max_rows,
            limits,
        );
    }
    let chain = m.chain(0);
    let Some(node_var) = &chain.node.var else {
        return Ok(None);
    };
    let (min_hops, max_hops) = match chain.edge.length {
        PathLength::Fixed(n) => (n, n),
        PathLength::Range { min, max } => (min, max),
    };
    if max_hops == 0 {
        return Ok(None);
    }

    match chain.edge.direction {
        Direction::Outgoing => try_shortest_via_bfs_on_view(
            graph,
            start_vertex,
            seed_bindings,
            seed_path,
            path_var,
            node_var,
            min_hops,
            max_hops,
            m,
            chain,
            graph,
            stats,
            where_clause,
            max_rows,
            limits,
        ),
        Direction::Either => {
            // Undirected shortest: try outgoing first, then incoming; return first hit.
            let out_result = try_shortest_via_bfs_on_view(
                graph,
                start_vertex,
                seed_bindings,
                seed_path,
                path_var,
                node_var,
                min_hops,
                max_hops,
                m,
                chain,
                graph,
                stats,
                where_clause,
                max_rows,
                limits,
            )?;
            if out_result.is_some() {
                return Ok(out_result);
            }
            let reverse_view = ReverseView(graph);
            try_shortest_via_bfs_on_view(
                &reverse_view,
                start_vertex,
                seed_bindings,
                seed_path,
                path_var,
                node_var,
                min_hops,
                max_hops,
                m,
                chain,
                graph,
                stats,
                where_clause,
                max_rows,
                limits,
            )
        }
        Direction::Incoming => {
            let reverse_view = ReverseView(graph);
            try_shortest_via_bfs_on_view(
                &reverse_view,
                start_vertex,
                seed_bindings,
                seed_path,
                path_var,
                node_var,
                min_hops,
                max_hops,
                m,
                chain,
                graph,
                stats,
                where_clause,
                max_rows,
                limits,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn try_shortest_via_bfs_on_view<Gv: GraphView, M: Memory>(
    gv: &Gv,
    start_vertex: u32,
    seed_bindings: &Bindings,
    seed_path: &[PathElement],
    path_var: &str,
    node_var: &str,
    min_hops: u32,
    max_hops: u32,
    m: &MatchClause,
    chain: &crate::ast::MatchChain,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<Option<Vec<Bindings>>, GleaphError> {
    let ts_range = extract_edge_ts_range(where_clause, chain.edge.var.as_deref());
    let prebound_target = match seed_bindings.get(node_var) {
        Some(Binding::Vertex(v)) => Some(*v),
        Some(Binding::Value(_)) | Some(Binding::Edge { .. }) => return Ok(Some(Vec::new())),
        None => None,
    };

    let mut candidate_targets: Vec<u32> = if let Some(t) = prebound_target {
        vec![t]
    } else {
        initial_candidates(&chain.node, graph, stats, limits)?
    };
    if candidate_targets.is_empty() {
        return Ok(Some(Vec::new()));
    }

    if prebound_target.is_none() {
        // When the target pattern has inline property constraints (e.g. {id: 5})
        // and resolves to a small candidate set, skip the untargeted BFS pre-filter.
        // Per-target BFS with early termination is more efficient and avoids missing
        // candidates that fall outside the untargeted BFS's max_visited budget.
        let target_has_props = !chain.node.props_hint.is_empty();
        let skip_prefilter = target_has_props && candidate_targets.len() <= 128;

        if !skip_prefilter {
            let mut budget = CountingBudget::new(500_000);
            let bfs_all = bfs(
                gv,
                start_vertex,
                &BfsConfig {
                    max_depth: Some(max_hops),
                    max_visited: Some(10_000),
                    target: None,
                    edge_label: chain.edge.label.clone(),
                    edge_label_expr: chain.edge.label_expr.clone(),
                    ts_range: ts_range.clone(),
                },
                &mut budget,
            )?;
            stats.execution_steps = stats.execution_steps.saturating_add(budget.used);
            let dist_map = bfs_all.distances.into_iter().collect::<BTreeMap<_, _>>();
            candidate_targets.retain(|t| {
                dist_map
                    .get(t)
                    .is_some_and(|d| *d >= min_hops && *d <= max_hops)
            });
            candidate_targets.sort_by_key(|t| dist_map.get(t).copied().unwrap_or(u32::MAX));
        }
    }

    // When both endpoints are resolved (target has inline props, small candidate
    // set), try bidirectional BFS — explores from both ends simultaneously for O(b^(d/2)).
    let target_has_props = !chain.node.props_hint.is_empty();
    if target_has_props && candidate_targets.len() <= 128 {
        let mut budget = CountingBudget::new(500_000);
        let bfs_result = bfs_bidirectional(
            gv,
            start_vertex,
            &candidate_targets,
            &BfsConfig {
                max_depth: Some(max_hops),
                max_visited: Some(10_000),
                target: None, // ignored by bfs_bidirectional, targets passed explicitly
                edge_label: chain.edge.label.clone(),
                edge_label_expr: chain.edge.label_expr.clone(),
                ts_range: ts_range.clone(),
            },
            &mut budget,
        );
        stats.execution_steps = stats.execution_steps.saturating_add(budget.used);

        if let Ok(br) = bfs_result
            && let Some(vertices) = br.path
        {
            let hops = vertices.len().saturating_sub(1) as u32;
            if vertices.len() >= 2 && hops >= min_hops && hops <= max_hops {
                let target = *vertices.last().unwrap();
                if node_matches(&chain.node, target, graph)
                    && let Some(out) = process_shortest_path_result(
                        &vertices,
                        target,
                        seed_bindings,
                        seed_path,
                        path_var,
                        node_var,
                        chain,
                        m,
                        graph,
                        stats,
                        where_clause,
                        max_rows,
                        limits,
                    )?
                    && !out.is_empty()
                {
                    return Ok(Some(out));
                }
            }
        }
        // Fall through to per-target BFS if bidirectional didn't produce results
        // (e.g., budget exhaustion or WHERE clause filtered out the result).
    }

    for target in candidate_targets {
        let mut budget = CountingBudget::new(500_000);
        let bfs_result = bfs(
            gv,
            start_vertex,
            &BfsConfig {
                max_depth: Some(max_hops),
                max_visited: Some(10_000),
                target: Some(target),
                edge_label: chain.edge.label.clone(),
                edge_label_expr: chain.edge.label_expr.clone(),
                ts_range: ts_range.clone(),
            },
            &mut budget,
        )?;
        stats.execution_steps = stats.execution_steps.saturating_add(budget.used);
        let Some(vertices) = bfs_result.path else {
            continue;
        };
        if vertices.len() < 2 {
            continue;
        }
        let hops = (vertices.len() - 1) as u32;
        if hops < min_hops || hops > max_hops {
            continue;
        }
        if !node_matches(&chain.node, target, graph) {
            continue;
        }

        if let Some(out) = process_shortest_path_result(
            &vertices,
            target,
            seed_bindings,
            seed_path,
            path_var,
            node_var,
            chain,
            m,
            graph,
            stats,
            where_clause,
            max_rows,
            limits,
        )? && !out.is_empty()
        {
            return Ok(Some(out));
        }
    }
    Ok(Some(Vec::new()))
}

/// Shared path-processing logic for SHORTEST results.
///
/// Given a BFS-produced vertex path, builds bindings, path elements, and runs
/// `extend_match` (WHERE evaluation). Returns `Ok(Some(rows))` on success.
#[allow(clippy::too_many_arguments)]
fn process_shortest_path_result<M: Memory>(
    vertices: &[u32],
    target: u32,
    seed_bindings: &Bindings,
    seed_path: &[PathElement],
    path_var: &str,
    node_var: &str,
    chain: &crate::ast::MatchChain,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<Option<Vec<Bindings>>, GleaphError> {
    let mut next_bindings = seed_bindings.clone();
    if let Some(edge_var) = &chain.edge.var {
        let (src, dst) = match chain.edge.direction {
            Direction::Outgoing | Direction::Either => {
                let src = vertices[vertices.len() - 2];
                let dst = *vertices.last().expect("path has at least 2 vertices");
                (src, dst)
            }
            Direction::Incoming => {
                let dst = vertices[vertices.len() - 2];
                let src = *vertices.last().expect("path has at least 2 vertices");
                (src, dst)
            }
        };
        let neighbors = graph.collect_neighbors(src).unwrap_or_default();
        let payload = neighbors.iter().find(|e| e.target == dst);
        let label = payload
            .and_then(|e| graph.label_name_by_id(e.label_id()))
            .map(Arc::from);
        next_bindings.insert(
            edge_var.clone(),
            Binding::Edge {
                src,
                dst,
                label,
                edge_id: payload.map_or(0, |e| e.edge_id),
                weight: payload.map_or(0.0, |e| e.weight),
                timestamp: payload.map_or(0, |e| e.timestamp),
            },
        );
    }
    next_bindings.insert(node_var.to_string(), Binding::Vertex(target));

    let mut path_elems = seed_path.to_vec();
    for pair in vertices.windows(2) {
        let (src, dst) = match chain.edge.direction {
            Direction::Outgoing | Direction::Either => (pair[0], pair[1]),
            Direction::Incoming => (pair[1], pair[0]),
        };
        path_elems.push(PathElement::Edge {
            src,
            dst,
            label: graph.edge_label(src, dst),
        });
        path_elems.push(PathElement::Node(pair[1]));
    }
    let mut out = Vec::new();
    extend_match(
        1,
        target,
        &next_bindings,
        &path_elems,
        Some(path_var),
        m,
        graph,
        stats,
        &mut out,
        where_clause,
        max_rows,
        limits,
    )?;
    Ok(Some(out))
}

/// Reverse-anchor SHORTEST for single-chain outgoing patterns.
///
/// When target candidates are significantly fewer than start candidates, iterate
/// target candidates with reverse BFS instead of start candidates with forward BFS.
/// For each target, runs untargeted reverse BFS to discover reachable start vertices,
/// then runs targeted reverse BFS to reconstruct the shortest path.
#[allow(clippy::too_many_arguments)]
fn try_shortest_reversed_anchor<M: Memory>(
    target_candidates: &[u32],
    start_candidates: &[u32],
    seed: &Bindings,
    path_var: &str,
    m: &MatchClause,
    chain: &crate::ast::MatchChain,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<Vec<Bindings>, GleaphError> {
    let node_var = match &chain.node.var {
        Some(v) => v.as_str(),
        None => return Ok(Vec::new()),
    };
    let (min_hops, max_hops) = match chain.edge.length {
        PathLength::Fixed(n) => (n, n),
        PathLength::Range { min, max } => (min, max),
    };
    if max_hops == 0 {
        return Ok(Vec::new());
    }
    let ts_range = extract_edge_ts_range(where_clause, chain.edge.var.as_deref());
    let edge_label = chain.edge.label.clone();
    let edge_label_expr = chain.edge.label_expr.clone();
    let start_set: VertexIdSet = start_candidates.iter().copied().collect();
    let reverse_view = ReverseView(graph);

    for &target in target_candidates {
        if !graph.is_vertex_active(target) {
            continue;
        }
        // Step 1: Untargeted reverse BFS from target to discover reachable starts.
        let mut budget = CountingBudget::new(500_000);
        let bfs_discovery = bfs(
            &reverse_view,
            target,
            &BfsConfig {
                max_depth: Some(max_hops),
                max_visited: Some(10_000),
                target: None,
                edge_label: edge_label.clone(),
                edge_label_expr: edge_label_expr.clone(),
                ts_range: ts_range.clone(),
            },
            &mut budget,
        )?;
        stats.execution_steps = stats.execution_steps.saturating_add(budget.used);

        // Find nearest start candidate reachable via reverse BFS.
        let dist_map: BTreeMap<u32, u32> = bfs_discovery.distances.into_iter().collect();
        let nearest_start = start_set
            .iter()
            .filter_map(|s| dist_map.get(&s).map(|d| (s, *d)))
            .filter(|(s, d)| *d >= min_hops && *d <= max_hops && node_matches(&m.start, *s, graph))
            .min_by_key(|(_, d)| *d);

        let Some((start, _hops)) = nearest_start else {
            continue;
        };

        // Step 2: Targeted reverse BFS from target to nearest start → get path.
        let mut budget2 = CountingBudget::new(500_000);
        let bfs_path = bfs(
            &reverse_view,
            target,
            &BfsConfig {
                max_depth: Some(max_hops),
                max_visited: Some(10_000),
                target: Some(start),
                edge_label: edge_label.clone(),
                edge_label_expr: edge_label_expr.clone(),
                ts_range: ts_range.clone(),
            },
            &mut budget2,
        )?;
        stats.execution_steps = stats.execution_steps.saturating_add(budget2.used);

        let Some(reverse_vertices) = bfs_path.path else {
            continue;
        };
        // reverse_vertices is [target, ..., start] in BFS order.
        // Forward path is [start, ..., target] — reverse it.
        let vertices: Vec<u32> = reverse_vertices.into_iter().rev().collect();
        if vertices.len() < 2 {
            continue;
        }
        let hops = (vertices.len() - 1) as u32;
        if hops < min_hops || hops > max_hops {
            continue;
        }

        // Build bindings — mirror the forward BFS path construction.
        let mut next_bindings = seed.clone();
        if let Some(start_var) = &m.start.var {
            next_bindings.insert(start_var.clone(), Binding::Vertex(start));
        }
        next_bindings.insert(node_var.to_string(), Binding::Vertex(target));

        if let Some(edge_var) = &chain.edge.var {
            let src = vertices[vertices.len() - 2];
            let dst = *vertices.last().expect("path has at least 2 vertices");
            let neighbors = graph.collect_neighbors(src).unwrap_or_default();
            let payload = neighbors.iter().find(|e| e.target == dst);
            let label = payload
                .and_then(|e| graph.label_name_by_id(e.label_id()))
                .map(Arc::from);
            next_bindings.insert(
                edge_var.clone(),
                Binding::Edge {
                    src,
                    dst,
                    label,
                    edge_id: payload.map_or(0, |e| e.edge_id),
                    weight: payload.map_or(0.0, |e| e.weight),
                    timestamp: payload.map_or(0, |e| e.timestamp),
                },
            );
        }

        // Build path elements in forward order.
        let mut path_elems = vec![PathElement::Node(vertices[0])];
        for pair in vertices.windows(2) {
            let (src, dst) = (pair[0], pair[1]);
            path_elems.push(PathElement::Edge {
                src,
                dst,
                label: graph.edge_label(src, dst),
            });
            path_elems.push(PathElement::Node(pair[1]));
        }

        // Use extend_match for remaining chains (none for single-chain) + WHERE evaluation.
        let mut out = Vec::new();
        extend_match(
            1,
            target,
            &next_bindings,
            &path_elems,
            Some(path_var),
            m,
            graph,
            stats,
            &mut out,
            where_clause,
            max_rows,
            limits,
        )?;
        if !out.is_empty() {
            return Ok(out);
        }
    }
    Ok(Vec::new())
}

/// §16.6: Chained BFS for multi-chain SHORTEST patterns.
///
/// Processes each chain in `m.elements` by BFS from the previous endpoint,
/// collecting all complete (total_edge_hops, bindings, path_elems) combinations,
/// and returning the one with minimum total hops.
#[allow(clippy::too_many_arguments)]
fn try_shortest_via_bfs_multi_chain<M: Memory>(
    start_vertex: u32,
    seed_bindings: &Bindings,
    seed_path: &[PathElement],
    path_var: &str,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<Option<Vec<Bindings>>, GleaphError> {
    let paths = collect_multi_chain_paths(
        start_vertex,
        seed_bindings,
        seed_path,
        0,
        m,
        graph,
        stats,
        where_clause,
        limits,
    )?;
    if paths.is_empty() {
        return Ok(Some(Vec::new()));
    }
    // Build candidate rows, attach path variable, filter by WHERE, then pick shortest.
    // We must apply WHERE before selecting the minimum because the WHERE clause may
    // reference end-node variables that differ across paths (e.g. `c.name = 'Carol'`).
    let mut best: Option<(usize, Bindings)> = None;
    for (hops, bindings, path) in paths {
        let mut row = bindings;
        row.insert(path_var.to_string(), Binding::Value(Value::Path(path)));
        if where_clause.is_none_or(|w| eval_where(w, &row, graph)) {
            match &best {
                Some((best_hops, _)) if hops >= *best_hops => {}
                _ => {
                    best = Some((hops, row));
                }
            }
        }
    }
    let Some((_, row)) = best else {
        return Ok(Some(Vec::new()));
    };
    if max_rows.is_some_and(|cap| cap == 0) {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(vec![row]))
}

/// Recursively collects all (total_edge_hops, bindings, path_elems) for paths through
/// `m.elements[chain_idx..]` starting from `current_vertex`.
#[allow(clippy::too_many_arguments)]
fn collect_multi_chain_paths<M: Memory>(
    current_vertex: u32,
    current_bindings: &Bindings,
    current_path: &[PathElement],
    chain_idx: usize,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    limits: ExecutionLimits,
) -> Result<Vec<(usize, Bindings, Vec<PathElement>)>, GleaphError> {
    bump_steps(stats, 1, limits)?;
    if chain_idx >= m.elements.len() {
        // All chains processed — count edge hops in path.
        let edge_hops = current_path
            .iter()
            .filter(|e| matches!(e, PathElement::Edge { .. }))
            .count();
        return Ok(vec![(
            edge_hops,
            current_bindings.clone(),
            current_path.to_vec(),
        )]);
    }

    let chain = m.chain(chain_idx);
    let (min_hops, max_hops) = match chain.edge.length {
        PathLength::Fixed(n) => (n, n),
        PathLength::Range { min, max } => (min, max),
    };
    let ts_range = extract_edge_ts_range(where_clause, chain.edge.var.as_deref());

    // BFS from current_vertex to discover all reachable vertices within hop range.
    let mut budget = CountingBudget::new(500_000);
    let bfs_cfg = BfsConfig {
        max_depth: Some(max_hops),
        max_visited: Some(10_000),
        target: None,
        edge_label: chain.edge.label.clone(),
        edge_label_expr: chain.edge.label_expr.clone(),
        ts_range: ts_range.clone(),
    };
    let bfs_all = match chain.edge.direction {
        Direction::Outgoing => bfs(graph, current_vertex, &bfs_cfg, &mut budget)?,
        Direction::Incoming => {
            let rev = ReverseView(graph);
            bfs(&rev, current_vertex, &bfs_cfg, &mut budget)?
        }
        Direction::Either => {
            let both = BothWaysView(graph);
            bfs(&both, current_vertex, &bfs_cfg, &mut budget)?
        }
    };
    stats.execution_steps = stats.execution_steps.saturating_add(budget.used);

    let dist_map: BTreeMap<u32, u32> = bfs_all.distances.into_iter().collect();
    // Intermediate candidates: vertices within hop range that match the chain's node pattern.
    let candidates: Vec<(u32, u32)> = dist_map
        .iter()
        .filter(|(v, d)| {
            **d >= min_hops && **d <= max_hops && node_matches(&chain.node, **v, graph)
        })
        .map(|(v, d)| (*v, *d))
        .collect();

    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let mut all_paths = Vec::new();
    for (target_v, _dist) in candidates {
        // Find the actual BFS path from current_vertex to target_v.
        let mut path_budget = CountingBudget::new(500_000);
        let target_bfs_cfg = BfsConfig {
            max_depth: Some(max_hops),
            max_visited: Some(10_000),
            target: Some(target_v),
            edge_label: chain.edge.label.clone(),
            edge_label_expr: chain.edge.label_expr.clone(),
            ts_range: ts_range.clone(),
        };
        let path_result = match chain.edge.direction {
            Direction::Outgoing => bfs(graph, current_vertex, &target_bfs_cfg, &mut path_budget)?,
            Direction::Incoming => {
                let rev = ReverseView(graph);
                bfs(&rev, current_vertex, &target_bfs_cfg, &mut path_budget)?
            }
            Direction::Either => {
                let both = BothWaysView(graph);
                bfs(&both, current_vertex, &target_bfs_cfg, &mut path_budget)?
            }
        };
        stats.execution_steps = stats.execution_steps.saturating_add(path_budget.used);

        let Some(vertices) = path_result.path else {
            continue;
        };
        if vertices.len() < 2 {
            continue;
        }

        // Build bindings and path elements for this hop segment.
        let mut next_bindings = current_bindings.clone();
        if let Some(node_var) = &chain.node.var {
            next_bindings.insert(node_var.clone(), Binding::Vertex(target_v));
        }
        if let Some(edge_var) = &chain.edge.var {
            let (src, dst) = match chain.edge.direction {
                Direction::Outgoing | Direction::Either => {
                    let src = vertices[vertices.len() - 2];
                    let dst = *vertices.last().expect("non-empty path");
                    (src, dst)
                }
                Direction::Incoming => {
                    let dst = vertices[vertices.len() - 2];
                    let src = *vertices.last().expect("non-empty path");
                    (src, dst)
                }
            };
            let neighbors = graph.collect_neighbors(src).unwrap_or_default();
            let payload = neighbors.iter().find(|e| e.target == dst);
            let label = payload
                .and_then(|e| graph.label_name_by_id(e.label_id()))
                .map(Arc::from);
            next_bindings.insert(
                edge_var.clone(),
                Binding::Edge {
                    src,
                    dst,
                    label,
                    edge_id: payload.map_or(0, |e| e.edge_id),
                    weight: payload.map_or(0.0, |e| e.weight),
                    timestamp: payload.map_or(0, |e| e.timestamp),
                },
            );
        }
        let mut next_path = current_path.to_vec();
        for pair in vertices.windows(2) {
            let (src, dst) = match chain.edge.direction {
                Direction::Outgoing | Direction::Either => (pair[0], pair[1]),
                Direction::Incoming => (pair[1], pair[0]),
            };
            next_path.push(PathElement::Edge {
                src,
                dst,
                label: graph.edge_label(src, dst),
            });
            next_path.push(PathElement::Node(pair[1]));
        }

        // Recurse for the remaining chains.
        let sub_paths = collect_multi_chain_paths(
            target_v,
            &next_bindings,
            &next_path,
            chain_idx + 1,
            m,
            graph,
            stats,
            where_clause,
            limits,
        )?;
        all_paths.extend(sub_paths);
    }
    Ok(all_paths)
}

fn select_shortest_rows(
    rows: Vec<Bindings>,
    path_variable: Option<&str>,
) -> Result<Vec<Bindings>, GleaphError> {
    let path_var = path_variable.expect("internal: shortest always has path var");
    let mut best_len: Option<usize> = None;
    let mut best_row: Option<Bindings> = None;
    for row in rows {
        let len = match row.get(path_var) {
            Some(Binding::Value(Value::Path(p))) => p
                .iter()
                .filter(|e| matches!(e, PathElement::Edge { .. }))
                .count(),
            _ => continue,
        };
        if best_len.is_none_or(|cur| len < cur) {
            best_len = Some(len);
            best_row = Some(row);
        }
    }
    Ok(best_row.into_iter().collect())
}

/// §16.6: ALL SHORTEST — return all rows whose path length equals the minimum.
fn select_all_shortest_rows(
    rows: Vec<Bindings>,
    path_variable: Option<&str>,
) -> Result<Vec<Bindings>, GleaphError> {
    let path_var = path_variable.expect("internal: shortest always has path var");
    let path_len = |row: &Bindings| match row.get(path_var) {
        Some(Binding::Value(Value::Path(p))) => Some(
            p.iter()
                .filter(|e| matches!(e, PathElement::Edge { .. }))
                .count(),
        ),
        _ => None,
    };
    let best_len = rows.iter().filter_map(path_len).min();
    let Some(min_len) = best_len else {
        return Ok(Vec::new());
    };
    Ok(rows
        .into_iter()
        .filter(|row| path_len(row) == Some(min_len))
        .collect())
}

/// §16.6: SHORTEST k N — return the N shortest paths in ascending order of length.
fn select_k_shortest_rows(
    mut rows: Vec<Bindings>,
    path_variable: Option<&str>,
    k: usize,
) -> Result<Vec<Bindings>, GleaphError> {
    let path_var = path_variable.expect("internal: shortest always has path var");
    let path_len = |row: &Bindings| match row.get(path_var) {
        Some(Binding::Value(Value::Path(p))) => p
            .iter()
            .filter(|e| matches!(e, PathElement::Edge { .. }))
            .count(),
        _ => usize::MAX,
    };
    rows.sort_by_key(|r| path_len(r));
    rows.truncate(k);
    Ok(rows)
}

/// SHORTEST GROUP — for each (source, destination) endpoint pair keep only the shortest path.
fn select_shortest_group_rows(
    rows: Vec<Bindings>,
    path_variable: Option<&str>,
) -> Result<Vec<Bindings>, GleaphError> {
    let path_var = path_variable.expect("internal: shortest always has path var");
    let path_endpoints = |row: &Bindings| -> Option<(u32, u32, usize)> {
        match row.get(path_var) {
            Some(Binding::Value(Value::Path(p))) => {
                let nodes: Vec<u32> = p
                    .iter()
                    .filter_map(|e| {
                        if let PathElement::Node(id) = e {
                            Some(*id)
                        } else {
                            None
                        }
                    })
                    .collect();
                let first = *nodes.first()?;
                let last = *nodes.last()?;
                let edge_count = p
                    .iter()
                    .filter(|e| matches!(e, PathElement::Edge { .. }))
                    .count();
                Some((first, last, edge_count))
            }
            _ => None,
        }
    };
    // Group by (start_node, end_node); keep the row with the minimum edge count.
    let mut best: BTreeMap<(u32, u32), (usize, Bindings)> = BTreeMap::new();
    for row in rows {
        if let Some((start, end, len)) = path_endpoints(&row) {
            let key = (start, end);
            match best.get(&key) {
                Some((best_len, _)) if len >= *best_len => {}
                _ => {
                    best.insert(key, (len, row));
                }
            }
        }
    }
    Ok(best.into_values().map(|(_, row)| row).collect())
}

#[allow(clippy::too_many_arguments)]
fn extend_match<M: Memory>(
    hop_idx: usize,
    current_vertex: u32,
    current_bindings: &Bindings,
    current_path: &[PathElement],
    path_variable: Option<&str>,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    out: &mut Vec<Bindings>,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<(), GleaphError> {
    if max_rows.is_some_and(|cap| out.len() >= cap) {
        return Ok(());
    }
    if hop_idx >= m.elements.len() {
        bump_steps(stats, 1, limits)?;
        if where_clause.is_none_or(|w| eval_where(w, current_bindings, graph)) {
            if let Some(cap) = limits.max_rows
                && out.len() >= cap
            {
                return Err(GleaphError::ExecutionError(format!(
                    "result row count {} exceeds default cap {}",
                    out.len() + 1,
                    cap
                )));
            }
            let mut row = current_bindings.clone();
            if let Some(path_var) = path_variable {
                row.insert(
                    path_var.to_string(),
                    Binding::Value(Value::Path(current_path.to_vec())),
                );
            }
            out.push(row);
        }
        return Ok(());
    }

    match &m.elements[hop_idx] {
        PatternElement::Hop(chain) => match &chain.edge.length {
            PathLength::Fixed(1) => extend_hop(
                hop_idx,
                current_vertex,
                current_bindings,
                current_path,
                path_variable,
                m,
                graph,
                stats,
                out,
                where_clause,
                max_rows,
                limits,
            )?,
            PathLength::Fixed(n) => extend_var_len(
                hop_idx,
                current_vertex,
                current_bindings,
                current_path,
                path_variable,
                *n,
                *n,
                m,
                graph,
                stats,
                out,
                where_clause,
                max_rows,
                limits,
            )?,
            PathLength::Range { min, max } => extend_var_len(
                hop_idx,
                current_vertex,
                current_bindings,
                current_path,
                path_variable,
                *min,
                *max,
                m,
                graph,
                stats,
                out,
                where_clause,
                max_rows,
                limits,
            )?,
        },
        PatternElement::SubPath {
            inner_start,
            inner_elements,
            quantifier,
            var,
            trailing_node,
        } => {
            let (rep_min, rep_max) = match quantifier {
                PathLength::Fixed(n) => (*n, *n),
                PathLength::Range { min, max } => (*min, *max),
            };
            // Build a temporary MatchClause for the inner subpath pattern.
            let inner_clause = MatchClause {
                start: inner_start.clone(),
                elements: inner_elements.clone(),
            };
            // Expand subpath: for each repetition count k in [rep_min, rep_max],
            // apply the inner pattern k times sequentially.
            extend_subpath(
                hop_idx,
                current_vertex,
                current_bindings,
                current_path,
                path_variable,
                &inner_clause,
                rep_min,
                rep_max,
                var.as_deref(),
                trailing_node.as_ref(),
                m,
                graph,
                stats,
                out,
                where_clause,
                max_rows,
                limits,
            )?;
        }
    }
    Ok(())
}

/// Expand a parenthesized subpath pattern by repeating the inner pattern `rep_min..=rep_max`
/// times. Each repetition feeds the output of the previous one as input.
///
/// **Complexity warning**: O(fan_out^(hops × rep_max)). Keep quantifier ranges small.
#[allow(clippy::too_many_arguments)]
fn extend_subpath<M: Memory>(
    hop_idx: usize,
    current_vertex: u32,
    current_bindings: &Bindings,
    current_path: &[PathElement],
    path_variable: Option<&str>,
    inner_clause: &MatchClause,
    rep_min: u32,
    rep_max: u32,
    _subpath_var: Option<&str>,
    trailing_node: Option<&NodePattern>,
    outer_clause: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    out: &mut Vec<Bindings>,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<(), GleaphError> {
    // Internal path variable used to accumulate path elements across subpath repetitions.
    // This enables path mode filtering (TRAIL/SIMPLE/ACYCLIC) across repetition boundaries.
    const SUBPATH_INTERNAL_PATH: &str = "__subpath_path__";

    // Frontier: (bindings, last_vertex, accumulated_path)
    let mut frontier: Vec<(Bindings, u32, Vec<PathElement>)> = vec![(
        current_bindings.clone(),
        current_vertex,
        current_path.to_vec(),
    )];

    for k in 1..=rep_max {
        if max_rows.is_some_and(|cap| out.len() >= cap) {
            break;
        }
        let mut next_frontier = Vec::new();
        for (bindings, vtx, path) in &frontier {
            bump_steps(stats, 1, limits)?;
            // Run the inner pattern once from this frontier entry.
            let mut inner_results = Vec::new();
            // Match inner_clause.start node against vtx.
            if !node_matches_pattern(vtx, &inner_clause.start, bindings, graph) {
                continue;
            }
            let mut inner_bindings = bindings.clone();
            if let Some(var) = &inner_clause.start.var {
                inner_bindings.insert(var.clone(), Binding::Vertex(*vtx));
            }
            if inner_clause.elements.is_empty() {
                // Subpath with no hops — just the start node (degenerate case).
                inner_results.push(inner_bindings);
            } else {
                // Use the internal path variable to accumulate path elements
                // through each repetition — this enables path mode checks.
                extend_match(
                    0,
                    *vtx,
                    &inner_bindings,
                    path,
                    Some(SUBPATH_INTERNAL_PATH),
                    inner_clause,
                    graph,
                    stats,
                    &mut inner_results,
                    None, // inner WHERE handled by extend_match
                    max_rows,
                    limits,
                )?;
            }
            for row in inner_results {
                // Find the last vertex from the inner pattern's last hop.
                let last_vtx = find_last_vertex_in_clause(inner_clause, &row).unwrap_or(*vtx);
                // Extract accumulated path from the internal path variable.
                let accumulated_path =
                    if let Some(Binding::Value(Value::Path(p))) = row.get(SUBPATH_INTERNAL_PATH) {
                        p.clone()
                    } else {
                        path.clone()
                    };
                next_frontier.push((row, last_vtx, accumulated_path));
            }
        }
        frontier = next_frontier;
        if frontier.is_empty() {
            break;
        }
        // For k >= rep_min, emit results.
        if k >= rep_min {
            for (bindings, vtx, path) in &frontier {
                // If there's a trailing node pattern, check it matches the endpoint.
                if let Some(tn) = trailing_node
                    && !node_matches_pattern(vtx, tn, bindings, graph)
                {
                    continue;
                }
                // Bind trailing node variable if present.
                let mut bindings = bindings.clone();
                if let Some(tn) = trailing_node
                    && let Some(var) = &tn.var
                {
                    bindings.insert(var.clone(), Binding::Vertex(*vtx));
                }
                // Remove the internal path var — it's not user-visible.
                bindings.remove(SUBPATH_INTERNAL_PATH);
                extend_match(
                    hop_idx + 1,
                    *vtx,
                    &bindings,
                    path,
                    path_variable,
                    outer_clause,
                    graph,
                    stats,
                    out,
                    where_clause,
                    max_rows,
                    limits,
                )?;
            }
        }
    }
    Ok(())
}

/// Check if a vertex matches a node pattern's labels and inline properties.
fn node_matches_pattern<M: Memory>(
    vtx: &u32,
    pattern: &NodePattern,
    _bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> bool {
    // Check label constraints.
    for label in &pattern.labels {
        if !graph.vertex_has_label(*vtx, label) {
            return false;
        }
    }
    // Check inline property hints.
    for (prop, expected) in &pattern.props_hint {
        let actual = graph.get_single_vertex_property(*vtx, prop);
        let expected_val = match expected {
            Expr::Literal(v) => v,
            _ => return true, // non-literal: skip
        };
        match actual {
            Some(v) if v == *expected_val => {}
            _ => return false,
        }
    }
    true
}

/// Find the last vertex in a match clause's results by looking up the last hop's node variable.
fn find_last_vertex_in_clause(clause: &MatchClause, bindings: &Bindings) -> Option<u32> {
    // Find the last Hop element's node variable.
    for elem in clause.elements.iter().rev() {
        if let PatternElement::Hop(chain) = elem
            && let Some(var) = &chain.node.var
            && let Some(Binding::Vertex(v)) = bindings.get(var)
        {
            return Some(*v);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn recurse_extend_match_with_optional_path<M: Memory>(
    next_hop_idx: usize,
    next_vertex: u32,
    next_bindings: &Bindings,
    current_path: &[PathElement],
    path_variable: Option<&str>,
    edge_src: u32,
    edge_dst: u32,
    edge_label: Option<&str>,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    out: &mut Vec<Bindings>,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<(), GleaphError> {
    if path_variable.is_some() {
        let mut next_path = current_path.to_vec();
        next_path.push(PathElement::Edge {
            src: edge_src,
            dst: edge_dst,
            label: edge_label.map(str::to_string),
        });
        next_path.push(PathElement::Node(next_vertex));
        extend_match(
            next_hop_idx,
            next_vertex,
            next_bindings,
            &next_path,
            path_variable,
            m,
            graph,
            stats,
            out,
            where_clause,
            max_rows,
            limits,
        )
    } else {
        // Fast path: no path variable requested, so avoid per-row path vector allocations.
        extend_match(
            next_hop_idx,
            next_vertex,
            next_bindings,
            current_path,
            path_variable,
            m,
            graph,
            stats,
            out,
            where_clause,
            max_rows,
            limits,
        )
    }
}

/// Reverse-anchor optimisation for single- or multi-chain patterns where every
/// chain is `Fixed(1)`.  The start node is unbound (many candidates) but the
/// last chain's target IS bound in `seed`.
///
/// Walks chains from last to first, reverse-iterating edges at each step.
/// For a 2-chain pattern `(a:A)-[:X]->(b:B)-[:Y]->(c:C)` with `c` bound:
///   1. From `c`, reverse chain\[1\] → find `b` candidates matching `:B`
///   2. From each `b`, reverse chain\[0\] → find `a` candidates matching `:A`
#[allow(clippy::too_many_arguments)]
fn execute_reverse_chain_anchor<M: Memory>(
    seed: &Bindings,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<Vec<Bindings>, GleaphError> {
    let last_idx = m.elements.len() - 1;
    let last_chain = m.chain(last_idx);
    let target_var = last_chain
        .node
        .var
        .as_ref()
        .expect("caller checked target var is bound");
    let target_id = match seed.get(target_var) {
        Some(Binding::Vertex(id)) => *id,
        _ => unreachable!("caller checked target is bound vertex"),
    };
    let mut bindings = seed.clone();
    bindings.insert(target_var.clone(), Binding::Vertex(target_id));
    let mut out = Vec::new();
    reverse_chain_step(
        last_idx,
        target_id,
        &bindings,
        m,
        graph,
        stats,
        where_clause,
        max_rows,
        limits,
        &mut out,
    )?;
    Ok(out)
}

/// Process one chain in reverse during `execute_reverse_chain_anchor`.
///
/// `chain_idx` counts down from `elements.len()-1` to `0`.  At each step
/// `current_vertex` is the vertex on the "target" side of `elements[chain_idx]`.
/// The function reverse-iterates edges matching that chain, and for each
/// matching reached vertex either recurses (more chains remain) or adds the
/// completed row to `out`.
#[allow(clippy::too_many_arguments)]
fn reverse_chain_step<M: Memory>(
    chain_idx: usize,
    current_vertex: u32,
    bindings: &Bindings,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
    out: &mut Vec<Bindings>,
) -> Result<(), GleaphError> {
    let chain = m.chain(chain_idx);

    // The node pattern that the reached vertex must match:
    //   chain_idx == 0  → m.start (reached the pattern start)
    //   chain_idx >  0  → m.chain(chain_idx - 1).node (intermediate node)
    let reached_pattern = if chain_idx == 0 {
        &m.start
    } else {
        &m.chain(chain_idx - 1).node
    };
    let reached_var: Option<&str> = if chain_idx == 0 {
        m.start.var.as_deref()
    } else {
        m.chain(chain_idx - 1).node.var.as_deref()
    };

    let edge_ts_range = extract_edge_ts_range(where_clause, chain.edge.var.as_deref());
    let resolved_label = resolve_edge_label(&chain.edge, graph);

    // Macro-like closure: given a reached vertex + PMA edge info, check the
    // node pattern, build bindings, and either recurse or emit the row.
    // Defined as a nested fn to keep the match-arms DRY.
    #[allow(clippy::too_many_arguments)]
    fn accept_reverse_edge<M: Memory>(
        reached: u32,
        pma_src: u32,
        pma_dst: u32,
        label_arc: Option<Arc<str>>,
        edge_id: u32,
        weight: f32,
        timestamp: u64,
        chain_idx: usize,
        reached_pattern: &crate::ast::NodePattern,
        reached_var: Option<&str>,
        chain: &crate::ast::MatchChain,
        bindings: &Bindings,
        m: &MatchClause,
        graph: &PmaGraph<M>,
        stats: &mut QueryStats,
        where_clause: Option<&WhereClause>,
        max_rows: Option<usize>,
        limits: ExecutionLimits,
        out: &mut Vec<Bindings>,
    ) -> Result<(), GleaphError> {
        if !node_matches(reached_pattern, reached, graph) {
            return Ok(());
        }
        // If the reached variable is already bound (e.g. shared with a prior
        // MATCH clause), the reached vertex must equal the bound one.
        if let Some(var) = reached_var
            && let Some(Binding::Vertex(bound_id)) = bindings.get(var)
            && reached != *bound_id
        {
            return Ok(());
        }
        let mut next = bindings.clone();
        if let Some(var) = reached_var {
            next.insert(var.to_string(), Binding::Vertex(reached));
        }
        if let Some(edge_var) = &chain.edge.var {
            next.insert(
                edge_var.clone(),
                Binding::Edge {
                    src: pma_src,
                    dst: pma_dst,
                    label: label_arc,
                    edge_id,
                    weight,
                    timestamp,
                },
            );
        }
        if !eval_where_partial_pushdown(where_clause, &next, graph) {
            return Ok(());
        }
        if chain_idx == 0 {
            out.push(next);
        } else {
            reverse_chain_step(
                chain_idx - 1,
                reached,
                &next,
                m,
                graph,
                stats,
                where_clause,
                max_rows,
                limits,
                out,
            )?;
        }
        Ok(())
    }

    match chain.edge.direction {
        Direction::Outgoing => {
            // Forward: reached → current.  Reverse: incoming edges to current.
            let reverse_label_filter = match &resolved_label {
                ResolvedEdgeLabel::Exact(id) => Some(*id),
                _ => None,
            };
            let mut hit_cap = false;
            graph.for_each_reverse_neighbor(
                current_vertex,
                reverse_label_filter,
                edge_ts_range.as_ref(),
                &mut |rev| {
                    if max_rows.is_some_and(|cap| out.len() >= cap) {
                        hit_cap = true;
                        return Ok(());
                    }
                    bump_steps(stats, 1, limits)?;
                    stats.scanned_edges += 1;
                    stats.breakdown.incoming_hop_candidates = stats
                        .breakdown
                        .incoming_hop_candidates
                        .saturating_add(1);
                    if !timestamp_matches_range(edge_ts_range.as_ref(), rev.timestamp) {
                        return Ok(());
                    }
                    if !resolved_label.matches(rev.label_id()) {
                        stats.breakdown.hop_label_rejects = stats
                            .breakdown
                            .hop_label_rejects
                            .saturating_add(1);
                        stats.breakdown.incoming_hop_label_rejects = stats
                            .breakdown
                            .incoming_hop_label_rejects
                            .saturating_add(1);
                        return Ok(());
                    }
                    let label_ref = graph.label_name_by_id(rev.label_id());
                    if !check_edge_properties(&chain.edge, rev.src, current_vertex, label_ref, graph)
                    {
                        return Ok(());
                    }
                    let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
                    accept_reverse_edge(
                        rev.src,
                        rev.src,
                        current_vertex,
                        label_arc,
                        rev.edge_id,
                        rev.weight,
                        rev.timestamp,
                        chain_idx,
                        reached_pattern,
                        reached_var,
                        chain,
                        bindings,
                        m,
                        graph,
                        stats,
                        where_clause,
                        max_rows,
                        limits,
                        out,
                    )
                },
            )?;
            if hit_cap {
                return Ok(());
            }
        }
        Direction::Incoming => {
            // Forward: current → reached in PMA.  Reverse: outgoing from current.
            let outgoing_label_filter = match &resolved_label {
                ResolvedEdgeLabel::Exact(id) => Some(*id),
                _ => None,
            };
            let total = graph.for_each_neighbor_filtered(
                current_vertex,
                outgoing_label_filter,
                edge_ts_range.as_ref(),
                &mut |edge| {
                    if max_rows.is_some_and(|cap| out.len() >= cap) {
                        return Ok::<(), GleaphError>(());
                    }
                    bump_steps(stats, 1, limits)?;
                    stats.breakdown.outgoing_hop_candidates = stats
                        .breakdown
                        .outgoing_hop_candidates
                        .saturating_add(1);
                    if graph.is_vertex_tombstoned(edge.target) {
                        return Ok::<(), GleaphError>(());
                    }
                    if !resolved_label.matches(edge.label_id()) {
                        stats.breakdown.hop_label_rejects = stats
                            .breakdown
                            .hop_label_rejects
                            .saturating_add(1);
                        stats.breakdown.outgoing_hop_label_rejects = stats
                            .breakdown
                            .outgoing_hop_label_rejects
                            .saturating_add(1);
                        return Ok::<(), GleaphError>(());
                    }
                    if edge.is_tombstoned() {
                        return Ok::<(), GleaphError>(());
                    }
                    let label_ref = graph.label_name_by_id(edge.label_id());
                    if !check_edge_properties(
                        &chain.edge,
                        current_vertex,
                        edge.target,
                        label_ref,
                        graph,
                    ) {
                        return Ok::<(), GleaphError>(());
                    }
                    let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
                    accept_reverse_edge(
                        edge.target,
                        current_vertex,
                        edge.target,
                        label_arc,
                        edge.edge_id,
                        edge.weight,
                        edge.timestamp,
                        chain_idx,
                        reached_pattern,
                        reached_var,
                        chain,
                        bindings,
                        m,
                        graph,
                        stats,
                        where_clause,
                        max_rows,
                        limits,
                        out,
                    )?;
                    Ok::<(), GleaphError>(())
                },
            )?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(total);
        }
        Direction::Either => {
            // 1) Incoming edges to current (outgoing start→current in PMA).
            let reverse_label_filter = match &resolved_label {
                ResolvedEdgeLabel::Exact(id) => Some(*id),
                _ => None,
            };
            let mut hit_cap = false;
            graph.for_each_reverse_neighbor(
                current_vertex,
                reverse_label_filter,
                edge_ts_range.as_ref(),
                &mut |rev| {
                    if max_rows.is_some_and(|cap| out.len() >= cap) {
                        hit_cap = true;
                        return Ok(());
                    }
                    bump_steps(stats, 1, limits)?;
                    stats.scanned_edges += 1;
                    stats.breakdown.incoming_hop_candidates = stats
                        .breakdown
                        .incoming_hop_candidates
                        .saturating_add(1);
                    if !timestamp_matches_range(edge_ts_range.as_ref(), rev.timestamp) {
                        return Ok(());
                    }
                    if !resolved_label.matches(rev.label_id()) {
                        stats.breakdown.hop_label_rejects = stats
                            .breakdown
                            .hop_label_rejects
                            .saturating_add(1);
                        stats.breakdown.incoming_hop_label_rejects = stats
                            .breakdown
                            .incoming_hop_label_rejects
                            .saturating_add(1);
                        return Ok(());
                    }
                    let label_ref = graph.label_name_by_id(rev.label_id());
                    if !check_edge_properties(&chain.edge, rev.src, current_vertex, label_ref, graph)
                    {
                        return Ok(());
                    }
                    let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
                    accept_reverse_edge(
                        rev.src,
                        rev.src,
                        current_vertex,
                        label_arc,
                        rev.edge_id,
                        rev.weight,
                        rev.timestamp,
                        chain_idx,
                        reached_pattern,
                        reached_var,
                        chain,
                        bindings,
                        m,
                        graph,
                        stats,
                        where_clause,
                        max_rows,
                        limits,
                        out,
                    )
                },
            )?;
            if hit_cap {
                return Ok(());
            }
            // 2) Outgoing edges from current (current→reached in PMA).
            let outgoing_label_filter = match &resolved_label {
                ResolvedEdgeLabel::Exact(id) => Some(*id),
                _ => None,
            };
            let total = graph.for_each_neighbor_filtered(
                current_vertex,
                outgoing_label_filter,
                edge_ts_range.as_ref(),
                &mut |edge| {
                    if max_rows.is_some_and(|cap| out.len() >= cap) {
                        return Ok::<(), GleaphError>(());
                    }
                    bump_steps(stats, 1, limits)?;
                    stats.breakdown.outgoing_hop_candidates = stats
                        .breakdown
                        .outgoing_hop_candidates
                        .saturating_add(1);
                    if graph.is_vertex_tombstoned(edge.target) || edge.is_tombstoned() {
                        return Ok::<(), GleaphError>(());
                    }
                    if !resolved_label.matches(edge.label_id()) {
                        stats.breakdown.hop_label_rejects = stats
                            .breakdown
                            .hop_label_rejects
                            .saturating_add(1);
                        stats.breakdown.outgoing_hop_label_rejects = stats
                            .breakdown
                            .outgoing_hop_label_rejects
                            .saturating_add(1);
                        return Ok::<(), GleaphError>(());
                    }
                    let label_ref = graph.label_name_by_id(edge.label_id());
                    if !check_edge_properties(
                        &chain.edge,
                        current_vertex,
                        edge.target,
                        label_ref,
                        graph,
                    ) {
                        return Ok::<(), GleaphError>(());
                    }
                    let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
                    accept_reverse_edge(
                        edge.target,
                        current_vertex,
                        edge.target,
                        label_arc,
                        edge.edge_id,
                        edge.weight,
                        edge.timestamp,
                        chain_idx,
                        reached_pattern,
                        reached_var,
                        chain,
                        bindings,
                        m,
                        graph,
                        stats,
                        where_clause,
                        max_rows,
                        limits,
                        out,
                    )?;
                    Ok::<(), GleaphError>(())
                },
            )?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(total);
        }
    }
    Ok(())
}

/// Check edge property hints (shared by forward and reverse traversal paths).
fn check_edge_properties<M: Memory>(
    edge_pattern: &crate::ast::EdgePattern,
    src: u32,
    dst: u32,
    label_ref: Option<&str>,
    graph: &PmaGraph<M>,
) -> bool {
    if edge_pattern.properties.is_empty() {
        return true;
    }
    let edge_props = graph
        .edge_record(src, dst, label_ref)
        .map(|e| e.props)
        .unwrap_or_default();
    edge_pattern.properties.iter().all(|(key, expected_expr)| {
        if let Expr::Literal(expected) = expected_expr {
            let actual = edge_props
                .iter()
                .find_map(|(k, v)| if k == key { Some(v.clone()) } else { None })
                .unwrap_or(Value::Null);
            compare_values(&actual, expected) == Some(Ordering::Equal)
        } else {
            true
        }
    })
}

/// Bind edge/node variables, evaluate inline WHERE clauses, check pushdown predicates,
/// and recurse into the next hop.  Shared by all three direction arms of `extend_hop`.
#[allow(clippy::too_many_arguments)]
fn bind_and_recurse_hop<M: Memory>(
    chain: &crate::ast::MatchChain,
    hop_idx: usize,
    next_vertex: u32,
    edge_src: u32,
    edge_dst: u32,
    label_arc: Option<Arc<str>>,
    edge_id: u32,
    weight: f32,
    timestamp: u64,
    current_bindings: &Bindings,
    current_path: &[PathElement],
    path_variable: Option<&str>,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    out: &mut Vec<Bindings>,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<bool, GleaphError> {
    let mut next_bindings = current_bindings.clone();
    if let Some(edge_var) = &chain.edge.var {
        next_bindings.insert(
            edge_var.clone(),
            Binding::Edge {
                src: edge_src,
                dst: edge_dst,
                label: label_arc.clone(),
                edge_id,
                weight,
                timestamp,
            },
        );
    }
    // GQL §16.7: evaluate edge inline WHERE per-hop.
    if let Some(w) = chain.edge.where_clause.as_deref()
        && !truthy(&eval_expr(w, &next_bindings, graph))
    {
        return Ok(false);
    }
    if let Some(node_var) = &chain.node.var {
        next_bindings.insert(node_var.clone(), Binding::Vertex(next_vertex));
    }
    // GQL §16.7: evaluate chain node inline WHERE per-hop.
    if let Some(w) = chain.node.where_clause.as_deref()
        && !truthy(&eval_expr(w, &next_bindings, graph))
    {
        return Ok(false);
    }
    // Mid-hop predicate pushdown: prune this neighbor if all-bound conjuncts fail.
    if !eval_where_partial_pushdown(where_clause, &next_bindings, graph) {
        stats.breakdown.hop_where_pushdown_rejects = stats
            .breakdown
            .hop_where_pushdown_rejects
            .saturating_add(1);
        return Ok(false);
    }
    recurse_extend_match_with_optional_path(
        hop_idx + 1,
        next_vertex,
        &next_bindings,
        current_path,
        path_variable,
        edge_src,
        edge_dst,
        label_arc.as_deref(),
        m,
        graph,
        stats,
        out,
        where_clause,
        max_rows,
        limits,
    )?;
    Ok(max_rows.is_some_and(|cap| out.len() >= cap))
}

/// Process a single outgoing edge candidate: apply filters, property checks,
/// then bind and recurse.  Returns `true` when `max_rows` cap is reached.
#[allow(clippy::too_many_arguments)]
fn process_outgoing_edge<M: Memory>(
    edge: &EdgeEntry,
    chain: &crate::ast::MatchChain,
    resolved_node: &ResolvedNodeMatch,
    hop_idx: usize,
    current_vertex: u32,
    ts_pre_filtered: bool,
    edge_ts_range: Option<&TimestampRange>,
    resolved_label: &ResolvedEdgeLabel,
    edge_index_filter: Option<&VertexIdSet>,
    current_bindings: &Bindings,
    current_path: &[PathElement],
    path_variable: Option<&str>,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    out: &mut Vec<Bindings>,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<bool, GleaphError> {
    stats.breakdown.outgoing_hop_candidates = stats
        .breakdown
        .outgoing_hop_candidates
        .saturating_add(1);
    if graph.is_vertex_tombstoned(edge.target) {
        return Ok(false);
    }
    if !ts_pre_filtered && !timestamp_matches_range(edge_ts_range, edge.timestamp) {
        return Ok(false);
    }
    if !resolved_label.matches(edge.label_id()) {
        stats.breakdown.hop_label_rejects = stats
            .breakdown
            .hop_label_rejects
            .saturating_add(1);
        stats.breakdown.outgoing_hop_label_rejects = stats
            .breakdown
            .outgoing_hop_label_rejects
            .saturating_add(1);
        return Ok(false);
    }
    if let Some(indexed) = edge_index_filter
        && !indexed.contains(edge.target)
    {
        return Ok(false);
    }
    if edge.is_tombstoned() {
        return Ok(false);
    }
    let label_ref = graph.label_name_by_id(edge.label_id());
    if edge_index_filter.is_none()
        && !check_edge_properties(&chain.edge, current_vertex, edge.target, label_ref, graph)
    {
        stats.breakdown.hop_edge_property_rejects = stats
            .breakdown
            .hop_edge_property_rejects
            .saturating_add(1);
        return Ok(false);
    }
    if !resolved_node.matches_no_tombstone(&chain.node, edge.target, graph) {
        stats.breakdown.hop_node_rejects = stats
            .breakdown
            .hop_node_rejects
            .saturating_add(1);
        return Ok(false);
    }
    let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
    bind_and_recurse_hop(
        chain,
        hop_idx,
        edge.target,    // next_vertex
        current_vertex, // edge_src
        edge.target,    // edge_dst
        label_arc,
        edge.edge_id,
        edge.weight,
        edge.timestamp,
        current_bindings,
        current_path,
        path_variable,
        m,
        graph,
        stats,
        out,
        where_clause,
        max_rows,
        limits,
    )
}

/// Process a single reverse (incoming) edge candidate: apply filters, property checks,
/// then bind and recurse.  Returns `true` when `max_rows` cap is reached.
#[allow(clippy::too_many_arguments)]
fn process_incoming_edge<M: Memory>(
    rev: &RevEntry,
    chain: &crate::ast::MatchChain,
    resolved_node: &ResolvedNodeMatch,
    hop_idx: usize,
    current_vertex: u32,
    edge_ts_range: Option<&TimestampRange>,
    resolved_label: &ResolvedEdgeLabel,
    edge_index_filter: Option<&VertexIdSet>,
    current_bindings: &Bindings,
    current_path: &[PathElement],
    path_variable: Option<&str>,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    out: &mut Vec<Bindings>,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<bool, GleaphError> {
    stats.breakdown.incoming_hop_candidates = stats
        .breakdown
        .incoming_hop_candidates
        .saturating_add(1);
    if !timestamp_matches_range(edge_ts_range, rev.timestamp) {
        return Ok(false);
    }
    if !resolved_label.matches(rev.label_id()) {
        stats.breakdown.hop_label_rejects = stats
            .breakdown
            .hop_label_rejects
            .saturating_add(1);
        stats.breakdown.incoming_hop_label_rejects = stats
            .breakdown
            .incoming_hop_label_rejects
            .saturating_add(1);
        return Ok(false);
    }
    if let Some(indexed) = edge_index_filter
        && !indexed.contains(rev.src)
    {
        return Ok(false);
    }
    let label_ref = graph.label_name_by_id(rev.label_id());
    if edge_index_filter.is_none()
        && !check_edge_properties(&chain.edge, rev.src, current_vertex, label_ref, graph)
    {
        stats.breakdown.hop_edge_property_rejects = stats
            .breakdown
            .hop_edge_property_rejects
            .saturating_add(1);
        return Ok(false);
    }
    if !resolved_node.matches(&chain.node, rev.src, graph) {
        stats.breakdown.hop_node_rejects = stats
            .breakdown
            .hop_node_rejects
            .saturating_add(1);
        return Ok(false);
    }
    let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
    bind_and_recurse_hop(
        chain,
        hop_idx,
        rev.src,        // next_vertex
        rev.src,        // edge_src
        current_vertex, // edge_dst
        label_arc,
        rev.edge_id,
        rev.weight,
        rev.timestamp,
        current_bindings,
        current_path,
        path_variable,
        m,
        graph,
        stats,
        out,
        where_clause,
        max_rows,
        limits,
    )
}

#[allow(clippy::too_many_arguments)]
fn extend_hop<M: Memory>(
    hop_idx: usize,
    current_vertex: u32,
    current_bindings: &Bindings,
    current_path: &[PathElement],
    path_variable: Option<&str>,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    out: &mut Vec<Bindings>,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<(), GleaphError> {
    let chain = m.chain(hop_idx);
    // GQL §16.7: also extract timestamp range from edge inline WHERE for pushdown.
    let edge_ts_range =
        extract_edge_ts_range(where_clause, chain.edge.var.as_deref()).or_else(|| {
            chain
                .edge
                .where_clause
                .as_deref()
                .and_then(|w| extract_edge_ts_range_from_expr(w, chain.edge.var.as_deref()?))
        });
    let resolved_label = resolve_edge_label(&chain.edge, graph);
    let resolved_node = resolve_node_match(&chain.node, graph);
    // Edge-index pre-filter: when edge property hints have indexed literal values,
    // compute matching targets/sources to skip loading edge records for non-matching edges.
    let edge_index_filter: Option<VertexIdSet> = if !chain.edge.properties.is_empty() {
        let mut result: Option<VertexIdSet> = None;
        for (key, expr) in &chain.edge.properties {
            if let Expr::Literal(val) = expr {
                let indexed = match chain.edge.direction {
                    Direction::Outgoing => {
                        graph.edge_index_targets_for_src(key, val, current_vertex)
                    }
                    Direction::Incoming => {
                        graph.edge_index_sources_for_dst(key, val, current_vertex)
                    }
                    Direction::Either => None, // Both directions — skip index filter
                };
                if let Some(targets) = indexed {
                    let target_set: VertexIdSet = targets.into_iter().collect();
                    result = Some(match result {
                        Some(existing) => &existing & &target_set,
                        None => target_set,
                    });
                }
            }
        }
        result
    } else {
        None
    };
    let reverse_label_filter = match &resolved_label {
        ResolvedEdgeLabel::Exact(id) => Some(*id),
        _ => None,
    };
    let outgoing_label_filter = match &resolved_label {
        ResolvedEdgeLabel::Exact(id) => Some(*id),
        _ => None,
    };

    // Iterate outgoing edges and process each candidate.
    let iterate_outgoing =
        |stats: &mut QueryStats, out: &mut Vec<Bindings>| -> Result<(), GleaphError> {
            let mut hit_cap = false;
            let total = graph.for_each_neighbor_filtered(
                current_vertex,
                outgoing_label_filter,
                edge_ts_range.as_ref(),
                &mut |edge| {
                    if hit_cap {
                        return Ok(());
                    }
                    bump_steps(stats, 1, limits)?;
                    if process_outgoing_edge(
                        &edge,
                        chain,
                        &resolved_node,
                        hop_idx,
                        current_vertex,
                        true,
                        edge_ts_range.as_ref(),
                        &resolved_label,
                        edge_index_filter.as_ref(),
                        current_bindings,
                        current_path,
                        path_variable,
                        m,
                        graph,
                        stats,
                        out,
                        where_clause,
                        max_rows,
                        limits,
                    )? {
                        hit_cap = true;
                    }
                    Ok(())
                },
            )?;
            stats.scanned_edges = stats.scanned_edges.saturating_add(total);
            Ok(())
        };

    // Iterate incoming (reverse) edges and process each candidate.
    let iterate_incoming =
        |stats: &mut QueryStats, out: &mut Vec<Bindings>| -> Result<(), GleaphError> {
            let mut hit_cap = false;
            graph.for_each_reverse_neighbor(
                current_vertex,
                reverse_label_filter,
                edge_ts_range.as_ref(),
                &mut |rev| {
                    bump_steps(stats, 1, limits)?;
                    stats.scanned_edges += 1;
                    if process_incoming_edge(
                        &rev,
                        chain,
                        &resolved_node,
                        hop_idx,
                        current_vertex,
                        edge_ts_range.as_ref(),
                        &resolved_label,
                        edge_index_filter.as_ref(),
                        current_bindings,
                        current_path,
                        path_variable,
                        m,
                        graph,
                        stats,
                        out,
                        where_clause,
                        max_rows,
                        limits,
                    )? {
                        hit_cap = true;
                    }
                    Ok(())
                },
            )?;
            if hit_cap {
                return Ok(());
            }
            Ok(())
        };

    match chain.edge.direction {
        Direction::Outgoing => iterate_outgoing(stats, out)?,
        Direction::Incoming => iterate_incoming(stats, out)?,
        Direction::Either => {
            iterate_outgoing(stats, out)?;
            if max_rows.is_none_or(|cap| out.len() < cap) {
                iterate_incoming(stats, out)?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn extend_var_len<M: Memory>(
    hop_idx: usize,
    start_vertex: u32,
    current_bindings: &Bindings,
    current_path: &[PathElement],
    path_variable: Option<&str>,
    min_hops: u32,
    max_hops: u32,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    out: &mut Vec<Bindings>,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<(), GleaphError> {
    let chain = m.chain(hop_idx);
    let edge_slot = chain
        .edge
        .var
        .as_ref()
        .and_then(|v| current_bindings.reg.slot_opt(v));
    let node_slot = chain
        .node
        .var
        .as_ref()
        .and_then(|v| current_bindings.reg.slot_opt(v));
    let resolved_label = resolve_edge_label(&chain.edge, graph);
    let mut path = vec![start_vertex];
    let mut path_elems = current_path.to_vec();
    traverse_var_len(
        hop_idx,
        start_vertex,
        current_bindings,
        &mut path,
        &mut path_elems,
        0,
        None,
        path_variable,
        min_hops,
        max_hops,
        edge_slot,
        node_slot,
        &resolved_label,
        m,
        graph,
        stats,
        out,
        where_clause,
        max_rows,
        limits,
    )
}

#[allow(clippy::too_many_arguments)]
fn traverse_var_len<M: Memory>(
    hop_idx: usize,
    current_vertex: u32,
    current_bindings: &Bindings,
    path: &mut Vec<u32>,
    path_elems: &mut Vec<PathElement>,
    depth: u32,
    // Timestamp of the edge that led to current_vertex (None at the start vertex).
    last_edge_ts: Option<u64>,
    path_variable: Option<&str>,
    min_hops: u32,
    max_hops: u32,
    edge_slot: Option<usize>,
    node_slot: Option<usize>,
    resolved_label: &ResolvedEdgeLabel,
    m: &MatchClause,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    out: &mut Vec<Bindings>,
    where_clause: Option<&WhereClause>,
    max_rows: Option<usize>,
    limits: ExecutionLimits,
) -> Result<(), GleaphError> {
    stats.breakdown.var_len_dfs_calls = stats.breakdown.var_len_dfs_calls.saturating_add(1);
    if max_rows.is_some_and(|cap| out.len() >= cap) {
        return Ok(());
    }
    let chain = m.chain(hop_idx);
    let resolved_node = resolve_node_match(&chain.node, graph);
    let outgoing_label_filter = match resolved_label {
        ResolvedEdgeLabel::Exact(id) => Some(*id),
        _ => None,
    };
    if depth >= min_hops
        && depth <= max_hops
        && depth > 0
        && {
            stats.breakdown.var_len_node_match_checks = stats
                .breakdown
                .var_len_node_match_checks
                .saturating_add(1);
            resolved_node.matches(&chain.node, current_vertex, graph)
        }
        // Apply temporal predicates only against the final (last) edge binding so
        // that intermediate hops are not incorrectly pruned.
        && edge_temporal_predicates_match(
            where_clause,
            chain.edge.var.as_deref(),
            last_edge_ts.unwrap_or(0),
        )
    {
        let mut next_bindings = current_bindings.clone();
        stats.breakdown.var_len_binding_clones = stats
            .breakdown
            .var_len_binding_clones
            .saturating_add(1);
        if let Some(s) = node_slot {
            next_bindings.set_slot(s, Binding::Vertex(current_vertex));
        }
        // GQL §16.7: evaluate terminal node inline WHERE in variable-length path.
        // Only skip yielding (not traversal) if the predicate fails.
        let node_where_ok = chain
            .node
            .where_clause
            .as_deref()
            .is_none_or(|w| truthy(&eval_expr(w, &next_bindings, graph)));
        // GQL §16.7: evaluate edge inline WHERE against the final edge binding.
        let edge_where_ok = chain
            .edge
            .where_clause
            .as_deref()
            .is_none_or(|w| truthy(&eval_expr(w, &next_bindings, graph)));
        if node_where_ok && edge_where_ok {
            extend_match(
                hop_idx + 1,
                current_vertex,
                &next_bindings,
                path_elems,
                path_variable,
                m,
                graph,
                stats,
                out,
                where_clause,
                max_rows,
                limits,
            )?;
        }
    }
    if depth == max_hops {
        return Ok(());
    }
    graph.for_each_neighbor_filtered(current_vertex, outgoing_label_filter, None, &mut |edge| {
        bump_steps(stats, 1, limits)?;
        stats.scanned_edges = stats.scanned_edges.saturating_add(1);
        stats.breakdown.outgoing_hop_candidates = stats
            .breakdown
            .outgoing_hop_candidates
            .saturating_add(1);
        if graph.is_vertex_tombstoned(edge.target) {
            return Ok::<(), GleaphError>(());
        }
        if !resolved_label.matches(edge.label_id()) {
            stats.breakdown.hop_label_rejects = stats
                .breakdown
                .hop_label_rejects
                .saturating_add(1);
            stats.breakdown.outgoing_hop_label_rejects = stats
                .breakdown
                .outgoing_hop_label_rejects
                .saturating_add(1);
            return Ok::<(), GleaphError>(());
        }
        if edge.is_tombstoned() {
            return Ok::<(), GleaphError>(());
        }
        let label_ref = graph.label_name_by_id(edge.label_id());
        stats.breakdown.var_len_path_contains_checks = stats
            .breakdown
            .var_len_path_contains_checks
            .saturating_add(1);
        if path.contains(&edge.target) {
            stats.breakdown.var_len_cycle_rejects = stats
                .breakdown
                .var_len_cycle_rejects
                .saturating_add(1);
            return Ok::<(), GleaphError>(());
        }
        stats.breakdown.var_len_node_match_checks = stats
            .breakdown
            .var_len_node_match_checks
            .saturating_add(1);
        if !resolved_node.matches_no_tombstone(&chain.node, edge.target, graph) {
            stats.breakdown.hop_node_rejects = stats
                .breakdown
                .hop_node_rejects
                .saturating_add(1);
            return Ok::<(), GleaphError>(());
        }
        let label_arc: Option<Arc<str>> = label_ref.map(Arc::from);
        let mut next_bindings = current_bindings.clone();
        stats.breakdown.var_len_binding_clones = stats
            .breakdown
            .var_len_binding_clones
            .saturating_add(1);
        if let Some(s) = edge_slot {
            next_bindings.set_slot(
                s,
                Binding::Edge {
                    src: current_vertex,
                    dst: edge.target,
                    label: label_arc.clone(),
                    edge_id: edge.edge_id,
                    weight: edge.weight,
                    timestamp: edge.timestamp,
                },
            );
        }
        path.push(edge.target);
        path_elems.push(PathElement::Edge {
            src: current_vertex,
            dst: edge.target,
            label: label_arc.as_deref().map(str::to_string),
        });
        path_elems.push(PathElement::Node(edge.target));
        traverse_var_len(
            hop_idx,
            edge.target,
            &next_bindings,
            path,
            path_elems,
            depth + 1,
            Some(edge.timestamp),
            path_variable,
            min_hops,
            max_hops,
            edge_slot,
            node_slot,
            resolved_label,
            m,
            graph,
            stats,
            out,
            where_clause,
            max_rows,
            limits,
        )?;
        path.pop();
        path_elems.pop();
        path_elems.pop();
        if max_rows.is_some_and(|cap| out.len() >= cap) {
            return Ok::<(), GleaphError>(());
        }
        Ok::<(), GleaphError>(())
    })?;
    Ok(())
}

fn edge_temporal_predicates_match(
    where_clause: Option<&WhereClause>,
    edge_var: Option<&str>,
    timestamp: u64,
) -> bool {
    let range = extract_edge_ts_range(where_clause, edge_var);
    timestamp_matches_range(range.as_ref(), timestamp)
}

#[inline]
fn timestamp_matches_range(range: Option<&TimestampRange>, timestamp: u64) -> bool {
    let Some(range) = range else {
        return true;
    };
    if let Some(start) = range.start
        && timestamp < start
    {
        return false;
    }
    if let Some(end) = range.end
        && timestamp > end
    {
        return false;
    }
    true
}

/// Returns `true` if `e` is a `gleaph_timestamp(var)` or `timestamp(var)` call
/// where `var` is one of the given edge variables.
fn is_edge_ts_fn_call(e: &Expr, edge_vars: &[&str]) -> bool {
    if let Expr::FunctionCall { name, args } = e
        && let lower = name.to_ascii_lowercase()
        && (lower == "gleaph_timestamp" || lower == "timestamp")
        && args.len() == 1
        && let Expr::Variable(v) = &args[0]
    {
        return edge_vars.contains(&v.as_str());
    }
    false
}

/// Removes edge-timestamp comparison predicates (already handled by `ts_ranges`)
/// from a WHERE expression.  Returns `None` when the entire expression was stripped.
fn strip_edge_ts_predicates(expr: &Expr, edge_vars: &[&str]) -> Option<Expr> {
    fn is_edge_ts_compare(expr: &Expr, edge_vars: &[&str]) -> bool {
        let Expr::Compare { left, op: _, right } = expr else {
            return false;
        };
        (is_edge_ts_fn_call(left, edge_vars) && eval_literal_expr(right).is_some())
            || (is_edge_ts_fn_call(right, edge_vars) && eval_literal_expr(left).is_some())
    }
    if is_edge_ts_compare(expr, edge_vars) {
        return None;
    }
    match expr {
        Expr::And(l, r) => {
            let l2 = strip_edge_ts_predicates(l, edge_vars);
            let r2 = strip_edge_ts_predicates(r, edge_vars);
            match (l2, r2) {
                (Some(l2), Some(r2)) => Some(Expr::And(Box::new(l2), Box::new(r2))),
                (Some(l2), None) => Some(l2),
                (None, Some(r2)) => Some(r2),
                (None, None) => None,
            }
        }
        _ => Some(expr.clone()),
    }
}

fn extract_edge_ts_range(
    where_clause: Option<&WhereClause>,
    edge_var: Option<&str>,
) -> Option<TimestampRange> {
    let edge_var = edge_var?;
    let expr = where_clause?;
    extract_edge_ts_range_from_expr(expr, edge_var)
}

fn extract_edge_ts_range_from_expr(expr: &Expr, edge_var: &str) -> Option<TimestampRange> {
    fn merge(mut a: TimestampRange, b: TimestampRange) -> TimestampRange {
        a.start = match (a.start, b.start) {
            (Some(x), Some(y)) => Some(x.max(y)),
            (None, Some(y)) => Some(y),
            (x, None) => x,
        };
        a.end = match (a.end, b.end) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (None, Some(y)) => Some(y),
            (x, None) => x,
        };
        a
    }

    fn ts_side(expr: &Expr, edge_var: &str) -> bool {
        is_edge_ts_fn_call(expr, &[edge_var])
    }

    match expr {
        Expr::And(l, r) => {
            let l = extract_edge_ts_range_from_expr(l, edge_var);
            let r = extract_edge_ts_range_from_expr(r, edge_var);
            match (l, r) {
                (Some(a), Some(b)) => Some(merge(a, b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }
        }
        Expr::Compare { left, op, right } => {
            if ts_side(left, edge_var) {
                let lit = timestamp_literal(&eval_literal_expr(right)?)?;
                match op {
                    CmpOp::Eq => Some(TimestampRange {
                        start: Some(lit),
                        end: Some(lit),
                    }),
                    CmpOp::Ge => Some(TimestampRange {
                        start: Some(lit),
                        end: None,
                    }),
                    CmpOp::Gt => Some(TimestampRange {
                        start: lit.checked_add(1),
                        end: None,
                    }),
                    CmpOp::Le => Some(TimestampRange {
                        start: None,
                        end: Some(lit),
                    }),
                    CmpOp::Lt => Some(TimestampRange {
                        start: None,
                        end: lit.checked_sub(1),
                    }),
                    CmpOp::Ne => None,
                }
            } else if ts_side(right, edge_var) {
                let lit = timestamp_literal(&eval_literal_expr(left)?)?;
                match op {
                    CmpOp::Eq => Some(TimestampRange {
                        start: Some(lit),
                        end: Some(lit),
                    }),
                    CmpOp::Le => Some(TimestampRange {
                        start: Some(lit),
                        end: None,
                    }), // lit <= ts  => ts >= lit
                    CmpOp::Lt => Some(TimestampRange {
                        start: lit.checked_add(1),
                        end: None,
                    }), // lit < ts   => ts > lit
                    CmpOp::Ge => Some(TimestampRange {
                        start: None,
                        end: Some(lit),
                    }), // lit >= ts  => ts <= lit
                    CmpOp::Gt => Some(TimestampRange {
                        start: None,
                        end: lit.checked_sub(1),
                    }), // lit > ts   => ts < lit
                    CmpOp::Ne => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn eval_literal_expr(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::Literal(v) => Some(v.clone()),
        _ => None,
    }
}

fn timestamp_literal(v: &Value) -> Option<u64> {
    match v {
        Value::Timestamp(ts) => Some(*ts),
        _ => v.as_i64().and_then(|i| u64::try_from(i).ok()),
    }
}

fn initial_candidates<M: Memory>(
    node: &crate::ast::NodePattern,
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
    limits: ExecutionLimits,
) -> Result<Vec<u32>, GleaphError> {
    // Fast path: use live equality index for inline props_hint.
    if let Some(indexed) = try_indexed_props_hint_scan(node, graph) {
        let mut out = Vec::new();
        for v in indexed {
            bump_steps(stats, 1, limits)?;
            stats.scanned_vertices += 1;
            if node_matches(node, v, graph) {
                out.push(v);
            }
        }
        return Ok(out);
    }

    let (candidates, label_scanned) = if let Some(label) = node.labels.first() {
        (graph.scan_vertices_by_label(label), true)
    } else {
        let n = graph.vertex_count() as u32;
        let mut all = VertexIdSet::from_sorted_iter(0..n).unwrap_or_default();
        let tombstoned = graph.tombstoned_vertex_set();
        if !tombstoned.is_empty() {
            all -= tombstoned;
        }
        (all, false)
    };

    // When candidates came from scan_vertices_by_label, the label and tombstone
    // checks are already done.  Skip node_matches if no additional filters exist.
    let skip_node_matches = label_scanned
        && node.labels.len() <= 1
        && node.label_expr.is_none()
        && node.type_annotation.is_none()
        && node.props_hint.is_empty()
        && node.where_clause.is_none();

    let mut out = Vec::new();
    for v in candidates {
        bump_steps(stats, 1, limits)?;
        stats.scanned_vertices += 1;
        if skip_node_matches || node_matches(node, v, graph) {
            out.push(v);
        }
    }
    Ok(out)
}

/// Attempts to use the live equality index to narrow candidates from `props_hint`.
/// Returns `Some(vertex_ids)` if an indexed property was found, `None` otherwise.
fn try_indexed_props_hint_scan<M: Memory>(
    node: &crate::ast::NodePattern,
    graph: &PmaGraph<M>,
) -> Option<VertexIdSet> {
    for (prop, expr) in &node.props_hint {
        if let Expr::Literal(val) = expr
            && let Some(ids) = graph.scan_vertices_by_property_eq_live(prop, val)
        {
            return Some(ids);
        }
    }
    None
}

fn bump_steps(
    stats: &mut QueryStats,
    inc: u64,
    limits: ExecutionLimits,
) -> Result<(), GleaphError> {
    stats.execution_steps = stats.execution_steps.saturating_add(inc);
    if let Some(cap) = limits.max_execution_steps
        && stats.execution_steps > cap
    {
        return Err(GleaphError::ExecutionError(format!(
            "execution steps {} exceed hard cap {}",
            stats.execution_steps, cap
        )));
    }
    Ok(())
}

fn node_matches<M: Memory>(
    node: &crate::ast::NodePattern,
    vertex_id: u32,
    graph: &PmaGraph<M>,
) -> bool {
    // Type annotation check — resolve to labels and filter.
    if let Some(type_expr) = &node.type_annotation {
        if !node_matches_type_expr(type_expr, vertex_id, graph) {
            return false;
        }
    } else if let Some(label_expr) = &node.label_expr {
        // Check label_expr first if present; otherwise fall back to labels vec
        if !matches_label_expr(label_expr, vertex_id, graph) {
            return false;
        }
    } else if node
        .labels
        .iter()
        .any(|label| !graph.vertex_has_label(vertex_id, label))
    {
        return false;
    }
    if node.props_hint.is_empty() {
        return true;
    }
    let props = graph.get_vertex_props(vertex_id).unwrap_or_default();
    for (key, expr) in &node.props_hint {
        // parser/validator restrict props_hint to literals only
        let Expr::Literal(expected) = expr else {
            return false;
        };
        // Property hints are exact-match filters on node properties.
        let actual = props
            .iter()
            .find_map(|(k, v)| if k == key { Some(v.clone()) } else { None })
            .unwrap_or(Value::Null);
        if compare_values(&actual, expected) != Some(Ordering::Equal) {
            return false;
        }
    }
    true
}

enum ResolvedNodeMatch {
    Any,
    ExactLabel(u32),
    NoMatch,
    Fallback,
}

fn resolve_node_match<M: Memory>(
    node: &crate::ast::NodePattern,
    graph: &PmaGraph<M>,
) -> ResolvedNodeMatch {
    if node.type_annotation.is_some()
        || node.label_expr.is_some()
        || !node.props_hint.is_empty()
        || node.where_clause.is_some()
    {
        return ResolvedNodeMatch::Fallback;
    }
    match node.labels.as_slice() {
        [] => ResolvedNodeMatch::Any,
        [label] => graph
            .label_index
            .label_id(label)
            .map(ResolvedNodeMatch::ExactLabel)
            .unwrap_or(ResolvedNodeMatch::NoMatch),
        _ => ResolvedNodeMatch::Fallback,
    }
}

impl ResolvedNodeMatch {
    #[inline]
    fn matches<M: Memory>(&self, node: &crate::ast::NodePattern, vertex_id: u32, graph: &PmaGraph<M>) -> bool {
        match self {
            Self::Any => !graph.is_vertex_tombstoned(vertex_id),
            Self::ExactLabel(label_id) => graph.vertex_has_label_id(vertex_id, *label_id),
            Self::NoMatch => false,
            Self::Fallback => node_matches(node, vertex_id, graph),
        }
    }

    #[inline]
    fn matches_no_tombstone<M: Memory>(
        &self,
        node: &crate::ast::NodePattern,
        vertex_id: u32,
        graph: &PmaGraph<M>,
    ) -> bool {
        match self {
            Self::Any => true,
            Self::ExactLabel(label_id) => graph.vertex_has_label_id_unchecked(vertex_id, *label_id),
            Self::NoMatch => false,
            Self::Fallback => node_matches_no_tombstone(node, vertex_id, graph),
        }
    }
}

/// Like `node_matches` but skips the vertex tombstone check inside label lookups.
/// Use when the caller has already verified the vertex is not tombstoned.
fn node_matches_no_tombstone<M: Memory>(
    node: &crate::ast::NodePattern,
    vertex_id: u32,
    graph: &PmaGraph<M>,
) -> bool {
    if let Some(type_expr) = &node.type_annotation {
        if !node_matches_type_expr(type_expr, vertex_id, graph) {
            return false;
        }
    } else if let Some(label_expr) = &node.label_expr {
        if !matches_label_expr_unchecked(label_expr, vertex_id, graph) {
            return false;
        }
    } else if node
        .labels
        .iter()
        .any(|label| !graph.vertex_has_label_unchecked(vertex_id, label))
    {
        return false;
    }
    if node.props_hint.is_empty() {
        return true;
    }
    let props = graph.get_vertex_props(vertex_id).unwrap_or_default();
    for (key, expr) in &node.props_hint {
        let Expr::Literal(expected) = expr else {
            return false;
        };
        let actual = props
            .iter()
            .find_map(|(k, v)| if k == key { Some(v.clone()) } else { None })
            .unwrap_or(Value::Null);
        if compare_values(&actual, expected) != Some(Ordering::Equal) {
            return false;
        }
    }
    true
}

/// Checks whether a vertex matches a type expression.
///
/// Resolves each type name to a set of labels via the thread-local `NODE_TYPE_DEFS`.
/// If no definition is found, the type name itself is treated as a label (graceful degradation).
/// For Union types, the vertex must match at least one branch.
fn node_matches_type_expr<M: Memory>(
    type_expr: &crate::ast::TypeExpr,
    vertex_id: u32,
    graph: &PmaGraph<M>,
) -> bool {
    match type_expr {
        crate::ast::TypeExpr::Name(name) => {
            let labels = resolve_type_name_to_labels(name);
            labels.iter().all(|l| graph.vertex_has_label(vertex_id, l))
        }
        crate::ast::TypeExpr::Union(left, right) => {
            node_matches_type_expr(left, vertex_id, graph)
                || node_matches_type_expr(right, vertex_id, graph)
        }
    }
}

/// Resolves a single type name to its label list.
///
/// Checks `NODE_TYPE_DEFS` first; if not found, returns the type name itself as a label.
fn resolve_type_name_to_labels(name: &str) -> Vec<String> {
    NODE_TYPE_DEFS.with(|d| {
        let defs = d.borrow();
        if let Some(labels) = defs.get(&name.to_ascii_lowercase()) {
            labels.clone()
        } else if let Some(labels) = defs.get(name) {
            labels.clone()
        } else {
            // Graceful degradation: treat type name as label name
            vec![name.to_string()]
        }
    })
}

#[derive(Clone, Debug)]
struct CompiledWhereClause {
    conjuncts: Vec<CompiledWhereConjunct>,
}

#[derive(Clone, Debug)]
struct CompiledWhereConjunct {
    source_expr: Expr,
    required_slots: Vec<usize>,
    has_unresolved_slots: bool,
    compiled_expr: CompiledWhereExpr,
}

#[derive(Clone, Debug)]
enum CompiledWhereExpr {
    Compare {
        left: CompiledWhereOperand,
        op: CmpOp,
        right: CompiledWhereOperand,
    },
    IsNull(CompiledWhereOperand),
    IsNotNull(CompiledWhereOperand),
    Truthy(CompiledWhereOperand),
    And(Box<CompiledWhereExpr>, Box<CompiledWhereExpr>),
    Or(Box<CompiledWhereExpr>, Box<CompiledWhereExpr>),
    Not(Box<CompiledWhereExpr>),
    Fallback(Expr),
}

#[derive(Clone, Debug)]
enum CompiledWhereOperand {
    Literal(Value),
    Parameter { name: String },
    Variable { slot: usize },
    PropertyAccess { slot: usize, property: String },
}

#[derive(Clone, Debug)]
enum CompiledValueExpr {
    Operand(CompiledWhereOperand),
    Unary {
        op: UnaryOp,
        expr: Box<CompiledValueExpr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<CompiledValueExpr>,
        right: Box<CompiledValueExpr>,
    },
    Compare {
        left: Box<CompiledValueExpr>,
        op: CmpOp,
        right: Box<CompiledValueExpr>,
    },
    And(Box<CompiledValueExpr>, Box<CompiledValueExpr>),
    Or(Box<CompiledValueExpr>, Box<CompiledValueExpr>),
    Xor(Box<CompiledValueExpr>, Box<CompiledValueExpr>),
    Not(Box<CompiledValueExpr>),
    IsNull(Box<CompiledValueExpr>),
    IsNotNull(Box<CompiledValueExpr>),
    Concat(Box<CompiledValueExpr>, Box<CompiledValueExpr>),
    Coalesce(Vec<CompiledValueExpr>),
    NullIf {
        left: Box<CompiledValueExpr>,
        right: Box<CompiledValueExpr>,
    },
    Fallback(Expr),
}

struct CompiledWhereGuard;

impl Drop for CompiledWhereGuard {
    fn drop(&mut self) {
        COMPILED_WHERE_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

fn with_current_compiled_where<R>(f: impl FnOnce(Option<&CompiledWhereClause>) -> R) -> R {
    let compiled =
        COMPILED_WHERE_STACK.with(|stack| stack.borrow().last().and_then(|entry| entry.clone()));
    f(compiled.as_deref())
}

fn push_compiled_where_scope(where_clause: Option<&WhereClause>) -> CompiledWhereGuard {
    let compiled = where_clause.and_then(compile_where_clause).map(Rc::new);
    COMPILED_WHERE_STACK.with(|stack| stack.borrow_mut().push(compiled));
    CompiledWhereGuard
}

fn split_where_conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::And(left, right) = expr {
        split_where_conjuncts(left, out);
        split_where_conjuncts(right, out);
    } else {
        out.push(expr);
    }
}

/// Collect all `$param` names referenced in a [`QueryStmt`].
pub fn collect_param_names_from_query(query: &QueryStmt, out: &mut BTreeSet<String>) {
    for entry in &query.match_clauses {
        collect_param_names_from_pattern(&entry.pattern, out);
    }
    if let Some(w) = &query.where_clause {
        collect_param_names_expr(w, out);
    }
    for wc in &query.with_clauses {
        for item in &wc.items {
            collect_param_names_expr(&item.expr, out);
        }
        if let Some(w) = &wc.where_clause {
            collect_param_names_expr(w, out);
        }
        for entry in &wc.match_clauses {
            collect_param_names_from_pattern(&entry.pattern, out);
        }
        if let Some(w) = &wc.post_match_where {
            collect_param_names_expr(w, out);
        }
    }
    for item in &query.return_clause.items {
        collect_param_names_expr(&item.expr, out);
    }
    if let Some(ob) = &query.order_by {
        for item in &ob.items {
            collect_param_names_expr(&item.expr, out);
        }
    }
    if let Some(gb) = &query.group_by {
        for e in gb {
            collect_param_names_expr(e, out);
        }
    }
    if let Some(h) = &query.having {
        collect_param_names_expr(h, out);
    }
}

fn collect_param_names_from_pattern(mc: &MatchClause, out: &mut BTreeSet<String>) {
    for (_, e) in &mc.start.props_hint {
        collect_param_names_expr(e, out);
    }
    if let Some(w) = &mc.start.where_clause {
        collect_param_names_expr(w, out);
    }
    for chain in mc.hops() {
        for (_, e) in &chain.edge.properties {
            collect_param_names_expr(e, out);
        }
        if let Some(w) = &chain.edge.where_clause {
            collect_param_names_expr(w, out);
        }
        for (_, e) in &chain.node.props_hint {
            collect_param_names_expr(e, out);
        }
        if let Some(w) = &chain.node.where_clause {
            collect_param_names_expr(w, out);
        }
    }
}

fn collect_param_names_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Parameter { name, .. } => {
            out.insert(name.clone());
        }
        Expr::Literal(_) | Expr::Variable(_) | Expr::PathVar(_) => {}
        Expr::PropertyAccess { target, .. }
        | Expr::UnaryOp { expr: target, .. }
        | Expr::Not(target)
        | Expr::IsNull(target)
        | Expr::IsNotNull(target)
        | Expr::PathLength(target)
        | Expr::IsLabeled { expr: target, .. }
        | Expr::IsTruth { expr: target, .. }
        | Expr::Cast { expr: target, .. }
        | Expr::IsType { expr: target, .. }
        | Expr::IsDirected { expr: target, .. }
        | Expr::PropertyExists { target, .. } => collect_param_names_expr(target, out),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right) => {
            collect_param_names_expr(left, out);
            collect_param_names_expr(right, out);
        }
        Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
            collect_param_names_expr(node, out);
            collect_param_names_expr(edge, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_param_names_expr(expr, out);
            for e in list {
                collect_param_names_expr(e, out);
            }
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            collect_param_names_expr(expr, out);
            collect_param_names_expr(pattern, out);
        }
        Expr::Case(c) => {
            if let Some(op) = &c.operand {
                collect_param_names_expr(op, out);
            }
            for wt in &c.when_then {
                collect_param_names_expr(&wt.when, out);
                collect_param_names_expr(&wt.then, out);
            }
            if let Some(e) = &c.else_expr {
                collect_param_names_expr(e, out);
            }
        }
        Expr::Coalesce(items)
        | Expr::ListLiteral(items)
        | Expr::AllDifferent(items)
        | Expr::Same(items)
        | Expr::PathConstructor(items) => {
            for e in items {
                collect_param_names_expr(e, out);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_param_names_expr(a, out);
            }
        }
        Expr::Aggregate(a) => {
            if let Some(e) = &a.expr {
                collect_param_names_expr(e, out);
            }
        }
        Expr::RecordLiteral(pairs) => {
            for (_, e) in pairs {
                collect_param_names_expr(e, out);
            }
        }
        Expr::Exists(_) | Expr::ValueSubquery(_) => {}
        Expr::LetIn { bindings, body } => {
            for (_, e) in bindings {
                collect_param_names_expr(e, out);
            }
            collect_param_names_expr(body, out);
        }
    }
}

/// Collect all `$param` names referenced in an arbitrary [`Statement`].
///
/// Walks queries, mutations (CREATE, DELETE, SET, REMOVE, MERGE, FOR, LET, FILTER, CALL),
/// and compound statements recursively.
pub fn collect_param_names_from_stmt(stmt: &Statement, out: &mut BTreeSet<String>) {
    match stmt {
        Statement::Query(q) => collect_param_names_from_query(q, out),
        Statement::Compound { left, right, .. } => {
            collect_param_names_from_stmt(left, out);
            collect_param_names_from_stmt(right, out);
        }
        Statement::Create(cs) => {
            for c in cs {
                collect_param_names_from_create(c, out);
            }
        }
        Statement::Merge(ms) => {
            collect_param_names_from_create(&ms.create, out);
            for item in &ms.on_create_set {
                collect_param_names_from_set_item(item, out);
            }
            for item in &ms.on_match_set {
                collect_param_names_from_set_item(item, out);
            }
        }
        Statement::Delete(ds) => {
            collect_param_names_from_pattern(&ds.match_clause, out);
            if let Some(w) = &ds.where_clause {
                collect_param_names_expr(w, out);
            }
        }
        Statement::Set(ss) => {
            collect_param_names_from_pattern(&ss.match_clause, out);
            if let Some(w) = &ss.where_clause {
                collect_param_names_expr(w, out);
            }
            for item in &ss.set_clause.items {
                collect_param_names_from_set_item(item, out);
            }
        }
        Statement::Remove(rs) => {
            collect_param_names_from_pattern(&rs.match_clause, out);
            if let Some(w) = &rs.where_clause {
                collect_param_names_expr(w, out);
            }
        }
        Statement::Filter(fs) => {
            collect_param_names_from_pattern(&fs.match_clause, out);
            if let Some(w) = &fs.where_clause {
                collect_param_names_expr(w, out);
            }
            collect_param_names_expr(&fs.filter_expr, out);
        }
        Statement::Let(ls) => {
            collect_param_names_from_pattern(&ls.match_clause, out);
            if let Some(w) = &ls.where_clause {
                collect_param_names_expr(w, out);
            }
            for (_, e) in &ls.bindings {
                collect_param_names_expr(e, out);
            }
            for item in &ls.return_clause.items {
                collect_param_names_expr(&item.expr, out);
            }
        }
        Statement::For(fs) => {
            collect_param_names_expr(&fs.list_expr, out);
            for item in &fs.return_clause.items {
                collect_param_names_expr(&item.expr, out);
            }
        }
        Statement::Call(cs) => {
            collect_param_names_from_stmt(&cs.body, out);
        }
        Statement::Finish
        | Statement::UseGraph(_)
        | Statement::CreateGraph { .. }
        | Statement::DropGraph { .. }
        | Statement::CreateGraphType { .. }
        | Statement::DropGraphType { .. }
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. }
        | Statement::DescribeGraphType(_)
        | Statement::CreateIndex { .. }
        | Statement::DropIndex { .. }
        | Statement::Show(_)
        | Statement::Grant { .. }
        | Statement::Revoke { .. }
        | Statement::Analyze
        | Statement::CallProcedure(_)
        | Statement::SetTypeCheck(_)
        | Statement::CreateConstraint(_)
        | Statement::DropConstraint(_) => {}
    }
}

/// Collects prepared parameter metadata from a statement.
///
/// Parameters are considered optional when every occurrence is annotated with a
/// union type that includes `NULL`, e.g. `$x :: INT | NULL`.
pub fn collect_prepared_parameter_info_from_stmt(
    stmt: &Statement,
    out: &mut BTreeMap<String, bool>,
) {
    fn merge_param(
        out: &mut BTreeMap<String, bool>,
        name: &str,
        type_annotation: &Option<Vec<crate::ast::ValueType>>,
    ) {
        let allows_null = type_annotation
            .as_ref()
            .is_some_and(|types| types.contains(&crate::ast::ValueType::Null));
        let required = !allows_null;
        out.entry(name.to_string())
            .and_modify(|existing| *existing = *existing || required)
            .or_insert(required);
    }

    fn collect_expr(expr: &Expr, out: &mut BTreeMap<String, bool>) {
        match expr {
            Expr::Parameter {
                name,
                type_annotation,
            } => {
                merge_param(out, name, type_annotation);
            }
            Expr::BinaryOp { left, right, .. }
            | Expr::Compare { left, right, .. }
            | Expr::NullIf { left, right }
            | Expr::ListIndex {
                list: left,
                index: right,
            }
            | Expr::Concat(left, right)
            | Expr::And(left, right)
            | Expr::Or(left, right)
            | Expr::Xor(left, right) => {
                collect_expr(left, out);
                collect_expr(right, out);
            }
            Expr::UnaryOp { expr: e, .. }
            | Expr::Not(e)
            | Expr::IsNull(e)
            | Expr::IsNotNull(e)
            | Expr::PathLength(e)
            | Expr::PropertyAccess { target: e, .. }
            | Expr::IsLabeled { expr: e, .. }
            | Expr::IsTruth { expr: e, .. }
            | Expr::Cast { expr: e, .. }
            | Expr::IsType { expr: e, .. }
            | Expr::IsDirected { expr: e, .. }
            | Expr::PropertyExists { target: e, .. } => collect_expr(e, out),
            Expr::InList { expr, list, .. } => {
                collect_expr(expr, out);
                for item in list {
                    collect_expr(item, out);
                }
            }
            Expr::StringPredicate { expr, pattern, .. } => {
                collect_expr(expr, out);
                collect_expr(pattern, out);
            }
            Expr::Case(c) => {
                if let Some(op) = &c.operand {
                    collect_expr(op, out);
                }
                for wt in &c.when_then {
                    collect_expr(&wt.when, out);
                    collect_expr(&wt.then, out);
                }
                if let Some(el) = &c.else_expr {
                    collect_expr(el, out);
                }
            }
            Expr::Coalesce(items)
            | Expr::ListLiteral(items)
            | Expr::AllDifferent(items)
            | Expr::Same(items)
            | Expr::PathConstructor(items) => {
                for item in items {
                    collect_expr(item, out);
                }
            }
            Expr::Aggregate(agg) => {
                if let Some(e) = &agg.expr {
                    collect_expr(e, out);
                }
                if let Some(sep) = &agg.separator {
                    collect_expr(sep, out);
                }
            }
            Expr::FunctionCall { args, .. } => {
                for arg in args {
                    collect_expr(arg, out);
                }
            }
            Expr::RecordLiteral(pairs) => {
                for (_, e) in pairs {
                    collect_expr(e, out);
                }
            }
            Expr::LetIn { bindings, body } => {
                for (_, e) in bindings {
                    collect_expr(e, out);
                }
                collect_expr(body, out);
            }
            Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
                collect_expr(node, out);
                collect_expr(edge, out);
            }
            Expr::Exists(s) | Expr::ValueSubquery(s) => {
                collect_prepared_parameter_info_from_stmt(s, out);
            }
            Expr::Literal(_) | Expr::Variable(_) | Expr::PathVar(_) => {}
        }
    }

    fn collect_pattern(mc: &MatchClause, out: &mut BTreeMap<String, bool>) {
        for (_, e) in &mc.start.props_hint {
            collect_expr(e, out);
        }
        if let Some(w) = &mc.start.where_clause {
            collect_expr(w, out);
        }
        for pe in &mc.elements {
            if let PatternElement::Hop(c) = pe {
                for (_, e) in &c.edge.properties {
                    collect_expr(e, out);
                }
                for (_, e) in &c.node.props_hint {
                    collect_expr(e, out);
                }
            }
        }
    }

    match stmt {
        Statement::Query(q) => {
            for me in &q.match_clauses {
                collect_pattern(&me.pattern, out);
            }
            if let Some(w) = &q.where_clause {
                collect_expr(w, out);
            }
            for wc in &q.with_clauses {
                for item in &wc.items {
                    collect_expr(&item.expr, out);
                }
                if let Some(w) = &wc.where_clause {
                    collect_expr(w, out);
                }
                for me in &wc.match_clauses {
                    collect_pattern(&me.pattern, out);
                }
                if let Some(w) = &wc.post_match_where {
                    collect_expr(w, out);
                }
            }
            for item in &q.return_clause.items {
                collect_expr(&item.expr, out);
            }
            if let Some(h) = &q.having {
                collect_expr(h, out);
            }
        }
        Statement::Compound { left, right, .. } => {
            collect_prepared_parameter_info_from_stmt(left, out);
            collect_prepared_parameter_info_from_stmt(right, out);
        }
        Statement::Create(cs) => {
            for c in cs {
                match c {
                    CreateStmt::Node(nc) => {
                        for (_, e) in &nc.node.props_hint {
                            collect_expr(e, out);
                        }
                    }
                    CreateStmt::Edge(ec) => {
                        for (_, e) in &ec.left.props_hint {
                            collect_expr(e, out);
                        }
                        for (_, e) in &ec.edge.properties {
                            collect_expr(e, out);
                        }
                        for (_, e) in &ec.right.props_hint {
                            collect_expr(e, out);
                        }
                    }
                }
            }
        }
        Statement::Merge(ms) => {
            collect_prepared_parameter_info_from_stmt(
                &Statement::Create(vec![ms.create.clone()]),
                out,
            );
            for item in &ms.on_create_set {
                if let SetItem::Property { value, .. } = item {
                    collect_expr(value, out);
                }
            }
            for item in &ms.on_match_set {
                if let SetItem::Property { value, .. } = item {
                    collect_expr(value, out);
                }
            }
        }
        Statement::Delete(ds) => {
            collect_pattern(&ds.match_clause, out);
            if let Some(w) = &ds.where_clause {
                collect_expr(w, out);
            }
        }
        Statement::Set(ss) => {
            collect_pattern(&ss.match_clause, out);
            if let Some(w) = &ss.where_clause {
                collect_expr(w, out);
            }
            for item in &ss.set_clause.items {
                if let SetItem::Property { value, .. } = item {
                    collect_expr(value, out);
                }
            }
        }
        Statement::Remove(rs) => {
            collect_pattern(&rs.match_clause, out);
            if let Some(w) = &rs.where_clause {
                collect_expr(w, out);
            }
        }
        Statement::Filter(fs) => {
            collect_pattern(&fs.match_clause, out);
            if let Some(w) = &fs.where_clause {
                collect_expr(w, out);
            }
            collect_expr(&fs.filter_expr, out);
        }
        Statement::Let(ls) => {
            collect_pattern(&ls.match_clause, out);
            if let Some(w) = &ls.where_clause {
                collect_expr(w, out);
            }
            for (_, e) in &ls.bindings {
                collect_expr(e, out);
            }
            for item in &ls.return_clause.items {
                collect_expr(&item.expr, out);
            }
        }
        Statement::For(fs) => {
            collect_expr(&fs.list_expr, out);
            for item in &fs.return_clause.items {
                collect_expr(&item.expr, out);
            }
        }
        Statement::Call(cs) => collect_prepared_parameter_info_from_stmt(&cs.body, out),
        Statement::Finish
        | Statement::UseGraph(_)
        | Statement::CreateGraph { .. }
        | Statement::DropGraph { .. }
        | Statement::CreateGraphType { .. }
        | Statement::DropGraphType { .. }
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. }
        | Statement::DescribeGraphType(_)
        | Statement::CreateIndex { .. }
        | Statement::DropIndex { .. }
        | Statement::Show(_)
        | Statement::Grant { .. }
        | Statement::Revoke { .. }
        | Statement::Analyze
        | Statement::CallProcedure(_)
        | Statement::SetTypeCheck(_)
        | Statement::CreateConstraint(_)
        | Statement::DropConstraint(_) => {}
    }
}

/// Collects all function call names used in a statement's AST.
/// Used to detect `caller()` usage in prepared statements.
pub fn collect_function_calls_from_stmt(stmt: &Statement, out: &mut BTreeSet<String>) {
    // Re-use the param name walker structure, but collect function names from expressions.
    fn collect_fn_calls_expr(expr: &Expr, out: &mut BTreeSet<String>) {
        match expr {
            Expr::FunctionCall { name, args } => {
                out.insert(name.clone());
                for a in args {
                    collect_fn_calls_expr(a, out);
                }
            }
            Expr::BinaryOp { left, right, .. }
            | Expr::Compare { left, right, .. }
            | Expr::NullIf { left, right }
            | Expr::ListIndex {
                list: left,
                index: right,
            }
            | Expr::Concat(left, right)
            | Expr::And(left, right)
            | Expr::Or(left, right)
            | Expr::Xor(left, right) => {
                collect_fn_calls_expr(left, out);
                collect_fn_calls_expr(right, out);
            }
            Expr::UnaryOp { expr: e, .. }
            | Expr::Not(e)
            | Expr::IsNull(e)
            | Expr::IsNotNull(e)
            | Expr::PathLength(e)
            | Expr::PropertyAccess { target: e, .. }
            | Expr::IsLabeled { expr: e, .. }
            | Expr::IsTruth { expr: e, .. }
            | Expr::Cast { expr: e, .. }
            | Expr::IsType { expr: e, .. }
            | Expr::IsDirected { expr: e, .. }
            | Expr::PropertyExists { target: e, .. } => collect_fn_calls_expr(e, out),
            Expr::InList { expr, list, .. } => {
                collect_fn_calls_expr(expr, out);
                for item in list {
                    collect_fn_calls_expr(item, out);
                }
            }
            Expr::StringPredicate { expr, pattern, .. } => {
                collect_fn_calls_expr(expr, out);
                collect_fn_calls_expr(pattern, out);
            }
            Expr::Case(c) => {
                if let Some(op) = &c.operand {
                    collect_fn_calls_expr(op, out);
                }
                for wt in &c.when_then {
                    collect_fn_calls_expr(&wt.when, out);
                    collect_fn_calls_expr(&wt.then, out);
                }
                if let Some(el) = &c.else_expr {
                    collect_fn_calls_expr(el, out);
                }
            }
            Expr::Coalesce(items)
            | Expr::ListLiteral(items)
            | Expr::AllDifferent(items)
            | Expr::Same(items)
            | Expr::PathConstructor(items) => {
                for item in items {
                    collect_fn_calls_expr(item, out);
                }
            }
            Expr::Aggregate(agg) => {
                if let Some(e) = &agg.expr {
                    collect_fn_calls_expr(e, out);
                }
                if let Some(sep) = &agg.separator {
                    collect_fn_calls_expr(sep, out);
                }
            }
            Expr::RecordLiteral(pairs) => {
                for (_, e) in pairs {
                    collect_fn_calls_expr(e, out);
                }
            }
            Expr::LetIn { bindings, body } => {
                for (_, e) in bindings {
                    collect_fn_calls_expr(e, out);
                }
                collect_fn_calls_expr(body, out);
            }
            Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
                collect_fn_calls_expr(node, out);
                collect_fn_calls_expr(edge, out);
            }
            Expr::Exists(s) | Expr::ValueSubquery(s) => {
                collect_function_calls_from_stmt(s, out);
            }
            _ => {}
        }
    }
    fn collect_fn_calls_pattern(mc: &MatchClause, out: &mut BTreeSet<String>) {
        for (_, e) in &mc.start.props_hint {
            collect_fn_calls_expr(e, out);
        }
        if let Some(w) = &mc.start.where_clause {
            collect_fn_calls_expr(w, out);
        }
        for pe in &mc.elements {
            if let PatternElement::Hop(c) = pe {
                for (_, e) in &c.edge.properties {
                    collect_fn_calls_expr(e, out);
                }
                for (_, e) in &c.node.props_hint {
                    collect_fn_calls_expr(e, out);
                }
            }
        }
    }
    fn collect_fn_calls_return(rc: &ReturnClause, out: &mut BTreeSet<String>) {
        for item in &rc.items {
            collect_fn_calls_expr(&item.expr, out);
        }
    }

    match stmt {
        Statement::Query(q) => {
            for me in &q.match_clauses {
                collect_fn_calls_pattern(&me.pattern, out);
            }
            if let Some(w) = &q.where_clause {
                collect_fn_calls_expr(w, out);
            }
            for wc in &q.with_clauses {
                for item in &wc.items {
                    collect_fn_calls_expr(&item.expr, out);
                }
                if let Some(w) = &wc.where_clause {
                    collect_fn_calls_expr(w, out);
                }
                for me in &wc.match_clauses {
                    collect_fn_calls_pattern(&me.pattern, out);
                }
                if let Some(w) = &wc.post_match_where {
                    collect_fn_calls_expr(w, out);
                }
            }
            collect_fn_calls_return(&q.return_clause, out);
            if let Some(h) = &q.having {
                collect_fn_calls_expr(h, out);
            }
        }
        Statement::Compound { left, right, .. } => {
            collect_function_calls_from_stmt(left, out);
            collect_function_calls_from_stmt(right, out);
        }
        Statement::Create(cs) => {
            for c in cs {
                match c {
                    CreateStmt::Node(nc) => {
                        for (_, e) in &nc.node.props_hint {
                            collect_fn_calls_expr(e, out);
                        }
                    }
                    CreateStmt::Edge(ec) => {
                        for (_, e) in &ec.left.props_hint {
                            collect_fn_calls_expr(e, out);
                        }
                        for (_, e) in &ec.edge.properties {
                            collect_fn_calls_expr(e, out);
                        }
                        for (_, e) in &ec.right.props_hint {
                            collect_fn_calls_expr(e, out);
                        }
                    }
                }
            }
        }
        Statement::Merge(ms) => {
            collect_function_calls_from_stmt(&Statement::Create(vec![ms.create.clone()]), out);
            for item in &ms.on_create_set {
                if let SetItem::Property { value, .. } = item {
                    collect_fn_calls_expr(value, out);
                }
            }
            for item in &ms.on_match_set {
                if let SetItem::Property { value, .. } = item {
                    collect_fn_calls_expr(value, out);
                }
            }
        }
        Statement::Delete(ds) => {
            collect_fn_calls_pattern(&ds.match_clause, out);
            if let Some(w) = &ds.where_clause {
                collect_fn_calls_expr(w, out);
            }
        }
        Statement::Set(ss) => {
            collect_fn_calls_pattern(&ss.match_clause, out);
            if let Some(w) = &ss.where_clause {
                collect_fn_calls_expr(w, out);
            }
            for item in &ss.set_clause.items {
                if let SetItem::Property { value, .. } = item {
                    collect_fn_calls_expr(value, out);
                }
            }
        }
        Statement::Call(cs) => {
            collect_function_calls_from_stmt(&cs.body, out);
        }
        _ => {}
    }
}

fn collect_param_names_from_create(cs: &CreateStmt, out: &mut BTreeSet<String>) {
    match cs {
        CreateStmt::Node(nc) => {
            for (_, e) in &nc.node.props_hint {
                collect_param_names_expr(e, out);
            }
        }
        CreateStmt::Edge(ec) => {
            for (_, e) in &ec.left.props_hint {
                collect_param_names_expr(e, out);
            }
            for (_, e) in &ec.edge.properties {
                collect_param_names_expr(e, out);
            }
            for (_, e) in &ec.right.props_hint {
                collect_param_names_expr(e, out);
            }
        }
    }
}

fn collect_param_names_from_set_item(item: &SetItem, out: &mut BTreeSet<String>) {
    if let SetItem::Property { value, .. } = item {
        collect_param_names_expr(value, out);
    }
}

fn collect_expr_vars(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Variable(v) | Expr::PathVar(v) => {
            out.insert(v.clone());
        }
        Expr::Literal(_) | Expr::Parameter { .. } => {}
        Expr::PropertyAccess { target, .. }
        | Expr::UnaryOp { expr: target, .. }
        | Expr::Not(target)
        | Expr::IsNull(target)
        | Expr::IsNotNull(target)
        | Expr::PathLength(target)
        | Expr::IsLabeled { expr: target, .. }
        | Expr::IsTruth { expr: target, .. }
        | Expr::Cast { expr: target, .. }
        | Expr::IsType { expr: target, .. }
        | Expr::IsDirected { expr: target, .. }
        | Expr::PropertyExists { target, .. } => collect_expr_vars(target, out),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right) => {
            collect_expr_vars(left, out);
            collect_expr_vars(right, out);
        }
        Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
            collect_expr_vars(node, out);
            collect_expr_vars(edge, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_expr_vars(expr, out);
            for item in list {
                collect_expr_vars(item, out);
            }
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            collect_expr_vars(expr, out);
            collect_expr_vars(pattern, out);
        }
        Expr::Case(c) => {
            if let Some(operand) = &c.operand {
                collect_expr_vars(operand, out);
            }
            for wt in &c.when_then {
                collect_expr_vars(&wt.when, out);
                collect_expr_vars(&wt.then, out);
            }
            if let Some(else_expr) = &c.else_expr {
                collect_expr_vars(else_expr, out);
            }
        }
        Expr::Coalesce(items)
        | Expr::ListLiteral(items)
        | Expr::AllDifferent(items)
        | Expr::Same(items)
        | Expr::PathConstructor(items) => {
            for item in items {
                collect_expr_vars(item, out);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_expr_vars(arg, out);
            }
        }
        Expr::RecordLiteral(pairs) => {
            for (_, expr) in pairs {
                collect_expr_vars(expr, out);
            }
        }
        Expr::LetIn {
            bindings: lets,
            body,
        } => {
            for (_, expr) in lets {
                collect_expr_vars(expr, out);
            }
            collect_expr_vars(body, out);
        }
        Expr::Exists(_) | Expr::Aggregate(_) | Expr::ValueSubquery(_) => {}
    }
}

fn collect_required_slots(expr: &Expr, reg: &VarRegistry) -> (Vec<usize>, bool) {
    let mut vars = BTreeSet::new();
    collect_expr_vars(expr, &mut vars);
    let mut required_slots = Vec::with_capacity(vars.len());
    let mut unresolved = false;
    for var in vars {
        if let Some(slot) = reg.slot_opt(&var) {
            required_slots.push(slot);
        } else {
            unresolved = true;
        }
    }
    (required_slots, unresolved)
}

fn compile_where_operand(expr: &Expr, reg: &VarRegistry) -> Option<CompiledWhereOperand> {
    match expr {
        Expr::Literal(v) => Some(CompiledWhereOperand::Literal(v.clone())),
        Expr::Parameter { name, .. } => {
            Some(CompiledWhereOperand::Parameter { name: name.clone() })
        }
        Expr::Variable(var) | Expr::PathVar(var) => reg
            .slot_opt(var)
            .map(|slot| CompiledWhereOperand::Variable { slot }),
        Expr::PropertyAccess { target, property } => {
            let (Expr::Variable(var) | Expr::PathVar(var)) = target.as_ref() else {
                return None;
            };
            reg.slot_opt(var)
                .map(|slot| CompiledWhereOperand::PropertyAccess {
                    slot,
                    property: property.clone(),
                })
        }
        _ => None,
    }
}

fn compile_where_expr(expr: &Expr, reg: &VarRegistry) -> CompiledWhereExpr {
    match expr {
        Expr::Compare { left, op, right } => {
            if let (Some(left), Some(right)) = (
                compile_where_operand(left, reg),
                compile_where_operand(right, reg),
            ) {
                CompiledWhereExpr::Compare {
                    left,
                    op: *op,
                    right,
                }
            } else {
                CompiledWhereExpr::Fallback(expr.clone())
            }
        }
        Expr::IsNull(inner) => {
            if let Some(operand) = compile_where_operand(inner, reg) {
                CompiledWhereExpr::IsNull(operand)
            } else {
                CompiledWhereExpr::Fallback(expr.clone())
            }
        }
        Expr::IsNotNull(inner) => {
            if let Some(operand) = compile_where_operand(inner, reg) {
                CompiledWhereExpr::IsNotNull(operand)
            } else {
                CompiledWhereExpr::Fallback(expr.clone())
            }
        }
        Expr::And(left, right) => CompiledWhereExpr::And(
            Box::new(compile_where_expr(left, reg)),
            Box::new(compile_where_expr(right, reg)),
        ),
        Expr::Or(left, right) => CompiledWhereExpr::Or(
            Box::new(compile_where_expr(left, reg)),
            Box::new(compile_where_expr(right, reg)),
        ),
        Expr::Not(inner) => CompiledWhereExpr::Not(Box::new(compile_where_expr(inner, reg))),
        _ => {
            if let Some(operand) = compile_where_operand(expr, reg) {
                CompiledWhereExpr::Truthy(operand)
            } else {
                CompiledWhereExpr::Fallback(expr.clone())
            }
        }
    }
}

fn compile_where_clause(where_clause: &WhereClause) -> Option<CompiledWhereClause> {
    if !registry_is_active() {
        return None;
    }
    let reg = current_registry_rc();
    let mut conjunct_exprs = Vec::new();
    split_where_conjuncts(where_clause, &mut conjunct_exprs);
    let conjuncts = conjunct_exprs
        .into_iter()
        .map(|expr| {
            let (required_slots, has_unresolved_slots) = collect_required_slots(expr, &reg);
            CompiledWhereConjunct {
                source_expr: expr.clone(),
                required_slots,
                has_unresolved_slots,
                compiled_expr: compile_where_expr(expr, &reg),
            }
        })
        .collect::<Vec<_>>();
    Some(CompiledWhereClause { conjuncts })
}

fn compile_value_expr(expr: &Expr, reg: &VarRegistry) -> CompiledValueExpr {
    if let Some(operand) = compile_where_operand(expr, reg) {
        return CompiledValueExpr::Operand(operand);
    }
    match expr {
        Expr::UnaryOp { op, expr } => CompiledValueExpr::Unary {
            op: *op,
            expr: Box::new(compile_value_expr(expr, reg)),
        },
        Expr::BinaryOp { op, left, right } => CompiledValueExpr::Binary {
            op: *op,
            left: Box::new(compile_value_expr(left, reg)),
            right: Box::new(compile_value_expr(right, reg)),
        },
        Expr::Compare { left, op, right } => CompiledValueExpr::Compare {
            left: Box::new(compile_value_expr(left, reg)),
            op: *op,
            right: Box::new(compile_value_expr(right, reg)),
        },
        Expr::And(left, right) => CompiledValueExpr::And(
            Box::new(compile_value_expr(left, reg)),
            Box::new(compile_value_expr(right, reg)),
        ),
        Expr::Or(left, right) => CompiledValueExpr::Or(
            Box::new(compile_value_expr(left, reg)),
            Box::new(compile_value_expr(right, reg)),
        ),
        Expr::Xor(left, right) => CompiledValueExpr::Xor(
            Box::new(compile_value_expr(left, reg)),
            Box::new(compile_value_expr(right, reg)),
        ),
        Expr::Not(inner) => CompiledValueExpr::Not(Box::new(compile_value_expr(inner, reg))),
        Expr::IsNull(inner) => CompiledValueExpr::IsNull(Box::new(compile_value_expr(inner, reg))),
        Expr::IsNotNull(inner) => {
            CompiledValueExpr::IsNotNull(Box::new(compile_value_expr(inner, reg)))
        }
        Expr::Concat(left, right) => CompiledValueExpr::Concat(
            Box::new(compile_value_expr(left, reg)),
            Box::new(compile_value_expr(right, reg)),
        ),
        Expr::Coalesce(items) => CompiledValueExpr::Coalesce(
            items
                .iter()
                .map(|item| compile_value_expr(item, reg))
                .collect(),
        ),
        Expr::NullIf { left, right } => CompiledValueExpr::NullIf {
            left: Box::new(compile_value_expr(left, reg)),
            right: Box::new(compile_value_expr(right, reg)),
        },
        _ => CompiledValueExpr::Fallback(expr.clone()),
    }
}

fn compile_value_exprs<'a>(
    exprs: impl IntoIterator<Item = &'a Expr>,
) -> Option<Vec<CompiledValueExpr>> {
    if !registry_is_active() {
        return None;
    }
    let reg = current_registry_rc();
    Some(
        exprs
            .into_iter()
            .map(|expr| compile_value_expr(expr, &reg))
            .collect(),
    )
}

fn binding_value_from_slot(bindings: &Bindings, slot: usize) -> Value {
    match bindings.slots.get(slot).and_then(|entry| entry.as_ref()) {
        Some(Binding::Vertex(id)) => Value::Int64(i64::from(*id)),
        Some(Binding::Edge {
            src, dst, label, ..
        }) => Value::Text(format!(
            "{src}->{dst}:{}",
            label.as_deref().unwrap_or_default()
        )),
        Some(Binding::Value(v)) => v.clone(),
        None => Value::Null,
    }
}

fn binding_property_value_from_slot<M: Memory>(
    bindings: &Bindings,
    slot: usize,
    property: &str,
    graph: &PmaGraph<M>,
) -> Value {
    match bindings.slots.get(slot).and_then(|entry| entry.as_ref()) {
        Some(Binding::Vertex(id)) => vertex_property(*id, property, graph),
        Some(Binding::Edge {
            src,
            dst,
            label,
            weight,
            timestamp,
            ..
        }) => edge_property(
            *src,
            *dst,
            label.as_deref(),
            property,
            *weight,
            *timestamp,
            graph,
        ),
        Some(Binding::Value(v)) => record_property_lookup(v, property),
        None => Value::Null,
    }
}

fn eval_compiled_where_operand<M: Memory>(
    operand: &CompiledWhereOperand,
    bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> Value {
    match operand {
        CompiledWhereOperand::Literal(v) => v.clone(),
        CompiledWhereOperand::Parameter { name } => QUERY_PARAMS
            .with(|p| p.borrow().get(name.as_str()).cloned())
            .unwrap_or(Value::Null),
        CompiledWhereOperand::Variable { slot } => binding_value_from_slot(bindings, *slot),
        CompiledWhereOperand::PropertyAccess { slot, property } => {
            binding_property_value_from_slot(bindings, *slot, property, graph)
        }
    }
}

fn eval_compiled_value_expr<M: Memory>(
    expr: &CompiledValueExpr,
    bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> Value {
    match expr {
        CompiledValueExpr::Operand(operand) => {
            eval_compiled_where_operand(operand, bindings, graph)
        }
        CompiledValueExpr::Unary { op, expr } => {
            eval_unary_op(*op, &eval_compiled_value_expr(expr, bindings, graph))
        }
        CompiledValueExpr::Binary { op, left, right } => eval_binary_op(
            *op,
            &eval_compiled_value_expr(left, bindings, graph),
            &eval_compiled_value_expr(right, bindings, graph),
        ),
        CompiledValueExpr::Compare { left, op, right } => {
            let l = eval_compiled_value_expr(left, bindings, graph);
            let r = eval_compiled_value_expr(right, bindings, graph);
            Value::Bool(compare_cmp(*op, &l, &r))
        }
        CompiledValueExpr::And(left, right) => Value::Bool(
            truthy(&eval_compiled_value_expr(left, bindings, graph))
                && truthy(&eval_compiled_value_expr(right, bindings, graph)),
        ),
        CompiledValueExpr::Or(left, right) => Value::Bool(
            truthy(&eval_compiled_value_expr(left, bindings, graph))
                || truthy(&eval_compiled_value_expr(right, bindings, graph)),
        ),
        CompiledValueExpr::Xor(left, right) => Value::Bool(
            truthy(&eval_compiled_value_expr(left, bindings, graph))
                ^ truthy(&eval_compiled_value_expr(right, bindings, graph)),
        ),
        CompiledValueExpr::Not(inner) => {
            Value::Bool(!truthy(&eval_compiled_value_expr(inner, bindings, graph)))
        }
        CompiledValueExpr::IsNull(inner) => Value::Bool(matches!(
            eval_compiled_value_expr(inner, bindings, graph),
            Value::Null
        )),
        CompiledValueExpr::IsNotNull(inner) => Value::Bool(!matches!(
            eval_compiled_value_expr(inner, bindings, graph),
            Value::Null
        )),
        CompiledValueExpr::Concat(left, right) => match (
            eval_compiled_value_expr(left, bindings, graph),
            eval_compiled_value_expr(right, bindings, graph),
        ) {
            (Value::Text(a), Value::Text(b)) => Value::Text(a + &b),
            _ => Value::Null,
        },
        CompiledValueExpr::Coalesce(items) => items
            .iter()
            .map(|item| eval_compiled_value_expr(item, bindings, graph))
            .find(|value| !matches!(value, Value::Null))
            .unwrap_or(Value::Null),
        CompiledValueExpr::NullIf { left, right } => {
            let l = eval_compiled_value_expr(left, bindings, graph);
            let r = eval_compiled_value_expr(right, bindings, graph);
            if compare_values(&l, &r) == Some(Ordering::Equal) {
                Value::Null
            } else {
                l
            }
        }
        CompiledValueExpr::Fallback(expr) => eval_expr(expr, bindings, graph),
    }
}

fn eval_compiled_where_expr<M: Memory>(
    expr: &CompiledWhereExpr,
    bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> bool {
    match expr {
        CompiledWhereExpr::Compare { left, op, right } => {
            let l = eval_compiled_where_operand(left, bindings, graph);
            let r = eval_compiled_where_operand(right, bindings, graph);
            compare_cmp(*op, &l, &r)
        }
        CompiledWhereExpr::IsNull(inner) => {
            matches!(
                eval_compiled_where_operand(inner, bindings, graph),
                Value::Null
            )
        }
        CompiledWhereExpr::IsNotNull(inner) => !matches!(
            eval_compiled_where_operand(inner, bindings, graph),
            Value::Null
        ),
        CompiledWhereExpr::Truthy(inner) => {
            truthy(&eval_compiled_where_operand(inner, bindings, graph))
        }
        CompiledWhereExpr::And(left, right) => {
            eval_compiled_where_expr(left, bindings, graph)
                && eval_compiled_where_expr(right, bindings, graph)
        }
        CompiledWhereExpr::Or(left, right) => {
            eval_compiled_where_expr(left, bindings, graph)
                || eval_compiled_where_expr(right, bindings, graph)
        }
        CompiledWhereExpr::Not(inner) => !eval_compiled_where_expr(inner, bindings, graph),
        CompiledWhereExpr::Fallback(expr) => truthy(&eval_expr(expr, bindings, graph)),
    }
}

fn eval_where_compiled<M: Memory>(
    compiled: &CompiledWhereClause,
    bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> bool {
    compiled
        .conjuncts
        .iter()
        .all(|conj| eval_compiled_where_expr(&conj.compiled_expr, bindings, graph))
}

fn eval_where_partial_compiled<M: Memory>(
    compiled: &CompiledWhereClause,
    bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> bool {
    for conjunct in &compiled.conjuncts {
        let ready = if conjunct.has_unresolved_slots {
            all_expr_vars_bound(&conjunct.source_expr, bindings)
        } else {
            conjunct.required_slots.iter().all(|slot| {
                bindings
                    .slots
                    .get(*slot)
                    .is_some_and(|entry| entry.is_some())
            })
        };
        if !ready {
            continue;
        }
        if !eval_compiled_where_expr(&conjunct.compiled_expr, bindings, graph) {
            return false;
        }
    }
    true
}

fn eval_where<M: Memory>(w: &WhereClause, bindings: &Bindings, graph: &PmaGraph<M>) -> bool {
    with_current_compiled_where(|compiled| {
        if let Some(compiled) = compiled {
            eval_where_compiled(compiled, bindings, graph)
        } else {
            truthy(&eval_expr(w, bindings, graph))
        }
    })
}

fn eval_where_partial_pushdown<M: Memory>(
    where_clause: Option<&WhereClause>,
    bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> bool {
    let Some(where_clause) = where_clause else {
        return true;
    };
    with_current_compiled_where(|compiled| {
        if let Some(compiled) = compiled {
            eval_where_partial_compiled(compiled, bindings, graph)
        } else {
            eval_where_partial(where_clause, bindings, graph)
        }
    })
}

/// Applies inline WHERE conditions from all node/edge patterns in a match clause.
///
/// Called after `extend_match` produces a full binding row. Evaluates any inline
/// `WHERE` predicate found in `(n WHERE expr)` or `[e WHERE expr]` patterns against
/// the complete row bindings.
fn apply_pattern_inline_where<M: Memory>(
    m: &crate::ast::MatchClause,
    rows: &mut Vec<Bindings>,
    graph: &PmaGraph<M>,
) {
    // Collect all inline WHERE expressions from patterns (start node + chains).
    let mut conditions: Vec<&crate::ast::Expr> = Vec::new();
    if let Some(w) = m.start.where_clause.as_deref() {
        conditions.push(w);
    }
    for chain in m.hops() {
        if let Some(w) = chain.edge.where_clause.as_deref() {
            conditions.push(w);
        }
        if let Some(w) = chain.node.where_clause.as_deref() {
            conditions.push(w);
        }
    }
    if !conditions.is_empty() {
        rows.retain(|b| conditions.iter().all(|w| truthy(&eval_expr(w, b, graph))));
    }
}

/// Partial WHERE evaluator for predicate pushdown.
///
/// Evaluates any AND-conjuncts whose referenced variables are all present in `bindings`.
/// Conjuncts with unbound variables are treated as `true` (not yet applicable) and
/// will be re-evaluated when all variables are bound at the final stage.
///
/// Returns `false` iff at least one fully-bound conjunct evaluates to false.
/// This is safe: it never prunes rows that the full WHERE would pass, and may prune
/// rows early when conjuncts are satisfied at the current partial binding stage.
fn eval_where_partial<M: Memory>(
    w: &WhereClause,
    bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> bool {
    match w {
        Expr::And(left, right) => {
            eval_where_partial(left, bindings, graph) && eval_where_partial(right, bindings, graph)
        }
        expr => {
            if all_expr_vars_bound(expr, bindings) {
                truthy(&eval_expr(expr, bindings, graph))
            } else {
                true // Not yet evaluable; defer to final stage
            }
        }
    }
}

/// Returns `true` iff every variable/path-var referenced in `expr` is present in `bindings`.
fn all_expr_vars_bound(expr: &Expr, bindings: &Bindings) -> bool {
    match expr {
        Expr::Variable(v) | Expr::PathVar(v) => bindings.contains_key(v),
        Expr::Literal(_) | Expr::Parameter { .. } => true,
        Expr::PropertyAccess { target, .. }
        | Expr::UnaryOp { expr: target, .. }
        | Expr::Not(target)
        | Expr::IsNull(target)
        | Expr::IsNotNull(target)
        | Expr::PathLength(target)
        | Expr::IsLabeled { expr: target, .. }
        | Expr::IsTruth { expr: target, .. }
        | Expr::Cast { expr: target, .. }
        | Expr::IsType { expr: target, .. }
        | Expr::IsDirected { expr: target, .. }
        | Expr::PropertyExists { target, .. } => all_expr_vars_bound(target, bindings),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right) => {
            all_expr_vars_bound(left, bindings) && all_expr_vars_bound(right, bindings)
        }
        Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
            all_expr_vars_bound(node, bindings) && all_expr_vars_bound(edge, bindings)
        }
        Expr::InList { expr, list, .. } => {
            all_expr_vars_bound(expr, bindings)
                && list.iter().all(|e| all_expr_vars_bound(e, bindings))
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            all_expr_vars_bound(expr, bindings) && all_expr_vars_bound(pattern, bindings)
        }
        Expr::Case(c) => {
            c.operand
                .as_ref()
                .is_none_or(|e| all_expr_vars_bound(e, bindings))
                && c.when_then.iter().all(|wt| {
                    all_expr_vars_bound(&wt.when, bindings)
                        && all_expr_vars_bound(&wt.then, bindings)
                })
                && c.else_expr
                    .as_ref()
                    .is_none_or(|e| all_expr_vars_bound(e, bindings))
        }
        Expr::Coalesce(items)
        | Expr::ListLiteral(items)
        | Expr::AllDifferent(items)
        | Expr::Same(items)
        | Expr::PathConstructor(items) => items.iter().all(|e| all_expr_vars_bound(e, bindings)),
        Expr::FunctionCall { args, .. } => args.iter().all(|e| all_expr_vars_bound(e, bindings)),
        Expr::RecordLiteral(pairs) => pairs.iter().all(|(_, e)| all_expr_vars_bound(e, bindings)),
        Expr::LetIn {
            bindings: lets,
            body,
        } => {
            lets.iter().all(|(_, e)| all_expr_vars_bound(e, bindings))
                && all_expr_vars_bound(body, bindings)
        }
        // Subqueries and aggregates create inner scope — treat as non-pushable.
        Expr::Exists(_) | Expr::Aggregate(_) | Expr::ValueSubquery(_) => false,
    }
}

fn query_has_aggregate(q: &QueryStmt) -> bool {
    q.return_clause
        .items
        .iter()
        .any(|i| expr_contains_aggregate(&i.expr))
        || q.order_by
            .as_ref()
            .is_some_and(|o| o.items.iter().any(|i| expr_contains_aggregate(&i.expr)))
}

fn with_clause_has_aggregate(w: &crate::ast::WithClause) -> bool {
    w.items.iter().any(|i| expr_contains_aggregate(&i.expr))
        || w.where_clause.as_ref().is_some_and(expr_contains_aggregate)
        || w.order_by
            .as_ref()
            .is_some_and(|o| o.items.iter().any(|i| expr_contains_aggregate(&i.expr)))
}

fn with_item_name(item: &ReturnItem) -> Result<String, GleaphError> {
    if let Some(alias) = &item.alias {
        return Ok(alias.clone());
    }
    match &item.expr {
        Expr::Variable(v) => Ok(v.clone()),
        Expr::PropertyAccess { target, property } => {
            if let Expr::Variable(v) = target.as_ref() {
                Ok(format!("{v}.{property}"))
            } else {
                Ok(property.clone())
            }
        }
        _ => Err(GleaphError::ValidationError(
            "WITH expressions must use AS alias unless they are simple variables or property accesses".into(),
        )),
    }
}

fn project_with_row<M: Memory>(
    w: &crate::ast::WithClause,
    bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> Result<Bindings, GleaphError> {
    if w.star {
        return Ok(bindings.clone());
    }
    let mut out = Bindings::new();
    for item in &w.items {
        let name = with_item_name(item)?;
        let binding = match &item.expr {
            Expr::Variable(v) => bindings
                .get(v)
                .cloned()
                .map_or_else(|| Binding::Value(Value::Null), |b| b),
            _ => Binding::Value(eval_expr(&item.expr, bindings, graph)),
        };
        out.insert(name, binding);
    }
    Ok(out)
}

fn project_with_aggregated_rows<M: Memory>(
    w: &crate::ast::WithClause,
    rows: &[Bindings],
    graph: &PmaGraph<M>,
    stats: &mut QueryStats,
) -> Result<Vec<Bindings>, GleaphError> {
    let explicit_group_by: Option<&[Expr]> = None;
    let non_agg_indices = w
        .items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| (!expr_contains_aggregate(&item.expr)).then_some(idx))
        .collect::<Vec<_>>();
    let build_hasher = RandomState::new();
    let mut groups: Vec<(Vec<Value>, Vec<&Bindings>)> = Vec::new();
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    if rows.is_empty() {
        if non_agg_indices.is_empty() {
            let empty: Vec<&Bindings> = Vec::new();
            let mut out = Bindings::new();
            for item in &w.items {
                let name = with_item_name(item)?;
                out.insert(
                    name,
                    Binding::Value(eval_group_expr(&item.expr, None, &empty, graph)),
                );
            }
            return Ok(vec![out]);
        }
        return Ok(Vec::new());
    }
    for row in rows {
        let key_values = if let Some(group_by_exprs) = explicit_group_by {
            group_by_exprs
                .iter()
                .map(|e| eval_expr(e, row, graph))
                .collect::<Vec<_>>()
        } else {
            non_agg_indices
                .iter()
                .map(|idx| eval_expr(&w.items[*idx].expr, row, graph))
                .collect::<Vec<_>>()
        };
        let h = hash_value_slice(&key_values, &build_hasher);
        let max_groups = effective_max_groups();
        let bucket = group_index.entry(h).or_default();
        let mut found_idx = None;
        for &idx in bucket.iter() {
            if groups[idx].0 == key_values {
                found_idx = Some(idx);
                break;
            }
        }
        if let Some(idx) = found_idx {
            groups[idx].1.push(row);
        } else {
            if groups.len() >= max_groups {
                return Err(GleaphError::ExecutionError(format!(
                    "MAX_GROUPS exceeded ({max_groups})"
                )));
            }
            let idx = groups.len();
            bucket.push(idx);
            groups.push((key_values, vec![row]));
        }
    }
    stats.breakdown.groups_formed = stats
        .breakdown
        .groups_formed
        .saturating_add(groups.len() as u64);
    let mut out_rows = Vec::new();
    for (_key_values, group_rows) in groups {
        let rep = group_rows.first().copied();
        let mut out = Bindings::new();
        for item in &w.items {
            let name = with_item_name(item)?;
            out.insert(
                name,
                Binding::Value(eval_group_expr(&item.expr, rep, &group_rows, graph)),
            );
        }
        out_rows.push(out);
    }
    Ok(out_rows)
}

fn sort_binding_rows_for_with_aggregate<M: Memory>(
    w: &crate::ast::WithClause,
    order_by: &crate::ast::OrderBy,
    rows: &mut [Bindings],
    graph: &PmaGraph<M>,
) -> Result<(), GleaphError> {
    // Pre-compute sort keys per row so the closure stays pure.
    let sort_keys: Vec<Vec<Value>> = rows
        .iter()
        .map(|row| with_aggregate_order_keys_for_row(w, order_by, row, graph))
        .collect::<Result<_, _>>()?;

    // Sort using pre-computed keys via index array to avoid borrow conflicts.
    let mut indices: Vec<usize> = (0..rows.len()).collect();
    indices.sort_by(|&ai, &bi| compare_order_keys(order_by, &sort_keys[ai], &sort_keys[bi]));
    // Apply permutation in-place.
    let orig: Vec<Bindings> = rows.to_vec();
    for (dst, src) in indices.iter().enumerate() {
        rows[dst] = orig[*src].clone();
    }
    Ok(())
}

fn with_aggregate_order_keys_for_row<M: Memory>(
    w: &crate::ast::WithClause,
    order_by: &crate::ast::OrderBy,
    row: &Bindings,
    graph: &PmaGraph<M>,
) -> Result<Vec<Value>, GleaphError> {
    order_by
        .items
        .iter()
        .map(|item| {
            // Fast path: find a WITH item whose name matches this ORDER BY expression.
            let matched_name = w.items.iter().find_map(|ret| {
                if ret.expr == item.expr {
                    with_item_name(ret).ok()
                } else {
                    match (&item.expr, &ret.alias) {
                        (Expr::Variable(v), Some(alias)) if v.eq_ignore_ascii_case(alias) => {
                            Some(alias.clone())
                        }
                        _ => None,
                    }
                }
            });
            if let Some(name) = matched_name {
                return Ok(binding_value(&name, row));
            }
            // Fallback: evaluate the expression directly against the projected bindings.
            // Handles e.g. `ORDER BY cnt + 1` when `WITH count(n) AS cnt`.
            Ok(eval_expr(&item.expr, row, graph))
        })
        .collect::<Result<Vec<_>, _>>()
}

fn top_k_binding_rows_for_with_aggregate<M: Memory>(
    w: &crate::ast::WithClause,
    order_by: &crate::ast::OrderBy,
    rows: Vec<Bindings>,
    k: usize,
    graph: &PmaGraph<M>,
) -> Result<Vec<Bindings>, GleaphError> {
    if k == 0 {
        return Ok(Vec::new());
    }

    let mut best: Vec<(usize, Vec<Value>, Bindings)> = Vec::new(); // sorted in final ORDER BY order
    for (idx, row) in rows.into_iter().enumerate() {
        let row_keys = with_aggregate_order_keys_for_row(w, order_by, &row, graph)?;
        let cmp_existing_with_candidate = |existing: &(usize, Vec<Value>, Bindings)| {
            compare_order_keys(order_by, &existing.1, &row_keys).then_with(|| existing.0.cmp(&idx))
        };
        let candidate_vs_existing = |existing: &(usize, Vec<Value>, Bindings)| {
            compare_order_keys(order_by, &row_keys, &existing.1).then_with(|| idx.cmp(&existing.0))
        };
        let insert_pos = || {
            best.iter()
                .position(|probe| cmp_existing_with_candidate(probe) != Ordering::Less)
                .unwrap_or(best.len())
        };

        if best.len() < k {
            let pos = insert_pos();
            best.insert(pos, (idx, row_keys, row));
            continue;
        }

        // `best` is sorted best->worst, so skip rows that are not better than the current worst.
        if candidate_vs_existing(best.last().expect("non-empty")) != Ordering::Less {
            continue;
        }

        let pos = insert_pos();
        best.insert(pos, (idx, row_keys, row));
        best.pop();
    }

    Ok(best.into_iter().map(|(_, _, row)| row).collect())
}

fn expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(_) => true,
        Expr::PropertyAccess { target, .. }
        | Expr::UnaryOp { expr: target, .. }
        | Expr::Not(target)
        | Expr::IsNull(target)
        | Expr::IsNotNull(target)
        | Expr::PathLength(target) => expr_contains_aggregate(target),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right) => expr_contains_aggregate(left) || expr_contains_aggregate(right),
        Expr::InList { expr, list, .. } => {
            expr_contains_aggregate(expr) || list.iter().any(expr_contains_aggregate)
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            expr_contains_aggregate(expr) || expr_contains_aggregate(pattern)
        }
        Expr::Case(case_expr) => {
            case_expr
                .operand
                .as_ref()
                .is_some_and(|e| expr_contains_aggregate(e))
                || case_expr.when_then.iter().any(|wt| {
                    expr_contains_aggregate(&wt.when) || expr_contains_aggregate(&wt.then)
                })
                || case_expr
                    .else_expr
                    .as_ref()
                    .is_some_and(|e| expr_contains_aggregate(e))
        }
        Expr::Coalesce(items) | Expr::ListLiteral(items) => {
            items.iter().any(expr_contains_aggregate)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_contains_aggregate),
        Expr::Exists(_)
        | Expr::Literal(_)
        | Expr::Variable(_)
        | Expr::PathVar(_)
        | Expr::Parameter { .. } => false,
        Expr::Cast { expr, .. } | Expr::IsTruth { expr, .. } | Expr::IsLabeled { expr, .. } => {
            expr_contains_aggregate(expr)
        }
        Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
            expr_contains_aggregate(node) || expr_contains_aggregate(edge)
        }
        Expr::AllDifferent(exprs) | Expr::Same(exprs) => exprs.iter().any(expr_contains_aggregate),
        Expr::PropertyExists { target, .. } => expr_contains_aggregate(target),
        Expr::RecordLiteral(pairs) => pairs.iter().any(|(_, e)| expr_contains_aggregate(e)),
        Expr::IsType { expr, .. } | Expr::IsDirected { expr, .. } => expr_contains_aggregate(expr),
        Expr::ValueSubquery(_) => false,
        Expr::LetIn { bindings, body } => {
            bindings.iter().any(|(_, e)| expr_contains_aggregate(e))
                || expr_contains_aggregate(body)
        }
        Expr::PathConstructor(elems) => elems.iter().any(expr_contains_aggregate),
    }
}

/// Compute a deterministic `u64` hash from a slice of `Value`s.
/// Generic over [`BuildHasher`] so callers can supply a faster implementation
/// (e.g. `rapidhash::fast::RandomState`).
fn hash_value_slice<S: BuildHasher>(values: &[Value], build_hasher: &S) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = build_hasher.build_hasher();
    values.len().hash(&mut hasher);
    for v in values {
        hash_value(v, &mut hasher);
    }
    hasher.finish()
}

fn hash_value(v: &Value, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    std::mem::discriminant(v).hash(h);
    match v {
        Value::Null => {}
        Value::Bool(b) => b.hash(h),
        Value::Int8(i) => i.hash(h),
        Value::Int16(i) => i.hash(h),
        Value::Int32(i) => i.hash(h),
        Value::Int64(i) => i.hash(h),
        Value::Int128(i) => i.hash(h),
        Value::Int256(i) => {
            let bytes = i.0.to_le_bytes();
            bytes.hash(h);
        }
        Value::Uint8(u) => u.hash(h),
        Value::Uint16(u) => u.hash(h),
        Value::Uint32(u) => u.hash(h),
        Value::Uint64(u) => u.hash(h),
        Value::Uint128(u) => u.hash(h),
        Value::Uint256(u) => {
            let bytes = u.0.to_le_bytes();
            bytes.hash(h);
        }
        Value::Float32(f) => f.to_bits().hash(h),
        Value::Float64(f) => f.to_bits().hash(h),
        Value::Text(s) => s.hash(h),
        Value::Timestamp(t) => t.hash(h),
        Value::List(items) => {
            items.len().hash(h);
            for item in items {
                hash_value(item, h);
            }
        }
        Value::Bytes(b) => b.hash(h),
        Value::Date(d) => d.hash(h),
        Value::Time(t) => t.hash(h),
        Value::DateTime(s, n) => {
            s.hash(h);
            n.hash(h);
        }
        Value::Duration(m, n) => {
            m.hash(h);
            n.hash(h);
        }
        Value::Principal(p) => p.as_slice().hash(h),
        Value::Decimal(d) => d.normalize().0.hash(h),
        Value::Path(elems) => {
            elems.len().hash(h);
            for e in elems {
                match e {
                    PathElement::Node(id) => {
                        0u8.hash(h);
                        id.hash(h);
                    }
                    PathElement::Edge { src, dst, label } => {
                        1u8.hash(h);
                        src.hash(h);
                        dst.hash(h);
                        label.hash(h);
                    }
                }
            }
        }
    }
}

fn is_fast_path_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(agg) => {
            matches!(
                agg.func,
                AggFunc::Count
                    | AggFunc::Sum
                    | AggFunc::Avg
                    | AggFunc::Min
                    | AggFunc::Max
                    | AggFunc::Collect
                    | AggFunc::StringAgg
                    | AggFunc::PercentileCont
                    | AggFunc::PercentileDisc
            )
        }
        _ => false,
    }
}

fn is_simple_fast_path_aggregate(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Aggregate(agg) if matches!(agg.func,
            AggFunc::Count
                | AggFunc::Sum
                | AggFunc::Avg
                | AggFunc::Min
                | AggFunc::Max
                | AggFunc::Collect
                | AggFunc::StringAgg
                | AggFunc::PercentileCont
                | AggFunc::PercentileDisc
        )
    )
}

fn can_use_projection_fast_path(q: &QueryStmt) -> bool {
    // Accept all supported aggregate functions (including DISTINCT).
    q.having.is_none()
        && q.return_clause
            .items
            .iter()
            .any(|item| expr_contains_aggregate(&item.expr))
        && q.return_clause.items.iter().all(|item| {
            if expr_contains_aggregate(&item.expr) {
                is_simple_fast_path_aggregate(&item.expr)
            } else {
                true
            }
        })
}

fn project_aggregated_rows_fast<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    rows: &[Bindings],
    graph: &PmaGraph<M>,
    build_hasher: &S,
    mut stats: Option<&mut QueryStats>,
) -> Result<Vec<Vec<Value>>, GleaphError> {
    if let Some(stats) = stats.as_deref_mut() {
        stats.breakdown.compiled_projection_fast_calls = stats
            .breakdown
            .compiled_projection_fast_calls
            .saturating_add(1);
        stats.breakdown.compiled_projection_input_rows = stats
            .breakdown
            .compiled_projection_input_rows
            .saturating_add(rows.len() as u64);
    }
    let explicit_group_by = q.group_by.as_deref();
    let non_agg_indices = q
        .return_clause
        .items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| (!expr_contains_aggregate(&item.expr)).then_some(idx))
        .collect::<Vec<_>>();

    // Describe each return item: for aggregates, record (kind, operand expr, distinct, param).
    // agg_slots collects (return_item_index, kind, distinct, param) for aggregate items only.
    #[allow(clippy::type_complexity)]
    let agg_descs: Vec<Option<(AggAccumKind, Option<&Expr>, bool, Option<Value>)>> = q
        .return_clause
        .items
        .iter()
        .map(|item| {
            if let Expr::Aggregate(agg) = &item.expr {
                let kind = AggAccumKind::from(agg.func);
                let operand = if agg.count_all {
                    None
                } else {
                    agg.expr.as_deref()
                };
                let param = agg.separator.as_ref().and_then(|s| {
                    if rows.is_empty() {
                        None
                    } else {
                        Some(eval_expr(s, &rows[0], graph))
                    }
                });
                Some((kind, operand, agg.distinct, param))
            } else {
                None
            }
        })
        .collect();

    let agg_slots: Vec<(usize, AggAccumKind, bool, Option<Value>)> = agg_descs
        .iter()
        .enumerate()
        .filter_map(|(i, desc)| {
            desc.as_ref()
                .map(|(kind, _, distinct, param)| (i, *kind, *distinct, param.clone()))
        })
        .collect();

    // Mapping: return item index → position in group accumulator vec.
    let mut agg_idx_map: Vec<Option<usize>> = vec![None; agg_descs.len()];
    for (pos, (ret_idx, _, _, _)) in agg_slots.iter().enumerate() {
        agg_idx_map[*ret_idx] = Some(pos);
    }

    if rows.is_empty() {
        if let Some(stats) = stats.as_deref_mut() {
            stats.breakdown.aggregate_compiled_fast_path_used = true;
            stats.breakdown.compiled_projection_empty_returns = stats
                .breakdown
                .compiled_projection_empty_returns
                .saturating_add(1);
        }
        let has_group_keys = explicit_group_by.is_some() || !non_agg_indices.is_empty();
        if !has_group_keys {
            let row = agg_descs
                .iter()
                .map(|desc| {
                    if let Some((kind, _, _, _)) = desc {
                        AggAccum::default_empty(*kind)
                    } else {
                        Value::Null
                    }
                })
                .collect::<Vec<_>>();
            return Ok(vec![row]);
        }
        return Ok(Vec::new());
    }

    let new_accums = || -> Vec<AggAccum> {
        agg_slots
            .iter()
            .map(|(_, kind, distinct, param)| {
                if *distinct {
                    AggAccum::new_distinct_parameterized(*kind, param.as_ref())
                } else {
                    AggAccum::new_parameterized(*kind, param.as_ref())
                }
            })
            .collect()
    };

    // Group state: (group_key_values, representative_row, accumulators).
    let mut groups: Vec<(Vec<Value>, &Bindings, Vec<AggAccum>)> = Vec::new();
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    let mut compiled_group_key_evals: u64 = 0;
    let mut compiled_group_bucket_probes: u64 = 0;
    let mut compiled_agg_updates: u64 = 0;

    for row in rows {
        let key_values = if let Some(group_by_exprs) = explicit_group_by {
            group_by_exprs
                .iter()
                .map(|e| {
                    compiled_group_key_evals = compiled_group_key_evals.saturating_add(1);
                    eval_expr(e, row, graph)
                })
                .collect::<Vec<_>>()
        } else {
            non_agg_indices
                .iter()
                .map(|idx| {
                    compiled_group_key_evals = compiled_group_key_evals.saturating_add(1);
                    eval_expr(&q.return_clause.items[*idx].expr, row, graph)
                })
                .collect::<Vec<_>>()
        };
        let h = hash_value_slice(&key_values, build_hasher);
        let bucket = group_index.entry(h).or_default();
        let mut found_idx: Option<usize> = None;
        for &idx in bucket.iter() {
            compiled_group_bucket_probes = compiled_group_bucket_probes.saturating_add(1);
            if groups[idx].0 == key_values {
                found_idx = Some(idx);
                break;
            }
        }
        let group_idx = if let Some(idx) = found_idx {
            idx
        } else {
            let max_groups = effective_max_groups();
            if groups.len() >= max_groups {
                return Err(GleaphError::ExecutionError(format!(
                    "MAX_GROUPS exceeded ({max_groups})"
                )));
            }
            let idx = groups.len();
            bucket.push(idx);
            groups.push((key_values, row, new_accums()));
            idx
        };

        // Accumulate each aggregate for this row.
        for (pos, (ret_idx, _, _, _)) in agg_slots.iter().enumerate() {
            let (_, operand, _, _) = agg_descs[*ret_idx].as_ref().unwrap();
            if let Some(operand_expr) = operand {
                let val = eval_expr(operand_expr, row, graph);
                groups[group_idx].2[pos].accumulate(&val);
                compiled_agg_updates = compiled_agg_updates.saturating_add(1);
            } else {
                // COUNT(*) — always count (pass non-NULL to trigger increment).
                groups[group_idx].2[pos].accumulate(&Value::Int64(1));
                compiled_agg_updates = compiled_agg_updates.saturating_add(1);
            }
        }
    }

    if let Some(stats) = stats {
        stats.breakdown.aggregate_compiled_fast_path_used = true;
        stats.breakdown.compiled_group_key_evals = stats
            .breakdown
            .compiled_group_key_evals
            .saturating_add(compiled_group_key_evals);
        stats.breakdown.compiled_group_bucket_probes = stats
            .breakdown
            .compiled_group_bucket_probes
            .saturating_add(compiled_group_bucket_probes);
        stats.breakdown.compiled_agg_updates = stats
            .breakdown
            .compiled_agg_updates
            .saturating_add(compiled_agg_updates);
        stats.breakdown.groups_formed = stats
            .breakdown
            .groups_formed
            .saturating_add(groups.len() as u64);
    }

    // Aggregation + LIMIT pushdown: when LIMIT is present without ORDER BY or HAVING,
    // we can stop emitting groups early once we have enough output rows.
    let agg_limit = if q.limit.is_some() && q.order_by.is_none() && q.having.is_none() {
        q.limit.map(|l| l.0 as usize)
    } else {
        None
    };

    let mut result = Vec::new();
    for (_key_values, rep, accums) in groups {
        if agg_limit.is_some_and(|k| result.len() >= k) {
            break;
        }
        let finalized: Vec<Value> = accums.into_iter().map(|a| a.finalize()).collect();
        result.push(
            q.return_clause
                .items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    if let Some(pos) = agg_idx_map[i] {
                        finalized[pos].clone()
                    } else {
                        eval_expr(&item.expr, rep, graph)
                    }
                })
                .collect::<Vec<_>>(),
        );
    }
    Ok(result)
}

fn project_aggregated_rows<M: Memory, S: BuildHasher>(
    q: &QueryStmt,
    rows: &[Bindings],
    graph: &PmaGraph<M>,
    build_hasher: &S,
    stats: Option<&mut QueryStats>,
) -> Result<Vec<Vec<Value>>, GleaphError> {
    if can_use_projection_fast_path(q) {
        return project_aggregated_rows_fast(q, rows, graph, build_hasher, stats);
    }
    let explicit_group_by = q.group_by.as_deref();
    let non_agg_indices = q
        .return_clause
        .items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| (!expr_contains_aggregate(&item.expr)).then_some(idx))
        .collect::<Vec<_>>();

    let mut groups: Vec<(Vec<Value>, Vec<&Bindings>)> = Vec::new();
    // Map from value_hash → index into `groups` for fast lookup.
    let mut group_index: RapidHashMap<u64, Vec<usize>> = RapidHashMap::default();
    if rows.is_empty() {
        let has_group_keys = explicit_group_by.is_some() || !non_agg_indices.is_empty();
        if !has_group_keys {
            let empty_group: Vec<&Bindings> = Vec::new();
            let row = q
                .return_clause
                .items
                .iter()
                .map(|item| eval_group_expr(&item.expr, None, &empty_group, graph))
                .collect::<Vec<_>>();
            let keep = q
                .having
                .as_ref()
                .map(|h| truthy(&eval_group_expr(h, None, &empty_group, graph)))
                .unwrap_or(true);
            return Ok(if keep { vec![row] } else { Vec::new() });
        }
        return Ok(Vec::new());
    }

    for row in rows {
        let key_values = if let Some(group_by_exprs) = explicit_group_by {
            group_by_exprs
                .iter()
                .map(|e| eval_expr(e, row, graph))
                .collect::<Vec<_>>()
        } else {
            non_agg_indices
                .iter()
                .map(|idx| eval_expr(&q.return_clause.items[*idx].expr, row, graph))
                .collect::<Vec<_>>()
        };
        let h = hash_value_slice(&key_values, build_hasher);
        let bucket = group_index.entry(h).or_default();
        let mut found = false;
        for &idx in bucket.iter() {
            if groups[idx].0 == key_values {
                groups[idx].1.push(row);
                found = true;
                break;
            }
        }
        if !found {
            let max_groups = effective_max_groups();
            if groups.len() >= max_groups {
                return Err(GleaphError::ExecutionError(format!(
                    "MAX_GROUPS exceeded ({max_groups})"
                )));
            }
            let idx = groups.len();
            bucket.push(idx);
            groups.push((key_values, vec![row]));
        }
    }
    if let Some(stats) = stats {
        stats.breakdown.groups_formed = stats
            .breakdown
            .groups_formed
            .saturating_add(groups.len() as u64);
    }

    // Aggregation + LIMIT pushdown: when LIMIT is present without ORDER BY or HAVING,
    // we can stop evaluating groups early once we have enough output rows.
    let agg_limit = if q.limit.is_some() && q.order_by.is_none() && q.having.is_none() {
        q.limit.map(|l| l.0 as usize)
    } else {
        None
    };

    let mut result = Vec::new();
    for (_key_values, group_rows) in groups {
        if agg_limit.is_some_and(|k| result.len() >= k) {
            break;
        }
        let rep = group_rows.first().copied();
        if q.having
            .as_ref()
            .is_some_and(|h| !truthy(&eval_group_expr(h, rep, &group_rows, graph)))
        {
            continue;
        }
        result.push(
            q.return_clause
                .items
                .iter()
                .map(|item| eval_group_expr(&item.expr, rep, &group_rows, graph))
                .collect::<Vec<_>>(),
        );
    }
    Ok(result)
}

fn eval_group_expr<M: Memory>(
    expr: &Expr,
    rep: Option<&Bindings>,
    group_rows: &[&Bindings],
    graph: &PmaGraph<M>,
) -> Value {
    match expr {
        Expr::Aggregate(agg) => eval_aggregate_expr(agg, group_rows, graph),
        Expr::Literal(_) | Expr::Variable(_) | Expr::PathVar(_) | Expr::Parameter { .. } => rep
            .map(|r| eval_expr(expr, r, graph))
            .unwrap_or(Value::Null),
        Expr::PropertyAccess {
            target,
            property: _,
        } => {
            let target = eval_group_expr(target, rep, group_rows, graph);
            match target {
                _ => {
                    // For non-binding targets, fallback to representative row evaluation.
                    rep.map(|r| eval_expr(expr, r, graph))
                        .unwrap_or(Value::Null)
                }
            }
        }
        Expr::BinaryOp { op, left, right } => eval_binary_op(
            *op,
            &eval_group_expr(left, rep, group_rows, graph),
            &eval_group_expr(right, rep, group_rows, graph),
        ),
        Expr::UnaryOp { op, expr } => {
            eval_unary_op(*op, &eval_group_expr(expr, rep, group_rows, graph))
        }
        Expr::And(l, r) => Value::Bool(
            truthy(&eval_group_expr(l, rep, group_rows, graph))
                && truthy(&eval_group_expr(r, rep, group_rows, graph)),
        ),
        Expr::Or(l, r) => Value::Bool(
            truthy(&eval_group_expr(l, rep, group_rows, graph))
                || truthy(&eval_group_expr(r, rep, group_rows, graph)),
        ),
        Expr::Xor(l, r) => Value::Bool(
            truthy(&eval_group_expr(l, rep, group_rows, graph))
                ^ truthy(&eval_group_expr(r, rep, group_rows, graph)),
        ),
        Expr::Not(e) => Value::Bool(!truthy(&eval_group_expr(e, rep, group_rows, graph))),
        Expr::Compare { left, op, right } => {
            let l = eval_group_expr(left, rep, group_rows, graph);
            let r = eval_group_expr(right, rep, group_rows, graph);
            Value::Bool(compare_cmp(*op, &l, &r))
        }
        Expr::IsNull(e) => Value::Bool(matches!(
            eval_group_expr(e, rep, group_rows, graph),
            Value::Null
        )),
        Expr::IsNotNull(e) => Value::Bool(!matches!(
            eval_group_expr(e, rep, group_rows, graph),
            Value::Null
        )),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let needle = eval_group_expr(expr, rep, group_rows, graph);
            let found = list.iter().any(|c| {
                let val = eval_group_expr(c, rep, group_rows, graph);
                if let Value::List(items) = &val {
                    items
                        .iter()
                        .any(|item| compare_values(&needle, item) == Some(Ordering::Equal))
                } else {
                    compare_values(&needle, &val) == Some(Ordering::Equal)
                }
            });
            Value::Bool(if *negated { !found } else { found })
        }
        Expr::StringPredicate { .. }
        | Expr::Case(_)
        | Expr::Coalesce(_)
        | Expr::NullIf { .. }
        | Expr::FunctionCall { .. }
        | Expr::Exists(_)
        | Expr::Concat(_, _)
        | Expr::PathLength(_)
        | Expr::ListLiteral(_)
        | Expr::ListIndex { .. }
        | Expr::Cast { .. }
        | Expr::IsTruth { .. }
        | Expr::IsLabeled { .. }
        | Expr::IsSourceOf { .. }
        | Expr::IsDestOf { .. }
        | Expr::AllDifferent(_)
        | Expr::Same(_)
        | Expr::PropertyExists { .. }
        | Expr::RecordLiteral(_)
        | Expr::IsType { .. }
        | Expr::IsDirected { .. }
        | Expr::ValueSubquery(_)
        | Expr::LetIn { .. }
        | Expr::PathConstructor(_) => rep.map(|r| eval_expr(expr, r, graph)).unwrap_or_else(|| {
            if matches!(expr, Expr::Coalesce(_)) {
                eval_expr(expr, &Bindings::new(), graph)
            } else {
                Value::Null
            }
        }),
    }
}

fn eval_aggregate_expr<M: Memory>(
    agg: &crate::ast::AggregateExpr,
    group_rows: &[&Bindings],
    graph: &PmaGraph<M>,
) -> Value {
    let mut values = Vec::new();
    if !agg.count_all {
        for row in group_rows {
            let v = agg
                .expr
                .as_ref()
                .map(|e| eval_expr(e, row, graph))
                .unwrap_or(Value::Null);
            if !matches!(v, Value::Null) {
                values.push(v);
            }
        }
        if agg.distinct {
            let mut seen = BTreeSet::new();
            values.retain(|v| seen.insert(format!("{v:?}")));
        }
    }
    match agg.func {
        AggFunc::Count => {
            if agg.count_all {
                Value::Int64(group_rows.len() as i64)
            } else {
                Value::Int64(values.len() as i64)
            }
        }
        AggFunc::Sum => values
            .into_iter()
            .try_fold(0.0, |acc, v| numeric_as_f64(&v).map(|n| acc + n))
            .map(Value::Float64)
            .unwrap_or(Value::Null),
        AggFunc::Avg => {
            if values.is_empty() {
                Value::Null
            } else {
                let len = values.len() as f64;
                values
                    .into_iter()
                    .try_fold(0.0, |acc, v| numeric_as_f64(&v).map(|n| acc + n))
                    .map(|sum| Value::Float64(sum / len))
                    .unwrap_or(Value::Null)
            }
        }
        AggFunc::Min => values
            .into_iter()
            .min_by(|a, b| compare_values(a, b).unwrap_or(Ordering::Equal))
            .unwrap_or(Value::Null),
        AggFunc::Max => values
            .into_iter()
            .max_by(|a, b| compare_values(a, b).unwrap_or(Ordering::Equal))
            .unwrap_or(Value::Null),
        AggFunc::Collect => Value::List(values),
        AggFunc::StringAgg => {
            // STRING_AGG(expr, separator) / GROUP_CONCAT(expr)
            // Evaluate separator from the first row (it's a literal in practice)
            let sep = agg
                .separator
                .as_ref()
                .and_then(|s| group_rows.first().map(|r| eval_expr(s, r, graph)))
                .and_then(|v| {
                    if let Value::Text(t) = v {
                        Some(t)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let parts: Vec<String> = values
                .into_iter()
                .filter_map(|v| {
                    if let Value::Text(t) = v {
                        Some(t)
                    } else {
                        None
                    }
                })
                .collect();
            if parts.is_empty() {
                Value::Null
            } else {
                Value::Text(parts.join(&sep))
            }
        }
        AggFunc::PercentileCont => {
            // PERCENTILE_CONT(expr, percentile) — linear interpolation
            // percentile is stored in the separator field (reused for second argument)
            let p = agg
                .separator
                .as_ref()
                .and_then(|s| group_rows.first().map(|r| eval_expr(s, r, graph)))
                .and_then(|v| numeric_as_f64(&v))
                .unwrap_or(0.5);
            let p = p.clamp(0.0, 1.0);
            let mut nums: Vec<f64> = values.iter().filter_map(numeric_as_f64).collect();
            if nums.is_empty() {
                return Value::Null;
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
            let n = nums.len() as f64;
            let rank = p * (n - 1.0);
            let lo = rank.floor() as usize;
            let hi = rank.ceil() as usize;
            if lo == hi {
                Value::Float64(nums[lo])
            } else {
                let frac = rank - lo as f64;
                Value::Float64(nums[lo] + frac * (nums[hi] - nums[lo]))
            }
        }
        AggFunc::PercentileDisc => {
            // PERCENTILE_DISC(expr, percentile) — nearest rank (discrete)
            let p = agg
                .separator
                .as_ref()
                .and_then(|s| group_rows.first().map(|r| eval_expr(s, r, graph)))
                .and_then(|v| numeric_as_f64(&v))
                .unwrap_or(0.5);
            let p = p.clamp(0.0, 1.0);
            let mut nums: Vec<f64> = values.iter().filter_map(numeric_as_f64).collect();
            if nums.is_empty() {
                return Value::Null;
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
            let n = nums.len() as f64;
            let idx = (p * n).ceil() as usize;
            let idx = if idx == 0 { 0 } else { idx.min(nums.len()) - 1 };
            Value::Float64(nums[idx])
        }
    }
}

fn numeric_as_f64(v: &Value) -> Option<f64> {
    if let Some(f) = v.as_f64() {
        return Some(f);
    }
    match v {
        Value::Float32(f) => Some(*f as f64),
        Value::Float64(f) => Some(*f),
        Value::Decimal(d) => d.to_f64(),
        _ => None,
    }
}

/// §20.18: Look up a named property in a record value.
///
/// Records are encoded as `Value::List` of `[key, value]` two-element lists.
/// Returns `Value::Null` if the key is not found or the value is not a record.
fn record_property_lookup(val: &Value, property: &str) -> Value {
    if let Value::List(items) = val {
        for item in items {
            if let Value::List(pair) = item
                && pair.len() == 2
                && let Value::Text(k) = &pair[0]
                && k == property
            {
                return pair[1].clone();
            }
        }
    }
    Value::Null
}

fn eval_expr<M: Memory>(expr: &Expr, bindings: &Bindings, graph: &PmaGraph<M>) -> Value {
    match expr {
        Expr::Literal(v) => v.clone(),
        Expr::Variable(v) | Expr::PathVar(v) => binding_value(v, bindings),
        Expr::Parameter {
            name,
            type_annotation,
        } => {
            let val = QUERY_PARAMS
                .with(|p| p.borrow().get(name.as_str()).cloned())
                .unwrap_or(Value::Null);
            if let Some(types) = type_annotation {
                validate_param_types(name, &val, types);
            }
            val
        }
        Expr::PropertyAccess { target, property } => {
            // Fast path for plain variable targets.
            if let Expr::Variable(var) | Expr::PathVar(var) = target.as_ref() {
                match bindings.get(var) {
                    Some(Binding::Vertex(id)) => return vertex_property(*id, property, graph),
                    Some(Binding::Edge {
                        src,
                        dst,
                        label,
                        weight,
                        timestamp,
                        ..
                    }) => {
                        return edge_property(
                            *src,
                            *dst,
                            label.as_deref(),
                            property,
                            *weight,
                            *timestamp,
                            graph,
                        );
                    }
                    Some(Binding::Value(v)) => return record_property_lookup(v, property),
                    None => return Value::Null,
                }
            }
            // Generic case: evaluate target expression then look up property in record.
            let val = eval_expr(target, bindings, graph);
            record_property_lookup(&val, property)
        }
        Expr::BinaryOp { op, left, right } => eval_binary_op(
            *op,
            &eval_expr(left, bindings, graph),
            &eval_expr(right, bindings, graph),
        ),
        Expr::UnaryOp { op, expr } => eval_unary_op(*op, &eval_expr(expr, bindings, graph)),
        Expr::And(left, right) => Value::Bool(
            truthy(&eval_expr(left, bindings, graph)) && truthy(&eval_expr(right, bindings, graph)),
        ),
        Expr::Or(left, right) => Value::Bool(
            truthy(&eval_expr(left, bindings, graph)) || truthy(&eval_expr(right, bindings, graph)),
        ),
        Expr::Not(expr) => Value::Bool(!truthy(&eval_expr(expr, bindings, graph))),
        Expr::Xor(left, right) => Value::Bool(
            truthy(&eval_expr(left, bindings, graph)) ^ truthy(&eval_expr(right, bindings, graph)),
        ),
        Expr::Compare { left, op, right } => {
            let left = eval_expr(left, bindings, graph);
            let right = eval_expr(right, bindings, graph);
            Value::Bool(compare_cmp(*op, &left, &right))
        }
        Expr::IsNull(expr) => Value::Bool(matches!(eval_expr(expr, bindings, graph), Value::Null)),
        Expr::IsNotNull(expr) => {
            Value::Bool(!matches!(eval_expr(expr, bindings, graph), Value::Null))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let needle = eval_expr(expr, bindings, graph);
            let found = list.iter().any(|candidate| {
                let val = eval_expr(candidate, bindings, graph);
                // When a candidate evaluates to a list (e.g. `x IN $param`
                // where $param is a list), check membership in that list.
                if let Value::List(items) = &val {
                    items
                        .iter()
                        .any(|item| compare_values(&needle, item) == Some(Ordering::Equal))
                } else {
                    compare_values(&needle, &val) == Some(Ordering::Equal)
                }
            });
            Value::Bool(if *negated { !found } else { found })
        }
        Expr::StringPredicate {
            expr,
            kind,
            pattern,
        } => {
            let val = eval_expr(expr, bindings, graph);
            let pat = eval_expr(pattern, bindings, graph);
            match (val, pat) {
                (Value::Text(s), Value::Text(p)) => Value::Bool(match kind {
                    StringPredicateKind::StartsWith => s.starts_with(p.as_str()),
                    StringPredicateKind::EndsWith => s.ends_with(p.as_str()),
                    StringPredicateKind::Contains => s.contains(p.as_str()),
                    StringPredicateKind::Like => like_match(&s, &p, false),
                    StringPredicateKind::ILike => like_match(&s, &p, true),
                }),
                _ => Value::Null,
            }
        }
        Expr::Concat(left, right) => match (
            eval_expr(left, bindings, graph),
            eval_expr(right, bindings, graph),
        ) {
            (Value::Text(a), Value::Text(b)) => Value::Text(a + &b),
            _ => Value::Null,
        },
        Expr::Coalesce(items) => items
            .iter()
            .map(|e| eval_expr(e, bindings, graph))
            .find(|v| !matches!(v, Value::Null))
            .unwrap_or(Value::Null),
        Expr::NullIf { left, right } => {
            let l = eval_expr(left, bindings, graph);
            let r = eval_expr(right, bindings, graph);
            if compare_values(&l, &r) == Some(Ordering::Equal) {
                Value::Null
            } else {
                l
            }
        }
        Expr::Case(case_expr) => {
            let operand = case_expr
                .operand
                .as_ref()
                .map(|e| eval_expr(e, bindings, graph));
            for wt in &case_expr.when_then {
                let matched = if let Some(opv) = &operand {
                    compare_values(opv, &eval_expr(&wt.when, bindings, graph))
                        == Some(Ordering::Equal)
                } else {
                    truthy(&eval_expr(&wt.when, bindings, graph))
                };
                if matched {
                    return eval_expr(&wt.then, bindings, graph);
                }
            }
            case_expr
                .else_expr
                .as_ref()
                .map(|e| eval_expr(e, bindings, graph))
                .unwrap_or(Value::Null)
        }
        Expr::ListLiteral(items) => Value::List(
            items
                .iter()
                .map(|e| eval_expr(e, bindings, graph))
                .collect(),
        ),
        Expr::ListIndex { list, index } => {
            let (list, index) = (
                eval_expr(list, bindings, graph),
                eval_expr(index, bindings, graph),
            );
            match (list, index) {
                (Value::List(items), ref idx_val) if idx_val.as_i64().is_some() => {
                    let i = idx_val.as_i64().unwrap();
                    let len = items.len() as i64;
                    let idx = if i < 0 { len + i } else { i };
                    if idx >= 0 {
                        items.get(idx as usize).cloned().unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    }
                }
                (Value::Text(s), ref idx_val) if idx_val.as_i64().is_some() => {
                    let i = idx_val.as_i64().unwrap();
                    let chars: Vec<char> = s.chars().collect();
                    let len = chars.len() as i64;
                    let idx = if i < 0 { len + i } else { i };
                    if idx >= 0 {
                        chars
                            .get(idx as usize)
                            .map(|c| Value::Text(c.to_string()))
                            .unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    }
                }
                _ => Value::Null,
            }
        }
        Expr::Aggregate(_) | Expr::PathLength(_) => Value::Null,
        Expr::FunctionCall { name, args } => eval_function_call_expr(name, args, bindings, graph),
        Expr::Exists(stmt) => match stmt.as_ref() {
            // §19.4: Correlated EXISTS — pass outer bindings as seed row so inner query
            // can reference outer variables (e.g. EXISTS { MATCH (n)-[e]->(m) RETURN e }).
            Statement::Query(q) => {
                let seed_rows = vec![bindings.clone()];
                let mut stats = QueryStats::default();
                let limits = ExecutionLimits {
                    max_rows: Some(1),
                    ..Default::default()
                };
                execute_query_match_entries_from_seed_rows(
                    q,
                    graph,
                    &mut stats,
                    q.where_clause.as_ref(),
                    Some(1),
                    limits,
                    seed_rows,
                    None,
                )
                .map(|rows| Value::Bool(!rows.is_empty()))
                .unwrap_or(Value::Bool(false))
            }
            Statement::Compound { .. } => Value::Bool(
                execute_read_statement(
                    stmt,
                    graph,
                    ExecutionLimits {
                        max_rows: Some(1),
                        ..Default::default()
                    },
                )
                .map(|r| !r.rows.is_empty())
                .unwrap_or(false),
            ),
            _ => Value::Bool(false),
        },
        Expr::Cast { expr, target_type } => {
            let val = eval_expr(expr, bindings, graph);
            cast_value(val, *target_type)
        }
        Expr::IsTruth {
            expr,
            negated,
            truth,
        } => {
            let val = eval_expr(expr, bindings, graph);
            let result = match truth {
                TruthValue::True => truthy(&val),
                TruthValue::False => !truthy(&val) && !matches!(val, Value::Null),
                TruthValue::Unknown => matches!(val, Value::Null),
            };
            Value::Bool(if *negated { !result } else { result })
        }
        Expr::IsLabeled {
            expr,
            negated,
            label_expr,
        } => {
            let val = eval_expr(expr, bindings, graph);
            let result = match val {
                Value::Int64(id) => {
                    let vid = id as u32;
                    matches_label_expr(label_expr, vid, graph)
                }
                _ => false,
            };
            Value::Bool(if *negated { !result } else { result })
        }
        Expr::IsSourceOf {
            node,
            negated,
            edge,
        } => {
            let node_val = eval_expr(node, bindings, graph);
            let edge_val = eval_expr(edge, bindings, graph);
            let result = match (&node_val, edge.as_ref()) {
                (Value::Int64(node_id), Expr::Variable(edge_var)) => match bindings.get(edge_var) {
                    Some(Binding::Edge { src, .. }) => *node_id == i64::from(*src),
                    _ => false,
                },
                _ => false,
            };
            let _ = edge_val;
            Value::Bool(if *negated { !result } else { result })
        }
        Expr::IsDestOf {
            node,
            negated,
            edge,
        } => {
            let node_val = eval_expr(node, bindings, graph);
            let edge_val = eval_expr(edge, bindings, graph);
            let result = match (&node_val, edge.as_ref()) {
                (Value::Int64(node_id), Expr::Variable(edge_var)) => match bindings.get(edge_var) {
                    Some(Binding::Edge { dst, .. }) => *node_id == i64::from(*dst),
                    _ => false,
                },
                _ => false,
            };
            let _ = edge_val;
            Value::Bool(if *negated { !result } else { result })
        }
        Expr::AllDifferent(exprs) => {
            let vals: Vec<Value> = exprs
                .iter()
                .map(|e| eval_expr(e, bindings, graph))
                .collect();
            let mut seen = BTreeSet::new();
            Value::Bool(vals.iter().all(|v| seen.insert(format!("{v:?}"))))
        }
        Expr::Same(exprs) => {
            let vals: Vec<Value> = exprs
                .iter()
                .map(|e| eval_expr(e, bindings, graph))
                .collect();
            if vals.is_empty() {
                Value::Bool(true)
            } else {
                let first = format!("{:?}", vals[0]);
                Value::Bool(vals.iter().all(|v| format!("{v:?}") == first))
            }
        }
        Expr::PropertyExists { target, property } => {
            let result = match target.as_ref() {
                Expr::Variable(var) => match bindings.get(var) {
                    Some(Binding::Vertex(id)) => {
                        let props = graph.get_vertex_props(*id).unwrap_or_default();
                        props.iter().any(|(k, _)| k == property)
                    }
                    Some(Binding::Edge {
                        src, dst, label, ..
                    }) => {
                        if let Some(edge) = graph.edge_record(*src, *dst, label.as_deref()) {
                            edge.props.iter().any(|(k, _)| k == property)
                        } else {
                            false
                        }
                    }
                    _ => false,
                },
                _ => false,
            };
            Value::Bool(result)
        }
        Expr::RecordLiteral(pairs) => {
            // Represent records as Value::List of 2-element [key, value] lists
            let items = pairs
                .iter()
                .map(|(k, v)| {
                    Value::List(vec![Value::Text(k.clone()), eval_expr(v, bindings, graph)])
                })
                .collect();
            Value::List(items)
        }
        // §19.6: IS :: typename — runtime type predicate
        Expr::IsType {
            expr,
            negated,
            value_type,
            type_name,
        } => {
            let val = eval_expr(expr, bindings, graph);
            let matches = if let Some(vt) = value_type {
                matches_value_type(&val, *vt)
            } else {
                // Not a built-in type — try node type check.
                match &val {
                    Value::Int64(vid) if *vid >= 0 => {
                        let vertex_id = *vid as u32;
                        let labels = resolve_type_name_to_labels(type_name);
                        labels.iter().all(|l| graph.vertex_has_label(vertex_id, l))
                    }
                    _ => false,
                }
            };
            Value::Bool(if *negated { !matches } else { matches })
        }
        // §20.6: VALUE { query } — scalar subquery
        Expr::ValueSubquery(stmt) => {
            // §20.6: Execute inner query and return first column of first row, or NULL.
            execute_read_statement(
                stmt,
                graph,
                ExecutionLimits {
                    max_rows: Some(1),
                    ..Default::default()
                },
            )
            .ok()
            .and_then(|r| r.rows.into_iter().next())
            .and_then(|row| row.into_iter().next())
            .unwrap_or(Value::Null)
        }
        // §20.5: LET x = e1, ... IN body END — value-expression binding
        Expr::LetIn {
            bindings: let_bindings,
            body,
        } => {
            let mut local_bindings = bindings.clone();
            for (name, expr) in let_bindings {
                let val = eval_expr(expr, &local_bindings, graph);
                local_bindings.insert(name.clone(), Binding::Value(val));
            }
            eval_expr(body, &local_bindings, graph)
        }
        // §20.14: PATH [n1, e1, n2, ...] — explicit path constructor; returns a list of values.
        Expr::PathConstructor(elems) => {
            let values: Vec<Value> = elems
                .iter()
                .map(|e| eval_expr(e, bindings, graph))
                .collect();
            Value::List(values)
        }
        // §19.8: e IS [NOT] DIRECTED — all edges in this engine are directed.
        Expr::IsDirected { expr, negated } => {
            let is_directed = match expr.as_ref() {
                Expr::Variable(var) | Expr::PathVar(var) => {
                    matches!(bindings.get(var), Some(Binding::Edge { .. }))
                }
                _ => false,
            };
            Value::Bool(if *negated { !is_directed } else { is_directed })
        }
    }
}

fn cast_value(val: Value, target_type: ValueType) -> Value {
    match target_type {
        // ── Signed integer targets ──
        ValueType::Int8
        | ValueType::Int16
        | ValueType::Int32
        | ValueType::Int64
        | ValueType::Int128 => {
            let width = match target_type {
                ValueType::Int8 => 8,
                ValueType::Int16 => 16,
                ValueType::Int32 => 32,
                ValueType::Int64 => 64,
                ValueType::Int128 => 128,
                _ => unreachable!(),
            };
            cast_to_signed_int(val, width)
        }
        ValueType::Int256 => cast_to_int256(val),
        // ── Unsigned integer targets ──
        ValueType::Uint8
        | ValueType::Uint16
        | ValueType::Uint32
        | ValueType::Uint64
        | ValueType::Uint128 => {
            let width = match target_type {
                ValueType::Uint8 => 8,
                ValueType::Uint16 => 16,
                ValueType::Uint32 => 32,
                ValueType::Uint64 => 64,
                ValueType::Uint128 => 128,
                _ => unreachable!(),
            };
            cast_to_unsigned_int(val, width)
        }
        ValueType::Uint256 => cast_to_uint256(val),
        ValueType::Float32 => {
            if let Some(f) = val.as_f64() {
                let f32val = f as f32;
                return if f32val.is_infinite() && !f.is_infinite() {
                    Value::Null // overflow
                } else {
                    Value::Float32(f32val)
                };
            }
            match val {
                Value::Float32(f) => Value::Float32(f),
                Value::Float64(f) => {
                    let f32val = f as f32;
                    if f32val.is_infinite() && !f.is_infinite() {
                        Value::Null
                    } else {
                        Value::Float32(f32val)
                    }
                }
                Value::Text(s) => s.parse::<f32>().map(Value::Float32).unwrap_or(Value::Null),
                Value::Decimal(d) => d
                    .to_f64()
                    .map(|f| {
                        let f32val = f as f32;
                        if f32val.is_infinite() && !f.is_infinite() {
                            Value::Null
                        } else {
                            Value::Float32(f32val)
                        }
                    })
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        ValueType::Float64 => {
            if let Some(f) = val.as_f64() {
                return Value::Float64(f);
            }
            match val {
                Value::Float32(f) => Value::Float64(f as f64),
                Value::Float64(f) => Value::Float64(f),
                Value::Text(s) => s.parse::<f64>().map(Value::Float64).unwrap_or(Value::Null),
                Value::Decimal(d) => d.to_f64().map(Value::Float64).unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        ValueType::Text => {
            // All integer types have Display, handled via generic arm.
            match val {
                Value::Null => Value::Null,
                Value::Bool(b) => Value::Text(b.to_string()),
                Value::Float32(f) => Value::Text(f.to_string()),
                Value::Float64(f) => Value::Text(f.to_string()),
                Value::Text(s) => Value::Text(s),
                Value::Timestamp(t) => Value::Text(t.to_string()),
                Value::List(v) => Value::Text(format!("{v:?}")),
                Value::Path(v) => Value::Text(format!("{v:?}")),
                Value::Bytes(b) => {
                    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
                    Value::Text(hex)
                }
                Value::Date(d) => Value::Text(crate::temporal::format_date(d)),
                Value::Time(t) => Value::Text(crate::temporal::format_time(t)),
                Value::DateTime(s, n) => Value::Text(crate::temporal::format_datetime(s, n)),
                Value::Duration(m, n) => Value::Text(crate::temporal::format_duration(m, n)),
                Value::Principal(p) => Value::Text(p.to_text()),
                Value::Decimal(d) => Value::Text(d.to_string()),
                // All integer variants
                Value::Int8(i) => Value::Text(i.to_string()),
                Value::Int16(i) => Value::Text(i.to_string()),
                Value::Int32(i) => Value::Text(i.to_string()),
                Value::Int64(i) => Value::Text(i.to_string()),
                Value::Int128(i) => Value::Text(i.to_string()),
                Value::Int256(i) => Value::Text(i.0.to_string()),
                Value::Uint8(u) => Value::Text(u.to_string()),
                Value::Uint16(u) => Value::Text(u.to_string()),
                Value::Uint32(u) => Value::Text(u.to_string()),
                Value::Uint64(u) => Value::Text(u.to_string()),
                Value::Uint128(u) => Value::Text(u.to_string()),
                Value::Uint256(u) => Value::Text(u.0.to_string()),
            }
        }
        ValueType::TextConstrained {
            min_length,
            max_length,
            fixed,
        } => {
            // First, cast to unconstrained Text, then enforce length.
            let text_val = cast_value(val, ValueType::Text);
            match text_val {
                Value::Text(s) => {
                    let char_len = s.chars().count() as u32;
                    if fixed {
                        if char_len <= max_length {
                            // Pad with spaces to exactly max_length.
                            Value::Text(format!("{:<width$}", s, width = max_length as usize))
                        } else {
                            Value::Null
                        }
                    } else if char_len >= min_length && char_len <= max_length {
                        Value::Text(s)
                    } else {
                        Value::Null
                    }
                }
                other => other, // Null passthrough
            }
        }
        ValueType::Bool => {
            if val.is_any_int() {
                return Value::Bool(truthy(&val));
            }
            match val {
                Value::Bool(b) => Value::Bool(b),
                Value::Text(s) => match s.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "1" => Value::Bool(true),
                    "false" | "no" | "0" => Value::Bool(false),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        ValueType::Timestamp => {
            if val.is_signed_int() {
                if let Some(i) = val.as_i128() {
                    return Value::Timestamp(i as u64);
                }
            }
            match val {
                Value::Timestamp(t) => Value::Timestamp(t),
                Value::Date(d) => Value::Timestamp((d as i64 * 86400) as u64 * 1_000_000_000),
                Value::DateTime(secs, sub) => {
                    let nanos = secs as i128 * 1_000_000_000 + sub as i128;
                    if nanos >= 0 {
                        Value::Timestamp(nanos as u64)
                    } else {
                        Value::Null
                    }
                }
                _ => Value::Null,
            }
        }
        ValueType::List | ValueType::TypedList(_) => match val {
            Value::List(l) => Value::List(l),
            _ => Value::Null,
        },
        ValueType::Null => Value::Null,
        ValueType::Bytes => match val {
            Value::Bytes(b) => Value::Bytes(b),
            Value::Text(s) => {
                // Hex string → bytes
                let s = s
                    .strip_prefix("0x")
                    .or_else(|| s.strip_prefix("0X"))
                    .unwrap_or(&s);
                if s.len() % 2 != 0 {
                    return Value::Null;
                }
                let mut out = Vec::with_capacity(s.len() / 2);
                for chunk in s.as_bytes().chunks(2) {
                    let hi = hex_char_to_nibble(chunk[0]);
                    let lo = hex_char_to_nibble(chunk[1]);
                    match (hi, lo) {
                        (Some(h), Some(l)) => out.push(h << 4 | l),
                        _ => return Value::Null,
                    }
                }
                Value::Bytes(out)
            }
            Value::List(items) => {
                // List of ints → bytes
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(i) = item.as_i128() {
                        if (0..=255).contains(&i) {
                            out.push(i as u8);
                            continue;
                        }
                    }
                    return Value::Null;
                }
                Value::Bytes(out)
            }
            _ => Value::Null,
        },
        ValueType::BytesConstrained {
            min_length,
            max_length,
            fixed,
        } => {
            let bytes_val = cast_value(val, ValueType::Bytes);
            match bytes_val {
                Value::Bytes(b) => {
                    let len = b.len() as u32;
                    if fixed {
                        if len <= max_length {
                            let mut padded = b;
                            padded.resize(max_length as usize, 0u8);
                            Value::Bytes(padded)
                        } else {
                            Value::Null
                        }
                    } else if len >= min_length && len <= max_length {
                        Value::Bytes(b)
                    } else {
                        Value::Null
                    }
                }
                other => other,
            }
        }
        ValueType::Date => {
            if val.is_signed_int() {
                if let Some(i) = val.as_i128() {
                    return Value::Date(i as i32); // epoch days
                }
            }
            match val {
                Value::Date(d) => Value::Date(d),
                Value::DateTime(secs, _) => {
                    let day_secs = secs.rem_euclid(86400);
                    Value::Date(((secs - day_secs) / 86400) as i32)
                }
                Value::Text(s) => crate::temporal::parse_date(&s)
                    .map(Value::Date)
                    .unwrap_or(Value::Null),
                Value::Timestamp(t) => {
                    let secs = (t / 1_000_000_000) as i64;
                    Value::Date((secs / 86400) as i32)
                }
                _ => Value::Null,
            }
        }
        ValueType::Time => match val {
            Value::Time(t) => Value::Time(t),
            Value::DateTime(secs, sub) => {
                let day_nanos = secs.rem_euclid(86400) as u64 * 1_000_000_000 + sub as u64;
                Value::Time(day_nanos)
            }
            Value::Text(s) => crate::temporal::parse_time(&s)
                .map(Value::Time)
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        ValueType::DateTime => {
            if val.is_signed_int() {
                if let Some(i) = val.as_i128() {
                    return Value::DateTime(i as i64, 0); // epoch seconds
                }
            }
            match val {
                Value::DateTime(s, n) => Value::DateTime(s, n),
                Value::Date(d) => Value::DateTime(d as i64 * 86400, 0), // midnight
                Value::Text(s) => crate::temporal::parse_datetime(&s)
                    .map(|(s, n)| Value::DateTime(s, n))
                    .unwrap_or(Value::Null),
                Value::Timestamp(t) => {
                    let secs = (t / 1_000_000_000) as i64;
                    let sub = (t % 1_000_000_000) as u32;
                    Value::DateTime(secs, sub)
                }
                _ => Value::Null,
            }
        }
        ValueType::Duration => match val {
            Value::Duration(m, n) => Value::Duration(m, n),
            Value::Text(s) => crate::temporal::parse_duration(&s)
                .map(|(m, n)| Value::Duration(m, n))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        ValueType::Decimal => {
            if val.is_signed_int() {
                if let Some(i) = val.as_i128() {
                    return Value::Decimal(gleaph_types::Decimal::new(
                        rust_decimal::Decimal::from(i),
                    ));
                }
            }
            if val.is_unsigned_int() {
                if let Some(u) = val.as_u128() {
                    return Value::Decimal(gleaph_types::Decimal::new(
                        rust_decimal::Decimal::from(u),
                    ));
                }
            }
            match val {
                Value::Decimal(d) => Value::Decimal(d),
                Value::Text(s) => gleaph_types::Decimal::from_str(&s)
                    .map(Value::Decimal)
                    .unwrap_or(Value::Null),
                Value::Float32(f) => rust_decimal::Decimal::try_from(f as f64)
                    .ok()
                    .map(|d| Value::Decimal(gleaph_types::Decimal::new(d)))
                    .unwrap_or(Value::Null),
                Value::Float64(f) => rust_decimal::Decimal::try_from(f)
                    .ok()
                    .map(|d| Value::Decimal(gleaph_types::Decimal::new(d)))
                    .unwrap_or(Value::Null),
                Value::Bool(b) => {
                    Value::Decimal(gleaph_types::Decimal::from_i64(if b { 1 } else { 0 }))
                }
                _ => Value::Null,
            }
        }
    }
}

/// Cast any value to a signed integer of the given width (8/16/32/64/128).
fn cast_to_signed_int(val: Value, width: u16) -> Value {
    if val.is_signed_int() {
        if let Some(i) = val.as_i128() {
            return gleaph_types::narrow_signed(i, width);
        }
    }
    if val.is_unsigned_int() {
        if let Some(u) = val.as_u128() {
            if let Ok(i) = i128::try_from(u) {
                return gleaph_types::narrow_signed(i, width);
            }
            return Value::Null;
        }
    }
    match val {
        Value::Int256(v) => {
            // Try to fit in i128 first
            let i256 = v.0;
            if i256 >= ethnum::I256::from(i128::MIN) && i256 <= ethnum::I256::from(i128::MAX) {
                gleaph_types::narrow_signed(i256.as_i128(), width)
            } else {
                Value::Null
            }
        }
        Value::Uint256(v) => {
            if v.0 <= ethnum::U256::from(i128::MAX as u128) {
                gleaph_types::narrow_signed(v.0.as_i128(), width)
            } else {
                Value::Null
            }
        }
        Value::Float32(f) => gleaph_types::narrow_signed(f as i128, width),
        Value::Float64(f) => gleaph_types::narrow_signed(f as i128, width),
        Value::Text(s) => s
            .parse::<i128>()
            .ok()
            .map(|i| gleaph_types::narrow_signed(i, width))
            .unwrap_or(Value::Null),
        Value::Bool(b) => gleaph_types::narrow_signed(if b { 1 } else { 0 }, width),
        Value::Decimal(d) => {
            use rust_decimal::prelude::ToPrimitive;
            d.0.to_i128()
                .map(|i| gleaph_types::narrow_signed(i, width))
                .unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

/// Cast any value to an unsigned integer of the given width (8/16/32/64/128).
fn cast_to_unsigned_int(val: Value, width: u16) -> Value {
    if val.is_unsigned_int() {
        if let Some(u) = val.as_u128() {
            return gleaph_types::narrow_unsigned(u, width);
        }
    }
    if val.is_signed_int() {
        if let Some(i) = val.as_i128() {
            if i < 0 {
                return Value::Null;
            }
            return gleaph_types::narrow_unsigned(i as u128, width);
        }
    }
    match val {
        Value::Int256(v) => {
            if v.0.is_negative() {
                Value::Null
            } else {
                let u256 = v.0.as_u256();
                if u256 <= ethnum::U256::from(u128::MAX) {
                    gleaph_types::narrow_unsigned(u256.as_u128(), width)
                } else {
                    Value::Null
                }
            }
        }
        Value::Uint256(v) => {
            if v.0 <= ethnum::U256::from(u128::MAX) {
                gleaph_types::narrow_unsigned(v.0.as_u128(), width)
            } else {
                Value::Null
            }
        }
        Value::Float32(f) => {
            if f < 0.0 || f.is_nan() || (f as f64) > u128::MAX as f64 {
                Value::Null
            } else {
                gleaph_types::narrow_unsigned(f as u128, width)
            }
        }
        Value::Float64(f) => {
            if f < 0.0 || f.is_nan() || f > u128::MAX as f64 {
                Value::Null
            } else {
                gleaph_types::narrow_unsigned(f as u128, width)
            }
        }
        Value::Text(s) => s
            .parse::<u128>()
            .ok()
            .map(|u| gleaph_types::narrow_unsigned(u, width))
            .unwrap_or(Value::Null),
        Value::Bool(b) => gleaph_types::narrow_unsigned(if b { 1 } else { 0 }, width),
        Value::Decimal(d) => {
            use rust_decimal::prelude::ToPrimitive;
            if d.0.is_sign_negative() {
                Value::Null
            } else {
                d.0.to_u128()
                    .map(|u| gleaph_types::narrow_unsigned(u, width))
                    .unwrap_or(Value::Null)
            }
        }
        _ => Value::Null,
    }
}

/// Cast any value to Int256.
fn cast_to_int256(val: Value) -> Value {
    if val.is_signed_int() {
        if let Some(i) = val.as_i128() {
            return Value::Int256(gleaph_types::Int256::new(ethnum::I256::from(i)));
        }
    }
    if val.is_unsigned_int() {
        if let Some(u) = val.as_u128() {
            return Value::Int256(gleaph_types::Int256::new(ethnum::I256::from(u as i128)));
        }
    }
    match val {
        Value::Int256(v) => Value::Int256(v),
        Value::Uint256(v) => Value::Int256(gleaph_types::Int256::new(v.0.as_i256())),
        Value::Float32(f) => {
            Value::Int256(gleaph_types::Int256::new(ethnum::I256::from(f as i128)))
        }
        Value::Float64(f) => {
            Value::Int256(gleaph_types::Int256::new(ethnum::I256::from(f as i128)))
        }
        Value::Text(s) => gleaph_types::Int256::from_str(&s)
            .map(Value::Int256)
            .unwrap_or(Value::Null),
        Value::Bool(b) => Value::Int256(gleaph_types::Int256::new(ethnum::I256::from(if b {
            1i128
        } else {
            0
        }))),
        _ => Value::Null,
    }
}

/// Cast any value to Uint256.
fn cast_to_uint256(val: Value) -> Value {
    if val.is_unsigned_int() {
        if let Some(u) = val.as_u128() {
            return Value::Uint256(gleaph_types::Uint256::new(ethnum::U256::from(u)));
        }
    }
    if val.is_signed_int() {
        if let Some(i) = val.as_i128() {
            if i < 0 {
                return Value::Null;
            }
            return Value::Uint256(gleaph_types::Uint256::new(ethnum::U256::from(i as u128)));
        }
    }
    match val {
        Value::Uint256(v) => Value::Uint256(v),
        Value::Int256(v) => {
            if v.0.is_negative() {
                Value::Null
            } else {
                Value::Uint256(gleaph_types::Uint256::new(v.0.as_u256()))
            }
        }
        Value::Float32(f) => {
            if f < 0.0 || f.is_nan() {
                Value::Null
            } else {
                Value::Uint256(gleaph_types::Uint256::new(ethnum::U256::from(f as u128)))
            }
        }
        Value::Float64(f) => {
            if f < 0.0 || f.is_nan() {
                Value::Null
            } else {
                Value::Uint256(gleaph_types::Uint256::new(ethnum::U256::from(f as u128)))
            }
        }
        Value::Text(s) => gleaph_types::Uint256::from_str(&s)
            .map(Value::Uint256)
            .unwrap_or(Value::Null),
        Value::Bool(b) => Value::Uint256(gleaph_types::Uint256::new(ethnum::U256::from(if b {
            1u128
        } else {
            0
        }))),
        _ => Value::Null,
    }
}

fn hex_char_to_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Check if a value matches the given built-in value type.
fn matches_value_type(val: &Value, vt: ValueType) -> bool {
    match vt {
        ValueType::Int8 => matches!(val, Value::Int8(_)),
        ValueType::Int16 => matches!(val, Value::Int16(_)),
        ValueType::Int32 => matches!(val, Value::Int32(_)),
        ValueType::Int64 => matches!(val, Value::Int64(_)),
        ValueType::Int128 => matches!(val, Value::Int128(_)),
        ValueType::Int256 => matches!(val, Value::Int256(_)),
        ValueType::Uint8 => matches!(val, Value::Uint8(_)),
        ValueType::Uint16 => matches!(val, Value::Uint16(_)),
        ValueType::Uint32 => matches!(val, Value::Uint32(_)),
        ValueType::Uint64 => matches!(val, Value::Uint64(_)),
        ValueType::Uint128 => matches!(val, Value::Uint128(_)),
        ValueType::Uint256 => matches!(val, Value::Uint256(_)),
        ValueType::Float32 => matches!(val, Value::Float32(_)),
        ValueType::Float64 => matches!(val, Value::Float64(_)),
        ValueType::Text | ValueType::TextConstrained { .. } => matches!(val, Value::Text(_)),
        ValueType::BytesConstrained { .. } => matches!(val, Value::Bytes(_)),
        ValueType::Bool => matches!(val, Value::Bool(_)),
        ValueType::Timestamp => matches!(val, Value::Timestamp(_)),
        ValueType::List | ValueType::TypedList(_) => matches!(val, Value::List(_)),
        ValueType::Null => matches!(val, Value::Null),
        ValueType::Bytes => matches!(val, Value::Bytes(_)),
        ValueType::Date => matches!(val, Value::Date(_)),
        ValueType::Time => matches!(val, Value::Time(_)),
        ValueType::DateTime => matches!(val, Value::DateTime(_, _)),
        ValueType::Duration => matches!(val, Value::Duration(_, _)),
        ValueType::Decimal => matches!(val, Value::Decimal(_)),
    }
}

/// Validate that a parameter value matches the expected type annotation.
/// Null is allowed for any type (SQL null semantics).
fn validate_param_types(name: &str, val: &Value, expected: &[ValueType]) {
    if matches!(val, Value::Null) {
        return; // Null is universally accepted.
    }
    if !expected.iter().any(|vt| matches_value_type(val, *vt)) {
        // Log a warning but don't fail — the type annotation is informational in eval_expr.
        // For strict checking, use the pre-execution validation in execute_plan_with_params.
    }
    let _ = name;
}

fn matches_label_expr<M: Memory>(
    label_expr: &LabelExpr,
    vertex_id: u32,
    graph: &PmaGraph<M>,
) -> bool {
    match label_expr {
        LabelExpr::Name(name) => graph.vertex_has_label(vertex_id, name),
        LabelExpr::Wildcard => {
            // Wildcard matches any node that has at least one label
            !graph.scan_vertices_by_label("").is_empty()
                || graph
                    .overlay_snapshot()
                    .vertex_labels
                    .into_iter()
                    .any(|(vid, labels)| vid == vertex_id && !labels.is_empty())
        }
        LabelExpr::And(a, b) => {
            matches_label_expr(a, vertex_id, graph) && matches_label_expr(b, vertex_id, graph)
        }
        LabelExpr::Or(a, b) => {
            matches_label_expr(a, vertex_id, graph) || matches_label_expr(b, vertex_id, graph)
        }
        LabelExpr::Not(e) => !matches_label_expr(e, vertex_id, graph),
    }
}

/// Like `matches_label_expr` but uses `vertex_has_label_unchecked` (no tombstone check).
fn matches_label_expr_unchecked<M: Memory>(
    label_expr: &LabelExpr,
    vertex_id: u32,
    graph: &PmaGraph<M>,
) -> bool {
    match label_expr {
        LabelExpr::Name(name) => graph.vertex_has_label_unchecked(vertex_id, name),
        LabelExpr::Wildcard => {
            !graph.scan_vertices_by_label("").is_empty()
                || graph
                    .overlay_snapshot()
                    .vertex_labels
                    .into_iter()
                    .any(|(vid, labels)| vid == vertex_id && !labels.is_empty())
        }
        LabelExpr::And(a, b) => {
            matches_label_expr_unchecked(a, vertex_id, graph)
                && matches_label_expr_unchecked(b, vertex_id, graph)
        }
        LabelExpr::Or(a, b) => {
            matches_label_expr_unchecked(a, vertex_id, graph)
                || matches_label_expr_unchecked(b, vertex_id, graph)
        }
        LabelExpr::Not(e) => !matches_label_expr_unchecked(e, vertex_id, graph),
    }
}

/// Evaluate a label expression against the single label of an edge.
///
/// Unlike `matches_label_expr` (which is vertex-oriented and takes a vertex id),
/// this function takes the actual label string that the edge carries.
/// An edge has exactly one label (or none), so AND is only true if both sides
/// accept that same label, NOT inverts, and Wildcard accepts any non-None label.
///
/// Pre-resolved edge label filter for O(1) integer matching against
/// `EdgeEntry.label_id` / `RevEntry.label_id`.  Avoids per-edge HashMap
/// lookups and string comparisons entirely.
enum ResolvedEdgeLabel {
    /// No label constraint — match all edges.
    Any,
    /// Single label — match when `label_id == id`.
    Exact(u32),
    /// The pattern label doesn't exist in the graph — no edge can match.
    NoMatch,
    /// Complex expression — pre-resolved to integer tree.
    Expr(ResolvedLabelExpr),
}

/// Integer-based mirror of `LabelExpr`.
enum ResolvedLabelExpr {
    Id(u32),
    NoMatch,
    Wildcard,
    And(Box<ResolvedLabelExpr>, Box<ResolvedLabelExpr>),
    Or(Box<ResolvedLabelExpr>, Box<ResolvedLabelExpr>),
    Not(Box<ResolvedLabelExpr>),
}

impl ResolvedEdgeLabel {
    fn matches(&self, label_id: u32) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(id) => label_id == *id,
            Self::NoMatch => false,
            Self::Expr(expr) => expr.matches(label_id),
        }
    }
}

impl ResolvedLabelExpr {
    fn matches(&self, label_id: u32) -> bool {
        match self {
            Self::Id(id) => label_id == *id,
            Self::NoMatch => false,
            Self::Wildcard => label_id != 0,
            Self::And(a, b) => a.matches(label_id) && b.matches(label_id),
            Self::Or(a, b) => a.matches(label_id) || b.matches(label_id),
            Self::Not(e) => !e.matches(label_id),
        }
    }
}

fn resolve_edge_label<M: Memory>(
    edge_pattern: &crate::ast::EdgePattern,
    graph: &PmaGraph<M>,
) -> ResolvedEdgeLabel {
    if let Some(label_expr) = &edge_pattern.label_expr {
        ResolvedEdgeLabel::Expr(resolve_label_expr(label_expr, graph))
    } else if let Some(label) = &edge_pattern.label {
        match graph.label_index.label_id(label) {
            Some(id) => ResolvedEdgeLabel::Exact(id),
            None => ResolvedEdgeLabel::NoMatch,
        }
    } else {
        ResolvedEdgeLabel::Any
    }
}

fn resolve_label_expr<M: Memory>(expr: &LabelExpr, graph: &PmaGraph<M>) -> ResolvedLabelExpr {
    match expr {
        LabelExpr::Name(name) => match graph.label_index.label_id(name) {
            Some(id) => ResolvedLabelExpr::Id(id),
            None => ResolvedLabelExpr::NoMatch,
        },
        LabelExpr::Wildcard => ResolvedLabelExpr::Wildcard,
        LabelExpr::And(a, b) => ResolvedLabelExpr::And(
            Box::new(resolve_label_expr(a, graph)),
            Box::new(resolve_label_expr(b, graph)),
        ),
        LabelExpr::Or(a, b) => ResolvedLabelExpr::Or(
            Box::new(resolve_label_expr(a, graph)),
            Box::new(resolve_label_expr(b, graph)),
        ),
        LabelExpr::Not(e) => ResolvedLabelExpr::Not(Box::new(resolve_label_expr(e, graph))),
    }
}

/// Project all bound variables (sorted by name) for a `RETURN *` / `WITH *` row.
fn project_star_row(bindings: &Bindings) -> Vec<Value> {
    bindings
        .keys()
        .iter()
        .map(|k| binding_value(k, bindings))
        .collect()
}

/// Column names for a `RETURN *` result: sorted variable names from the first row,
/// or empty when there are no rows.
fn star_columns(rows: &[Bindings]) -> Vec<String> {
    rows.first().map_or_else(Vec::new, |b| b.keys())
}

fn binding_value(var: &str, bindings: &Bindings) -> Value {
    match bindings.get(var) {
        Some(Binding::Vertex(id)) => Value::Int64(i64::from(*id)),
        Some(Binding::Edge {
            src, dst, label, ..
        }) => Value::Text(format!(
            "{src}->{dst}:{}",
            label.as_deref().unwrap_or_default()
        )),
        Some(Binding::Value(v)) => v.clone(),
        None => Value::Null,
    }
}

/// SQL LIKE pattern matching. `%` matches zero or more chars; `_` matches exactly one char.
/// If `case_insensitive` is true, both string and pattern are lowercased before matching.
fn like_match(s: &str, pattern: &str, case_insensitive: bool) -> bool {
    let (s, pattern) = if case_insensitive {
        (s.to_lowercase(), pattern.to_lowercase())
    } else {
        (s.to_string(), pattern.to_string())
    };
    // dp[i][j] = true iff s[..i] matches pattern[..j]
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (m, n) = (s.len(), p.len());
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[0][0] = true;
    // dp[0][j]: only `%` can match empty string
    for j in 1..=n {
        if p[j - 1] == '%' {
            dp[0][j] = dp[0][j - 1];
        }
    }
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = match p[j - 1] {
                '%' => dp[i - 1][j] || dp[i][j - 1],
                '_' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && s[i - 1] == c,
            };
        }
    }
    dp[m][n]
}

fn truthy(v: &Value) -> bool {
    if v.is_any_int() {
        return match v {
            Value::Int8(i) => *i != 0,
            Value::Int16(i) => *i != 0,
            Value::Int32(i) => *i != 0,
            Value::Int64(i) => *i != 0,
            Value::Int128(i) => *i != 0,
            Value::Int256(i) => i.0 != ethnum::I256::ZERO,
            Value::Uint8(u) => *u != 0,
            Value::Uint16(u) => *u != 0,
            Value::Uint32(u) => *u != 0,
            Value::Uint64(u) => *u != 0,
            Value::Uint128(u) => *u != 0,
            Value::Uint256(u) => u.0 != ethnum::U256::ZERO,
            _ => unreachable!(),
        };
    }
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Float32(f) => *f != 0.0,
        Value::Float64(f) => *f != 0.0,
        Value::Text(s) => !s.is_empty(),
        Value::Timestamp(ts) => *ts != 0,
        Value::List(items) => !items.is_empty(),
        Value::Path(items) => !items.is_empty(),
        Value::Bytes(b) => !b.is_empty(),
        Value::Date(_) | Value::Time(_) | Value::DateTime(_, _) | Value::Duration(_, _) => true,
        Value::Principal(_) => true,
        Value::Decimal(d) => !d.0.is_zero(),
        _ => false,
    }
}

fn compare_cmp(op: CmpOp, left: &Value, right: &Value) -> bool {
    match compare_values(left, right) {
        Some(ord) => match op {
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Ne => ord != Ordering::Equal,
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Le => ord != Ordering::Greater,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Ge => ord != Ordering::Less,
        },
        None => false,
    }
}

fn eval_unary_op(op: UnaryOp, v: &Value) -> Value {
    // Handle all integer types via helpers.
    if v.is_signed_int() {
        return match op {
            UnaryOp::Neg => {
                if let Value::Int256(i) = v {
                    Value::Int256(gleaph_types::Int256::new(-i.0))
                } else {
                    let val = v.as_i128().unwrap();
                    let negated = val.checked_neg().unwrap_or(i128::MIN);
                    gleaph_types::narrow_signed(negated, v.int_width().unwrap())
                }
            }
            UnaryOp::Pos => v.clone(),
        };
    }
    if v.is_unsigned_int() {
        return match op {
            UnaryOp::Neg => {
                if let Value::Uint256(u) = v {
                    if u.0 == ethnum::U256::ZERO {
                        Value::Int256(gleaph_types::Int256::new(ethnum::I256::ZERO))
                    } else {
                        // Cannot negate large unsigned 256 into signed 256 safely
                        let as_i = u.0.as_i256();
                        if as_i > ethnum::I256::ZERO {
                            Value::Null // too large
                        } else {
                            Value::Int256(gleaph_types::Int256::new(-as_i))
                        }
                    }
                } else {
                    let val = v.as_u128().unwrap();
                    if val == 0 {
                        gleaph_types::narrow_signed(0, v.int_width().unwrap())
                    } else if let Ok(signed) = i128::try_from(val) {
                        let negated = -signed;
                        gleaph_types::narrow_signed(negated, v.int_width().unwrap())
                    } else {
                        Value::Null
                    }
                }
            }
            UnaryOp::Pos => v.clone(),
        };
    }
    match (op, v) {
        (UnaryOp::Neg, Value::Float32(f)) => Value::Float32(-f),
        (UnaryOp::Pos, Value::Float32(f)) => Value::Float32(*f),
        (UnaryOp::Neg, Value::Float64(f)) => Value::Float64(-f),
        (UnaryOp::Pos, Value::Float64(f)) => Value::Float64(*f),
        (UnaryOp::Neg, Value::Decimal(d)) => Value::Decimal(gleaph_types::Decimal::new(-d.0)),
        (UnaryOp::Pos, Value::Decimal(d)) => Value::Decimal(*d),
        _ => Value::Null,
    }
}

fn eval_binary_op(op: BinaryOp, left: &Value, right: &Value) -> Value {
    // ── Integer × Integer (all widths/signedness) ──
    if left.is_any_int() && right.is_any_int() {
        return promote_and_compute_int(op, left, right);
    }

    match (left, right) {
        (Value::Text(a), Value::Text(b)) if op == BinaryOp::Add => Value::Text(format!("{a}{b}")),
        (Value::List(a), Value::List(b)) if op == BinaryOp::Add => {
            let mut result = a.clone();
            result.extend(b.iter().cloned());
            Value::List(result)
        }
        // ── Temporal arithmetic ──
        (Value::Date(d), Value::Duration(m, n)) if op == BinaryOp::Add => {
            add_duration_to_date(*d, *m, *n)
        }
        (Value::Date(d), Value::Duration(m, n)) if op == BinaryOp::Sub => {
            add_duration_to_date(*d, -m, -n)
        }
        (Value::Date(a), Value::Date(b)) if op == BinaryOp::Sub => {
            Value::Duration(0, (*a as i64 - *b as i64) * 86_400_000_000_000)
        }
        (Value::DateTime(s, n), Value::Duration(dm, dn)) if op == BinaryOp::Add => {
            add_duration_to_datetime(*s, *n, *dm, *dn)
        }
        (Value::DateTime(s, n), Value::Duration(dm, dn)) if op == BinaryOp::Sub => {
            add_duration_to_datetime(*s, *n, -dm, -dn)
        }
        (Value::DateTime(s1, n1), Value::DateTime(s2, n2)) if op == BinaryOp::Sub => {
            let nanos1 = *s1 as i128 * 1_000_000_000 + *n1 as i128;
            let nanos2 = *s2 as i128 * 1_000_000_000 + *n2 as i128;
            Value::Duration(0, (nanos1 - nanos2) as i64)
        }
        (Value::Time(a), Value::Duration(0, dn)) if op == BinaryOp::Add => {
            let total = (*a as i128 + *dn as i128).rem_euclid(86_400_000_000_000);
            Value::Time(total as u64)
        }
        (Value::Duration(m1, n1), Value::Duration(m2, n2)) if op == BinaryOp::Add => {
            Value::Duration(m1 + m2, n1 + n2)
        }
        (Value::Duration(m1, n1), Value::Duration(m2, n2)) if op == BinaryOp::Sub => {
            Value::Duration(m1 - m2, n1 - n2)
        }
        // Duration × any-signed-int
        (Value::Duration(m, n), r) if r.is_signed_int() && op == BinaryOp::Mul => {
            let i = r.as_i128().unwrap_or(0) as i64;
            Value::Duration(m.saturating_mul(i as i32), n.saturating_mul(i))
        }
        (l, Value::Duration(m, n)) if l.is_signed_int() && op == BinaryOp::Mul => {
            let i = l.as_i128().unwrap_or(0) as i64;
            Value::Duration((i as i32).saturating_mul(*m), i.saturating_mul(*n))
        }
        // ── Float32 × Float32 arithmetic ──
        (Value::Float32(a), Value::Float32(b)) => match op {
            BinaryOp::Add => Value::Float32(a + b),
            BinaryOp::Sub => Value::Float32(a - b),
            BinaryOp::Mul => Value::Float32(a * b),
            BinaryOp::Div => {
                if *b == 0.0 {
                    Value::Null
                } else {
                    Value::Float32(a / b)
                }
            }
            BinaryOp::Mod => {
                if *b == 0.0 {
                    Value::Null
                } else {
                    Value::Float32(a % b)
                }
            }
        },
        // Float32 × Float64: promote to Float64
        (Value::Float32(a), Value::Float64(b)) => {
            eval_binary_op(op, &Value::Float64(*a as f64), &Value::Float64(*b))
        }
        (Value::Float64(a), Value::Float32(b)) => {
            eval_binary_op(op, &Value::Float64(*a), &Value::Float64(*b as f64))
        }
        // Float32 × Integer: promote int to Float32
        (Value::Float32(a), r) if r.is_any_int() => {
            let b = r.as_f64().unwrap_or(0.0) as f32;
            eval_binary_op(op, &Value::Float32(*a), &Value::Float32(b))
        }
        (l, Value::Float32(b)) if l.is_any_int() => {
            let a = l.as_f64().unwrap_or(0.0) as f32;
            eval_binary_op(op, &Value::Float32(a), &Value::Float32(*b))
        }
        // Float32 × Decimal: promote to Decimal
        (Value::Float32(a), Value::Decimal(d)) => {
            let promoted = rust_decimal::Decimal::try_from(*a as f64);
            match promoted {
                Ok(p) => eval_binary_op(
                    op,
                    &Value::Decimal(gleaph_types::Decimal::new(p)),
                    &Value::Decimal(*d),
                ),
                Err(_) => Value::Null,
            }
        }
        (Value::Decimal(d), Value::Float32(b)) => {
            let promoted = rust_decimal::Decimal::try_from(*b as f64);
            match promoted {
                Ok(p) => eval_binary_op(
                    op,
                    &Value::Decimal(*d),
                    &Value::Decimal(gleaph_types::Decimal::new(p)),
                ),
                Err(_) => Value::Null,
            }
        }
        // ── Decimal arithmetic ──
        (Value::Decimal(a), Value::Decimal(b)) => match op {
            BinaryOp::Add => Value::Decimal(gleaph_types::Decimal::new(a.0 + b.0)),
            BinaryOp::Sub => Value::Decimal(gleaph_types::Decimal::new(a.0 - b.0)),
            BinaryOp::Mul => Value::Decimal(gleaph_types::Decimal::new(a.0 * b.0)),
            BinaryOp::Div => {
                if b.0.is_zero() {
                    Value::Null
                } else {
                    Value::Decimal(gleaph_types::Decimal::new(a.0 / b.0))
                }
            }
            BinaryOp::Mod => {
                if b.0.is_zero() {
                    Value::Null
                } else {
                    Value::Decimal(gleaph_types::Decimal::new(a.0 % b.0))
                }
            }
        },
        // Any signed int × Decimal
        (l, Value::Decimal(d)) if l.is_signed_int() => {
            let promoted =
                gleaph_types::Decimal::new(rust_decimal::Decimal::from(l.as_i128().unwrap_or(0)));
            eval_binary_op(op, &Value::Decimal(promoted), &Value::Decimal(*d))
        }
        (Value::Decimal(d), r) if r.is_signed_int() => {
            let promoted =
                gleaph_types::Decimal::new(rust_decimal::Decimal::from(r.as_i128().unwrap_or(0)));
            eval_binary_op(op, &Value::Decimal(*d), &Value::Decimal(promoted))
        }
        // Any unsigned int × Decimal
        (l, Value::Decimal(d)) if l.is_unsigned_int() => {
            let promoted =
                gleaph_types::Decimal::new(rust_decimal::Decimal::from(l.as_u128().unwrap_or(0)));
            eval_binary_op(op, &Value::Decimal(promoted), &Value::Decimal(*d))
        }
        (Value::Decimal(d), r) if r.is_unsigned_int() => {
            let promoted =
                gleaph_types::Decimal::new(rust_decimal::Decimal::from(r.as_u128().unwrap_or(0)));
            eval_binary_op(op, &Value::Decimal(*d), &Value::Decimal(promoted))
        }
        _ => {
            // Fallback: promote to f64 for any numeric mix (int×float, etc.)
            let to_f = |v: &Value| -> Option<f64> {
                v.as_f64().or_else(|| match v {
                    Value::Decimal(d) => d.to_f64(),
                    _ => None,
                })
            };
            match (to_f(left), to_f(right)) {
                (Some(a), Some(b)) => match op {
                    BinaryOp::Add => Value::Float64(a + b),
                    BinaryOp::Sub => Value::Float64(a - b),
                    BinaryOp::Mul => Value::Float64(a * b),
                    BinaryOp::Div => {
                        if b == 0.0 {
                            Value::Null
                        } else {
                            Value::Float64(a / b)
                        }
                    }
                    BinaryOp::Mod => {
                        if b == 0.0 {
                            Value::Null
                        } else {
                            Value::Float64(a % b)
                        }
                    }
                },
                _ => Value::Null,
            }
        }
    }
}

/// Compute `left op right` for two integer values of any width/signedness.
fn promote_and_compute_int(op: BinaryOp, left: &Value, right: &Value) -> Value {
    // Same type, same width — fast path.
    if std::mem::discriminant(left) == std::mem::discriminant(right) {
        return compute_same_int(op, left, right);
    }

    // Both signed → promote to wider i128, compute, narrow to wider width.
    if left.is_signed_int() && right.is_signed_int() {
        // 256-bit path
        if matches!(left, Value::Int256(_)) || matches!(right, Value::Int256(_)) {
            let a = match left {
                Value::Int256(v) => v.0,
                _ => ethnum::I256::from(left.as_i128().unwrap()),
            };
            let b = match right {
                Value::Int256(v) => v.0,
                _ => ethnum::I256::from(right.as_i128().unwrap()),
            };
            return compute_i256(op, a, b);
        }
        let a = left.as_i128().unwrap();
        let b = right.as_i128().unwrap();
        let w = left.int_width().unwrap().max(right.int_width().unwrap());
        return compute_i128_narrow(op, a, b, w);
    }

    // Both unsigned → promote to wider u128.
    if left.is_unsigned_int() && right.is_unsigned_int() {
        if matches!(left, Value::Uint256(_)) || matches!(right, Value::Uint256(_)) {
            let a = match left {
                Value::Uint256(v) => v.0,
                _ => ethnum::U256::from(left.as_u128().unwrap()),
            };
            let b = match right {
                Value::Uint256(v) => v.0,
                _ => ethnum::U256::from(right.as_u128().unwrap()),
            };
            return compute_u256(op, a, b);
        }
        let a = left.as_u128().unwrap();
        let b = right.as_u128().unwrap();
        let w = left.int_width().unwrap().max(right.int_width().unwrap());
        return compute_u128_narrow(op, a, b, w);
    }

    // Mixed signed × unsigned → promote to i128 (wider signed), or i256 for 256-bit.
    let w = left.int_width().unwrap().max(right.int_width().unwrap());
    if w == 256
        || matches!(left, Value::Int256(_) | Value::Uint256(_))
        || matches!(right, Value::Int256(_) | Value::Uint256(_))
    {
        // Promote everything to I256 for mixed 256-bit.
        let to_i256 = |v: &Value| -> ethnum::I256 {
            match v {
                Value::Int256(i) => i.0,
                Value::Uint256(u) => u.0.as_i256(),
                _ if v.is_signed_int() => ethnum::I256::from(v.as_i128().unwrap()),
                _ => ethnum::I256::from(v.as_u128().unwrap() as i128),
            }
        };
        return compute_i256(op, to_i256(left), to_i256(right));
    }

    // Both fit in i128: promote unsigned to signed at wider width.
    let (s, u) = if left.is_signed_int() {
        (left.as_i128().unwrap(), right.as_u128().unwrap())
    } else {
        (right.as_i128().unwrap(), left.as_u128().unwrap())
    };
    // Safe promotion: u128 might overflow i128.
    if let Ok(u_as_i) = i128::try_from(u) {
        compute_i128_narrow(op, s, u_as_i, w.max(64))
    } else {
        // Overflow: fallback to f64.
        eval_binary_op(
            op,
            &Value::Float64(left.as_f64().unwrap_or(0.0)),
            &Value::Float64(right.as_f64().unwrap_or(0.0)),
        )
    }
}

/// Same-type integer arithmetic (fast path).
fn compute_same_int(op: BinaryOp, left: &Value, right: &Value) -> Value {
    macro_rules! arith {
        ($a:expr, $b:expr, $ty:ident, $variant:path) => {
            match op {
                BinaryOp::Add => $a.checked_add(*$b).map($variant).unwrap_or(Value::Null),
                BinaryOp::Sub => $a.checked_sub(*$b).map($variant).unwrap_or(Value::Null),
                BinaryOp::Mul => $a.checked_mul(*$b).map($variant).unwrap_or(Value::Null),
                BinaryOp::Div => {
                    if *$b == 0 as $ty {
                        Value::Null
                    } else {
                        $a.checked_div(*$b).map($variant).unwrap_or(Value::Null)
                    }
                }
                BinaryOp::Mod => {
                    if *$b == 0 as $ty {
                        Value::Null
                    } else {
                        $a.checked_rem(*$b).map($variant).unwrap_or(Value::Null)
                    }
                }
            }
        };
    }
    match (left, right) {
        (Value::Int8(a), Value::Int8(b)) => arith!(a, b, i8, Value::Int8),
        (Value::Int16(a), Value::Int16(b)) => arith!(a, b, i16, Value::Int16),
        (Value::Int32(a), Value::Int32(b)) => arith!(a, b, i32, Value::Int32),
        (Value::Int64(a), Value::Int64(b)) => arith!(a, b, i64, Value::Int64),
        (Value::Int128(a), Value::Int128(b)) => arith!(a, b, i128, Value::Int128),
        (Value::Int256(a), Value::Int256(b)) => compute_i256(op, a.0, b.0),
        (Value::Uint8(a), Value::Uint8(b)) => arith!(a, b, u8, Value::Uint8),
        (Value::Uint16(a), Value::Uint16(b)) => arith!(a, b, u16, Value::Uint16),
        (Value::Uint32(a), Value::Uint32(b)) => arith!(a, b, u32, Value::Uint32),
        (Value::Uint64(a), Value::Uint64(b)) => arith!(a, b, u64, Value::Uint64),
        (Value::Uint128(a), Value::Uint128(b)) => arith!(a, b, u128, Value::Uint128),
        (Value::Uint256(a), Value::Uint256(b)) => compute_u256(op, a.0, b.0),
        _ => Value::Null,
    }
}

fn compute_i128_narrow(op: BinaryOp, a: i128, b: i128, width: u16) -> Value {
    let result = match op {
        BinaryOp::Add => a.checked_add(b),
        BinaryOp::Sub => a.checked_sub(b),
        BinaryOp::Mul => a.checked_mul(b),
        BinaryOp::Div => {
            if b == 0 {
                return Value::Null;
            } else {
                a.checked_div(b)
            }
        }
        BinaryOp::Mod => {
            if b == 0 {
                return Value::Null;
            } else {
                a.checked_rem(b)
            }
        }
    };
    match result {
        Some(v) => gleaph_types::narrow_signed(v, width),
        None => Value::Null,
    }
}

fn compute_u128_narrow(op: BinaryOp, a: u128, b: u128, width: u16) -> Value {
    let result = match op {
        BinaryOp::Add => a.checked_add(b),
        BinaryOp::Sub => a.checked_sub(b),
        BinaryOp::Mul => a.checked_mul(b),
        BinaryOp::Div => {
            if b == 0 {
                return Value::Null;
            } else {
                a.checked_div(b)
            }
        }
        BinaryOp::Mod => {
            if b == 0 {
                return Value::Null;
            } else {
                a.checked_rem(b)
            }
        }
    };
    match result {
        Some(v) => gleaph_types::narrow_unsigned(v, width),
        None => Value::Null,
    }
}

fn compute_i256(op: BinaryOp, a: ethnum::I256, b: ethnum::I256) -> Value {
    let result = match op {
        BinaryOp::Add => a.checked_add(b),
        BinaryOp::Sub => a.checked_sub(b),
        BinaryOp::Mul => a.checked_mul(b),
        BinaryOp::Div => {
            if b == ethnum::I256::ZERO {
                return Value::Null;
            } else {
                a.checked_div(b)
            }
        }
        BinaryOp::Mod => {
            if b == ethnum::I256::ZERO {
                return Value::Null;
            } else {
                a.checked_rem(b)
            }
        }
    };
    match result {
        Some(v) => Value::Int256(gleaph_types::Int256::new(v)),
        None => Value::Null,
    }
}

fn compute_u256(op: BinaryOp, a: ethnum::U256, b: ethnum::U256) -> Value {
    let result = match op {
        BinaryOp::Add => a.checked_add(b),
        BinaryOp::Sub => a.checked_sub(b),
        BinaryOp::Mul => a.checked_mul(b),
        BinaryOp::Div => {
            if b == ethnum::U256::ZERO {
                return Value::Null;
            } else {
                a.checked_div(b)
            }
        }
        BinaryOp::Mod => {
            if b == ethnum::U256::ZERO {
                return Value::Null;
            } else {
                a.checked_rem(b)
            }
        }
    };
    match result {
        Some(v) => Value::Uint256(gleaph_types::Uint256::new(v)),
        None => Value::Null,
    }
}

fn add_duration_to_date(days: i32, months: i32, nanos: i64) -> Value {
    use crate::temporal::{days_in_month, days_to_ymd, ymd_to_days};
    let (y, m, d) = days_to_ymd(days);
    let total_months = y as i64 * 12 + (m as i64 - 1) + months as i64;
    let new_y = (total_months.div_euclid(12)) as i32;
    let new_m = (total_months.rem_euclid(12) + 1) as u32;
    let new_d = d.min(days_in_month(new_y, new_m));
    let new_days = ymd_to_days(new_y, new_m, new_d).unwrap_or(days);
    let extra_days = nanos / 86_400_000_000_000;
    Value::Date(new_days + extra_days as i32)
}

fn add_duration_to_datetime(secs: i64, sub: u32, months: i32, dur_nanos: i64) -> Value {
    use crate::temporal::{days_in_month, days_to_ymd, ymd_to_days};
    // Decompose datetime into date + time-of-day
    let day_secs = secs.rem_euclid(86400);
    let days = ((secs - day_secs) / 86400) as i32;
    let (y, m, d) = days_to_ymd(days);
    // Add calendar months
    let total_months = y as i64 * 12 + (m as i64 - 1) + months as i64;
    let new_y = (total_months.div_euclid(12)) as i32;
    let new_m = (total_months.rem_euclid(12) + 1) as u32;
    let new_d = d.min(days_in_month(new_y, new_m));
    let new_days = ymd_to_days(new_y, new_m, new_d).unwrap_or(days);
    // Add nanos component
    let total_nanos =
        new_days as i64 * 86_400_000_000_000 + day_secs * 1_000_000_000 + sub as i64 + dur_nanos;
    let new_secs = total_nanos.div_euclid(1_000_000_000);
    let new_sub = total_nanos.rem_euclid(1_000_000_000) as u32;
    Value::DateTime(new_secs, new_sub)
}

fn eval_function_call_expr<M: Memory>(
    name: &str,
    args: &[Expr],
    bindings: &Bindings,
    graph: &PmaGraph<M>,
) -> Value {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        // caller() — returns the IC caller principal (Gleaph extension)
        "caller" => {
            if !args.is_empty() {
                return Value::Null;
            }
            CALLER_PRINCIPAL.with(|c| c.borrow().clone().unwrap_or(Value::Null))
        }
        "id" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Vertex(id)) => Value::Int64(i64::from(*id)),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        "labels" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Vertex(id)) => {
                        let labels = graph
                            .overlay_snapshot()
                            .vertex_labels
                            .into_iter()
                            .find_map(|(vid, labels)| (vid == *id).then_some(labels))
                            .unwrap_or_default()
                            .into_iter()
                            .map(Value::Text)
                            .collect();
                        Value::List(labels)
                    }
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        "type" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Edge { label, .. }) => label
                        .as_deref()
                        .map(|s| Value::Text(s.to_string()))
                        .unwrap_or(Value::Null),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        "element_id" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Vertex(id)) => Value::Int64(i64::from(*id)),
                    Some(Binding::Edge { src, dst, .. }) => Value::Text(format!("{src}->{dst}")),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        // source(e) — source vertex of an edge (GQL standard name)
        "source" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Edge { src, .. }) => Value::Int64(i64::from(*src)),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        // destination(e) — destination vertex of an edge (GQL standard name)
        "destination" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Edge { dst, .. }) => Value::Int64(i64::from(*dst)),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        // gleaph_weight(e) — structural weight of an edge (canonical name)
        // weight(e) — deprecated alias
        "gleaph_weight" | "weight" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Edge { weight, .. }) => Value::Float64(*weight as f64),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        // gleaph_timestamp(e) — structural timestamp of an edge (canonical name)
        // timestamp(e) — deprecated alias
        "gleaph_timestamp" | "timestamp" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Edge { timestamp, .. }) => Value::Timestamp(*timestamp),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        // edge_id(e) — stable PMA edge identifier for an edge variable (0 = unassigned)
        "edge_id" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Edge { edge_id, .. }) => Value::Int64(i64::from(*edge_id)),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        // keys(n) — list of property key names for a vertex/edge/record
        "keys" => {
            if args.len() != 1 {
                return Value::Null;
            }
            let val = eval_expr(&args[0], bindings, graph);
            if let Value::List(ref pairs) = val {
                // Record encoding: list of [Text(key), value] pairs
                let keys = pairs
                    .iter()
                    .filter_map(|pair| {
                        if let Value::List(kv) = pair {
                            kv.first().cloned()
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                if !keys.is_empty() {
                    return Value::List(keys);
                }
            }
            // Fall back: check bindings for vertex/edge
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Vertex(id)) => {
                        let props = graph.get_vertex_props(*id).unwrap_or_default();
                        Value::List(props.into_iter().map(|(k, _)| Value::Text(k)).collect())
                    }
                    Some(Binding::Edge {
                        src, dst, label, ..
                    }) => {
                        if let Some(edge) = graph.edge_record(*src, *dst, label.as_deref()) {
                            Value::List(
                                edge.props
                                    .into_iter()
                                    .map(|(k, _)| Value::Text(k))
                                    .collect(),
                            )
                        } else {
                            Value::List(vec![])
                        }
                    }
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        // properties(n) — record value (all key-value pairs) for a vertex/edge
        "properties" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Vertex(id)) => {
                        let props = graph.get_vertex_props(*id).unwrap_or_default();
                        Value::List(
                            props
                                .into_iter()
                                .map(|(k, v)| Value::List(vec![Value::Text(k), v]))
                                .collect(),
                        )
                    }
                    Some(Binding::Edge {
                        src, dst, label, ..
                    }) => {
                        if let Some(edge) = graph.edge_record(*src, *dst, label.as_deref()) {
                            Value::List(
                                edge.props
                                    .into_iter()
                                    .map(|(k, v)| Value::List(vec![Value::Text(k), v]))
                                    .collect(),
                            )
                        } else {
                            Value::List(vec![])
                        }
                    }
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        "property_exists" => {
            if args.len() != 2 {
                return Value::Null;
            }
            let property = match &args[1] {
                Expr::Literal(Value::Text(s)) => s.clone(),
                Expr::Variable(_) => match eval_expr(&args[1], bindings, graph) {
                    Value::Text(s) => s,
                    _ => return Value::Null,
                },
                _ => match eval_expr(&args[1], bindings, graph) {
                    Value::Text(s) => s,
                    _ => return Value::Null,
                },
            };
            match &args[0] {
                Expr::Variable(v) => match bindings.get(v) {
                    Some(Binding::Vertex(id)) => {
                        let props = graph.get_vertex_props(*id).unwrap_or_default();
                        Value::Bool(props.iter().any(|(k, _)| k == &property))
                    }
                    Some(Binding::Edge {
                        src, dst, label, ..
                    }) => {
                        if let Some(edge) = graph.edge_record(*src, *dst, label.as_deref()) {
                            Value::Bool(edge.props.iter().any(|(k, _)| k == &property))
                        } else {
                            Value::Bool(false)
                        }
                    }
                    _ => Value::Bool(false),
                },
                _ => Value::Bool(false),
            }
        }
        _ => {
            let values = args
                .iter()
                .map(|a| eval_expr(a, bindings, graph))
                .collect::<Vec<_>>();
            eval_function_call(&lower, &values)
        }
    }
}

fn eval_function_call(name_lower: &str, args: &[Value]) -> Value {
    match name_lower {
        "coalesce" => args
            .iter()
            .find(|v| !matches!(v, Value::Null))
            .cloned()
            .unwrap_or(Value::Null),
        "nullif" => {
            if args.len() != 2 {
                return Value::Null;
            }
            if compare_values(&args[0], &args[1]) == Some(Ordering::Equal) {
                Value::Null
            } else {
                args[0].clone()
            }
        }
        "size" | "length" | "cardinality" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Text(s) => Value::Int64(s.chars().count() as i64),
                Value::List(v) => Value::Int64(v.len() as i64),
                Value::Path(v) => {
                    let hops = v
                        .iter()
                        .filter(|e| matches!(e, gleaph_types::PathElement::Edge { .. }))
                        .count();
                    Value::Int64(hops as i64)
                }
                _ => Value::Null,
            }
        }
        "upper" => unary_text(args, |s| Value::Text(s.to_uppercase())),
        "lower" => unary_text(args, |s| Value::Text(s.to_lowercase())),
        "trim" => unary_text(args, |s| Value::Text(s.trim().to_string())),
        "ltrim" => unary_text(args, |s| Value::Text(s.trim_start().to_string())),
        "rtrim" => unary_text(args, |s| Value::Text(s.trim_end().to_string())),
        "left" => {
            if args.len() != 2 {
                return Value::Null;
            }
            match (&args[0], args[1].as_i64()) {
                (Value::Text(s), Some(n)) if n >= 0 => {
                    Value::Text(s.chars().take(n as usize).collect())
                }
                _ => Value::Null,
            }
        }
        "right" => {
            if args.len() != 2 {
                return Value::Null;
            }
            match (&args[0], args[1].as_i64()) {
                (Value::Text(s), Some(n)) if n >= 0 => {
                    let chars = s.chars().collect::<Vec<_>>();
                    let take = (n as usize).min(chars.len());
                    Value::Text(chars[chars.len() - take..].iter().collect())
                }
                _ => Value::Null,
            }
        }
        "starts_with" => binary_text_bool(args, |a, b| a.starts_with(b)),
        "ends_with" => binary_text_bool(args, |a, b| a.ends_with(b)),
        "contains" => binary_text_bool(args, |a, b| a.contains(b)),
        "replace" => {
            if args.len() != 3 {
                return Value::Null;
            }
            match (&args[0], &args[1], &args[2]) {
                (Value::Text(s), Value::Text(from), Value::Text(to)) => {
                    Value::Text(s.replace(from, to))
                }
                _ => Value::Null,
            }
        }
        "substring" => {
            if !(2..=3).contains(&args.len()) {
                return Value::Null;
            }
            match (&args[0], args[1].as_i64(), args.get(2)) {
                (Value::Text(s), Some(start), len_opt) if start >= 0 => {
                    let chars = s.chars().collect::<Vec<_>>();
                    let start = (start as usize).min(chars.len());
                    let end = match len_opt {
                        Some(v) => match v.as_i64() {
                            Some(len) if len >= 0 => (start + len as usize).min(chars.len()),
                            _ => return Value::Null,
                        },
                        None => chars.len(),
                    };
                    Value::Text(chars[start..end].iter().collect())
                }
                _ => Value::Null,
            }
        }
        "abs" => unary_numeric(args, |i| i.abs(), |f| f.abs()),
        "floor" => unary_numeric(args, |i| i, |f| f.floor()),
        "ceil" | "ceiling" => unary_numeric(args, |i| i, |f| f.ceil()),
        "round" => unary_numeric(args, |i| i, |f| f.round()),
        "tostring" => {
            if args.len() != 1 {
                return Value::Null;
            }
            // Use cast_value(Text) which handles all types including all integers.
            cast_value(args[0].clone(), ValueType::Text)
        }
        "tointeger" => {
            if args.len() != 1 {
                return Value::Null;
            }
            let v = &args[0];
            if v.is_any_int() {
                if let Some(i) = v.as_i128() {
                    return gleaph_types::narrow_signed(i, 64);
                }
                if let Some(u) = v.as_u128() {
                    return if let Ok(i) = i64::try_from(u) {
                        Value::Int64(i)
                    } else {
                        Value::Null
                    };
                }
                return Value::Null;
            }
            match v {
                Value::Float32(f) => Value::Int64(*f as i64),
                Value::Float64(f) => Value::Int64(*f as i64),
                Value::Text(s) => s.parse::<i64>().map(Value::Int64).unwrap_or(Value::Null),
                Value::Timestamp(t) => i64::try_from(*t).map(Value::Int64).unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        "tofloat" => {
            if args.len() != 1 {
                return Value::Null;
            }
            let v = &args[0];
            if let Some(f) = v.as_f64() {
                return Value::Float64(f);
            }
            match v {
                Value::Float32(f) => Value::Float64(*f as f64),
                Value::Float64(f) => Value::Float64(*f),
                Value::Text(s) => s.parse::<f64>().map(Value::Float64).unwrap_or(Value::Null),
                Value::Timestamp(v) => Value::Float64(*v as f64),
                _ => Value::Null,
            }
        }
        "head" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) => v.first().cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        "tail" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) if !v.is_empty() => Value::List(v[1..].to_vec()),
                Value::List(_) => Value::List(Vec::new()),
                _ => Value::Null,
            }
        }
        "range" => {
            if !(2..=3).contains(&args.len()) {
                return Value::Null;
            }
            let (s, e) = match (args[0].as_i64(), args[1].as_i64()) {
                (Some(s), Some(e)) => (s, e),
                _ => return Value::Null,
            };
            let step = match args.get(2) {
                None => {
                    if s <= e {
                        1i64
                    } else {
                        -1i64
                    }
                }
                Some(v) => match v.as_i64() {
                    Some(st) if st != 0 => st,
                    _ => return Value::Null,
                },
            };
            let mut result = Vec::new();
            let mut cur = s;
            if step > 0 {
                while cur <= e {
                    result.push(Value::Int64(cur));
                    cur = cur.saturating_add(step);
                }
            } else {
                while cur >= e {
                    result.push(Value::Int64(cur));
                    cur = cur.saturating_add(step);
                }
            }
            Value::List(result)
        }
        // Math functions
        "sqrt" => unary_float(args, |f| f.sqrt()),
        "power" | "pow" => {
            if args.len() != 2 {
                return Value::Null;
            }
            match (numeric_as_f64(&args[0]), numeric_as_f64(&args[1])) {
                (Some(base), Some(exp)) => Value::Float64(base.powf(exp)),
                _ => Value::Null,
            }
        }
        "exp" => unary_float(args, |f| f.exp()),
        "ln" => unary_float(args, |f| f.ln()),
        "log" => {
            // log(x) → natural log; log(base, x) → log with base
            if args.len() == 1 {
                unary_float(args, |f| f.ln())
            } else if args.len() == 2 {
                match (numeric_as_f64(&args[0]), numeric_as_f64(&args[1])) {
                    (Some(base), Some(x)) if base > 0.0 && base != 1.0 => {
                        Value::Float64(x.ln() / base.ln())
                    }
                    _ => Value::Null,
                }
            } else {
                Value::Null
            }
        }
        "log10" => unary_float(args, |f| f.log10()),
        "log2" => unary_float(args, |f| f.log2()),
        "sin" => unary_float(args, |f| f.sin()),
        "cos" => unary_float(args, |f| f.cos()),
        "tan" => unary_float(args, |f| f.tan()),
        "asin" => unary_float(args, |f| f.asin()),
        "acos" => unary_float(args, |f| f.acos()),
        "atan" => unary_float(args, |f| f.atan()),
        "atan2" => {
            if args.len() != 2 {
                return Value::Null;
            }
            match (numeric_as_f64(&args[0]), numeric_as_f64(&args[1])) {
                (Some(y), Some(x)) => Value::Float64(y.atan2(x)),
                _ => Value::Null,
            }
        }
        "degrees" => unary_float(args, |f| f.to_degrees()),
        "radians" => unary_float(args, |f| f.to_radians()),
        "sinh" => unary_float(args, |f| f.sinh()),
        "cosh" => unary_float(args, |f| f.cosh()),
        "tanh" => unary_float(args, |f| f.tanh()),
        "mod" => {
            if args.len() != 2 {
                return Value::Null;
            }
            match (&args[0], &args[1]) {
                (a, b)
                    if a.as_i64().is_some() && b.as_i64().is_some() && b.as_i64().unwrap() != 0 =>
                {
                    Value::Int64(a.as_i64().unwrap() % b.as_i64().unwrap())
                }
                _ => match (numeric_as_f64(&args[0]), numeric_as_f64(&args[1])) {
                    (Some(a), Some(b)) if b != 0.0 => Value::Float64(a % b),
                    _ => Value::Null,
                },
            }
        }
        "cot" => unary_float(args, |f| 1.0 / f.tan()),
        "pi" => {
            if args.is_empty() {
                Value::Float64(std::f64::consts::PI)
            } else {
                Value::Null
            }
        }
        "e" => {
            if args.is_empty() {
                Value::Float64(std::f64::consts::E)
            } else {
                Value::Null
            }
        }
        // String functions
        "btrim" => unary_text(args, |s| Value::Text(s.trim().to_string())),
        "char_length" | "character_length" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Text(s) => Value::Int64(s.chars().count() as i64),
                _ => Value::Null,
            }
        }
        "byte_length" | "octet_length" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Text(s) => Value::Int64(s.len() as i64),
                Value::Bytes(b) => Value::Int64(b.len() as i64),
                _ => Value::Null,
            }
        }
        "to_hex" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Bytes(b) => {
                    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
                    Value::Text(hex)
                }
                _ => Value::Null,
            }
        }
        "from_hex" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Text(s) => {
                    let s = s
                        .strip_prefix("0x")
                        .or_else(|| s.strip_prefix("0X"))
                        .unwrap_or(s);
                    if s.len() % 2 != 0 {
                        return Value::Null;
                    }
                    let mut out = Vec::with_capacity(s.len() / 2);
                    for chunk in s.as_bytes().chunks(2) {
                        let hi = hex_char_to_nibble(chunk[0]);
                        let lo = hex_char_to_nibble(chunk[1]);
                        match (hi, lo) {
                            (Some(h), Some(l)) => out.push(h << 4 | l),
                            _ => return Value::Null,
                        }
                    }
                    Value::Bytes(out)
                }
                _ => Value::Null,
            }
        }
        "normalize" => {
            if args.is_empty() || args.len() > 2 {
                return Value::Null;
            }
            let form = if args.len() == 2 {
                match &args[1] {
                    Value::Text(s) => s.to_ascii_uppercase(),
                    _ => return Value::Null,
                }
            } else {
                "NFC".to_string()
            };
            match &args[0] {
                Value::Text(s) => {
                    use unicode_normalization::UnicodeNormalization;
                    let normalized = match form.as_str() {
                        "NFC" => s.nfc().collect::<String>(),
                        "NFD" => s.nfd().collect::<String>(),
                        "NFKC" => s.nfkc().collect::<String>(),
                        "NFKD" => s.nfkd().collect::<String>(),
                        _ => return Value::Null,
                    };
                    Value::Text(normalized)
                }
                _ => Value::Null,
            }
        }
        "split" => {
            if args.len() != 2 {
                return Value::Null;
            }
            match (&args[0], &args[1]) {
                (Value::Text(s), Value::Text(sep)) => Value::List(
                    s.split(sep.as_str())
                        .map(|p| Value::Text(p.to_string()))
                        .collect(),
                ),
                _ => Value::Null,
            }
        }
        "reverse" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Text(s) => Value::Text(s.chars().rev().collect()),
                Value::List(v) => {
                    let mut rev = v.clone();
                    rev.reverse();
                    Value::List(rev)
                }
                _ => Value::Null,
            }
        }
        // Element ID (alias for id - returns the vertex/edge ID)
        "element_id" => {
            if args.len() != 1 {
                return Value::Null;
            }
            // element_id for a text-encoded ID returns the text form, for int returns int
            match &args[0] {
                Value::Int64(i) => Value::Int64(*i),
                Value::Text(s) => Value::Text(s.clone()),
                _ => Value::Null,
            }
        }
        // Predicates that can also be function-style
        "all_different" => {
            let mut seen = BTreeSet::new();
            Value::Bool(args.iter().all(|v| seen.insert(format!("{v:?}"))))
        }
        "same" => {
            if args.is_empty() {
                return Value::Bool(true);
            }
            let first = format!("{:?}", args[0]);
            Value::Bool(args.iter().all(|v| format!("{v:?}") == first))
        }
        "property_exists" => {
            // Handled in eval_function_call_expr by direct binding inspection
            // If called here with values, just check non-null
            if args.len() != 1 {
                return Value::Null;
            }
            Value::Bool(!matches!(args[0], Value::Null))
        }
        // ── Temporal functions (§20.27): Temporal functions ──────────────────────────────────────────
        "current_timestamp" => {
            if !args.is_empty() {
                return Value::Null;
            }
            Value::Int64(temporal_now_nanos())
        }
        "current_date" => {
            if !args.is_empty() {
                return Value::Null;
            }
            let now = temporal_now_nanos();
            if now == 0 {
                return Value::Null;
            }
            let secs = now / 1_000_000_000;
            Value::Date((secs / 86400) as i32)
        }
        "date" => {
            // date(string) → Date value via ISO parse
            match args.first() {
                Some(Value::Text(s)) => crate::temporal::parse_date(s)
                    .map(Value::Date)
                    .unwrap_or(Value::Null),
                Some(v) if v.as_i64().is_some() => Value::Date(v.as_i64().unwrap() as i32), // epoch days
                _ => Value::Null,
            }
        }
        "duration_between" => {
            // duration_between(ts1, ts2) — difference in nanoseconds.
            if args.len() != 2 {
                return Value::Null;
            }
            match (&args[0], &args[1]) {
                (a, b) if a.as_i64().is_some() && b.as_i64().is_some() => {
                    Value::Int64(b.as_i64().unwrap() - a.as_i64().unwrap())
                }
                _ => Value::Null,
            }
        }
        "localdatetime" | "localtimestamp" | "localtime" => {
            if args.is_empty() {
                Value::Int64(temporal_now_nanos())
            } else {
                Value::Null
            }
        }
        // ── List utility functions ─────────────────────────────────────────────
        "last" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) => v.last().cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        "sort" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) => {
                    let mut sorted = v.clone();
                    sorted.sort_by(|a, b| compare_values(a, b).unwrap_or(Ordering::Equal));
                    Value::List(sorted)
                }
                _ => Value::Null,
            }
        }
        "append" => {
            if args.len() != 2 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) => {
                    let mut result = v.clone();
                    result.push(args[1].clone());
                    Value::List(result)
                }
                _ => Value::Null,
            }
        }
        "prepend" => {
            if args.len() != 2 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) => {
                    let mut result = vec![args[1].clone()];
                    result.extend(v.iter().cloned());
                    Value::List(result)
                }
                _ => Value::Null,
            }
        }
        // list_sum/list_avg/list_min/list_max over a list (scalar, not aggregate)
        // Note: "sum"/"avg"/"min"/"max" are reserved keywords for group aggregation.
        "list_sum" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) => {
                    let mut total_i = 0i64;
                    let mut has_float = false;
                    let mut total_f = 0.0f64;
                    let mut all_fit_i32 = true;
                    for item in v {
                        match item {
                            Value::Float32(f) => {
                                has_float = true;
                                total_f += *f as f64;
                            }
                            Value::Float64(f) => {
                                has_float = true;
                                total_f += f;
                            }
                            other => {
                                if let Some(i) = other.as_i64() {
                                    total_i = total_i.saturating_add(i);
                                    total_f += i as f64;
                                    if !matches!(
                                        other,
                                        Value::Int8(_) | Value::Int16(_) | Value::Int32(_)
                                    ) {
                                        all_fit_i32 = false;
                                    }
                                } else {
                                    all_fit_i32 = false;
                                }
                            }
                        }
                    }
                    if has_float {
                        Value::Float64(total_f)
                    } else if all_fit_i32 {
                        if let Ok(v32) = i32::try_from(total_i) {
                            Value::Int32(v32)
                        } else {
                            Value::Int64(total_i)
                        }
                    } else {
                        Value::Int64(total_i)
                    }
                }
                _ => Value::Null,
            }
        }
        "list_avg" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) => {
                    if v.is_empty() {
                        return Value::Null;
                    }
                    let mut total = 0.0f64;
                    let mut count = 0usize;
                    for item in v {
                        match item {
                            Value::Float32(f) => {
                                total += *f as f64;
                                count += 1;
                            }
                            Value::Float64(f) => {
                                total += f;
                                count += 1;
                            }
                            other => {
                                if let Some(i) = other.as_i64() {
                                    total += i as f64;
                                    count += 1;
                                }
                            }
                        }
                    }
                    if count == 0 {
                        Value::Null
                    } else {
                        Value::Float64(total / count as f64)
                    }
                }
                _ => Value::Null,
            }
        }
        // min/max over a list (scalar, not aggregate)
        "list_min" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) => v
                    .iter()
                    .filter(|x| !matches!(x, Value::Null))
                    .min_by(|a, b| compare_values(a, b).unwrap_or(Ordering::Equal))
                    .cloned()
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        "list_max" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::List(v) => v
                    .iter()
                    .filter(|x| !matches!(x, Value::Null))
                    .max_by(|a, b| compare_values(a, b).unwrap_or(Ordering::Equal))
                    .cloned()
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        // ── Temporal extraction functions ──
        "year" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Date(d) => {
                    let (y, _, _) = crate::temporal::days_to_ymd(*d);
                    Value::Int64(y as i64)
                }
                Value::DateTime(secs, _) => {
                    let day_secs = secs.rem_euclid(86400);
                    let days = ((secs - day_secs) / 86400) as i32;
                    let (y, _, _) = crate::temporal::days_to_ymd(days);
                    Value::Int64(y as i64)
                }
                _ => Value::Null,
            }
        }
        "month" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Date(d) => {
                    let (_, m, _) = crate::temporal::days_to_ymd(*d);
                    Value::Int64(m as i64)
                }
                Value::DateTime(secs, _) => {
                    let day_secs = secs.rem_euclid(86400);
                    let days = ((secs - day_secs) / 86400) as i32;
                    let (_, m, _) = crate::temporal::days_to_ymd(days);
                    Value::Int64(m as i64)
                }
                _ => Value::Null,
            }
        }
        "day" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Date(d) => {
                    let (_, _, d) = crate::temporal::days_to_ymd(*d);
                    Value::Int64(d as i64)
                }
                Value::DateTime(secs, _) => {
                    let day_secs = secs.rem_euclid(86400);
                    let days = ((secs - day_secs) / 86400) as i32;
                    let (_, _, d) = crate::temporal::days_to_ymd(days);
                    Value::Int64(d as i64)
                }
                _ => Value::Null,
            }
        }
        "hour" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Time(t) => Value::Int64((*t / 3_600_000_000_000) as i64),
                Value::DateTime(secs, _) => {
                    let day_secs = secs.rem_euclid(86400) as u64;
                    Value::Int64((day_secs / 3600) as i64)
                }
                _ => Value::Null,
            }
        }
        "minute" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Time(t) => Value::Int64(((*t / 60_000_000_000) % 60) as i64),
                Value::DateTime(secs, _) => {
                    let day_secs = secs.rem_euclid(86400) as u64;
                    Value::Int64(((day_secs % 3600) / 60) as i64)
                }
                _ => Value::Null,
            }
        }
        "second" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::Time(t) => Value::Int64(((*t / 1_000_000_000) % 60) as i64),
                Value::DateTime(secs, _) => {
                    let day_secs = secs.rem_euclid(86400) as u64;
                    Value::Int64((day_secs % 60) as i64)
                }
                _ => Value::Null,
            }
        }
        // ── Temporal construction functions ──
        "make_date" => {
            if args.len() != 3 {
                return Value::Null;
            }
            match (args[0].as_i64(), args[1].as_i64(), args[2].as_i64()) {
                (Some(y), Some(m), Some(d)) => {
                    crate::temporal::ymd_to_days(y as i32, m as u32, d as u32)
                        .map(Value::Date)
                        .unwrap_or(Value::Null)
                }
                _ => Value::Null,
            }
        }
        "make_time" => {
            if args.len() != 3 {
                return Value::Null;
            }
            match (args[0].as_i64(), args[1].as_i64(), args[2].as_i64()) {
                (Some(h), Some(m), Some(s)) => {
                    if h < 0 || h >= 24 || m < 0 || m >= 60 || s < 0 || s >= 60 {
                        return Value::Null;
                    }
                    let nanos = (h as u64 * 3600 + m as u64 * 60 + s as u64) * 1_000_000_000;
                    Value::Time(nanos)
                }
                _ => Value::Null,
            }
        }
        // ── Current temporal functions ──
        "current_time" => {
            if !args.is_empty() {
                return Value::Null;
            }
            let now = temporal_now_nanos();
            if now == 0 {
                return Value::Null;
            }
            let day_nanos = now % 86_400_000_000_000;
            Value::Time(day_nanos as u64)
        }
        "current_datetime" => {
            if !args.is_empty() {
                return Value::Null;
            }
            let now = temporal_now_nanos();
            if now == 0 {
                return Value::Null;
            }
            let secs = now / 1_000_000_000;
            let sub = (now % 1_000_000_000) as u32;
            Value::DateTime(secs, sub)
        }
        // ── Epoch conversion ──
        "to_epoch_millis" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                Value::DateTime(secs, sub) => {
                    Value::Int64(*secs * 1000 + (*sub / 1_000_000) as i64)
                }
                Value::Date(d) => Value::Int64(*d as i64 * 86_400_000),
                Value::Timestamp(t) => Value::Int64((*t / 1_000_000) as i64),
                _ => Value::Null,
            }
        }
        "from_epoch_millis" => {
            if args.len() != 1 {
                return Value::Null;
            }
            match &args[0] {
                v if v.as_i64().is_some() => {
                    let ms = v.as_i64().unwrap();
                    let secs = ms / 1000;
                    let sub = ((ms % 1000) * 1_000_000) as u32;
                    Value::DateTime(secs, sub)
                }
                _ => Value::Null,
            }
        }
        _ => Value::Null,
    }
}

/// Returns current time as nanoseconds since Unix epoch.
///
/// When IC time has been injected via `set_current_time`, returns that value.
/// Otherwise: on native uses SystemTime; on wasm32 returns 0.
fn temporal_now_nanos() -> i64 {
    let injected = CURRENT_TIME_NANOS.with(|c| c.get());
    if injected != 0 {
        return injected as i64;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
    }
    #[cfg(target_arch = "wasm32")]
    {
        0
    }
}

fn unary_text<F>(args: &[Value], f: F) -> Value
where
    F: FnOnce(&str) -> Value,
{
    if args.len() != 1 {
        return Value::Null;
    }
    match &args[0] {
        Value::Text(s) => f(s),
        _ => Value::Null,
    }
}

fn binary_text_bool<F>(args: &[Value], f: F) -> Value
where
    F: FnOnce(&str, &str) -> bool,
{
    if args.len() != 2 {
        return Value::Null;
    }
    match (&args[0], &args[1]) {
        (Value::Text(a), Value::Text(b)) => Value::Bool(f(a, b)),
        _ => Value::Null,
    }
}

fn unary_numeric<FInt, FFloat>(args: &[Value], fi: FInt, ff: FFloat) -> Value
where
    FInt: FnOnce(i64) -> i64,
    FFloat: FnOnce(f64) -> f64,
{
    if args.len() != 1 {
        return Value::Null;
    }
    let v = &args[0];
    if v.is_signed_int() {
        if let Some(i) = v.as_i128() {
            let result = fi(i as i64);
            return gleaph_types::narrow_signed(result as i128, v.int_width().unwrap());
        }
    }
    if v.is_unsigned_int() {
        if let Some(u) = v.as_u128() {
            let result = fi(u as i64);
            if result >= 0 {
                return gleaph_types::narrow_unsigned(result as u128, v.int_width().unwrap());
            } else {
                return gleaph_types::narrow_signed(result as i128, v.int_width().unwrap());
            }
        }
    }
    match v {
        Value::Float32(f) => Value::Float32(ff(*f as f64) as f32),
        Value::Float64(f) => Value::Float64(ff(*f)),
        _ => Value::Null,
    }
}

fn unary_float<F>(args: &[Value], f: F) -> Value
where
    F: FnOnce(f64) -> f64,
{
    if args.len() != 1 {
        return Value::Null;
    }
    match numeric_as_f64(&args[0]) {
        Some(n) => Value::Float64(f(n)),
        None => Value::Null,
    }
}

fn vertex_property<M: Memory>(vertex_id: u32, property: &str, graph: &PmaGraph<M>) -> Value {
    let val = graph
        .get_single_vertex_property(vertex_id, property)
        .unwrap_or(Value::Null);
    apply_char_padding(val, property)
}

fn edge_property<M: Memory>(
    src: u32,
    dst: u32,
    label: Option<&str>,
    property: &str,
    _cached_weight: f32,
    _cached_timestamp: u64,
    graph: &PmaGraph<M>,
) -> Value {
    let Some(edge) = graph.edge_record(src, dst, label) else {
        return Value::Null;
    };
    edge.props
        .into_iter()
        .find_map(|(k, v)| if k == property { Some(v) } else { None })
        .unwrap_or(Value::Null)
}

/// Returns the display name for a RETURN item (alias if present, otherwise derived from expr).
pub fn column_name_for_return_item(item: &ReturnItem) -> String {
    column_name(item)
}

fn column_name(item: &ReturnItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    match &item.expr {
        Expr::Variable(v) => v.clone(),
        Expr::PropertyAccess { target, property } => {
            if let Expr::Variable(var) = target.as_ref() {
                format!("{var}.{property}")
            } else {
                property.clone()
            }
        }
        Expr::Aggregate(_) => "aggregate".into(),
        _ => "expr".into(),
    }
}

fn compare_rows_for_order<M: Memory>(
    order_by: &crate::ast::OrderBy,
    a: &Bindings,
    b: &Bindings,
    compiled_order_exprs: Option<&[CompiledValueExpr]>,
    graph: &PmaGraph<M>,
) -> Ordering {
    let a_keys = order_keys_for_row(order_by, a, compiled_order_exprs, graph);
    let b_keys = order_keys_for_row(order_by, b, compiled_order_exprs, graph);
    compare_order_keys(order_by, &a_keys, &b_keys)
}

fn order_keys_for_row<M: Memory>(
    order_by: &crate::ast::OrderBy,
    row: &Bindings,
    compiled_order_exprs: Option<&[CompiledValueExpr]>,
    graph: &PmaGraph<M>,
) -> Vec<Value> {
    if let Some(compiled_exprs) = compiled_order_exprs
        && compiled_exprs.len() == order_by.items.len()
    {
        return compiled_exprs
            .iter()
            .map(|expr| eval_compiled_value_expr(expr, row, graph))
            .collect();
    }
    order_by
        .items
        .iter()
        .map(|item| eval_expr(&item.expr, row, graph))
        .collect()
}

fn compare_order_keys(
    order_by: &crate::ast::OrderBy,
    lhs_keys: &[Value],
    rhs_keys: &[Value],
) -> Ordering {
    for (item, (lhs, rhs)) in order_by
        .items
        .iter()
        .zip(lhs_keys.iter().zip(rhs_keys.iter()))
    {
        // Determine whether NULL sorts before or after non-null values.
        // Default (None): NULLS LAST for ASC, NULLS FIRST for DESC (SQL semantics).
        let null_first = item.nulls_first.unwrap_or(item.descending);
        match (lhs, rhs) {
            (Value::Null, Value::Null) => continue,
            (Value::Null, _) => {
                return if null_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (_, Value::Null) => {
                return if null_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            _ => {
                let ord = compare_values(lhs, rhs).unwrap_or(Ordering::Equal);
                if ord != Ordering::Equal {
                    return if item.descending { ord.reverse() } else { ord };
                }
            }
        }
    }
    Ordering::Equal
}

fn sort_projected_aggregate_rows<M: Memory>(
    q: &QueryStmt,
    order_by: &crate::ast::OrderBy,
    rows: &mut Vec<Vec<Value>>,
    graph: &PmaGraph<M>,
) -> Result<(), GleaphError> {
    // Pre-compute column names from RETURN clause for fallback expression evaluation.
    let col_names: Vec<String> = q.return_clause.items.iter().map(column_name).collect();

    // For each row, evaluate ORDER BY sort keys upfront so the sort closure is pure.
    let sort_keys: Vec<Vec<Value>> = rows
        .iter()
        .map(|row| projected_aggregate_order_keys_for_row(q, order_by, row, &col_names, graph))
        .collect();

    // Stable-sort by pre-computed keys: pair each row with its keys, sort, then unzip.
    let mut indexed: Vec<(Vec<Value>, Vec<Value>)> =
        sort_keys.into_iter().zip(rows.drain(..)).collect();
    indexed.sort_by(|(ak, _), (bk, _)| compare_order_keys(order_by, ak, bk));
    *rows = indexed.into_iter().map(|(_, row)| row).collect();
    Ok(())
}

fn projected_aggregate_order_keys_for_row<M: Memory>(
    q: &QueryStmt,
    order_by: &crate::ast::OrderBy,
    row: &[Value],
    col_names: &[String],
    graph: &PmaGraph<M>,
) -> Vec<Value> {
    order_by
        .items
        .iter()
        .map(|item| {
            // Fast path: exact expression match or variable-alias match → column index.
            let col_idx = q
                .return_clause
                .items
                .iter()
                .enumerate()
                .find_map(|(idx, ret)| {
                    if ret.expr == item.expr {
                        return Some(idx);
                    }
                    match (&item.expr, &ret.alias) {
                        (Expr::Variable(v), Some(alias)) if v.eq_ignore_ascii_case(alias) => {
                            Some(idx)
                        }
                        _ => None,
                    }
                });
            if let Some(idx) = col_idx {
                return row[idx].clone();
            }
            // Fallback: build mini-Bindings from projected columns and eval expression.
            // Handles e.g. `ORDER BY cnt + 1` when `RETURN count(n) AS cnt`.
            let mut bindings = Bindings::new();
            for (name, val) in col_names.iter().zip(row.iter()) {
                bindings.insert(name.clone(), Binding::Value(val.clone()));
            }
            eval_expr(&item.expr, &bindings, graph)
        })
        .collect()
}

fn top_k_projected_aggregate_rows<M: Memory>(
    q: &QueryStmt,
    order_by: &crate::ast::OrderBy,
    rows: Vec<Vec<Value>>,
    k: usize,
    graph: &PmaGraph<M>,
) -> Result<Vec<Vec<Value>>, GleaphError> {
    if k == 0 {
        return Ok(Vec::new());
    }

    let col_names: Vec<String> = q.return_clause.items.iter().map(column_name).collect();
    let mut best: Vec<(usize, Vec<Value>, Vec<Value>)> = Vec::new(); // sorted in final ORDER BY order
    for (idx, row) in rows.into_iter().enumerate() {
        let row_keys = projected_aggregate_order_keys_for_row(q, order_by, &row, &col_names, graph);
        let cmp_existing_with_candidate = |existing: &(usize, Vec<Value>, Vec<Value>)| {
            compare_order_keys(order_by, &existing.1, &row_keys).then_with(|| existing.0.cmp(&idx))
        };
        let candidate_vs_existing = |existing: &(usize, Vec<Value>, Vec<Value>)| {
            compare_order_keys(order_by, &row_keys, &existing.1).then_with(|| idx.cmp(&existing.0))
        };
        let insert_pos = || {
            best.iter()
                .position(|probe| cmp_existing_with_candidate(probe) != Ordering::Less)
                .unwrap_or(best.len())
        };

        if best.len() < k {
            let pos = insert_pos();
            best.insert(pos, (idx, row_keys, row));
            continue;
        }

        // `best` is sorted best->worst, so skip rows that are not better than the current worst.
        if candidate_vs_existing(best.last().expect("non-empty")) != Ordering::Less {
            continue;
        }

        let pos = insert_pos();
        best.insert(pos, (idx, row_keys, row));
        best.pop();
    }

    Ok(best.into_iter().map(|(_, _, row)| row).collect())
}

fn top_k_rows<M: Memory>(
    rows: Vec<Bindings>,
    order_by: &crate::ast::OrderBy,
    compiled_order_exprs: Option<&[CompiledValueExpr]>,
    k: usize,
    graph: &PmaGraph<M>,
) -> Vec<Bindings> {
    if k == 0 {
        return Vec::new();
    }

    let mut best: Vec<(usize, Vec<Value>, Bindings)> = Vec::new(); // sorted in final ORDER BY order
    for (idx, row) in rows.into_iter().enumerate() {
        let row_keys = order_keys_for_row(order_by, &row, compiled_order_exprs, graph);
        let cmp_existing_with_candidate = |existing: &(usize, Vec<Value>, Bindings)| {
            compare_order_keys(order_by, &existing.1, &row_keys).then_with(|| existing.0.cmp(&idx))
        };
        let candidate_vs_existing = |existing: &(usize, Vec<Value>, Bindings)| {
            compare_order_keys(order_by, &row_keys, &existing.1).then_with(|| idx.cmp(&existing.0))
        };
        let insert_pos = || {
            best.iter()
                .position(|probe| cmp_existing_with_candidate(probe) != Ordering::Less)
                .unwrap_or(best.len())
        };

        if best.len() < k {
            let pos = insert_pos();
            best.insert(pos, (idx, row_keys, row));
            continue;
        }

        // `best` is sorted best->worst, so skip rows that are not better than the current worst.
        if candidate_vs_existing(best.last().expect("non-empty")) != Ordering::Less {
            continue;
        }

        let pos = insert_pos();
        best.insert(pos, (idx, row_keys, row));
        best.pop();
    }

    best.into_iter().map(|(_, _, row)| row).collect()
}

fn execute_create_single<M: Memory>(
    stmt: &CreateStmt,
    graph: &mut PmaGraph<M>,
    edge_timestamp: u64,
) -> Result<MutationOutcome, GleaphError> {
    match stmt {
        CreateStmt::Node(n) => {
            let props = literal_props_to_map(&n.node.props_hint)?;
            let id = graph.create_vertex(n.node.labels.clone(), props)?;
            Ok(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 1,
                    affected_edges: 0,
                    warnings: Vec::new(),
                },
                affected_vertex_ids: vec![id],
            })
        }
        CreateStmt::Edge(e) => {
            let left = graph.create_vertex(
                e.left.labels.clone(),
                literal_props_to_map(&e.left.props_hint)?,
            )?;
            let right = graph.create_vertex(
                e.right.labels.clone(),
                literal_props_to_map(&e.right.props_hint)?,
            )?;
            let (src, dst) = match e.edge.direction {
                Direction::Outgoing | Direction::Either => (left, right),
                Direction::Incoming => (right, left),
            };
            graph.create_edge(
                src,
                dst,
                e.edge.label.clone(),
                literal_props_to_map(&e.edge.properties)?,
                1.0,
                edge_timestamp,
            )?;
            Ok(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 2,
                    affected_edges: 1,
                    warnings: Vec::new(),
                },
                affected_vertex_ids: vec![left, right],
            })
        }
    }
}

fn execute_create_multi<M: Memory>(
    stmts: &[CreateStmt],
    graph: &mut PmaGraph<M>,
    edge_timestamp: u64,
) -> Result<MutationOutcome, GleaphError> {
    let mut total = MutationOutcome {
        result: MutationResult {
            affected_vertices: 0,
            affected_edges: 0,
            warnings: Vec::new(),
        },
        affected_vertex_ids: Vec::new(),
    };
    for stmt in stmts {
        let outcome = execute_create_single(stmt, graph, edge_timestamp)?;
        total.result.affected_vertices += outcome.result.affected_vertices;
        total.result.affected_edges += outcome.result.affected_edges;
        total
            .affected_vertex_ids
            .extend(outcome.affected_vertex_ids);
    }
    Ok(total)
}

fn execute_merge<M: Memory>(
    stmt: &MergeStmt,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
    edge_timestamp: u64,
) -> Result<MutationOutcome, GleaphError> {
    // Build a MatchClause from the merge pattern to check for existing matches.
    let match_clause = match &stmt.create {
        CreateStmt::Node(n) => MatchClause {
            start: n.node.clone(),
            elements: Vec::new(),
        },
        CreateStmt::Edge(e) => MatchClause {
            start: e.left.clone(),
            elements: vec![PatternElement::Hop(MatchChain {
                edge: e.edge.clone(),
                node: e.right.clone(),
            })],
        },
    };

    let mut stats = QueryStats::default();
    let rows = execute_match_clause(&match_clause, graph, &mut stats, None, None, limits)?;

    if rows.is_empty() {
        // Pattern not found — create it.
        let created = execute_create_single(&stmt.create, graph, edge_timestamp)?;
        // Apply ON CREATE SET if present.
        if !stmt.on_create_set.is_empty() {
            let mut bindings = Bindings::new();
            match &stmt.create {
                CreateStmt::Node(n) => {
                    if let (Some(var), Some(&id)) =
                        (&n.node.var, created.affected_vertex_ids.first())
                    {
                        bindings.insert(var.clone(), Binding::Vertex(id));
                    }
                }
                CreateStmt::Edge(e) => {
                    if let (Some(var), Some(&id)) =
                        (&e.left.var, created.affected_vertex_ids.first())
                    {
                        bindings.insert(var.clone(), Binding::Vertex(id));
                    }
                    if let (Some(var), Some(&id)) =
                        (&e.right.var, created.affected_vertex_ids.get(1))
                    {
                        bindings.insert(var.clone(), Binding::Vertex(id));
                    }
                }
            }
            apply_set_items(&stmt.on_create_set, &bindings, graph)?;
        }
        Ok(created)
    } else {
        // Pattern matched — apply ON MATCH SET to each row.
        if !stmt.on_match_set.is_empty() {
            for bindings in &rows {
                apply_set_items(&stmt.on_match_set, bindings, graph)?;
            }
        }
        let affected_ids: Vec<u32> = rows
            .iter()
            .filter_map(|b| {
                let var = match &stmt.create {
                    CreateStmt::Node(n) => n.node.var.as_deref(),
                    CreateStmt::Edge(e) => e.left.var.as_deref(),
                };
                var.and_then(|v| {
                    if let Some(Binding::Vertex(id)) = b.get(v) {
                        Some(*id)
                    } else {
                        None
                    }
                })
            })
            .collect();
        Ok(MutationOutcome {
            result: MutationResult {
                affected_vertices: affected_ids.len() as u64,
                affected_edges: 0,
                warnings: Vec::new(),
            },
            affected_vertex_ids: affected_ids,
        })
    }
}

/// Applies a slice of `SetItem`s to a single binding row against `graph`.
fn apply_set_items<M: Memory>(
    items: &[SetItem],
    bindings: &Bindings,
    graph: &mut PmaGraph<M>,
) -> Result<(), GleaphError> {
    for item in items {
        match item {
            SetItem::Property {
                var,
                property,
                value,
            } => {
                let evaled = eval_expr(value, bindings, graph);
                match bindings.get(var) {
                    Some(Binding::Vertex(id)) => {
                        graph.set_vertex_prop(*id, property.clone(), evaled)?;
                    }
                    Some(Binding::Edge {
                        src, dst, label, ..
                    }) => {
                        graph.set_edge_prop(
                            *src,
                            *dst,
                            label.as_deref(),
                            property.clone(),
                            evaled,
                        )?;
                    }
                    _ => {}
                }
            }
            SetItem::AllProperties { var, properties } => match bindings.get(var) {
                Some(Binding::Vertex(id)) => {
                    let new_props: Vec<(String, Value)> = properties
                        .iter()
                        .map(|(k, v)| (k.clone(), eval_expr(v, bindings, graph)))
                        .collect();
                    graph.set_vertex_props(*id, new_props)?;
                }
                Some(Binding::Edge {
                    src, dst, label, ..
                }) => {
                    let old = graph
                        .edge_record(*src, *dst, label.as_deref())
                        .map(|r| r.props)
                        .unwrap_or_default();
                    let new_keys: std::collections::BTreeSet<&str> =
                        properties.iter().map(|(k, _)| k.as_str()).collect();
                    for (k, _) in &old {
                        if !new_keys.contains(k.as_str()) {
                            graph.delete_edge_prop(*src, *dst, label.as_deref(), k)?;
                        }
                    }
                    for (k, v) in properties {
                        let evaled = eval_expr(v, bindings, graph);
                        graph.set_edge_prop(*src, *dst, label.as_deref(), k.clone(), evaled)?;
                    }
                }
                _ => {}
            },
            SetItem::Label { var, label } => {
                if let Some(Binding::Vertex(id)) = bindings.get(var) {
                    graph.add_vertex_label(*id, label.clone())?;
                }
            }
        }
    }
    Ok(())
}

fn execute_delete<M: Memory>(
    stmt: &DeleteStmt,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationOutcome, GleaphError> {
    let mut stats = QueryStats::default();
    let rows = execute_match_clause(
        &stmt.match_clause,
        graph,
        &mut stats,
        stmt.where_clause.as_ref(),
        None,
        limits,
    )?;

    let mut vertices_to_delete = BTreeSet::new();
    let mut edges_to_delete = BTreeSet::new();
    for bindings in rows {
        for target_var in &stmt.target_vars {
            match bindings.get(target_var) {
                Some(Binding::Vertex(v)) => {
                    vertices_to_delete.insert(*v);
                }
                Some(Binding::Edge {
                    src, dst, label, ..
                }) => {
                    edges_to_delete.insert((*src, *dst, label.as_deref().map(str::to_string)));
                }
                Some(Binding::Value(_)) => {}
                None => {}
            }
        }
    }
    if stmt.detach {
        let mut extra_edges = BTreeSet::new();
        for v in &vertices_to_delete {
            for e in graph.collect_neighbors_filtered(*v)? {
                let lbl = graph.label_name_by_id(e.label_id()).map(str::to_string);
                extra_edges.insert((*v, e.target, lbl));
            }
            for rev in graph.reverse_neighbors_rich(*v) {
                let lbl = graph.label_name_by_id(rev.label_id()).map(str::to_string);
                extra_edges.insert((rev.src, *v, lbl));
            }
        }
        edges_to_delete.extend(extra_edges);
    } else {
        for v in &vertices_to_delete {
            let has_out = !graph.collect_neighbors_filtered(*v)?.is_empty();
            let has_in = !graph.reverse_neighbors_rich(*v).is_empty();
            if has_out || has_in {
                let msg = if stmt.nodetach {
                    "NODETACH DELETE failed: vertex has incident edges"
                } else {
                    "DELETE on vertex with incident edges requires DETACH"
                };
                return Err(GleaphError::ValidationError(msg.into()));
            }
        }
    }
    let affected_ids: Vec<u32> = vertices_to_delete.iter().copied().collect();
    for v in &vertices_to_delete {
        graph.delete_vertex(*v)?;
    }
    for (src, dst, label) in &edges_to_delete {
        graph.delete_edge(*src, *dst, label.as_deref())?;
    }
    Ok(MutationOutcome {
        result: MutationResult {
            affected_vertices: vertices_to_delete.len() as u64,
            affected_edges: edges_to_delete.len() as u64,
            warnings: Vec::new(),
        },
        affected_vertex_ids: affected_ids,
    })
}

// ── Resumable DELETE ─────────────────────────────────────────────────────

fn execute_delete_resumable<M: Memory>(
    stmt: &DeleteStmt,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationProgress, GleaphError> {
    // Phase 1: collect targets — uses a large budget for the MATCH scan (same as non-resumable).
    // The step budget in `limits` only governs phase 2 (the apply loop).
    let match_limits = ExecutionLimits {
        max_rows: limits.max_rows,
        max_execution_steps: Some(1_000_000),
    };
    let mut stats = QueryStats::default();
    let rows = execute_match_clause(
        &stmt.match_clause,
        graph,
        &mut stats,
        stmt.where_clause.as_ref(),
        None,
        match_limits,
    )?;

    let mut vertices_to_delete = BTreeSet::new();
    let mut edges_to_delete = BTreeSet::new();
    for bindings in rows {
        for target_var in &stmt.target_vars {
            match bindings.get(target_var) {
                Some(Binding::Vertex(v)) => {
                    vertices_to_delete.insert(*v);
                }
                Some(Binding::Edge {
                    src, dst, label, ..
                }) => {
                    edges_to_delete.insert((*src, *dst, label.as_deref().map(str::to_string)));
                }
                Some(Binding::Value(_)) => {}
                None => {}
            }
        }
    }
    if stmt.detach {
        let mut extra_edges = BTreeSet::new();
        for v in &vertices_to_delete {
            for e in graph.collect_neighbors_filtered(*v)? {
                let lbl = graph.label_name_by_id(e.label_id()).map(str::to_string);
                extra_edges.insert((*v, e.target, lbl));
            }
            for rev in graph.reverse_neighbors_rich(*v) {
                let lbl = graph.label_name_by_id(rev.label_id()).map(str::to_string);
                extra_edges.insert((rev.src, *v, lbl));
            }
        }
        edges_to_delete.extend(extra_edges);
    } else {
        for v in &vertices_to_delete {
            let has_out = !graph.collect_neighbors_filtered(*v)?.is_empty();
            let has_in = !graph.reverse_neighbors_rich(*v).is_empty();
            if has_out || has_in {
                let msg = if stmt.nodetach {
                    "NODETACH DELETE failed: vertex has incident edges"
                } else {
                    "DELETE on vertex with incident edges requires DETACH"
                };
                return Err(GleaphError::ValidationError(msg.into()));
            }
        }
    }

    // Phase 2: apply with budget
    let vertices: Vec<u32> = vertices_to_delete.into_iter().collect();
    let edges: Vec<(u32, u32, Option<String>)> = edges_to_delete.into_iter().collect();
    apply_deletes_budgeted(graph, vertices, edges, limits, 0, 0, Vec::new())
}

fn apply_deletes_budgeted<M: Memory>(
    graph: &mut PmaGraph<M>,
    vertices: Vec<u32>,
    edges: Vec<(u32, u32, Option<String>)>,
    limits: ExecutionLimits,
    mut affected_v: u64,
    mut affected_e: u64,
    mut affected_ids: Vec<u32>,
) -> Result<MutationProgress, GleaphError> {
    let budget = limits.max_execution_steps.unwrap_or(u64::MAX);
    let mut steps = 0u64;
    let mut v_idx = 0;
    let mut e_idx = 0;

    while v_idx < vertices.len() {
        if steps >= budget {
            return Ok(MutationProgress::Suspended {
                partial: MutationOutcome {
                    result: MutationResult {
                        affected_vertices: affected_v,
                        affected_edges: affected_e,
                        warnings: Vec::new(),
                    },
                    affected_vertex_ids: affected_ids.clone(),
                },
                checkpoint: gleaph_types::MutationCheckpoint::Delete(
                    gleaph_types::DeleteCheckpoint {
                        remaining_vertices: vertices[v_idx..].to_vec(),
                        remaining_edges: edges[e_idx..].to_vec(),
                        affected_vertices: affected_v,
                        affected_edges: affected_e,
                        affected_vertex_ids: affected_ids,
                    },
                ),
            });
        }
        graph.delete_vertex(vertices[v_idx])?;
        affected_v += 1;
        affected_ids.push(vertices[v_idx]);
        v_idx += 1;
        steps += 1;
    }

    while e_idx < edges.len() {
        if steps >= budget {
            return Ok(MutationProgress::Suspended {
                partial: MutationOutcome {
                    result: MutationResult {
                        affected_vertices: affected_v,
                        affected_edges: affected_e,
                        warnings: Vec::new(),
                    },
                    affected_vertex_ids: affected_ids.clone(),
                },
                checkpoint: gleaph_types::MutationCheckpoint::Delete(
                    gleaph_types::DeleteCheckpoint {
                        remaining_vertices: Vec::new(),
                        remaining_edges: edges[e_idx..].to_vec(),
                        affected_vertices: affected_v,
                        affected_edges: affected_e,
                        affected_vertex_ids: affected_ids,
                    },
                ),
            });
        }
        let (src, dst, ref label) = edges[e_idx];
        graph.delete_edge(src, dst, label.as_deref())?;
        affected_e += 1;
        e_idx += 1;
        steps += 1;
    }

    Ok(MutationProgress::Done(MutationOutcome {
        result: MutationResult {
            affected_vertices: affected_v,
            affected_edges: affected_e,
            warnings: Vec::new(),
        },
        affected_vertex_ids: affected_ids,
    }))
}

/// Resumes a suspended DELETE mutation from a checkpoint.
pub fn resume_delete<M: Memory>(
    cp: gleaph_types::DeleteCheckpoint,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationProgress, GleaphError> {
    apply_deletes_budgeted(
        graph,
        cp.remaining_vertices,
        cp.remaining_edges,
        limits,
        cp.affected_vertices,
        cp.affected_edges,
        cp.affected_vertex_ids,
    )
}

/// Resumes any suspended mutation from a `MutationCheckpoint`.
pub fn resume_mutation<M: Memory>(
    cp: gleaph_types::MutationCheckpoint,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationProgress, GleaphError> {
    match cp {
        gleaph_types::MutationCheckpoint::Delete(dc) => resume_delete(dc, graph, limits),
        gleaph_types::MutationCheckpoint::Set(sc) => resume_set_remove(sc, true, graph, limits),
        gleaph_types::MutationCheckpoint::Remove(rc) => resume_set_remove(rc, false, graph, limits),
    }
}

/// Phase 1 + Phase 2 for SET with budget support.
fn execute_set_resumable<M: Memory>(
    stmt: &SetStmt,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationProgress, GleaphError> {
    // Phase 1: collect all SET operations into flat MutationOp list
    let match_limits = ExecutionLimits {
        max_rows: limits.max_rows,
        max_execution_steps: Some(1_000_000),
    };
    let mut stats = QueryStats::default();
    let rows = execute_match_clause(
        &stmt.match_clause,
        graph,
        &mut stats,
        stmt.where_clause.as_ref(),
        None,
        match_limits,
    )?;

    let mut ops = Vec::new();
    for bindings in &rows {
        for item in &stmt.set_clause.items {
            match item {
                SetItem::Property {
                    var,
                    property,
                    value,
                } => {
                    let evaled = eval_expr(value, bindings, graph);
                    match bindings.get(var) {
                        Some(Binding::Vertex(id)) => {
                            ops.push(gleaph_types::MutationOp::SetVertexProp {
                                id: *id,
                                property: property.clone(),
                                value: evaled,
                            });
                        }
                        Some(Binding::Edge {
                            src, dst, label, ..
                        }) => {
                            ops.push(gleaph_types::MutationOp::SetEdgeProp {
                                src: *src,
                                dst: *dst,
                                label: label.as_deref().map(str::to_string),
                                property: property.clone(),
                                value: evaled,
                            });
                        }
                        _ => {}
                    }
                }
                SetItem::AllProperties { var, properties } => match bindings.get(var) {
                    Some(Binding::Vertex(id)) => {
                        let old = graph.get_vertex_props(*id).unwrap_or_default();
                        let new_keys: std::collections::BTreeSet<&str> =
                            properties.iter().map(|(k, _)| k.as_str()).collect();
                        for (k, _) in &old {
                            if !new_keys.contains(k.as_str()) {
                                ops.push(gleaph_types::MutationOp::RemoveVertexProp {
                                    id: *id,
                                    property: k.clone(),
                                });
                            }
                        }
                        for (k, v) in properties {
                            let evaled = eval_expr(v, bindings, graph);
                            ops.push(gleaph_types::MutationOp::SetVertexProp {
                                id: *id,
                                property: k.clone(),
                                value: evaled,
                            });
                        }
                    }
                    Some(Binding::Edge {
                        src, dst, label, ..
                    }) => {
                        let old = graph
                            .edge_record(*src, *dst, label.as_deref())
                            .map(|r| r.props)
                            .unwrap_or_default();
                        let lbl = label.as_deref().map(str::to_string);
                        let new_keys: std::collections::BTreeSet<&str> =
                            properties.iter().map(|(k, _)| k.as_str()).collect();
                        for (k, _) in &old {
                            if !new_keys.contains(k.as_str()) {
                                ops.push(gleaph_types::MutationOp::RemoveEdgeProp {
                                    src: *src,
                                    dst: *dst,
                                    label: lbl.clone(),
                                    property: k.clone(),
                                });
                            }
                        }
                        for (k, v) in properties {
                            let evaled = eval_expr(v, bindings, graph);
                            ops.push(gleaph_types::MutationOp::SetEdgeProp {
                                src: *src,
                                dst: *dst,
                                label: lbl.clone(),
                                property: k.clone(),
                                value: evaled,
                            });
                        }
                    }
                    _ => {}
                },
                SetItem::Label { var, label } => {
                    if let Some(Binding::Vertex(id)) = bindings.get(var) {
                        ops.push(gleaph_types::MutationOp::AddVertexLabel {
                            id: *id,
                            label: label.clone(),
                        });
                    }
                }
            }
        }
    }

    // Phase 2: apply with budget
    apply_mutation_ops_budgeted(graph, ops, true, limits, 0, 0, Vec::new())
}

/// Phase 1 + Phase 2 for REMOVE with budget support.
fn execute_remove_resumable<M: Memory>(
    stmt: &RemoveStmt,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationProgress, GleaphError> {
    // Phase 1: collect all REMOVE operations into flat MutationOp list
    let match_limits = ExecutionLimits {
        max_rows: limits.max_rows,
        max_execution_steps: Some(1_000_000),
    };
    let mut stats = QueryStats::default();
    let rows = execute_match_clause(
        &stmt.match_clause,
        graph,
        &mut stats,
        stmt.where_clause.as_ref(),
        None,
        match_limits,
    )?;

    let mut ops = Vec::new();
    for bindings in &rows {
        for item in &stmt.remove_clause.items {
            match item {
                RemoveItem::Property { var, property } => match bindings.get(var) {
                    Some(Binding::Vertex(id)) => {
                        ops.push(gleaph_types::MutationOp::RemoveVertexProp {
                            id: *id,
                            property: property.clone(),
                        });
                    }
                    Some(Binding::Edge {
                        src, dst, label, ..
                    }) => {
                        ops.push(gleaph_types::MutationOp::RemoveEdgeProp {
                            src: *src,
                            dst: *dst,
                            label: label.as_deref().map(str::to_string),
                            property: property.clone(),
                        });
                    }
                    _ => {}
                },
                RemoveItem::Label { var, label } => {
                    if let Some(Binding::Vertex(id)) = bindings.get(var) {
                        ops.push(gleaph_types::MutationOp::RemoveVertexLabel {
                            id: *id,
                            label: label.clone(),
                        });
                    }
                }
            }
        }
    }

    // Phase 2: apply with budget
    apply_mutation_ops_budgeted(graph, ops, false, limits, 0, 0, Vec::new())
}

/// Shared Phase 2: applies `MutationOp`s with step budget, suspending if exceeded.
/// `is_set` controls whether the checkpoint is `MutationCheckpoint::Set` or `::Remove`.
fn apply_mutation_ops_budgeted<M: Memory>(
    graph: &mut PmaGraph<M>,
    ops: Vec<gleaph_types::MutationOp>,
    is_set: bool,
    limits: ExecutionLimits,
    mut affected_v: u64,
    mut affected_e: u64,
    mut affected_ids: Vec<u32>,
) -> Result<MutationProgress, GleaphError> {
    let budget = limits.max_execution_steps.unwrap_or(u64::MAX);
    let mut steps = 0u64;
    let mut idx = 0;

    let mut seen_vertices = BTreeSet::new();
    let mut seen_edges = BTreeSet::new();
    // Rebuild sets from previously affected state
    for &vid in &affected_ids {
        seen_vertices.insert(vid);
    }

    while idx < ops.len() {
        if steps >= budget {
            let wrap = if is_set {
                gleaph_types::MutationCheckpoint::Set
            } else {
                gleaph_types::MutationCheckpoint::Remove
            };
            return Ok(MutationProgress::Suspended {
                partial: MutationOutcome {
                    result: MutationResult {
                        affected_vertices: affected_v,
                        affected_edges: affected_e,
                        warnings: Vec::new(),
                    },
                    affected_vertex_ids: affected_ids.clone(),
                },
                checkpoint: wrap(gleaph_types::SetRemoveCheckpoint {
                    remaining_ops: ops[idx..].to_vec(),
                    affected_vertices: affected_v,
                    affected_edges: affected_e,
                    affected_vertex_ids: affected_ids,
                }),
            });
        }

        match &ops[idx] {
            gleaph_types::MutationOp::SetVertexProp {
                id,
                property,
                value,
            } => {
                graph.set_vertex_prop(*id, property.clone(), value.clone())?;
                if seen_vertices.insert(*id) {
                    affected_v += 1;
                    affected_ids.push(*id);
                }
            }
            gleaph_types::MutationOp::SetEdgeProp {
                src,
                dst,
                label,
                property,
                value,
            } => {
                graph.set_edge_prop(
                    *src,
                    *dst,
                    label.as_deref(),
                    property.clone(),
                    value.clone(),
                )?;
                if seen_edges.insert((*src, *dst, label.clone())) {
                    affected_e += 1;
                }
            }
            gleaph_types::MutationOp::AddVertexLabel { id, label } => {
                graph.add_vertex_label(*id, label.clone())?;
                if seen_vertices.insert(*id) {
                    affected_v += 1;
                    affected_ids.push(*id);
                }
            }
            gleaph_types::MutationOp::RemoveVertexProp { id, property } => {
                graph.delete_vertex_prop(*id, property)?;
                if seen_vertices.insert(*id) {
                    affected_v += 1;
                    affected_ids.push(*id);
                }
            }
            gleaph_types::MutationOp::RemoveEdgeProp {
                src,
                dst,
                label,
                property,
            } => {
                graph.delete_edge_prop(*src, *dst, label.as_deref(), property)?;
                if seen_edges.insert((*src, *dst, label.clone())) {
                    affected_e += 1;
                }
            }
            gleaph_types::MutationOp::RemoveVertexLabel { id, label } => {
                graph.remove_vertex_label(*id, label)?;
                if seen_vertices.insert(*id) {
                    affected_v += 1;
                    affected_ids.push(*id);
                }
            }
        }
        idx += 1;
        steps += 1;
    }

    Ok(MutationProgress::Done(MutationOutcome {
        result: MutationResult {
            affected_vertices: affected_v,
            affected_edges: affected_e,
            warnings: Vec::new(),
        },
        affected_vertex_ids: affected_ids,
    }))
}

/// Resumes a suspended SET or REMOVE mutation from a checkpoint.
pub fn resume_set_remove<M: Memory>(
    cp: gleaph_types::SetRemoveCheckpoint,
    is_set: bool,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationProgress, GleaphError> {
    apply_mutation_ops_budgeted(
        graph,
        cp.remaining_ops,
        is_set,
        limits,
        cp.affected_vertices,
        cp.affected_edges,
        cp.affected_vertex_ids,
    )
}

/// Executes a mutation with resumable support for DELETE, SET, and REMOVE.
/// Other mutations run to completion (wrapped in `MutationProgress::Done`).
pub fn execute_mutation_resumable<M: Memory>(
    stmt: &Statement,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
    edge_timestamp: u64,
) -> Result<MutationProgress, GleaphError> {
    let _reg_guard = ensure_registry(stmt);
    match stmt {
        Statement::Delete(d) => execute_delete_resumable(d, graph, limits),
        Statement::Set(s) => execute_set_resumable(s, graph, limits),
        Statement::Remove(r) => execute_remove_resumable(r, graph, limits),
        other => execute_mutation_tracked(other, graph, limits, edge_timestamp)
            .map(MutationProgress::Done),
    }
}

fn execute_set<M: Memory>(
    stmt: &SetStmt,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationOutcome, GleaphError> {
    let mut stats = QueryStats::default();
    let rows = execute_match_clause(
        &stmt.match_clause,
        graph,
        &mut stats,
        stmt.where_clause.as_ref(),
        None,
        limits,
    )?;

    let mut affected_vertices = BTreeSet::new();
    let mut affected_edges = BTreeSet::new();
    for bindings in &rows {
        for item in &stmt.set_clause.items {
            match item {
                SetItem::Property {
                    var,
                    property,
                    value,
                } => {
                    let evaled = eval_expr(value, bindings, graph);
                    match bindings.get(var) {
                        Some(Binding::Vertex(id)) => {
                            graph.set_vertex_prop(*id, property.clone(), evaled)?;
                            affected_vertices.insert(*id);
                        }
                        Some(Binding::Edge {
                            src, dst, label, ..
                        }) => {
                            graph.set_edge_prop(
                                *src,
                                *dst,
                                label.as_deref(),
                                property.clone(),
                                evaled,
                            )?;
                            affected_edges.insert((
                                *src,
                                *dst,
                                label.as_deref().map(str::to_string),
                            ));
                        }
                        Some(Binding::Value(_)) => {}
                        None => {}
                    }
                }
                SetItem::AllProperties { var, properties } => match bindings.get(var) {
                    Some(Binding::Vertex(id)) => {
                        let new_props: Vec<(String, Value)> = properties
                            .iter()
                            .map(|(k, v)| (k.clone(), eval_expr(v, bindings, graph)))
                            .collect();
                        graph.set_vertex_props(*id, new_props)?;
                        affected_vertices.insert(*id);
                    }
                    Some(Binding::Edge {
                        src, dst, label, ..
                    }) => {
                        let old = graph
                            .edge_record(*src, *dst, label.as_deref())
                            .map(|r| r.props)
                            .unwrap_or_default();
                        let new_keys: std::collections::BTreeSet<&str> =
                            properties.iter().map(|(k, _)| k.as_str()).collect();
                        for (k, _) in &old {
                            if !new_keys.contains(k.as_str()) {
                                graph.delete_edge_prop(*src, *dst, label.as_deref(), k)?;
                            }
                        }
                        for (k, v) in properties {
                            let evaled = eval_expr(v, bindings, graph);
                            graph.set_edge_prop(*src, *dst, label.as_deref(), k.clone(), evaled)?;
                        }
                        affected_edges.insert((*src, *dst, label.as_deref().map(str::to_string)));
                    }
                    _ => {}
                },
                SetItem::Label { var, label } => {
                    if let Some(Binding::Vertex(id)) = bindings.get(var) {
                        graph.add_vertex_label(*id, label.clone())?;
                        affected_vertices.insert(*id);
                    }
                }
            }
        }
    }

    let affected_ids: Vec<u32> = affected_vertices.iter().copied().collect();
    Ok(MutationOutcome {
        result: MutationResult {
            affected_vertices: affected_vertices.len() as u64,
            affected_edges: affected_edges.len() as u64,
            warnings: Vec::new(),
        },
        affected_vertex_ids: affected_ids,
    })
}

fn execute_remove<M: Memory>(
    stmt: &RemoveStmt,
    graph: &mut PmaGraph<M>,
    limits: ExecutionLimits,
) -> Result<MutationOutcome, GleaphError> {
    let mut stats = QueryStats::default();
    let rows = execute_match_clause(
        &stmt.match_clause,
        graph,
        &mut stats,
        stmt.where_clause.as_ref(),
        None,
        limits,
    )?;

    let mut affected_vertices = BTreeSet::new();
    let mut affected_edges = BTreeSet::new();
    for bindings in &rows {
        for item in &stmt.remove_clause.items {
            match item {
                RemoveItem::Property { var, property } => match bindings.get(var) {
                    Some(Binding::Vertex(id)) => {
                        graph.delete_vertex_prop(*id, property)?;
                        affected_vertices.insert(*id);
                    }
                    Some(Binding::Edge {
                        src, dst, label, ..
                    }) => {
                        graph.delete_edge_prop(*src, *dst, label.as_deref(), property)?;
                        affected_edges.insert((*src, *dst, label.as_deref().map(str::to_string)));
                    }
                    Some(Binding::Value(_)) => {}
                    None => {}
                },
                RemoveItem::Label { var, label } => {
                    if let Some(Binding::Vertex(id)) = bindings.get(var) {
                        graph.remove_vertex_label(*id, label)?;
                        affected_vertices.insert(*id);
                    }
                }
            }
        }
    }

    let affected_ids: Vec<u32> = affected_vertices.iter().copied().collect();
    Ok(MutationOutcome {
        result: MutationResult {
            affected_vertices: affected_vertices.len() as u64,
            affected_edges: affected_edges.len() as u64,
            warnings: Vec::new(),
        },
        affected_vertex_ids: affected_ids,
    })
}

fn literal_props_to_map(props: &[(String, Expr)]) -> Result<Vec<(String, Value)>, GleaphError> {
    let mut out = Vec::with_capacity(props.len());
    for (k, expr) in props {
        let Expr::Literal(v) = expr else {
            return Err(GleaphError::ExecutionError(
                "CREATE props currently require literal values".into(),
            ));
        };
        out.push((k.clone(), v.clone()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_statement, planner::build_plan, validate_statement};
    use gleaph_pma::VecMemory;

    #[test]
    fn node_scan_full_and_label_scan() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_vertex(vec!["Company".into()], vec![]).unwrap();
        g.create_vertex(vec!["User".into()], vec![]).unwrap();

        let q = parse_query("MATCH (a)-[:X]->(b) RETURN a LIMIT 1");
        let mut s = QueryStats::default();
        let all = initial_candidates(
            &q.match_clauses[0].pattern.start,
            &g,
            &mut s,
            ExecutionLimits::default(),
        )
        .unwrap();
        assert_eq!(all.len(), 3);

        let q = parse_query("MATCH (a:User)-[:X]->(b) RETURN a LIMIT 1");
        let mut s = QueryStats::default();
        let users = initial_candidates(
            &q.match_clauses[0].pattern.start,
            &g,
            &mut s,
            ExecutionLimits::default(),
        )
        .unwrap();
        assert_eq!(users.len(), 2);
    }

    #[test]
    fn limit_pushdown_stops_match_expansion_when_safe() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        for i in 0..20 {
            let b = g
                .create_vertex(vec!["User".into()], vec![("n".into(), Value::Int64(i))])
                .unwrap();
            g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
                .unwrap();
        }

        let q = parse_query("MATCH (a)-[:KNOWS]->(b) RETURN b LIMIT 3");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 3);
    }

    #[test]
    fn limit_pushdown_applies_with_where_when_no_order_by() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        for i in 0..20 {
            let b = g
                .create_vertex(vec!["User".into()], vec![("n".into(), Value::Int64(i))])
                .unwrap();
            g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
                .unwrap();
        }

        let q = parse_query("MATCH (a)-[:KNOWS]->(b) WHERE b.n >= 10 RETURN b.n LIMIT 2");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert!(
            r.rows
                .iter()
                .all(|row| matches!(row.first(), Some(Value::Int64(v)) if *v >= 10))
        );
    }

    #[test]
    fn expand_and_property_filter() {
        let mut g = seeded_chain();
        let q =
            parse_query(r#"MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.name = 'Bob' RETURN b.name"#);
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Text("Bob".into())]]);

        // Tombstone target should be filtered from expansion results.
        let bob_id = 1u32;
        g.delete_vertex(bob_id).unwrap();
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert!(r.rows.is_empty());
    }

    #[test]
    fn where_expr_boolean_null_in_and_precedence() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b1 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("name".into(), Value::Text("Bob".into())),
                    ("n".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let b2 = g
            .create_vertex(vec!["User".into()], vec![("n".into(), Value::Int64(2))])
            .unwrap();
        let b3 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("name".into(), Value::Text("Zed".into())),
                    ("n".into(), Value::Int64(3)),
                ],
            )
            .unwrap();
        for b in [b1, b2, b3] {
            g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
                .unwrap();
        }

        let q = parse_query(
            r#"MATCH (a)-[:KNOWS]->(b) WHERE NOT b.n = 1 AND (b.name IS NULL OR b.n IN [2, 4]) RETURN b.n ORDER BY b.n"#,
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(2)]]);
    }

    #[test]
    fn where_exists_nested_query_filters_rows() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("X".into()), vec![], 1.0, 1)
            .unwrap();

        let q = parse_query(
            r#"MATCH (x)-[:X]->(y) WHERE EXISTS { MATCH (m)-[:X]->(n) RETURN m } RETURN x LIMIT 1"#,
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn set_updates_vertex_property_and_label_and_edge_property() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g
            .create_vertex(vec!["User".into()], vec![("age".into(), Value::Int64(20))])
            .unwrap();
        let b = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let s = parse_statement(
            "MATCH (a)-[e:KNOWS]->(b) WHERE id(a) = 0 SET a.age = 31, a:Member, e.since = 2020",
        )
        .unwrap();
        validate_statement(&s).unwrap();
        let m = execute_mutation(&s, &mut g).unwrap();
        assert_eq!(m.affected_vertices, 1);
        assert_eq!(m.affected_edges, 1);

        let q = parse_query(
            "MATCH (a:Member)-[e:KNOWS]->(b) WHERE a.age = 31 AND e.since = 2020 RETURN a.age",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int32(31)]]);
    }

    #[test]
    fn remove_deletes_vertex_property_label_and_edge_property() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g
            .create_vertex(
                vec!["User".into(), "Temporary".into()],
                vec![("age".into(), Value::Int64(20))],
            )
            .unwrap();
        let b = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(
            a,
            b,
            Some("KNOWS".into()),
            vec![("since".into(), Value::Int64(2020))],
            1.0,
            1,
        )
        .unwrap();

        let s = parse_statement(
            "MATCH (a)-[e:KNOWS]->(b) WHERE id(a) = 0 REMOVE a.age, a:Temporary, e.since",
        )
        .unwrap();
        validate_statement(&s).unwrap();
        let m = execute_mutation(&s, &mut g).unwrap();
        assert_eq!(m.affected_vertices, 1);
        assert_eq!(m.affected_edges, 1);

        let q = parse_query(
            "MATCH (a:User)-[e:KNOWS]->(b) WHERE a.age IS NULL AND e.since IS NULL RETURN a",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 1);
        let q2 = parse_query("MATCH (a:Temporary)-[:KNOWS]->(b) RETURN a");
        let r2 = execute_query(&q2, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert!(r2.rows.is_empty());
    }

    #[test]
    fn incoming_match_works() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        let q = parse_query("MATCH (x)<-[:KNOWS]-(y) RETURN x, y");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn combined_forward_backward_hops_work() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let c = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("K".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(c, b, Some("K".into()), vec![], 1.0, 1)
            .unwrap();
        let q = parse_query("MATCH (x)-[:K]->(y)<-[:K]-(z) RETURN y");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert!(!r.rows.is_empty());
    }

    #[test]
    fn detach_delete_removes_incident_edges_plain_delete_rejects() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("X".into()), vec![], 1.0, 1)
            .unwrap();

        let plain = parse_statement("MATCH (a)-[:X]->(b) WHERE id(a) = 0 DELETE a").unwrap();
        let err = execute_mutation(&plain, &mut g).unwrap_err();
        assert!(matches!(err, GleaphError::ValidationError(_)));

        let det = parse_statement("MATCH (a)-[:X]->(b) WHERE id(a) = 0 DETACH DELETE a").unwrap();
        let m = execute_mutation(&det, &mut g).unwrap();
        assert_eq!(m.affected_vertices, 1);
        let q = parse_query("MATCH (a)-[:X]->(b) RETURN a");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert!(r.rows.is_empty());
    }

    #[test]
    fn distinct_and_offset_work() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Bob".into()))],
            )
            .unwrap();
        let c = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Bob".into()))],
            )
            .unwrap();
        let d = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Zed".into()))],
            )
            .unwrap();
        for t in [b, c, d] {
            g.create_edge(a, t, Some("X".into()), vec![], 1.0, 1)
                .unwrap();
        }

        let q = parse_query(
            "MATCH (a)-[:X]->(b) RETURN DISTINCT b.name ORDER BY b.name LIMIT 10 OFFSET 1",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Text("Zed".into())]]);

        let q2 = parse_query("MATCH (a)-[:X]->(b) RETURN b.name ORDER BY b.name OFFSET 2");
        let r2 = execute_query(&q2, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r2.rows.len(), 1);
    }

    #[test]
    fn where_scalar_functions_string_numeric_element_and_list() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("  Bob  ".into()))],
            )
            .unwrap();
        let b = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let q = parse_query(
            r#"MATCH (a)-[e:KNOWS]->(b)
               WHERE upper(trim(a.name)) = 'BOB'
                 AND abs(-2) = 2
                 AND floor(2.9) = 2.0
                 AND id(a) = 0
                 AND type(e) = 'KNOWS'
                 AND size(labels(a)) >= 1
                 AND head(range(1,3)) = 1
                 AND head(tail(range(1,3))) = 2
               RETURN a"#,
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn optional_match_with_and_without_result_emits_null_binding() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b2 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let c = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b1, Some("X".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(a, b2, Some("X".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(b1, c, Some("Y".into()), vec![], 1.0, 1)
            .unwrap();

        let q =
            parse_query("MATCH (a)-[:X]->(b) OPTIONAL MATCH (b)-[:Y]->(c) RETURN b, c ORDER BY b");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert!(
            r.rows
                .iter()
                .any(|row| matches!(row.get(1), Some(Value::Null)))
        );
        assert!(
            r.rows
                .iter()
                .any(|row| matches!(row.get(1), Some(Value::Int64(_))))
        );
    }

    #[test]
    fn leading_optional_match_is_supported() {
        let g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let q = parse_query("OPTIONAL MATCH (a)-[:X]->(b) RETURN a, b");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 1);
        assert!(r.rows[0].iter().all(|v| matches!(v, Value::Null)));
    }

    #[test]
    fn project_sort_limit() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("C".into()))],
            )
            .unwrap();
        let c = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("A".into()))],
            )
            .unwrap();
        let d = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("B".into()))],
            )
            .unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(a, c, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(a, d, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let q = parse_query(
            "MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name ORDER BY b.name ASC LIMIT 2",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(
            r.rows,
            vec![vec![Value::Text("A".into())], vec![Value::Text("B".into())]]
        );
        assert!(r.stats.execution_steps > 0);
    }

    #[test]
    fn order_by_can_use_non_projected_expression() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("name".into(), Value::Text("B".into())),
                    ("rank".into(), Value::Int64(2)),
                ],
            )
            .unwrap();
        let c = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("name".into(), Value::Text("A".into())),
                    ("rank".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(a, c, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let q = parse_query("MATCH (a)-[:KNOWS]->(b) RETURN b.name ORDER BY b.rank ASC");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(
            r.rows,
            vec![vec![Value::Text("A".into())], vec![Value::Text("B".into())]]
        );
    }

    #[test]
    fn create_and_delete_mutations() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();

        let s = parse_statement(r#"INSERT (:User {name: 'A'})"#).unwrap();
        validate_statement(&s).unwrap();
        let m = execute_mutation(&s, &mut g).unwrap();
        assert_eq!(m.affected_vertices, 1);

        let s =
            parse_statement(r#"INSERT (:User {name: 'B'})-[:KNOWS]->(:User {name: 'C'})"#).unwrap();
        validate_statement(&s).unwrap();
        let m = execute_mutation(&s, &mut g).unwrap();
        assert_eq!(m.affected_edges, 1);

        let d = parse_statement(
            r#"MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.name = 'C' DETACH DELETE b"#,
        )
        .unwrap();
        validate_statement(&d).unwrap();
        let m = execute_mutation(&d, &mut g).unwrap();
        assert_eq!(m.affected_vertices, 1);
    }

    #[test]
    fn create_incoming_edge_preserves_direction() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let s =
            parse_statement(r#"INSERT (:User {name: 'A'})<-[:KNOWS]-(:User {name: 'B'})"#).unwrap();
        validate_statement(&s).unwrap();
        let m = execute_mutation(&s, &mut g).unwrap();
        assert_eq!(m.affected_edges, 1);

        let q =
            parse_query(r#"MATCH (a:User)-[:KNOWS]->(b:User) WHERE a.name = 'B' RETURN b.name"#);
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Text("A".into())]]);
    }

    #[test]
    fn delete_counts_unique_targets_once() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("B".into()))],
            )
            .unwrap();
        let c = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(c, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let d = parse_statement(
            r#"MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.name = 'B' DETACH DELETE b"#,
        )
        .unwrap();
        validate_statement(&d).unwrap();
        let m = execute_mutation(&d, &mut g).unwrap();
        assert_eq!(m.affected_vertices, 1);
    }

    #[test]
    fn incoming_match_returns_rows() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let q = parse_query("MATCH (a)<-[:KNOWS]-(b) RETURN a");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn delete_edge_variable_hides_edge_from_future_matches() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("A".into()))],
            )
            .unwrap();
        let b = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("B".into()))],
            )
            .unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let d = parse_statement(r#"MATCH (a)-[e]->(b) WHERE b.name = 'B' DELETE e"#).unwrap();
        validate_statement(&d).unwrap();
        let m = execute_mutation(&d, &mut g).unwrap();
        assert_eq!(m.affected_edges, 1);

        let q = parse_query(r#"MATCH (a)-[:KNOWS]->(b) WHERE b.name = 'B' RETURN b.name"#);
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert!(r.rows.is_empty());
    }

    #[test]
    fn plan_execution_round_trip_multi_hop_pipeline() {
        let g = seeded_chain();
        let stmt = parse_statement(
            "MATCH (a:User)-[:KNOWS]->(b:User)-[:KNOWS]->(c:User) RETURN c.name ORDER BY c.name DESC LIMIT 1",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let result = execute_plan(&plan, &g).unwrap();
        assert_eq!(result.rows, vec![vec![Value::Text("Carol".into())]]);
    }

    #[test]
    fn execution_limits_stop_on_row_cap_during_match() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        for i in 0..5 {
            let b = g
                .create_vertex(vec!["User".into()], vec![("n".into(), Value::Int64(i))])
                .unwrap();
            g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
                .unwrap();
        }

        let stmt = parse_statement("MATCH (a)-[:KNOWS]->(b) RETURN b.n").unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let err = execute_plan_with_limits(
            &plan,
            &g,
            ExecutionLimits {
                max_rows: Some(2),
                max_execution_steps: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, GleaphError::ExecutionError(_)));
        assert!(err.to_string().contains("row count"));
    }

    #[test]
    fn execution_limits_stop_on_step_cap_during_query() {
        let g = seeded_chain();
        let stmt = parse_statement("MATCH (a)-[:KNOWS]->(b) RETURN b.name").unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let err = execute_plan_with_limits(
            &plan,
            &g,
            ExecutionLimits {
                max_rows: None,
                max_execution_steps: Some(1),
            },
        )
        .unwrap_err();
        assert!(matches!(err, GleaphError::ExecutionError(_)));
        assert!(err.to_string().contains("execution steps"));
    }

    fn seeded_chain() -> PmaGraph<VecMemory> {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Alice".into()))],
            )
            .unwrap();
        let b = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Bob".into()))],
            )
            .unwrap();
        let c = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Carol".into()))],
            )
            .unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(b, c, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        g
    }

    #[test]
    fn executes_union_and_union_all() {
        let g = seeded_chain();
        let union = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) RETURN b.name \
             UNION \
             MATCH (a)-[:KNOWS]->(b) RETURN b.name",
        )
        .unwrap();
        validate_statement(&union).unwrap();
        let r = execute_read_statement(&union, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows.len(), 2);

        let union_all = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) RETURN b.name \
             UNION ALL \
             MATCH (a)-[:KNOWS]->(b) RETURN b.name",
        )
        .unwrap();
        validate_statement(&union_all).unwrap();
        let r = execute_read_statement(&union_all, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows.len(), 4);
    }

    #[test]
    fn executes_except_and_intersect() {
        let g = seeded_chain();
        let except = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) RETURN b.name \
             EXCEPT \
             MATCH (a)-[:KNOWS]->(b) WHERE b.name = 'Bob' RETURN b.name",
        )
        .unwrap();
        validate_statement(&except).unwrap();
        let r = execute_read_statement(&except, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Text("Carol".into())]]);

        let intersect = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) RETURN b.name \
             INTERSECT \
             MATCH (a)-[:KNOWS]->(b) WHERE b.name = 'Bob' RETURN b.name",
        )
        .unwrap();
        validate_statement(&intersect).unwrap();
        let r = execute_read_statement(&intersect, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Text("Bob".into())]]);
    }

    #[test]
    fn aggregates_count_and_implicit_grouping() {
        let g = seeded_chain();
        let stmt = parse_statement("MATCH (a)-[:KNOWS]->(b) RETURN COUNT(*)").unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(2)]]);

        let stmt = parse_statement("MATCH (a)-[:KNOWS]->(b) RETURN a.name, COUNT(*)").unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert!(
            r.rows
                .iter()
                .any(|row| row == &vec![Value::Text("Alice".into()), Value::Int64(1)])
        );
        assert!(
            r.rows
                .iter()
                .any(|row| row == &vec![Value::Text("Bob".into()), Value::Int64(1)])
        );
    }

    #[test]
    fn aggregates_group_by_and_having() {
        let g = seeded_chain();
        let stmt = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) RETURN a.name, COUNT(*) GROUP BY a.name HAVING COUNT(*) = 1",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn aggregates_order_by_limit_offset_on_grouped_rows() {
        let mut g = seeded_chain();
        let a = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Alice".into()))],
            )
            .unwrap();
        let d = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Dora".into()))],
            )
            .unwrap();
        g.create_edge(a, d, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) RETURN a.name AS n, COUNT(*) AS c GROUP BY a.name ORDER BY c DESC, n ASC LIMIT 1",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(
            r.rows,
            vec![vec![Value::Text("Alice".into()), Value::Int64(2)]]
        );

        let stmt = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) RETURN a.name AS n, COUNT(*) AS c GROUP BY a.name ORDER BY n ASC OFFSET 1",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn aggregates_empty_group_and_null_handling() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b1 = g
            .create_vertex(
                vec!["User".into()],
                vec![("score".into(), Value::Int64(10))],
            )
            .unwrap();
        let b2 = g
            .create_vertex(vec!["User".into()], vec![("score".into(), Value::Null)])
            .unwrap();
        g.create_edge(a, b1, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(a, b2, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) RETURN COUNT(b.score), SUM(b.score), AVG(b.score), MIN(b.score), MAX(b.score), COLLECT(b.score)",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int64(1));
        assert_eq!(r.rows[0][1], Value::Float64(10.0));
        assert_eq!(r.rows[0][2], Value::Float64(10.0));
        assert_eq!(r.rows[0][3], Value::Int64(10));
        assert_eq!(r.rows[0][4], Value::Int64(10));
        assert_eq!(r.rows[0][5], Value::List(vec![Value::Int64(10)]));

        let stmt = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) WHERE b.score > 999 RETURN COUNT(*), COUNT(b.score)",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(0), Value::Int64(0)]]);
    }

    #[test]
    fn aggregates_enforce_max_groups() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) RETURN a, COUNT(*) GROUP BY a").unwrap();
        validate_statement(&stmt).unwrap();
        let _guard = ensure_registry(&stmt);
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        let g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let mut rows = Vec::new();
        for i in 0..=10_000u32 {
            let mut b = Bindings::new();
            b.insert("a".into(), Binding::Vertex(i + 1));
            rows.push(b);
        }
        let err = project_aggregated_rows(&q, &rows, &g, &RandomState::new(), None).unwrap_err();
        assert!(matches!(err, GleaphError::ExecutionError(_)));
        assert!(err.to_string().contains("MAX_GROUPS"));
    }

    #[test]
    fn with_clause_supports_post_agg_filter_and_chaining() {
        let g = seeded_chain();
        let stmt = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) \
             WITH a.name AS name, COUNT(*) AS c \
             WHERE c > 0 \
             WITH name, c \
             RETURN name, c ORDER BY name ASC",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(
            r.rows[0],
            vec![Value::Text("Alice".into()), Value::Int64(1)]
        );
        assert_eq!(r.rows[1], vec![Value::Text("Bob".into()), Value::Int64(1)]);
    }

    #[test]
    fn path_variable_binding_and_length_function_work() {
        let g = seeded_chain();
        let stmt = parse_statement(
            "MATCH p = (a)-[:KNOWS]->(b) RETURN p, length(p) ORDER BY length(p) DESC",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert!(matches!(r.rows[0][0], Value::Path(_)));
        assert_eq!(r.rows[0][1], Value::Int64(1));
    }

    #[test]
    fn shortest_match_selects_shortest_path() {
        let mut g = seeded_chain();
        let alice = g
            .all_vertices()
            .into_iter()
            .find(|v| {
                g.get_vertex_props(*v)
                    .unwrap_or_default()
                    .iter()
                    .any(|(k, val)| k == "name" && *val == Value::Text("Alice".into()))
            })
            .unwrap();
        let carol = g
            .all_vertices()
            .into_iter()
            .find(|v| {
                g.get_vertex_props(*v)
                    .unwrap_or_default()
                    .iter()
                    .any(|(k, val)| k == "name" && *val == Value::Text("Carol".into()))
            })
            .unwrap();
        g.create_edge(alice, carol, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let stmt = parse_statement(
            "MATCH SHORTEST p = (a)-[:KNOWS*1..3]->(b) \
             WHERE a.name = 'Alice' AND b.name = 'Carol' \
             RETURN length(p)",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(1)]]);
    }

    #[test]
    fn shortest_match_on_disconnected_graph_returns_no_rows() {
        let g = seeded_chain();
        let stmt = parse_statement(
            "MATCH SHORTEST p = (a)-[:KNOWS*1..3]->(b) \
             WHERE a.name = 'Carol' AND b.name = 'Alice' \
             RETURN p, length(p)",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let r = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert!(r.rows.is_empty());
    }

    #[test]
    fn shortest_match_can_use_bfs_when_target_is_prebound() {
        let g = seeded_chain();
        let stmt = parse_statement(
            "MATCH SHORTEST p = (a)-[:KNOWS*1..3]->(b) \
             WHERE a.name = 'Alice' AND b.name = 'Carol' \
             RETURN length(p)",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let _guard = ensure_registry(&stmt);
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        let entry = &q.match_clauses[0];
        let mut stats = QueryStats::default();
        let mut seed = Bindings::new();

        let alice = g
            .all_vertices()
            .into_iter()
            .find(|v| {
                g.get_vertex_props(*v)
                    .unwrap_or_default()
                    .iter()
                    .any(|(k, val)| k == "name" && *val == Value::Text("Alice".into()))
            })
            .unwrap();
        let carol = g
            .all_vertices()
            .into_iter()
            .find(|v| {
                g.get_vertex_props(*v)
                    .unwrap_or_default()
                    .iter()
                    .any(|(k, val)| k == "name" && *val == Value::Text("Carol".into()))
            })
            .unwrap();
        seed.insert("a".into(), Binding::Vertex(alice));
        seed.insert("b".into(), Binding::Vertex(carol));
        let rows = try_shortest_via_bfs(
            alice,
            &seed,
            &[PathElement::Node(alice)],
            entry.path_variable.as_deref(),
            &entry.pattern,
            &g,
            &mut stats,
            q.where_clause.as_ref(),
            None,
            ExecutionLimits::default(),
        )
        .unwrap()
        .expect("bfs shortcut should apply");
        assert_eq!(rows.len(), 1);
        let p = binding_value("p", &rows[0]);
        assert_eq!(eval_function_call("length", &[p]), Value::Int64(2));
    }

    #[test]
    fn execute_plan_supports_shortest_plan_operator_path() {
        let g = seeded_chain();
        let stmt = parse_statement(
            "MATCH SHORTEST p = (a)-[:KNOWS*1..3]->(b) WHERE a.name = 'Alice' AND b.name = 'Carol' RETURN length(p)",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::ShortestPath)));
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(2)]]);
    }

    #[test]
    fn execute_plan_supports_aggregate_plan_operator_path() {
        let g = seeded_chain();
        let stmt =
            parse_statement("MATCH (a)-[:KNOWS]->(b) RETURN a.name, COUNT(*) ORDER BY a.name ASC")
                .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::Aggregate)));
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn execute_plan_supports_top_k_count_by_terminal_key_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a0 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(0))])
            .unwrap();
        let a1 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(1))])
            .unwrap();
        let a2 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(2))])
            .unwrap();
        let b0 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(100))])
            .unwrap();
        let b1 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(101))])
            .unwrap();
        g.create_edge(a0, b0, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();
        g.create_edge(a1, b0, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();
        g.create_edge(a2, b1, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (a:User)-[:FOLLOWS]->(b:User) \
             RETURN b.id, COUNT(*) ORDER BY COUNT(*) DESC LIMIT 1",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(100), Value::Int64(2)]]);
        assert!(r.stats.rows_emitted == 1);
    }

    #[test]
    fn execute_plan_supports_top_k_count_by_terminal_property_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a0 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let a1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let a2 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b0 = g
            .create_vertex(vec!["User".into()], vec![("score".into(), Value::Int64(5))])
            .unwrap();
        let b1 = g
            .create_vertex(vec!["User".into()], vec![("score".into(), Value::Int64(9))])
            .unwrap();
        g.create_edge(a0, b0, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();
        g.create_edge(a1, b0, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();
        g.create_edge(a2, b1, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (a:User)-[:FOLLOWS]->(b:User) \
             RETURN b.score, COUNT(*) ORDER BY COUNT(*) DESC LIMIT 1",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(5), Value::Int64(2)]]);
        assert_eq!(r.stats.rows_emitted, 1);
    }

    #[test]
    fn execute_plan_supports_top_k_count_by_labeled_terminal_key_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a0 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(0))])
            .unwrap();
        let a1 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(1))])
            .unwrap();
        let a2 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(2))])
            .unwrap();
        let b0 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(100))])
            .unwrap();
        let b1 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(101))])
            .unwrap();
        g.create_edge(a0, b0, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();
        g.create_edge(a1, b0, Some("LIKES".into()), vec![], 1.0, 0)
            .unwrap();
        g.create_edge(a2, b1, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (a:User)-[:FOLLOWS]->(b:User) \
             RETURN b.id, COUNT(*) ORDER BY COUNT(*) DESC, b.id DESC LIMIT 1",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(101), Value::Int64(1)]]);
        assert_eq!(r.stats.rows_emitted, 1);
    }

    #[test]
    fn execute_plan_supports_top_k_count_by_start_key_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a0 = g
            .create_vertex(vec!["User".into()], vec![("score".into(), Value::Int64(7))])
            .unwrap();
        let a1 = g
            .create_vertex(vec!["User".into()], vec![("score".into(), Value::Int64(7))])
            .unwrap();
        let a2 = g
            .create_vertex(vec!["User".into()], vec![("score".into(), Value::Int64(9))])
            .unwrap();
        let b0 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b2 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a0, b0, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();
        g.create_edge(a1, b1, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();
        g.create_edge(a2, b2, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();
        g.create_edge(a2, b0, Some("FOLLOWS".into()), vec![], 1.0, 0)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (a:User)-[:FOLLOWS]->(b:User) \
             RETURN a.score, COUNT(*) ORDER BY COUNT(*) DESC, a.score DESC LIMIT 1",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(9), Value::Int64(2)]]);
        assert_eq!(r.stats.rows_emitted, 1);
    }

    #[test]
    fn execute_plan_supports_recent_two_hop_top_k_projection_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let me = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(42))])
            .unwrap();
        let f0 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let f1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let p0 = g
            .create_vertex(
                vec!["Post".into()],
                vec![
                    ("id".into(), Value::Int64(100)),
                    ("viral_score".into(), Value::Int64(7)),
                ],
            )
            .unwrap();
        let p1 = g
            .create_vertex(
                vec!["Post".into()],
                vec![
                    ("id".into(), Value::Int64(101)),
                    ("viral_score".into(), Value::Int64(9)),
                ],
            )
            .unwrap();
        let p2 = g
            .create_vertex(
                vec!["Post".into()],
                vec![
                    ("id".into(), Value::Int64(102)),
                    ("viral_score".into(), Value::Int64(5)),
                ],
            )
            .unwrap();
        g.create_edge(me, f0, Some("Follows".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(me, f1, Some("Follows".into()), vec![], 1.0, 2)
            .unwrap();
        g.create_edge(f0, p0, Some("Posted".into()), vec![], 1.0, 100)
            .unwrap();
        g.create_edge(f1, p1, Some("Posted".into()), vec![], 1.0, 200)
            .unwrap();
        g.create_edge(f1, p2, Some("Posted".into()), vec![], 1.0, 150)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (me:User {id: 42})-[:Follows]->(f:User)-[e:Posted]->(p:Post) \
             WHERE gleaph_timestamp(e) > 120 \
             RETURN p.id, p.viral_score, gleaph_timestamp(e) AS ts \
             ORDER BY ts DESC LIMIT 2",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(
            r.rows,
            vec![
                vec![Value::Int64(101), Value::Int64(9), Value::Timestamp(200)],
                vec![Value::Int64(102), Value::Int64(5), Value::Timestamp(150)],
            ]
        );
        assert_eq!(r.stats.rows_emitted, 2);
        assert!(r.stats.breakdown.recent_two_hop_projection_fast_path_used);
    }

    #[test]
    fn execute_plan_supports_var_len_terminal_projection_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let u = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(42))])
            .unwrap();
        let f = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let p0 = g
            .create_vertex(
                vec!["Post".into()],
                vec![
                    ("id".into(), Value::Int64(100)),
                    ("viral_score".into(), Value::Int64(7)),
                ],
            )
            .unwrap();
        let p1 = g
            .create_vertex(
                vec!["Post".into()],
                vec![
                    ("id".into(), Value::Int64(101)),
                    ("viral_score".into(), Value::Int64(9)),
                ],
            )
            .unwrap();
        g.create_edge(u, f, Some("Follows".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(u, p0, Some("Liked".into()), vec![], 1.0, 2)
            .unwrap();
        g.create_edge(f, p1, Some("Liked".into()), vec![], 1.0, 3)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (u:User {id: 42})-[:Follows|Liked*1..3]->(p:Post) \
             RETURN p.id, p.viral_score LIMIT 20",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(
            r.rows,
            vec![
                vec![Value::Int64(101), Value::Int64(9)],
                vec![Value::Int64(100), Value::Int64(7)],
            ]
        );
        assert_eq!(r.stats.rows_emitted, 2);
        assert!(r.stats.breakdown.var_len_terminal_projection_fast_path_used);
    }

    #[test]
    fn execute_plan_supports_two_hop_top_k_count_by_terminal_key_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let me = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(42))])
            .unwrap();
        let f0 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let f1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let rec0 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(100))])
            .unwrap();
        let rec1 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(101))])
            .unwrap();
        let rec_self = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(42))])
            .unwrap();

        g.create_edge(me, f0, Some("Follows".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(me, f1, Some("Follows".into()), vec![], 1.0, 2)
            .unwrap();
        g.create_edge(f0, rec0, Some("Follows".into()), vec![], 1.0, 3)
            .unwrap();
        g.create_edge(f1, rec0, Some("Follows".into()), vec![], 1.0, 4)
            .unwrap();
        g.create_edge(f1, rec1, Some("Follows".into()), vec![], 1.0, 5)
            .unwrap();
        g.create_edge(f0, rec_self, Some("Follows".into()), vec![], 1.0, 6)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (me:User {id: 42})-[:Follows]->(f:User)-[:Follows]->(rec:User) \
             WHERE rec.id <> 42 \
             RETURN rec.id, COUNT(*) AS mutual \
             ORDER BY mutual DESC LIMIT 10",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(
            r.rows,
            vec![
                vec![Value::Int64(100), Value::Int64(2)],
                vec![Value::Int64(101), Value::Int64(1)],
            ]
        );
        assert_eq!(r.stats.rows_emitted, 2);
        assert!(r.stats.breakdown.aggregate_fast_path_used);
    }

    #[test]
    fn execute_plan_supports_two_hop_count_by_middle_vertex_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let me = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(42))])
            .unwrap();
        let f0 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(100)),
                    ("verified".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let f1 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(101)),
                    ("verified".into(), Value::Int64(0)),
                ],
            )
            .unwrap();
        let p0 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();
        let p1 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();
        let p2 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();

        g.create_edge(me, f0, Some("Follows".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(me, f1, Some("Follows".into()), vec![], 1.0, 2)
            .unwrap();
        g.create_edge(f0, p0, Some("Posted".into()), vec![], 1.0, 3)
            .unwrap();
        g.create_edge(f0, p1, Some("Posted".into()), vec![], 1.0, 4)
            .unwrap();
        g.create_edge(f1, p2, Some("Posted".into()), vec![], 1.0, 5)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (:User {id: 42})-[:Follows]->(f:User)-[:Posted]->(p:Post) \
             RETURN f.id, f.verified, COUNT(*) AS post_count \
             ORDER BY post_count DESC",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(
            r.rows,
            vec![
                vec![Value::Int64(100), Value::Int64(1), Value::Int64(2)],
                vec![Value::Int64(101), Value::Int64(0), Value::Int64(1)],
            ]
        );
        assert_eq!(r.stats.rows_emitted, 2);
        assert!(r.stats.breakdown.aggregate_fast_path_used);
    }

    #[test]
    fn execute_plan_supports_seeded_top_k_count_by_terminal_key_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a0 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(1)),
                    ("verified".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let a1 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(2)),
                    ("verified".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let a2 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(3)),
                    ("verified".into(), Value::Int64(0)),
                ],
            )
            .unwrap();
        let b0 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(100))])
            .unwrap();
        let b1 = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(101))])
            .unwrap();
        g.create_edge(a0, b0, Some("Follows".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(a1, b0, Some("Follows".into()), vec![], 1.0, 2)
            .unwrap();
        g.create_edge(a1, b1, Some("Follows".into()), vec![], 1.0, 3)
            .unwrap();
        g.create_edge(a2, b1, Some("Follows".into()), vec![], 1.0, 4)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (a:User {verified: 1}) \
             WITH a LIMIT 500 \
             MATCH (a)-[:Follows]->(b:User) \
             RETURN b.id, COUNT(*) AS followers \
             ORDER BY followers DESC LIMIT 10",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(
            r.rows,
            vec![
                vec![Value::Int64(100), Value::Int64(2)],
                vec![Value::Int64(101), Value::Int64(1)],
            ]
        );
        assert_eq!(r.stats.rows_emitted, 2);
        assert!(r.stats.breakdown.aggregate_fast_path_used);
    }

    #[test]
    fn execute_plan_supports_seeded_top_k_count_with_edge_timestamp_filter_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let u0 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(1)),
                    ("verified".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let u1 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(2)),
                    ("verified".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let p0 = g
            .create_vertex(vec!["Post".into()], vec![("id".into(), Value::Int64(100))])
            .unwrap();
        let p1 = g
            .create_vertex(vec!["Post".into()], vec![("id".into(), Value::Int64(101))])
            .unwrap();
        g.create_edge(u0, p0, Some("Liked".into()), vec![], 1.0, 10)
            .unwrap();
        g.create_edge(u1, p0, Some("Liked".into()), vec![], 1.0, 11)
            .unwrap();
        g.create_edge(u1, p1, Some("Liked".into()), vec![], 1.0, 5)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (u:User {verified: 1}) \
             WITH u LIMIT 500 \
             MATCH (u)-[e:Liked]->(p:Post) \
             WHERE gleaph_timestamp(e) > 9 \
             RETURN p.id, COUNT(*) AS likes \
             ORDER BY likes DESC LIMIT 10",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int64(100), Value::Int64(2)]]);
        assert_eq!(r.stats.rows_emitted, 1);
        assert!(r.stats.breakdown.aggregate_fast_path_used);
    }

    #[test]
    fn execute_plan_supports_reverse_two_hop_top_k_count_by_terminal_key_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let h0 = g
            .create_vertex(vec!["Hashtag".into()], vec![("id".into(), Value::Int64(0))])
            .unwrap();
        let h1 = g
            .create_vertex(
                vec!["Hashtag".into()],
                vec![
                    ("id".into(), Value::Int64(1)),
                    ("category".into(), Value::Text("sports".into())),
                ],
            )
            .unwrap();
        let h2 = g
            .create_vertex(
                vec!["Hashtag".into()],
                vec![
                    ("id".into(), Value::Int64(2)),
                    ("category".into(), Value::Text("sports".into())),
                ],
            )
            .unwrap();
        let h3 = g
            .create_vertex(
                vec!["Hashtag".into()],
                vec![
                    ("id".into(), Value::Int64(3)),
                    ("category".into(), Value::Text("music".into())),
                ],
            )
            .unwrap();
        let p0 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();
        let p1 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();

        g.create_edge(p0, h0, Some("Tagged".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(p0, h1, Some("Tagged".into()), vec![], 1.0, 2)
            .unwrap();
        g.create_edge(p0, h2, Some("Tagged".into()), vec![], 1.0, 3)
            .unwrap();
        g.create_edge(p1, h0, Some("Tagged".into()), vec![], 1.0, 4)
            .unwrap();
        g.create_edge(p1, h3, Some("Tagged".into()), vec![], 1.0, 5)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (h:Hashtag {id: 0})<-[:Tagged]-(p:Post)-[:Tagged]->(other:Hashtag) \
             WHERE other.id <> 0 \
             RETURN other.category, COUNT(*) AS co_count \
             ORDER BY co_count DESC LIMIT 10",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(
            r.rows,
            vec![
                vec![Value::Text("sports".into()), Value::Int64(2)],
                vec![Value::Text("music".into()), Value::Int64(1)],
            ]
        );
        assert_eq!(r.stats.rows_emitted, 2);
        assert!(r.stats.breakdown.aggregate_fast_path_used);
    }

    #[test]
    fn execute_plan_supports_seeded_segmentation_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let u0 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(1)),
                    ("verified".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let u1 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(2)),
                    ("verified".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let p0 = g
            .create_vertex(
                vec!["Post".into()],
                vec![
                    ("content_type".into(), Value::Text("photo".into())),
                    ("viral_score".into(), Value::Int64(90)),
                ],
            )
            .unwrap();
        let p1 = g
            .create_vertex(
                vec!["Post".into()],
                vec![
                    ("content_type".into(), Value::Text("photo".into())),
                    ("viral_score".into(), Value::Int64(70)),
                ],
            )
            .unwrap();
        let p2 = g
            .create_vertex(
                vec!["Post".into()],
                vec![
                    ("content_type".into(), Value::Text("video".into())),
                    ("viral_score".into(), Value::Int64(95)),
                ],
            )
            .unwrap();

        g.create_edge(u0, p0, Some("Posted".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(u0, p1, Some("Posted".into()), vec![], 1.0, 2)
            .unwrap();
        g.create_edge(u1, p2, Some("Posted".into()), vec![], 1.0, 3)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (u:User {verified: 1}) \
             WITH u LIMIT 500 \
             MATCH (u)-[:Posted]->(p:Post) \
             RETURN p.content_type, COUNT(DISTINCT u) AS users, \
               COUNT(DISTINCT p) AS total_posts, \
               SUM(CASE WHEN p.viral_score > 80 THEN 1 ELSE 0 END) AS viral_posts, \
               AVG(p.viral_score) AS avg_score \
             ORDER BY users DESC",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(
            r.rows,
            vec![
                vec![
                    Value::Text("photo".into()),
                    Value::Int64(1),
                    Value::Int64(2),
                    Value::Float64(1.0),
                    Value::Float64(80.0),
                ],
                vec![
                    Value::Text("video".into()),
                    Value::Int64(1),
                    Value::Int64(1),
                    Value::Float64(1.0),
                    Value::Float64(95.0),
                ],
            ]
        );
        assert_eq!(r.stats.rows_emitted, 2);
        assert!(r.stats.breakdown.aggregate_fast_path_used);
    }

    #[test]
    fn execute_plan_supports_seeded_verified_influence_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let u0 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(1)),
                    ("verified".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let u1 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("id".into(), Value::Int64(2)),
                    ("verified".into(), Value::Int64(1)),
                ],
            )
            .unwrap();
        let p0 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();
        let p1 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();
        let h0 = g
            .create_vertex(
                vec!["Hashtag".into()],
                vec![("category".into(), Value::Text("sports".into()))],
            )
            .unwrap();
        let h1 = g
            .create_vertex(
                vec!["Hashtag".into()],
                vec![("category".into(), Value::Text("music".into()))],
            )
            .unwrap();

        g.create_edge(u0, p0, Some("Posted".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(u1, p1, Some("Posted".into()), vec![], 1.0, 2)
            .unwrap();
        g.create_edge(p0, h0, Some("Tagged".into()), vec![], 1.0, 3)
            .unwrap();
        g.create_edge(p0, h1, Some("Tagged".into()), vec![], 1.0, 4)
            .unwrap();
        g.create_edge(p1, h0, Some("Tagged".into()), vec![], 1.0, 5)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (u:User {verified: 1}) \
             WITH u LIMIT 500 \
             MATCH (u)-[:Posted]->(p:Post)-[:Tagged]->(h:Hashtag) \
             RETURN u.id, COLLECT(DISTINCT h.category) AS categories, \
               COUNT(DISTINCT h) AS hashtag_reach \
             ORDER BY hashtag_reach DESC LIMIT 10",
        )
        .unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(
            r.rows,
            vec![
                vec![
                    Value::Int64(1),
                    Value::List(vec![
                        Value::Text("music".into()),
                        Value::Text("sports".into()),
                    ]),
                    Value::Int64(2),
                ],
                vec![
                    Value::Int64(2),
                    Value::List(vec![Value::Text("sports".into())]),
                    Value::Int64(1),
                ],
            ]
        );
        assert_eq!(r.stats.rows_emitted, 2);
        assert!(r.stats.breakdown.aggregate_fast_path_used);
    }

    #[test]
    fn direct_two_hop_top_k_count_fast_path_matches_social_query_shape() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let me = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(42))])
            .unwrap();
        let f = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let rec = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(100))])
            .unwrap();
        g.create_edge(me, f, Some("Follows".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(f, rec, Some("Follows".into()), vec![], 1.0, 2)
            .unwrap();

        let stmt = parse_statement(
            "MATCH (me:User {id: 42})-[:Follows]->(f:User)-[:Follows]->(rec:User) \
             WHERE rec.id <> 42 \
             RETURN rec.id, COUNT(*) AS mutual \
             ORDER BY mutual DESC LIMIT 10",
        )
        .unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        let result = execute_two_hop_top_k_count_by_terminal_key_query(
            &q,
            &g,
            ExecutionLimits::default(),
            &RandomState::new(),
        )
        .unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn direct_two_hop_top_k_count_fast_path_matches_tokenized_social_query_shape() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let me = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(42))])
            .unwrap();
        let f = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let rec = g
            .create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(100))])
            .unwrap();
        g.create_edge(me, f, Some("Follows".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(f, rec, Some("Follows".into()), vec![], 1.0, 2)
            .unwrap();

        let tokens = crate::lexer::tokenize(
            "MATCH (me:User {id: 42})-[:Follows]->(f:User)-[:Follows]->(rec:User) \
             WHERE rec.id <> 42 \
             RETURN rec.id, COUNT(*) AS mutual \
             ORDER BY mutual DESC LIMIT 10",
        )
        .unwrap();
        let stmt = crate::parser::parse_statement_from_tokens(&tokens).unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        let result = execute_two_hop_top_k_count_by_terminal_key_query(
            &q,
            &g,
            ExecutionLimits::default(),
            &RandomState::new(),
        )
        .unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn execute_plan_supports_index_scan_operator_path_for_start_node_equality() {
        use crate::stats::TableStats;

        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a1 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("uid".into(), Value::Int32(42)),
                    ("name".into(), Value::Text("A".into())),
                ],
            )
            .unwrap();
        let a2 = g
            .create_vertex(
                vec!["User".into()],
                vec![
                    ("uid".into(), Value::Int32(7)),
                    ("name".into(), Value::Text("B".into())),
                ],
            )
            .unwrap();
        let b1 = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("X".into()))],
            )
            .unwrap();
        let b2 = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Y".into()))],
            )
            .unwrap();
        g.create_edge(a1, b1, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(a2, b2, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();

        let stmt =
            parse_statement("MATCH (a:User)-[:KNOWS]->(b) WHERE a.uid = 42 RETURN b.name").unwrap();
        validate_statement(&stmt).unwrap();

        let mut planner_stats = TableStats {
            vertex_count: 10_000,
            ..Default::default()
        };
        planner_stats.label_cardinality.insert("User".into(), 2_000);
        planner_stats
            .property_selectivity
            .insert("vertex:uid".into(), 0.9); // high cardinality = nearly all unique
        planner_stats.indexed_vertex_properties.insert("uid".into());
        let plan = crate::planner::build_plan_with_stats(&stmt, Some(&planner_stats)).unwrap();
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::IndexScan)));
        g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
            .unwrap();
        assert_eq!(
            g.scan_vertices_by_property_eq("uid", &Value::Int32(42)),
            VertexIdSet::from_iter([a1])
        );
        let _guard = ensure_registry(&stmt);
        let Statement::Query(q) = stmt.clone() else {
            panic!("expected query");
        };
        assert!(node_matches(&q.match_clauses[0].pattern.start, a1, &g));
        let mut seed = Bindings::new();
        seed.insert("a".into(), Binding::Vertex(a1));
        let mut tmp_stats = QueryStats::default();
        let seeded_rows = execute_query_match_entries_from_seed_rows(
            &q,
            &g,
            &mut tmp_stats,
            q.where_clause.as_ref(),
            None,
            ExecutionLimits::default(),
            vec![seed],
            None,
        )
        .unwrap();
        assert_eq!(seeded_rows.len(), 1);

        let r = execute_plan_with_limits(&plan, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Text("X".into())]]);
    }

    #[test]
    fn execute_plan_can_early_reject_on_estimated_budget() {
        let g = seeded_chain();
        let stmt = parse_statement("MATCH (a)-[:KNOWS]->(b) RETURN a, b").unwrap();
        validate_statement(&stmt).unwrap();
        let plan = build_plan(&stmt).unwrap();
        let err = execute_plan_with_limits(
            &plan,
            &g,
            ExecutionLimits {
                max_rows: None,
                max_execution_steps: Some(1),
            },
        )
        .unwrap_err();
        assert!(matches!(err, GleaphError::ExecutionError(_)));
        assert!(err.to_string().contains("estimated execution steps"));
    }

    // ── Scalar function unit tests (Step 5) ────────────────────────────────
    // Tests call eval_function_call directly (same module) to avoid the
    // MATCH-requires-edge validation constraint.

    #[test]
    fn scalar_string_upper_lower_trim() {
        use std::slice;
        let s = Value::Text("  Hello World  ".into());
        assert_eq!(
            eval_function_call("upper", slice::from_ref(&s)),
            Value::Text("  HELLO WORLD  ".into())
        );
        assert_eq!(
            eval_function_call("lower", slice::from_ref(&s)),
            Value::Text("  hello world  ".into())
        );
        assert_eq!(
            eval_function_call("trim", slice::from_ref(&s)),
            Value::Text("Hello World".into())
        );
        assert_eq!(
            eval_function_call("ltrim", slice::from_ref(&s)),
            Value::Text("Hello World  ".into())
        );
        assert_eq!(
            eval_function_call("rtrim", &[s]),
            Value::Text("  Hello World".into())
        );
    }

    #[test]
    fn scalar_string_left_right_substring() {
        let s = Value::Text("Hello World".into());
        assert_eq!(
            eval_function_call("left", &[s.clone(), Value::Int64(5)]),
            Value::Text("Hello".into())
        );
        assert_eq!(
            eval_function_call("right", &[s.clone(), Value::Int64(5)]),
            Value::Text("World".into())
        );
        // substring(str, start, len) and substring(str, start)
        assert_eq!(
            eval_function_call("substring", &[s.clone(), Value::Int64(6), Value::Int64(5)]),
            Value::Text("World".into())
        );
        assert_eq!(
            eval_function_call("substring", &[s, Value::Int64(6)]),
            Value::Text("World".into())
        );
    }

    #[test]
    fn scalar_string_contains_starts_ends_replace_size() {
        let s = Value::Text("Hello World".into());
        assert_eq!(
            eval_function_call("contains", &[s.clone(), Value::Text("World".into())]),
            Value::Bool(true)
        );
        assert_eq!(
            eval_function_call("starts_with", &[s.clone(), Value::Text("Hello".into())]),
            Value::Bool(true)
        );
        assert_eq!(
            eval_function_call("ends_with", &[s.clone(), Value::Text("World".into())]),
            Value::Bool(true)
        );
        assert_eq!(
            eval_function_call(
                "replace",
                &[
                    s.clone(),
                    Value::Text("World".into()),
                    Value::Text("Rust".into())
                ]
            ),
            Value::Text("Hello Rust".into()),
        );
        assert_eq!(eval_function_call("size", &[s]), Value::Int64(11));
    }

    #[test]
    fn scalar_numeric_abs_floor_ceil_round() {
        assert_eq!(
            eval_function_call("abs", &[Value::Int64(-5)]),
            Value::Int64(5)
        );
        assert_eq!(
            eval_function_call("abs", &[Value::Float64(-3.5)]),
            Value::Float64(3.5)
        );
        assert_eq!(
            eval_function_call("floor", &[Value::Float64(3.7)]),
            Value::Float64(3.0)
        );
        assert_eq!(
            eval_function_call("ceil", &[Value::Float64(3.1)]),
            Value::Float64(4.0)
        );
        assert_eq!(
            eval_function_call("round", &[Value::Float64(3.5)]),
            Value::Float64(4.0)
        );
        // int passes through unchanged
        assert_eq!(
            eval_function_call("floor", &[Value::Int64(7)]),
            Value::Int64(7)
        );
        assert_eq!(
            eval_function_call("ceil", &[Value::Int64(-2)]),
            Value::Int64(-2)
        );
    }

    #[test]
    fn scalar_type_conversion_functions() {
        assert_eq!(
            eval_function_call("tointeger", &[Value::Text("42".into())]),
            Value::Int64(42)
        );
        assert_eq!(
            eval_function_call("tointeger", &[Value::Float64(3.9)]),
            Value::Int64(3)
        );
        assert_eq!(
            eval_function_call("tointeger", &[Value::Text("bad".into())]),
            Value::Null
        );
        assert_eq!(
            eval_function_call("tofloat", &[Value::Int64(-5)]),
            Value::Float64(-5.0)
        );
        assert_eq!(
            eval_function_call("tofloat", &[Value::Text("1.5".into())]),
            Value::Float64(1.5)
        );
        assert_eq!(
            eval_function_call("tostring", &[Value::Int64(-5)]),
            Value::Text("-5".into())
        );
        assert_eq!(
            eval_function_call("tostring", &[Value::Bool(true)]),
            Value::Text("true".into())
        );
        assert_eq!(
            eval_function_call("tostring", &[Value::Float64(1.0)]),
            Value::Text("1".into())
        );
    }

    #[test]
    fn scalar_list_head_tail_range_size() {
        let list_1_5: Value = Value::List((1..=5).map(Value::Int64).collect());
        assert_eq!(
            eval_function_call("head", std::slice::from_ref(&list_1_5)),
            Value::Int64(1)
        );
        assert_eq!(eval_function_call("size", &[list_1_5]), Value::Int64(5));
        assert_eq!(
            eval_function_call(
                "tail",
                &[Value::List(vec![
                    Value::Int64(1),
                    Value::Int64(2),
                    Value::Int64(3)
                ])]
            ),
            Value::List(vec![Value::Int64(2), Value::Int64(3)]),
        );
        // single-element tail → empty list
        assert_eq!(
            eval_function_call("tail", &[Value::List(vec![Value::Int64(99)])]),
            Value::List(vec![])
        );
        // head on empty list → Null
        assert_eq!(
            eval_function_call("head", &[Value::List(vec![])]),
            Value::Null
        );
        // range ascending and descending
        assert_eq!(
            eval_function_call("range", &[Value::Int64(1), Value::Int64(3)]),
            Value::List(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)]),
        );
        assert_eq!(
            eval_function_call("range", &[Value::Int64(3), Value::Int64(1)]),
            Value::List(vec![Value::Int64(3), Value::Int64(2), Value::Int64(1)]),
        );
    }

    #[test]
    fn scalar_coalesce_and_nullif() {
        assert_eq!(
            eval_function_call("coalesce", &[Value::Null, Value::Int64(7), Value::Int64(8)]),
            Value::Int64(7),
        );
        assert_eq!(
            eval_function_call("coalesce", &[Value::Null, Value::Null]),
            Value::Null,
        );
        assert_eq!(
            eval_function_call("nullif", &[Value::Int64(5), Value::Int64(5)]),
            Value::Null,
        );
        assert_eq!(
            eval_function_call("nullif", &[Value::Int64(5), Value::Int64(6)]),
            Value::Int64(5),
        );
    }

    #[test]
    fn scalar_functions_return_null_on_wrong_type() {
        // String functions with non-string input
        assert_eq!(eval_function_call("upper", &[Value::Int64(1)]), Value::Null);
        assert_eq!(eval_function_call("lower", &[Value::Int64(1)]), Value::Null);
        assert_eq!(eval_function_call("trim", &[Value::Null]), Value::Null);
        assert_eq!(
            eval_function_call("left", &[Value::Int64(1), Value::Int64(2)]),
            Value::Null
        );
        assert_eq!(
            eval_function_call("starts_with", &[Value::Int64(1), Value::Text("x".into())]),
            Value::Null
        );
        assert_eq!(
            eval_function_call("contains", &[Value::Null, Value::Text("x".into())]),
            Value::Null
        );
        // Numeric functions with non-numeric input
        assert_eq!(
            eval_function_call("abs", &[Value::Text("a".into())]),
            Value::Null
        );
        assert_eq!(
            eval_function_call("floor", &[Value::Text("a".into())]),
            Value::Null
        );
        assert_eq!(eval_function_call("ceil", &[Value::Null]), Value::Null);
        // List functions with non-list input
        assert_eq!(eval_function_call("head", &[Value::Int64(1)]), Value::Null);
        assert_eq!(eval_function_call("tail", &[Value::Int64(1)]), Value::Null);
        assert_eq!(eval_function_call("size", &[Value::Int64(1)]), Value::Null);
        // Wrong argument count
        assert_eq!(eval_function_call("upper", &[]), Value::Null);
        assert_eq!(
            eval_function_call("substring", &[Value::Text("a".into())]),
            Value::Null
        );
        assert_eq!(eval_function_call("range", &[Value::Int64(1)]), Value::Null);
        assert_eq!(
            eval_function_call("nullif", &[Value::Int64(1)]),
            Value::Null
        );
    }

    #[test]
    fn scalar_functions_return_null_on_null_input() {
        assert_eq!(eval_function_call("upper", &[Value::Null]), Value::Null);
        assert_eq!(eval_function_call("lower", &[Value::Null]), Value::Null);
        assert_eq!(eval_function_call("abs", &[Value::Null]), Value::Null);
        assert_eq!(eval_function_call("floor", &[Value::Null]), Value::Null);
        assert_eq!(eval_function_call("tostring", &[Value::Null]), Value::Null);
        assert_eq!(eval_function_call("tointeger", &[Value::Null]), Value::Null);
        assert_eq!(eval_function_call("tofloat", &[Value::Null]), Value::Null);
        assert_eq!(eval_function_call("head", &[Value::Null]), Value::Null);
        assert_eq!(eval_function_call("tail", &[Value::Null]), Value::Null);
    }

    // ── end scalar function tests ───────────────────────────────────────────

    #[test]
    fn extracts_edge_timestamp_range_from_where_expr() {
        let stmt = parse_statement(
            "MATCH SHORTEST p = (a)-[e:KNOWS*1..3]->(b) \
             WHERE gleaph_timestamp(e) >= 10 AND gleaph_timestamp(e) < 20 RETURN p",
        )
        .unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        let range = extract_edge_ts_range(q.where_clause.as_ref(), Some("e")).expect("range");
        assert_eq!(range.start, Some(10));
        assert_eq!(range.end, Some(19));
    }

    // ── accessor function tests ───────────────────────────────────────────

    #[test]
    fn source_and_destination_functions() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec![], vec![]).unwrap();
        let b = g.create_vertex(vec![], vec![]).unwrap();
        g.create_edge(a, b, Some("X".into()), vec![], 1.0, 0)
            .unwrap();

        let q = parse_query("MATCH (a)-[e:X]->(b) RETURN source(e), destination(e)");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(
            r.rows,
            vec![vec![Value::Int64(i64::from(a)), Value::Int64(i64::from(b))]]
        );
    }

    #[test]
    fn gleaph_weight_and_gleaph_timestamp_functions() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec![], vec![]).unwrap();
        let b = g.create_vertex(vec![], vec![]).unwrap();
        g.create_edge(a, b, Some("X".into()), vec![], 3.5, 42)
            .unwrap();

        let q = parse_query("MATCH (a)-[e:X]->(b) RETURN gleaph_weight(e), gleaph_timestamp(e)");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(
            r.rows,
            vec![vec![Value::Float64(3.5), Value::Timestamp(42)]]
        );
    }

    #[test]
    fn weight_and_timestamp_deprecated_aliases() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec![], vec![]).unwrap();
        let b = g.create_vertex(vec![], vec![]).unwrap();
        g.create_edge(a, b, Some("X".into()), vec![], 2.0, 99)
            .unwrap();

        let q = parse_query("MATCH (a)-[e:X]->(b) RETURN weight(e), timestamp(e)");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(
            r.rows,
            vec![vec![Value::Float64(2.0), Value::Timestamp(99)]]
        );
    }

    #[test]
    fn dot_id_returns_user_property_not_vertex_id() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let _v0 = g
            .create_vertex(vec![], vec![("id".into(), Value::Int64(999))])
            .unwrap();

        let q = parse_query("MATCH (n) RETURN n.id");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        // n.id should return the stored "id" property (999), NOT the vertex internal id (0).
        assert_eq!(r.rows, vec![vec![Value::Int64(999)]]);
    }

    #[test]
    fn dot_id_returns_null_when_no_id_property() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let _v0 = g
            .create_vertex(vec![], vec![("name".into(), Value::Text("test".into()))])
            .unwrap();

        let q = parse_query("MATCH (n) RETURN n.id");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        // No stored "id" property → Null
        assert_eq!(r.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn sum_gleaph_weight_uses_aggregate_fast_path() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let a = g.create_vertex(vec!["A".into()], vec![]).unwrap();
        let b = g.create_vertex(vec!["A".into()], vec![]).unwrap();
        let c = g.create_vertex(vec!["A".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("X".into()), vec![], 2.0, 0)
            .unwrap();
        g.create_edge(a, c, Some("X".into()), vec![], 3.0, 0)
            .unwrap();

        let q = parse_query("MATCH (a:A)-[e:X]->(b:A) RETURN SUM(gleaph_weight(e))");
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Float64(5.0)]]);
        assert!(r.stats.breakdown.aggregate_compiled_fast_path_used);
    }

    /// Test that bound-chain-target reverse anchor produces identical results to
    /// the unoptimized path.  The OPTIONAL MATCH has an unbound start (:A, many
    /// candidates) and a bound chain target (p, from the first MATCH).  The
    /// optimisation reverses direction to iterate incoming edges of `p` instead
    /// of scanning all :A candidates.
    #[test]
    fn reverse_chain_anchor_optional_match_outgoing() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        // Create many :A vertices (to make start_candidates > 1)
        let mut a_ids = Vec::new();
        for _ in 0..10 {
            a_ids.push(g.create_vertex(vec!["A".into()], vec![]).unwrap());
        }
        // Create target :B vertices
        let b0 = g.create_vertex(vec!["B".into()], vec![]).unwrap();
        let b1 = g.create_vertex(vec!["B".into()], vec![]).unwrap();
        // Anchor vertex
        let root = g
            .create_vertex(vec!["Root".into()], vec![("id".into(), Value::Int64(0))])
            .unwrap();
        // root -[:R]-> b0, root -[:R]-> b1
        g.create_edge(root, b0, Some("R".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(root, b1, Some("R".into()), vec![], 1.0, 2)
            .unwrap();
        // a_ids[0] -[:L]-> b0, a_ids[1] -[:L]-> b0, a_ids[2] -[:L]-> b1
        g.create_edge(a_ids[0], b0, Some("L".into()), vec![], 1.0, 10)
            .unwrap();
        g.create_edge(a_ids[1], b0, Some("L".into()), vec![], 1.0, 11)
            .unwrap();
        g.create_edge(a_ids[2], b1, Some("L".into()), vec![], 1.0, 12)
            .unwrap();

        let q = parse_query(
            "MATCH (root:Root {id: 0})-[:R]->(p:B) \
             OPTIONAL MATCH (liker:A)-[:L]->(p) \
             RETURN id(root), id(p), id(liker) ORDER BY id(p), id(liker)",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        // b0 has 2 likers (a0, a1), b1 has 1 liker (a2)
        assert_eq!(r.rows.len(), 3);
        // Check that all rows have the correct root
        for row in &r.rows {
            assert_eq!(row[0], Value::Int64(root as i64));
        }
        // Check target and liker ids
        assert_eq!(r.rows[0][1], Value::Int64(b0 as i64));
        assert_eq!(r.rows[0][2], Value::Int64(a_ids[0] as i64));
        assert_eq!(r.rows[1][1], Value::Int64(b0 as i64));
        assert_eq!(r.rows[1][2], Value::Int64(a_ids[1] as i64));
        assert_eq!(r.rows[2][1], Value::Int64(b1 as i64));
        assert_eq!(r.rows[2][2], Value::Int64(a_ids[2] as i64));
    }

    /// Test reverse chain anchor with Incoming direction:
    /// OPTIONAL MATCH (liker:A)<-[:L]-(p)
    #[test]
    fn reverse_chain_anchor_incoming_direction() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let mut a_ids = Vec::new();
        for _ in 0..5 {
            a_ids.push(g.create_vertex(vec!["A".into()], vec![]).unwrap());
        }
        let b0 = g.create_vertex(vec!["B".into()], vec![]).unwrap();
        let root = g
            .create_vertex(vec!["Root".into()], vec![("id".into(), Value::Int64(0))])
            .unwrap();
        g.create_edge(root, b0, Some("R".into()), vec![], 1.0, 1)
            .unwrap();
        // b0 -[:L]-> a_ids[0], b0 -[:L]-> a_ids[1]
        g.create_edge(b0, a_ids[0], Some("L".into()), vec![], 1.0, 10)
            .unwrap();
        g.create_edge(b0, a_ids[1], Some("L".into()), vec![], 1.0, 11)
            .unwrap();

        let q = parse_query(
            "MATCH (root:Root {id: 0})-[:R]->(p:B) \
             OPTIONAL MATCH (reached:A)<-[:L]-(p) \
             RETURN id(root), id(p), id(reached) ORDER BY id(reached)",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][2], Value::Int64(a_ids[0] as i64));
        assert_eq!(r.rows[1][2], Value::Int64(a_ids[1] as i64));
    }

    /// Test reverse chain anchor correctness with COUNT aggregation,
    /// matching the engagement_rate benchmark pattern.
    #[test]
    fn reverse_chain_anchor_count_distinct() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        // 20 :User vertices (enough to trigger the optimisation)
        let mut users = Vec::new();
        for i in 0..20 {
            users.push(
                g.create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(i))])
                    .unwrap(),
            );
        }
        // user 0 posts 2 posts
        let p0 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();
        let p1 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();
        g.create_edge(users[0], p0, Some("Posted".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(users[0], p1, Some("Posted".into()), vec![], 1.0, 2)
            .unwrap();
        // user 1 and user 2 like p0; user 3 likes p1
        g.create_edge(users[1], p0, Some("Liked".into()), vec![], 1.0, 10)
            .unwrap();
        g.create_edge(users[2], p0, Some("Liked".into()), vec![], 1.0, 11)
            .unwrap();
        g.create_edge(users[3], p1, Some("Liked".into()), vec![], 1.0, 12)
            .unwrap();

        let q = parse_query(
            "MATCH (u:User {id: 0})-[:Posted]->(p:Post) \
             OPTIONAL MATCH (liker:User)-[:Liked]->(p) \
             RETURN u.id, COUNT(DISTINCT p) AS posts, COUNT(DISTINCT liker) AS likers",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int64(0));
        assert_eq!(r.rows[0][1], Value::Int64(2)); // 2 distinct posts
        assert_eq!(r.rows[0][2], Value::Int64(3)); // 3 distinct likers
    }

    /// Multi-chain (2-hop) reverse anchor: content attribution pattern.
    /// OPTIONAL MATCH (author:User)-[:Posted]->(page:Page)-[:Contains]->(p)
    /// where `p` is bound from first MATCH.
    #[test]
    fn reverse_chain_anchor_multi_chain_2hop() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        // Many :User vertices (start candidates)
        let mut users = Vec::new();
        for i in 0..15 {
            users.push(
                g.create_vertex(vec!["User".into()], vec![("id".into(), Value::Int64(i))])
                    .unwrap(),
            );
        }
        // Pages and posts
        let page0 = g.create_vertex(vec!["Page".into()], vec![]).unwrap();
        let page1 = g.create_vertex(vec!["Page".into()], vec![]).unwrap();
        let post0 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();
        let post1 = g.create_vertex(vec!["Post".into()], vec![]).unwrap();
        let anchor = g
            .create_vertex(vec!["Root".into()], vec![("id".into(), Value::Int64(99))])
            .unwrap();
        // anchor -[:R]-> post0, anchor -[:R]-> post1
        g.create_edge(anchor, post0, Some("R".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(anchor, post1, Some("R".into()), vec![], 1.0, 2)
            .unwrap();
        // page0 -[:Contains]-> post0, page1 -[:Contains]-> post1
        g.create_edge(page0, post0, Some("Contains".into()), vec![], 1.0, 10)
            .unwrap();
        g.create_edge(page1, post1, Some("Contains".into()), vec![], 1.0, 11)
            .unwrap();
        // users[0] -[:Posted]-> page0, users[1] -[:Posted]-> page1, users[2] -[:Posted]-> page0
        g.create_edge(users[0], page0, Some("Posted".into()), vec![], 1.0, 20)
            .unwrap();
        g.create_edge(users[1], page1, Some("Posted".into()), vec![], 1.0, 21)
            .unwrap();
        g.create_edge(users[2], page0, Some("Posted".into()), vec![], 1.0, 22)
            .unwrap();

        let q = parse_query(
            "MATCH (root:Root {id: 99})-[:R]->(p:Post) \
             OPTIONAL MATCH (author:User)-[:Posted]->(page:Page)-[:Contains]->(p) \
             RETURN id(p), id(author), id(page) ORDER BY id(p), id(author)",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        // post0: authors = users[0] via page0, users[2] via page0 → 2 rows
        // post1: authors = users[1] via page1 → 1 row
        assert_eq!(r.rows.len(), 3);
        // post0 rows
        assert_eq!(r.rows[0][0], Value::Int64(post0 as i64));
        assert_eq!(r.rows[0][1], Value::Int64(users[0] as i64));
        assert_eq!(r.rows[0][2], Value::Int64(page0 as i64));
        assert_eq!(r.rows[1][0], Value::Int64(post0 as i64));
        assert_eq!(r.rows[1][1], Value::Int64(users[2] as i64));
        assert_eq!(r.rows[1][2], Value::Int64(page0 as i64));
        // post1 row
        assert_eq!(r.rows[2][0], Value::Int64(post1 as i64));
        assert_eq!(r.rows[2][1], Value::Int64(users[1] as i64));
        assert_eq!(r.rows[2][2], Value::Int64(page1 as i64));
    }

    /// Multi-chain reverse anchor with an intermediate variable bound in seed.
    /// Ensures the bound-variable consistency check works correctly.
    #[test]
    fn reverse_chain_anchor_multi_chain_bound_intermediate() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let mut users = Vec::new();
        for _ in 0..10 {
            users.push(g.create_vertex(vec!["User".into()], vec![]).unwrap());
        }
        let group = g.create_vertex(vec!["Group".into()], vec![]).unwrap();
        let res = g.create_vertex(vec!["Resource".into()], vec![]).unwrap();
        let anchor = g
            .create_vertex(vec!["Root".into()], vec![("id".into(), Value::Int64(0))])
            .unwrap();
        // anchor -[:R]-> group, anchor -[:R]-> res
        g.create_edge(anchor, group, Some("Ref".into()), vec![], 1.0, 1)
            .unwrap();
        g.create_edge(anchor, res, Some("Ref".into()), vec![], 1.0, 2)
            .unwrap();
        // group -[:HasAccess]-> res
        g.create_edge(group, res, Some("HasAccess".into()), vec![], 1.0, 10)
            .unwrap();
        // users[0] -[:MemberOf]-> group, users[1] -[:MemberOf]-> group
        g.create_edge(users[0], group, Some("MemberOf".into()), vec![], 1.0, 20)
            .unwrap();
        g.create_edge(users[1], group, Some("MemberOf".into()), vec![], 1.0, 21)
            .unwrap();

        // First MATCH binds both `g` and `r`.  The OPTIONAL MATCH re-uses both
        // and adds `user`.  The intermediate variable `g` is already bound.
        let q = parse_query(
            "MATCH (root:Root {id: 0})-[:Ref]->(g:Group)-[:HasAccess]->(r:Resource) \
             OPTIONAL MATCH (user:User)-[:MemberOf]->(g)-[:HasAccess]->(r) \
             RETURN id(g), id(r), id(user) ORDER BY id(user)",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][0], Value::Int64(group as i64));
        assert_eq!(r.rows[0][1], Value::Int64(res as i64));
        assert_eq!(r.rows[0][2], Value::Int64(users[0] as i64));
        assert_eq!(r.rows[1][0], Value::Int64(group as i64));
        assert_eq!(r.rows[1][1], Value::Int64(res as i64));
        assert_eq!(r.rows[1][2], Value::Int64(users[1] as i64));
    }

    /// Multi-chain reverse anchor: 3-hop chain with mixed directions.
    #[test]
    fn reverse_chain_anchor_multi_chain_3hop_mixed_directions() {
        let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        let mut sources = Vec::new();
        for _ in 0..8 {
            sources.push(g.create_vertex(vec!["S".into()], vec![]).unwrap());
        }
        let mid1 = g.create_vertex(vec!["M1".into()], vec![]).unwrap();
        let mid2 = g.create_vertex(vec!["M2".into()], vec![]).unwrap();
        let target = g.create_vertex(vec!["T".into()], vec![]).unwrap();
        let anchor = g
            .create_vertex(vec!["Root".into()], vec![("id".into(), Value::Int64(0))])
            .unwrap();
        g.create_edge(anchor, target, Some("R".into()), vec![], 1.0, 1)
            .unwrap();
        // Chain: (s:S)-[:A]->(mid1:M1)<-[:B]-(mid2:M2)-[:C]->(target:T)
        //   chain[0]: [:A] Outgoing, node (mid1)
        //   chain[1]: [:B] Incoming, node (mid2)   — mid2 ←[:B]← mid1 means PMA: mid1→mid2? No.
        // Let me think carefully.
        // Pattern: (s)-[:A]->(mid1)<-[:B]-(mid2)-[:C]->(target)
        //   chain[0]: edge=[:A] dir=Outgoing, node=mid1
        //   chain[1]: edge=[:B] dir=Incoming, node=mid2
        //     Incoming means: mid1 is traversed via incoming edge → mid2 → mid1 in PMA
        //     Actually: from mid1, Incoming direction → reverse_neighbors_rich(mid1) → edges X→mid1
        //     next_vertex = X = mid2.  PMA: mid2→mid1.  Binding: src=mid2, dst=mid1.
        //   chain[2]: edge=[:C] dir=Outgoing, node=target
        //     From mid2, Outgoing → collect_neighbors(mid2) → mid2→target
        //
        // So edges needed:
        // sources[0] -[:A]-> mid1     (chain[0] forward)
        // mid2 -[:B]-> mid1           (chain[1] forward: Incoming from mid1's perspective)
        // mid2 -[:C]-> target         (chain[2] forward)
        g.create_edge(sources[0], mid1, Some("A".into()), vec![], 1.0, 10)
            .unwrap();
        g.create_edge(sources[1], mid1, Some("A".into()), vec![], 1.0, 11)
            .unwrap();
        g.create_edge(mid2, mid1, Some("B".into()), vec![], 1.0, 20)
            .unwrap();
        g.create_edge(mid2, target, Some("C".into()), vec![], 1.0, 30)
            .unwrap();

        let q = parse_query(
            "MATCH (root:Root {id: 0})-[:R]->(t:T) \
             OPTIONAL MATCH (s:S)-[:A]->(m1:M1)<-[:B]-(m2:M2)-[:C]->(t) \
             RETURN id(s), id(m1), id(m2), id(t) ORDER BY id(s)",
        );
        let r = execute_query(&q, &g, ExecutionLimits::default(), &RandomState::new()).unwrap();
        // Two source vertices (sources[0], sources[1]) can reach target via the chain
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][0], Value::Int64(sources[0] as i64));
        assert_eq!(r.rows[0][1], Value::Int64(mid1 as i64));
        assert_eq!(r.rows[0][2], Value::Int64(mid2 as i64));
        assert_eq!(r.rows[0][3], Value::Int64(target as i64));
        assert_eq!(r.rows[1][0], Value::Int64(sources[1] as i64));
        assert_eq!(r.rows[1][1], Value::Int64(mid1 as i64));
        assert_eq!(r.rows[1][2], Value::Int64(mid2 as i64));
        assert_eq!(r.rows[1][3], Value::Int64(target as i64));
    }

    fn parse_query(gql: &str) -> QueryStmt {
        let stmt = parse_statement(gql).unwrap();
        validate_statement(&stmt).unwrap_or_else(|e| panic!("{gql}: {e}"));
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        q
    }

    #[test]
    fn caller_function_returns_injected_principal() {
        let g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        // Use anonymous principal (empty bytes slice = 0x04)
        let caller_val = Value::Principal(candid::Principal::anonymous());
        set_caller(caller_val.clone());
        let stmt = parse_statement("RETURN caller() AS c").unwrap();
        validate_statement(&stmt).unwrap();
        let result = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        clear_caller();
        assert_eq!(result.columns, vec!["c"]);
        assert_eq!(result.rows[0][0], caller_val);
    }

    #[test]
    fn caller_function_returns_null_when_not_set() {
        let g = PmaGraph::new(VecMemory::default(), 0).unwrap();
        clear_caller();
        let stmt = parse_statement("RETURN caller() AS c").unwrap();
        validate_statement(&stmt).unwrap();
        let result = execute_read_statement(&stmt, &g, ExecutionLimits::default()).unwrap();
        assert_eq!(result.rows[0][0], Value::Null);
    }
}
