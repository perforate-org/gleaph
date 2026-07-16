#!/usr/bin/env node
import { readdirSync, readFileSync } from "node:fs";
import fs from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import YAML from "yaml";
import { createRouterActor } from "./actor.mjs";

const SCRIPT_DIR = fileURLToPath(new URL(".", import.meta.url));
const ROOT = process.cwd();
const SCENARIOS_DIR = process.argv[2]
  ? resolve(process.argv[2])
  : resolve(SCRIPT_DIR, "..", "config", "scenarios");
const ROUTER_CANISTER = process.env.GLEAPH_DEMO_ROUTER_CANISTER || "gleaph-router";

function semanticVectorFromGatewayLib() {
  // The Gateway now reads its semantic vectors from the same generated scenario JSON
  // that drives the frontend. Drift detection therefore compares the scenario YAML
  // vectors against the committed generated JSON rather than parsing inline Rust arrays.
  const jsonPath = resolve(SCRIPT_DIR, "..", "src", "data", "scenarios.generated.json");
  if (!fs.existsSync(jsonPath)) {
    console.error(`[social-demo] WARN: ${jsonPath} not found; cannot check semantic vector drift`);
    return null;
  }
  try {
    const manifest = JSON.parse(readFileSync(jsonPath, "utf8"));
    const vectors = {};
    for (const scenario of manifest.scenarios || []) {
      if (scenario.semanticVector != null) {
        vectors[scenario.id] = scenario.semanticVector.map((v) => Number(v));
      }
    }
    return vectors;
  } catch (err) {
    console.error(`[social-demo] WARN: could not parse scenarios.generated.json: ${err.message}`);
    return null;
  }
}

function checkSemanticVectorDrift(scenarios) {
  const canonicalById = semanticVectorFromGatewayLib();
  if (!canonicalById) return;
  for (const scenario of scenarios) {
    const vec = scenario.semanticVector;
    if (!Array.isArray(vec)) continue;
    const canonical = canonicalById[scenario.id];
    if (!canonical) continue;
    const drift =
      vec.length !== canonical.length ||
      vec.some((v, i) => Math.abs(v - canonical[i]) > 1e-6);
    if (drift) {
      console.log(
        `[social-demo] WARN: semanticVector in ${scenario.id} drifted from scenarios.generated.json`
      );
    }
  }
}

async function main() {
  const files = readdirSync(SCENARIOS_DIR)
    .filter((name) => name.endsWith(".yaml"))
    .sort();
  const scenarios = [];
  for (const file of files) {
    const doc = YAML.parse(readFileSync(join(SCENARIOS_DIR, file), "utf8"));
    scenarios.push({
      id: doc.id,
      preparedQueryId: doc.preparedQueryId,
      query: doc.preparedQuery,
      semanticVector: doc.semanticVector,
    });
  }
  console.log(`[social-demo] Registering ${scenarios.length} prepared queries (batch)`);

  const records = scenarios.map((s) => [s.preparedQueryId, s.query]);
  const router = createRouterActor(ROUTER_CANISTER);
  const parsed = await router.prepared_register_batch(records);

  if (parsed.length !== scenarios.length) {
    throw new Error(
      `Batch result length mismatch: expected ${scenarios.length}, got ${parsed.length}`
    );
  }

  let registeredCount = 0;
  let duplicateCount = 0;
  for (let i = 0; i < scenarios.length; i += 1) {
    const scenario = scenarios[i];
    const result = parsed[i];
    if ("Err" in result) {
      const errText = JSON.stringify(result.Err);
      if (errText.includes("Conflict")) {
        duplicateCount += 1;
        continue;
      }
      throw new Error(
        `prepared_register_batch failed for ${scenario.id}: ${errText}`
      );
    }
    registeredCount += 1;
  }
  console.log(
    `[social-demo] Prepared query batch complete: ${registeredCount} registered, ${duplicateCount} duplicates skipped`
  );
  checkSemanticVectorDrift(scenarios);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
