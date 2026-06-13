import { Show } from "solid-js";

import { type QueryRunState, formatDurationMs } from "~/api/queryTiming";
import type { PlaybackStatus } from "~/types";

type DemoHeaderProps = {
  playbackStatus: PlaybackStatus;
  queryRun: QueryRunState;
  technicalMode: boolean;
  onPlay: () => void;
  onPause: () => void;
  onReset: () => void;
  onRunAgain: () => void;
  onToggleTechnical: () => void;
};

export function DemoHeader(props: DemoHeaderProps) {
  return (
    <header class="flex flex-col gap-3 rounded-md border border-slate-200/80 bg-white/80 px-4 py-3 shadow-[0_24px_70px_rgba(15,23,42,0.08)] backdrop-blur md:flex-row md:items-center md:justify-between">
      <div>
        <p class="text-xs font-semibold uppercase tracking-[0.18em] text-sky-700">
          Gleaph demo
        </p>
        <div class="mt-1 flex flex-wrap items-center gap-3">
          <h1 class="text-2xl font-semibold text-slate-950">Knowledge Map</h1>
          <Show when={props.queryRun.status === "loading"}>
            <span class="rounded-full border border-sky-200 bg-sky-50 px-3 py-1 text-sm font-medium tabular-nums text-sky-800">
              Querying…
            </span>
          </Show>
          <Show
            when={
              props.queryRun.status === "ready"
                ? props.queryRun
                : undefined
            }
          >
            {(run) => (
              <span class="rounded-full border border-teal-200 bg-teal-50 px-3 py-1 text-sm font-semibold tabular-nums text-teal-900">
                {formatDurationMs(run().timing.durationMs)}
              </span>
            )}
          </Show>
        </div>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <button
          class="rounded-md border border-sky-300 bg-sky-50 px-3 py-2 text-sm font-medium text-sky-900 transition hover:border-sky-400 hover:bg-sky-100 disabled:cursor-not-allowed disabled:opacity-50"
          type="button"
          disabled={props.queryRun.status === "loading"}
          onClick={() => props.onRunAgain()}
        >
          Run query
        </button>
        <button
          class="rounded-md border border-slate-300 bg-white px-3 py-2 text-sm font-medium text-slate-950 transition hover:border-sky-300 hover:bg-sky-50 disabled:cursor-not-allowed disabled:opacity-50"
          type="button"
          disabled={props.queryRun.status !== "ready"}
          onClick={props.playbackStatus === "playing" ? props.onPause : props.onPlay}
        >
          {props.playbackStatus === "playing" ? "Pause" : "Play"}
        </button>
        <button
          class="rounded-md border border-slate-300 bg-white px-3 py-2 text-sm font-medium text-slate-950 transition hover:border-sky-300 hover:bg-sky-50 disabled:cursor-not-allowed disabled:opacity-50"
          type="button"
          disabled={props.queryRun.status !== "ready"}
          onClick={props.onReset}
        >
          Replay
        </button>
        <button
          class="rounded-md border border-indigo-200 bg-indigo-50 px-3 py-2 text-sm font-medium text-indigo-800 transition hover:border-indigo-300 hover:bg-indigo-100"
          type="button"
          aria-pressed={props.technicalMode}
          onClick={props.onToggleTechnical}
        >
          {props.technicalMode ? "Hide technical flow" : "Show technical flow"}
        </button>
      </div>
    </header>
  );
}
