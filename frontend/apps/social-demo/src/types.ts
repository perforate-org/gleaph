import type { ScenarioDefinition } from "~/data/scenarios";

export type FeedRow =
  | {
      kind: "post";
      postId: bigint;
      parentPostId?: bigint;
      authorName: string;
      body: string;
      createdAt: bigint;
    }
  | {
      kind: "topicPath";
      postId: bigint;
      authorName: string;
      body: string;
      followsEdgeId: string;
      postedEdgeId: string;
      topicEdgeId: string;
      topicId: bigint;
      createdAt: bigint;
    }
  | {
      kind: "semanticPost";
      postId: bigint;
      authorName: string;
      body: string;
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
