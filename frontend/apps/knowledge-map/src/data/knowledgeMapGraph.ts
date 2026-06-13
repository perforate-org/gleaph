import { layerPosition } from "~/graph/fanOutLayout";
import type { RouterKnowledgeMapResponse, RouterKnowledgeMapRow } from "~/api/viewModelAdapter";
import type { NodeKind } from "~/types";
import knowledgeMapGraphData from "../../seeds/knowledge-map-graph.json";

export const DEMO_GRAPH = "knowledge-map";

export type GraphNodeSpec = {
  id: string;
  label: string;
  kind: NodeKind;
  gqlLabel: "Person" | "Post" | "Topic" | "Project" | "Document";
  layer: number;
  index: number;
  count: number;
  property: "name" | "title";
};

export type GraphEdgeSpec = {
  id: string;
  source: string;
  target: string;
  gqlLabel: "WROTE" | "TAGGED" | "USED_BY" | "DOCUMENTED_BY" | "SECURED_BY";
  displayLabel: string;
};

export type ScenarioSpec = {
  id: string;
  title: string;
  question: string;
  path: Array<{
    nodeId: string;
    edgeId?: string;
    text: string;
  }>;
  results: Array<{
    title: string;
    kind: string;
    reason: string;
    nodeId: string;
  }>;
  technical: Array<{
    title: string;
    detail: string;
  }>;
};

export const KNOWLEDGE_MAP_NODES = knowledgeMapGraphData.nodes as GraphNodeSpec[];
export const KNOWLEDGE_MAP_EDGES = knowledgeMapGraphData.edges as GraphEdgeSpec[];
export const KNOWLEDGE_MAP_SCENARIOS = knowledgeMapGraphData.scenarios as ScenarioSpec[];

export const KNOWLEDGE_MAP_LIVE_QUERY =
  "MATCH ()-[e]->() WHERE e.demo_edge_id IS NOT NULL " +
  "RETURN e.demo_edge_id AS edge_id, e.demo_kind AS edge_kind " +
  "ORDER BY edge_id";

export type LiveGraphEdgeRow = {
  sourceDemoId: string;
  targetDemoId: string;
  edgeId: string;
  edgeKind: string;
};

export const parseLiveGraphEdgeRow = (columns: Map<string, string>): LiveGraphEdgeRow => {
  const edgeId = columns.get("edge_id");
  const edgeKind = columns.get("edge_kind");
  if (!edgeId || !edgeKind) {
    throw new Error("Live knowledge-map query row is missing demo edge columns.");
  }
  return {
    sourceDemoId: columns.get("source_demo_id") ?? "",
    targetDemoId: columns.get("target_demo_id") ?? "",
    edgeId,
    edgeKind,
  };
};

export const buildKnowledgeMapGraphRows = (): RouterKnowledgeMapRow[] => [
  ...KNOWLEDGE_MAP_NODES.map((node) => {
    const [node_x, node_y, node_z] = layerPosition(node.layer, node.index, node.count);
    return {
      kind: "node" as const,
      node_id: node.id,
      node_label: node.label,
      node_kind: node.kind,
      node_x,
      node_y,
      node_z,
    };
  }),
  ...KNOWLEDGE_MAP_EDGES.map((edge) => ({
    kind: "edge" as const,
    edge_id: edge.id,
    edge_source: edge.source,
    edge_target: edge.target,
    edge_label: edge.displayLabel,
  })),
];

export const buildScenarioResponse = (scenarioId: string): RouterKnowledgeMapResponse => {
  const scenario = KNOWLEDGE_MAP_SCENARIOS.find((entry) => entry.id === scenarioId);
  if (!scenario) {
    throw new Error(`Unknown knowledge-map scenario: ${scenarioId}`);
  }

  const rows: RouterKnowledgeMapRow[] = [
    ...buildKnowledgeMapGraphRows(),
    ...scenario.path.map((step, path_index) => ({
      kind: "path" as const,
      path_index,
      path_node_id: step.nodeId,
      path_edge_id: step.edgeId,
      story_text: step.text,
    })),
    ...scenario.results.map((result) => ({
      kind: "result" as const,
      result_title: result.title,
      result_kind: result.kind,
      result_reason: result.reason,
      result_node_id: result.nodeId,
    })),
    ...scenario.technical.map((step, technical_index) => ({
      kind: "technical" as const,
      technical_index,
      technical_title: step.title,
      technical_detail: step.detail,
    })),
  ];

  return {
    id: scenario.id,
    title: scenario.title,
    question: scenario.question,
    rows,
  };
};

export const buildLiveScenarioResponse = (
  scenarioId: string,
  liveEdges: LiveGraphEdgeRow[],
): RouterKnowledgeMapResponse => {
  const scenario = buildScenarioResponse(scenarioId);
  const edgeIds = new Set(KNOWLEDGE_MAP_EDGES.map((edge) => edge.id));
  const liveEdgeIds = new Set(liveEdges.map((edge) => edge.edgeId));

  for (const edgeId of edgeIds) {
    if (!liveEdgeIds.has(edgeId)) {
      throw new Error(`Live knowledge-map graph is missing seeded edge: ${edgeId}`);
    }
  }

  if (liveEdges.length !== edgeIds.size) {
    throw new Error(
      `Live knowledge-map graph edge count mismatch: expected ${edgeIds.size}, got ${liveEdges.length}`,
    );
  }

  return scenario;
};

export const routerKnowledgeMapResponses: RouterKnowledgeMapResponse[] =
  KNOWLEDGE_MAP_SCENARIOS.map((scenario) => buildScenarioResponse(scenario.id));
