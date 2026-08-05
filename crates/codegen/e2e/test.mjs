import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createPrivateKey } from "node:crypto";
import { pathToFileURL } from "node:url";
import { Secp256k1KeyIdentity } from "../../../sdk/client/js/node_modules/@icp-sdk/core/lib/esm/identity/secp256k1/secp256k1.js";
import { createGleaphClient } from "../../../sdk/client/js/dist/index.mjs";

const generatedPath = process.env.GLEAPH_CODEGEN_OUTPUT;
const routerCanister = process.env.GLEAPH_ROUTER_CANISTER;
const identityPem = process.env.GLEAPH_CODEGEN_IDENTITY_PEM;
if (!generatedPath || !routerCanister || !identityPem) {
  throw new Error("GLEAPH_CODEGEN_OUTPUT and GLEAPH_ROUTER_CANISTER are required");
}

const generated = await import(pathToFileURL(generatedPath).href);
const key = createPrivateKey(await readFile(identityPem, "utf8"));
const jwk = key.export({ format: "jwk" });
const secretKey = Uint8Array.from(Buffer.from(jwk.d, "base64url"));
const identity = Secp256k1KeyIdentity.fromSecretKey(secretKey);
const graph = await createGleaphClient({
  canisterId: routerCanister,
  host: "http://localhost:8000",
  identity,
  fetchRootKey: true,
});
const prepared = generated.withPreparedQueries(graph);
const response = await prepared["list-vertices"]({}, [{ key: "name", direction: "desc" }]);

assert.equal(response.rows.length, 0);
console.log("codegen local E2E passed");
