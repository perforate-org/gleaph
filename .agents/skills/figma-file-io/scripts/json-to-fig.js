/**
 * Sync DTCG JSON tokens into a .fig file's VARIABLE nodes
 * Usage: node json-to-fig.js tokens.json design.fig
 */

import { parseFig, encodeFigParts, assembleCanvasFig, createFigZip } from 'openfig-core';
import { run as zstdRun } from 'zstd-codec';
import { readFileSync, writeFileSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));

const JSON_PATH = process.argv[2] || join(__dirname, 'tokens.json');
const FIG_PATH = process.argv[3] || join(__dirname, 'design.fig');

// Load JSON tokens
const tokenData = JSON.parse(readFileSync(JSON_PATH, 'utf8'));
const flatTokens = flattenTokens(tokenData);

// Parse .fig
const figData = new Uint8Array(readFileSync(FIG_PATH));
const doc = parseFig(figData);

// Build lookup: slash-path -> VARIABLE node
const varMap = new Map();
for (const node of doc.message.nodeChanges) {
  if (node.type === 'VARIABLE') {
    varMap.set(node.name, node);
  }
}

// Update matching VARIABLEs
let updated = 0;
let skipped = 0;
for (const [jsonPath, token] of Object.entries(flatTokens)) {
  const figPath = jsonPath.replace(/\./g, '/');
  const node = varMap.get(figPath);

  if (!node) {
    console.warn(`Skip: no VARIABLE named "${figPath}" in .fig`);
    skipped++;
    continue;
  }

  if (token.$type === 'color' && token.$value) {
    const v = token.$value;
    const hex = v.hex || v;
    const rgb = hexToRgb(hex);
    node.resolvedData = {
      ...node.resolvedData,
      color: { r: rgb.r, g: rgb.g, b: rgb.b, a: v.alpha ?? 1 }
    };
    updated++;
  } else if (token.$type === 'number' && token.$value) {
    node.resolvedData = token.$value;
    updated++;
  } else {
    console.warn(`Skip: unsupported type "${token.$type}" for "${figPath}"`);
    skipped++;
  }
}

console.log(`Updated ${updated} VARIABLEs, skipped ${skipped}`);

// Re-encode
const parts = encodeFigParts(doc);

zstdRun(zstd => {
  const simple = new zstd.Simple();
  const messageCompressed = simple.compress(parts.messageRaw, 3);

  const canvasFig = assembleCanvasFig({
    prelude: parts.prelude,
    version: parts.version,
    schemaCompressed: parts.schemaCompressed,
    messageCompressed,
    passThrough: parts.passThrough
  });

  const figZip = createFigZip({
    canvasFig,
    meta: doc.meta,
    thumbnail: doc.thumbnail,
    images: doc.images
  });

  writeFileSync(FIG_PATH, figZip);
  console.log(`Saved ${FIG_PATH}`);
});

// --- helpers ---

function flattenTokens(obj, prefix = '') {
  const result = {};
  for (const [key, value] of Object.entries(obj)) {
    if (key.startsWith('$')) continue;
    const newKey = prefix ? `${prefix}.${key}` : key;
    if (value && typeof value === 'object') {
      if ('$value' in value) {
        result[newKey] = value;
      } else {
        Object.assign(result, flattenTokens(value, newKey));
      }
    }
  }
  return result;
}

function hexToRgb(hex) {
  const clean = hex.replace('#', '');
  return {
    r: parseInt(clean.slice(0, 2), 16) / 255,
    g: parseInt(clean.slice(2, 4), 16) / 255,
    b: parseInt(clean.slice(4, 6), 16) / 255
  };
}
