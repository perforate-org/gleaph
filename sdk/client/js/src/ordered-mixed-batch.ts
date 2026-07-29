import { encodeCanonicalGqlValue } from "./canonical-value.ts";
import type { ApiValue } from "./types";
import type { CandidOption, OrderedEdgePropertyPublic } from "./ordered-edge-batch";

const MAX_ITEMS = 1024;
const MAX_CLIENT_MUTATION_KEY_BYTES = 256;
const MAX_INLINE_PROPERTY_BYTES = 0xffff;
const utf8 = new TextEncoder();

export type OrderedMixedBatchPublicRequest = {
  V1: OrderedMixedBatchPublicRequestV1;
};

export interface OrderedMixedBatchPublicRequestV1 {
  client_mutation_key: string;
  logical_graph_name: string;
  target_shard_id: number;
  operations: OrderedMixedBatchOperation[];
}

export type OrderedMixedBatchOperation =
  | { Vertex: OrderedVertexInsertPublicItem }
  | { Edge: OrderedMixedEdgeInsertPublicItem };

export interface OrderedVertexInsertPublicItem {
  vertex_labels: string[];
  initial_properties: OrderedEdgePropertyPublic[];
}

export interface OrderedMixedEdgeInsertPublicItem {
  source: OrderedMixedEndpoint;
  target: OrderedMixedEndpoint;
  directed: boolean;
  edge_label_name: CandidOption<string>;
  inline_property: CandidOption<Uint8Array>;
  initial_edge_properties: OrderedEdgePropertyPublic[];
}

export type OrderedMixedEndpoint = { Existing: Uint8Array } | { NewVertexOrdinal: number };

export interface OrderedMixedBatchPublicRequestInput {
  client_mutation_key: string;
  logical_graph_name: string;
  target_shard_id: number;
  operations: OrderedMixedBatchOperationInput[];
}

export type OrderedMixedBatchOperationInput =
  | { vertex: OrderedVertexInsertPublicItemInput }
  | { edge: OrderedMixedEdgeInsertPublicItemInput };

export interface OrderedVertexInsertPublicItemInput {
  vertex_labels?: string[];
  initial_properties?: Record<string, ApiValue>;
}

export interface OrderedMixedEdgeInsertPublicItemInput {
  source: OrderedMixedEndpointInput;
  target: OrderedMixedEndpointInput;
  directed: boolean;
  edge_label_name?: string;
  inline_property?: ApiValue;
  initial_edge_properties?: Record<string, ApiValue>;
}

export type OrderedMixedEndpointInput = { existing: Uint8Array } | { new_vertex_ordinal: number };

function sortUtf8(left: string, right: string): number {
  const leftBytes = utf8.encode(left);
  const rightBytes = utf8.encode(right);
  const length = Math.min(leftBytes.length, rightBytes.length);
  for (let index = 0; index < length; index += 1) {
    if (leftBytes[index] !== rightBytes[index]) return leftBytes[index] - rightBytes[index];
  }
  return leftBytes.length - rightBytes.length;
}

function option<T>(value: T | undefined): CandidOption<T> {
  return value === undefined ? [] : [value];
}

function properties(values: Record<string, ApiValue> | undefined): OrderedEdgePropertyPublic[] {
  const entries = Object.entries(values ?? {});
  for (const [name] of entries) {
    if (name.length === 0) throw new Error("property_name must not be empty");
  }
  entries.sort(([left], [right]) => sortUtf8(left, right));
  return entries.map(([property_name, value]) => ({
    property_name,
    value: encodeCanonicalGqlValue(value),
  }));
}

function endpoint(value: OrderedMixedEndpointInput, ordinal: number): OrderedMixedEndpoint {
  if ("existing" in value) {
    if (value.existing.byteLength !== 8) {
      throw new Error(`operations[${ordinal}] endpoint must be exactly 8 bytes`);
    }
    return { Existing: value.existing };
  }
  if (!Number.isInteger(value.new_vertex_ordinal) || value.new_vertex_ordinal < 0) {
    throw new Error(`operations[${ordinal}] new_vertex_ordinal must be a non-negative integer`);
  }
  return { NewVertexOrdinal: value.new_vertex_ordinal };
}

/** Build the Candid-shaped Router request for the ADR 0049 mixed public batch. */
export function makeOrderedMixedBatchPublicRequest(
  input: OrderedMixedBatchPublicRequestInput,
): OrderedMixedBatchPublicRequest {
  const keyBytes = utf8.encode(input.client_mutation_key);
  if (keyBytes.byteLength === 0 || keyBytes.byteLength > MAX_CLIENT_MUTATION_KEY_BYTES) {
    throw new Error("client_mutation_key must be 1..=256 bytes");
  }
  if (input.logical_graph_name.length === 0) {
    throw new Error("logical_graph_name must not be empty");
  }
  if (!Number.isInteger(input.target_shard_id) || input.target_shard_id < 0) {
    throw new Error("target_shard_id must be a non-negative integer");
  }
  if (input.operations.length === 0 || input.operations.length > MAX_ITEMS) {
    throw new Error("operations must contain 1..=1024 entries");
  }
  if (!input.operations.some((operation) => "vertex" in operation)) {
    throw new Error("operations must contain at least one vertex");
  }
  if (!input.operations.some((operation) => "edge" in operation)) {
    throw new Error("operations must contain at least one edge");
  }

  return {
    V1: {
      client_mutation_key: input.client_mutation_key,
      logical_graph_name: input.logical_graph_name,
      target_shard_id: input.target_shard_id,
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
            source: endpoint(item.source, ordinal),
            target: endpoint(item.target, ordinal),
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
