import type { ScenarioDefinition } from "~/data/scenarios";

export type FeedRow =
  | { kind: "post"; postId: bigint; createdAt: bigint }
  | {
      kind: "topicPath";
      postId: bigint;
      followsEdgeId: string;
      postedEdgeId: string;
      topicEdgeId: string;
      topicId: bigint;
      createdAt: bigint;
    }
  | {
      kind: "semanticPost";
      postId: bigint;
      distance: number;
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
