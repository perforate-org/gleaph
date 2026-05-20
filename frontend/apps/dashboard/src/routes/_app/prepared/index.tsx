import { Link, createFileRoute } from "@tanstack/solid-router";

import { Button } from "~/components/ui/button";

export const Route = createFileRoute("/_app/prepared/")({
  component: PreparedListPage,
});

function PreparedListPage() {
  return (
    <div class="space-y-4">
      <div class="flex items-center justify-between">
        <h1 class="text-2xl font-semibold">Prepared queries</h1>
        <Button disabled>Register (Phase 3)</Button>
      </div>
      <p class="text-sm text-muted-foreground">
        Placeholder list — wire to router <code>prepared_*</code> APIs.
      </p>
      <ul class="divide-y rounded-md border">
        <li class="flex items-center justify-between px-4 py-3">
          <span class="font-mono text-sm">example_get_by_caller</span>
          <Link
            to="/prepared/$id"
            params={{ id: "example_get_by_caller" }}
            class="text-sm text-primary underline"
          >
            Open
          </Link>
        </li>
      </ul>
    </div>
  );
}
