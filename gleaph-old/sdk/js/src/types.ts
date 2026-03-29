import type { Principal } from "@icp-sdk/core/principal";

// ── Value types ──

/** An element in a graph path — either a node or an edge. */
export type PathElement =
	| { Node: number }
	| { Edge: { src: number; dst: number; label: string | undefined } };

/**
 * A GQL scalar or complex value returned from queries.
 *
 * Represented as a Candid variant — exactly one key is set per value.
 *
 * @example
 * ```ts
 * const v: Value = { Text: "hello" };
 * if ("Text" in v) console.log(v.Text);
 * ```
 */
export type Value =
	| { Null: null }
	| { Bool: boolean }
	| { Int8: number }
	| { Int16: number }
	| { Int32: number }
	| { Int64: bigint }
	| { Int128: bigint }
	| { Int256: string }
	| { Uint8: number }
	| { Uint16: number }
	| { Uint32: number }
	| { Uint64: bigint }
	| { Uint128: bigint }
	| { Uint256: string }
	| { Float32: number }
	| { Float64: number }
	| { Text: string }
	| { Timestamp: bigint }
	| { List: Value[] }
	| { Path: PathElement[] }
	| { Bytes: Uint8Array }
	| { Date: number }
	| { Time: bigint }
	| { DateTime: [bigint, number] }
	| { Duration: [number, bigint] }
	| { Principal: Principal }
	| { Decimal: string };

// ── Error ──

/**
 * Candid error variant returned by the graph canister.
 *
 * This is the raw variant type; for the thrown error class see
 * {@link import("./errors.js").GleaphError}.
 */
export type GleaphError =
	| { VertexNotFound: number }
	| { OutOfCapacity: null }
	| { InvalidHeader: null }
	| { Memory: string }
	| { Unsupported: string }
	| { ParseError: string }
	| { ValidationError: string }
	| { UnsupportedFeature: string }
	| { ExecutionError: string }
	| { BudgetExhausted: null }
	| { AlgorithmError: string };

// ── Graph stats ──

/** Basic graph statistics (vertex/edge counts, capacity, average degree). */
export interface GraphStats {
	num_vertices: bigint;
	num_edges: bigint;
	elem_capacity: bigint;
	segment_size: number;
	segment_count: number;
	avg_degree: number;
}

/** IC-certified graph statistics with certificate and witness for verification. */
export interface CertifiedGraphStats {
	data: GraphStats;
	certificate: Uint8Array;
	witness: Uint8Array;
}

// ── Query ──

/** Fine-grained execution metrics from the query engine. */
export interface QueryExecutionBreakdown {
	index_fast_path_attempted: boolean;
	index_fast_path_used: boolean;
	aggregate_fast_path_attempted: boolean;
	aggregate_fast_path_used: boolean;
	shortest_fast_path_attempted: boolean;
	shortest_fast_path_used: boolean;
	rows_after_match: bigint;
	rows_after_with: bigint;
	rows_before_projection: bigint;
	groups_formed: bigint;
	top_k_calls: bigint;
	full_sort_calls: bigint;
	limit_truncate_calls: bigint;
	selectivity_refresh_ran: boolean;
}

/** Execution statistics returned alongside query results. */
export interface QueryStats {
	scanned_vertices: bigint;
	scanned_edges: bigint;
	rows_emitted: bigint;
	execution_steps: bigint;
	breakdown: QueryExecutionBreakdown;
}

/** Result of a GQL query — columns, rows, and execution statistics. */
export interface QueryResult {
	columns: string[];
	rows: Value[][];
	stats: QueryStats;
	warnings: TypeDiagnostic[];
}

/**
 * Query result with an optional continuation token.
 *
 * If `continuation` is defined, more rows are available. Use
 * {@link import("./graph.js").GraphClient.queryAll | queryAll} for automatic
 * pagination, or call `query_continue` manually.
 */
export interface QueryResultWithContinuation {
	result: QueryResult;
	continuation: ContinuationToken | undefined;
}

/** Execution mode for read-only GQL requests. */
export type QueryMode = "query" | "explain";

