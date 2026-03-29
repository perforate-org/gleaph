import type { ActorSubclass } from "@icp-sdk/core/agent";
import { unwrap } from "./errors.js";
import type {
	BfsConfig,
	BfsResult,
	BfsResultWithContinuation,
	CertifiedGraphStats,
	CertifiedPageRank,
	EdgeData,
	EdgeInfo,
	EntityType,
	GraphStats,
	IndexType,
	MutationResult,
	MutationResultWithContinuation,
	PageRankConfig,
	PageRankResult,
	PageRankResultWithContinuation,
	PlannerStats,
	PreparedOptions,
	PreparedSortSpec,
	PreparedStatementInfo,
	QueryRequestOptions,
	QueryResult,
	QueryResultWithContinuation,
	Recommendation,
	RecommendConfig,
	SsspConfig,
	SsspResult,
	SsspResultWithContinuation,
	VertexData,
} from "./types.js";
import { toPropertyMap } from "./values.js";

type GraphActor = ActorSubclass<
	Record<string, (...args: any[]) => Promise<any>>
>;

/**
 * Client for a single Gleaph graph canister.
 *
 * Provides typed methods for GQL queries/mutations, graph algorithms,
 * low-level vertex/edge operations, and index management.
 *
 * Obtain an instance via {@link import("./client.js").GleaphClient.graph | GleaphClient.graph()}.
 *
 * @example
 * ```ts
 * const graph = client.graph("bkyz2-fmaaa-aaaaa-qaaaq-cai");
 * const result = await graph.query("MATCH (n:User) RETURN n.name");
 * ```
 */
export class GraphClient {
	/** @internal */
	constructor(readonly actor: GraphActor) {}

	// ── GQL Query ──

	/**
	 * Execute a read-only GQL query.
	 *
	 * @param gql - GQL query string (e.g. `MATCH (n) RETURN n`).
	 * @param params - Optional query parameters substituted for `$name` placeholders.
	 * @param options - Optional request controls such as `{ mode: "explain" }`.
	 * @returns Query result with an optional continuation token for pagination.
	 * @throws {import("./errors.js").GleaphError} On parse, validation, or execution errors.
	 *
	 * @example
	 * ```ts
	 * const result = await graph.query(
	 *   "MATCH (u:User {name: $name}) RETURN u",
	 *   { name: "Alice" },
	 * );
	 * ```
	 */
	async query(
		gql: string,
		params?: Record<string, unknown>,
		options?: QueryRequestOptions,
	): Promise<QueryResultWithContinuation> {
		if (options?.mode === "explain") {
			const result = unwrap(await this.actor.explain(gql)) as QueryResult;
			return { result, continuation: undefined };
		}
		const raw = await this.actor.query(
			gql,
			params ? [toPropertyMap(params)] : [],
		);
		return unwrap(raw);
	}

	/**
	 * Return planner/semantic explain lines for a read-only GQL query.
	 *
	 * Equivalent to `graph.query(gql, undefined, { mode: "explain" })`, but
	 * exposed as a dedicated method for a simpler call site.
	 */
	async explain(gql: string): Promise<QueryResult> {
		return unwrap(await this.actor.explain(gql));
	}

	/**
	 * Execute a read-only GQL query, automatically paginating through all
	 * continuation pages and merging the rows into a single result.
	 *
	 * @param gql - GQL query string.
	 * @param params - Optional query parameters.
	 * @returns Merged query result containing all rows.
	 */
	async queryAll(
		gql: string,
		params?: Record<string, unknown>,
		options?: QueryRequestOptions,
	): Promise<QueryResult> {
		const first = await this.query(gql, params, options);
		if (options?.mode === "explain") {
			return first.result;
		}
		const allRows = [...first.result.rows];
		let continuation = first.continuation;

		while (continuation) {
			const next = unwrap(await this.actor.query_continue(continuation));
			const qr = next as QueryResultWithContinuation;
			allRows.push(...qr.result.rows);
			continuation = qr.continuation;
		}

		return {
			columns: first.result.columns,
			rows: allRows,
			stats: first.result.stats,
			warnings: first.result.warnings,
		};
	}

	// ── GQL Mutation ──

