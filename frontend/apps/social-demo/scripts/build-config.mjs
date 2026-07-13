import { readFileSync, writeFileSync, readdirSync, existsSync } from "node:fs";
import { basename, dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import YAML from "yaml";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const APP_ROOT = resolve(SCRIPT_DIR, "..");
const CONFIG_DIR = join(APP_ROOT, "config");
const KM_SEEDS_DIR = resolve(APP_ROOT, "..", "knowledge-map", "seeds");
const DATA_DIR = join(APP_ROOT, "src", "data");

const DEMO_GRAPH = "social";
const EMBEDDING_NAME = "post_vec";
const EMBEDDING_DIMS = 8;
const EMBEDDING_METRIC = "L2Squared";

// ---------------------------------------------------------------------------
// Deterministic helpers
// ---------------------------------------------------------------------------

const sha256Hex = (input) =>
  createHash("sha256").update(input, "utf8").digest("hex");

/**
 * Deterministic created_at fallback for posts that do not declare one.
 * Derived from the post file path so renames change the timestamp but content
 * edits do not.
 */
const fallbackCreatedAt = (relPath) => {
  const hash = sha256Hex(relPath);
  const offset = parseInt(hash.slice(0, 6), 16) % 10000;
  return 202607030000 + offset * 100;
};

/**
 * Deterministic 8-dimensional L2Squared embedding fallback.
 */
const fallbackEmbedding = (postId) => {
  const hash = sha256Hex(`social-demo:${postId}`);
  const values = [];
  for (let i = 0; i < EMBEDDING_DIMS; i += 1) {
    const byte = parseInt(hash.slice(i * 2, i * 2 + 2), 16);
    values.push(byte / 127.5 - 1.0);
  }
  return {
    name: EMBEDDING_NAME,
    dims: EMBEDDING_DIMS,
    metric: EMBEDDING_METRIC,
    values,
  };
};

const readYaml = (path) => YAML.parse(readFileSync(path, "utf8"));

const sortedDirNames = (dir) =>
  readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort();

const sortedYamlFiles = (dir) =>
  readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isFile() && entry.name.endsWith(".yaml"))
    .map((entry) => entry.name)
    .sort();

const fileStem = (filename) => basename(filename, extname(filename));

// ---------------------------------------------------------------------------
// Load configuration tree
// ---------------------------------------------------------------------------

const users = [];
const posts = [];
const userDir = join(CONFIG_DIR, "users");

const userEmbeddingsById = new Map();

for (const userName of sortedDirNames(userDir)) {
  const profilePath = join(userDir, userName, "profile.yaml");
  const profile = readYaml(profilePath);
  if (profile.id !== userName) {
    throw new Error(
      `User directory ${userName} does not match profile id ${profile.id}`,
    );
  }
  users.push(profile);

  const userEmbeddingsPath = join(userDir, userName, "embeddings.yaml");
  if (existsSync(userEmbeddingsPath)) {
    userEmbeddingsById.set(userName, readYaml(userEmbeddingsPath));
  }

  const postsDir = join(userDir, userName, "posts");
  const postFiles = sortedYamlFiles(postsDir);
  for (const postFile of postFiles) {
    const stem = fileStem(postFile);
    const doc = readYaml(join(postsDir, postFile));
    const relPath = `users/${userName}/posts/${postFile}`;
    const postId = doc.id ?? `post-${userName}-${stem}`;
    const createdAt = doc.created_at ?? fallbackCreatedAt(relPath);
    const isPublic = doc.is_public ?? true;

    const userEmbeddings = userEmbeddingsById.get(userName);
    let embedding;
    if (
      userEmbeddings &&
      Object.prototype.hasOwnProperty.call(userEmbeddings, stem)
    ) {
      const e = userEmbeddings[stem];
      embedding = {
        name: e.name ?? EMBEDDING_NAME,
        dims: e.dims ?? EMBEDDING_DIMS,
        metric: e.metric ?? EMBEDDING_METRIC,
        values: e.values,
      };
    } else {
      embedding = fallbackEmbedding(postId);
    }

    posts.push({
      id: postId,
      userId: userName,
      fileStem: stem,
      label: doc.body,
      body: doc.body,
      createdAt,
      isPublic,
      topics: doc.topics ?? [],
      replyTo: doc.reply_to,
      embedding,
    });
  }
}

