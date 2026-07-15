import { displayPostId } from "~/data/scenarios";
import { useI18n } from "~/i18n";
import type { FeedItemProps } from "~/types";

export function FeedItem(props: FeedItemProps) {
  const { row, definition, formatDate } = props;
  const { t } = useI18n();

  return (
    <article class="rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
      <div class="flex items-start gap-3">
        <div class="flex h-10 w-10 shrink-0 items-center justify-center rounded-full bg-indigo-100 font-semibold text-indigo-700">
          {row.authorName.charAt(0)}
        </div>
        <div class="min-w-0 flex-1">
          <div class="flex items-center gap-2">
            <span class="font-medium text-slate-900">{row.authorName}</span>
            {row.kind !== "semanticPost" && (
              <span class="text-sm text-slate-500"> {formatDate(row.createdAt)}</span>
            )}
            {row.kind === "semanticPost" && (
              <span class="text-sm text-slate-500"> {t("feed.l2Distance")}</span>
            )}
          </div>
          <p class="mt-1 text-base text-slate-900">{row.body}</p>

          {row.kind === "semanticPost" && (
            <div class="mt-3 rounded-lg bg-slate-50 p-3">
              <h3 class="text-xs font-semibold uppercase tracking-wide text-slate-500">
                {t("feed.vectorDistance")}
              </h3>
              <p class="mt-1 text-sm text-slate-700">
                {t("feed.l2DistanceValue", { value: row.distance })}
              </p>
            </div>
          )}

          {row.kind === "topicPath" && (
            <div class="mt-3 rounded-lg bg-slate-50 p-3">
              <h3 class="text-xs font-semibold uppercase tracking-wide text-slate-500">
                {t("feed.relationshipTrail")}
              </h3>
              <ul class="mt-2 space-y-1 text-sm text-slate-700">
                <li>
                  {t("feed.followerEdge")} {" "}
                  <code class="rounded bg-slate-200 px-1 py-0.5 text-xs">{row.followsEdgeId}</code>
                </li>
                <li>
                  {t("feed.secondFollowerEdge")} {" "}
                  <code class="rounded bg-slate-200 px-1 py-0.5 text-xs">{row.secondFollowsEdgeId}</code>
                </li>
                <li>
                  {t("feed.authorPostEdge")} {" "}
                  <code class="rounded bg-slate-200 px-1 py-0.5 text-xs">{row.postedEdgeId}</code>{" "}
                  {t("feed.edgeOn")} <span class="font-medium">{row.body}</span>
                </li>
                <li>
                  {t("feed.postTopicEdge")} {" "}
                  <code class="rounded bg-slate-200 px-1 py-0.5 text-xs">{row.topicEdgeId}</code> to
                  {t("feed.edgeToTopic")} <span class="font-medium">{displayPostId(row.topicId)}</span>
                </li>
              </ul>
              <p class="mt-2 text-xs text-slate-500">
                {t("feed.seedLabelsNote")}
              </p>
            </div>
          )}
        </div>
      </div>
    </article>
  );
}
