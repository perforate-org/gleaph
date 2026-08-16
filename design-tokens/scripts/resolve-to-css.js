/**
 * Gleaph Design Token Resolver
 * Resolves DTCG format tokens into CSS variables with shadcn/ui compatible mapping.
 */

import { readFileSync, writeFileSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));

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

function resolveAlias(value, primitives) {
  if (typeof value === 'string' && value.startsWith('{') && value.endsWith('}')) {
    const path = value.slice(1, -1);
    const resolved = primitives[path];
    if (!resolved) throw new Error(`Unresolved alias: ${value}`);
    return resolved;
  }
  return value;
}

function colorToHex(value) {
  if (typeof value === 'string') return value;
  if (value && typeof value === 'object') {
    if (value.colorSpace === 'srgb' && value.components) {
      const [r, g, b] = value.components;
      const ri = Math.round(r * 255);
      const gi = Math.round(g * 255);
      const bi = Math.round(b * 255);
      return `#${ri.toString(16).padStart(2,'0')}${gi.toString(16).padStart(2,'0')}${bi.toString(16).padStart(2,'0')}`;
    }
    if (value.hex) return value.hex;
  }
  return value;
}

function hexToHsl(hex) {
  let r = parseInt(hex.slice(1, 3), 16) / 255;
  let g = parseInt(hex.slice(3, 5), 16) / 255;
  let b = parseInt(hex.slice(5, 7), 16) / 255;

  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  let h = 0, s = 0, l = (max + min) / 2;

  if (max !== min) {
    const d = max - min;
    s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
    switch (max) {
      case r: h = ((g - b) / d + (g < b ? 6 : 0)) / 6; break;
      case g: h = ((b - r) / d + 2) / 6; break;
      case b: h = ((r - g) / d + 4) / 6; break;
    }
  }

  h = Math.round(h * 360);
  s = Math.round(s * 1000) / 10;
  l = Math.round(l * 1000) / 10;

  return `${h} ${s}% ${l}%`;
}

function dimensionToCss(value) {
  if (value && typeof value === 'object' && 'value' in value && 'unit' in value) {
    return `${value.value}${value.unit}`;
  }
  return value;
}

function generateThemeTokens(primitiveFile, semanticFile) {
  const primitiveData = JSON.parse(readFileSync(primitiveFile, 'utf8'));
  const semanticData = JSON.parse(readFileSync(semanticFile, 'utf8'));

  const primitives = flattenTokens(primitiveData);
  const semantic = flattenTokens(semanticData);

  // Resolve semantic aliases
  const resolved = {};
  for (const [key, value] of Object.entries(semantic)) {
    resolved[key] = resolveAlias(value, primitives);
  }

  // Resolve primitives to hex for further alias resolution
  const primitiveHex = {};
  for (const [key, value] of Object.entries(primitives)) {
    primitiveHex[key] = colorToHex(value);
  }
  const semanticHex = {};
  for (const [key, value] of Object.entries(resolved)) {
    semanticHex[key] = colorToHex(value);
  }

  // shadcn/ui compatible mappings (color names → hex)
  const get = (path) => {
    if (semanticHex[path]) return semanticHex[path];
    if (primitiveHex[path]) return primitiveHex[path];
    throw new Error(`Token not found: ${path}`);
  };

  const map = {
    background:       get('color.background.canvas'),
    foreground:       get('color.text.primary'),
    card:             get('color.background.surface'),
    'card-foreground': get('color.text.primary'),
    popover:          get('color.background.surface'),
    'popover-foreground': get('color.text.primary'),
    primary:          get('color.accent.primary'),
    'primary-foreground': get('color.sand.0'),
    secondary:        get('color.background.canvas'),
    'secondary-foreground': get('color.text.primary'),
    muted:            get('color.sand.50'),
    'muted-foreground': get('color.text.secondary'),
    accent:           get('color.accent.primary'),
    'accent-foreground': get('color.sand.0'),
    destructive:      '#ef4444',
    'destructive-foreground': '#f8fafc',
    border:           get('color.border.default'),
    input:            get('color.border.default'),
    ring:             get('color.focus.ring'),
    radius:           dimensionToCss(primitives['radius.md']),
  };

  // Primitive + Semantic tokens as flat CSS vars
  const allPrimitiveVars = {};
  for (const [key, value] of Object.entries(primitives)) {
    const cssKey = `--${key.replace(/\./g, '-')}`;
    if (key.startsWith('color.')) {
      allPrimitiveVars[cssKey] = colorToHex(value);
    } else {
      allPrimitiveVars[cssKey] = dimensionToCss(value);
    }
  }

  const allSemanticVars = {};
  for (const [key, value] of Object.entries(resolved)) {
    const cssKey = `--${key.replace(/\./g, '-')}`;
    allSemanticVars[cssKey] = colorToHex(value);
  }

  return { map, primitiveVars: allPrimitiveVars, semanticVars: allSemanticVars };
}

function buildThemeBlock(theme) {
  const lines = [];
  // shadcn/ui tokens (with -- prefix, HSL values for colors, raw for radius)
  for (const [key, value] of Object.entries(theme.map)) {
    const cssVar = `--${key}`;
    if (key === 'radius') {
      lines.push(`    ${cssVar}: ${value};`);
    } else {
      lines.push(`    ${cssVar}: ${hexToHsl(value)};`);
    }
  }
  return lines.join('\n');
}

function buildBlockRaw(vars) {
  const lines = [];
  for (const [key, value] of Object.entries(vars)) {
    lines.push(`    ${key}: ${value};`);
  }
  return lines.join('\n');
}

// Main
const baseDir = join(__dirname, '..');

const light = generateThemeTokens(
  join(baseDir, 'primitive.json'),
  join(baseDir, 'semantic-light.json')
);

const dark = generateThemeTokens(
  join(baseDir, 'primitive.json'),
  join(baseDir, 'semantic-dark.json')
);

// Build output
const output = `/* Gleaph Design Tokens */
/* Generated from W3C DTCG format */
/* Do not edit manually */

@tailwind base;
@tailwind components;
@tailwind utilities;

@layer base {
  :root {
    /* shadcn/ui compatible theme tokens */
${buildThemeBlock(light)}

    /* Primitive tokens */
${buildBlockRaw(light.primitiveVars)}

    /* Semantic tokens */
${buildBlockRaw(light.semanticVars)}
  }

  .dark {
    /* shadcn/ui compatible theme tokens */
${buildThemeBlock(dark)}

    /* Primitive tokens */
${buildBlockRaw(dark.primitiveVars)}

    /* Semantic tokens */
${buildBlockRaw(dark.semanticVars)}
  }
}

@layer base {
  * {
    @apply border-border;
  }
  body {
    @apply bg-background text-foreground;
  }
}
`;

const outputPath = join(baseDir, '..', 'frontend', 'apps', 'dashboard', 'src', 'index.css');
writeFileSync(outputPath, output);
console.log(`Wrote ${outputPath}`);
