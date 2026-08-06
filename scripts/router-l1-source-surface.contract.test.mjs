import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

// Source-level L1 surface contract: the Router client/types, the JS SDK, the Rust
// CDK, and the CLI load driver must expose only the replacement names. The live
// Candid surface (call kinds and retired symbols on the deployed canisters) is
// covered separately by `scripts/check-router-and-graph-candid.sh`, which extracts
// the .did from freshly built wasm — the SDK-direct frontend commits no actor
// bindings, so there is no checked-in .did for this test to read.

const root = new URL("../", import.meta.url);

async function source(relativePath) {
  return readFile(new URL(relativePath, root), "utf8");
}

test("Router L1 source surfaces expose only the replacement names", async () => {
  const retiredGqlMethod = ["gql_execute", "batch"].join("_");
  const [
    client,
    types,
    sdkIndex,
    sdkAtomic,
    sdkBulk,
    sdkIdl,
    sdkClient,
    sdkIc,
    cdk,
    bulkApi,
  ] = await Promise.all([
    source("crates/router/src/api/client.rs"),
    source("crates/router/src/types.rs"),
    source("sdk/client/js/src/index.ts"),
    source("sdk/client/js/src/atomic.ts"),
    source("sdk/client/js/src/bulk.ts"),
    source("sdk/client/js/src/idl.ts"),
    source("sdk/client/js/src/client.ts"),
    source("sdk/client/js/src/ic.ts"),
    source("sdk/canister/rust/src/lib.rs"),
    source("crates/bulk-load-api/src/lib.rs"),
  ]);

  assert.match(
    client,
    /async\s+fn\s+bulk_load\s*\(/,
    "bulk_load update entrypoint",
  );
  assert.match(
    client,
    /fn\s+bulk_load_status\s*\(/,
    "bulk_load_status query entrypoint",
  );
  // The bulk-load wire enums live in gleaph-bulk-load-api (ADR 0057/0060) and are re-exported
  // from Router types.rs; assert both the owning crate and the Router re-export.
  assert.match(
    bulkApi,
    /enum\s+BulkLoadCommand\b/,
    "BulkLoadCommand wire type",
  );
  assert.match(
    bulkApi,
    /enum\s+BulkLoadResponse\b/,
    "BulkLoadResponse wire type",
  );
  assert.match(
    types,
    /pub use gleaph_bulk_load_api::[\s\S]*\bBulkLoadCommand\b/,
    "Router re-exports BulkLoadCommand",
  );
  assert.match(sdkBulk, /makeBulkLoadCommand/);
  assert.match(sdkClient, /bulkLoadStatus/);
  assert.match(sdkIc, /bulk_load_status/);
  assert.match(cdk, /pub async fn bulk_load(?:<[^>]+>)?\s*\(/);

  for (const [label, text] of [
    ["Router client entrypoints", client],
    ["Router public types", types],
    ["JS SDK index", sdkIndex],
    ["JS SDK atomic insert builder", sdkAtomic],
    ["JS SDK bulk-load builder", sdkBulk],
    ["JS SDK IDL", sdkIdl],
    ["JS SDK client", sdkClient],
    ["JS SDK IC transport", sdkIc],
    ["Rust CDK", cdk],
  ]) {
    for (const oldName of [
      "batch_insert",
      "get_mutation_status",
      "execute_prepared_update",
      "prepared_update_idempotent",
    ]) {
      assert.doesNotMatch(
        text,
        new RegExp(`\\b${oldName}\\b`),
        `${label}: ${oldName}`,
      );
    }
  }
  assert.doesNotMatch(
    client,
    /\basync\s+fn\s+gql_execute\s*\(/,
    "gql_execute entrypoint",
  );
  assert.doesNotMatch(
    client,
    /\basync\s+fn\s+execute_prepared\s*\(/,
    "execute_prepared entrypoint",
  );
  assert.match(sdkAtomic, /makeAtomicInsertRequest/);
  // The social-demo loader is the Gleaph CLI `load` driver, which owns the durable
  // bulk_load lifecycle (Start/Append/Finalize/status) and receipt-based resume.
  const socialLoader = await source("crates/cli/src/load.rs");
  assert.match(socialLoader, /bulk_load/);
  assert.match(socialLoader, /bulk_load_status/);
  assert.match(socialLoader, /resume/);
  assert.doesNotMatch(
    socialLoader,
    new RegExp(`${retiredGqlMethod}|ELEMENT_ID`),
  );
});
