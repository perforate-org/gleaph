import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

const seedsPath = process.argv[2]
  ? resolve(process.argv[2])
  : join(root, "seeds/knowledge-map-seeds.json");
const canisterName = process.argv[3] ?? "gleaph-router";
const methodName = process.argv[4] ?? "gql_execute_idempotent_batch";
const pageSizeInput = process.env.SEED_PAGE_SIZE ?? process.argv[5];
const pageSize = pageSizeInput === undefined ? undefined : Number(pageSizeInput);

if (pageSize !== undefined && (!Number.isInteger(pageSize) || pageSize <= 0)) {
  throw new Error("SEED_PAGE_SIZE/page size must be a positive integer when specified");
}

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

const callRouter = (method, candid) => {
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
      method,
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
    throw new Error(`Router call ${method} failed`);
  }

  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  if (output.includes("variant {") && output.includes("Err")) {
    throw new Error(`Router rejected ${method}: ${output}`);
  }

  return output;
};

const nextIndexFrom = (output) => {
  const match = output.match(/next_index\s*=\s*opt\s+(\d+)/);
  return match ? Number(match[1]) : undefined;
};

if (methodName !== "gql_execute_idempotent_batch") {
  for (const seed of seeds) {
    const candid = `(\"${escapeCandidText(seed.gql)}\", vec {}, \"${escapeCandidText(seed.key)}\")`;
    callRouter(methodName, candid);
    process.stderr.write(`[seeds] Seeded ${seed.key}\n`);
  }
} else {
  const effectivePageSize = pageSize ?? seeds.length;
  for (let offset = 0; offset < seeds.length; offset += effectivePageSize) {
    const page = seeds.slice(offset, offset + effectivePageSize);
    const items = page
      .map(
        (seed) =>
          `record { gql_query = \"${escapeCandidText(seed.gql)}\"; params = vec {}; mutation_key = \"${escapeCandidText(seed.key)}\" }`,
      )
      .join("; ");
    let startIndex = 0;
    while (startIndex < page.length) {
      const output = callRouter(
        methodName,
        `(record { mutations = vec { ${items} }; start_index = ${startIndex}; instruction_budget = null; max_items = null })`,
      );
      const nextIndex = nextIndexFrom(output);
      if (nextIndex === undefined) break;
      if (nextIndex <= startIndex || nextIndex > page.length) {
        throw new Error(
          `Router returned invalid next_index ${nextIndex} for page cursor ${startIndex}`,
        );
      }
      startIndex = nextIndex;
    }
    process.stderr.write(
      `[seeds] Seeded page ${offset / effectivePageSize + 1} (${page.length} seeds): ${page[0].key} .. ${page.at(-1).key}\n`,
    );
  }
}
