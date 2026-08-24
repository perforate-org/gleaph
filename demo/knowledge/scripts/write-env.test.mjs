// Assertions for scripts/write-env.mjs. Run: node --test scripts/write-env.test.mjs

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync as readTextSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  buildEnvContent,
  readRouterCanisterId,
  requirePlatformMapping,
  resolveGatewayUrl,
  resolveStateInputs,
  writeEnvFile,
} from "./write-env.mjs";

const STATUS_PATH = "/tmp/fake-status/status.json";

test("buildEnvContent emits exactly the three VITE_ variables in order", () => {
  const content = buildEnvContent({
    routerCanisterId: "r7inp-6aaaa-aaaaa-aaabq-cai",
    icHost: "http://localhost:32768",
    fetchRootKey: true,
  });
  const lines = content.split("\n");
  // Trailing newline yields one empty final element.
  assert.deepEqual(lines.slice(0, -1), [
    "VITE_GLEAPH_ROUTER_CANISTER_ID=r7inp-6aaaa-aaaaa-aaabq-cai",
    "VITE_IC_HOST=http://localhost:32768",
    "VITE_FETCH_ROOT_KEY=true",
  ]);
});

test("gateway URL precedence: flag beats env beats status file beats default", () => {
  const readFile = () => JSON.stringify({ v: "1", gateway_port: 4943 });
  assert.equal(
    resolveGatewayUrl({
      flagUrl: "http://localhost:9000/",
      envUrl: "http://localhost:8000",
      launcherStatusPath: STATUS_PATH,
      readFile,
    }),
    "http://localhost:9000",
  );
  assert.equal(
    resolveGatewayUrl({
      flagUrl: undefined,
      envUrl: "http://localhost:8000",
      launcherStatusPath: STATUS_PATH,
      readFile,
    }),
    "http://localhost:8000",
  );
  assert.equal(
    resolveGatewayUrl({
      flagUrl: undefined,
      envUrl: undefined,
      launcherStatusPath: STATUS_PATH,
      readFile,
    }),
    "http://localhost:4943",
  );
});

const enoent = () => {
  throw Object.assign(new Error("absent"), { code: "ENOENT" });
};

test("absent status file falls back to the launcher default port", () => {
  let calls = 0;
  const counting = (path) => {
    calls += 1;
    enoent(path);
  };
  assert.equal(
    resolveGatewayUrl({
      flagUrl: undefined,
      envUrl: undefined,
      launcherStatusPath: "/does/not/exist/status.json",
      readFile: counting,
    }),
    "http://localhost:8000",
  );
  assert.equal(calls, 1, "the default must come after consulting the status file");
});

test("corrupt or incomplete status files fail loudly instead of guessing", () => {
  assert.throws(
    () =>
      resolveGatewayUrl({
        flagUrl: undefined,
        envUrl: undefined,
        launcherStatusPath: STATUS_PATH,
        readFile: () => "not json{",
      }),
    /not valid JSON/,
  );
  assert.throws(
    () =>
      resolveGatewayUrl({
        flagUrl: undefined,
        envUrl: undefined,
        launcherStatusPath: STATUS_PATH,
        readFile: () => JSON.stringify({ v: "1" }),
      }),
    /gateway_port/,
  );
});

function fixtureDemoRoot() {
  const root = mkdtempSync(join(tmpdir(), "gleaph-write-env-"));
  const mappingsDir = join(root, ".gleaph", "data", "mappings");
  mkdirSync(mappingsDir, { recursive: true });
  writeFileSync(
    join(mappingsDir, "local.ids.json"),
    JSON.stringify({ account: "acct-cai", provision: "prov-cai" }),
  );
  return root;
}

test("resolveStateInputs fails with next-step messages before any env write", () => {
  const absentMapping = mkdtempSync(join(tmpdir(), "gleaph-write-env-"));
  assert.throws(() => resolveStateInputs({ demoRoot: absentMapping }), /network start/);
  rmSync(absentMapping, { recursive: true, force: true });

  const absentRouter = fixtureDemoRoot();
  assert.throws(
    () => resolveStateInputs({ demoRoot: absentRouter }),
    /run migration apply first/,
  );
  rmSync(absentRouter, { recursive: true, force: true });
});

