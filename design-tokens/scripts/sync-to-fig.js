/**
 * Sync JSON tokens back into .fig file
 * Reads primitive.json and semantic-{light,dark}.json,
 * updates matching VARIABLE nodes in .fig by GUID map,
 * and re-encodes the .fig file.
 */

import { parseFig, encodeFigParts, assembleCanvasFig, createFigZip } from 'openfig-core';
import { run as zstdRun } from 'zstd-codec';
import { readFileSync, writeFileSync } from 'fs';

const FIG_PATH = '/Users/yota/dev/gleaph/design-tokens/gleaph-design-system.fig';

// Flatten nested JSON token structure into dot-paths
function flattenTokens(obj, prefix = '') {
  const result = {};
  for (const [key, value] of Object.entries(obj)) {
    if (key.startsWith('$')) continue;
    const newKey = prefix ? `${prefix}.${key}` : key;
    if (value && typeof value === 'object') {
      if ('$value' in value) {
        result[newKey] = value['$value'];
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

// Parse .fig
const data = new Uint8Array(readFileSync(FIG_PATH));
const doc = parseFig(data);

// Build name -> node map
const varMap = new Map();
for (const node of doc.message.nodeChanges) {
  if (node.type === 'VARIABLE') {
    varMap.set(node.name, node);
  }
}

// Load JSON tokens
const primitiveData = JSON.parse(readFileSync('/Users/yota/dev/gleaph/design-tokens/primitive.json', 'utf8'));
const lightData = JSON.parse(readFileSync('/Users/yota/dev/gleaph/design-tokens/semantic-light.json', 'utf8'));
const darkData = JSON.parse(readFileSync('/Users/yota/dev/gleaph/design-tokens/semantic-dark.json', 'utf8'));

const primitives = flattenTokens(primitiveData);
const lightSemantic = flattenTokens(lightData);
const darkSemantic = flattenTokens(darkData);

// Resolve aliases in semantic tokens
function resolveSemantic(semantic, primitives) {
  const resolved = {};
  for (const [key, value] of Object.entries(semantic)) {
    if (typeof value === 'string' && value.startsWith('{') && value.endsWith('}')) {
      const path = value.slice(1, -1);
      resolved[key] = primitives[path];
    } else {
      resolved[key] = value;
    }
  }
  return resolved;
}

const lightResolved = resolveSemantic(lightSemantic, primitives);
const darkResolved = resolveSemantic(darkSemantic, primitives);

// Update primitive tokens in .fig
let updated = 0;
for (const [jsonPath, value] of Object.entries(primitives)) {
  // Convert json dot-path to fig slash-path
  const figName = jsonPath.replace(/\./g, '/');
  const node = varMap.get(figName);
  if (!node) {
    console.warn(`Skipping unknown primitive token: ${figName}`);
    continue;
  }

  if (figName.startsWith('color/')) {
    const hex = typeof value === 'string' ? value : (value.hex || '');
    if (!hex) {
      console.warn(`No hex for ${figName}`);
      continue;
    }
    const c = hexToSrgb(hex);
    // Update the color value
    node.variableDataValues.entries[0].variableData.value.colorValue = c;
    updated++;
  } else if (figName.startsWith('space/') || figName.startsWith('radius/')) {
    const num = typeof value === 'object' ? value.value : value;
    node.variableDataValues.entries[0].variableData.value.floatValue = num;
    updated++;
  }
}

// Update semantic tokens in .fig
// Semantic tokens have two mode entries: light (6:0) and dark (6:1)
const semanticLightModes = {};
const semanticDarkModes = {};
for (const [jsonPath, value] of Object.entries(lightResolved)) {
  semanticLightModes[jsonPath] = value;
}
for (const [jsonPath, value] of Object.entries(darkResolved)) {
  semanticDarkModes[jsonPath] = value;
}

// Semantic name mapping: json path -> fig name
// json: color.background.canvas -> fig: color/bg/canvas
for (const [jsonPath, lightValue] of Object.entries(lightResolved)) {
  const figName = jsonPath.replace(/\./g, '/');
  const darkValue = darkResolved[jsonPath];
  const node = varMap.get(figName);
  if (!node) {
    console.warn(`Skipping unknown semantic token: ${figName}`);
    continue;
  }

  // Find target primitive GUIDs for light and dark
  function findPrimitiveGuid(targetValue) {
    if (typeof targetValue === 'string' && targetValue.startsWith('{') && targetValue.endsWith('}')) {
      const path = targetValue.slice(1, -1).replace(/\./g, '/');
      const targetNode = varMap.get(path);
      if (targetNode) return targetNode.guid;
    }
    // If it's a direct hex, find matching primitive by value
    const targetHex = typeof targetValue === 'string' ? targetValue : (targetValue.hex || '');
    if (targetHex) {
      for (const [n, v] of varMap.entries()) {
        if (!n.startsWith('color/')) continue;
        const entry = v.variableDataValues.entries[0]?.variableData?.value;
        if (entry?.colorValue) {
          const c = entry.colorValue;
          const hex = `#${Math.round(c.r*255).toString(16).padStart(2,'0')}${Math.round(c.g*255).toString(16).padStart(2,'0')}${Math.round(c.b*255).toString(16).padStart(2,'0')}`;
          if (hex.toLowerCase() === targetHex.toLowerCase()) return v.guid;
        }
      }
    }
    return null;
  }

  const lightGuid = findPrimitiveGuid(lightValue);
  const darkGuid = findPrimitiveGuid(darkValue);

  if (!lightGuid || !darkGuid) {
    console.warn(`Could not resolve alias for ${figName}`);
    continue;
  }

  node.variableDataValues.entries = [
    {
      modeID: { sessionID: 6, localID: 0 },
      variableData: {
        value: { alias: { guid: lightGuid } },
        dataType: "ALIAS",
        resolvedDataType: "COLOR"
      }
    },
    {
      modeID: { sessionID: 6, localID: 1 },
      variableData: {
        value: { alias: { guid: darkGuid } },
        dataType: "ALIAS",
        resolvedDataType: "COLOR"
      }
    }
  ];
  updated++;
}

console.log(`Updated ${updated} VARIABLE nodes`);

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
    passThrough: parts.passThrough,
  });
  const figZip = createFigZip({
    canvasFig,
    meta: doc.meta,
    thumbnail: doc.thumbnail,
    images: doc.images,
  });
  writeFileSync(FIG_PATH, figZip);
  console.log('Saved updated .fig');
});