const topics = [];
const topicsDir = join(CONFIG_DIR, "topics");
for (const topicFile of sortedYamlFiles(topicsDir)) {
  const doc = readYaml(join(topicsDir, topicFile));
  const stem = fileStem(topicFile);
  if (doc.id !== stem) {
    throw new Error(
      `Topic file ${topicFile} does not match topic id ${doc.id}`,
    );
  }
  topics.push(doc);
}

const communities = [];
const communitiesDir = join(CONFIG_DIR, "communities");
for (const communityFile of sortedYamlFiles(communitiesDir)) {
  const doc = readYaml(join(communitiesDir, communityFile));
  const stem = fileStem(communityFile);
  if (doc.id !== stem) {
    throw new Error(
      `Community file ${communityFile} does not match community id ${doc.id}`,
    );
  }
  communities.push(doc);
}

const feeds = [];
const feedsDir = join(CONFIG_DIR, "feeds");
for (const feedFile of sortedYamlFiles(feedsDir)) {
  const doc = readYaml(join(feedsDir, feedFile));
  const stem = fileStem(feedFile);
  if (doc.id !== stem && doc.id !== `${stem}-feed`) {
    throw new Error(`Feed file ${feedFile} does not match feed id ${doc.id}`);
  }
  feeds.push(doc);
}

// Sort posts by created_at descending (ties broken by file path) to keep the
// same ordering as the original hand-maintained social-graph.json.
posts.sort((a, b) => {
  if (b.createdAt !== a.createdAt) return b.createdAt - a.createdAt;
  return `${a.userId}/${a.fileStem}`.localeCompare(`${b.userId}/${b.fileStem}`);
});

// ---------------------------------------------------------------------------
// Deterministic global numeric id allocator
// ---------------------------------------------------------------------------
// Order: users (sorted dir name), communities (sorted stem), topics (sorted
// stem), posts (sorted by user dir then file stem within each user).

const idMap = new Map();
let nextId = 1n;

for (const user of users.sort((a, b) => a.id.localeCompare(b.id))) {
  idMap.set(user.id, nextId++);
}
for (const community of communities.sort((a, b) => a.id.localeCompare(b.id))) {
  idMap.set(community.id, nextId++);
}
for (const topic of topics.sort((a, b) => a.id.localeCompare(b.id))) {
  idMap.set(topic.id, nextId++);
}
for (const post of posts.slice().sort((a, b) => {
  if (a.userId !== b.userId) return a.userId.localeCompare(b.userId);
  return a.fileStem.localeCompare(b.fileStem);
})) {
  idMap.set(post.id, nextId++);
}
for (const feed of feeds.sort((a, b) => a.id.localeCompare(b.id))) {
  idMap.set(feed.id, nextId++);
}

const demoId = (stringId) => {
  const id = idMap.get(stringId);
  if (id === undefined) {
    throw new Error(`No numeric demo_id assigned for string id: ${stringId}`);
  }
  return id;
};

// ---------------------------------------------------------------------------
// Build graph nodes and edges
// ---------------------------------------------------------------------------

const nodes = [];

// Layer-0 nodes: users, then communities, then topics.
for (const user of users) {
  nodes.push({
    id: user.id,
    label: user.name,
    kind: "user",
    gqlLabel: "User",
    layer: 0,
    property: "name",
  });
}
for (const community of communities) {
  nodes.push({
    id: community.id,
    label: community.name,
    kind: "community",
    gqlLabel: "Community",
    layer: 0,
    property: "name",
  });
}
for (const topic of topics) {
  nodes.push({
    id: topic.id,
    label: topic.name,
    kind: "topic",
    gqlLabel: "Topic",
    layer: 0,
    property: "name",
  });
}

// Post nodes are layer 1.
for (const post of posts) {
  const properties = {};
  // Posts carry a wall-clock timestamp from CURRENT_TIMESTAMP at seed execution
  // time so the frontend can render relative time. Deterministic feed ordering is
  // declared in the prepared query with GLEAPH.SEQUENCE on the materialized feed
  // edge rather than a synthetic ordering property.
  properties.created_at = { raw: "CURRENT_TIMESTAMP" };
  properties.is_public = post.isPublic;

  nodes.push({
    id: post.id,
    label: post.label,
    kind: "post",
    gqlLabel: "Post",
    layer: 1,
    property: "body",
    properties,
    embedding: post.embedding,
  });
}

