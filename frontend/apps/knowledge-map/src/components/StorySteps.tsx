import { For } from "solid-js";

import type { StoryStep } from "~/types";

type StoryStepsProps = {
  steps: StoryStep[];
  activeStepIndex: number;
};

export function StorySteps(props: StoryStepsProps) {
  return (
    <section class="rounded-md border border-slate-200 bg-white p-3">
      <h2 class="text-sm font-semibold uppercase tracking-[0.16em] text-slate-500">
        What is happening
      </h2>
      <ol class="mt-3 space-y-2">
        <For each={props.steps}>
          {(step, index) => {
            const active = () => index() === props.activeStepIndex;
            const visited = () => index() < props.activeStepIndex;
            return (
              <li
                class="flex gap-3 rounded-md border px-3 py-2 transition"
                classList={{
                  "border-sky-300 bg-sky-50 text-slate-950": active(),
                  "border-teal-200 bg-teal-50 text-slate-800": visited(),
                  "border-slate-200 text-slate-500": !active() && !visited(),
                }}
              >
                <span class="flex size-6 shrink-0 items-center justify-center rounded-full border border-current text-xs">
                  {visited() ? "✓" : index() + 1}
                </span>
                <span class="text-sm leading-6">{step.text}</span>
              </li>
            );
          }}
        </For>
      </ol>
    </section>
  );
}
