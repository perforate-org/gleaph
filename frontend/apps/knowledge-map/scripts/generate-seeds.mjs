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


const validateEmbedding = (node) => {
  const e = node.embedding;
  if (!e) {
    throw new Error(`Post node ${node.id} is missing embedding`);
  }
  if (!Number.isFinite(e.dims) || e.dims <= 0 || (e.dims | 0) !== e.dims) {
    throw new Error(`Invalid embedding dims for ${node.id}: ${e.dims}`);
  }
  if (!Array.isArray(e.values) || e.values.length !== e.dims) {
    throw new Error(`Embedding values length mismatch for ${node.id}: expected ${e.dims}, got ${e.values ? e.values.length : 'none'}`);
  }
  for (const v of e.values) {
    if (!Number.isFinite(v)) {
      throw new Error(`Non-finite embedding value for ${node.id}: ${v}`);
    }
  }
  if (e.name !== "post_vec") {
    throw new Error(`Unexpected embedding name for ${node.id}: ${e.name}`);
  }
  if (!["L2Squared", "Cosine"].includes(e.metric)) {
    throw new Error(`Unsupported embedding metric for ${node.id}: ${e.metric}`);
  }
  return { name: e.name, dims: e.dims, metric: e.metric, values: e.values };
};

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

const hasPostEmbeddings = graph.nodes.some((node) => node.kind === "post" && node.embedding);
const embeddings = {};
for (const node of graph.nodes) {
  if (node.embedding) {
    if (node.kind !== "post") {
      throw new Error(`Non-Post node ${node.id} has embedding`);
    }
    embeddings[node.id] = validateEmbedding(node);
  } else if (hasPostEmbeddings && node.kind === "post") {
    throw new Error(`Post node ${node.id} is missing embedding`);
  }
}

writeFileSync(outputPath, `${JSON.stringify({ seeds, embeddings }, null, 2)}
`);
console.log(`Wrote ${outputPath} (${seeds.length} seeds)`);
