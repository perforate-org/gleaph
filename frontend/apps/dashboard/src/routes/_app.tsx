import { Outlet, createFileRoute, redirect } from "@tanstack/solid-router";

import { AppShell } from "~/components/app-shell";
import { isAuthenticated } from "~/lib/auth";

export const Route = createFileRoute("/_app")({
  beforeLoad: () => {
    if (!isAuthenticated()) {
      throw redirect({ to: "/login" });
    }
  },
  component: AppLayout,
});

function AppLayout() {
  return (
    <AppShell>
      <Outlet />
    </AppShell>
  );
}
