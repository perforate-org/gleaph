import { Actor, HttpAgent, type ActorSubclass } from "@icp-sdk/core/agent";
import { safeGetCanisterEnv } from "@icp-sdk/core/agent/canister-env";
import { IDL } from "@icp-sdk/core/candid";
import { Principal } from "@icp-sdk/core/principal";

import { IcWirePlanQueryResult, routerIdlFactory } from "~/api/routerIdl";
import type { RouterKnowledgeMapResponse, RouterKnowledgeMapRow } from "~/api/viewModelAdapter";

const KNOWLEDGE_MAP_QUERY =
  "MATCH (a)-[e:KNOWS {weight: 5}]->(b) " +
  "RETURN ELEMENT_ID(a) AS source_id, ELEMENT_ID(e) AS edge_id, " +
  "ELEMENT_ID(b) AS target_id, e.weight AS edge_weight";

type Result<T, E> = { Ok: T; Err?: never } | { Ok?: never; Err: E };
type ActorInterfaceFactory = Parameters<typeof Actor.createActor>[0];

type GqlQueryResult = {
  row_count: bigint;
  rows_blob: [] | [number[] | Uint8Array];
};

type RouterActorMethods = {
  gql_query(query: string, params: number[] | Uint8Array): Promise<Result<GqlQueryResult, unknown>>;
};

type RouterActor = ActorSubclass<RouterActorMethods>;

export type LiveRouterKnowledgeMapOptions = {
  canisterId: string;
  host: string;
  fetchRootKey: boolean;
  rootKey?: Uint8Array;
};

type WireValue =
  | { Int64: bigint }
  | { Uint64: bigint }
  | { Text: string }
  | { Bytes: number[] | Uint8Array };

type WireRow = {
  columns: [string, WireValue][];
};

type WireResult = {
  rows: WireRow[];
};

export const getLiveRouterKnowledgeMapOptions = ():
  | LiveRouterKnowledgeMapOptions
  | undefined => {
  const canisterEnv = safeGetCanisterEnv<{
    readonly "PUBLIC_CANISTER_ID:gleaph-router"?: string;
    readonly "PUBLIC_CANISTER_ID:router"?: string;
  }>();
  const canisterId =
    canisterEnv?.["PUBLIC_CANISTER_ID:gleaph-router"] ??
    canisterEnv?.["PUBLIC_CANISTER_ID:router"] ??
    (import.meta.env.VITE_ROUTER_CANISTER_ID as string | undefined);
  if (!canisterId) {
    return undefined;
  }

  return {
    canisterId,
    host:
      (import.meta.env.VITE_IC_HOST as string | undefined) ??
      (canisterEnv ? globalThis.location.origin : "https://icp-api.io"),
    fetchRootKey: canisterEnv ? false : import.meta.env.VITE_FETCH_ROOT_KEY === "true",
    rootKey: canisterEnv?.IC_ROOT_KEY,
  };
};

export const fetchKnowledgeMapFromRouter = async (
  scenarioId: string,
  options: LiveRouterKnowledgeMapOptions,
): Promise<RouterKnowledgeMapResponse> => {
  const actor = await createRouterActor(options);
  const result = await actor.gql_query(KNOWLEDGE_MAP_QUERY, []);
  if ("Err" in result) {
    throw new Error(`Router gql_query failed: ${formatRouterError(result.Err)}`);
  }

  const rowsBlob = result.Ok.rows_blob[0];
  if (!rowsBlob) {
    throw new Error("Router gql_query returned no rows_blob for knowledge-map query.");
  }

  const wire = decodeWireRows(toUint8Array(rowsBlob));
  const row = wire.rows[0];
  if (!row) {
    throw new Error("Router gql_query returned no relationship rows for knowledge-map query.");
  }

  return relationshipRowToKnowledgeMapResponse(scenarioId, row);
};

const createRouterActor = async (
  options: LiveRouterKnowledgeMapOptions,
): Promise<RouterActor> => {
  const agent = HttpAgent.createSync({ host: options.host, rootKey: options.rootKey });
  if (options.fetchRootKey) {
    await agent.fetchRootKey();
  }

  return Actor.createActor<RouterActorMethods>(
    routerIdlFactory as unknown as ActorInterfaceFactory,
    {
      agent,
      canisterId: Principal.fromText(options.canisterId),
    },
  );
};