test("router cache and platform mapping are validated as their canonical shapes", () => {
  const root = fixtureDemoRoot();
  const cacheDir = join(root, ".gleaph", "cache", "account");
  mkdirSync(cacheDir, { recursive: true });
  // Router cache holds one JSON string (config::write_router_cache format).
  const routerCachePath = join(cacheDir, "local.router.json");
  writeFileSync(routerCachePath, JSON.stringify("r7inp-6aaaa-aaaaa-aaabq-cai"));
  assert.equal(readRouterCanisterId(routerCachePath), "r7inp-6aaaa-aaaaa-aaabq-cai");

  writeFileSync(routerCachePath, JSON.stringify(42));
  assert.throws(() => readRouterCanisterId(routerCachePath), /principal string/);

  const mappingsPath = join(root, ".gleaph", "data", "mappings", "local.ids.json");
  writeFileSync(mappingsPath, JSON.stringify({ account: "acct-cai" }));
  assert.throws(() => requirePlatformMapping(mappingsPath), /account\/provision entries/);
  rmSync(root, { recursive: true, force: true });
});

test("writeEnvFile produces a complete .env.local from post-migration state", async () => {
  const root = fixtureDemoRoot();
  const cacheDir = join(root, ".gleaph", "cache", "account");
  mkdirSync(cacheDir, { recursive: true });
  const routerCachePath = join(cacheDir, "local.router.json");
  writeFileSync(routerCachePath, JSON.stringify("r7inp-6aaaa-aaaaa-aaabq-cai"));

  const envPath = await writeEnvFile({
    demoRoot: root,
    environment: "local",
    flagGatewayUrl: undefined,
    envGatewayUrl: undefined,
    readFile: (path) =>
      path.endsWith("status.json") ? JSON.stringify({ gateway_port: 4321 }) : "",
    writeFile: (path, text) => writeFileSync(path, text),
  });

  const content = readTextSync(envPath, "utf8");
  assert.match(content, /^VITE_GLEAPH_ROUTER_CANISTER_ID=r7inp-6aaaa-aaaaa-aaabq-cai$/m);
  assert.match(content, /^VITE_IC_HOST=http:\/\/localhost:4321$/m);
  assert.match(content, /^VITE_FETCH_ROOT_KEY=true$/m);
  rmSync(root, { recursive: true, force: true });
});

test("writeEnvFile refuses cleanly when the router cache is missing", async () => {
  const root = fixtureDemoRoot();
  await assert.rejects(
    () =>
      writeEnvFile({
        demoRoot: root,
        environment: "local",
        flagGatewayUrl: undefined,
        envGatewayUrl: undefined,
      }),
    /run migration apply first/,
  );
  rmSync(root, { recursive: true, force: true });
});

test("explicit GLEAPH_CANISTER bypasses mapping and cache requirements", async () => {
  // No .gleaph state at all: Account-less networks carry neither mapping nor cache.
  const root = mkdtempSync(join(tmpdir(), "gleaph-write-env-"));
  const envPath = await writeEnvFile({
    demoRoot: root,
    environment: "local",
    envCanisterId: "r7inp-6aaaa-aaaaa-aaabq-cai",
    flagGatewayUrl: "http://localhost:32768/",
    envGatewayUrl: undefined,
    readFile: () => {
      throw Object.assign(new Error("no status file"), { code: "ENOENT" });
    },
    writeFile: (path, text) => writeFileSync(path, text),
  });
  const content = readTextSync(envPath, "utf8");
  assert.match(content, /^VITE_GLEAPH_ROUTER_CANISTER_ID=r7inp-6aaaa-aaaaa-aaabq-cai$/m);
  assert.match(content, /^VITE_IC_HOST=http:\/\/localhost:32768$/m);
  rmSync(root, { recursive: true, force: true });
});
