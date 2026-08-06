import { createIntersectionObserver } from "@solid-primitives/intersection-observer";
import { bytesToHex } from "@gleaph/sdk";
import { batch, createEffect, createSignal, For, Match, Show, Switch } from "solid-js";

import { createRouterClient, getRouterClientOptions } from "~/api/routerClient";
import { type ScenarioDefinition, type ScenarioId, scenarioDefinitionById } from "~/data/scenarios";
import { scenarioTranslationKey, useI18n, type Translate } from "~/i18n";
import type { FeedResult, FeedRow } from "~/types";
import {
  type PreparedGleaphClient,
  type PublicTimelineRow,
  type SemanticDiscoveryRow,
  type TopicPathExplanationRow,
} from "~/generated";

import { DemoNotice } from "~/components/DemoNotice";
import { ErrorCard } from "~/components/ErrorCard";
import { ExplanationPanel } from "~/components/ExplanationPanel";
import { FeedItem } from "~/components/FeedItem";
import { LanguageSwitcher } from "~/components/LanguageSwitcher";
import { ReplyTree } from "~/components/ReplyTree";
import { ScenarioNav } from "~/components/ScenarioNav";

const PAGE_SIZE = 20;

// Structural Temporal.Instant shape; the SDK decodes DateTime columns to one.
const epochSeconds = (instant: { epochNanoseconds: bigint }): bigint =>
  instant.epochNanoseconds / 1_000_000_000n;

// The generated client decodes rows through the SDK (`decodeRows` with the static column
// schema per operation); the mappers below only reshape into the app's presentation types
// (hex edge ids, epoch seconds for dates).
const semanticVector = (definition: ScenarioDefinition): Uint8Array => {
  if (!definition.semanticVector) {
    throw new Error(`scenario ${definition.id} requires a semanticVector`);
  }
  return Uint8Array.from(definition.semanticVector);
};

const toPostRow = (row: PublicTimelineRow): FeedRow => ({
  kind: "post",
  postId: row.post_id,
  parentPostId: row.parent_post_id ?? undefined,
  parentAuthorName: row.parent_author_name ?? undefined,
  parentBody: row.parent_body ?? undefined,
  parentCreatedAt: row.parent_created_at ? epochSeconds(row.parent_created_at) : undefined,
  authorName: row.author_name,
  body: row.body,
  createdAt: epochSeconds(row.created_at),
});

const toTopicPathRow = (row: TopicPathExplanationRow): FeedRow => ({
  kind: "topicPath",
  postId: row.post_id,
  authorName: row.author_name,
  body: row.body,
  createdAt: epochSeconds(row.created_at),
  followsEdgeId: bytesToHex(row.follows_edge_id),
  secondFollowsEdgeId: bytesToHex(row.second_follows_edge_id),
  postedEdgeId: bytesToHex(row.posted_edge_id),
  topicEdgeId: bytesToHex(row.topic_edge_id),
  topicId: row.topic_id,
});

const toSemanticRow = (row: SemanticDiscoveryRow): FeedRow => ({
  kind: "semanticPost",
  postId: row.post_id,
  authorName: row.author_name,
  body: row.body,
  // SEARCH rows always carry a distance; the manifest types it nullable, so
  // coalesce defensively.
  distance: row.distance ?? 0,
});

const feedResult = <Row,>(
  result: { rows: Row[]; row_count: bigint },
  map: (row: Row) => FeedRow,
): FeedResult => ({
  rows: result.rows.map(map),
  rowCount: result.row_count,
});

