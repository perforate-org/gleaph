import { encodeCanonicalGqlValue } from "./canonical-value.ts";
import type { ApiValue } from "./types";
import type { CandidOption, OrderedEdgePropertyPublic } from "./ordered-edge-batch";

const MAX_ITEMS = 1024;
const MAX_CLIENT_MUTATION_KEY_BYTES = 256;
const utf8 = new TextEncoder();

export interface OrderedVertexBatchPublicRequest {
  V1: OrderedVertexBatchPublicRequestV1;
}

export interface OrderedVertexBatchPublicRequestV1 {
  client_mutation_key: string;
  logical_graph_name: string;
  target_shard_id: number;
  items: OrderedVertexInsertPublicItem[];
}

export interface OrderedVertexInsertPublicItem {
  vertex_labels: string[];
  initial_properties: OrderedEdgePropertyPublic[];
}

export interface OrderedVertexInsertPublicItemInput {
  vertex_labels?: string[];
  initial_properties?: Record<string, ApiValue>;
}

export interface OrderedVertexBatchPublicRequestInput {
  client_mutation_key: string;
  logical_graph_name: string;
  target_shard_id: number;
  items: OrderedVertexInsertPublicItemInput[];
}

function sortUtf8(left: string, right: string): number {
  const leftBytes = utf8.encode(left);
  const rightBytes = utf8.encode(right);
  const length = Math.min(leftBytes.length, rightBytes.length);
  for (let index = 0; index < length; index += 1) {
    if (leftBytes[index] !== rightBytes[index]) return leftBytes[index] - rightBytes[index];
  }
  return leftBytes.length - rightBytes.length;
}

/** Build the Candid-shaped Router request for the ADR 0049 public vertex batch. */
export function makeOrderedVertexBatchPublicRequest(
  input: OrderedVertexBatchPublicRequestInput,
): OrderedVertexBatchPublicRequest {
  const keyBytes = utf8.encode(input.client_mutation_key);
  if (keyBytes.byteLength === 0 || keyBytes.byteLength > MAX_CLIENT_MUTATION_KEY_BYTES) {
    throw new Error("client_mutation_key must be 1..=256 bytes");
  }
  if (input.logical_graph_name.length === 0) {
    throw new Error("logical_graph_name must not be empty");
  }
  if (
    !Number.isInteger(input.target_shard_id) ||
    input.target_shard_id < 0 ||
    input.target_shard_id > 0xffff_ffff
  ) {
    throw new Error("target_shard_id must be a uint32");
  }
  if (input.items.length === 0 || input.items.length > MAX_ITEMS) {
    throw new Error("items must contain 1..=1024 entries");
  }

  return {
    V1: {
      client_mutation_key: input.client_mutation_key,
      logical_graph_name: input.logical_graph_name,
      target_shard_id: input.target_shard_id,
      items: input.items.map((item, ordinal) => {
        const labels = [...(item.vertex_labels ?? [])];
        for (const label of labels) {
          if (label.length === 0) {
            throw new Error(`items[${ordinal}].vertex_labels must not contain empty names`);
          }
        }
        labels.sort(sortUtf8);
        const properties = Object.entries(item.initial_properties ?? {});
        for (const [property_name] of properties) {
          if (property_name.length === 0) {
            throw new Error(`items[${ordinal}].property_name must not be empty`);
          }
        }
        properties.sort(([left], [right]) => sortUtf8(left, right));
        return {
          vertex_labels: labels,
          initial_properties: properties.map(([property_name, value]) => ({
            property_name,
            value: encodeCanonicalGqlValue(value),
          })),
        };
      }),
    },
  };
}

// Keep the imported option type in the generated public type surface for consumers that use the
// shared property representation, without manufacturing a second Candid option definition.
export type { CandidOption };
