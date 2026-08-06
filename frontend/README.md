# Gleaph frontend

Solid CSR apps for Gleaph operator UIs. Legacy UI lives in `frontend-old/` (ignored).

## Stack

| Layer     | Choice                                                                                       |
| --------- | -------------------------------------------------------------------------------------------- |
| Workspace | pnpm (`frontend/apps/*`)                                                                     |
| Build     | Vite                                                                                         |
| UI        | Solid + [solid-ui](https://www.solid-ui.com/) (Tailwind 3)                                   |
| Routing   | [@tanstack/solid-router](https://tanstack.com/router) file-based + `@tanstack/router-plugin` |
| SDK       | Optional `@gleaph/sdk` in apps; primary consumer is user dapps                               |

## Apps

| Package               | Path                 | Audience                             |
| --------------------- | -------------------- | ------------------------------------ |
| `@gleaph/dashboard`   | `apps/dashboard`     | Tenant admins (Manager/Admin)        |
| `@gleaph/social-demo` | `../../demo/social`  | Public graph comparison demo viewers |
| `@gleaph/ops`         | `apps/ops` (planned) | Internal operators                   |

## `apps/dashboard` route map

Directory routes + pathless `_app` (authenticated shell). Public routes sit beside `_app`.

| URL               | File                                 | Notes                                   |
| ----------------- | ------------------------------------ | --------------------------------------- |
| `/`               | `src/routes/_app/index.tsx`          | Overview (auth required)                |
| `/login`          | `src/routes/login.tsx`               | II stub; redirects if already signed in |
| `/prepared`       | `src/routes/_app/prepared/index.tsx` | Prepared query list                     |
| `/prepared/:id`   | `src/routes/_app/prepared/$id.tsx`   | Detail / edit                           |
| `/settings/roles` | `src/routes/_app/settings/roles.tsx` | RBAC                                    |
| `/query`          | `src/routes/_app/query.tsx`          | Read-only GQL (Read+)                   |

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
pnpm --filter @gleaph/social-demo dev
pnpm --filter @gleaph/social-demo build
pnpm social-demo:check
pnpm social-demo:build
```

`@gleaph/social-demo` talks to the Router canister directly through `@gleaph/sdk` (prepared
queries by global name). It reads the Router principal from `VITE_GLEAPH_ROUTER_CANISTER_ID`;
in `pnpm --filter @gleaph/social-demo dev` that comes from `demo/social/.env.local`, and the
deploy scripts write the file before the frontend build so the baked bundle carries the Router id.
`demo/social/scripts/deploy-local.sh` writes that file automatically; set
`GLEAPH_DEMO_SKIP_VITE_ENV=1` to opt out (the file is gitignored, so this only matters for shared
checkouts that want to keep `.env.local` untouched across runs).

`GLEAPH_DEMO_FORCE_VITE_IC_HOST=1` additionally overwrites the cached `VITE_IC_HOST` to the current local replica URL (useful when the docker `0:4943` host port drifts between sessions; default keeps a hand-pinned host stable for CI). If the local replica is not reachable at deploy time, the script logs a warning and leaves the existing `.env.local` alone.

The repository root `icp.yaml` builds the Gleaph platform canisters (Router/Index/Graph/Vector).
The social-demo application canisters live in `demo/social/icp.yaml`. `scripts/deploy-demo-local.sh`
deploys the platform bootstrap and then runs the demo's own flow
(`demo/social/scripts/deploy-local.sh`: `gleaph migration apply` + `gleaph load` +
`gleaph prepared apply` + `gleaph codegen`); set `GLEAPH_DEMO_DIR` to point the same
bootstrap at another demo directory.

The local deploy script uses the named `gleaph-demo-deployer` identity for canister creation and
calls. Before creating the stack it transfers `1_000T` of fabricated local cycles from the seeded
`anonymous` identity when the deployer balance is below the bootstrap budget; no mainnet ICP or
manual `icp cycles mint` step is required. The amount covers the six canister creates and the
script's per-canister `100T` top-ups.

If the local network is already managed outside the script, set `GLEAPH_DEMO_SKIP_NETWORK_START=1`; the script will require the `local` environment to be running before it proceeds.

## Phases

1. **Done (scaffold)** — Vite, Solid, TanStack file routes, solid-ui `Button`, route stubs
2. **Shell** — Internet Identity + `@icp-sdk/core`, real `beforeLoad`
3. **Screens** — prepared list, roles, router admin API wiring
4. **ops** — clone template under `apps/ops`

## Conventions

- Import alias: `~/` → `src/`
- Route files prefixed with `-` are ignored by the router plugin
- Add UI via `pnpm --filter @gleaph/dashboard exec solidui-cli add <component>`
