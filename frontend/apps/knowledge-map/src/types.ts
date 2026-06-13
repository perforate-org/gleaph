export type NodeKind = "person" | "post" | "topic" | "project" | "document";

export type DemoNode = {
  id: string;
  label: string;
  kind: NodeKind;
  positionHint?: [number, number, number];
};

export type DemoEdge = {
  id: string;
  source: string;
  target: string;
  label: string;
};

export type StoryStep = {
  nodeId?: string;
  edgeId?: string;
  text: string;
};

export type ResultCard = {
  title: string;
  kind: string;
  reason: string;
  nodeId?: string;
};

export type TechnicalFlowStep = {
  title: string;
  detail: string;
};

export type KnowledgeMapViewModel = {
  id: string;
  title: string;
  question: string;
  nodes: DemoNode[];
  edges: DemoEdge[];
  activePath: string[];
  storySteps: StoryStep[];
  results: ResultCard[];
  technicalFlow: TechnicalFlowStep[];
};

export type ScenarioSummary = {
  id: string;
  title: string;
  question: string;
};

export type PlaybackStatus = "idle" | "playing" | "paused" | "complete";
