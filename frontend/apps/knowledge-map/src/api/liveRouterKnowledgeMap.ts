import { Actor, HttpAgent, type ActorSubclass } from "@icp-sdk/core/agent";
import { safeGetCanisterEnv } from "@icp-sdk/core/agent/canister-env";
import { IDL } from "@icp-sdk/core/candid";
import { Principal } from "@icp-sdk/core/principal";

import { type QueryTiming } from "~/api/queryTiming";
import { IcWirePlanQueryResult, routerIdlFactory } from "~/api/routerIdl";
import {
  KNOWLEDGE_MAP_LIVE_QUERY,
  buildLiveScenarioResponse,
  parseLiveGraphEdgeRow,
} from "~/data/knowledgeMapGraph";
import type { RouterKnowledgeMapResponse } from "~/api/viewModelAdapter";

export const KNOWLEDGE_MAP_QUERY = KNOWLEDGE_MAP_LIVE_QUERY;

const LIVE_SCENARIO_ID = "alice-fan-out";

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

export type LiveKnowledgeMapQueryResult = {
  response: RouterKnowledgeMapResponse;
  timing: QueryTiming;
  queryText: string;
};

export const fetchKnowledgeMapFromRouter = async (
  scenarioId: string,
  options: LiveRouterKnowledgeMapOptions,
): Promise<LiveKnowledgeMapQueryResult> => {
  if (scenarioId !== "live-router-relationship") {
    throw new Error(`Live Router source does not support scenario: ${scenarioId}`);
  }

  const startedAt = performance.now();
  const actor = await createRouterActor(options);
  const result = await actor.gql_query(KNOWLEDGE_MAP_LIVE_QUERY, []);
  if ("Err" in result) {
    throw new Error(`Router gql_query failed: ${formatRouterError(result.Err)}`);
  }

  const rowsBlob = result.Ok.rows_blob[0];
  if (!rowsBlob) {
    throw new Error("Router gql_query returned no rows_blob for knowledge-map query.");
  }

  const wire = decodeWireRows(toUint8Array(rowsBlob));
  if (wire.rows.length === 0) {
    throw new Error("Router gql_query returned no demo graph rows for knowledge-map query.");
  }

  const liveEdges = wire.rows.map((row) => parseLiveGraphEdgeRow(wireRowToColumnMap(row)));
  const finishedAt = performance.now();
  return {
    response: buildLiveScenarioResponse(LIVE_SCENARIO_ID, liveEdges),
    timing: {
      startedAt,
      finishedAt,
      durationMs: finishedAt - startedAt,
      rowCount: Number(result.Ok.row_count),
    },
    queryText: KNOWLEDGE_MAP_LIVE_QUERY,
  };
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

const wireRowToColumnMap = (row: WireRow): Map<string, string> => {
  const columns = new Map<string, string>();
  for (const [name, value] of row.columns) {
    if ("Text" in value) {
      columns.set(name, value.Text);
      continue;
    }
    if ("Int64" in value) {
      columns.set(name, value.Int64.toString());
      continue;
    }
    if ("Uint64" in value) {
      columns.set(name, value.Uint64.toString());
    }
  }
  return columns;
};

const toUint8Array = (bytes: number[] | Uint8Array): Uint8Array =>
  bytes instanceof Uint8Array ? bytes : Uint8Array.from(bytes);

const formatRouterError = (error: unknown): string => {
  if (typeof error === "string") {
    return error;
  }
  return JSON.stringify(error);
};
