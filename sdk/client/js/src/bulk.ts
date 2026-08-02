import type { AtomicInsertProperty, AtomicInsertVertex } from "./atomic";

const ENCODED_VERTEX_ID_BYTES = 8;
const MAX_ATOMIC_INSERT_OPERATIONS = 1024;
const MAX_CLIENT_KEY_BYTES = 256;
const MAX_BULK_LOAD_RECEIPTS_PER_PAGE = 64;
const MAX_INLINE_PROPERTY_BYTES = 0xffff;
const utf8 = new TextEncoder();

export type BulkLoadEdge = {
  source: Uint8Array;
  target: Uint8Array;
  directed: boolean;
  edge_label_name: [] | [string];
  inline_property: [] | [Uint8Array];
  initial_edge_properties: AtomicInsertProperty[];
};

export type BulkLoadChunk = { Vertices: AtomicInsertVertex[] } | { Edges: BulkLoadEdge[] };

export type BulkLoadCommand =
  | { Start: { logical_graph_name: string; client_bulk_key: string } }
  | {
      Append: {
        logical_graph_name: string;
        client_bulk_key: string;
        chunk_index: number;
        chunk: BulkLoadChunk;
      };
    }
  | { Finalize: { logical_graph_name: string; client_bulk_key: string } }
  | { Abort: { logical_graph_name: string; client_bulk_key: string } };

export type AtomicInsertReceipt = {
  logical_operation_count: bigint;
  logical_vertex_count: bigint;
  logical_edge_count: bigint;
  allocated_vertex_ids: Uint8Array[];
};

export type BulkLoadResponse =
  | { Started: { next_chunk_index: number } }
  | { Appended: { chunk_index: number; receipt: AtomicInsertReceipt } }
  | { FinalizeAccepted: { state: BulkLoadPublicState } }
  | { AbortAccepted: { state: BulkLoadPublicState } };

export type BulkLoadPublicState =
  | { Open: null }
  | { AppendPending: null }
  | { FinalizePending: null }
  | { AbortPending: null }
  | { Completed: null }
  | { Aborted: null }
  | { Failed: { reason: string } };

export type BulkLoadChunkReceipt = {
  chunk_index: number;
  receipt: AtomicInsertReceipt;
};

export type BulkLoadStatusPage = {
  state: BulkLoadPublicState;
  next_chunk_index: number;
  committed_chunk_count: number;
  completed_chunk_count: number;
  terminal_at_ns: [] | [bigint];
  expires_at_ns: [] | [bigint];
  receipts: BulkLoadChunkReceipt[];
  next_receipt_cursor: [] | [number];
};

export type BulkLoadStatusRequest = {
  logical_graph_name: string;
  client_bulk_key: string;
  receipt_cursor?: number;
  max_receipts: number;
};

function validateIdentity(logicalGraphName: string, clientBulkKey: string): void {
  const graphBytes = utf8.encode(logicalGraphName).byteLength;
  const keyBytes = utf8.encode(clientBulkKey).byteLength;
  if (graphBytes === 0 || graphBytes > MAX_CLIENT_KEY_BYTES) {
    throw new Error("logical_graph_name must be 1..=256 UTF-8 bytes");
  }
  if (keyBytes === 0 || keyBytes > MAX_CLIENT_KEY_BYTES) {
    throw new Error("client_bulk_key must be 1..=256 UTF-8 bytes");
  }
}

