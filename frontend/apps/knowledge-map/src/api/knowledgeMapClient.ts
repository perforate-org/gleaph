import { adaptRouterKnowledgeMapResponse } from "~/api/viewModelAdapter";
import {
  fetchKnowledgeMapFromRouter,
  getLiveRouterKnowledgeMapOptions,
} from "~/api/liveRouterKnowledgeMap";
import { routerKnowledgeMapResponses } from "~/data/routerRowFixtures";
import type { KnowledgeMapViewModel, ScenarioSummary } from "~/types";

export type KnowledgeMapClient = {
  listScenarios(): Promise<ScenarioSummary[]>;
  getScenario(id: string): Promise<KnowledgeMapViewModel>;
};

export const createKnowledgeMapClient = (): KnowledgeMapClient => ({
  async listScenarios() {
    const scenarios = routerKnowledgeMapResponses.map((scenario) => ({
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
        title: "Live Router relationship",
        question: "Show one relationship returned by Gleaph Router.",
      },
      ...scenarios,
    ];
  },
  async getScenario(id) {
    const liveOptions = getLiveRouterKnowledgeMapOptions();
    if (liveOptions && id === "live-router-relationship") {
      return adaptRouterKnowledgeMapResponse(await fetchKnowledgeMapFromRouter(id, liveOptions));
    }

    const scenario = routerKnowledgeMapResponses.find((item) => item.id === id);
    if (!scenario) {
      throw new Error(`Unknown knowledge-map scenario: ${id}`);
    }
    return adaptRouterKnowledgeMapResponse(scenario);
  },
});
