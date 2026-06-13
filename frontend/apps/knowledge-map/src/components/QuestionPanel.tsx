import { For } from "solid-js";

import type { ScenarioSummary } from "~/types";

type QuestionPanelProps = {
  scenarios: ScenarioSummary[];
  selectedScenarioId: string;
  onSelect: (id: string) => void;
};

export function QuestionPanel(props: QuestionPanelProps) {
  return (
    <aside class="rounded-md border border-slate-200/80 bg-white/78 p-3 shadow-[0_18px_50px_rgba(15,23,42,0.06)] backdrop-blur">
      <div class="mb-4">
        <h2 class="text-sm font-semibold uppercase tracking-[0.16em] text-slate-500">
          Demo paths
        </h2>
        <p class="mt-2 text-sm leading-6 text-slate-600">
          Choose a path, then watch the real Router round trip before the graph animates.
        </p>
      </div>
      <div class="space-y-2">
        <For each={props.scenarios}>
          {(scenario) => {
            const selected = () => scenario.id === props.selectedScenarioId;
            return (
              <button
                type="button"
                class="w-full rounded-md border p-3 text-left transition"
                classList={{
                  "border-sky-300 bg-sky-50 text-slate-950 shadow-[0_12px_30px_rgba(14,165,233,0.14)]": selected(),
                  "border-slate-200 bg-white text-slate-900 hover:border-sky-200 hover:bg-sky-50/60":
                    !selected(),
                }}
                onClick={() => props.onSelect(scenario.id)}
              >
                <span class="block text-sm font-semibold">{scenario.title}</span>
                <span class="mt-2 block text-sm leading-5 text-slate-600">
                  {scenario.question}
                </span>
              </button>
            );
          }}
        </For>
      </div>
    </aside>
  );
}
