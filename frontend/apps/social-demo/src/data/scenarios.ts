import { SocialDemoScenario } from "~/generated/social_demo_gateway";

export const SOCIAL_DEMO_SCENARIO_IDS = [
  "PublicTimeline",
  "AliceHomeFeed",
  "TopicPath",
  "SemanticDiscovery",
  "AliceSemanticFeed",
] as const;

export type ScenarioId = (typeof SOCIAL_DEMO_SCENARIO_IDS)[number];

export type ScenarioDefinition = {
  id: ScenarioId;
  label: string;
  shortLabel: string;
  feedTitle: string;
  explanationTitle: string;
  rdbSummary: string;
  graphSummary: string;
  scenario: SocialDemoScenario;
};

export const SCENARIO_DEFINITIONS: Record<ScenarioId, ScenarioDefinition> = {
  PublicTimeline: {
    id: "PublicTimeline",
    label: "Public timeline",
    shortLabel: "Timeline",
    feedTitle: "Public posts",
    explanationTitle: "Relational baseline",
    rdbSummary:
      "A chronological list of public rows is exactly what an RDB does well: one index on created_at, a simple visibility predicate, and no joins.",
    graphSummary:
      "The graph stores the same Post vertices and answers the same scan. The value here is not speed; it is that the same vertex model will later power relationship-aware feeds without a new schema.",
    scenario: SocialDemoScenario.PublicTimeline,
  },
  AliceHomeFeed: {
    id: "AliceHomeFeed",
    label: "Alice home feed",
    shortLabel: "Home",
    feedTitle: "Alice’s home feed",
    explanationTitle: "Graph traversal",
    rdbSummary:
      "An RDB needs a follows join table and a two-hop join (follower → followee → posts). The query is still SQL-shaped, but the relationship is now a traversal, not a foreign-key lookup.",
    graphSummary:
      "Gleaph walks Alice → FOLLOWS → author → POSTED → Post as one bounded graph pattern. The feed is the natural result of the relationship shape, not an application-side assembly.",
    scenario: SocialDemoScenario.AliceHomeFeed,
  },
  TopicPath: {
    id: "TopicPath",
    label: "Topic path",
    shortLabel: "Topic",
    feedTitle: "Topic explanation path",
    explanationTitle: "Explainable recommendation",
    rdbSummary:
      "Proving why a post was recommended requires re-running and documenting the joins: follow, post, topic. The answer is scattered across tables.",
    graphSummary:
      "Gleaph returns the same result plus the edge identities that caused it: alice-follows-bob, bob-posted-1, post-bob-1-topic-graph. The path is part of the query result.",
    scenario: SocialDemoScenario.TopicPath,
  },
  SemanticDiscovery: {
    id: "SemanticDiscovery",
    label: "Semantic discovery",
    shortLabel: "Semantic",
    feedTitle: "Vector-only semantic discovery",
    explanationTitle: "Vector retrieval",
    rdbSummary:
      "Pure vector search has no join table: it scores every public Post against a fixed query vector and returns the nearest neighbors by L2-squared distance.",
    graphSummary:
      "Gleaph stores canonical Post embeddings on the Graph shard and routes vector SEARCH through the derived index. `post-dave-1` is deliberately the nearest public Post even though Alice does not follow Dave.",
    scenario: SocialDemoScenario.SemanticDiscovery,
  },
  AliceSemanticFeed: {
    id: "AliceSemanticFeed",
    label: "Alice semantic feed",
    shortLabel: "Semantic+Graph",
    feedTitle: "Alice’s graph-constrained semantic feed",
    explanationTitle: "Hybrid retrieval",
    rdbSummary:
      "Combining vector similarity with a relational filter requires joining the vector result set back to followee authorship, usually in application code.",
    graphSummary:
      "Gleaph applies the same vector SEARCH inside the graph pattern `Alice → FOLLOWS → author → POSTED → Post`. `post-dave-1` is excluded despite being nearer because Dave is not a followee; only Bob and Carol’s posts are returned, in semantic order.",
    scenario: SocialDemoScenario.AliceSemanticFeed,
  },
};

export const scenarioDefinitionById = (id: ScenarioId): ScenarioDefinition => {
  const definition = SCENARIO_DEFINITIONS[id];
  if (!definition) {
    throw new Error(`Unknown social demo scenario: ${id}`);
  }
  return definition;
};