/** Optional controls for read-only GQL requests. */
export interface QueryRequestOptions {
	/** `query` executes the statement; `explain` returns planner/semantic explain lines. */
	mode?: QueryMode;
}

// ── Mutation ──

/** Result of a GQL mutation (CREATE, DELETE, SET, REMOVE, MERGE). */
export interface MutationResult {
	affected_vertices: bigint;
	affected_edges: bigint;
	warnings: TypeDiagnostic[];
}

/**
 * Mutation result with an optional continuation token.
 *
 * Large DELETE operations may return a continuation token for resuming
 * via `mutate_continue`.
 */
export interface MutationResultWithContinuation {
	result: MutationResult;
	continuation: ContinuationToken | undefined;
}

/** BFS result with an optional continuation token for large traversals. */
export interface BfsResultWithContinuation {
	result: BfsResult;
	continuation: ContinuationToken | undefined;
}

/** PageRank result with an optional continuation token for large graphs. */
export interface PageRankResultWithContinuation {
	result: PageRankResult;
	continuation: ContinuationToken | undefined;
}

/** SSSP result with an optional continuation token for large graphs. */
export interface SsspResultWithContinuation {
	result: SsspResult;
	continuation: ContinuationToken | undefined;
}

// ── Continuation ──

/** The kind of algorithm or operation that produced a continuation token. */
export type AlgorithmKind = "Bfs" | "Sssp" | "PageRank" | "GqlQuery";

/** A fingerprint of the graph state at the time a continuation was created. */
export interface GraphFingerprint {
	num_vertices: bigint;
	num_edges: bigint;
	next_edge_id: number;
}

/**
 * Opaque token for resuming a query or algorithm across IC calls.
 *
 * Continuation tokens become invalid if the graph is mutated between calls
 * (the fingerprint is checked on resume).
 */
export interface ContinuationToken {
	kind: { [K in AlgorithmKind]?: null };
	data: Uint8Array;
	graph_fingerprint: GraphFingerprint;
}

// ── Algorithms ──

/** Optional timestamp range filter for algorithm queries. */
export interface TimestampRange {
	start: bigint | undefined;
	end: bigint | undefined;
}

/** Configuration for breadth-first search. */
export interface BfsConfig {
	/** Maximum traversal depth. */
	max_depth?: number;
	/** Maximum number of vertices to visit before stopping. */
	max_visited?: bigint;
	/** If set, stop when this vertex is reached and return the path. */
	target?: number;
	/** Only traverse edges with this label. */
	edge_label?: string;
	/** Only traverse edges within this timestamp range. */
	ts_range?: TimestampRange;
}

/** Result of a breadth-first search. */
export interface BfsResult {
	/** All visited vertex IDs. */
	visited: number[];
	/** Pairs of (vertex_id, distance_from_start). */
	distances: [number, number][];
	/** Shortest path to the target vertex, if one was specified and found. */
	path: number[] | undefined;
}

/** Configuration for PageRank computation. */
export interface PageRankConfig {
	/** Damping factor (typically 0.85). */
	damping: number;
	/** Maximum number of iterations. */
	max_iterations: number;
	/** Stop when score changes fall below this threshold. */
	convergence_threshold: number;
	/** Only consider edges within this timestamp range. */
	ts_range?: TimestampRange;
}

/** Result of a PageRank computation. */
export interface PageRankResult {
	/** Pairs of (vertex_id, pagerank_score). */
	scores: [number, number][];
	/** Number of iterations performed. */
	iterations: number;
	/** Whether the algorithm converged within the threshold. */
	converged: boolean;
}

/** IC-certified PageRank result with certificate and witness. */
export interface CertifiedPageRank {
	data: PageRankResult;
	certificate: Uint8Array;
	witness: Uint8Array;
}

/** Configuration for single-source shortest path (Dijkstra). */
export interface SsspConfig {
	/** Maximum distance to explore. */
	max_distance?: number;
	/** Maximum number of vertices to visit. */
	max_visited?: bigint;
	/** If set, stop when this vertex is reached. */
	target?: number;
	/** Only traverse edges with this label. */
	edge_label?: string;
	/** Only traverse edges within this timestamp range. */
	ts_range?: TimestampRange;
}

