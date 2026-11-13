import assert from "node:assert/strict";
import {
  EMBEDDING_CONTRACT_VERSION,
  EmbeddingContractError,
  encodeEmbeddingVector,
  validateEmbeddingContract,
  validateEmbeddingVector,
} from "../src/embedding.ts";

const contract = {
  contractVersion: EMBEDDING_CONTRACT_VERSION,
  model: "test/model",
  modelRevision: "rev-1",
  dimensions: 2,
  queryPrefix: "query: ",
  passagePrefix: "passage: ",
  pooling: "mean",
  normalized: true,
  similarity: "cosine",
  maxInputTokens: 512,
};

validateEmbeddingContract(contract);
validateEmbeddingVector(contract, [1, -0.5]);
assert.throws(
  () => validateEmbeddingVector(contract, [1]),
  (error) =>
    error instanceof EmbeddingContractError && error.code === "dimension-mismatch",
);

const bytes = encodeEmbeddingVector(contract, [1, -0.5]);
const view = new DataView(bytes.buffer);
assert.equal(bytes.length, 8);
assert.equal(view.getFloat32(0, true), 1);
assert.equal(view.getFloat32(4, true), -0.5);

assert.throws(
  () => validateEmbeddingContract({ ...contract, contractVersion: 999 }),
  (error) => error instanceof EmbeddingContractError && error.code === "invalid-contract",
);

console.log("embedding contract ok");
