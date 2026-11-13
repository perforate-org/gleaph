/**
 * Optional text-embedding provider boundary (ADR 0120).
 *
 * The core SDK never bundles, downloads or initialises a model. Applications implement
 * `EmbeddingProvider` with an HTTP sidecar, Transformers.js/ONNX, a hosted API, or their own
 * backend, then pass the encoded bytes to a prepared operation.
 */

/** Version of the public embedding-contract shape. */
export const EMBEDDING_CONTRACT_VERSION = 1;

/** Identity and preprocessing of one embedding space. */
export interface EmbeddingContract {
  readonly contractVersion: number;
  readonly model: string;
  readonly modelRevision?: string;
  readonly dimensions: number;
  readonly queryPrefix: string;
  readonly passagePrefix: string;
  readonly pooling: string;
  readonly normalized: boolean;
  readonly similarity: string;
  readonly maxInputTokens?: number;
}

/** One optional text-embedding backend. Loading is implementation-specific and never implicit. */
export interface EmbeddingProvider {
  readonly contract: EmbeddingContract;
  embedQuery(text: string): Promise<Float32Array> | Float32Array;
  embedPassage(text: string): Promise<Float32Array> | Float32Array;
}

/** Contract or vector validation failure. */
export class EmbeddingContractError extends Error {
  readonly code: "invalid-contract" | "dimension-mismatch";

  constructor(code: "invalid-contract" | "dimension-mismatch", message: string) {
    super(message);
    this.name = "EmbeddingContractError";
    this.code = code;
  }
}

/** Validate the structural invariants of an embedding contract. */
export function validateEmbeddingContract(contract: EmbeddingContract): void {
  if (contract.contractVersion !== EMBEDDING_CONTRACT_VERSION) {
    throw new EmbeddingContractError(
      "invalid-contract",
      `unsupported embedding contract version ${contract.contractVersion}`,
    );
  }
  if (contract.model.trim() === "") {
    throw new EmbeddingContractError("invalid-contract", "model must not be empty");
  }
  if (!Number.isInteger(contract.dimensions) || contract.dimensions <= 0) {
    throw new EmbeddingContractError("invalid-contract", "dimensions must be positive");
  }
  if (contract.pooling.trim() === "") {
    throw new EmbeddingContractError("invalid-contract", "pooling must not be empty");
  }
  if (contract.similarity.trim() === "") {
    throw new EmbeddingContractError("invalid-contract", "similarity must not be empty");
  }
  if (contract.maxInputTokens !== undefined && contract.maxInputTokens <= 0) {
    throw new EmbeddingContractError("invalid-contract", "maxInputTokens must be positive");
  }
}

/** Validate that `values` fits this contract. */
export function validateEmbeddingVector(
  contract: EmbeddingContract,
  values: Float32Array | ArrayLike<number>,
): void {
  validateEmbeddingContract(contract);
  if (values.length !== contract.dimensions) {
    throw new EmbeddingContractError(
      "dimension-mismatch",
      `embedding dimension mismatch: expected ${contract.dimensions}, got ${values.length}`,
    );
  }
}

/** Encode one vector as little-endian f32 bytes, the Router's `Value::Bytes` form. */
export function encodeEmbeddingVector(
  contract: EmbeddingContract,
  values: Float32Array | ArrayLike<number>,
): Uint8Array {
  validateEmbeddingVector(contract, values);
  const bytes = new Uint8Array(values.length * 4);
  const view = new DataView(bytes.buffer);
  for (let index = 0; index < values.length; index += 1) {
    view.setFloat32(index * 4, values[index] ?? 0, true);
  }
  return bytes;
}
