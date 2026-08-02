import { createRouterActor } from "./actor.mjs";

const GRAPH_NAME = process.env.GLEAPH_DEMO_GRAPH_NAME || "gleaph.pocket_ic";
const ROUTER_CANISTER = process.env.GLEAPH_DEMO_ROUTER_CANISTER || "gleaph-router";
const EMBEDDING_NAME = process.env.GLEAPH_DEMO_EMBEDDING_NAME || "post_vec";

function isDuplicateError(text) {
  return (
    text.includes("AlreadyExists") ||
    text.includes("already exists") ||
    text.includes("Conflict") ||
    text.includes("Duplicate") ||
    text.includes("UniquenessViolation")
  );
}

/** Ingest embeddings using only replay-proven canonical IDs from bulk vertex receipts. */
export async function ingestEmbeddingsBatch(
  sourceIdMap,
  embeddings,
  createActor = createRouterActor,
) {
  const entries = Object.entries(embeddings);
  const items = entries.map(([sourceId, meta]) => {
    const elementId = sourceIdMap.get(sourceId);
    if (!(elementId instanceof Uint8Array) || elementId.byteLength !== 8) {
      throw new Error(`No replay-proven element_id for Post source_id ${sourceId}`);
    }
    if (!Array.isArray(meta.values) || meta.values.some((value) => !Number.isFinite(value))) {
      throw new Error(`Embedding ${sourceId} must contain finite numeric values`);
    }
    if (meta.dims !== undefined && meta.values.length !== meta.dims) {
      throw new Error(`Embedding ${sourceId} dimension mismatch`);
    }
    return {
      encoded_vertex_id: elementId,
      values: meta.values.map(Number),
    };
  });

  if (items.length === 0) return;
  const router = await createActor(ROUTER_CANISTER);
  const result = await router.ingest_vertex_embeddings({
    logical_graph_name: GRAPH_NAME,
    embedding_name: EMBEDDING_NAME,
    items,
  });
  if ("Err" in result) {
    throw new Error(`ingest_vertex_embeddings failed: ${JSON.stringify(result.Err)}`);
  }
  if (result.Ok.length !== entries.length) {
    throw new Error(`Batch result length mismatch: expected ${entries.length}, got ${result.Ok.length}`);
  }

  for (let index = 0; index < entries.length; index += 1) {
    const itemResult = result.Ok[index];
    if ("Err" in itemResult && !isDuplicateError(String(itemResult.Err))) {
      throw new Error(`ingest_vertex_embeddings failed for ${entries[index][0]}: ${itemResult.Err}`);
    }
  }
}
