# Gleaph Design Tokens

Design token system for Gleaph, following the W3C Design Tokens Community Group (DTCG) specification.

## Structure

```
design-tokens/
├── gleaph-design-system.fig   # Figma source file (single source of truth)
├── primitive.json             # Primitive tokens (colors, space, radius)
├── semantic-light.json        # Semantic tokens (light theme)
├── semantic-dark.json         # Semantic tokens (dark theme)
├── tokens.resolver.json       # DTCG Resolver (theme modifier)
└── scripts/
    ├── resolve-to-css.js      # JSON → CSS (shadcn/ui compatible)
    ├── sync-to-fig.js         # JSON → .fig (Figma sync)
    └── generate-gpui-theme.js  # JSON → Rust theme (gleaph-theme crate)
```

## Token Layers

| Layer | File | Description |
|-------|------|-------------|
| Primitive | `primitive.json` | Raw values: colors, dimensions, radii |
| Semantic | `semantic-{light,dark}.json` | Contextual values (background, text, accent) via aliases |
| Theme | `tokens.resolver.json` | Combines primitives with theme modifier (light/dark) |

## Workflow

### Editing Tokens

Edit the JSON files directly. All files follow the W3C DTCG 2025.10 format:

```json
{
  "$type": "color",
  "$value": {
    "colorSpace": "srgb",
    "components": [0.902, 0.894, 0.871],
    "hex": "#E6E4DE"
  }
}
```

Semantic tokens reference primitives via aliases:

```json
{
  "$value": "{color.sand.100}"
}
```

### Generating GPUI Theme

To generate the Rust theme module for GPUI:

```bash
cd scripts
node generate-gpui-theme.js
```

This writes to `crates/gleaph-theme/src/lib.rs` with:
- `Theme` struct (`light()` / `dark()` constructors)
- `ColorPalette` (sand, blue, clay primitives)
- `SpaceScale` and `RadiusScale`

The generated crate is `gleaph-theme`, separate from the generic `gpui-graph` crate.

### Generating CSS

Run the resolver script to update the dashboard CSS:

```bash
cd scripts
node resolve-to-css.js
```

This writes to `../frontend/apps/dashboard/src/index.css` with:
- shadcn/ui compatible HSL variables (`--background`, `--primary`, etc.)
- Primitive hex variables (`--color-sand-100`, `--color-blue-500`, etc.)
- Semantic variables (`--color-background-canvas`, `--color-text-primary`, etc.)
- Light and dark theme blocks

### Syncing to Figma

To update the `.fig` file with JSON changes:

```bash
cd scripts
node sync-to-fig.js
```

This updates VARIABLE nodes inside the `.fig` binary.

## Color Palette

### Sand (Neutral)

| Token | Hex | Usage |
|-------|-----|-------|
| `color.sand.0` | `#FFFFFF` | Pure white |
| `color.sand.50` | `#F5F3EF` | Off-white |
| `color.sand.100` | `#E6E4DE` | Light beige (canvas bg) |
| `color.sand.200` | `#D1D1D1` | Light gray (borders) |
| `color.sand.300` | `#C1C1C1` | Gray |
| `color.sand.400` | `#A0A0A0` | Medium gray |
| `color.sand.500` | `#808080` | Gray |
| `color.sand.600` | `#606060` | Dark gray (secondary text) |
| `color.sand.700` | `#404040` | Darker gray |
| `color.sand.800` | `#373737` | Near black |
| `color.sand.900` | `#212121` | Very dark (primary text) |
| `color.sand.950` | `#161616` | Almost black (dark canvas) |

### Blue (Accent)

| Token | Hex | Usage |
|-------|-----|-------|
| `color.blue.300` | `#6589F5` | Focus rings |
| `color.blue.400` | `#3460E6` | Dark mode accent |
| `color.blue.500` | `#284DBE` | Primary accent |
| `color.blue.600` | `#0D69BE` | Deep blue |
| `color.blue.700` | `#0059FF` | Bright blue |

### Semantic Tokens (Light / Dark)

| Token | Light | Dark |
|-------|-------|------|
| `color.background.canvas` | `#E6E4DE` (sand.100) | `#161616` (sand.950) |
| `color.background.surface` | `#FFFFFF` (sand.0) | `#212121` (sand.900) |
| `color.text.primary` | `#212121` (sand.900) | `#FFFFFF` (sand.0) |
| `color.text.secondary` | `#606060` (sand.600) | `#C1C1C1` (sand.300) |
| `color.accent.primary` | `#284DBE` (blue.500) | `#3460E6` (blue.400) |
| `color.border.default` | `#D1D1D1` (sand.200) | `#404040` (sand.700) |
| `color.focus.ring` | `#6589F5` (blue.300) | `#6589F5` (blue.300) |

## Dimensions

| Token | Value |
|-------|-------|
| `space.4` | `16px` |
| `space.6` | `24px` |
| `radius.md` | `4px` |

## Specification

This project follows the [W3C DTCG Design Tokens Format Module 2025.10](https://www.designtokens.org/TR/2025.10/format/).

Key features used:
- `$type` / `$value` token properties
- `$extends` group extension
- `{path.to.token}` alias references
- Resolver modifiers for multi-theme support

## Integration

### Tailwind CSS / shadcn/ui

The generated `index.css` exposes CSS custom properties that map directly to Tailwind theme tokens:

```css
/* Light theme */
--background: 45 13.8% 88.6%;     /* hsl for bg-background */
--primary: 225 65.2% 45.1%;       /* hsl for bg-primary */
--border: 0 0% 82%;                /* hsl for border-border */

/* Direct access */
--color-sand-100: #E6E4DE;
--color-blue-500: #284DBE;
```

### Future: GPUI

GPUI (Rust) theme generation is planned. The JSON structure is ready for code generation into Rust constants or theme structs.

## Maintenance

- **Do not edit** `frontend/apps/dashboard/src/index.css` manually — it is auto-generated
- **Do not edit** `.fig` variables via Figma UI when JSON is the source of truth
- Run `sync-to-fig.js` after JSON changes to keep `.fig` in sync

## License

MIT