// Feed nodes are layer 0 and allocated after posts so existing demo_id values stay stable.
for (const feed of feeds) {
  nodes.push({
    id: feed.id,
    label: feed.name,
    kind: "feed",
    gqlLabel: feed.gqlLabel,
    layer: 0,
    property: "name",
  });
}

const nodeById = new Map(nodes.map((node) => [node.id, node]));

const validateEndpoint = (id, kind, edgeId) => {
  const node = nodeById.get(id);
  if (!node) throw new Error(`Unknown ${kind} endpoint in edge ${edgeId}`);
  return node;
};

const edges = [];

// FOLLOWS and MEMBER_OF edges first, in user order.
for (const user of users) {
  for (const target of user.follows ?? []) {
    validateEndpoint(target, "user", `${user.id}-follows-${target}`);
    edges.push({
      id: `${user.id}-follows-${target}`,
      source: user.id,
      target,
      gqlLabel: "FOLLOWS",
      displayLabel: "follows",
    });
  }
  for (const target of user.memberships ?? []) {
    validateEndpoint(target, "community", `${user.id}-member-of-${target}`);
    const edgeTarget = target;
    const stripped = target.replace(/^community-/, "");
    edges.push({
      id: `${user.id}-member-of-${stripped}`,
      source: user.id,
      target: edgeTarget,
      gqlLabel: "MEMBER_OF",
      displayLabel: "member of",
    });
  }
}

// POSTED edges follow the post order.
for (const post of posts) {
  validateEndpoint(post.id, "post", `${post.userId}-posted-${post.fileStem}`);
  edges.push({
    id: `${post.userId}-posted-${post.fileStem}`,
    source: post.userId,
    target: post.id,
    gqlLabel: "POSTED",
    displayLabel: "posted",
  });
}

// REPLY_TO is canonical social state authored by the reply post. Emit it only
// after every Post exists, so replies can target posts created by any author.
for (const post of posts) {
  if (!post.replyTo) continue;
  validateEndpoint(post.replyTo, "post", `${post.id}-reply-to-${post.replyTo}`);
  edges.push({
    id: `${post.id}-reply-to-${post.replyTo}`,
    source: post.id,
    target: post.replyTo,
    gqlLabel: "REPLY_TO",
    displayLabel: "reply to",
  });
}

// Materialized feed edges are derived from canonical POSTED, FOLLOWS, and is_public.
// They are emitted oldest-first so the default descending fixed-label scan returns
// newest posts first without an ORDER BY.
const publicFeed = feeds.find((feed) => feed.id === "public-feed");
if (!publicFeed) {
  throw new Error("Missing public-feed definition");
}

const followsByTarget = new Map();
for (const user of users) {
  for (const target of user.follows ?? []) {
    if (!followsByTarget.has(target)) {
      followsByTarget.set(target, []);
    }
    followsByTarget.get(target).push(user.id);
  }
}

const feedEdgeOrder = (a, b) => {
  if (a.createdAt !== b.createdAt) return a.createdAt - b.createdAt;
  return `${a.userId}/${a.fileStem}`.localeCompare(`${b.userId}/${b.fileStem}`);
};

const publicFeedPosts = posts
  .filter((post) => post.isPublic)
  .slice()
  .sort(feedEdgeOrder);
for (const post of publicFeedPosts) {
  validateEndpoint(post.id, "post", `post-${post.id}-in-public-feed`);
  validateEndpoint(publicFeed.id, "feed", `post-${post.id}-in-public-feed`);
  edges.push({
    id: `post-${post.id}-in-public-feed`,
    source: post.id,
    target: publicFeed.id,
    gqlLabel: "IN_PUBLIC_FEED",
    displayLabel: "in public feed",
  });
}

