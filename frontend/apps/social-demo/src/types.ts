import type { ScenarioDefinition } from "~/data/scenarios";

export type FeedRow =
  | { kind: "post"; postId: string; createdAt: bigint }
  | {
      kind: "topicPath";
      postId: string;
      followsEdgeId: string;
      postedEdgeId: string;
      topicEdgeId: string;
      topicId: string;
      createdAt: bigint;
    };

export type FeedResult = {
  rows: FeedRow[];
  rowCount: bigint;
};

export type FeedItemProps = {
  row: FeedRow;
  definition: ScenarioDefinition;
  formatDate: (seconds: bigint) => string;
};
