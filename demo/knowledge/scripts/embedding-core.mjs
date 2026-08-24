// Deterministic embedding derivation shared by two consumers that must agree bit-for-bit:
//
//   - scripts/gen-embeddings.mjs (Node; seeds/embeddings.jsonl, ingested by `gleaph embed ingest`)
//   - demo page src/lib/queryEmbedding.ts (browser; the scenario-3 `$query` vector)
//
// Because both sides call this exact module, the in-page query vector equals the ingested
// document vectors' recipe by construction: feeding the digest of one Document's
// `source_id` yields that document's stored embedding, so the semantic-search top-1 is
// fixed to the seeded document rather than depending on a second implementation.
//
// Pure module: no Node-only or browser-only imports. SHA-256 is supplied by the caller
// (node:crypto in Node, WebCrypto subtle in the browser).

export const EMBEDDING_DIMS = 768;
export const SEED_PREFIX = "gleaph-knowledge-demo-embedding-v1:";

/**
 * Derive one L2-normalized 768-dim vector from a 32-byte digest (e.g. sha256 of
 * `SEED_PREFIX + source_id`). Deterministic across engines: 32-bit xorshift with `>>> 0`
 * widening, IEEE-754 double arithmetic only.
 */
export function unitVectorFromDigest(digest) {
  if (!digest || digest.length < 4) {
    throw new Error("unitVectorFromDigest needs at least 4 digest bytes");
  }
  // First four digest bytes little-endian (matches Node's Buffer.readUInt32LE).
  let state =
    (digest[0] | (digest[1] << 8) | (digest[2] << 16) | (digest[3] << 24)) >>> 0;
  if (state === 0) state = 0x9e3779b9;
  const raw = new Array(EMBEDDING_DIMS);
  for (let i = 0; i < EMBEDDING_DIMS; i++) {
    state ^= state << 13;
    state >>>= 0;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    // Map to [-1, 1).
    raw[i] = state / 2147483648 - 1;
  }
  const norm = Math.sqrt(raw.reduce((sum, value) => sum + value * value, 0));
  return raw.map((value) => value / norm);
}
