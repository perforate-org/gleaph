import { adaptRouterKnowledgeMapResponse } from "~/api/viewModelAdapter";
import {
  fetchKnowledgeMapFromRouter,
  getLiveRouterKnowledgeMapOptions,
} from "~/api/liveRouterKnowledgeMap";
import { type QueryDataSource, type QueryTiming } from "~/api/queryTiming";
import {
  KNOWLEDGE_MAP_LIVE_QUERY,
  KNOWLEDGE_MAP_SCENARIOS,
  buildScenarioResponse,
} from "~/data/knowledgeMapGraph";
import type { KnowledgeMapViewModel, ScenarioSummary } from "~/types";

export type ScenarioQueryResult = {
  viewModel: KnowledgeMapViewModel;
  timing: QueryTiming;
  source: QueryDataSource;
  queryText?: string;
};

export type KnowledgeMapClient = {
  listScenarios(): Promise<ScenarioSummary[]>;
  runScenario(id: string): Promise<ScenarioQueryResult>;
};

export const createKnowledgeMapClient = (): KnowledgeMapClient => ({
  async listScenarios() {
    const scenarios = KNOWLEDGE_MAP_SCENARIOS.map((scenario) => ({
      id: scenario.id,
      title: scenario.title,
      question: scenario.question,
    }));
    if (!getLiveRouterKnowledgeMapOptions()) {
      return scenarios;
    }
    return [
      {
        id: "live-router-relationship",
        title: "Live Alice fan-out",
        question: "Show how Alice's connections fan out across Gleaph.",
      },
      ...scenarios,
    ];
  },
  async runScenario(id) {
    const startedAt = performance.now();
    const liveOptions = getLiveRouterKnowledgeMapOptions();
    if (liveOptions && id === "live-router-relationship") {
      const live = await fetchKnowledgeMapFromRouter(id, liveOptions);
      return {
        viewModel: adaptRouterKnowledgeMapResponse(live.response),
        timing: live.timing,
        source: "live",
        queryText: live.queryText,
      };
    }

    const scenario = KNOWLEDGE_MAP_SCENARIOS.find((item) => item.id === id);
    if (!scenario) {
      throw new Error(`Unknown knowledge-map scenario: ${id}`);
    }

    const viewModel = adaptRouterKnowledgeMapResponse(buildScenarioResponse(id));
    const finishedAt = performance.now();
    return {
      viewModel,
      timing: {
        startedAt,
        finishedAt,
        durationMs: finishedAt - startedAt,
      },
      source: "preview",
      queryText: KNOWLEDGE_MAP_LIVE_QUERY,
    };
  },
});

export const defaultScenarioId = (): string =>
  getLiveRouterKnowledgeMapOptions() ? "live-router-relationship" : "alice-fan-out";
