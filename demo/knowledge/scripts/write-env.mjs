// Writes demo/knowledge/.env.local from CLI-written Gleaph state so the browser page can
// reach the live Router through the generated client (@icp-sdk/core transport):
//
//   VITE_GLEAPH_ROUTER_CANISTER_ID  the Router principal
//   VITE_IC_HOST                    the local gateway base URL
//   VITE_FETCH_ROOT_KEY             true (a local network serves a self-signed root key)
//
// Router-id SSOT precedence:
//   1. --canister <PRINCIPAL> flag
//   2. GLEAPH_CANISTER environment variable (the bootstrap banner exports it)
//   3. The per-user cache file .gleaph/cache/account/<env>.router.json (populated by lazy
//      issuance, ADR 0068 — only exists on Account-based networks)
//
// Gateway URL precedence (mirrors README):
//   1. --gateway-url <URL> flag
//   2. GLEAPH_GATEWAY_URL environment variable
//   3. The Gleaph-owned launcher status file ($TMPDIR/gleaph-local-status/status.json),
//      which `gleaph network start` writes with the real gateway_port (future pure-CLI
//      path, GAP-2026-08-24-006)
//   4. Built-in default: http://localhost:8000 (the port the launcher is spawned with)
//
// State inputs (only consulted when no explicit canister is supplied):
//   .gleaph/data/mappings/<env>.ids.json     written by `gleaph network start`
//   .gleaph/cache/account/<env>.router.json  written after the first remote CLI command
//
// Usage: pnpm write-env [--canister <PRINCIPAL>] [--gateway-url <URL>] [--environment <NAME>]
//
// Run with `node --test scripts/write-env.test.mjs` for the assertions.

import { parseArgs } from "node:util";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const DEMO_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const DEFAULT_GATEWAY_URL = "http://localhost:8000";
const LAUNCHER_STATUS_DIRNAME = "gleaph-local-status";

/** Pure: derive the output file content. Exported for tests. */
export function buildEnvContent({ routerCanisterId, icHost, fetchRootKey }) {
  return [
    `VITE_GLEAPH_ROUTER_CANISTER_ID=${routerCanisterId}`,
    `VITE_IC_HOST=${icHost}`,
    `VITE_FETCH_ROOT_KEY=${fetchRootKey}`,
    "",
  ].join("\n");
}

/** Pure: gateway URL precedence flag > env > launcher status file > default. */
export function resolveGatewayUrl({
  flagUrl,
  envUrl,
  launcherStatusPath,
  readFile = readFileSync,
}) {
  if (flagUrl) return stripTrailingSlash(flagUrl);
  if (envUrl) return stripTrailingSlash(envUrl);
  let raw;
  try {
    raw = readFile(launcherStatusPath, "utf8");
  } catch (error) {
    // An absent launcher status file simply means "no recorded port yet"; anything else
    // must surface instead of silently degrading to the default.
    if (error && error.code === "ENOENT") return DEFAULT_GATEWAY_URL;
    throw new Error(`read launcher status file ${launcherStatusPath}: ${error}`);
  }
  let status;
  try {
    status = JSON.parse(raw);
  } catch (error) {
    throw new Error(
      `launcher status file ${launcherStatusPath} is not valid JSON (${error}); ` +
        "rerun `gleaph network start` or pass --gateway-url",
    );
  }
  if (typeof status.gateway_port !== "number") {
    throw new Error(
      `launcher status file ${launcherStatusPath} carries no gateway_port; ` +
        "rerun `gleaph network start` or pass --gateway-url",
    );
  }
  return `http://localhost:${status.gateway_port}`;
}

function stripTrailingSlash(url) {
  return url.endsWith("/") ? url.slice(0, -1) : url;
}

/** Pure: locate the CLI-written state inputs. The mapping file is only required when the
 * Router id must come from the lazy-issuance cache (Account-based networks); explicit
 * canister mode skips it because no Account is deployed there. */
