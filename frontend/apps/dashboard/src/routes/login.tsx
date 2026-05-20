import { createFileRoute, redirect } from "@tanstack/solid-router";

import { Button } from "~/components/ui/button";
import { isAuthenticated, signIn } from "~/lib/auth";

export const Route = createFileRoute("/login")({
  beforeLoad: () => {
    if (isAuthenticated()) {
      throw redirect({ to: "/" });
    }
  },
  component: LoginPage,
});

function LoginPage() {
  return (
    <div class="flex min-h-screen flex-col items-center justify-center gap-4">
      <h1 class="text-2xl font-semibold">Gleaph Dashboard</h1>
      <p class="text-sm text-muted-foreground">
        Sign in with Internet Identity (stub in Phase 1)
      </p>
      <Button
        onClick={() => {
          signIn();
          window.location.href = "/";
        }}
      >
        Sign in
      </Button>
    </div>
  );
}