	/**
	 * Execute a GQL mutation (CREATE, DELETE, SET, REMOVE, MERGE).
	 *
	 * @param gql - GQL mutation string.
	 * @param params - Optional mutation parameters.
	 * @returns Mutation result with affected vertex/edge counts.
	 *
	 * @example
	 * ```ts
	 * await graph.mutate(
	 *   "CREATE (:User {name: $name, age: $age})",
	 *   { name: "Bob", age: 25 },
	 * );
	 * ```
	 */
	async mutate(
		gql: string,
		params?: Record<string, unknown>,
	): Promise<MutationResultWithContinuation> {
		const raw = await this.actor.mutate(
			gql,
			params ? [toPropertyMap(params)] : [],
		);
		return unwrap(raw);
	}

	/**
	 * Execute multiple GQL mutations in a single canister call.
	 *
	 * @param gqls - Array of GQL mutation strings or `[gql, params]` pairs.
	 * @returns Array of mutation results (one per statement).
	 * @throws {import("./errors.js").GleaphError} If any individual mutation fails.
	 */
	async batchMutate(
		gqls: (string | [string, Record<string, unknown>?])[],
	): Promise<MutationResult[]> {
		const entries = gqls.map((g) => {
			if (typeof g === "string") return [g, []] as [string, []];
			const [gql, params] = g;
			return [gql, params ? [toPropertyMap(params)] : []] as [
				string,
				[] | [[string, unknown][]],
			];
		});
		const results = await this.actor.batch_mutate(entries);
		return (results as { Ok: MutationResult; Err: unknown }[]).map(unwrap);
	}

	// ── Prepared Statements ──

	/**
	 * Register a prepared statement for repeated execution.
	 *
	 * @param name - Unique name for the prepared statement.
	 * @param gql - GQL statement to prepare.
	 * @returns Metadata about the prepared statement.
	 */
	async prepare(
		name: string,
		gql: string,
		options?: PreparedOptions,
	): Promise<PreparedStatementInfo> {
		return unwrap(await this.actor.prepare(name, gql, options ? [options] : []));
	}

	/**
	 * Execute a prepared read-only query.
	 *
	 * @param name - Name of a previously prepared statement.
	 * @param params - Optional parameters to bind.
	 */
	async executePrepared(
		name: string,
		params?: Record<string, unknown>,
		sort?: PreparedSortSpec[],
	): Promise<QueryResultWithContinuation> {
		const pm = params ? toPropertyMap(params) : [];
		return unwrap(await this.actor.execute_prepared(name, pm, sort ? [sort] : []));
	}

	/**
	 * Execute a prepared mutation.
	 *
	 * @param name - Name of a previously prepared statement.
	 * @param params - Optional parameters to bind.
	 */
	async executePreparedMutation(
		name: string,
		params?: Record<string, unknown>,
	): Promise<MutationResult> {
		const pm = params ? toPropertyMap(params) : [];
		return unwrap(await this.actor.execute_prepared_mutation(name, pm));
	}

	/**
	 * Remove a prepared statement.
	 *
	 * @param name - Name of the prepared statement to drop.
	 * @returns `true` if the statement existed and was dropped.
	 */
	async dropPrepared(name: string): Promise<boolean> {
		return unwrap(await this.actor.drop_prepared(name));
	}

	/**
	 * List all registered prepared statements.
	 *
	 * @returns Array of prepared statement metadata.
	 */
	async listPrepared(): Promise<PreparedStatementInfo[]> {
		return unwrap(await this.actor.list_prepared());
	}

	// ── Low-level Graph Ops ──

	/**
	 * Add a single vertex.
	 *
	 * @returns Total vertex count after insertion.
	 */
	async addVertex(vertex: VertexData): Promise<bigint> {
		return unwrap(await this.actor.add_vertex(vertex));
	}

	/**
	 * Add a single edge.
	 *
	 * @returns Total edge count after insertion.
	 */
	async addEdge(edge: EdgeData): Promise<bigint> {
		return unwrap(await this.actor.add_edge(edge));
	}

	/**
	 * Bulk insert multiple vertices.
	 *
	 * @returns Total vertex count after insertion.
	 */
	async bulkInsertVertices(vertices: VertexData[]): Promise<bigint> {
		return unwrap(await this.actor.bulk_insert_vertices(vertices));
	}

