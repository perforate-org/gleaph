import { Link } from "@tanstack/solid-router";
import type { JSX } from "solid-js";

import { Button } from "~/components/ui/button";
import { signOut } from "~/lib/auth";

const nav = [
  { to: "/", label: "Overview" },
  { to: "/prepared", label: "Prepared" },
  { to: "/settings/roles", label: "Roles" },
  { to: "/query", label: "Query" },
] as const;

export function AppShell(props: { children: JSX.Element }) {
  return (
    <div class="flex min-h-screen">
      <aside class="flex w-56 flex-col border-r bg-card p-4">
        <div class="mb-6 text-lg font-semibold">Gleaph</div>
        <nav class="flex flex-1 flex-col gap-1">
          {nav.map((item) => (
            <Link
              to={item.to}
              class="rounded-md px-3 py-2 text-sm hover:bg-accent"
              activeProps={{ class: "bg-accent font-medium" }}
            >
              {item.label}
            </Link>
          ))}
        </nav>
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            signOut();
            window.location.href = "/login";
          }}
        >
          Sign out
        </Button>
      </aside>
      <main class="flex-1 p-8">{props.children}</main>
    </div>
  );
}
