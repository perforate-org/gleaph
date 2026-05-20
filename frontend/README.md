# Gleaph frontend

Solid CSR apps for Gleaph operator UIs. Legacy UI lives in `frontend-old/` (ignored).

## Stack

| Layer | Choice |
|-------|--------|
| Workspace | pnpm (`frontend/apps/*`) |
| Build | Vite |
| UI | Solid + [solid-ui](https://www.solid-ui.com/) (Tailwind 3) |
| Routing | [@tanstack/solid-router](https://tanstack.com/router) file-based + `@tanstack/router-plugin` |
| SDK | Optional `@gleaph/sdk` in apps; primary consumer is user dapps |

## Apps

| Package | Path | Audience |
|---------|------|----------|
| `@gleaph/dashboard` | `apps/dashboard` | Tenant admins (Manager/Admin) |
| `@gleaph/ops` | `apps/ops` (planned) | Internal operators |

## `apps/dashboard` route map

Directory routes + pathless `_app` (authenticated shell). Public routes sit beside `_app`.

| URL | File | Notes |
|-----|------|-------|
| `/` | `src/routes/_app/index.tsx` | Overview (auth required) |
| `/login` | `src/routes/login.tsx` | II stub; redirects if already signed in |
| `/prepared` | `src/routes/_app/prepared/index.tsx` | Prepared query list |
| `/prepared/:id` | `src/routes/_app/prepared/$id.tsx` | Detail / edit |
| `/settings/roles` | `src/routes/_app/settings/roles.tsx` | RBAC |
| `/query` | `src/routes/_app/query.tsx` | Read-only GQL (Read+) |

Layout:

- `src/routes/__root.tsx` — document shell, `<Outlet />`, 404
- `src/routes/_app.tsx` — sidebar shell, `beforeLoad` auth guard
- `src/components/app-shell.tsx` — nav links

Generated: `src/routeTree.gen.ts` (do not edit).

## Commands

From repo root:

```bash
pnpm install
pnpm --filter @gleaph/dashboard dev
pnpm --filter @gleaph/dashboard build
pnpm dashboard:check
```

## Phases

1. **Done (scaffold)** — Vite, Solid, TanStack file routes, solid-ui `Button`, route stubs
2. **Shell** — Internet Identity + `@icp-sdk/core`, real `beforeLoad`
3. **Screens** — prepared list, roles, router admin API wiring
4. **ops** — clone template under `apps/ops`

## Conventions

- Import alias: `~/` → `src/`
- Route files prefixed with `-` are ignored by the router plugin
- Add UI via `pnpm --filter @gleaph/dashboard exec solidui-cli add <component>`
