import { createEffect, createMemo, createResource, createSignal, onCleanup } from "solid-js";

import {
  createKnowledgeMapClient,
  defaultScenarioId,
} from "~/api/knowledgeMapClient";
import { adaptRouterKnowledgeMapResponse } from "~/api/viewModelAdapter";
import { type QueryRunState } from "~/api/queryTiming";
import { buildScenarioResponse } from "~/data/knowledgeMapGraph";
import { DemoHeader } from "~/components/DemoHeader";
import { GraphStage } from "~/components/GraphStage";
import { InsightPanel } from "~/components/InsightPanel";
import { QuestionPanel } from "~/components/QuestionPanel";
import type { KnowledgeMapViewModel, PlaybackStatus } from "~/types";

const STEP_MS = 520;
const PLAYBACK_START_DELAY_MS = 120;

const initialViewModel = (): KnowledgeMapViewModel =>
  adaptRouterKnowledgeMapResponse(buildScenarioResponse("alice-fan-out"));
const MAX_RECENT_TIMINGS = 8;

export function KnowledgeMapDemo() {
  const client = createKnowledgeMapClient();
  const [scenarios] = createResource(() => client.listScenarios());
  const [selectedScenarioId, setSelectedScenarioId] = createSignal(defaultScenarioId());
  const [viewModel, setViewModel] = createSignal<KnowledgeMapViewModel | undefined>(initialViewModel());
  const [queryRun, setQueryRun] = createSignal<QueryRunState>({ status: "idle" });
  const [queryText, setQueryText] = createSignal<string | undefined>();
  const [recentTimingsMs, setRecentTimingsMs] = createSignal<number[]>([]);
  const [playbackStatus, setPlaybackStatus] = createSignal<PlaybackStatus>("idle");
  const [activeStepIndex, setActiveStepIndex] = createSignal(0);
  const [technicalMode, setTechnicalMode] = createSignal(false);
  const [runNonce, setRunNonce] = createSignal(0);

  const maxStepIndex = createMemo(() => Math.max(0, (viewModel()?.storySteps.length ?? 1) - 1));

  const runQuery = async (scenarioId: string) => {
    const startedAt = performance.now();
    const liveSource = scenarioId === "live-router-relationship";
    setQueryRun({
      status: "loading",
      startedAt,
      source: liveSource ? "live" : "preview",
    });
    setQueryText(undefined);
    setActiveStepIndex(0);
    setPlaybackStatus("idle");

    try {
      const result = await client.runScenario(scenarioId);
      setViewModel(result.viewModel);
      setQueryText(result.queryText);
      setQueryRun({
        status: "ready",
        timing: result.timing,
        source: result.source,
      });
      setRecentTimingsMs((current) =>
        [result.timing.durationMs, ...current].slice(0, MAX_RECENT_TIMINGS),
      );
      window.setTimeout(() => setPlaybackStatus("playing"), PLAYBACK_START_DELAY_MS);
    } catch (error) {
      const finishedAt = performance.now();
      setQueryRun({
        status: "error",
        message: error instanceof Error ? error.message : String(error),
        source: liveSource ? "live" : "preview",
        timing: {
          startedAt,
          finishedAt,
          durationMs: finishedAt - startedAt,
        },
      });
      setPlaybackStatus("idle");
    }
  };

  createEffect(() => {
    selectedScenarioId();
    runNonce();
    void runQuery(selectedScenarioId());
  });

  createEffect(() => {
    if (playbackStatus() !== "playing" || queryRun().status !== "ready") {
      return;
    }

    const timer = window.setInterval(() => {
      setActiveStepIndex((current) => {
        if (current >= maxStepIndex()) {
          setPlaybackStatus("complete");
          window.clearInterval(timer);
          return current;
        }
        return current + 1;
      });
    }, STEP_MS);

    onCleanup(() => window.clearInterval(timer));
  });

  const selectScenario = (id: string) => {
    setSelectedScenarioId(id);
  };

  const reset = () => {
    setActiveStepIndex(0);
    if (queryRun().status === "ready") {
      setPlaybackStatus("playing");
      return;
    }
    setRunNonce((value) => value + 1);
  };

  const runAgain = () => {
    setRunNonce((value) => value + 1);
  };

  return (
    <main class="min-h-screen px-4 py-4 text-ink md:px-6 md:py-5">
      <div class="mx-auto flex max-w-[1480px] flex-col gap-4">
        <DemoHeader
          playbackStatus={playbackStatus()}
          queryRun={queryRun()}
          technicalMode={technicalMode()}
          onPlay={() => setPlaybackStatus("playing")}
          onPause={() => setPlaybackStatus("paused")}
          onReset={reset}
          onRunAgain={runAgain}
          onToggleTechnical={() => setTechnicalMode((value) => !value)}
        />

        <section class="grid min-h-[calc(100vh-116px)] gap-4 lg:grid-cols-[280px_minmax(0,1fr)_340px]">
          <QuestionPanel
            scenarios={scenarios() ?? []}
            selectedScenarioId={selectedScenarioId()}
            onSelect={selectScenario}
          />
          <GraphStage
            viewModel={viewModel()}
            queryRun={queryRun()}
            activeStepIndex={activeStepIndex()}
            playbackStatus={playbackStatus()}
          />
          <InsightPanel
            viewModel={viewModel()}
            queryRun={queryRun()}
            queryText={queryText()}
            recentTimingsMs={recentTimingsMs()}
            activeStepIndex={activeStepIndex()}
            playbackStatus={playbackStatus()}
            technicalMode={technicalMode()}
            onRunAgain={runAgain}
          />
        </section>
      </div>
    </main>
  );
}
