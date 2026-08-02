import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);

async function source(relativePath) {
  return readFile(new URL(relativePath, root), "utf8");
}

test("Router L1 runtime and generated surfaces expose only the replacement names", async () => {
  const retiredGqlMethod = ["gql_execute", "batch"].join("_");
  const retiredGqlType = ["GqlExecuteIdempotent", "Batch"].join("");
  const [client, types, routerDid, sdkIndex, sdkAtomic, sdkBulk, sdkIdl, sdkClient, sdkIc, cdk, gateway] = await Promise.all([
    source("crates/router/src/api/client.rs"),
    source("crates/router/src/types.rs"),
    source("frontend/apps/social-demo/src/generated/gleaph_router/declarations/gleaph_router.did"),
    source("sdk/client/js/src/index.ts"),
    source("sdk/client/js/src/atomic.ts"),
    source("sdk/client/js/src/bulk.ts"),
    source("sdk/client/js/src/idl.ts"),
    source("sdk/client/js/src/client.ts"),
    source("sdk/client/js/src/ic.ts"),
    source("sdk/canister/rust/src/lib.rs"),
    source("crates/social-demo-gateway/src/lib.rs"),
  ]);

  for (const symbol of [
    "gql_query",
    "gql_mutate",
    "prepared_query",
    "prepared_mutate",
    "atomic_insert",
    "mutation_status",
    "atomic_insert_status",
    "bulk_load",
    "bulk_load_status",
  ]) {
    assert.match(routerDid, new RegExp(`^[ \\t]*${symbol}\\s*:`, "m"), symbol);
  }
  const declaration = (symbol) => {
    const match = new RegExp(`^[ \\t]*${symbol}\\s*:[\\s\\S]*?;`, "m").exec(routerDid);
    assert.ok(match, `missing Candid declaration for ${symbol}`);
    return match[0];
  };
  for (const symbol of ["gql_query", "prepared_query"]) {
    assert.match(declaration(symbol), /composite_query\s*;$/m, `${symbol} call kind`);
  }
  for (const symbol of ["mutation_status", "atomic_insert_status", "bulk_load_status"]) {
    assert.match(declaration(symbol), /query\s*;$/m, `${symbol} call kind`);
  }
  for (const symbol of ["gql_mutate", "prepared_mutate", "atomic_insert", "bulk_load"]) {
    assert.doesNotMatch(declaration(symbol), /(composite_query|query)\s*;$/m, `${symbol} call kind`);
  }
  assert.doesNotMatch(routerDid, new RegExp(`^[ \\t]*${retiredGqlMethod}\\s*:`, "m"), `retired ${retiredGqlMethod}`);
  assert.doesNotMatch(routerDid, new RegExp(`^type\\s+${retiredGqlType}`, "m"));
  assert.match(client, /async\s+fn\s+bulk_load\s*\(/, "bulk_load update entrypoint");
  assert.match(client, /fn\s+bulk_load_status\s*\(/, "bulk_load_status query entrypoint");
  assert.match(types, /enum\s+BulkLoadCommand\b/, "BulkLoadCommand wire type");
  assert.match(types, /enum\s+BulkLoadResponse\b/, "BulkLoadResponse wire type");
  assert.match(routerDid, /^[ \t]*bulk_load\s*:/m, "generated bulk_load update");
  assert.match(routerDid, /^[ \t]*bulk_load_status\s*:/m, "generated bulk_load_status query");
  assert.match(sdkBulk, /makeBulkLoadCommand/);
  assert.match(sdkClient, /bulkLoadStatus/);
  assert.match(sdkIc, /bulk_load_status/);
  assert.match(cdk, /pub async fn bulk_load(?:<[^>]+>)?\s*\(/);

  for (const [label, text] of [
    ["Router client entrypoints", client],
    ["Router public types", types],
    ["generated Router Candid", routerDid],
    ["JS SDK index", sdkIndex],
    ["JS SDK atomic insert builder", sdkAtomic],
    ["JS SDK bulk-load builder", sdkBulk],
    ["JS SDK IDL", sdkIdl],
    ["JS SDK client", sdkClient],
    ["JS SDK IC transport", sdkIc],
    ["Rust CDK", cdk],
    ["social gateway", gateway],
  ]) {
    for (const oldName of [
      "batch_insert",
      "get_mutation_status",
      "execute_prepared_update",
      "prepared_update_idempotent",
    ]) {
      assert.doesNotMatch(text, new RegExp(`\\b${oldName}\\b`), `${label}: ${oldName}`);
    }
  }
  assert.doesNotMatch(client, /\basync\s+fn\s+gql_execute\s*\(/, "gql_execute entrypoint");
  assert.doesNotMatch(client, /\basync\s+fn\s+execute_prepared\s*\(/, "execute_prepared entrypoint");
  assert.match(sdkAtomic, /makeAtomicInsertRequest/);
  const socialLoader = await source("frontend/apps/social-demo/scripts/apply-social-load.mjs");
  assert.match(socialLoader, /bulk_load/);
  assert.match(socialLoader, /mutation_status/);
  assert.match(socialLoader, /ingestEmbeddings/);
  assert.doesNotMatch(socialLoader, new RegExp(`${retiredGqlMethod}|Post\\.demo_id|ELEMENT_ID`));
});
