import { IDL } from "@icp-sdk/core/candid";

// ── Shared types ──

const PathElement = IDL.Variant({
	Node: IDL.Nat32,
	Edge: IDL.Record({
		src: IDL.Nat32,
		dst: IDL.Nat32,
		label: IDL.Opt(IDL.Text),
	}),
});

const Value: IDL.Type = IDL.Rec();
const ValueVariant = IDL.Variant({
	Null: IDL.Null,
	Bool: IDL.Bool,
	Int8: IDL.Int8,
	Int16: IDL.Int16,
	Int32: IDL.Int32,
	Int64: IDL.Int64,
	Int128: IDL.Int,
	Int256: IDL.Text,
	Uint8: IDL.Nat8,
	Uint16: IDL.Nat16,
	Uint32: IDL.Nat32,
	Uint64: IDL.Nat64,
	Uint128: IDL.Nat,
	Uint256: IDL.Text,
	Float32: IDL.Float32,
	Float64: IDL.Float64,
	Text: IDL.Text,
	Timestamp: IDL.Nat64,
	List: IDL.Vec(Value),
	Path: IDL.Vec(PathElement),
	Bytes: IDL.Vec(IDL.Nat8),
	Date: IDL.Int32,
	Time: IDL.Nat64,
	DateTime: IDL.Tuple(IDL.Int64, IDL.Nat32),
	Duration: IDL.Tuple(IDL.Int32, IDL.Int64),
	Principal: IDL.Principal,
	Decimal: IDL.Text,
});
(Value as unknown as { fill: (t: IDL.Type) => void }).fill(ValueVariant);

const GleaphError = IDL.Variant({
	VertexNotFound: IDL.Nat32,
	OutOfCapacity: IDL.Null,
	InvalidHeader: IDL.Null,
	Memory: IDL.Text,
	Unsupported: IDL.Text,
	ParseError: IDL.Text,
	ValidationError: IDL.Text,
	UnsupportedFeature: IDL.Text,
	ExecutionError: IDL.Text,
	BudgetExhausted: IDL.Null,
	AlgorithmError: IDL.Text,
});

const TimestampRange = IDL.Record({
	start: IDL.Opt(IDL.Nat64),
	end: IDL.Opt(IDL.Nat64),
});

const AlgorithmKind = IDL.Variant({
	Bfs: IDL.Null,
	Sssp: IDL.Null,
	PageRank: IDL.Null,
	GqlQuery: IDL.Null,
});

const GraphFingerprint = IDL.Record({
	num_vertices: IDL.Nat64,
	num_edges: IDL.Nat64,
	next_edge_id: IDL.Nat32,
});

const ContinuationToken = IDL.Record({
	kind: AlgorithmKind,
	data: IDL.Vec(IDL.Nat8),
	graph_fingerprint: GraphFingerprint,
});

// ── Query types ──

const QueryExecutionBreakdown = IDL.Record({
	index_fast_path_attempted: IDL.Bool,
	index_fast_path_used: IDL.Bool,
	aggregate_fast_path_attempted: IDL.Bool,
	aggregate_fast_path_used: IDL.Bool,
	shortest_fast_path_attempted: IDL.Bool,
	shortest_fast_path_used: IDL.Bool,
	rows_after_match: IDL.Nat64,
	rows_after_with: IDL.Nat64,
	rows_before_projection: IDL.Nat64,
	groups_formed: IDL.Nat64,
	top_k_calls: IDL.Nat64,
	full_sort_calls: IDL.Nat64,
	limit_truncate_calls: IDL.Nat64,
	selectivity_refresh_ran: IDL.Bool,
});

const QueryStats = IDL.Record({
	scanned_vertices: IDL.Nat64,
	scanned_edges: IDL.Nat64,
	rows_emitted: IDL.Nat64,
	execution_steps: IDL.Nat64,
	breakdown: QueryExecutionBreakdown,
});

