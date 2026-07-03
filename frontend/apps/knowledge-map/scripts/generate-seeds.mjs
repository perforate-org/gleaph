import { readFileSync, writeFileSync } from "node:fs";
import { basename, dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

const defaultGraphPath = join(root, "seeds", "knowledge-map-graph.json");
const defaultOutputPath = join(root, "seeds", "knowledge-map-seeds.json");

const graphPath = process.argv[2] ? resolve(process.argv[2]) : defaultGraphPath;
const outputPath = process.argv[3] ? resolve(process.argv[3]) : defaultOutputPath;

const graphNameFromPath = (path) => {
  const name = basename(path, extname(path));
  return name.endsWith("-graph") ? name.slice(0, -6) : name;
};
const DEMO_GRAPH = graphNameFromPath(graphPath);

const graph = JSON.parse(readFileSync(graphPath, "utf8"));
const nodes = new Map(graph.nodes.map((node) => [node.id, node]));

const escapeGqlString = (value) => value.replace(/'/g, "''");
const nodePropertyLiteral = (node) => `${node.property}: '${escapeGqlString(node.label)}'`;

const nodeProperties = (node) => {
  const props = [
    `demo_id: '${node.id}'`,
    `demo_graph: '${DEMO_GRAPH}'`,
    nodePropertyLiteral(node),
  ];
  if (node.properties) {
    for (const [key, value] of Object.entries(node.properties)) {
      if (typeof value === "string") {
        props.push(`${key}: '${escapeGqlString(value)}'`);
      } else if (typeof value === "number" || typeof value === "boolean") {
        props.push(`${key}: ${value}`);
      } else {
        throw new Error(`Unsupported property type for ${node.id}.${key}: ${typeof value}`);
      }
    }
  }
  return props.join(", ");
};

const nodeMatch = (node, variable) =>
  `(${variable}:${node.gqlLabel} {demo_id: '${node.id}', demo_graph: '${DEMO_GRAPH}'})`;
const nodeCreate = (node, variable) =>
  `(${variable}:${node.gqlLabel} {${nodeProperties(node)}})`;
const edgeProperties = (edge) =>
  `{demo_edge_id: '${edge.id}', demo_kind: '${edge.displayLabel}'}`;

const seedPrefix = DEMO_GRAPH === "knowledge-map" ? "km" : DEMO_GRAPH;

const seeds = [];

for (const node of graph.nodes.filter((entry) => entry.layer === 0)) {
  seeds.push({
    key: `${seedPrefix}-seed-node-${node.id}`,
    gql: `INSERT ${nodeCreate(node, "n")}`,
  });
}

const created = new Set(graph.nodes.filter((entry) => entry.layer === 0).map((entry) => entry.id));

for (const edge of graph.edges) {
  const source = nodes.get(edge.source);
  const target = nodes.get(edge.target);
  if (!source || !target) {
    throw new Error(`Unknown ${DEMO_GRAPH} edge endpoint: ${edge.id}`);
  }

  if (created.has(edge.target)) {
    seeds.push({
      key: `${seedPrefix}-seed-edge-${edge.id}`,
      gql:
        `MATCH ${nodeMatch(source, "a")}, ${nodeMatch(target, "b")} RETURN a NEXT ` +
        `INSERT (a)-[:${edge.gqlLabel} ${edgeProperties(edge)}]->(b)`,
    });
    continue;
  }

  seeds.push({
    key: `${seedPrefix}-seed-edge-${edge.id}`,
    gql:
      `MATCH ${nodeMatch(source, "a")} RETURN a NEXT ` +
      `INSERT (a)-[:${edge.gqlLabel} ${edgeProperties(edge)}]->${nodeCreate(target, "b")}`,
  });
  created.add(edge.target);
}

writeFileSync(outputPath, `${JSON.stringify({ seeds }, null, 2)}\n`);
console.log(`Wrote ${outputPath} (${seeds.length} seeds)`);
