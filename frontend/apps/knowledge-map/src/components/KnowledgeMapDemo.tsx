import { createEffect, createMemo, createResource, createSignal, onCleanup } from "solid-js";

import { createKnowledgeMapClient } from "~/api/knowledgeMapClient";
import { DemoHeader } from "~/components/DemoHeader";
import { GraphStage } from "~/components/GraphStage";
import { InsightPanel } from "~/components/InsightPanel";
import { QuestionPanel } from "~/components/QuestionPanel";
import type { PlaybackStatus } from "~/types";

const STEP_MS = 1400;

export function KnowledgeMapDemo() {
  const client = createKnowledgeMapClient();
  const [scenarios] = createResource(() => client.listScenarios());
  const [selectedScenarioId, setSelectedScenarioId] = createSignal("alice-projects");
  const [scenario] = createResource(selectedScenarioId, (id) => client.getScenario(id));
  const [playbackStatus, setPlaybackStatus] = createSignal<PlaybackStatus>("playing");
  const [activeStepIndex, setActiveStepIndex] = createSignal(0);
  const [technicalMode, setTechnicalMode] = createSignal(false);

  const maxStepIndex = createMemo(() => Math.max(0, (scenario()?.storySteps.length ?? 1) - 1));

  createEffect(() => {
    selectedScenarioId();
    setActiveStepIndex(0);
    setPlaybackStatus("playing");
  });

  createEffect(() => {
    if (playbackStatus() !== "playing") {
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
    setPlaybackStatus("playing");
  };

  return (
    <main class="min-h-screen px-4 py-4 text-ink md:px-6 md:py-5">
      <div class="mx-auto flex max-w-[1480px] flex-col gap-4">
        <DemoHeader
          playbackStatus={playbackStatus()}
          technicalMode={technicalMode()}
          onPlay={() => setPlaybackStatus("playing")}
          onPause={() => setPlaybackStatus("paused")}
          onReset={reset}
          onToggleTechnical={() => setTechnicalMode((value) => !value)}
        />

        <section class="grid min-h-[calc(100vh-116px)] gap-4 lg:grid-cols-[280px_minmax(0,1fr)_340px]">
          <QuestionPanel
            scenarios={scenarios() ?? []}
            selectedScenarioId={selectedScenarioId()}
            onSelect={selectScenario}
          />
          <GraphStage
            viewModel={scenario()}
            activeStepIndex={activeStepIndex()}
            playbackStatus={playbackStatus()}
          />
          <InsightPanel
            viewModel={scenario()}
            activeStepIndex={activeStepIndex()}
            playbackStatus={playbackStatus()}
            technicalMode={technicalMode()}
          />
        </section>
      </div>
    </main>
  );
}
