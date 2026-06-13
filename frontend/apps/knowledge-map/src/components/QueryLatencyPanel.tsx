import { For, Show, createEffect, createSignal, onCleanup } from "solid-js";

import {
  type QueryRunState,
  elapsedMs,
  formatDurationMs,
} from "~/api/queryTiming";

type QueryLatencyPanelProps = {
  queryRun: QueryRunState;
  queryText?: string;
  technicalMode: boolean;
  recentTimingsMs: number[];
  onRunAgain: () => void;
};

export function QueryLatencyPanel(props: QueryLatencyPanelProps) {
  const [liveElapsedMs, setLiveElapsedMs] = createSignal(0);

  createEffect(() => {
    const run = props.queryRun;
    if (run.status !== "loading") {
      setLiveElapsedMs(0);
      return;
    }

    const tick = () => setLiveElapsedMs(elapsedMs(run.startedAt));
    tick();
    const timer = window.setInterval(tick, 32);
    onCleanup(() => window.clearInterval(timer));
  });

  const sourceLabel = () => {
    const run = props.queryRun;
    if (run.status === "idle") {
      return undefined;
    }
    return run.source === "live" ? "Live Router gql_query" : "Preview fixture";
  };

  return (
    <section class="rounded-md border border-slate-200 bg-white p-3">
      <div class="flex items-start justify-between gap-3">
        <div>
          <p class="text-xs font-semibold uppercase tracking-[0.16em] text-slate-500">
            Query latency
          </p>
          <Show when={props.queryRun.status === "loading"}>
            <>
              <p class="mt-2 text-3xl font-semibold tabular-nums tracking-tight text-sky-700">
                {formatDurationMs(liveElapsedMs())}
              </p>
              <p class="mt-1 text-sm text-slate-600">Waiting for Gleaph Router…</p>
            </>
          </Show>
          <Show
            when={
              props.queryRun.status === "ready"
                ? props.queryRun
                : undefined
            }
          >
            {(run) => (
              <>
                <p class="mt-2 text-3xl font-semibold tabular-nums tracking-tight text-slate-950">
                  {formatDurationMs(run().timing.durationMs)}
                </p>
                <p class="mt-1 text-sm text-slate-600">
                  Round trip from this browser to Gleaph Router and back.
                  <Show when={run().timing.rowCount !== undefined}>
                    {" "}
                    {run().timing.rowCount} row
                    {run().timing.rowCount === 1 ? "" : "s"} returned.
                  </Show>
                </p>
              </>
            )}
          </Show>
          <Show
            when={
              props.queryRun.status === "error"
                ? props.queryRun
                : undefined
            }
          >
            {(run) => (
              <p class="mt-2 text-sm font-medium text-rose-700">
                Query failed: {run().message}
              </p>
            )}
          </Show>
          <Show when={sourceLabel()}>
            {(label) => (
              <p class="mt-2 text-xs font-medium uppercase tracking-[0.14em] text-slate-500">
                {label()}
              </p>
            )}
          </Show>
        </div>
        <button
          type="button"
          class="shrink-0 rounded-md border border-sky-300 bg-sky-50 px-3 py-2 text-sm font-medium text-sky-900 transition hover:border-sky-400 hover:bg-sky-100 disabled:cursor-not-allowed disabled:opacity-50"
          disabled={props.queryRun.status === "loading"}
          onClick={() => props.onRunAgain()}
        >
          Run again
        </button>
      </div>

      <Show when={props.recentTimingsMs.length > 1}>
        <div class="mt-3 rounded-md border border-slate-100 bg-slate-50 px-3 py-2">
          <p class="text-xs font-semibold uppercase tracking-[0.14em] text-slate-500">
            Recent runs
          </p>
          <p class="mt-1 text-sm tabular-nums text-slate-700">
            <For each={props.recentTimingsMs}>
              {(duration, index) => (
                <>
                  <Show when={index() > 0}>, </Show>
                  {formatDurationMs(duration)}
                </>
              )}
            </For>
          </p>
        </div>
      </Show>

      <Show when={props.technicalMode && props.queryText}>
        <pre class="mt-3 overflow-x-auto rounded-md border border-indigo-100 bg-indigo-50/70 p-3 text-xs leading-5 text-indigo-950">
          {props.queryText}
        </pre>
      </Show>
    </section>
  );
}