const homeFeedEntries = [];
for (const post of posts) {
  if (!post.isPublic) continue;
  const homeFeedRecipients = new Set([post.userId, ...(followsByTarget.get(post.userId) ?? [])]);
  for (const recipientId of homeFeedRecipients) {
    homeFeedEntries.push({
      postId: post.id,
      recipientId,
      createdAt: post.createdAt,
      userId: post.userId,
      fileStem: post.fileStem,
    });
  }
}
homeFeedEntries.sort((a, b) => {
  if (a.createdAt !== b.createdAt) return a.createdAt - b.createdAt;
  return `${a.userId}/${a.fileStem}`.localeCompare(`${b.userId}/${b.fileStem}`);
});

for (const entry of homeFeedEntries) {
  const edgeId = `${entry.postId}-in-home-${entry.recipientId}`;
  validateEndpoint(entry.postId, "post", edgeId);
  validateEndpoint(entry.recipientId, "user", edgeId);
  edges.push({
    id: edgeId,
    source: entry.postId,
    target: entry.recipientId,
    gqlLabel: "IN_HOME_FEED",
    displayLabel: "in home feed",
  });
}

// HAS_TOPIC edges last.
for (const post of posts) {
  for (const topicId of post.topics) {
    validateEndpoint(topicId, "topic", `${post.id}-${topicId}`);
    edges.push({
      id: `${post.id}-${topicId}`,
      source: post.id,
      target: topicId,
      gqlLabel: "HAS_TOPIC",
      displayLabel: "has topic",
    });
  }
}

const graph = { nodes, edges };

// ---------------------------------------------------------------------------
// Seed GQL generation (mirrors frontend/apps/knowledge-map/scripts/generate-seeds.mjs)
// ---------------------------------------------------------------------------