/** Result of single-source shortest path. */
export interface SsspResult {
	/** Pairs of (vertex_id, shortest_distance). */
	distances: [number, number][];
	/** Pairs of (vertex_id, predecessor_vertex_id). */
	predecessors: [number, number | undefined][];
}

/** Configuration for collaborative filtering recommendations. */
export interface RecommendConfig {
	/** Edge label to follow (e.g. "PURCHASED", "LIKED"). */
	edge_label: string;
	/** Maximum hops from the source user. */
	max_hops: number;
	/** Maximum number of recommendations to return. */
	limit: number;
	/** Only consider edges within this timestamp range. */
	ts_range?: TimestampRange;
	/** Exclude items already connected to the source user. */
	exclude_known: boolean;
}

/** A single recommendation result. */
export interface Recommendation {
	/** The recommended vertex ID. */
	vertex_id: number;
	/** Relevance score (higher is better). */
	score: number;
	/** The path from the source user to the recommended vertex. */
	path: number[];
}

// ── Edge / Vertex ──

/** Edge information returned from {@link import("./graph.js").GraphClient.getNeighbors | getNeighbors}. */
export interface EdgeInfo {
	target: number;
	weight: number;
	timestamp: bigint;
}

/** Vertex data for insertion via {@link import("./graph.js").GraphClient.addVertex | addVertex}. */
export interface VertexData {
	id: number;
}

/** Edge data for insertion via {@link import("./graph.js").GraphClient.addEdge | addEdge}. */
export interface EdgeData {
	src: number;
	dst: number;
	weight: number;
	timestamp: bigint;
}

// ── Index ──

/** Entity type for index creation: `{ Vertex: null }` or `{ Edge: null }`. */
export type EntityType = { Vertex: null } | { Edge: null };

/** Index type for index creation. Currently only `{ Equality: null }`. */
export type IndexType = { Equality: null };

// ── Planner ──

/** Statistics used by the query planner for cost-based optimization. */
export interface PlannerStats {
	/** Pairs of (label, count) showing how many vertices/edges have each label. */
	label_cardinality: [string, bigint][];
	/** Average vertex degree across the graph. */
	avg_degree: number;
	/** Pairs of (property_name, selectivity) for cost estimation. */
	property_selectivity: [string, number][];
	/** Property names that have an equality index on vertices. */
	indexed_vertex_properties: string[];
	/** Total vertex count. */
	vertex_count: bigint;
	/** Total edge count. */
	edge_count: bigint;
}

// ── Registry ──

/** Configuration for creating a new graph canister via the registry. */
export interface GraphConfig {
	/** Human-readable name for the graph. */
	name: string;
	/** Maximum number of vertices the graph can hold. */
	max_vertices: number;
	/** Initial edge storage capacity. */
	initial_edge_capacity: bigint;
}

/** Information about a graph managed by the registry. */
export interface GraphInfo {
	/** Unique graph identifier assigned by the registry. */
	id: bigint;
	/** Human-readable graph name. */
	name: string;
	/** Canister principal of the deployed graph (undefined if not yet deployed). */
	canister_id: Principal | undefined;
	/** Principal of the graph owner. */
	owner: Principal;
	/** Maximum vertex capacity. */
	max_vertices: number;
}

/** Access control level for a principal on a graph. */
export type AccessLevel =
	| { Execute: null }
	| { Read: null }
	| { Write: null }
	| { Admin: null };

// ── Prepared Statements ──

/** Whether a prepared statement is a read-only query or a mutation. */
export type PreparedKind = { Query: null } | { Mutation: null };

/** Dynamic sort options declared on a prepared statement. */
export interface PreparedOptions {
	/** Optional human-written description used in generated API docs. */
	description?: string;
	/** Allowed externally visible sort keys for this prepared statement. */
	allowed_sorts: PreparedSortKey[];
	/** Default sort when executePrepared() omits the sort argument. */
	default_sort?: PreparedSortSpec[];
}

