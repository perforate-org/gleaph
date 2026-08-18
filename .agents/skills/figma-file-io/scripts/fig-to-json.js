/**
 * Extract .fig VARIABLEs into W3C DTCG format JSON
 * Usage: node fig-to-json.js input.fig output.json
 */

import { parseFig } from "openfig-core";
import { readFileSync, writeFileSync } from "fs";
import { join, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));

const FIG_PATH = process.argv[2] || join(__dirname, "design.fig");
const OUT_PATH = process.argv[3] || join(__dirname, "tokens.json");

const data = new Uint8Array(readFileSync(FIG_PATH));
const doc = parseFig(data);

// Build nested DTCG structure from slash-separated VARIABLE names
const tokens = { color: {} };

for (const node of doc.nodes) {
  if (node.type !== "VARIABLE") continue;

  const path = node.name; // e.g. "color/sand/100"
  const parts = path.split("/"); // ["color", "sand", "100"]

  // Build nested object: tokens.color.sand["100"]
  let current = tokens;
  for (let i = 0; i < parts.length - 1; i++) {
    const key = parts[i];
    if (!current[key]) current[key] = {};
    current = current[key];
  }

  const leafKey = parts[parts.length - 1];
  const value = extractValue(node);
  current[leafKey] = {
    $type: value.type,
    $value: value.raw,
  };
}

writeFileSync(OUT_PATH, JSON.stringify(tokens, null, 2));
console.log(`Wrote ${OUT_PATH}`);

function extractValue(node) {
  const rd = node.resolvedData;
  if (!rd) return { type: "unknown", raw: null };

  if (rd.color) {
    const { r, g, b, a = 1 } = rd.color;
    return {
      type: "color",
      raw: {
        colorSpace: "srgb",
        components: [r, g, b],
        hex: rgbToHex(r, g, b),
        alpha: a,
      },
    };
  }

  if (typeof rd === "number") {
    return { type: "number", raw: rd };
  }

  if (typeof rd === "string") {
    return { type: "string", raw: rd };
  }

  return { type: "unknown", raw: rd };
}

function rgbToHex(r, g, b) {
  const toHex = (c) =>
    Math.round(c * 255)
      .toString(16)
      .padStart(2, "0");
  return `#${toHex(r)}${toHex(g)}${toHex(b)}`;
}