const TypeDiagnosticKind = IDL.Variant({
	Info: IDL.Null,
	BinaryOpMismatch: IDL.Null,
	NonBooleanCondition: IDL.Null,
	FunctionArgMismatch: IDL.Null,
	ComparisonMismatch: IDL.Null,
	NullCheckOnNonNull: IDL.Null,
	ImpossiblePattern: IDL.Null,
	GroupingViolation: IDL.Null,
	ParameterInferenceConflict: IDL.Null,
});

const TypeDiagnostic = IDL.Record({
	kind: TypeDiagnosticKind,
	message: IDL.Text,
});

const QueryResultRecord = IDL.Record({
	columns: IDL.Vec(IDL.Text),
	rows: IDL.Vec(IDL.Vec(Value)),
	stats: QueryStats,
	warnings: IDL.Vec(IDL.Record({
		kind: IDL.Variant({
			Info: IDL.Null,
			BinaryOpMismatch: IDL.Null,
			NonBooleanCondition: IDL.Null,
			FunctionArgMismatch: IDL.Null,
			ComparisonMismatch: IDL.Null,
			NullCheckOnNonNull: IDL.Null,
			ImpossiblePattern: IDL.Null,
			GroupingViolation: IDL.Null,
			ParameterInferenceConflict: IDL.Null,
		}),
		message: IDL.Text,
	})),
});

const QueryResultWithContinuation = IDL.Record({
	result: QueryResultRecord,
	continuation: IDL.Opt(ContinuationToken),
});

const MutationResultRecord = IDL.Record({
	affected_vertices: IDL.Nat64,
	affected_edges: IDL.Nat64,
	warnings: IDL.Vec(TypeDiagnostic),
});

const MutationResultWithContinuation = IDL.Record({
	result: MutationResultRecord,
	continuation: IDL.Opt(ContinuationToken),
});

// ── Stats types ──

const GraphStats = IDL.Record({
	num_vertices: IDL.Nat64,
	num_edges: IDL.Nat64,
	elem_capacity: IDL.Nat64,
	segment_size: IDL.Nat32,
	segment_count: IDL.Nat32,
	avg_degree: IDL.Float64,
});

const CertifiedGraphStats = IDL.Record({
	data: GraphStats,
	certificate: IDL.Vec(IDL.Nat8),
	witness: IDL.Vec(IDL.Nat8),
});

const PlannerStats = IDL.Record({
	label_cardinality: IDL.Vec(IDL.Tuple(IDL.Text, IDL.Nat64)),
	avg_degree: IDL.Float64,
	property_selectivity: IDL.Vec(IDL.Tuple(IDL.Text, IDL.Float64)),
	indexed_vertex_properties: IDL.Vec(IDL.Text),
	range_indexed_vertex_properties: IDL.Vec(IDL.Text),
	vertex_count: IDL.Nat64,
	edge_count: IDL.Nat64,
});

// ── Algorithm types ──

const BfsConfig = IDL.Record({
	max_depth: IDL.Opt(IDL.Nat32),
	max_visited: IDL.Opt(IDL.Nat64),
	target: IDL.Opt(IDL.Nat32),
	edge_label: IDL.Opt(IDL.Text),
	ts_range: IDL.Opt(TimestampRange),
});

const BfsResult = IDL.Record({
	visited: IDL.Vec(IDL.Nat32),
	distances: IDL.Vec(IDL.Tuple(IDL.Nat32, IDL.Nat32)),
	path: IDL.Opt(IDL.Vec(IDL.Nat32)),
});

const BfsResultWithContinuation = IDL.Record({
	result: BfsResult,
	continuation: IDL.Opt(ContinuationToken),
});

const PageRankConfig = IDL.Record({
	damping: IDL.Float64,
	max_iterations: IDL.Nat32,
	convergence_threshold: IDL.Float64,
	ts_range: IDL.Opt(TimestampRange),
});

