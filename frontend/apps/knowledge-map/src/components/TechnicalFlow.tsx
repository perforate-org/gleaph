import { For } from "solid-js";

import type { TechnicalFlowStep } from "~/types";

type TechnicalFlowProps = {
  steps: TechnicalFlowStep[];
  activeStepIndex: number;
};

export function TechnicalFlow(props: TechnicalFlowProps) {
  return (
    <section class="rounded-md border border-indigo-200 bg-indigo-50 p-3">
      <h2 class="text-sm font-semibold uppercase tracking-[0.16em] text-indigo-700">
        Technical flow
      </h2>
      <div class="mt-3 space-y-2">
        <For each={props.steps}>
          {(step, index) => (
            <div
              class="rounded-md border px-3 py-2"
              classList={{
                "border-indigo-300 bg-white text-slate-950": index() <= props.activeStepIndex,
                "border-indigo-100 bg-indigo-50/50 text-slate-500": index() > props.activeStepIndex,
              }}
            >
              <p class="text-sm font-semibold">{step.title}</p>
              <p class="mt-1 text-sm leading-5">{step.detail}</p>
            </div>
          )}
        </For>
      </div>
    </section>
  );
}
