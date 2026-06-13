import { For, Show } from "solid-js";

import type { ResultCard } from "~/types";

type ResultCardsProps = {
  results: ResultCard[];
  visible: boolean;
};

export function ResultCards(props: ResultCardsProps) {
  return (
    <section class="rounded-md border border-slate-200 bg-white p-3">
      <h2 class="text-sm font-semibold uppercase tracking-[0.16em] text-slate-500">
        Results
      </h2>
      <Show
        when={props.visible}
        fallback={
          <p class="mt-3 rounded-md border border-slate-200 bg-slate-50 px-3 py-4 text-sm leading-6 text-slate-600">
            Results appear after the path finishes.
          </p>
        }
      >
        <div class="mt-3 space-y-2">
          <For each={props.results}>
            {(result) => (
              <article class="rounded-md border border-sky-200 bg-gradient-to-br from-sky-50 to-indigo-50 p-3">
                <div class="flex items-center justify-between gap-3">
                  <h3 class="text-sm font-semibold text-slate-950">{result.title}</h3>
                  <span class="rounded-full border border-sky-200 bg-white/70 px-2 py-1 text-xs text-sky-800">
                    {result.kind}
                  </span>
                </div>
                <p class="mt-2 text-sm leading-6 text-slate-700">{result.reason}</p>
              </article>
            )}
          </For>
        </div>
      </Show>
    </section>
  );
}
