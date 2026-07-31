import { encodeCanonicalGqlValue } from "./canonical-value.ts";
import type { ApiValue } from "./types";

const ENCODED_VERTEX_ID_BYTES = 8;
const MAX_BATCH_OPERATIONS = 1024;
const MAX_CLIENT_MUTATION_KEY_BYTES = 256;
const MAX_INLINE_PROPERTY_BYTES = 0xffff;
const utf8 = new TextEncoder();

export type CandidOption<T> = [] | [T];

export type BatchRequest = { V1: BatchRequestV1 };

export interface BatchRequestV1 {
  client_mutation_key: string;
  logical_graph_name: string;
  operations: BatchOperation[];
}

export type BatchOperation = { Vertex: BatchVertexInsert } | { Edge: BatchEdgeInsert };

export interface BatchVertexInsert {
  vertex_labels: string[];
  initial_properties: BatchProperty[];
}

export interface BatchEdgeInsert {
  source: BatchEndpoint;
  target: BatchEndpoint;
  directed: boolean;
  edge_label_name: CandidOption<string>;
  inline_property: CandidOption<Uint8Array>;
  initial_edge_properties: BatchProperty[];
}

export type BatchEndpoint = { Existing: Uint8Array } | { NewVertexOrdinal: number };

export interface BatchProperty {
  property_name: string;
  value: Uint8Array;
}

export interface BatchRequestInput {
  client_mutation_key: string;
  logical_graph_name: string;
  operations: BatchOperationInput[];
}

export type BatchOperationInput =
  | { vertex: BatchVertexInsertInput }
  | { edge: BatchEdgeInsertInput };

export interface BatchVertexInsertInput {
  vertex_labels?: string[];
  initial_properties?: Record<string, ApiValue>;
}

export interface BatchEdgeInsertInput {
  source: BatchEndpointInput;
  target: BatchEndpointInput;
  directed: boolean;
  edge_label_name?: string;
  inline_property?: ApiValue;
  initial_edge_properties?: Record<string, ApiValue>;
}

export type BatchEndpointInput = { existing: Uint8Array } | { new_vertex_ordinal: number };

function sortUtf8(left: string, right: string): number {
  const leftBytes = utf8.encode(left);
  const rightBytes = utf8.encode(right);
  const length = Math.min(leftBytes.length, rightBytes.length);
  for (let index = 0; index < length; index += 1) {
    if (leftBytes[index] !== rightBytes[index]) return leftBytes[index]! - rightBytes[index]!;
  }
  return leftBytes.length - rightBytes.length;
}

function option<T>(value: T | undefined): CandidOption<T> {
  return value === undefined ? [] : [value];
}

function properties(values: Record<string, ApiValue> | undefined): BatchProperty[] {
  const entries = Object.entries(values ?? {});
  if (entries.some(([name]) => name.length === 0)) {
    throw new Error("property_name must not be empty");
  }
  entries.sort(([left], [right]) => sortUtf8(left, right));
  return entries.map(([property_name, value]) => ({
    property_name,
    value: encodeCanonicalGqlValue(value),
  }));
}

function endpoint(value: BatchEndpointInput, ordinal: number, vertexCount: number): BatchEndpoint {
  if ("existing" in value) {
    if (value.existing.byteLength !== ENCODED_VERTEX_ID_BYTES) {
      throw new Error(
        `operations[${ordinal}] endpoint must be exactly ${ENCODED_VERTEX_ID_BYTES} bytes`,
      );
    }
    return { Existing: value.existing };
  }
  if (
    !Number.isInteger(value.new_vertex_ordinal) ||
    value.new_vertex_ordinal < 0 ||
    value.new_vertex_ordinal >= vertexCount
  ) {
    throw new Error(`operations[${ordinal}] new_vertex_ordinal is out of range`);
  }
  return { NewVertexOrdinal: value.new_vertex_ordinal };
}

/** Build the Candid-shaped request accepted by the Router `batch` update method. */
export function makeBatchRequest(input: BatchRequestInput): BatchRequest {
  const keyBytes = utf8.encode(input.client_mutation_key);
  if (keyBytes.byteLength === 0 || keyBytes.byteLength > MAX_CLIENT_MUTATION_KEY_BYTES) {
    throw new Error("client_mutation_key must be 1..=256 bytes");
  }
  if (input.logical_graph_name.length === 0) {
    throw new Error("logical_graph_name must not be empty");
  }
  if (input.operations.length === 0 || input.operations.length > MAX_BATCH_OPERATIONS) {
    throw new Error("operations must contain 1..=1024 entries");
  }

  const vertexCount = input.operations.filter((operation) => "vertex" in operation).length;
  return {
    V1: {
      client_mutation_key: input.client_mutation_key,
      logical_graph_name: input.logical_graph_name,
      operations: input.operations.map((operation, ordinal) => {
        if ("vertex" in operation) {
          const labels = [...(operation.vertex.vertex_labels ?? [])];
          if (labels.some((label) => label.length === 0)) {
            throw new Error(`operations[${ordinal}].vertex_labels must not contain empty names`);
          }
          labels.sort(sortUtf8);
          return {
            Vertex: {
              vertex_labels: labels,
              initial_properties: properties(operation.vertex.initial_properties),
            },
          };
        }

        const item = operation.edge;
        if (item.edge_label_name === "") {
          throw new Error(`operations[${ordinal}].edge_label_name must not be empty`);
        }
        const inline =
          item.inline_property === undefined
            ? undefined
            : encodeCanonicalGqlValue(item.inline_property);
        if (inline !== undefined && inline.byteLength > MAX_INLINE_PROPERTY_BYTES) {
          throw new Error(`operations[${ordinal}].inline_property exceeds 65535 bytes`);
        }
        return {
          Edge: {
            source: endpoint(item.source, ordinal, vertexCount),
            target: endpoint(item.target, ordinal, vertexCount),
            directed: item.directed,
            edge_label_name: option(item.edge_label_name),
            inline_property: option(inline),
            initial_edge_properties: properties(item.initial_edge_properties),
          },
        };
      }),
    },
  };
}
