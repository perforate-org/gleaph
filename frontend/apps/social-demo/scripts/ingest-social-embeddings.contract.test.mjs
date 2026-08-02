import assert from "node:assert/strict";
import test from "node:test";
import { ingestEmbeddingsBatch } from "./ingest-social-embeddings.mjs";

test("embedding ingestion consumes receipt-derived source IDs without a GQL lookup", async () => {
  const calls = [];
  const actorFactory = async () => ({
    async ingest_vertex_embeddings(request) {
      calls.push(request);
      return { Ok: [{ Ok: null }] };
    },
  });
  const id = Uint8Array.from([1, 2, 3, 4, 5, 6, 7, 8]);
  await ingestEmbeddingsBatch(
    new Map([["post-source", id]]),
    { "post-source": { dims: 2, values: [0.25, -0.5] } },
    actorFactory,
  );
  assert.deepEqual(calls, [{
    logical_graph_name: "gleaph.pocket_ic",
    embedding_name: "post_vec",
    items: [{ encoded_vertex_id: id, values: [0.25, -0.5] }],
  }]);
});

test("embedding ingestion fails before calling Router without a replay-proven ID", async () => {
  let called = false;
  await assert.rejects(
    ingestEmbeddingsBatch(new Map(), { post: { dims: 1, values: [1] } }, async () => {
      called = true;
      return {};
    }),
    /No replay-proven element_id/,
  );
  assert.equal(called, false);
});
