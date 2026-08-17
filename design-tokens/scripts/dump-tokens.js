import { parseFig } from 'openfig-core';
import { readFileSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIG_PATH = join(__dirname, '..', 'gleaph-design-system.fig');

const data = new Uint8Array(readFileSync(FIG_PATH));
const doc = parseFig(data);

// Build GUID -> name map for alias resolution
const guidToName = new Map();
for (const node of doc.message.nodeChanges) {
  if (node.type === 'VARIABLE') {
    guidToName.set(`${node.guid.sessionID}:${node.guid.localID}`, node.name);
  }
}

function colorToHex(c) {
  if (!c) return null;
  const r = Math.round(c.r * 255).toString(16).padStart(2, '0');
  const g = Math.round(c.g * 255).toString(16).padStart(2, '0');
  const b = Math.round(c.b * 255).toString(16).padStart(2, '0');
  return `#${r}${g}${b}`;
}

function describeValue(v) {
  if (!v) return '(none)';
  if (v.colorValue) return colorToHex(v.colorValue);
  if (v.floatValue !== undefined) return String(v.floatValue);
  if (v.alias) {
    const name = guidToName.get(`${v.alias.guid.sessionID}:${v.alias.guid.localID}`);
    return `alias -> ${name ?? '?'}`;
  }
  return JSON.stringify(v);
}

const rows = [];
for (const node of doc.message.nodeChanges) {
  if (node.type !== 'VARIABLE') continue;
  const modes = (node.variableDataValues?.entries ?? []).map((e) => {
    const mode = e.modeID ? `${e.modeID.sessionID}:${e.modeID.localID}` : '?';
    return `[${mode}] ${describeValue(e.variableData?.value)}`;
  });
  rows.push(`${node.name.padEnd(28)} | ${modes.join('  ')}`);
}

rows.sort((a, b) => a.localeCompare(b));
console.log(rows.join('\n'));
