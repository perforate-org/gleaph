import { readFileSync, writeFileSync, readdirSync, existsSync } from "node:fs";
import { basename, dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import YAML from "yaml";
import {
  readRawUsers,
  readRawPosts,
  scaleUsers,
  scalePostsForUsers,
  scaleUserEmbeddings,
  readScaleEnv,
} from "./social-scale.mjs";

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
 * Stable opaque Post identity. Authors reference posts by `<author>/<stem>` in
 * YAML; graph IDs are derived here so configuration never manages identifiers.
 */
const opaquePostId = (relPath) => `p_${sha256Hex(`social-demo:post:${relPath}`).slice(0, 20)}`;

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
// Load configuration tree and apply in-memory scaling
// ---------------------------------------------------------------------------
// Scale factors are controlled by environment variables so the same authored
// YAML can produce a small demo or a larger one without editing files.

const { userScale, postScale } = readScaleEnv();

const rawUsersAndEmbeddings = readRawUsers(CONFIG_DIR);
const rawUserEmbeddingsById = rawUsersAndEmbeddings.userEmbeddingsById;
const rawPosts = readRawPosts(CONFIG_DIR, rawUserEmbeddingsById);

const users = scaleUsers(rawUsersAndEmbeddings.users, userScale);
const posts = scalePostsForUsers(rawPosts, users, postScale);
const userEmbeddingsById = scaleUserEmbeddings(
  rawUserEmbeddingsById,
  userScale,
  postScale,
);

if (userScale > 1 || postScale > 1) {
  console.log(
    `Scaled social demo data: userScale=${userScale}, postScale=${postScale} (${users.length} users, ${posts.length} posts)`,
  );
}

const postIdByReference = new Map();
for (const post of posts) {
  if (postIdByReference.has(post.reference)) {
    throw new Error(`Duplicate post reference: ${post.reference}`);
  }
  postIdByReference.set(post.reference, post.id);
}
for (const post of posts) {
  if (!post.replyToReference) continue;
  const replyTo = postIdByReference.get(post.replyToReference);
  if (!replyTo) {
    throw new Error(
      `Unknown reply_to reference ${post.replyToReference} in ${post.reference}; expected <author>/<post-stem>`,
    );
  }
  post.replyTo = replyTo;
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

// Start with a chronological order, then deterministically shuffle small recent
// post windows. The seed graph is also the source of sequence numbers for
// materialized feeds, so bounded shuffling keeps the feed mostly recent while
// allowing natural-looking adjacent posts from the same author.
posts.sort((a, b) => {
  if (b.createdAt !== a.createdAt) return b.createdAt - a.createdAt;
  return `${a.userId}/${a.fileStem}`.localeCompare(`${b.userId}/${b.fileStem}`);
});

const seedOrderWindowSize = 48;
const mixedPosts = [];
for (let start = 0; start < posts.length; start += seedOrderWindowSize) {
  const window = posts.slice(start, start + seedOrderWindowSize);
  window.sort((a, b) => {
    const hashA = sha256Hex(`social-demo:seed-order:${a.id}`);
    const hashB = sha256Hex(`social-demo:seed-order:${b.id}`);
    return hashA.localeCompare(hashB);
  });
  mixedPosts.push(...window);
}

// Keep the deterministic shuffle from producing an implausible run of three
// or more posts by the same author, while deliberately leaving pairs possible.
for (let index = 2; index < mixedPosts.length; index += 1) {
  const author = mixedPosts[index].userId;
  if (
    mixedPosts[index - 1].userId !== author ||
    mixedPosts[index - 2].userId !== author
  ) {
    continue;
  }
  const replacement = mixedPosts.findIndex(
    (post, candidate) => candidate > index && post.userId !== author,
  );
  if (replacement >= 0) {
    [mixedPosts[index], mixedPosts[replacement]] = [
      mixedPosts[replacement],
      mixedPosts[index],
    ];
  }
}
posts.splice(0, posts.length, ...mixedPosts);

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
// Their insertion order is deterministic and mixed per recipient so a feed does not
// present one author's posts as a contiguous block. The fixed-label scan reverses this
// sequence for the displayed order without a runtime sort key.
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

const publicFeedPosts = posts.slice().reverse().filter((post) => post.isPublic);
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
const mixedHomeFeedEntries = [];
for (const recipientId of new Set(homeFeedEntries.map((entry) => entry.recipientId))) {
  const recipientEntries = homeFeedEntries
    .filter((entry) => entry.recipientId === recipientId)
    .sort((a, b) => {
      const hashA = sha256Hex(`social-demo:home-feed-order:${recipientId}:${a.postId}`);
      const hashB = sha256Hex(`social-demo:home-feed-order:${recipientId}:${b.postId}`);
      return hashA.localeCompare(hashB);
    });

  // A feed has its own author mix because a viewer sees only a subset of the
  // global post stream. Keep the order deterministic while avoiding adjacent
  // posts from the same author whenever another author is available.
  for (let index = 1; index < recipientEntries.length; index += 1) {
    if (recipientEntries[index - 1].userId !== recipientEntries[index].userId) continue;
    const replacement = recipientEntries.findIndex(
      (entry, candidate) => candidate > index && entry.userId !== recipientEntries[index].userId,
    );
    if (replacement >= 0) {
      [recipientEntries[index], recipientEntries[replacement]] = [
        recipientEntries[replacement],
        recipientEntries[index],
      ];
    }
  }
  mixedHomeFeedEntries.push(...recipientEntries);
}
homeFeedEntries.splice(0, homeFeedEntries.length, ...mixedHomeFeedEntries);

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

const paramName = (prefix, base) => `$${prefix}_${base}`;

const nodeIdentity = (node, prefix, params) => {
  if (node.gqlLabel === "User") {
    params[paramName(prefix, "user_id")] = node.id;
    return `user_id: ${paramName(prefix, "user_id")}`;
  }
  // demo_id values are small enough to fit in a JS Number for JSON serialization.
  params[paramName(prefix, "demo_id")] = Number(demoId(node.id));
  return `demo_id: ${paramName(prefix, "demo_id")}`;
};

const nodeProperties = (node, prefix, params) => {
  const props = [
    nodeIdentity(node, prefix, params),
    `demo_graph: '${DEMO_GRAPH}'`,
    `${node.property}: ${paramName(prefix, node.property)}`,
  ];
  params[paramName(prefix, node.property)] = node.label;
  if (node.properties) {
    for (const [key, value] of Object.entries(node.properties)) {
      if (value && typeof value === "object" && "raw" in value) {
        props.push(`${key}: ${value.raw}`);
      } else if (typeof value === "string") {
        props.push(`${key}: ${paramName(prefix, key)}`);
        params[paramName(prefix, key)] = value;
      } else if (typeof value === "boolean") {
        props.push(`${key}: ${paramName(prefix, key)}`);
        params[paramName(prefix, key)] = value;
      } else if (typeof value === "number") {
        props.push(`${key}: ${paramName(prefix, key)}`);
        params[paramName(prefix, key)] = value;
      } else {
        throw new Error(
          `Unsupported property type for ${node.id}.${key}: ${typeof value}`,
        );
      }
    }
  }
  return props.join(", ");
};

const nodeMatch = (node, variable, params) =>
  `(${variable}:${node.gqlLabel} {${nodeIdentity(node, variable, params)}, demo_graph: '${DEMO_GRAPH}'})`;

const nodeCreate = (node, variable, params) =>
  `(${variable}:${node.gqlLabel} {${nodeProperties(node, variable, params)}})`;

const edgeProperties = (edge, params) => {
  params.$edge_id = edge.id;
  params.$demo_kind = edge.displayLabel;
  return `{demo_edge_id: $edge_id, demo_kind: $demo_kind}`;
};

const seeds = [];

for (const node of graph.nodes.filter((entry) => entry.layer === 0)) {
  const params = {};
  seeds.push({
    key: `${DEMO_GRAPH}-seed-node-${node.id}`,
    gql: `INSERT ${nodeCreate(node, "n", params)}`,
    params,
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

  const params = {};
  const edgeProps = edgeProperties(edge, params);
  if (created.has(edge.target)) {
    seeds.push({
      key: `${DEMO_GRAPH}-seed-edge-${edge.id}`,
      gql:
        `MATCH ${nodeMatch(source, "a", params)}, ${nodeMatch(target, "b", params)} RETURN a NEXT ` +
        `INSERT (a)-[:${edge.gqlLabel} ${edgeProps}]->(b)`,
      params,
    });
  } else {
    seeds.push({
      key: `${DEMO_GRAPH}-seed-edge-${edge.id}`,
      gql:
        `MATCH ${nodeMatch(source, "a", params)} RETURN a NEXT ` +
        `INSERT (a)-[:${edge.gqlLabel} ${edgeProps}]->${nodeCreate(target, "b", params)}`,
      params,
    });
    created.add(edge.target);
  }

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
    embeddings[demoId(node.id).toString()] = node.embedding;
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
  "YuiHomeFeed",
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
        `rdbSummary: ${JSON.stringify(doc.rdbSummary)}`,
        `graphSummary: ${JSON.stringify(doc.graphSummary)}`,
        `preparedQuery: ${JSON.stringify(doc.preparedQuery)}`,
        `semanticVector: ${JSON.stringify(doc.semanticVector ?? null)}`,
        `scenario: (offset: number): SocialDemoScenario => ({ __kind__: ${JSON.stringify(doc.id)}, ${doc.id}: { offset } })`,
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
  rdbSummary: string;
  graphSummary: string;
  preparedQuery: string;
  semanticVector: number[] | null;
  scenario: (offset: number) => SocialDemoScenario;
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

const buildTsUserAvatars = () => {
  const entries = users
    .map((user) => `  ${JSON.stringify(user.name)}: ${JSON.stringify(`/avatars/${user.id}.svg`)}`)
    .join(",\n");
  return `// Generated by frontend/apps/social-demo/scripts/build-config.mjs.
// Do not edit manually; change the YAML files under config/ and rerun the build.

export const USER_AVATARS: Record<string, string> = {
${entries}
};
`;
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

writeFileSync(join(DATA_DIR, "userAvatars.generated.ts"), buildTsUserAvatars());

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
const expectedNullVectorCount = scenarios.length - 2;
if (nullVectors.length !== expectedNullVectorCount) {
  throw new Error(
    `Expected exactly ${expectedNullVectorCount} null semanticVector entries, found ${nullVectors.length}`,
  );
}

// Validate emitted seeds.
const seedsText = readFileSync(join(KM_SEEDS_DIR, "social-seeds.json"), "utf8");
const parsedSeeds = JSON.parse(seedsText);
const expectedSeedCount =
  graph.nodes.filter((node) => node.layer === 0).length + graph.edges.length;
if (
  !Array.isArray(parsedSeeds.seeds) ||
  parsedSeeds.seeds.length !== expectedSeedCount
) {
  throw new Error(
    `Expected exactly ${expectedSeedCount} seeds, found ${parsedSeeds.seeds?.length ?? 0}`,
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
  `Wrote 5 artifacts: social-graph.json, social-seeds.json, scenarios.generated.ts, scenarios.generated.json, userAvatars.generated.ts`,
);
