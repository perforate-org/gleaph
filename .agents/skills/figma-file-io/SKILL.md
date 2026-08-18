---
name: figma-file-io
description: Read and write Figma `.fig` binary files directly using `openfig-core` and `zstd-codec`.
version: 1.0.0
dependencies:
  - openfig-core
  - zstd-codec
tags:
  - figma
  - design-tokens
  - fig
  - binary-format
  - variable
  - color
---

# Figma File I/O

Read and write Figma `.fig` binary files directly — no REST API, no upload/download round-trip.

## When to use

- You need to read or modify a Figma design file programmatically
- You want to sync design tokens (colors, spacing, radii) between JSON and `.fig` VARIABLE nodes
- You need to extract a color palette, spacing scale, or component library from a `.fig` file
- You want to batch-update styles or variables in a Figma file
- **An AI agent needs to inspect the contents of a `.fig` file** (colors, frames, text, hierarchy) without opening Figma

## When NOT to use

- You need real-time collaboration or multiplayer features — use the Figma REST API or Plugin API
- You are building a Figma plugin — use the official Figma Plugin API
- You need to create new Figma files from scratch — this skill is for editing existing `.fig` files

## Prerequisites

```bash
npm install openfig-core zstd-codec
```

## Core concepts

A `.fig` file is a ZIP archive containing:

1. **`canvas.fig`** — The main document (prelude + version + schema + message)
2. **`meta.json`** — File metadata
3. **`thumbnail.png`** — Thumbnail image
4. **`images/`** — Embedded images

The `canvas.fig` contains a protobuf-like structure where the **message** is Zstd-compressed.

### Document structure (after `parseFig`)

`parseFig` returns a `FigDocument` with multiple views of the same data:

```javascript
{
  // High-level traversal API (recommended)
  nodes: FigNode[],                     // all nodes as flat array
  nodeMap: Map<string, FigNode>,        // id → node (for O(1) lookup)
  childrenMap: Map<string, FigNode[]>,   // parent id → children (hierarchy)
  
  // Low-level raw access
  message: {
    nodeChanges: [                      // same nodes, direct from kiwi message
      { type: 'VARIABLE', name: 'color/sand/100', guid: { sessionID, localID }, ... },
      { type: 'VARIABLE', name: 'color/blue/500', ... },
      // ... other nodes
    ]
  },
  
  header: { prelude: 'fig-kiwi', version: 52 },
  schema: any,                          // decoded kiwi binary schema
  compiledSchema: any,                   // compiled schema (encodeMessage/decodeMessage)
  rawChunks: Uint8Array[],             // raw length-prefixed binary chunks
  meta: Record<string, any>,           // meta.json contents
  thumbnail: Uint8Array,               // thumbnail.png bytes
  images: Map<string, Uint8Array>      // filename → image bytes
}
```

**Prefer `doc.nodes` over `doc.message.nodeChanges`** for general traversal. Use `doc.message.nodeChanges` only when you need to mutate and re-encode the file.

### FigNode structure

```typescript
interface FigNode {
  guid: { sessionID: number; localID: number };
  type: string;                          // FRAME, TEXT, ELLIPSE, SYMBOL, INSTANCE, VARIABLE, ...
  name: string;
  phase?: string;                        // CREATED, REMOVED, etc.
  parentIndex?: { guid: FigGuid; position: string };
  size?: { x: number; y: number };
  transform?: { m00, m01, m02, m10, m11, m12: number };
  fillPaints?: FigPaint[];
  textData?: { characters: string };
  [key: string]: any;                    // open for all kiwi-decoded fields
}
```

### VARIABLE node

Figma stores design tokens as `VARIABLE` nodes with:
- `name` — slash-separated path (e.g., `color/sand/100`)
- `guid` — `{ sessionID, localID }` (used for referencing)
- `resolvedData` — The actual value (color, number, string, etc.)

**Important**: `.fig` VARIABLE names use **slashes** (`color/sand/100`), while JSON design tokens typically use **dots** (`color.sand.100`).

## Workflow

### 1. Parse a `.fig` file

```javascript
import { parseFig } from 'openfig-core';
import { readFileSync } from 'fs';

const data = new Uint8Array(readFileSync('design.fig'));
const doc = parseFig(data);

console.log(doc.header);          // { prelude: 'fig-kiwi', version: 52 }
console.log(doc.nodes.length);    // number of nodes in the file
```

### 2. Traverse the node tree (read-only)

For reading and analysis, use the high-level `nodes` / `nodeMap` / `childrenMap` APIs:

```javascript
import { nodeId } from 'openfig-core';

// Walk all nodes
for (const node of doc.nodes) {
  const id = nodeId(node);                      // "sessionID:localID" (e.g. "1:127")
  const children = doc.childrenMap.get(id) ?? [];
  console.log(`${id} ${node.type} "${node.name}" (${children.length} children)`);
}

// O(1) lookup by ID
const targetNode = doc.nodeMap.get('1:127');

// Filter by type
const variables = doc.nodes.filter(n => n.type === 'VARIABLE');
const frames = doc.nodes.filter(n => n.type === 'FRAME');
```

