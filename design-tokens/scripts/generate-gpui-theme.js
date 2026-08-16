/**
 * Gleaph Design Token Resolver for GPUI
 * Generates Rust theme code from DTCG format JSON tokens.
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

function hexToGpuiHsla(hex) {
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

  return { h: h.toFixed(4), s: s.toFixed(4), l: l.toFixed(4), a: '1.0' };
}

function hslaConst(hex) {
  const c = hexToGpuiHsla(hex);
  return `hsla(${c.h}, ${c.s}, ${c.l}, ${c.a})`;
}

function resolveToHex(value) {
  if (typeof value === 'string' && value.startsWith('#')) return value;
  if (typeof value === 'object' && value.hex) return value.hex;
  return null;
}

// Load tokens
const primitiveData = JSON.parse(readFileSync(join(__dirname, '..', 'primitive.json'), 'utf8'));
const lightData = JSON.parse(readFileSync(join(__dirname, '..', 'semantic-light.json'), 'utf8'));
const darkData = JSON.parse(readFileSync(join(__dirname, '..', 'semantic-dark.json'), 'utf8'));

const primitives = flattenTokens(primitiveData);
const lightSemantic = flattenTokens(lightData);
const darkSemantic = flattenTokens(darkData);

const lightResolved = resolveSemantic(lightSemantic, primitives);
const darkResolved = resolveSemantic(darkSemantic, primitives);

// Palette entries
function generatePaletteEntries(primitives) {
  const lines = [];
  // Auto-detect color.* primitives and generate ColorPalette fields
  const colorPaths = Object.keys(primitives)
    .filter(k => k.startsWith('color.'))
    .sort();
  for (const path of colorPaths) {
    const parts = path.split('.');
    const fieldName = parts.slice(1).join('_');
    const hex = resolveToHex(primitives[path]);
    if (hex) {
      lines.push(`                ${fieldName}: ${hslaConst(hex)},`);
    }
  }
  return lines.join('\n');
}

function generateColorPaletteStruct(primitives) {
  const fields = [];
  const colorPaths = Object.keys(primitives)
    .filter(k => k.startsWith('color.'))
    .sort();
  for (const path of colorPaths) {
    const parts = path.split('.');
    const fieldName = parts.slice(1).join('_');
    fields.push(`    pub ${fieldName}: gpui::Hsla,`);
  }
  return fields.join('\n');
}

const rustCode = `// Auto-generated from Gleaph design tokens
// Source: design-tokens/primitive.json, semantic-light.json, semantic-dark.json
// Do not edit manually — regenerate with: node scripts/generate-gpui-theme.js

use gpui::hsla;
use gpui::TextStyle;
use gpui_graph::GraphStyle;

/// Gleaph application theme (light / dark variants).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// Canvas / window background.
    pub background: gpui::Hsla,
    /// Surface / card / panel background.
    pub surface: gpui::Hsla,
    /// Primary text.
    pub text_primary: gpui::Hsla,
    /// Secondary / muted text.
    pub text_secondary: gpui::Hsla,
    /// Accent / primary action.
    pub accent: gpui::Hsla,
    /// Text on accent backgrounds.
    pub accent_text: gpui::Hsla,
    /// Default borders.
    pub border: gpui::Hsla,
    /// Focus ring.
    pub focus_ring: gpui::Hsla,
    /// Destructive action (error).
    pub destructive: gpui::Hsla,
    /// Text on destructive backgrounds.
    pub destructive_text: gpui::Hsla,
    /// Primitive color palette.
    pub color: ColorPalette,
    /// Spacing scale.
    pub space: SpaceScale,
    /// Border radii.
    pub radius: RadiusScale,
}

/// Sand + Blue + Clay primitive color palette.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorPalette {
${generateColorPaletteStruct(primitives)}
}

/// Spacing primitive scale (px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpaceScale {
    pub space_4: f32,
    pub space_6: f32,
}

/// Border radius primitive scale (px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadiusScale {
    pub md: f32,
}

impl Theme {
    /// Light theme (default).
    pub fn light() -> Self {
        Self {
            background: ${hslaConst(resolveToHex(lightResolved['color.background.canvas']))},
            surface: ${hslaConst(resolveToHex(lightResolved['color.background.surface']))},
            text_primary: ${hslaConst(resolveToHex(lightResolved['color.text.primary']))},
            text_secondary: ${hslaConst(resolveToHex(lightResolved['color.text.secondary']))},
            accent: ${hslaConst(resolveToHex(lightResolved['color.accent.primary']))},
            accent_text: ${hslaConst(resolveToHex('#ffffff'))},
            border: ${hslaConst(resolveToHex(lightResolved['color.border.default']))},
            focus_ring: ${hslaConst(resolveToHex(lightResolved['color.focus.ring']))},
            destructive: ${hslaConst('#ef4444')},
            destructive_text: ${hslaConst('#f8fafc')},
            color: ColorPalette {
${generatePaletteEntries(primitives)}
            },
            space: SpaceScale {
                space_4: 16.0,
                space_6: 24.0,
            },
            radius: RadiusScale {
                md: 4.0,
            },
        }
    }

    /// Dark theme.
    pub fn dark() -> Self {
        Self {
            background: ${hslaConst(resolveToHex(darkResolved['color.background.canvas']))},
            surface: ${hslaConst(resolveToHex(darkResolved['color.background.surface']))},
            text_primary: ${hslaConst(resolveToHex(darkResolved['color.text.primary']))},
            text_secondary: ${hslaConst(resolveToHex(darkResolved['color.text.secondary']))},
            accent: ${hslaConst(resolveToHex(darkResolved['color.accent.primary']))},
            accent_text: ${hslaConst(resolveToHex('#ffffff'))},
            border: ${hslaConst(resolveToHex(darkResolved['color.border.default']))},
            focus_ring: ${hslaConst(resolveToHex(darkResolved['color.focus.ring']))},
            destructive: ${hslaConst('#ef4444')},
            destructive_text: ${hslaConst('#f8fafc')},
            color: ColorPalette {
${generatePaletteEntries(primitives)}
            },
            space: SpaceScale {
                space_4: 16.0,
                space_6: 24.0,
            },
            radius: RadiusScale {
                md: 4.0,
            },
        }
    }

    /// Build a graph visualization style from this theme.
    pub fn graph_style(&self) -> GraphStyle {
        GraphStyle::default()
            .with_node_fill(self.accent)
            .with_node_stroke_color(self.border)
            .with_node_stroke_width(1.0)
            .with_node_fill_selected(self.color.blue_400)
            .with_node_fill_hovered(self.color.blue_300)
            .with_edge_color(self.text_secondary)
            .with_edge_width(1.5)
            .with_edge_color_selected(self.accent)
            .with_edge_color_hovered(self.color.blue_300)
            .with_edge_arrow_enabled(true)
            .with_edge_arrow_size(8.0)
            .with_label_style(TextStyle {
                color: self.text_primary,
                ..TextStyle::default()
            })
            .with_label_offset(4.0)
    }
}
`;

const outputPath = join(__dirname, '..', '..', 'crates', 'gleaph-theme', 'src', 'lib.rs');
writeFileSync(outputPath, rustCode);
console.log(`Wrote ${outputPath}`);
