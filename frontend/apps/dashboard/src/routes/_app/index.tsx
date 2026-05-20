import { createFileRoute } from "@tanstack/solid-router";

import { Button } from "~/components/ui/button";

export const Route = createFileRoute("/_app/")({
  component: OverviewPage,
});

function OverviewPage() {
  return (
    <div class="space-y-4">
      <h1 class="text-2xl font-semibold">Overview</h1>
      <p class="text-muted-foreground">
        Tenant administration for Gleaph router and prepared queries.
      </p>
      <Button variant="outline" disabled>
        Connect router canister (Phase 2)
      </Button>
    </div>
  );
}