const PageRankResult = IDL.Record({
	scores: IDL.Vec(IDL.Tuple(IDL.Nat32, IDL.Float64)),
	iterations: IDL.Nat32,
	converged: IDL.Bool,
});

const PageRankResultWithContinuation = IDL.Record({
	result: PageRankResult,
	continuation: IDL.Opt(ContinuationToken),
});

const CertifiedPageRank = IDL.Record({
	data: PageRankResult,
	certificate: IDL.Vec(IDL.Nat8),
	witness: IDL.Vec(IDL.Nat8),
});

const SsspConfig = IDL.Record({
	max_distance: IDL.Opt(IDL.Float64),
	max_visited: IDL.Opt(IDL.Nat64),
	target: IDL.Opt(IDL.Nat32),
	edge_label: IDL.Opt(IDL.Text),
	ts_range: IDL.Opt(TimestampRange),
});

const SsspResult = IDL.Record({
	distances: IDL.Vec(IDL.Tuple(IDL.Nat32, IDL.Float64)),
	predecessors: IDL.Vec(IDL.Tuple(IDL.Nat32, IDL.Opt(IDL.Nat32))),
});

const SsspResultWithContinuation = IDL.Record({
	result: SsspResult,
	continuation: IDL.Opt(ContinuationToken),
});

const RecommendConfig = IDL.Record({
	edge_label: IDL.Text,
	max_hops: IDL.Nat8,
	limit: IDL.Nat32,
	ts_range: IDL.Opt(TimestampRange),
	exclude_known: IDL.Bool,
});

const Recommendation = IDL.Record({
	vertex_id: IDL.Nat32,
	score: IDL.Float64,
	path: IDL.Vec(IDL.Nat32),
});

// ── Entity / Index ──

const EntityType = IDL.Variant({ Vertex: IDL.Null, Edge: IDL.Null });
const IndexType = IDL.Variant({ Equality: IDL.Null });

const EdgeInfo = IDL.Record({
	target: IDL.Nat32,
	weight: IDL.Float32,
	timestamp: IDL.Nat64,
});

const VertexData = IDL.Record({ id: IDL.Nat32 });

const EdgeData = IDL.Record({
	src: IDL.Nat32,
	dst: IDL.Nat32,
	weight: IDL.Float32,
	timestamp: IDL.Nat64,
});

const PreparedKind = IDL.Variant({ Query: IDL.Null, Mutation: IDL.Null });

const PreparedSortKey = IDL.Record({
	key: IDL.Text,
	expr: IDL.Text,
});

const PreparedSortSpec = IDL.Record({
	key: IDL.Text,
	descending: IDL.Bool,
	nulls_first: IDL.Opt(IDL.Bool),
});

const PreparedScalarType = IDL.Variant({
	Int8: IDL.Null,
	Int16: IDL.Null,
	Int32: IDL.Null,
	Int64: IDL.Null,
	Int128: IDL.Null,
	Int256: IDL.Null,
	Uint8: IDL.Null,
	Uint16: IDL.Null,
	Uint32: IDL.Null,
	Uint64: IDL.Null,
	Uint128: IDL.Null,
	Uint256: IDL.Null,
	Float32: IDL.Null,
	Float64: IDL.Null,
	Text: IDL.Null,
	Bool: IDL.Null,
	Timestamp: IDL.Null,
	Bytes: IDL.Null,
	Date: IDL.Null,
	Time: IDL.Null,
	DateTime: IDL.Null,
	Duration: IDL.Null,
	Principal: IDL.Null,
	Decimal: IDL.Null,
});

