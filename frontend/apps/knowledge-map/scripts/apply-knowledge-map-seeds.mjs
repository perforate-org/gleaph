import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

const seedsPath = process.argv[2]
  ? resolve(process.argv[2])
  : join(root, "seeds/knowledge-map-seeds.json");
const canisterName = process.argv[3] ?? "gleaph-router";
const methodName = process.argv[4] ?? "gql_execute_idempotent";

const seeds = JSON.parse(readFileSync(seedsPath, "utf8")).seeds;

const icpEnv = () => ({
  ...process.env,
  HOME: process.env.ICP_CLI_HOME ?? process.env.HOME ?? "",
  COREPACK_HOME: process.env.ICP_COREPACK_HOME ?? "",
  XDG_CACHE_HOME: process.env.ICP_XDG_CACHE_HOME ?? "",
  XDG_DATA_HOME: process.env.ICP_XDG_DATA_HOME ?? "",
  DO_NOT_TRACK: process.env.DO_NOT_TRACK ?? "1",
});

const escapeCandidText = (s) => s.replace(/\\/g, "\\\\").replace(/"/g, '\\"');

for (const seed of seeds) {
  const candid = `(\"${escapeCandidText(seed.gql)}\", vec {}, \"${escapeCandidText(seed.key)}\")`;
  const result = spawnSync(
    "icp",
    [
      "canister",
      "call",
      "-e",
      "local",
      ...(process.env.ICP_IDENTITY_NAME
        ? ["--identity", process.env.ICP_IDENTITY_NAME]
        : []),
      canisterName,
      methodName,
      candid,
    ],
    {
      env: icpEnv(),
      encoding: "utf8",
    },
  );

  if (result.status !== 0) {
    process.stderr.write(result.stdout ?? "");
    process.stderr.write(result.stderr ?? "");
    throw new Error(`Failed to seed ${seed.key}`);
  }

  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  if (output.includes("variant {") && output.includes("Err")) {
    throw new Error(`Router rejected seed ${seed.key}: ${output}`);
  }

  process.stderr.write(`[seeds] Seeded ${seed.key}\n`);
}
