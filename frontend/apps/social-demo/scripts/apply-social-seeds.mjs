import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { IDL } from "@icp-sdk/core/candid";
import { encodeGqlParamsBlob, candidVecBytes } from "./encode-gql-params.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

const seedsPath = process.argv[2]
  ? resolve(process.argv[2])
  : join(root, "seeds/social-seeds.json");
const canisterName = process.argv[3] ?? "gleaph-router";
const methodName = process.argv[4] ?? "gql_execute_idempotent_batch";
const pageSizeInput = process.env.SEED_PAGE_SIZE ?? process.argv[5];
const pageSize = pageSizeInput === undefined ? undefined : Number(pageSizeInput);

let adaptiveInterCanisterPrefix;
if (methodName === "gql_execute_idempotent_batch") {
  const formatter = await import("../src/generated/gql_formatter/gql_formatter.js");
  formatter.initSync({
    module: readFileSync(
      join(root, "src/generated/gql_formatter/gql_formatter_bg.wasm"),
    ),
  });
  adaptiveInterCanisterPrefix = formatter.adaptive_inter_canister_prefix;
}

if (pageSize !== undefined && (!Number.isInteger(pageSize) || pageSize <= 0)) {
  throw new Error("SEED_PAGE_SIZE/page size must be a positive integer when specified");
}

const seeds = JSON.parse(readFileSync(seedsPath, "utf8")).seeds;