const PreparedValueType = IDL.Variant({
	Int8: IDL.Null,
	Int16: IDL.Null,
	Int32: IDL.Null,
	Int64: IDL.Null,
	Int128: IDL.Null,
	Int256: IDL.Null,
	Uint8: IDL.Null,
	Uint16: IDL.Null,
	Uint32: IDL.Null,
	Uint64: IDL.Null,
	Uint128: IDL.Null,
	Uint256: IDL.Null,
	Float32: IDL.Null,
	Float64: IDL.Null,
	Text: IDL.Null,
	Bool: IDL.Null,
	Timestamp: IDL.Null,
	List: IDL.Null,
	TypedList: PreparedScalarType,
	Null: IDL.Null,
	Bytes: IDL.Null,
	Date: IDL.Null,
	Time: IDL.Null,
	DateTime: IDL.Null,
	Duration: IDL.Null,
	Principal: IDL.Null,
	Decimal: IDL.Null,
});

const PreparedParameterInfo = IDL.Record({
	name: IDL.Text,
	required: IDL.Bool,
	types: IDL.Vec(PreparedValueType),
	inferred: IDL.Bool,
});

const PreparedOptions = IDL.Record({
	description: IDL.Opt(IDL.Text),
	allowed_sorts: IDL.Vec(PreparedSortKey),
	default_sort: IDL.Opt(IDL.Vec(PreparedSortSpec)),
});

const PreparedStatementInfo = IDL.Record({
	name: IDL.Text,
	kind: PreparedKind,
	parameters: IDL.Vec(PreparedParameterInfo),
	columns: IDL.Vec(IDL.Text),
	requires_caller: IDL.Bool,
	source: IDL.Text,
	description: IDL.Opt(IDL.Text),
	allowed_sorts: IDL.Vec(PreparedSortKey),
	default_sort: IDL.Opt(IDL.Vec(PreparedSortSpec)),
	type_warnings: IDL.Vec(TypeDiagnostic),
});

const PropertyMapEntry = IDL.Tuple(IDL.Text, Value);

// ── Graph canister IDL factory ──

