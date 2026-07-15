import type { ScenarioDefinition } from "~/data/scenarios";
import { scenarioTranslationKey, useI18n } from "~/i18n";

import { QueryPanel } from "~/components/QueryPanel";
import { TopicPathPerformanceChart } from "~/components/TopicPathPerformanceChart";

export function ExplanationPanel(props: { definition: ScenarioDefinition }) {
  const { t } = useI18n();

  return (
    <div class="space-y-5">
      <QueryPanel definition={props.definition} />

      <div>
        <h2 class="text-sm font-semibold uppercase tracking-wide text-slate-500">
          {t("explanation.rdbBaseline")}
        </h2>
        <p class="mt-2 text-sm leading-relaxed text-slate-700">
          {t(scenarioTranslationKey(props.definition.id, "rdbSummary"))}
        </p>
        {props.definition.id === "TopicPath" && <TopicPathPerformanceChart />}
      </div>

      <div class="rounded-lg bg-indigo-50 p-3">
        <h3 class="text-sm font-semibold text-indigo-900">{t("explanation.graphValueAdd")}</h3>
        <p class="mt-1 text-sm leading-relaxed text-indigo-800">
          {t(scenarioTranslationKey(props.definition.id, "graphSummary"))}
        </p>
      </div>

      {(props.definition.id === "SemanticDiscovery" ||
        props.definition.id === "AliceSemanticFeed") && (
        <div class="rounded-lg bg-amber-50 p-3">
          <h3 class="text-sm font-semibold text-amber-900">
            {t("explanation.whyResultsDiffer")}
          </h3>
          <p class="mt-1 text-sm leading-relaxed text-amber-800">
            {t("explanation.whyResultsDifferBody")}
          </p>
        </div>
      )}
    </div>
  );
}
