import { createFileRoute, Link } from "@tanstack/solid-router";

export const Route = createFileRoute("/_app/prepared/$id")({
  component: PreparedDetailPage,
});

function PreparedDetailPage() {
  const params = Route.useParams();
  return (
    <div class="space-y-4">
      <Link to="/prepared" class="text-sm text-primary underline">
        ← Prepared queries
      </Link>
      <h1 class="font-mono text-2xl font-semibold">{params().id}</h1>
      <p class="text-muted-foreground">
        Detail editor placeholder (Phase 3).
      </p>
    </div>
  );
}
