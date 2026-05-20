import { createFileRoute } from "@tanstack/solid-router";

import { Button } from "~/components/ui/button";

export const Route = createFileRoute("/_app/query")({
  component: QueryPage,
});

function QueryPage() {
  return (
    <div class="space-y-4">
      <h1 class="text-2xl font-semibold">Query</h1>
      <p class="text-sm text-muted-foreground">
        Read-only GQL for Read role and above (Phase 3).
      </p>
      <textarea
        class="min-h-40 w-full rounded-md border bg-background p-3 font-mono text-sm"
        placeholder="MATCH (n) RETURN n LIMIT 10"
        readonly
      />
      <Button disabled>Run query</Button>
    </div>
  );
}
