# SDK Workspace

`sdk/` contains publishable client SDK packages for Gleaph.

Initial layout:

- `sdk/js`
  - JS/TS-facing SDK runtime
  - typed DTOs for the graph canister API
  - helpers for `USE GRAPH` pushdown capability and warning handling
  - future home of IC transport and prepared-query runtime

Planned follow-up:

1. Add IC transport implementation on top of `@icp-sdk/core`
2. Point `gleaph-codegen` TS output at `@gleaph/sdk`
3. Add generated prepared-query package or generated source tree
4. Add `frontend/` app that consumes the SDK through the workspace
