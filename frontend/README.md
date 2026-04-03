# Frontend

`frontend/` holds the shared design system package and separate workspace apps:

- `design/`: CSS design tokens (`@gleaph/design`) and `figma/variables.manifest.json` for Figma WEB code syntax (`var(--…)`)
- `product/`: the public product website
- `dashboard/`: the authenticated user dashboard

The top-level `pnpm-workspace.yaml` includes both directories directly, so each
can evolve as its own app/package without sharing an artificial parent package.

Current intent:

- `product` stays light and marketing-focused
- `dashboard` is the main consumer of `@gleaph/sdk` and generated prepared clients
