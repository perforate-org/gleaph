import { EMBEDDING_DIMS, SEED_PREFIX, unitVectorFromDigest } from "../../scripts/embedding-core.mjs";

/**
 * The seeded Document whose exact embedding becomes the scenario-3 query vector
 * (`d4`, "Vector search in graphs"). Because the query vector IS that document's stored
 * embedding — same shared recipe as scripts/gen-embeddings.mjs — the semantic-search
 * top-1 row is fixed by construction to this document.
 */
export const SCENARIO_QUERY_SOURCE_ID = "d4";

/**
 * Build the `$query` parameter for team-readable-documents: 768 little-endian f32s
 * (6144 bytes) carried as the GQL `Value::Bytes` wire form.
 */
export async function scenarioQueryVector(
  sourceId: string = SCENARIO_QUERY_SOURCE_ID,
): Promise<Uint8Array> {
  const message = new TextEncoder().encode(SEED_PREFIX + sourceId);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", message));
  const vector = unitVectorFromDigest(digest);
  const bytes = new Uint8Array(EMBEDDING_DIMS * 4);
  const view = new DataView(bytes.buffer);
  vector.forEach((value, index) => view.setFloat32(index * 4, value, true));
  return bytes;
}