### 3. Build a GUID map

Every VARIABLE has a unique GUID. Build a map for lookups:

```javascript
// Using message.nodeChanges (the mutable source)
const guidMap = new Map();
for (const node of doc.message.nodeChanges) {
  if (node.type === 'VARIABLE') {
    guidMap.set(node.name, {
      sessionID: node.guid.sessionID,
      localID: node.guid.localID
    });
  }
}

// Or using nodeId helper with doc.nodes
const guidMap2 = new Map();
for (const node of doc.nodes) {
  if (node.type === 'VARIABLE') {
    guidMap2.set(node.name, nodeId(node));
  }
}
```

### 4. Read color values

```javascript
function getVariableColor(node) {
  // Figma stores colors in resolvedData with different structures
  // depending on the color type (SOLID, etc.)
  const data = node.resolvedData;
  if (data.color) {
    return {
      r: data.color.r,
      g: data.color.g,
      b: data.color.b,
      a: data.color.a ?? 1
    };
  }
  return null;
}
```

### 5. Modify VARIABLE values

When modifying, you **must** mutate `doc.message.nodeChanges` (the source of truth for encoding):

```javascript
// Update a VARIABLE's color
const variable = doc.message.nodeChanges.find(
  n => n.type === 'VARIABLE' && n.name === 'color/sand/100'
);

if (variable) {
  variable.resolvedData = {
    ...variable.resolvedData,
    color: { r: 1, g: 1, b: 1, a: 1 }  // #FFFFFF
  };
}
```

### 6. Re-encode and save

```javascript
import { encodeFigParts, assembleCanvasFig, createFigZip } from 'openfig-core';
import { run as zstdRun } from 'zstd-codec';
import { writeFileSync } from 'fs';

// Break the document into parts
const parts = encodeFigParts(doc);

// Re-compress the message with Zstd
zstdRun(zstd => {
  const simple = new zstd.Simple();
  const messageCompressed = simple.compress(parts.messageRaw, 3);

  // Reassemble the canvas.fig
  const canvasFig = assembleCanvasFig({
    prelude: parts.prelude,
    version: parts.version,
    schemaCompressed: parts.schemaCompressed,
    messageCompressed,
    passThrough: parts.passThrough
  });

  // Create the final .fig ZIP
  const figZip = createFigZip({
    canvasFig,
    meta: doc.meta,
    thumbnail: doc.thumbnail,
    images: doc.images
  });

  writeFileSync('design.fig', figZip);
});
```

## TypeScript support

`zstd-codec` has no `@types` package. Declare types locally:

```typescript
// types/zstd-codec.d.ts
export interface Simple {
  compress(data: Buffer | Uint8Array, level?: number): Uint8Array;
}

export interface ZstdCodec {
  Simple: new () => Simple;
}

export function run(callback: (zstd: ZstdCodec) => void): void;
```

Add to `tsconfig.json`:

```json
{
  "compilerOptions": {
    "types": ["node"]
  },
  "include": ["*.js", "types/**/*.d.ts"]
}
```

## Common patterns

### Sync JSON tokens → .fig

1. Parse `.fig`
2. Flatten JSON tokens to dot-paths (e.g., `color.sand.100`)
3. Convert dot-paths to slash-paths (`color/sand/100`)
4. Find matching VARIABLE by name
5. Update `resolvedData` with new value
6. Re-encode and save

Or use the provided script:

```bash
node scripts/json-to-fig.js tokens.json design.fig
```

### Sync .fig → JSON

1. Parse `.fig`
2. Iterate all VARIABLE nodes
3. Convert slash-paths to dot-paths
4. Build nested JSON structure
5. Write to `primitive.json`

Or use the provided script:

```bash
node scripts/fig-to-json.js design.fig tokens.json
```

### Extract all VARIABLEs as a list

```bash
node scripts/list-fig-variables.js design.fig variables.json
```

Outputs:

```json
{
  "color/sand/100": {
    "id": "1:127",
    "sessionID": 1,
    "localID": 127,
    "value": {
      "type": "color",
      "r": 0.9,
      "g": 0.89,
      "b": 0.87,
      "a": 1
    }
  }
}
```

### Name mismatch handling

JSON semantic tokens may use different naming than `.fig` VARIABLEs:

```javascript
// JSON: color.background.canvas
// .fig:  color/bg/canvas

const jsonToFig = (jsonPath) => jsonPath
  .replace(/background/g, 'bg')
  .replace(/\./g, '/');
```

## Safety notes

- **Always backup** the original `.fig` before writing
- `.fig` is a binary format — diffs are not human-readable
- GUIDs must remain stable — do not regenerate them
- Zstd compression level 3 is the default used by Figma
- The `passThrough` field in `encodeFigParts` must be preserved exactly

## Tools

| Package | Purpose |
|---------|---------|
| `openfig-core` | Parse/encode `.fig` format |
| `zstd-codec` | Zstd compression for the message section |

## References

- [openfig-core npm](https://www.npmjs.com/package/openfig-core)
- [zstd-codec npm](https://www.npmjs.com/package/zstd-codec)
- Figma `.fig` format: undocumented but reverse-engineered by the community
