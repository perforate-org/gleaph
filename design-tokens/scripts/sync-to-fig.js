/**
 * Sync JSON tokens back into .fig file
 *
 * The JSON files (primitive.json + semantic-{light,dark}.json) are the source of
 * truth for the design tokens. This script regenerates the VARIABLE nodes inside
 * the `.fig` file to match the JSON:
 *
 *   - Renames primitive nodes to the JSON names (gray -> sand, teal -> blue)
 *   - Revalues every primitive from JSON
 *   - Creates missing primitives (clay)
 *   - Renames semantic nodes (bg -> background) and re-points their aliases
 *   - Updates the Figma WEB code-syntax strings
 *
 * It then re-encodes the `.fig` binary.
 */

import {
  parseFig,
  encodeFigParts,
  assembleCanvasFig,
  createFigZip,
} from "openfig-core";
import pkg from "zstd-codec";
const zstdRun = pkg.ZstdCodec.run;
import { readFileSync, writeFileSync } from "fs";
import { join, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const baseDir = join(__dirname, "..");
const FIG_PATH = join(baseDir, "gleaph-design-system.fig");

// Flatten nested JSON token structure into dot-paths
function flattenTokens(obj, prefix = "") {
  const result = {};
  for (const [key, value] of Object.entries(obj)) {
    if (key.startsWith("$")) continue;
    const newKey = prefix ? `${prefix}.${key}` : key;
    if (value && typeof value === "object") {
      if ("$value" in value) {
        result[newKey] = value["$value"];
      } else {
        Object.assign(result, flattenTokens(value, newKey));
      }
    }
  }
  return result;
}

// Convert hex to normalized sRGB components for .fig
function hexToSrgb(hex) {
  const r = parseInt(hex.slice(1, 3), 16) / 255;
  const g = parseInt(hex.slice(3, 5), 16) / 255;
  const b = parseInt(hex.slice(5, 7), 16) / 255;
  return { r, g, b, a: 1 };
}

// Update the Figma WEB code-syntax string to match a token name
function updateCodeSyntax(node, name) {
  if (!node.codeSyntax?.entries) return;
  for (const e of node.codeSyntax.entries) {
    e.value = `var(--${name.replace(/\//g, "-")})`;
  }
}

const isPrimitiveSet = (node) => node.variableSetID?.guid?.sessionID === 4;
const isSemanticSet = (node) => node.variableSetID?.guid?.sessionID === 6;

// Parse .fig
const data = new Uint8Array(readFileSync(FIG_PATH));
const doc = parseFig(data);

const varNodes = doc.message.nodeChanges.filter((n) => n.type === "VARIABLE");
let byName = new Map(varNodes.map((n) => [n.name, n]));

// Load JSON tokens
const primitiveData = JSON.parse(
  readFileSync(join(baseDir, "primitive.json"), "utf8"),
);
const lightData = JSON.parse(
  readFileSync(join(baseDir, "semantic-light.json"), "utf8"),
);
const darkData = JSON.parse(
  readFileSync(join(baseDir, "semantic-dark.json"), "utf8"),
);

const primitives = flattenTokens(primitiveData); // key: dot-path, value: $value
const lightSemantic = flattenTokens(lightData);
const darkSemantic = flattenTokens(darkData);

// Target primitive names + values (slash paths)
const primitiveTargets = {};
for (const [key, value] of Object.entries(primitives)) {
  primitiveTargets[key.replace(/\./g, "/")] = value;
}

// Resolve semantic alias targets: name -> { light, dark } primitive slash-paths
function aliasTarget(value) {
  if (
    typeof value === "string" &&
    value.startsWith("{") &&
    value.endsWith("}")
  ) {
    return value.slice(1, -1).replace(/\./g, "/");
  }
  throw new Error(`Unsupported semantic value: ${JSON.stringify(value)}`);
}
const semanticTargets = {};
for (const key of Object.keys(lightSemantic)) {
  semanticTargets[key.replace(/\./g, "/")] = {
    light: aliasTarget(lightSemantic[key]),
    dark: aliasTarget(darkSemantic[key]),
  };
}

// Allocate fresh GUIDs for newly created primitive nodes
let nextLocal = 30;
function allocGuid() {
  const g = { sessionID: 4, localID: nextLocal };
  nextLocal++;
  return g;
}

let renamed = 0;
let revalued = 0;

// --- 1. Rename + revalue primitives (gray -> sand, teal -> blue, space/radius) ---
for (const node of varNodes) {
  if (!isPrimitiveSet(node)) continue;
  const oldName = node.name;
  let newName = oldName;
  if (oldName.startsWith("color/gray/"))
    newName = `color/sand/${oldName.slice("color/gray/".length)}`;
  else if (oldName.startsWith("color/teal/"))
    newName = `color/blue/${oldName.slice("color/teal/".length)}`;

  if (newName !== oldName) {
    node.name = newName;
    renamed++;
  }

  const target = primitiveTargets[newName];
  if (!target) {
    console.warn(`No JSON value for primitive: ${newName}`);
    continue;
  }

  const entry = node.variableDataValues.entries[0];
  if (newName.startsWith("color/")) {
    const hex = typeof target === "string" ? target : target.hex;
    entry.variableData.value.colorValue = hexToSrgb(hex);
  } else {
    const num = typeof target === "object" ? target.value : target;
    entry.variableData.value.floatValue = num;
  }
  revalued++;
  updateCodeSyntax(node, newName);
}

// Rebuild name map after primitive renames so semantic aliases resolve correctly
byName = new Map(
  doc.message.nodeChanges
    .filter((n) => n.type === "VARIABLE")
    .map((n) => [n.name, n]),
);

// --- 2. Create missing primitives (clay) ---
let created = 0;
for (const name of ["color/clay/300", "color/clay/400", "color/clay/500"]) {
  if (byName.has(name)) continue;
  const template = varNodes.find(
    (n) => isPrimitiveSet(n) && n.name.startsWith("color/"),
  );
  if (!template) throw new Error("No primitive template available to clone");
  const clone = JSON.parse(JSON.stringify(template));
  const g = allocGuid();
  clone.guid = g;
  clone.name = name;
  clone.version = `${g.sessionID}:${g.localID}`;
  clone.userFacingVersion = `${g.sessionID}:${g.localID}`;
  const target = primitiveTargets[name];
  const entry = clone.variableDataValues.entries[0];
  entry.variableData.value.colorValue = hexToSrgb(
    typeof target === "string" ? target : target.hex,
  );
  updateCodeSyntax(clone, name);
  doc.message.nodeChanges.push(clone);
  byName.set(name, clone);
  created++;
}

// --- 3. Rename semantic nodes + re-point aliases (bg -> background) ---
for (const node of varNodes) {
  if (!isSemanticSet(node)) continue;
  const oldName = node.name;
  const newName = oldName.replace("color/bg/", "color/background/");
  if (newName !== oldName) {
    node.name = newName;
    renamed++;
  }
  updateCodeSyntax(node, newName);

  const target = semanticTargets[newName];
  if (!target) {
    console.warn(`No JSON semantic target for: ${newName}`);
    continue;
  }
  for (const e of node.variableDataValues.entries) {
    const mode = `${e.modeID.sessionID}:${e.modeID.localID}`;
    const targetName =
      mode === "6:0" ? target.light : mode === "6:1" ? target.dark : null;
    if (!targetName) continue;
    const targetNode = byName.get(targetName);
    if (!targetNode) {
      console.warn(`Primitive not found for alias: ${targetName}`);
      continue;
    }
    e.variableData.value = { alias: { guid: targetNode.guid } };
  }
}

console.log(
  `Primitives: ${renamed} renamed, ${revalued} revalued, ${created} created`,
);

// --- Re-encode ---
const parts = encodeFigParts(doc);

zstdRun((zstd) => {
  const simple = new zstd.Simple();
  const messageCompressed = simple.compress(parts.messageRaw, 3);
  const canvasFig = assembleCanvasFig({
    prelude: parts.prelude,
    version: parts.version,
    schemaCompressed: parts.schemaCompressed,
    messageCompressed,
    passThrough: parts.passThrough,
  });
  const figZip = createFigZip({
    canvasFig,
    meta: doc.meta,
    thumbnail: doc.thumbnail,
    images: doc.images,
  });
  writeFileSync(FIG_PATH, figZip);
  console.log(`Saved regenerated .fig -> ${FIG_PATH}`);
});
