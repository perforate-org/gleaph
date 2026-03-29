export { GleaphClient, type GleaphClientOptions } from "./client.js";
export { GleaphError, unwrap } from "./errors.js";
export { GraphClient } from "./graph.js";
export { RegistryClient } from "./registry.js";
// Re-export Identity type so users can type-annotate without importing @gleaph/sdk/auth
export type { Identity } from "@icp-sdk/core/agent";
export type {
	AccessLevel,
	// Continuation
	AlgorithmKind,
	BfsConfig,
	BfsResult,
	BfsResultWithContinuation,
	CertifiedGraphStats,
	CertifiedPageRank,
	ContinuationToken,
	EdgeData,
	// Graph data
	EdgeInfo,
	EntityType,
	// Error
	GleaphError as GleaphErrorVariant,
	// Registry
	GraphConfig,
	GraphFingerprint,
	GraphInfo,
	// Graph stats
	GraphStats,
	IndexType,
	// Mutation
	MutationResult,
	MutationResultWithContinuation,
	PageRankConfig,
	PageRankResult,
	PageRankResultWithContinuation,
	PathElement,
	PlannerStats,
	// Prepared Statements
	PreparedKind,
	PreparedOptions,
	PreparedSortKey,
	PreparedSortSpec,
	PreparedStatementInfo,
	PropertyMap,
	// Query
	QueryExecutionBreakdown,
	QueryMode,
	QueryRequestOptions,
	QueryResult,
	QueryResultWithContinuation,
	QueryStats,
	Recommendation,
	RecommendConfig,
	SsspConfig,
	SsspResult,
	SsspResultWithContinuation,
	// Algorithms
	TimestampRange,
	// Value types
	Value,
	VertexData,
} from "./types.js";
export { toPropertyMap, toValue } from "./values.js";