export const graphIdlFactory: IDL.InterfaceFactory = ({ IDL: _IDL }) => {
	return IDL.Service({
		get_neighbors: IDL.Func([IDL.Nat32], [IDL.Vec(EdgeInfo)], ["query"]),
		get_stats: IDL.Func([], [GraphStats], ["query"]),
		get_stats_certified: IDL.Func([], [CertifiedGraphStats], ["query"]),
		get_planner_stats: IDL.Func([], [PlannerStats], ["query"]),

		query: IDL.Func(
			[IDL.Text, IDL.Opt(IDL.Vec(PropertyMapEntry))],
			[IDL.Variant({ Ok: QueryResultWithContinuation, Err: GleaphError })],
			["query"],
		),
		explain: IDL.Func(
			[IDL.Text],
			[IDL.Variant({ Ok: QueryResultRecord, Err: GleaphError })],
			["query"],
		),

		mutate: IDL.Func(
			[IDL.Text, IDL.Opt(IDL.Vec(PropertyMapEntry))],
			[IDL.Variant({ Ok: MutationResultWithContinuation, Err: GleaphError })],
			[],
		),
		batch_mutate: IDL.Func(
			[IDL.Vec(IDL.Tuple(IDL.Text, IDL.Opt(IDL.Vec(PropertyMapEntry))))],
			[IDL.Vec(IDL.Variant({ Ok: MutationResultRecord, Err: GleaphError }))],
			[],
		),

		bfs: IDL.Func(
			[IDL.Nat32, BfsConfig],
			[IDL.Variant({ Ok: BfsResultWithContinuation, Err: GleaphError })],
			["query"],
		),
		recommend: IDL.Func(
			[IDL.Nat32, RecommendConfig],
			[IDL.Variant({ Ok: IDL.Vec(Recommendation), Err: GleaphError })],
			["query"],
		),
		compute_pagerank: IDL.Func(
			[PageRankConfig],
			[IDL.Variant({ Ok: PageRankResultWithContinuation, Err: GleaphError })],
			[],
		),
		compute_sssp: IDL.Func(
			[IDL.Nat32, SsspConfig],
			[IDL.Variant({ Ok: SsspResultWithContinuation, Err: GleaphError })],
			[],
		),
		get_pagerank_certified: IDL.Func(
			[IDL.Vec(IDL.Nat8)],
			[IDL.Variant({ Ok: CertifiedPageRank, Err: GleaphError })],
			["query"],
		),
		compute_graph_stats: IDL.Func([], [PlannerStats], []),

		query_continue: IDL.Func(
			[ContinuationToken],
			[IDL.Variant({ Ok: QueryResultWithContinuation, Err: GleaphError })],
			["query"],
		),
		mutate_continue: IDL.Func(
			[ContinuationToken],
			[IDL.Variant({ Ok: MutationResultWithContinuation, Err: GleaphError })],
			[],
		),

		add_vertex: IDL.Func(
			[VertexData],
			[IDL.Variant({ Ok: IDL.Nat64, Err: GleaphError })],
			[],
		),
		add_edge: IDL.Func(
			[EdgeData],
			[IDL.Variant({ Ok: IDL.Nat64, Err: GleaphError })],
			[],
		),
		bulk_insert_vertices: IDL.Func(
			[IDL.Vec(VertexData)],
			[IDL.Variant({ Ok: IDL.Nat64, Err: GleaphError })],
			[],
		),
		bulk_insert_edges: IDL.Func(
			[IDL.Vec(EdgeData)],
			[IDL.Variant({ Ok: IDL.Nat64, Err: GleaphError })],
			[],
		),

		create_index: IDL.Func(
			[EntityType, IDL.Text, IndexType],
			[IDL.Variant({ Ok: IDL.Null, Err: GleaphError })],
			[],
		),

		prepare: IDL.Func(
			[IDL.Text, IDL.Text, IDL.Opt(PreparedOptions)],
			[IDL.Variant({ Ok: PreparedStatementInfo, Err: GleaphError })],
			[],
		),
		execute_prepared: IDL.Func(
			[IDL.Text, IDL.Vec(PropertyMapEntry), IDL.Opt(IDL.Vec(PreparedSortSpec))],
			[IDL.Variant({ Ok: QueryResultWithContinuation, Err: GleaphError })],
			["query"],
		),
		execute_prepared_mutation: IDL.Func(
			[IDL.Text, IDL.Vec(PropertyMapEntry)],
			[IDL.Variant({ Ok: MutationResultRecord, Err: GleaphError })],
			[],
		),
		drop_prepared: IDL.Func(
			[IDL.Text],
			[IDL.Variant({ Ok: IDL.Bool, Err: GleaphError })],
			[],
		),
		list_prepared: IDL.Func(
			[],
			[
				IDL.Variant({
					Ok: IDL.Vec(PreparedStatementInfo),
					Err: GleaphError,
				}),
			],
			["query"],
		),
	});
};

// ── Registry canister IDL factory ──

const AccessLevel = IDL.Variant({
	Execute: IDL.Null,
	Read: IDL.Null,
	Write: IDL.Null,
	Admin: IDL.Null,
});

const GraphInfoRecord = IDL.Record({
	id: IDL.Nat64,
	name: IDL.Text,
	canister_id: IDL.Opt(IDL.Principal),
	owner: IDL.Principal,
	max_vertices: IDL.Nat32,
});

const GraphConfigRecord = IDL.Record({
	name: IDL.Text,
	max_vertices: IDL.Nat32,
	initial_edge_capacity: IDL.Nat64,
});

export const registryIdlFactory: IDL.InterfaceFactory = ({ IDL: _IDL }) => {
	return IDL.Service({
		create_graph: IDL.Func([GraphConfigRecord], [GraphInfoRecord], []),
		delete_graph: IDL.Func([IDL.Nat64], [IDL.Bool], []),
		list_graphs: IDL.Func([], [IDL.Vec(GraphInfoRecord)], ["query"]),
		grant_access: IDL.Func(
			[IDL.Nat64, IDL.Principal, AccessLevel],
			[IDL.Bool],
			[],
		),
	});
};