	/**
	 * Bulk insert multiple edges.
	 *
	 * @returns Total edge count after insertion.
	 */
	async bulkInsertEdges(edges: EdgeData[]): Promise<bigint> {
		return unwrap(await this.actor.bulk_insert_edges(edges));
	}

	// ── Algorithms ──

	/**
	 * Run breadth-first search from a starting vertex.
	 *
	 * @param start - Starting vertex ID.
	 * @param config - Optional BFS configuration (max depth, target, filters).
	 */
	async bfs(
		start: number,
		config: BfsConfig = {},
	): Promise<BfsResultWithContinuation> {
		return unwrap(await this.actor.bfs(start, toOptionalConfig(config)));
	}

	/**
	 * Compute PageRank scores for all vertices.
	 *
	 * @param config - PageRank parameters (damping, iterations, convergence).
	 */
	async pagerank(
		config: PageRankConfig,
	): Promise<PageRankResultWithContinuation> {
		return unwrap(await this.actor.compute_pagerank(toOptionalConfig(config)));
	}

	/**
	 * Compute single-source shortest paths (Dijkstra's algorithm).
	 *
	 * @param start - Starting vertex ID.
	 * @param config - Optional SSSP configuration (max distance, target, filters).
	 */
	async sssp(
		start: number,
		config: SsspConfig = {},
	): Promise<SsspResultWithContinuation> {
		return unwrap(
			await this.actor.compute_sssp(start, toOptionalConfig(config)),
		);
	}

	/**
	 * Get collaborative filtering recommendations for a user.
	 *
	 * @param user - Source user vertex ID.
	 * @param config - Recommendation parameters (edge label, max hops, limit).
	 */
	async recommend(
		user: number,
		config: RecommendConfig,
	): Promise<Recommendation[]> {
		return unwrap(await this.actor.recommend(user, toOptionalConfig(config)));
	}

	// ── Index ──

	/**
	 * Create an equality index on a vertex or edge property.
	 *
	 * @param entityType - `"vertex"` or `"edge"`.
	 * @param propertyName - Name of the property to index.
	 */
	async createIndex(
		entityType: "vertex" | "edge",
		propertyName: string,
	): Promise<void> {
		const et: EntityType =
			entityType === "vertex" ? { Vertex: null } : { Edge: null };
		const it: IndexType = { Equality: null };
		unwrap(await this.actor.create_index(et, propertyName, it));
	}

	// ── Stats ──

	/**
	 * Get all outgoing edges (neighbors) of a vertex.
	 *
	 * @param vertexId - Vertex ID to query.
	 */
	async getNeighbors(vertexId: number): Promise<EdgeInfo[]> {
		return await this.actor.get_neighbors(vertexId);
	}

	/** Get basic graph statistics (vertex/edge counts, capacity, avg degree). */
	async getStats(): Promise<GraphStats> {
		return await this.actor.get_stats();
	}

	/** Get IC-certified graph statistics with certificate and witness. */
	async getStatsCertified(): Promise<CertifiedGraphStats> {
		return await this.actor.get_stats_certified();
	}

	/** Get query planner statistics (label cardinality, selectivity, indexes). */
	async getPlannerStats(): Promise<PlannerStats> {
		return await this.actor.get_planner_stats();
	}

	/** Recompute planner statistics by sampling the graph. */
	async computeGraphStats(): Promise<PlannerStats> {
		return await this.actor.compute_graph_stats();
	}

	/**
	 * Get a cached, IC-certified PageRank result.
	 *
	 * @param configHash - Hash of the PageRank config used to compute the result.
	 */
	async getPagerankCertified(
		configHash: Uint8Array,
	): Promise<CertifiedPageRank> {
		return unwrap(await this.actor.get_pagerank_certified(configHash));
	}
}

/**
 * Convert optional TS fields (undefined) to Candid Opt ([]/[value]).
 */
function toOptionalConfig<T extends object>(
	config: T,
): Record<string, unknown> {
	const out: Record<string, unknown> = {};
	for (const [k, v] of Object.entries(config)) {
		out[k] = v === undefined ? [] : [v];
	}
	return out;
}
