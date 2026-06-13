import type {
  DemoEdge,
  DemoNode,
  KnowledgeMapViewModel,
  NodeKind,
  ResultCard,
  StoryStep,
  TechnicalFlowStep,
} from "~/types";

const nodeKinds = new Set<NodeKind>(["person", "post", "topic", "project", "document"]);

export type RouterKnowledgeMapResponse = {
  id: string;
  title: string;
  question: string;
  rows: RouterKnowledgeMapRow[];
};

export type RouterKnowledgeMapRow =
  | RouterNodeRow
  | RouterEdgeRow
  | RouterPathRow
  | RouterResultRow
  | RouterTechnicalFlowRow;

export type RouterNodeRow = {
  kind: "node";
  node_id: string;
  node_label: string;
  node_kind: NodeKind;
  node_x?: number;
  node_y?: number;
  node_z?: number;
};

export type RouterEdgeRow = {
  kind: "edge";
  edge_id: string;
  edge_source: string;
  edge_target: string;
  edge_label: string;
};

export type RouterPathRow = {
  kind: "path";
  path_index: number;
  path_node_id: string;
  path_edge_id?: string;
  story_text: string;
};

export type RouterResultRow = {
  kind: "result";
  result_title: string;
  result_kind: string;
  result_reason: string;
  result_node_id?: string;
};

export type RouterTechnicalFlowRow = {
  kind: "technical";
  technical_index: number;
  technical_title: string;
  technical_detail: string;
};

export const adaptRouterKnowledgeMapResponse = (
  response: RouterKnowledgeMapResponse,
): KnowledgeMapViewModel => {
  const nodes: DemoNode[] = [];
  const edges: DemoEdge[] = [];
  const pathRows: RouterPathRow[] = [];
  const results: ResultCard[] = [];
  const technicalRows: RouterTechnicalFlowRow[] = [];

  for (const row of response.rows) {
    switch (row.kind) {
      case "node":
        nodes.push(adaptNode(row));
        break;
      case "edge":
        edges.push(adaptEdge(row));
        break;
      case "path":
        pathRows.push(row);
        break;
      case "result":
        results.push(adaptResult(row));
        break;
      case "technical":
        technicalRows.push(row);
        break;
      default:
        assertNever(row);
    }
  }

  const storySteps = [...pathRows]
    .sort((left, right) => left.path_index - right.path_index)
    .map<StoryStep>((row) => ({
      nodeId: row.path_node_id,
      edgeId: row.path_edge_id,
      text: row.story_text,
    }));

  const technicalFlow = [...technicalRows]
    .sort((left, right) => left.technical_index - right.technical_index)
    .map<TechnicalFlowStep>((row) => ({
      title: row.technical_title,
      detail: row.technical_detail,
    }));

  validateReferences(response.id, nodes, edges, storySteps, results);

  return {
    id: response.id,
    title: response.title,
    question: response.question,
    nodes,
    edges,
    activePath: storySteps.flatMap((step) => (step.nodeId ? [step.nodeId] : [])),
    storySteps,
    results,
    technicalFlow,
  };
};

const adaptNode = (row: RouterNodeRow): DemoNode => {
  if (!nodeKinds.has(row.node_kind)) {
    throw new Error(`Unknown knowledge-map node kind: ${row.node_kind}`);
  }

  const hasPosition =
    row.node_x !== undefined || row.node_y !== undefined || row.node_z !== undefined;

  return {
    id: row.node_id,
    label: row.node_label,
    kind: row.node_kind,
    positionHint: hasPosition
      ? [row.node_x ?? 0, row.node_y ?? 0, row.node_z ?? 0]
      : undefined,
  };
};

const adaptEdge = (row: RouterEdgeRow): DemoEdge => ({
  id: row.edge_id,
  source: row.edge_source,
  target: row.edge_target,
  label: row.edge_label,
});

const adaptResult = (row: RouterResultRow): ResultCard => ({
  title: row.result_title,
  kind: row.result_kind,
  reason: row.result_reason,
  nodeId: row.result_node_id,
});

const validateReferences = (
  scenarioId: string,
  nodes: DemoNode[],
  edges: DemoEdge[],
  storySteps: StoryStep[],
  results: ResultCard[],
) => {
  const nodeIds = new Set(nodes.map((node) => node.id));
  const edgeIds = new Set(edges.map((edge) => edge.id));

  for (const edge of edges) {
    if (!nodeIds.has(edge.source) || !nodeIds.has(edge.target)) {
      throw new Error(`Knowledge-map scenario ${scenarioId} has an edge with missing nodes.`);
    }
  }

  for (const step of storySteps) {
    if (step.nodeId && !nodeIds.has(step.nodeId)) {
      throw new Error(`Knowledge-map scenario ${scenarioId} has a story step with an unknown node.`);
    }
    if (step.edgeId && !edgeIds.has(step.edgeId)) {
      throw new Error(`Knowledge-map scenario ${scenarioId} has a story step with an unknown edge.`);
    }
  }

  for (const result of results) {
    if (result.nodeId && !nodeIds.has(result.nodeId)) {
      throw new Error(`Knowledge-map scenario ${scenarioId} has a result with an unknown node.`);
    }
  }
};

const assertNever = (value: never): never => {
  throw new Error(`Unsupported knowledge-map row: ${JSON.stringify(value)}`);
};
