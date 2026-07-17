import { readFileSync, readdirSync, existsSync } from "node:fs";
import { basename, extname, join } from "node:path";
import YAML from "yaml";
import { createHash } from "node:crypto";

// ---------------------------------------------------------------------------
// Deterministic helpers (mirror build-config.mjs)
// ---------------------------------------------------------------------------

const sha256Hex = (input) =>
  createHash("sha256").update(input, "utf8").digest("hex");

const fallbackCreatedAt = (relPath) => {
  const hash = sha256Hex(relPath);
  const offset = parseInt(hash.slice(0, 6), 16) % 10000;
  return 202607030000 + offset * 100;
};

const opaquePostId = (relPath) =>
  `p_${sha256Hex(`social-demo:post:${relPath}`).slice(0, 20)}`;

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
// Scale identifiers
// ---------------------------------------------------------------------------

export const scaleUserId = (id, gen) =>
  gen === 0 ? id : `${id}_gen${gen}`;

export const scalePostStem = (stem, gen) =>
  gen === 0 ? stem : `${stem}_gen${gen}`;

// ---------------------------------------------------------------------------
// Read raw user profiles from config/users
// ---------------------------------------------------------------------------

export const readRawUsers = (configDir) => {
  const users = [];
  const userEmbeddingsById = new Map();
  const userDir = join(configDir, "users");

  for (const userName of sortedDirNames(userDir)) {
    const profilePath = join(userDir, userName, "profile.yaml");
    const profile = readYaml(profilePath);
    if (profile.id !== userName) {
      throw new Error(
        `User directory ${userName} does not match profile id ${profile.id}`,
      );
    }
    users.push(profile);

    const embeddingsPath = join(userDir, userName, "embeddings.yaml");
    if (existsSync(embeddingsPath)) {
      userEmbeddingsById.set(userName, readYaml(embeddingsPath));
    }
  }

  return { users, userEmbeddingsById };
};

// ---------------------------------------------------------------------------
// Read raw posts from config/users/*/posts/*.yaml
// ---------------------------------------------------------------------------

const EMBEDDING_NAME = "post_vec";
const EMBEDDING_DIMS = 8;
const EMBEDDING_METRIC = "L2Squared";

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

export const readRawPosts = (configDir, userEmbeddingsById) => {
  const posts = [];
  const userDir = join(configDir, "users");

  for (const userName of sortedDirNames(userDir)) {
    const postsDir = join(userDir, userName, "posts");
    for (const postFile of sortedYamlFiles(postsDir)) {
      const stem = fileStem(postFile);
      const doc = readYaml(join(postsDir, postFile));
      const relPath = `users/${userName}/posts/${postFile}`;
      if (doc.id !== undefined) {
        throw new Error(
          `Post ${relPath} must not declare id; Post IDs are generated from the file path`,
        );
      }

      const postId = opaquePostId(relPath);
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
        reference: `${userName}/${stem}`,
        label: doc.body,
        body: doc.body,
        createdAt,
        isPublic,
        topics: doc.topics ?? [],
        replyToReference: doc.reply_to,
        embedding,
      });
    }
  }

  return posts;
};

// ---------------------------------------------------------------------------
// Scale users in memory
// ---------------------------------------------------------------------------

export const scaleUsers = (users, userScale) => {
  const scale = Math.max(1, Number(userScale) || 1);
  if (scale <= 1) {
    return users.map((user) => ({ ...user, _originalId: user.id, _gen: 0 }));
  }

  const scaled = [];
  for (const user of users) {
    for (let gen = 0; gen < scale; gen += 1) {
      const newId = scaleUserId(user.id, gen);
      scaled.push({
        ...user,
        id: newId,
        _originalId: user.id,
        _gen: gen,
        name: gen === 0 ? user.name : `${user.name} ${gen}`,
        follows: (user.follows ?? []).map((target) => scaleUserId(target, gen)),
        memberships: user.memberships ?? [],
      });
    }
  }
  return scaled;
};

// ---------------------------------------------------------------------------
// Scale posts in memory
// ---------------------------------------------------------------------------

export const scalePostsForUsers = (posts, users, postScale) => {
  const scale = Math.max(1, Number(postScale) || 1);
  const postsByUser = new Map();
  for (const post of posts) {
    if (!postsByUser.has(post.userId)) postsByUser.set(post.userId, []);
    postsByUser.get(post.userId).push(post);
  }

  const scaled = [];
  for (const user of users) {
    const originalUserId = user._originalId ?? user.id;
    const userGen = user._gen ?? 0;
    const userPosts = postsByUser.get(originalUserId) ?? [];
    for (const post of userPosts) {
      for (let gen = 0; gen < scale; gen += 1) {
        const newUserId = user.id;
        const newStem = scalePostStem(post.fileStem, gen);
        const newRelPath = `users/${newUserId}/posts/${newStem}.yaml`;
        const newReference = `${newUserId}/${newStem}`;

        let replyToReference;
        if (post.replyToReference) {
          const parts = post.replyToReference.split("/");
          if (parts.length === 2 && parts[0] && parts[1]) {
            replyToReference = `${scaleUserId(parts[0], userGen)}/${scalePostStem(parts[1], gen)}`;
          }
        }

        scaled.push({
          ...post,
          gen,
          id: opaquePostId(newRelPath),
          userId: newUserId,
          fileStem: newStem,
          relPath: newRelPath,
          reference: newReference,
          replyToReference,
        });
      }
    }
  }
  return scaled;
};

// ---------------------------------------------------------------------------
// Scale per-user embedding map so copies of a post get the same vector
// ---------------------------------------------------------------------------

export const scaleUserEmbeddings = (userEmbeddingsById, userScale, postScale) => {
  const uScale = Math.max(1, Number(userScale) || 1);
  const pScale = Math.max(1, Number(postScale) || 1);
  if (uScale <= 1 && pScale <= 1) return userEmbeddingsById;

  const scaled = new Map();
  for (const [userId, embeddings] of userEmbeddingsById.entries()) {
    for (let gen = 0; gen < uScale; gen += 1) {
      const newId = scaleUserId(userId, gen);
      const newEmbeddings = {};
      for (const [stem, vector] of Object.entries(embeddings)) {
        for (let pgen = 0; pgen < pScale; pgen += 1) {
          newEmbeddings[scalePostStem(stem, pgen)] = vector;
        }
      }
      scaled.set(newId, newEmbeddings);
    }
  }
  return scaled;
};

// ---------------------------------------------------------------------------
// Convenience: parse environment variables used by the demo scripts
// ---------------------------------------------------------------------------

export const readScaleEnv = () => ({
  userScale: parseInt(process.env.SOCIAL_DEMO_USER_SCALE ?? "1", 10) || 1,
  postScale: parseInt(process.env.SOCIAL_DEMO_POST_SCALE ?? "1", 10) || 1,
});
