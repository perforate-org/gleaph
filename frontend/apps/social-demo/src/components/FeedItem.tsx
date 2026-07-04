import type { FeedItemProps } from "~/types";

export function FeedItem(props: FeedItemProps) {
  const { row, definition, formatDate } = props;

  return (
    <article class="rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
      <div class="flex items-start gap-3">
        <div class="flex h-10 w-10 shrink-0 items-center justify-center rounded-full bg-indigo-100 font-semibold text-indigo-700">
          {definition.shortLabel.charAt(0)}
        </div>
        <div class="min-w-0 flex-1">
          <div class="flex items-center gap-2">
            <span class="font-semibold text-slate-900">{row.postId}</span>
            <span class="text-sm text-slate-500">· {formatDate(row.createdAt)}</span>
          </div>
          <p class="mt-1 text-sm text-slate-600">
            Returned by the <strong>{definition.label}</strong> Gateway scenario.
          </p>

          {row.kind === "topicPath" && (
            <div class="mt-3 rounded-lg bg-slate-50 p-3">
              <h3 class="text-xs font-semibold uppercase tracking-wide text-slate-500">
                Relationship trail
              </h3>
              <ul class="mt-2 space-y-1 text-sm text-slate-700">
                <li>
                  Follower edge{" "}
                  <code class="rounded bg-slate-200 px-1 py-0.5 text-xs">
                    {row.followsEdgeId}
                  </code>
                </li>
                <li>
                  Author-post edge{" "}
                  <code class="rounded bg-slate-200 px-1 py-0.5 text-xs">
                    {row.postedEdgeId}
                  </code>{" "}
                  on <span class="font-medium">{row.postId}</span>
                </li>
                <li>
                  Post-topic edge{" "}
                  <code class="rounded bg-slate-200 px-1 py-0.5 text-xs">
                    {row.topicEdgeId}</code>{" "}
                  to topic <span class="font-medium">{row.topicId}</span>
                </li>
              </ul>
              <p class="mt-2 text-xs text-slate-500">
                Labels reflect the fixed social-graph seed. Update them if the seed
                subject changes.
              </p>
            </div>
          )}
        </div>
      </div>
    </article>
  );
}
