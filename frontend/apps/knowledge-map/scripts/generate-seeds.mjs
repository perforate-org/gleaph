import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const DEMO_GRAPH = "knowledge-map";
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const graphPath = join(root, "seeds", "knowledge-map-graph.json");
const outputPath = join(root, "seeds", "knowledge-map-seeds.json");

const graph = JSON.parse(readFileSync(graphPath, "utf8"));
const nodes = new Map(graph.nodes.map((node) => [node.id, node]));

const escapeGqlString = (value) => value.replace(/'/g, "''");
const nodePropertyLiteral = (node) => `${node.property}: '${escapeGqlString(node.label)}'`;
const nodeMatch = (node, variable) =>
  `(${variable}:${node.gqlLabel} {demo_id: '${node.id}', demo_graph: '${DEMO_GRAPH}'})`;
const nodeCreate = (node, variable) =>
  `(${variable}:${node.gqlLabel} {demo_id: '${node.id}', demo_graph: '${DEMO_GRAPH}', ${nodePropertyLiteral(node)}})`;
const edgeProperties = (edge) =>
  `{demo_edge_id: '${edge.id}', demo_kind: '${edge.displayLabel}'}`;

const seeds = [];

for (const node of graph.nodes.filter((entry) => entry.layer === 0)) {
  seeds.push({
    key: `km-seed-node-${node.id}`,
    gql: `INSERT ${nodeCreate(node, "n")}`,
  });
}

const created = new Set(graph.nodes.filter((entry) => entry.layer === 0).map((entry) => entry.id));

for (const edge of graph.edges) {
  const source = nodes.get(edge.source);
  const target = nodes.get(edge.target);
  if (!source || !target) {
    throw new Error(`Unknown knowledge-map edge endpoint: ${edge.id}`);
  }

  if (created.has(edge.target)) {
    seeds.push({
      key: `km-seed-edge-${edge.id}`,
      gql:
        `MATCH ${nodeMatch(source, "a")}, ${nodeMatch(target, "b")} RETURN a NEXT ` +
        `INSERT (a)-[:${edge.gqlLabel} ${edgeProperties(edge)}]->(b)`,
    });
    continue;
  }

  seeds.push({
    key: `km-seed-edge-${edge.id}`,
    gql:
      `MATCH ${nodeMatch(source, "a")} RETURN a NEXT ` +
      `INSERT (a)-[:${edge.gqlLabel} ${edgeProperties(edge)}]->${nodeCreate(target, "b")}`,
  });
  created.add(edge.target);
}

writeFileSync(outputPath, `${JSON.stringify({ seeds }, null, 2)}\n`);
console.log(`Wrote ${outputPath} (${seeds.length} seeds)`);
