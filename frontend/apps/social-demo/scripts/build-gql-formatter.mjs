import { existsSync, rmSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const appDir = path.resolve(scriptDir, "..");
const crateDir = path.join(appDir, "wasm");
const outputDir = path.join(appDir, "src", "generated", "gql_formatter");

if (existsSync(outputDir)) {
  rmSync(outputDir, { recursive: true, force: true });
}

const result = spawnSync(
  "wasm-pack",
  [
    "build",
    crateDir,
    "--target",
    "web",
    "--release",
    "--mode",
    "no-install",
    "--out-dir",
    outputDir,
    "--out-name",
    "gql_formatter",
  ],
  { cwd: appDir, stdio: "inherit" },
);

if (result.error) {
  throw result.error;
}
if (result.status !== 0) {
  process.exit(result.status ?? 1);
}
