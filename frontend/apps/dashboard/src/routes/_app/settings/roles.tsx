import { createFileRoute } from "@tanstack/solid-router";

import { Button } from "~/components/ui/button";

export const Route = createFileRoute("/_app/settings/roles")({
  component: RolesPage,
});

function RolesPage() {
  return (
    <div class="space-y-4">
      <div class="flex items-center justify-between">
        <h1 class="text-2xl font-semibold">Roles</h1>
        <Button disabled>Grant role (Phase 3)</Button>
      </div>
      <p class="text-sm text-muted-foreground">
        RBAC via router <code>admin_grant_role</code> — table placeholder.
      </p>
      <div class="rounded-md border px-4 py-8 text-center text-muted-foreground">
        No principals loaded
      </div>
    </div>
  );
}
