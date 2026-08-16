import { parseFig } from 'openfig-core';
import { readFileSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIG_PATH = join(__dirname, '..', 'gleaph-design-system.fig');

// Parse .fig
const data = new Uint8Array(readFileSync(FIG_PATH));
const doc = parseFig(data);

// Build GUID map: name -> guid
const guidMap = new Map();
for (const node of doc.message.nodeChanges) {
  if (node.type === 'VARIABLE') {
    guidMap.set(node.name, { sessionID: node.guid.sessionID, localID: node.guid.localID });
  }
}

// Print all VARIABLEs with their GUIDs
console.log('=== VARIABLE GUID Map ===');
for (const [name, guid] of [...guidMap.entries()].sort((a,b) => a[0].localeCompare(b[0]))) {
  console.log(`${name.padEnd(30)} | ${guid.sessionID}:${guid.localID}`);
}
