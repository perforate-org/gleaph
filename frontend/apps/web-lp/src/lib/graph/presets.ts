// The three demo queries and the graph slices they run over.
//
// This is the page's fixture: the same shape `GraphSession` takes (positions, edge pairs, label
// slots) plus the preset's query and a handful of example rows. A live build swaps the rows and the
// slice for a prepared-query response; the view code does not change.

export interface DemoNode {
  name: string;
  /** Index into the preset's class list. */
  slot: number;
}

export interface DemoPreset {
  id: string;
  tab: string;
  title: string;
  detail: string;
  query: string;
  classes: string[];
  nodes: DemoNode[];
  /** Edge endpoint pairs, by node index. */
  edges: [number, number][];
  rows: [string, string, string][];
}

/** Deterministic pseudo random, so a preset looks the same on every load. */
function seeded(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state * 1664525 + 1013904223) >>> 0;
    return state / 0x100000000;
  };
}

/** A graph of `count` nodes in `classes`, laid out as a few overlapping clusters. */
function clusterGraph(
  count: number,
  classes: string[],
  seed: number,
): { nodes: DemoNode[]; edges: [number, number][] } {
  const random = seeded(seed);
  const nodes: DemoNode[] = [];
  for (let index = 0; index < count; index++) {
    const slot = index % classes.length;
    nodes.push({ name: `${classes[slot]} ${index + 1}`, slot });
  }
  const edges: [number, number][] = [];
  for (let index = 0; index < count; index++) {
    const source = nodes[index].slot;
    // Mostly local edges inside a class, a few bridges between classes: a graph that reads as a
    // graph rather than as a lattice.
    const sameClass = nodes
      .map((node, at) => ({ node, at }))
      .filter(({ node, at }) => node.slot === source && at !== index)
      .map(({ at }) => at);
    const target = sameClass[Math.floor(random() * sameClass.length)];
    if (target !== undefined) edges.push([index, target]);
    if (index % 7 === 0) {
      const other = Math.floor(random() * count);
      if (other !== index) edges.push([index, other]);
    }
  }
  return { nodes, edges };
}

const graphSlice = clusterGraph(28, ["Document", "Person", "Team", "Chunk"], 7);
const textSlice = clusterGraph(24, ["Document", "Chunk", "Term", "Query"], 11);
const vectorSlice = clusterGraph(26, ["Document", "Embedding", "Person", "Team"], 23);

export const presets: DemoPreset[] = [
  {
    id: "graph",
    tab: "Traversal",
    title: "A citation chain, planned once",
    detail: "1 to 3 hops over adjacency, bounded on the plan",
    query: `MATCH (src:Document {title: 'GraphRAG retrieval'})-[e:CITES]->{1,3}(dst:Document)
WHERE dst.lang = 'en'
RETURN dst.title AS title, e.weight AS weight
ORDER BY weight DESC
LIMIT 5`,
    classes: ["Document", "Person", "Team", "Chunk"],
    nodes: graphSlice.nodes,
    edges: graphSlice.edges,
    rows: [
      ["Running a graph database in a canister", "CITES", "0.94"],
      ["Sovereign knowledge graphs", "CITES", "0.88"],
      ["Vector search inside a boundary", "CITES", "0.81"],
      ["Prepared queries, end to end", "CITES", "0.76"],
      ["Graph-native retrieval", "CITES", "0.71"],
    ],
  },
  {
    id: "text",
    tab: "Full text",
    title: "Relevance inside the caller's grants",
    detail: "Inverted index first, then the graph",
    query: `MATCH (d:Document)-[:MENTIONS]->(t:Term)
WHERE t.text = 'sovereignty'
RETURN d.title AS title, score(d.text) AS score
ORDER BY score DESC
LIMIT 5`,
    classes: ["Document", "Chunk", "Term", "Query"],
    nodes: textSlice.nodes,
    edges: textSlice.edges,
    rows: [
      ["What sovereignty means for a graph", "score", "0.91"],
      ["Data that never leaves the boundary", "score", "0.84"],
      ["Canister storage and the boundary", "score", "0.79"],
      ["Authorization before retrieval", "score", "0.72"],
      ["Text search without a server", "score", "0.68"],
    ],
  },
  {
    id: "vector",
    tab: "Hybrid",
    title: "Graph pattern first, then vectors",
    detail: "The pattern bounds what the vector search may see",
    query: `MATCH (t:Team {name: 'Platform'})<-[:BELONGS_TO]-(p:Person)-[:OWNS]->(d:Document)
FOR $query IN vector(d.summary, 'top_k = 5', 'min_score = 0.7')
RETURN d.title AS title, p.name AS owner, $query.score AS score
ORDER BY score DESC`,
    classes: ["Document", "Embedding", "Person", "Team"],
    nodes: vectorSlice.nodes,
    edges: vectorSlice.edges,
    rows: [
      ["Sovereign knowledge graphs", "Platform team", "0.88"],
      ["Running a graph database in a canister", "Platform team", "0.83"],
      ["Prepared queries, end to end", "Platform team", "0.77"],
      ["Vector search inside a boundary", "Platform team", "0.74"],
      ["Graph-native retrieval", "Platform team", "0.69"],
    ],
  },
];
