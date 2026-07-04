import { For } from "solid-js";

import {
  SCENARIO_DEFINITIONS,
  SOCIAL_DEMO_SCENARIO_IDS,
  type ScenarioId,
} from "~/data/scenarios";

export function ScenarioNav(props: {
  active: ScenarioId;
  onSelect: (id: ScenarioId) => void;
}) {
  return (
    <nav aria-label="Scenario navigation">
      <ul class="space-y-1">
        <For each={SOCIAL_DEMO_SCENARIO_IDS}>
          {(id) => {
            const definition = SCENARIO_DEFINITIONS[id];
            const isActive = () => props.active === id;
            return (
              <li>
                <button
                  type="button"
                  onClick={() => props.onSelect(id)}
                  class="flex w-full items-center gap-3 rounded-lg px-3 py-2 text-left transition"
                  classList={{
                    "bg-indigo-50 text-indigo-900": isActive(),
                    "text-slate-700 hover:bg-slate-100": !isActive(),
                  }}
                  aria-current={isActive() ? "page" : undefined}
                >
                  <span class="font-medium">{definition.label}</span>
                </button>
              </li>
            );
          }}
        </For>
      </ul>
    </nav>
  );
}
