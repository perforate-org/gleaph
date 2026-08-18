/**
 * List all VARIABLE nodes from a .fig file
 * Outputs JSON with name -> { sessionID, localID, value }
 */

import { parseFig, nodeId } from 'openfig-core';
import { readFileSync, writeFileSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));

const FIG_PATH = process.argv[2] || join(__dirname, 'design.fig');
const OUT_PATH = process.argv[3] || join(__dirname, 'fig-variables.json');

const data = new Uint8Array(readFileSync(FIG_PATH));
const doc = parseFig(data);

const variables = {};
for (const node of doc.nodes) {
  if (node.type === 'VARIABLE') {
    const id = nodeId(node);
    const value = extractValue(node);
    variables[node.name] = {
      id,
      sessionID: node.guid.sessionID,
      localID: node.guid.localID,
      value
    };
  }
}

writeFileSync(OUT_PATH, JSON.stringify(variables, null, 2));
console.log(`Wrote ${Object.keys(variables).length} variables to ${OUT_PATH}`);

function extractValue(node) {
  const rd = node.resolvedData;
  if (!rd) return null;
  if (rd.color) {
    return {
      type: 'color',
      r: rd.color.r,
      g: rd.color.g,
      b: rd.color.b,
      a: rd.color.a ?? 1
    };
  }
  if (typeof rd === 'number') {
    return { type: 'number', value: rd };
  }
  if (typeof rd === 'string') {
    return { type: 'string', value: rd };
  }
  return { type: 'unknown', raw: rd };
}