function validateChunk(chunk: BulkLoadChunk): void {
  const count = "Vertices" in chunk ? chunk.Vertices.length : chunk.Edges.length;
  if (count === 0 || count > MAX_ATOMIC_INSERT_OPERATIONS) {
    throw new Error("bulk-load chunk must contain 1..=1024 operations");
  }
  if ("Vertices" in chunk) {
    for (const [index, vertex] of chunk.Vertices.entries()) {
      if (vertex.vertex_labels.some((label) => label.length === 0)) {
        throw new Error(`chunk.Vertices[${index}] contains an empty vertex label`);
      }
      const propertyNames = new Set<string>();
      for (const property of vertex.initial_properties) {
        if (property.property_name.length === 0 || propertyNames.has(property.property_name)) {
          throw new Error(`chunk.Vertices[${index}] contains a duplicate or empty property name`);
        }
        propertyNames.add(property.property_name);
      }
    }
    return;
  }
  for (const [index, edge] of chunk.Edges.entries()) {
    if (
      edge.source.byteLength !== ENCODED_VERTEX_ID_BYTES ||
      edge.target.byteLength !== ENCODED_VERTEX_ID_BYTES
    ) {
      throw new Error(
        `chunk.Edges[${index}] endpoints must be exactly ${ENCODED_VERTEX_ID_BYTES} bytes`,
      );
    }
    if (edge.edge_label_name.length === 1 && edge.edge_label_name[0] === "") {
      throw new Error(`chunk.Edges[${index}] edge_label_name must not be empty`);
    }
    if (
      edge.inline_property.length === 1 &&
      edge.inline_property[0].byteLength > MAX_INLINE_PROPERTY_BYTES
    ) {
      throw new Error(`chunk.Edges[${index}] inline_property exceeds 65535 bytes`);
    }
    const propertyNames = new Set<string>();
    for (const property of edge.initial_edge_properties) {
      if (property.property_name.length === 0 || propertyNames.has(property.property_name)) {
        throw new Error(`chunk.Edges[${index}] contains a duplicate or empty property name`);
      }
      propertyNames.add(property.property_name);
    }
  }
}

export function makeBulkLoadStartCommand(input: {
  logical_graph_name: string;
  client_bulk_key: string;
}): BulkLoadCommand {
  validateIdentity(input.logical_graph_name, input.client_bulk_key);
  return { Start: { ...input } };
}

export function makeBulkLoadAppendCommand(input: {
  logical_graph_name: string;
  client_bulk_key: string;
  chunk_index: number;
  chunk: BulkLoadChunk;
}): BulkLoadCommand {
  validateIdentity(input.logical_graph_name, input.client_bulk_key);
  if (!Number.isInteger(input.chunk_index) || input.chunk_index < 0) {
    throw new Error("chunk_index must be a non-negative integer");
  }
  validateChunk(input.chunk);
  return { Append: { ...input } };
}

export function makeBulkLoadFinalizeCommand(input: {
  logical_graph_name: string;
  client_bulk_key: string;
}): BulkLoadCommand {
  validateIdentity(input.logical_graph_name, input.client_bulk_key);
  return { Finalize: { ...input } };
}

export function makeBulkLoadAbortCommand(input: {
  logical_graph_name: string;
  client_bulk_key: string;
}): BulkLoadCommand {
  validateIdentity(input.logical_graph_name, input.client_bulk_key);
  return { Abort: { ...input } };
}

export type BulkLoadCommandInput =
  | ({ kind: "Start" } & Parameters<typeof makeBulkLoadStartCommand>[0])
  | ({ kind: "Append" } & Parameters<typeof makeBulkLoadAppendCommand>[0])
  | ({ kind: "Finalize" } & Parameters<typeof makeBulkLoadFinalizeCommand>[0])
  | ({ kind: "Abort" } & Parameters<typeof makeBulkLoadAbortCommand>[0]);

export function makeBulkLoadCommand(input: BulkLoadCommandInput): BulkLoadCommand {
  switch (input.kind) {
    case "Start":
      return makeBulkLoadStartCommand(input);
    case "Append":
      return makeBulkLoadAppendCommand(input);
    case "Finalize":
      return makeBulkLoadFinalizeCommand(input);
    case "Abort":
      return makeBulkLoadAbortCommand(input);
  }
}

export function makeBulkLoadStatusRequest(input: BulkLoadStatusRequest): BulkLoadStatusRequest {
  validateIdentity(input.logical_graph_name, input.client_bulk_key);
  if (
    !Number.isInteger(input.max_receipts) ||
    input.max_receipts < 1 ||
    input.max_receipts > MAX_BULK_LOAD_RECEIPTS_PER_PAGE
  ) {
    throw new Error("max_receipts must be in 1..=64");
  }
  if (
    input.receipt_cursor !== undefined &&
    (!Number.isInteger(input.receipt_cursor) || input.receipt_cursor < 0)
  ) {
    throw new Error("receipt_cursor must be a non-negative integer when provided");
  }
  return { ...input };
}