const decodeWireRows = (bytes: Uint8Array): WireResult => {
  const [decoded] = IDL.decode([IcWirePlanQueryResult], bytes);
  return decoded as WireResult;
};

const relationshipRowToKnowledgeMapResponse = (
  scenarioId: string,
  row: WireRow,
): RouterKnowledgeMapResponse => {
  const sourceId = bytesColumn(row, "source_id");
  const edgeId = bytesColumn(row, "edge_id");
  const targetId = bytesColumn(row, "target_id");
  const edgeWeight = numberColumn(row, "edge_weight");
  const sourceNodeId = `vertex-${sourceId}`;
  const targetNodeId = `vertex-${targetId}`;
  const edgeRowId = `edge-${edgeId}`;

  const rows: RouterKnowledgeMapRow[] = [
    {
      kind: "node",
      node_id: sourceNodeId,
      node_label: "Source vertex",
      node_kind: "person",
      node_x: -2.2,
      node_y: 0.3,
      node_z: 0,
    },
    {
      kind: "node",
      node_id: targetNodeId,
      node_label: "Related vertex",
      node_kind: "document",
      node_x: 2.2,
      node_y: 0.3,
      node_z: 0,
    },
    {
      kind: "edge",
      edge_id: edgeRowId,
      edge_source: sourceNodeId,
      edge_target: targetNodeId,
      edge_label: `KNOWS / weight ${edgeWeight}`,
    },
    {
      kind: "path",
      path_index: 0,
      path_node_id: sourceNodeId,
      story_text: "Router returned the starting vertex id from Gleaph.",
    },
    {
      kind: "path",
      path_index: 1,
      path_node_id: targetNodeId,
      path_edge_id: edgeRowId,
      story_text: "Follow the relationship row returned through Router gql_query.",
    },
    {
      kind: "result",
      result_title: "Router relationship row",
      result_kind: "Live Gleaph result",
      result_reason: `Decoded from rows_blob with edge weight ${edgeWeight}.`,
      result_node_id: targetNodeId,
    },
    {
      kind: "technical",
      technical_index: 0,
      technical_title: "Router gql_query",
      technical_detail: "The frontend calls only the Router canister.",
    },
    {
      kind: "technical",
      technical_index: 1,
      technical_title: "Rows blob",
      technical_detail: "The Router returns Candid-encoded graph rows.",
    },
    {
      kind: "technical",
      technical_index: 2,
      technical_title: "View model",
      technical_detail: "The frontend maps source, edge, and target ids into the demo graph.",
    },
  ];

  return {
    id: scenarioId,
    title: "Live Router relationship",
    question: "Show one relationship returned by Gleaph Router.",
    rows,
  };
};

const bytesColumn = (row: WireRow, column: string): string => {
  const value = columnValue(row, column);
  if (!("Bytes" in value)) {
    throw new Error(`Expected ${column} to be bytes.`);
  }
  return bytesToHex(value.Bytes);
};

const numberColumn = (row: WireRow, column: string): number => {
  const value = columnValue(row, column);
  if ("Int64" in value) {
    return Number(value.Int64);
  }
  if ("Uint64" in value) {
    return Number(value.Uint64);
  }
  throw new Error(`Expected ${column} to be an integer.`);
};

const columnValue = (row: WireRow, column: string): WireValue => {
  const entry = row.columns.find(([name]) => name === column);
  if (!entry) {
    throw new Error(`Missing Router row column: ${column}`);
  }
  return entry[1];
};

const bytesToHex = (bytes: number[] | Uint8Array): string =>
  Array.from(bytes)
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");

const toUint8Array = (bytes: number[] | Uint8Array): Uint8Array =>
  bytes instanceof Uint8Array ? bytes : Uint8Array.from(bytes);

const formatRouterError = (error: unknown): string => {
  if (typeof error === "string") {
    return error;
  }
  return JSON.stringify(error);
};
