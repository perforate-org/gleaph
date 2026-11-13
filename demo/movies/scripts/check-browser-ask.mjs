#!/usr/bin/env node
/** Smoke the public JS SDK ask path used by the browser demo. */
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createGleaphClient } from "../../../sdk/client/js/dist/index.mjs";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const config = JSON.parse(await readFile(join(ROOT, "browser/config.json"), "utf8"));
if (!config.canisterId) throw new Error("browser/config.json has no canisterId");
const client = await createGleaphClient({
  canisterId: config.canisterId,
  host: config.host,
  fetchRootKey: config.fetchRootKey,
});
const question = "千と千尋の神隠しに似ている映画は？";
const answer = await client.ask("movies", question);
const rows = answer.rows?.rows?.length ?? 0;
if (
  answer.status !== "Answered" ||
  answer.interpretation.operation !== "films-similar-to-ja" ||
  rows === 0
) {
  throw new Error(`unexpected ask answer: ${JSON.stringify(answer)}`);
}
console.log(`browser ask smoke: ${answer.status} ${answer.interpretation.operation} ${rows} rows`);
