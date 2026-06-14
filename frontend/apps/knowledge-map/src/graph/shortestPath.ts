import type { GraphEdgeSpec, GraphNodeSpec } from "~/data/knowledgeMapGraph";

export type ShortestPathResult = {
  nodeIds: string[];
  edgeIds: string[];
  hopCount: number;
};

export type ShortestPathStep = {
  nodeId: string;
  edgeId?: string;
  text: string;
};

const escapeGqlString = (value: string): string => value.replace(/'/g, "''");

export const buildShortestPathAdjacency = (
  edges: GraphEdgeSpec[],
): Map<string, Array<{ target: string; edgeId: string; label: string }>> => {
  const adjacency = new Map<string, Array<{ target: string; edgeId: string; label: string }>>();
  for (const edge of edges) {
    const outgoing = adjacency.get(edge.source) ?? [];
    outgoing.push({ target: edge.target, edgeId: edge.id, label: edge.displayLabel });
    adjacency.set(edge.source, outgoing);
  }
  return adjacency;
};

export const computeShortestPath = (
  sourceId: string,
  targetId: string,
  edges: GraphEdgeSpec[],
): ShortestPathResult | undefined => {
  if (sourceId === targetId) {
    return { nodeIds: [sourceId], edgeIds: [], hopCount: 0 };
  }

  const adjacency = buildShortestPathAdjacency(edges);
  const queue = [sourceId];
  const visited = new Set([sourceId]);
  const parent = new Map<string, { nodeId: string; edgeId: string }>();

  while (queue.length > 0) {
    const current = queue.shift();
    if (!current) {
      break;
    }
    if (current === targetId) {
      break;
    }

    for (const hop of adjacency.get(current) ?? []) {
      if (visited.has(hop.target)) {
        continue;
      }
      visited.add(hop.target);
      parent.set(hop.target, { nodeId: current, edgeId: hop.edgeId });
      queue.push(hop.target);
    }
  }

  if (!visited.has(targetId)) {
    return undefined;
  }

  const nodeIds: string[] = [];
  const edgeIds: string[] = [];
  let cursor = targetId;
  nodeIds.unshift(cursor);
  while (cursor !== sourceId) {
    const previous = parent.get(cursor);
    if (!previous) {
      return undefined;
    }
    edgeIds.unshift(previous.edgeId);
    cursor = previous.nodeId;
    nodeIds.unshift(cursor);
  }

  return { nodeIds, edgeIds, hopCount: edgeIds.length };
};

export const computeBfsFrontiers = (
  sourceId: string,
  targetId: string,
  edges: GraphEdgeSpec[],
): string[][] => {
  const adjacency = buildShortestPathAdjacency(edges);
  const frontiers: string[][] = [[sourceId]];
  const visited = new Set([sourceId]);
  let depth = 0;

  while (depth < frontiers.length) {
    const currentLayer = frontiers[depth] ?? [];
    const nextLayer: string[] = [];

    for (const nodeId of currentLayer) {
      if (nodeId === targetId) {
        return frontiers;
      }
      for (const hop of adjacency.get(nodeId) ?? []) {
        if (visited.has(hop.target)) {
          continue;
        }
        visited.add(hop.target);
        nextLayer.push(hop.target);
      }
    }

    if (nextLayer.length === 0) {
      return frontiers;
    }

    frontiers.push(nextLayer);
    depth += 1;
  }

  return frontiers;
};

export const buildShortestPathStorySteps = (
  path: ShortestPathResult,
  nodes: GraphNodeSpec[],
  edges: GraphEdgeSpec[],
): ShortestPathStep[] => {
  const nodeById = new Map(nodes.map((node) => [node.id, node]));
  const edgeById = new Map(edges.map((edge) => [edge.id, edge]));

  return path.nodeIds.map((nodeId, index) => {
    const node = nodeById.get(nodeId);
    if (!node) {
      throw new Error(`Shortest-path story references unknown node: ${nodeId}`);
    }

    const edgeId = index > 0 ? path.edgeIds[index - 1] : undefined;
    const edge = edgeId ? edgeById.get(edgeId) : undefined;
    if (index > 0 && !edge) {
      throw new Error(`Shortest-path story references unknown edge: ${edgeId}`);
    }

    if (index === 0) {
      return {
        nodeId,
        text: `Breadth-first search starts at ${node.label}.`,
      };
    }

    if (index === path.nodeIds.length - 1) {
      return {
        nodeId,
        edgeId,
        text: `Shortest path found in ${path.hopCount} hops: ${node.label}.`,
      };
    }

    return {
      nodeId,
      edgeId,
      text: `Hop ${index}: follow ${edge?.displayLabel ?? "relationship"} to ${node.label}.`,
    };
  });
};

export const buildShortestPathGqlQuery = (
  source: GraphNodeSpec,
  target: GraphNodeSpec,
  maxHops: number,
): string => {
  const sourceLiteral =
    source.property === "name"
      ? `name: '${escapeGqlString(source.label)}'`
      : `title: '${escapeGqlString(source.label)}'`;
  const targetLiteral =
    target.property === "name"
      ? `name: '${escapeGqlString(target.label)}'`
      : `title: '${escapeGqlString(target.label)}'`;

  return (
    `MATCH (a:${source.gqlLabel} {${sourceLiteral}}), (d:${target.gqlLabel} {${targetLiteral}}), ` +
    `p = ANY SHORTEST (a)-/WROTE|TAGGED|USED_BY|DOCUMENTED_BY|SECURED_BY/->{1,${maxHops}}(d) ` +
    "RETURN p"
  );
};