const escapeGqlString = (value) => String(value).replace(/'/g, "''");

const nodePropertyLiteral = (node) =>
  `${node.property}: '${escapeGqlString(node.label)}'`;

const nodeProperties = (node) => {
  const props = [
    `demo_id: ${demoId(node.id)}`,
    `demo_graph: '${DEMO_GRAPH}'`,
    nodePropertyLiteral(node),
  ];
  if (node.properties) {
    for (const [key, value] of Object.entries(node.properties)) {
      if (value && typeof value === "object" && "raw" in value) {
        props.push(`${key}: ${value.raw}`);
      } else if (typeof value === "string") {
        props.push(`${key}: '${escapeGqlString(value)}'`);
      } else if (typeof value === "boolean") {
        props.push(`${key}: ${value ? "TRUE" : "FALSE"}`);
      } else if (typeof value === "number") {
        props.push(`${key}: ${value}`);
      } else {
        throw new Error(
          `Unsupported property type for ${node.id}.${key}: ${typeof value}`,
        );
      }
    }
  }
  return props.join(", ");
};

const nodeMatch = (node, variable) =>
  `(${variable}:${node.gqlLabel} {demo_id: ${demoId(node.id)}, demo_graph: '${DEMO_GRAPH}'})`;

const nodeCreate = (node, variable) =>
  `(${variable}:${node.gqlLabel} {${nodeProperties(node)}})`;

const edgeProperties = (edge) =>
  `{demo_edge_id: '${edge.id}', demo_kind: '${edge.displayLabel}'}`;

const seeds = [];

for (const node of graph.nodes.filter((entry) => entry.layer === 0)) {
  seeds.push({
    key: `${DEMO_GRAPH}-seed-node-${node.id}`,
    gql: `INSERT ${nodeCreate(node, "n")}`,
  });
}

const created = new Set(
  graph.nodes.filter((entry) => entry.layer === 0).map((entry) => entry.id),
);

for (const edge of graph.edges) {
  const source = nodeById.get(edge.source);
  const target = nodeById.get(edge.target);
  if (!source || !target) {
    throw new Error(`Unknown ${DEMO_GRAPH} edge endpoint: ${edge.id}`);
  }

  if (created.has(edge.target)) {
    seeds.push({
      key: `${DEMO_GRAPH}-seed-edge-${edge.id}`,
      gql:
        `MATCH ${nodeMatch(source, "a")}, ${nodeMatch(target, "b")} RETURN a NEXT ` +
        `INSERT (a)-[:${edge.gqlLabel} ${edgeProperties(edge)}]->(b)`,
    });
    continue;
  }

  seeds.push({
    key: `${DEMO_GRAPH}-seed-edge-${edge.id}`,
    gql:
      `MATCH ${nodeMatch(source, "a")} RETURN a NEXT ` +
      `INSERT (a)-[:${edge.gqlLabel} ${edgeProperties(edge)}]->${nodeCreate(target, "b")}`,
  });
  created.add(edge.target);
}

const hasPostEmbeddings = graph.nodes.some(
  (node) => node.kind === "post" && node.embedding,
);
const embeddings = {};
for (const node of graph.nodes) {
  if (node.embedding) {
    if (node.kind !== "post") {
      throw new Error(`Non-Post node ${node.id} has embedding`);
    }
    embeddings[node.id] = node.embedding;
  } else if (hasPostEmbeddings && node.kind === "post") {
    throw new Error(`Post node ${node.id} is missing embedding`);
  }
}

// ---------------------------------------------------------------------------
// Scenario code generation
// ---------------------------------------------------------------------------

const SCENARIO_ORDER = [
  "PublicTimeline",
  "AliceHomeFeed",
  "TopicPath",
  "SemanticDiscovery",
  "AliceSemanticFeed",
];

const scenarios = [];
const scenariosDir = join(CONFIG_DIR, "scenarios");
for (const scenarioFile of sortedYamlFiles(scenariosDir)) {
  const doc = readYaml(join(scenariosDir, scenarioFile));
  scenarios.push(doc);
}

scenarios.sort((a, b) => {
  const aIndex = SCENARIO_ORDER.indexOf(a.id);
  const bIndex = SCENARIO_ORDER.indexOf(b.id);
  const aRank = aIndex === -1 ? Number.MAX_SAFE_INTEGER : aIndex;
  const bRank = bIndex === -1 ? Number.MAX_SAFE_INTEGER : bIndex;
  if (aRank !== bRank) return aRank - bRank;
  return a.id.localeCompare(b.id);
});

const scenarioOrder = scenarios.map((doc) => doc.id);

const buildTsScenarios = () => {
  const recordEntries = scenarios
    .map((doc) => {
      const fields = [
        `id: ${JSON.stringify(doc.id)}`,
        `preparedQueryId: ${JSON.stringify(doc.preparedQueryId)}`,
        `label: ${JSON.stringify(doc.label)}`,
        `shortLabel: ${JSON.stringify(doc.shortLabel)}`,
        `feedTitle: ${JSON.stringify(doc.feedTitle)}`,
        `explanationTitle: ${JSON.stringify(doc.explanationTitle)}`,
        `rdbSummary: ${JSON.stringify(doc.rdbSummary)}`,
        `graphSummary: ${JSON.stringify(doc.graphSummary)}`,
        `preparedQuery: ${JSON.stringify(doc.preparedQuery)}`,
        `semanticVector: ${JSON.stringify(doc.semanticVector ?? null)}`,
        `scenario: SocialDemoScenario.${doc.id}`,
      ].join(",\n    ");
      return `  ${doc.id}: {\n    ${fields},\n  }`;
    })
    .join(",\n");

  const demoIdMapEntries = Array.from(idMap.entries())
    .sort((a, b) => {
      if (a[1] < b[1]) return -1;
      if (a[1] > b[1]) return 1;
      return 0;
    })
    .map(([k, v]) => `  ${JSON.stringify(k)}: ${v}n`)
    .join(",\n");

  return `// Generated by frontend/apps/social-demo/scripts/build-config.mjs.
// Do not edit manually; change the YAML files under config/ and rerun the build.

import { SocialDemoScenario } from "~/generated/social_demo_gateway";

export const DEMO_ID_MAP: Record<string, bigint> = {\n${demoIdMapEntries}\n};\n\n\nexport const SOCIAL_DEMO_SCENARIO_IDS = [${scenarioOrder
    .map((id) => JSON.stringify(id))
    .join(", ")}] as const;

export type ScenarioId = (typeof SOCIAL_DEMO_SCENARIO_IDS)[number];

export type ScenarioDefinition = {
  id: ScenarioId;
  preparedQueryId: string;
  label: string;
  shortLabel: string;
  feedTitle: string;
  explanationTitle: string;
  rdbSummary: string;
  graphSummary: string;
  preparedQuery: string;
  semanticVector: number[] | null;
  scenario: SocialDemoScenario;
};

export const SCENARIO_DEFINITIONS: Record<ScenarioId, ScenarioDefinition> = {
${recordEntries},
};

export const displayPostId = (postId: bigint): string => postId.toString();

export const scenarioDefinitionById = (id: ScenarioId): ScenarioDefinition => {
  const definition = SCENARIO_DEFINITIONS[id];
  if (!definition) {
    throw new Error(\`Unknown social demo scenario: \${id}\`);
  }
  return definition;
};
`;
};

const buildJsonScenarios = () => {
  // Include the postId -> demo_id (Int64) map so the embeddings ingest script
  // (and any other non-TypeScript client) can resolve the canonical integer
  // id for each post without re-parsing the seed GQL strings.  Mirrors
  // `DEMO_ID_MAP` in scenarios.generated.ts; the keys are the post file stems
  // and the values are the same Int64 values used in the GQL seeds.
  // idMap is a Map<string, bigint>; serialize the values as JSON numbers
  // (Int64 in graph storage).  Number is safe here because the social-demo
  // demo_id space is well below 2^53.
  const demoIdMap = Object.fromEntries(
    Array.from(idMap.entries()).map(([k, v]) => [k, Number(v)]),
  );
  return `${JSON.stringify({ scenarios, demoIdMap }, null, 2)}\n`;
};

// ---------------------------------------------------------------------------
// Emit artifacts
// ---------------------------------------------------------------------------

writeFileSync(
  join(KM_SEEDS_DIR, "social-graph.json"),
  `${JSON.stringify(graph, null, 2)}\n`,
);

writeFileSync(
  join(KM_SEEDS_DIR, "social-seeds.json"),
  `${JSON.stringify({ seeds, embeddings }, null, 2)}\n`,
);

writeFileSync(join(DATA_DIR, "scenarios.generated.ts"), buildTsScenarios());

writeFileSync(join(DATA_DIR, "scenarios.generated.json"), buildJsonScenarios());

// Validate generated scenario JSON semanticVector shape.
const scenariosJsonText = readFileSync(
  join(DATA_DIR, "scenarios.generated.json"),
  "utf8",
);
const parsedScenariosJson = JSON.parse(scenariosJsonText);
if (!Array.isArray(parsedScenariosJson.scenarios)) {
  throw new Error(
    "Expected scenarios.generated.json to contain a scenarios array",
  );
}
const semanticVectors = parsedScenariosJson.scenarios.map(
  (s) => s.semanticVector,
);
const nonNullVectors = semanticVectors.filter(
  (v) => Array.isArray(v) && v.length === EMBEDDING_DIMS,
);
const nullVectors = semanticVectors.filter((v) => v === null);
if (nonNullVectors.length !== 2) {
  throw new Error(
    `Expected exactly 2 non-null semanticVector arrays of length ${EMBEDDING_DIMS}, found ${nonNullVectors.length}`,
  );
}
if (nullVectors.length !== 3) {
  throw new Error(
    `Expected exactly 3 null semanticVector entries, found ${nullVectors.length}`,
  );
}

// Validate emitted seeds.
const seedsText = readFileSync(join(KM_SEEDS_DIR, "social-seeds.json"), "utf8");
const parsedSeeds = JSON.parse(seedsText);
if (!Array.isArray(parsedSeeds.seeds) || parsedSeeds.seeds.length !== 37) {
  throw new Error(
    `Expected exactly 37 seeds, found ${parsedSeeds.seeds?.length ?? 0}`,
  );
}
const demoIdOccurrences = seedsText.match(/demo_id: [^,}]+/g) ?? [];
const textDemoIdOccurrences = demoIdOccurrences.filter((m) => m.includes("'"));
if (textDemoIdOccurrences.length > 0) {
  throw new Error(
    `Found text demo_id literals in seeds (expected numeric): ${textDemoIdOccurrences.join(", ")}`,
  );
}

console.log(
  `Wrote 4 artifacts: social-graph.json, social-seeds.json, scenarios.generated.ts, scenarios.generated.json`,
);
