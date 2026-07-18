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

// Dynamic Candid payload page sizing: each Router ingress call must stay well below the
// 2 MiB inter-canister request limit.  We use the Candid text length as a conservative proxy
// (it is close to the encoded binary size for ASCII-heavy social-demo queries) and target
// 500 KiB so that even waves with longer queries or escaping leave a large safety margin.
const MAX_CANDID_TEXT_BYTES = 500_000;
const MAX_ITEMS_PER_PAGE = Number(process.env.SEED_MAX_ITEMS_PER_PAGE ?? 150);

const requestShellBytes = () => {
  const prefix = '(record { mutations = vec { ';
  const suffix = ' }; start_index = 0; instruction_budget = null })';
  return Buffer.byteLength(prefix, 'utf8') + Buffer.byteLength(suffix, 'utf8');
};

const seedItemTextBytes = (seed) => {
  const text = `record { gql_query = "${escapeCandidText(seed.gql)}"; params = vec {}; mutation_key = "${escapeCandidText(seed.key)}" }`;
  return Buffer.byteLength(text, 'utf8');
};

const payloadPageSize = (waveSeeds, offset, maxTextBytes) => {
  const shell = requestShellBytes();
  let used = shell;
  let count = 0;
  for (let i = offset; i < waveSeeds.length; i++) {
    const itemBytes = seedItemTextBytes(waveSeeds[i]);
    const separator = i === offset ? 0 : 2; // "; " between items
    if (used + separator + itemBytes > maxTextBytes && count > 0) {
      break;
    }
    used += separator + itemBytes;
    count += 1;
  }
  return count;
};

const runBatchPage = (page) => {
  const items = page
    .map(
      (seed) =>
        `record { gql_query = "${escapeCandidText(seed.gql)}"; params = vec {}; mutation_key = "${escapeCandidText(seed.key)}" }`,
    )
    .join("; ");
  let startIndex = 0;
  while (startIndex < page.length) {
    const output = callRouter(
      methodName,
      `(record { mutations = vec { ${items} }; start_index = ${startIndex}; instruction_budget = null })`,
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
};

const runBatchPageRetryable = (page) => {
  let attempt = page;
  while (attempt.length > 0) {
    try {
      runBatchPage(attempt);
      return attempt.length;
    } catch (error) {
      // The inter-canister journal preflight can exceed the Graph canister's liquid cycle
      // budget when the page is too large. Halve the page and retry from the same offset.
      if (
        attempt.length === 1 ||
        !error.message.includes('insufficient liquid cycles balance')
      ) {
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
    const candid = `("${escapeCandidText(seed.gql)}", vec {}, "${escapeCandidText(seed.key)}")`;
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
  for (const [wave, waveSeeds] of sortedWaves) {
    if (waveSeeds.length === 0) continue;
    renderProgress(wave, 0, waveSeeds.length);
    let seededCount = 0;
    let offset = 0;
    while (offset < waveSeeds.length) {
      const dynamicPageSize = payloadPageSize(waveSeeds, offset, MAX_CANDID_TEXT_BYTES);
      const pageSize = Math.min(
        dynamicPageSize,
        explicitPageSize,
        MAX_ITEMS_PER_PAGE,
        waveSeeds.length - offset,
      );
      const page = waveSeeds.slice(offset, offset + pageSize);
      let processed;
      try {
        processed = runBatchPageRetryable(page);
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
  }
}
