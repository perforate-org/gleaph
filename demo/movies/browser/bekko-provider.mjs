/**
 * Optional browser embedding provider for the movie demo.
 *
 * This module is loaded only after an explicit user action. Importing it does not fetch a model;
 * `load()` dynamically imports Transformers.js and the pinned Bekko ONNX graph.
 */
import { EMBEDDING_CONTRACT_VERSION, encodeEmbeddingVector } from "./vendor/gleaph-sdk.mjs";

export const MODEL = "hotchpotch/bekko-embedding-v1-a8m";
export const MODEL_REVISION = "c721113d59a1d91b447450324f51c4b3332c924a";
export const CONTRACT = Object.freeze({
  contractVersion: EMBEDDING_CONTRACT_VERSION,
  model: MODEL,
  modelRevision: MODEL_REVISION,
  dimensions: 384,
  queryPrefix: "",
  passagePrefix: "",
  pooling: "mean",
  normalized: true,
  similarity: "cosine",
  maxInputTokens: 512,
});

const TRANSFORMERS_URL = "https://cdn.jsdelivr.net/npm/@huggingface/transformers@4.3.2";
let extractor = null;
let loading = null;

export function isLoaded() {
  return extractor !== null;
}

export function encodeQuery(values) {
  return encodeEmbeddingVector(CONTRACT, values);
}

export async function load({ onProgress } = {}) {
  if (extractor) return;
  if (loading) return loading;
  loading = (async () => {
    onProgress?.({ status: "import", message: "Loading Transformers.js…" });
    const { pipeline } = await import(TRANSFORMERS_URL);
    onProgress?.({ status: "model", message: "Downloading Bekko ONNX (~130 MB)…" });
    extractor = await pipeline("feature-extraction", MODEL, {
      dtype: "fp32",
      revision: MODEL_REVISION,
      progress_callback: (progress) => onProgress?.(progress),
    });
    onProgress?.({ status: "ready", message: "Bekko is ready." });
  })().catch((error) => {
    loading = null;
    extractor = null;
    throw error;
  });
  return loading;
}

export async function embedQuery(text) {
  if (!extractor) throw new Error("Bekko is not loaded");
  const result = await extractor([text], { pooling: "mean", normalize: true });
  return Float32Array.from(result.data);
}
