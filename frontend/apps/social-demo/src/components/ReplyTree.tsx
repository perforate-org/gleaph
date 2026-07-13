import { For, Show } from "solid-js";

import { FeedItem } from "~/components/FeedItem";
import type { ScenarioDefinition } from "~/data/scenarios";
import type { FeedRow } from "~/types";

type PostRow = Extract<FeedRow, { kind: "post" }>;

type ReplyNode = {
  row: PostRow;
  replies: ReplyNode[];
};

const buildReplyForest = (rows: PostRow[]): ReplyNode[] => {
  const nodes = new Map<bigint, ReplyNode>();
  const roots: ReplyNode[] = [];

  for (const row of rows) {
    nodes.set(row.postId, { row, replies: [] });
  }

  for (const row of rows) {
    const node = nodes.get(row.postId)!;
    const parent = row.parentPostId === undefined ? undefined : nodes.get(row.parentPostId);
    if (parent && parent !== node) {
      parent.replies.push(node);
    } else {
      roots.push(node);
    }
  }

  return roots;
};

export function ReplyTree(props: {
  rows: PostRow[];
  definition: ScenarioDefinition;
  formatDate: (seconds: bigint) => string;
}) {
  return (
    <div class="space-y-4">
      <For each={buildReplyForest(props.rows)}>
        {(node) => <ReplyBranch node={node} definition={props.definition} formatDate={props.formatDate} />}
      </For>
    </div>
  );
}

function ReplyBranch(props: {
  node: ReplyNode;
  definition: ScenarioDefinition;
  formatDate: (seconds: bigint) => string;
}) {
  return (
    <div>
      <FeedItem row={props.node.row} definition={props.definition} formatDate={props.formatDate} />
      <Show when={props.node.replies.length > 0}>
        <div class="ml-5 border-l-2 border-indigo-100 pl-3 pt-3 sm:ml-8">
          <For each={props.node.replies}>
            {(reply) => (
              <div class="mb-3 last:mb-0">
                <ReplyBranch node={reply} definition={props.definition} formatDate={props.formatDate} />
              </div>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
