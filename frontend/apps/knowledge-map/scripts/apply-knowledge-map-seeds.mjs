import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const seeds = JSON.parse(
  readFileSync(join(root, "seeds/knowledge-map-seeds.json"), "utf8"),
).seeds;

const icpEnv = () => ({
  ...process.env,
  HOME: process.env.ICP_CLI_HOME ?? process.env.HOME ?? "",
  COREPACK_HOME: process.env.ICP_COREPACK_HOME ?? "",
  XDG_CACHE_HOME: process.env.ICP_XDG_CACHE_HOME ?? "",
  XDG_DATA_HOME: process.env.ICP_XDG_DATA_HOME ?? "",
  DO_NOT_TRACK: process.env.DO_NOT_TRACK ?? "1",
});

for (const seed of seeds) {
  const candid = `(\"${seed.gql}\", vec {}, \"${seed.key}\")`;
  const result = spawnSync(
    "icp",
    ["canister", "call", "-e", "local", "gleaph-router", "gql_execute_idempotent", candid],
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

  process.stderr.write(`[knowledge-map] Seeded ${seed.key}\n`);
}