export function resolveStateInputs({
  demoRoot = DEMO_ROOT,
  environment = "local",
  canisterId,
}) {
  const mappingsPath = join(demoRoot, ".gleaph", "data", "mappings", `${environment}.ids.json`);
  if (!canisterId && !existsSync(mappingsPath)) {
    throw new Error(
      `${mappingsPath} not found; run \`gleaph network start\` first, or pass ` +
        "--canister / set GLEAPH_CANISTER (explicit-canister mode has no Account mapping)",
    );
  }
  const routerCachePath = join(
    demoRoot,
    ".gleaph",
    "cache",
    "account",
    `${environment}.router.json`,
  );
  if (!canisterId && !existsSync(routerCachePath)) {
    throw new Error(
      `${routerCachePath} not found; run migration apply first (the Router id is issued ` +
        "lazily on the first remote CLI command), or pass --canister / GLEAPH_CANISTER",
    );
  }
  return { mappingsPath, routerCachePath };
}

/** Read the cached Router principal text (the file holds one JSON string). */
export function readRouterCanisterId(routerCachePath) {
  const routerId = JSON.parse(readFileSync(routerCachePath, "utf8"));
  if (typeof routerId !== "string" || routerId.length === 0) {
    throw new Error(`${routerCachePath} does not carry a Router principal string`);
  }
  return routerId;
}

/** Verify the platform mapping was written by `gleaph network start`. */
export function requirePlatformMapping(mappingsPath) {
  const mapping = JSON.parse(readFileSync(mappingsPath, "utf8"));
  if (typeof mapping.account !== "string" || typeof mapping.provision !== "string") {
    throw new Error(`${mappingsPath} is missing account/provision entries; rerun \`gleaph network start\``);
  }
  return mapping;
}

async function runCli(argv) {
  const { values } = parseArgs({
    args: argv,
    options: {
      canister: { type: "string" },
      "gateway-url": { type: "string" },
      environment: { type: "string", default: process.env.GLEAPH_ENVIRONMENT ?? "local" },
    },
  });
  const envPath = await writeEnvFile({
    demoRoot: DEMO_ROOT,
    environment: values.environment,
    flagCanisterId: values.canister,
    envCanisterId: process.env.GLEAPH_CANISTER,
    flagGatewayUrl: values["gateway-url"],
    envGatewayUrl: process.env.GLEAPH_GATEWAY_URL,
  });
  console.log(`wrote ${envPath}`);
}

/**
 * Resolve every input and write .env.local. Returns the output path. Exported for tests.
 */
export async function writeEnvFile({
  demoRoot,
  environment,
  flagCanisterId,
  envCanisterId,
  flagGatewayUrl,
  envGatewayUrl,
  readFile = readFileSync,
  writeFile = writeFileSync,
}) {
  const canisterId = flagCanisterId ?? envCanisterId;
  const { mappingsPath, routerCachePath } = resolveStateInputs({
    demoRoot,
    environment,
    canisterId,
  });
  let routerCanisterId;
  if (canisterId) {
    // Explicit-canister mode: the bootstrap-exported GLEAPH_CANISTER is the SSOT; no
    // Account mapping or lazy-issuance cache exists on this network shape.
    routerCanisterId = canisterId;
  } else {
    requirePlatformMapping(mappingsPath);
    routerCanisterId = readRouterCanisterId(routerCachePath);
  }
  const launcherStatusPath = join(tmpdir(), `${LAUNCHER_STATUS_DIRNAME}`, "status.json");
  const icHost = resolveGatewayUrl({
    flagUrl: flagGatewayUrl,
    envUrl: envGatewayUrl,
    launcherStatusPath,
    readFile,
  });
  const envPath = join(demoRoot, ".env.local");
  writeFile(
    envPath,
    buildEnvContent({ routerCanisterId, icHost, fetchRootKey: true }),
  );
  return envPath;
}

const invokedDirectly =
  process.argv[1] !== undefined &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href;
if (invokedDirectly) {
  await runCli(process.argv.slice(2));
}
