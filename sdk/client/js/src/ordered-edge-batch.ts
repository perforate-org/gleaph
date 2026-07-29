import { encodeCanonicalGqlValue } from "./canonical-value.ts";
import type { ApiValue } from "./types";

const ENCODED_VERTEX_ID_BYTES = 8;
const MAX_ITEMS = 1024;
const MAX_CLIENT_MUTATION_KEY_BYTES = 256;
const MAX_INLINE_PROPERTY_BYTES = 0xffff;

export type CandidOption<T> = [] | [T];

export interface OrderedEdgeBatchPublicRequest {
  V1: OrderedEdgeBatchPublicRequestV1;
}

export interface OrderedEdgeBatchPublicRequestV1 {
  client_mutation_key: string;
  logical_graph_name: string;
  items: OrderedEdgeInsertPublicItem[];
}

export interface OrderedEdgeInsertPublicItem {
  source: Uint8Array;
  target: Uint8Array;
  directed: boolean;
  edge_label_name: CandidOption<string>;
  inline_property: CandidOption<Uint8Array>;
  initial_edge_properties: OrderedEdgePropertyPublic[];
}

export interface OrderedEdgePropertyPublic {
  property_name: string;
  value: Uint8Array;
}

export interface OrderedEdgeBatchPublicRequestInput {
  client_mutation_key: string;
  logical_graph_name: string;
  items: OrderedEdgeInsertPublicItemInput[];
}

export interface OrderedEdgeInsertPublicItemInput {
  source: Uint8Array;
  target: Uint8Array;
  directed: boolean;
  edge_label_name?: string;
  inline_property?: ApiValue;
  initial_edge_properties?: Record<string, ApiValue>;
}

const utf8 = new TextEncoder();

function assertEndpoint(name: string, value: Uint8Array): void {
  if (value.byteLength !== ENCODED_VERTEX_ID_BYTES) {
    throw new Error(`${name} must be exactly ${ENCODED_VERTEX_ID_BYTES} bytes`);
  }
}

function option<T>(value: T | undefined): CandidOption<T> {
  return value === undefined ? [] : [value];
}

/** Build the Candid-shaped Router request for the ADR 0049 public edge batch. */
export function makeOrderedEdgeBatchPublicRequest(
  input: OrderedEdgeBatchPublicRequestInput,
): OrderedEdgeBatchPublicRequest {
  if (utf8.encode(input.client_mutation_key).byteLength === 0) {
    throw new Error("client_mutation_key must not be empty");
  }
  if (utf8.encode(input.client_mutation_key).byteLength > MAX_CLIENT_MUTATION_KEY_BYTES) {
    throw new Error("client_mutation_key exceeds 256 bytes");
  }
  if (input.logical_graph_name.length === 0) {
    throw new Error("logical_graph_name must not be empty");
  }
  if (input.items.length === 0 || input.items.length > MAX_ITEMS) {
    throw new Error("items must contain 1..=1024 entries");
  }

  return {
    V1: {
      client_mutation_key: input.client_mutation_key,
      logical_graph_name: input.logical_graph_name,
      items: input.items.map((item, ordinal) => {
        assertEndpoint(`items[${ordinal}].source`, item.source);
        assertEndpoint(`items[${ordinal}].target`, item.target);
        if (item.edge_label_name === "") {
          throw new Error(`items[${ordinal}].edge_label_name must not be empty`);
        }
        const inline =
          item.inline_property === undefined
            ? undefined
            : encodeCanonicalGqlValue(item.inline_property);
        if (inline !== undefined && inline.byteLength > MAX_INLINE_PROPERTY_BYTES) {
          throw new Error(`items[${ordinal}].inline_property exceeds 65535 bytes`);
        }
        const properties = Object.entries(item.initial_edge_properties ?? {});
        for (const [property_name] of properties) {
          if (property_name.length === 0) {
            throw new Error(`items[${ordinal}].property_name must not be empty`);
          }
        }
        properties.sort(([left], [right]) => {
          const leftBytes = utf8.encode(left);
          const rightBytes = utf8.encode(right);
          const length = Math.min(leftBytes.length, rightBytes.length);
          for (let index = 0; index < length; index += 1) {
            if (leftBytes[index] !== rightBytes[index]) {
              return leftBytes[index] - rightBytes[index];
            }
          }
          return leftBytes.length - rightBytes.length;
        });
        return {
          source: item.source,
          target: item.target,
          directed: item.directed,
          edge_label_name: option(item.edge_label_name),
          inline_property: option(inline),
          initial_edge_properties: properties.map(([property_name, value]) => ({
            property_name,
            value: encodeCanonicalGqlValue(value),
          })),
        };
      }),
    },
  };
}
