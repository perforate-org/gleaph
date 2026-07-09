import { createResource, createSignal, For, Match, Show, Switch } from "solid-js";

import {
  type GatewayClient,
  createGatewayClient,
  getGatewayClientOptions,
} from "~/api/gatewayClient";
import {
  decodeWireRows,
  expectDateTimeSeconds,
  expectFloat64,
  expectInt64,
  expectText,
  rowToColumnMap,
} from "~/api/rowDecoder";
import {
  SCENARIO_DEFINITIONS,
  SOCIAL_DEMO_SCENARIO_IDS,
  type ScenarioDefinition,
  type ScenarioId,
  scenarioDefinitionById,
} from "~/data/scenarios";
import type { FeedResult, FeedRow } from "~/types";

import { DemoNotice } from "~/components/DemoNotice";
import { ErrorCard } from "~/components/ErrorCard";
import { ExplanationPanel } from "~/components/ExplanationPanel";
import { FeedItem } from "~/components/FeedItem";
import { ScenarioNav } from "~/components/ScenarioNav";

const isSemanticScenario = (definition: ScenarioDefinition): boolean =>
  definition.id === "SemanticDiscovery" || definition.id === "AliceSemanticFeed";

const decodeFeedResult = (
  definition: ScenarioDefinition,
  rowsBlob: Uint8Array,
): FeedResult => {
  const wire = decodeWireRows(rowsBlob);
  const rows: FeedRow[] = wire.rows.map((row) => {
    const map = rowToColumnMap(row);
    const postId = expectInt64(map, "post_id");

    if (definition.id === "TopicPath") {
      return {
        kind: "topicPath",
        postId,
        createdAt: expectDateTimeSeconds(map, "created_at"),
        followsEdgeId: expectText(map, "follows_edge_id"),
        postedEdgeId: expectText(map, "posted_edge_id"),
        topicEdgeId: expectText(map, "topic_edge_id"),
        topicId: expectInt64(map, "topic_id"),
      };
    }

    if (isSemanticScenario(definition)) {
      return {
        kind: "semanticPost",
        postId,
        distance: expectFloat64(map, "distance"),
      };
    }

    return { kind: "post", postId, createdAt: expectDateTimeSeconds(map, "created_at") };
  });

  return { rows, rowCount: BigInt(rows.length) };
};

const loadScenario = async (
  client: GatewayClient,
  definition: ScenarioDefinition,
): Promise<FeedResult> => {
  const result = await client.runScenario(definition.scenario);

  const rowsBlob = result.rows_blob;
  if (rowsBlob === undefined) {
    throw new Error("Gateway returned no rows. The scenario may not be seeded yet.");
  }

  if (result.row_count !== BigInt(decodeWireRows(rowsBlob).rows.length)) {
    throw new Error(
      "Gateway row_count does not match decoded rows. The response may be malformed.",
    );
  }

  return decodeFeedResult(definition, rowsBlob);
};

const formatDate = (seconds: bigint): string => {
  try {
    return new Date(Number(seconds) * 1000).toLocaleString();
  } catch {
    return String(seconds);
  }
};

export function SocialDemo() {
  const [activeScenarioId, setActiveScenarioId] =
    createSignal<ScenarioId>("PublicTimeline");

  const options = getGatewayClientOptions();
  const client = options ? createGatewayClient(options) : undefined;

  const [result, { refetch }] = createResource(activeScenarioId, async (id) => {
    const definition = scenarioDefinitionById(id);
    if (!client) {
      throw new Error(
        "Social demo Gateway canister id is not configured. The asset canister should inject PUBLIC_CANISTER_ID:gleaph-social-demo-gateway, or set VITE_SOCIAL_DEMO_GATEWAY_CANISTER_ID for local development.",
      );
    }
    return loadScenario(client, definition);
  });

  const activeDefinition = () => scenarioDefinitionById(activeScenarioId());

  return (
    <div class="min-h-screen">
      <header class="sticky top-0 z-10 border-b border-slate-200 bg-white/90 backdrop-blur">
        <div class="mx-auto flex max-w-6xl items-center justify-between px-4 py-3">
          <div class="flex items-center gap-2">
            <span class="text-xl font-bold text-indigo-700">Gleaph</span>
            <span class="hidden text-sm text-slate-500 sm:inline">Social Demo</span>
          </div>
          <DemoNotice />
        </div>
      </header>

      <main class="mx-auto grid max-w-6xl gap-6 px-4 py-6 lg:grid-cols-[16rem_1fr_20rem]">
        <aside class="hidden lg:block">
          <div class="sticky top-20 rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
            <ScenarioNav
              active={activeScenarioId()}
              onSelect={setActiveScenarioId}
            />
          </div>
        </aside>

        <section class="min-w-0">
          <div class="mb-4 lg:hidden">
            <ScenarioNav
              active={activeScenarioId()}
              onSelect={setActiveScenarioId}
            />
          </div>

          <div class="mb-4 rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
            <h1 class="text-lg font-semibold text-slate-900">
              {activeDefinition().feedTitle}
            </h1>
            <p class="text-sm text-slate-500">
              {activeDefinition().label} · Anonymous read-only demo
            </p>
          </div>

          <Switch fallback={<FeedList result={result()} definition={activeDefinition()} />}>
            <Match when={result.loading}>
              <div class="rounded-xl border border-slate-200 bg-white p-8 text-center text-slate-500 shadow-sm">
                Loading scenario through Gateway…
              </div>
            </Match>
            <Match when={result.error}>
              <ErrorCard
                title="Scenario failed"
                message={String(result.error)}
                onRetry={() => refetch()}
              />
            </Match>
          </Switch>
        </section>

        <aside class="min-w-0">
          <div class="sticky top-20 rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
            <ExplanationPanel definition={activeDefinition()} />
          </div>
        </aside>
      </main>
    </div>
  );
}

function FeedList(props: {
  result: FeedResult | undefined;
  definition: ScenarioDefinition;
}) {
  return (
    <div class="space-y-4">
      <Show
        when={props.result && props.result.rows.length > 0}
        fallback={
          <div class="rounded-xl border border-slate-200 bg-white p-8 text-center text-slate-500 shadow-sm">
            No posts returned for this scenario.
          </div>
        }
      >
        <For each={props.result!.rows}>
          {(row) => (
            <FeedItem
              row={row}
              definition={props.definition}
              formatDate={formatDate}
            />
          )}
        </For>
      </Show>
    </div>
  );
}
