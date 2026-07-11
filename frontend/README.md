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
| `@gleaph/knowledge-map` | `apps/knowledge-map` | Non-technical visual demo viewers |
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
pnpm --filter @gleaph/knowledge-map dev
pnpm --filter @gleaph/knowledge-map build
pnpm knowledge-map:check
pnpm knowledge-map:build
icp build knowledge-map
scripts/deploy-knowledge-map-local.sh
```

`@gleaph/knowledge-map` defaults to Router-shaped fixture data. To enable its live Router scenario during Vite development, copy `apps/knowledge-map/.env.example` and set:

```bash
VITE_ROUTER_CANISTER_ID=<router-canister-id>
VITE_IC_HOST=<local-gateway-or-replica-url>
VITE_FETCH_ROOT_KEY=true
```

When deployed from an IC asset canister, the app first reads `PUBLIC_CANISTER_ID:gleaph-router` or `PUBLIC_CANISTER_ID:router` from the `ic_env` cookie injected by `icp deploy`.

`@gleaph/social-demo` reads the deployed Gateway canister id from `PUBLIC_CANISTER_ID:gleaph-social-demo-gateway` (asset-canister cookie) or, in `pnpm --filter @gleaph/social-demo dev`, from `frontend/apps/social-demo/.env.local`. `scripts/deploy-social-demo-local.sh` writes that file automatically after deploying the Gateway; set `GLEAPH_DEMO_SKIP_VITE_ENV=1` to opt out (the file is gitignored, so this only matters for shared checkouts that want to keep `.env.local` untouched across runs).

`GLEAPH_DEMO_FORCE_VITE_IC_HOST=1` additionally overwrites the cached `VITE_IC_HOST` to the current local replica URL (useful when the docker `0:4943` host port drifts between sessions; default keeps a hand-pinned host stable for CI). If the local replica is not reachable at deploy time, the script logs a warning and leaves the existing `.env.local` alone.

The repository root `icp.yaml` builds the `knowledge-map` asset canister and the Gleaph Router/Index/Graph canisters. `scripts/deploy-knowledge-map-local.sh` starts the local IC network, installs the Rust canisters with the required init args, registers the demo graph/shard through Router, seeds the Alice fan-out knowledge-map graph (24 nodes, 26 edges) through Router GQL, and deploys the asset canister.

If the local network is already managed outside the script, set `GLEAPH_DEMO_SKIP_NETWORK_START=1`; the script will require the `local` environment to be running before it proceeds.

Set `GLEAPH_DEMO_VERIFY_QUERY=1` to make the deploy script run the Router `gql_query` smoke check after seeding.

## Phases

1. **Done (scaffold)** — Vite, Solid, TanStack file routes, solid-ui `Button`, route stubs
2. **Shell** — Internet Identity + `@icp-sdk/core`, real `beforeLoad`
3. **Screens** — prepared list, roles, router admin API wiring
4. **ops** — clone template under `apps/ops`

## Conventions

- Import alias: `~/` → `src/`
- Route files prefixed with `-` are ignored by the router plugin
- Add UI via `pnpm --filter @gleaph/dashboard exec solidui-cli add <component>`
