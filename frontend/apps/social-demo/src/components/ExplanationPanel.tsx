import type { ScenarioDefinition } from "~/data/scenarios";

export function ExplanationPanel(props: {
  definition: ScenarioDefinition;
}) {
  return (
    <div class="space-y-5">
      <div>
        <h2 class="text-sm font-semibold uppercase tracking-wide text-slate-500">
          {props.definition.explanationTitle}
        </h2>
        <p class="mt-2 text-sm leading-relaxed text-slate-700">
          {props.definition.rdbSummary}
        </p>
      </div>

      <div class="rounded-lg bg-indigo-50 p-3">
        <h3 class="text-sm font-semibold text-indigo-900">Graph value add</h3>
        <p class="mt-1 text-sm leading-relaxed text-indigo-800">
          {props.definition.graphSummary}
        </p>
      </div>

      {(props.definition.id === "SemanticDiscovery" ||
        props.definition.id === "AliceSemanticFeed") && (
        <div class="rounded-lg bg-amber-50 p-3">
          <h3 class="text-sm font-semibold text-amber-900">Why the results differ</h3>
          <p class="mt-1 text-sm leading-relaxed text-amber-800">
            The fixed query vector makes{" "}
            <span class="font-medium">post-dave-1</span> the globally nearest
            public Post. In the vector-only scenario it appears first. In Alice’s
            graph-constrained feed it is absent because Alice does not follow Dave,
            even though it is nearer than every followed-author result.
          </p>
        </div>
      )}

      <div class="border-t border-slate-200 pt-4">
        <h3 class="text-xs font-semibold uppercase tracking-wide text-slate-500">
          Scenario subject
        </h3>
        <p class="mt-1 text-sm text-slate-700">
          Alice is a selected demo subject, not the logged-in viewer. There is no
          identity, login, or row-level security in this read-only slice.
        </p>
      </div>
    </div>
  );
}
