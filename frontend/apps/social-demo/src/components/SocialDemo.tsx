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
  optionalInt64,
  optionalDateTimeSeconds,
  optionalText,
  rowToColumnMap,
} from "~/api/rowDecoder";
import {
  type ScenarioDefinition,
  type ScenarioId,
  scenarioDefinitionById,
} from "~/data/scenarios";
import { scenarioTranslationKey, useI18n, type Translate } from "~/i18n";
import type { FeedResult, FeedRow } from "~/types";

import { DemoNotice } from "~/components/DemoNotice";
import { ErrorCard } from "~/components/ErrorCard";
import { ExplanationPanel } from "~/components/ExplanationPanel";
import { FeedItem } from "~/components/FeedItem";
import { LanguageSwitcher } from "~/components/LanguageSwitcher";
import { ReplyTree } from "~/components/ReplyTree";
import { ScenarioNav } from "~/components/ScenarioNav";

const isSemanticScenario = (definition: ScenarioDefinition): boolean =>
  definition.id === "SemanticDiscovery" || definition.id === "AliceSemanticFeed";

const decodeFeedResult = (definition: ScenarioDefinition, rowsBlob: Uint8Array): FeedResult => {
  const wire = decodeWireRows(rowsBlob);
  const rows: FeedRow[] = wire.rows.map((row) => {
    const map = rowToColumnMap(row);
    const postId = expectInt64(map, "post_id");

    if (definition.id === "TopicPath") {
      return {
        kind: "topicPath",
        postId,
        authorName: expectText(map, "author_name"),
        body: expectText(map, "body"),
        createdAt: expectDateTimeSeconds(map, "created_at"),
        followsEdgeId: expectText(map, "follows_edge_id"),
        secondFollowsEdgeId: expectText(map, "second_follows_edge_id"),
        postedEdgeId: expectText(map, "posted_edge_id"),
        topicEdgeId: expectText(map, "topic_edge_id"),
        topicId: expectInt64(map, "topic_id"),
      };
    }

    if (isSemanticScenario(definition)) {
      return {
        kind: "semanticPost",
        postId,
        authorName: expectText(map, "author_name"),
        body: expectText(map, "body"),
        distance: expectFloat64(map, "distance"),
      };
    }

    return {
        kind: "post",
        postId,
        parentPostId: optionalInt64(map, "parent_post_id"),
        parentAuthorName: optionalText(map, "parent_author_name"),
        parentBody: optionalText(map, "parent_body"),
        parentCreatedAt: optionalDateTimeSeconds(map, "parent_created_at"),
        authorName: expectText(map, "author_name"),
      body: expectText(map, "body"),
      createdAt: expectDateTimeSeconds(map, "created_at"),
    };
  });

  return { rows, rowCount: BigInt(rows.length) };
};

const loadScenario = async (
  client: GatewayClient,
  definition: ScenarioDefinition,
  t: Translate,
): Promise<FeedResult> => {
  const result = await client.runScenario(definition.scenario);

  const rowsBlob = result.rows_blob;
  if (rowsBlob === undefined) {
    throw new Error(t("feed.noRows"));
  }

  if (result.row_count !== BigInt(decodeWireRows(rowsBlob).rows.length)) {
    throw new Error(
      t("feed.malformedRows"),
    );
  }

  return decodeFeedResult(definition, rowsBlob);
};

const MS_PER_SECOND = 1000;
const SECONDS_PER_MINUTE = 60;
const SECONDS_PER_HOUR = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY = 24 * SECONDS_PER_HOUR;

