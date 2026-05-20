import {
  Link,
  Outlet,
  createRootRoute,
} from "@tanstack/solid-router";

export const Route = createRootRoute({
  component: RootComponent,
  notFoundComponent: NotFound,
});

function RootComponent() {
  return <Outlet />;
}

function NotFound() {
  return (
    <div class="flex min-h-screen flex-col items-center justify-center gap-4">
      <p class="text-muted-foreground">Page not found</p>
      <Link to="/" class="text-primary underline">
        Go home
      </Link>
    </div>
  );
}
