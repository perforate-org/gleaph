import { SocialDemoScenario } from "~/generated/social_demo_gateway";

export const SOCIAL_DEMO_SCENARIO_IDS = [
  "PublicTimeline",
  "AliceHomeFeed",
  "TopicPath",
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
};

export const scenarioDefinitionById = (id: ScenarioId): ScenarioDefinition => {
  const definition = SCENARIO_DEFINITIONS[id];
  if (!definition) {
    throw new Error(`Unknown social demo scenario: ${id}`);
  }
  return definition;
};