const formatRelativeDate = (seconds: bigint, t: Translate, nowMs = Date.now()): string => {
  const postMs = Number(seconds) * MS_PER_SECOND;
  const diffSeconds = Math.floor((nowMs - postMs) / MS_PER_SECOND);

  // Treat future posts as having happened "now"; they should not appear in normal feeds.
  if (diffSeconds < 0) {
    const postDate = new Date(postMs);
    return `${postDate.getMonth() + 1}/${postDate.getDate()}/${postDate.getFullYear()}`;
  }

  if (diffSeconds < SECONDS_PER_MINUTE) {
    return t("date.justNow");
  }
  if (diffSeconds < SECONDS_PER_HOUR) {
    return t("date.minutesAgo", { count: Math.floor(diffSeconds / SECONDS_PER_MINUTE) });
  }
  if (diffSeconds < SECONDS_PER_DAY) {
    return t("date.hoursAgo", { count: Math.floor(diffSeconds / SECONDS_PER_HOUR) });
  }
  if (diffSeconds < 2 * SECONDS_PER_DAY) {
    return t("date.yesterday");
  }

  const postDate = new Date(postMs);
  const nowDate = new Date(nowMs);
  const month = postDate.getMonth() + 1;
  const day = postDate.getDate();

  if (postDate.getFullYear() === nowDate.getFullYear()) {
    return `${month}/${day}`;
  }
  return `${month}/${day}/${postDate.getFullYear()}`;
};

export function SocialDemo() {
  const { t } = useI18n();
  const [activeScenarioId, setActiveScenarioId] = createSignal<ScenarioId>("PublicTimeline");

  const options = getGatewayClientOptions();
  const client = options ? createGatewayClient(options) : undefined;

  const [result, { refetch }] = createResource(activeScenarioId, async (id) => {
    const definition = scenarioDefinitionById(id);
    if (!client) {
      throw new Error(t("feed.gatewayNotConfigured"));
    }
    return loadScenario(client, definition, t);
  });

  const activeDefinition = () => scenarioDefinitionById(activeScenarioId());
  const formatDate = (seconds: bigint): string => formatRelativeDate(seconds, t);

  return (
    <div class="min-h-screen">
      <header class="sticky top-0 z-10 border-b border-slate-200 bg-white/90 backdrop-blur">
        <div class="mx-auto flex max-w-6xl items-center justify-between px-4 py-3">
          <div class="flex items-center gap-2">
            <span class="text-xl font-bold text-indigo-700">Gleaph</span>
            <span class="hidden text-sm text-slate-500 sm:inline">{t("brand.socialDemo")}</span>
          </div>
          <div class="flex items-center gap-2">
            <DemoNotice />
            <LanguageSwitcher />
          </div>
        </div>
      </header>

      <main class="mx-auto grid max-w-6xl gap-6 px-4 py-6 lg:grid-cols-[16rem_1fr_20rem]">
        <aside class="hidden lg:block">
          <div class="sticky top-20 rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
            <ScenarioNav active={activeScenarioId()} onSelect={setActiveScenarioId} />
          </div>
        </aside>

        <section class="min-w-0">
          <div class="mb-4 lg:hidden">
            <ScenarioNav active={activeScenarioId()} onSelect={setActiveScenarioId} />
          </div>

          <Show when={activeDefinition().id !== "PublicTimeline"}>
            <div class="mb-4 rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
              <h1 class="text-lg font-semibold text-slate-900">
                {t(scenarioTranslationKey(activeDefinition().id, "feedTitle"))}
              </h1>
            </div>
          </Show>

          <Switch
            fallback={
              <FeedList
                result={result()}
                definition={activeDefinition()}
                formatDate={formatDate}
              />
            }
          >
            <Match when={result.loading}>
              <div class="rounded-xl border border-slate-200 bg-white p-8 text-center text-slate-500 shadow-sm">
                {t("feed.loading")}
              </div>
            </Match>
            <Match when={result.error}>
              <ErrorCard
                title={t("feed.errorTitle")}
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
  formatDate: (seconds: bigint) => string;
}) {
  const { t } = useI18n();

  return (
    <div class="space-y-4">
      <Show
        when={props.result && props.result.rows.length > 0}
        fallback={
          <div class="rounded-xl border border-slate-200 bg-white p-8 text-center text-slate-500 shadow-sm">
            {t("feed.empty")}
          </div>
        }
      >
        <Show
          when={props.result!.rows.every((row) => row.kind === "post")}
          fallback={
            <For each={props.result!.rows}>
              {(row) => (
                <FeedItem row={row} definition={props.definition} formatDate={props.formatDate} />
              )}
            </For>
          }
        >
          <ReplyTree
            rows={props.result!.rows.filter((row) => row.kind === "post")}
            definition={props.definition}
            formatDate={props.formatDate}
          />
        </Show>
      </Show>
    </div>
  );
}