const loadScenario = async (
  client: PreparedGleaphClient,
  definition: ScenarioDefinition,
  offset: number,
): Promise<FeedResult> => {
  const pageOffset = BigInt(offset);

  switch (definition.preparedQueryId) {
    case "public-timeline":
      return feedResult(await client.publicTimeline({ offset: pageOffset }), toPostRow);
    case "alice-home-feed":
      return feedResult(await client.aliceHomeFeed({ offset: pageOffset }), toPostRow);
    case "yui-home-feed":
      return feedResult(await client.yuiHomeFeed({ offset: pageOffset }), toPostRow);
    case "topic-path-explanation":
      return feedResult(await client.topicPathExplanation({ offset: pageOffset }), toTopicPathRow);
    case "semantic-discovery":
      return feedResult(
        await client.semanticDiscovery({ offset: pageOffset, query: semanticVector(definition) }),
        toSemanticRow,
      );
    case "alice-semantic-feed":
      return feedResult(
        await client.aliceSemanticFeed({ offset: pageOffset, query: semanticVector(definition) }),
        toSemanticRow,
      );
    default:
      throw new Error(`unknown prepared query: ${definition.preparedQueryId}`);
  }
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

  const routerOptions = getRouterClientOptions();
  const clientPromise = routerOptions ? createRouterClient(routerOptions) : undefined;

  const [feedResult, setFeedResult] = createSignal<FeedResult | undefined>();
  const [isLoadingInitial, setIsLoadingInitial] = createSignal(false);
  const [isLoadingMore, setIsLoadingMore] = createSignal(false);
  const [hasMore, setHasMore] = createSignal(true);
  const [error, setError] = createSignal<Error | undefined>();

  const activeDefinition = () => scenarioDefinitionById(activeScenarioId());
  const formatDate = (seconds: bigint): string => formatRelativeDate(seconds, t);

  const resetAndLoad = async (id: ScenarioId) => {
    if (!clientPromise) {
      setError(new Error(t("feed.routerNotConfigured")));
      return;
    }

    batch(() => {
      setFeedResult(undefined);
      setIsLoadingInitial(true);
      setIsLoadingMore(false);
      setHasMore(true);
      setError(undefined);
    });

    const definition = scenarioDefinitionById(id);
    try {
      const client = await clientPromise;
      const result = await loadScenario(client, definition, 0);
      if (activeScenarioId() !== id) return;
      batch(() => {
        setFeedResult(result);
        setHasMore(result.rows.length === PAGE_SIZE);
        setIsLoadingInitial(false);
      });
    } catch (err) {
      if (activeScenarioId() !== id) return;
      batch(() => {
        setError(err instanceof Error ? err : new Error(String(err)));
        setIsLoadingInitial(false);
      });
    }
  };

  createEffect(() => {
    const id = activeScenarioId();
    resetAndLoad(id);
  });

  const loadMore = async () => {
    if (!clientPromise || isLoadingInitial() || isLoadingMore() || !hasMore()) {
      return;
    }

    const id = activeScenarioId();
    const definition = scenarioDefinitionById(id);
    const offset = feedResult()?.rows.length ?? 0;

    setIsLoadingMore(true);
    try {
      const client = await clientPromise;
      const result = await loadScenario(client, definition, offset);
      if (activeScenarioId() !== id) return;
      batch(() => {
        setFeedResult((prev) => {
          if (!prev) return result;
          return {
            rows: [...prev.rows, ...result.rows],
            rowCount: prev.rowCount + result.rowCount,
          };
        });
        setHasMore(result.rows.length === PAGE_SIZE);
        setIsLoadingMore(false);
      });
    } catch (err) {
      if (activeScenarioId() !== id) return;
      batch(() => {
        setError(err instanceof Error ? err : new Error(String(err)));
        setIsLoadingMore(false);
      });
    }
  };

  const [sentinel, setSentinel] = createSignal<HTMLElement | undefined>();

  createIntersectionObserver(
    () => (sentinel() ? [sentinel()!] : []),
    (entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        loadMore();
      }
    },
    { threshold: 0, rootMargin: "200px" },
  );

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
                result={feedResult()}
                formatDate={formatDate}
                isLoadingMore={isLoadingMore()}
                hasMore={hasMore()}
                sentinelRef={setSentinel}
              />
            }
          >
            <Match when={isLoadingInitial()}>
              <div class="rounded-xl border border-slate-200 bg-white p-8 text-center text-slate-500 shadow-sm">
                {t("feed.loading")}
              </div>
            </Match>
            <Match when={error()}>
              <ErrorCard
                title={t("feed.errorTitle")}
                message={String(error())}
                onRetry={() => resetAndLoad(activeScenarioId())}
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
  formatDate: (seconds: bigint) => string;
  isLoadingMore: boolean;
  hasMore: boolean;
  sentinelRef: (el: HTMLElement) => void;
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
              {(row) => <FeedItem row={row} formatDate={props.formatDate} />}
            </For>
          }
        >
          <ReplyTree
            rows={props.result!.rows.filter((row) => row.kind === "post")}
            formatDate={props.formatDate}
          />
        </Show>

        <Show when={props.hasMore}>
          <div
            ref={props.sentinelRef}
            class="h-8 rounded-xl border border-transparent"
            aria-hidden="true"
          />
        </Show>

        <Show when={props.isLoadingMore}>
          <div class="rounded-xl border border-slate-200 bg-white p-4 text-center text-slate-500 shadow-sm">
            {t("feed.loading")}
          </div>
        </Show>
      </Show>
    </div>
  );
}