/** A prepared dynamic sort key exposed to callers. */
export interface PreparedSortKey {
	/** Stable key identifier used by clients, e.g. "age". */
	key: string;
	/** GQL expression associated with the key, e.g. "u.age". */
	expr: string;
}

/** A concrete sort request for executing a prepared query. */
export interface PreparedSortSpec {
	/** One of PreparedOptions.allowed_sorts[].key. */
	key: string;
	/** true = DESC, false = ASC. */
	descending: boolean;
	/** undefined = engine default, true = NULLS FIRST, false = NULLS LAST. */
	nulls_first?: boolean;
}

/** Scalar element type for typed lists in prepared statement metadata. */
export type PreparedScalarType =
	| "Int8"
	| "Int16"
	| "Int32"
	| "Int64"
	| "Int128"
	| "Int256"
	| "Uint8"
	| "Uint16"
	| "Uint32"
	| "Uint64"
	| "Uint128"
	| "Uint256"
	| "Float32"
	| "Float64"
	| "Text"
	| "Bool"
	| "Timestamp"
	| "Bytes"
	| "Date"
	| "Time"
	| "DateTime"
	| "Duration"
	| "Principal"
	| "Decimal";

/** Scalar value type exposed in prepared statement metadata. */
export type PreparedValueType =
	| "Int8"
	| "Int16"
	| "Int32"
	| "Int64"
	| "Int128"
	| "Int256"
	| "Uint8"
	| "Uint16"
	| "Uint32"
	| "Uint64"
	| "Uint128"
	| "Uint256"
	| "Float32"
	| "Float64"
	| "Text"
	| "Bool"
	| "Timestamp"
	| "List"
	| { TypedList: PreparedScalarType }
	| "Null"
	| "Bytes"
	| "Date"
	| "Time"
	| "DateTime"
	| "Duration"
	| "Principal"
	| "Decimal";

/** Metadata about a prepared statement parameter. */
export interface PreparedParameterInfo {
	/** Wire parameter name without the leading `$`. */
	name: string;
	/** Whether executePrepared requires this parameter to be present. */
	required: boolean;
	/** Inferred or annotated value types (empty = unknown). */
	types: PreparedValueType[];
	/** True when types were derived from reverse inference, not explicit annotation. */
	inferred: boolean;
}

/** Metadata about a prepared statement. */
export interface PreparedStatementInfo {
	/** Unique name of the prepared statement. */
	name: string;
	/** Whether this is a query or mutation. */
	kind: PreparedKind;
	/** Structured parameter metadata for the statement. */
	parameters: PreparedParameterInfo[];
	/** Column names from the RETURN clause (empty for mutations). */
	columns: string[];
	/** Whether the statement uses the `caller()` built-in function. */
	requires_caller: boolean;
	/** Original GQL source text. */
	source: string;
	/** Optional human-written description used in generated API docs. */
	description?: string;
	/** Allowed dynamic sort keys for prepared queries. */
	allowed_sorts: PreparedSortKey[];
	/** Default sort applied when executePrepared() omits sort. */
	default_sort?: PreparedSortSpec[];
	/** Schema-aware static type-check warnings captured at prepare time. */
	type_warnings: TypeDiagnostic[];
}

export type TypeDiagnosticKind =
	| "Info"
	| "BinaryOpMismatch"
	| "NonBooleanCondition"
	| "FunctionArgMismatch"
	| "ComparisonMismatch"
	| "NullCheckOnNonNull"
	| "ImpossiblePattern"
	| "GroupingViolation"
	| "ParameterInferenceConflict";

export interface TypeDiagnostic {
	kind: TypeDiagnosticKind;
	message: string;
}

// ── Property map (for parameterized queries) ──

/**
 * A list of (name, value) pairs for parameterized GQL queries.
 *
 * Use {@link import("./values.js").toPropertyMap | toPropertyMap} to convert
 * a plain JS object into this format.
 */
export type PropertyMap = [string, Value][];