// Map a seed GQL statement to a dependency wave.  Seeds in the same wave are
// independent and can be dispatched inside one gql_execute_idempotent_batch
// call.  Waves are executed in numeric order so parent/dependent entities
// (vertices before referencing edges) exist before a later wave needs them.
function seedWave(gql) {
  if (gql.includes('INSERT (n:User')) return 1;
  if (gql.includes('INSERT (n:Community')) return 1;
  if (gql.includes('INSERT (n:Topic')) return 1;
  if (gql.includes('INSERT (n:Feed')) return 1;
  if (gql.includes('-[:FOLLOWS')) return 2;
  if (gql.includes('-[:MEMBER_OF')) return 2;
  if (gql.includes('-[:POSTED')) return 3;
  if (gql.includes('-[:REPLY_TO')) return 4;
  if (gql.includes('-[:IN_TOPIC')) return 5;
  if (gql.includes('-[:IN_PUBLIC_FEED')) return 6;
  if (gql.includes('-[:IN_HOME')) return 6;
  // Fallback: assume unrecognised statements depend on everything and place
  // them after all structured waves.
  return 7;
}

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
  // The binary Candid payload can fit under the IC limit while its textual Candid form exceeds
  // the replica HTTP body limit. Use an args-file for both formats, and preserve the binary form
  // for batch calls so the request body matches the size measured by the adaptive sizer.
  const argsFormat = candid instanceof Uint8Array ? "bin" : "candid";
  const tempDir = mkdtempSync(join(tmpdir(), "gleaph-social-call-"));
  const argsPath = join(tempDir, "args.did");
  writeFileSync(argsPath, candid);
  let result;
  try {
    result = spawnSync(
      "icp",
      [
        "canister",
        "call",
        "-e",
        "local",
        ...(process.env.ICP_IDENTITY_NAME
          ? ["--identity", process.env.ICP_IDENTITY_NAME]
          : []),
        "--args-format",
        argsFormat,
        "--args-file",
        argsPath,
        canisterName,
        method,
      ],
      {
        env: icpEnv(),
        encoding: "utf8",
      },
    );
  } finally {
    rmSync(tempDir, { recursive: true, force: true });
  }

  if (result.status !== 0) {
    const failureOutput = `${result.stdout ?? ""}${result.stderr ?? ""}`;
    process.stderr.write(failureOutput);
    if (result.error) process.stderr.write(`${result.error.message}\n`);
    const detail = result.signal ? ` (signal ${result.signal})` : "";
    throw new Error(
      `Router call ${method} failed${detail}: ${failureOutput.trim() || result.error?.message || "unknown error"}`,
    );
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

const PROGRESS_BAR_WIDTH = 28;

const renderProgress = (wave, completed, total) => {
  const ratio = total === 0 ? 1 : completed / total;
  const percentage = Math.min(100, Math.floor(ratio * 100));
  const filled = Math.round(ratio * PROGRESS_BAR_WIDTH);
  const bar = `${"#".repeat(filled)}${"-".repeat(PROGRESS_BAR_WIDTH - filled)}`;
  process.stderr.write(
    `\r[seeds] wave ${wave} [${bar}] ${completed}/${total} (${percentage}%)`,
  );
};

const finishProgress = () => process.stderr.write("\n");

const backfillVertexPropertyPostings = async () => {
  const graphName = process.env.GLEAPH_DEMO_GRAPH_NAME ?? "gleaph.pocket_ic";
  while (true) {
    const output = callRouter(
      "admin_vertex_property_backfill_step",
      `(record { logical_graph_name = "${escapeCandidText(graphName)}"; shard_id = 0 : nat32; max_vertices = 1000 : nat32 })`,
    );
    if (output.includes("done = true")) return;
  }
};

// Keep the probe shape identical to the Router ingress argument and measure the binary Candid
// envelope, including fixed fields and per-item vectors.
const batchItemType = IDL.Record({
  gql_query: IDL.Text,
  mutation_key: IDL.Text,
  params: IDL.Vec(IDL.Nat8),
});
const batchArgsType = IDL.Record({
  instruction_budget: IDL.Opt(IDL.Nat64),
  mutations: IDL.Vec(batchItemType),
  start_index: IDL.Nat32,
});

const encodeBatchArgs = (seeds, startIndex = 0) =>
  IDL.encode(
    [batchArgsType],
    [
      {
        instruction_budget: [],
        mutations: seeds.map((seed) => ({
          gql_query: seed.gql,
          mutation_key: seed.key,
          params: encodeGqlParamsBlob(seed.params),
        })),
        start_index: startIndex,
      },
    ],
  );

const encodeBatchBytes = (seeds) => encodeBatchArgs(seeds).byteLength;

const adaptivePayloadPageSize = (waveSeeds, offset, hint) => {
  const remaining = waveSeeds.length - offset;
  const measure = (count) => encodeBatchBytes(waveSeeds.slice(offset, offset + count));
  return adaptiveInterCanisterPrefix(remaining, hint, measure);
};

const runBatchPage = (page, onProgress) => {
  let startIndex = 0;
  while (startIndex < page.length) {
    const output = callRouter(
      methodName,
      encodeBatchArgs(page, startIndex),
    );
    const nextIndex = nextIndexFrom(output);
    if (nextIndex === undefined) {
      // Router applied all items in this page in a single call.
      startIndex = page.length;
    } else if (nextIndex <= startIndex || nextIndex > page.length) {
      throw new Error(
        `Router returned invalid next_index ${nextIndex} for page cursor ${startIndex}`,
      );
    } else {
      startIndex = nextIndex;
    }
    if (onProgress) onProgress(startIndex);
  }
  return startIndex;
};

const RETRYABLE_PAGE_ERRORS = [
  'insufficient liquid cycles balance',
  'instruction budget was exhausted before the next mutation could start',
  'call perform failed',
  'payload_too_large',
  'payload is too large',
  'status 413',
];

const isRetryablePageError = (error) =>
  RETRYABLE_PAGE_ERRORS.some((marker) => error.message.toLowerCase().includes(marker));

const runBatchPageRetryable = (page, onProgress) => {
  let attempt = page;
  while (attempt.length > 0) {
    try {
      return runBatchPage(attempt, onProgress);
    } catch (error) {
      // Router-side instruction/cycle budgets or the transport body limit can be exceeded when
      // the page is too large for a single ingress call. Halve the page and retry from the same
      // offset so the dynamic paging keeps making forward progress.
      if (attempt.length === 1 || !isRetryablePageError(error)) {
        throw error;
      }
      const nextSize = Math.max(1, Math.floor(attempt.length / 2));
      attempt = attempt.slice(0, nextSize);
    }
  }
  return 0;
};

if (methodName !== "gql_execute_idempotent_batch") {
  for (const seed of seeds) {
    const params = encodeGqlParamsBlob(seed.params);
    const candid = `("${escapeCandidText(seed.gql)}", ${candidVecBytes(params)}, "${escapeCandidText(seed.key)}")`;
    callRouter(methodName, candid);
    process.stderr.write(`[seeds] Seeded ${seed.key}\n`);
  }
} else {
  const explicitPageSize = pageSize ?? Number.POSITIVE_INFINITY;
  // Group seeds into dependency waves so each wave can safely run inside one
  // gql_execute_idempotent_batch call.  The caller is responsible for seed order.
  const waves = new Map();
  for (const seed of seeds) {
    const wave = seedWave(seed.gql);
    if (!waves.has(wave)) waves.set(wave, []);
    waves.get(wave).push(seed);
  }
  const sortedWaves = Array.from(waves.entries()).sort((a, b) => a[0] - b[0]);
  for (const [wave, rawWaveSeeds] of sortedWaves) {
    // Router coalesces only consecutive identical plans. Keep dependency waves intact, but make
    // each plan contiguous so all rows for a multi-anchor relation share one typed V1 bulk group.
    // The first-seen plan order is preserved; seeds within a plan retain their source order.
    const planGroups = new Map();
    for (const seed of rawWaveSeeds) {
      if (!planGroups.has(seed.gql)) planGroups.set(seed.gql, []);
      planGroups.get(seed.gql).push(seed);
    }
    const waveSeeds = Array.from(planGroups.values()).flat();
    if (waveSeeds.length === 0) continue;
    renderProgress(wave, 0, waveSeeds.length);
    let seededCount = 0;
    let offset = 0;
    const pageSizeHints = new Map();
    while (offset < waveSeeds.length) {
      const hintKey = waveSeeds[offset]?.gql;
      const dynamicPageSize = adaptivePayloadPageSize(
        waveSeeds,
        offset,
        pageSizeHints.get(hintKey),
      );
      pageSizeHints.set(hintKey, dynamicPageSize.count);
      const pageSize = Math.min(
        dynamicPageSize.count,
        explicitPageSize,
        waveSeeds.length - offset,
      );
      const page = waveSeeds.slice(offset, offset + pageSize);
      const updateProgress = (pageCompleted) => {
        renderProgress(wave, seededCount + pageCompleted, waveSeeds.length);
      };
      let processed;
      try {
        processed = runBatchPageRetryable(page, updateProgress);
      } catch (error) {
        finishProgress();
        throw error;
      }
      seededCount += processed;
      offset += processed;
      renderProgress(wave, seededCount, waveSeeds.length);
    }
    finishProgress();
    process.stderr.write(
      `[seeds] Seeded wave ${wave} (${seededCount} seeds): ${waveSeeds[0].key} .. ${waveSeeds.at(-1).key}\n`,
    );
    if (wave === 3) {
      process.stderr.write("[seeds] Backfilling vertex property postings before dependent edge waves\n");
      await backfillVertexPropertyPostings();
    }
  }
}
