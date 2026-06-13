import { Show } from "solid-js";

import { type QueryRunState } from "~/api/queryTiming";
import { QueryLatencyPanel } from "~/components/QueryLatencyPanel";
import { ResultCards } from "~/components/ResultCards";
import { StorySteps } from "~/components/StorySteps";
import { TechnicalFlow } from "~/components/TechnicalFlow";
import type { KnowledgeMapViewModel, PlaybackStatus } from "~/types";

type InsightPanelProps = {
  viewModel?: KnowledgeMapViewModel;
  queryRun: QueryRunState;
  queryText?: string;
  recentTimingsMs: number[];
  activeStepIndex: number;
  playbackStatus: PlaybackStatus;
  technicalMode: boolean;
  onRunAgain: () => void;
};

export function InsightPanel(props: InsightPanelProps) {
  return (
    <aside class="flex flex-col gap-3 rounded-md border border-slate-200/80 bg-white/78 p-3 shadow-[0_18px_50px_rgba(15,23,42,0.06)] backdrop-blur">
      <QueryLatencyPanel
        queryRun={props.queryRun}
        queryText={props.queryText}
        technicalMode={props.technicalMode}
        recentTimingsMs={props.recentTimingsMs}
        onRunAgain={props.onRunAgain}
      />
      <Show when={props.viewModel}>
        {(viewModel) => (
          <>
            <div class="rounded-md border border-slate-200 bg-white p-3">
              <p class="text-xs font-semibold uppercase tracking-[0.16em] text-slate-500">
                Selected question
              </p>
              <h2 class="mt-2 text-lg font-semibold text-slate-950">
                {viewModel().question}
              </h2>
            </div>
            <StorySteps
              steps={viewModel().storySteps}
              activeStepIndex={props.playbackStatus === "idle" ? -1 : props.activeStepIndex}
            />
            <ResultCards
              results={viewModel().results}
              visible={props.playbackStatus === "complete"}
            />
            <Show when={props.technicalMode}>
              <TechnicalFlow
                steps={viewModel().technicalFlow}
                activeStepIndex={props.activeStepIndex}
              />
            </Show>
          </>
        )}
      </Show>
    </aside>
  );
}
