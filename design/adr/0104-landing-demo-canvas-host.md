# 0104. The landing-page graph demo is a canvas host over the wasm core

Date: 2026-11-09
Status: implemented (P0/P1; the page's polish phase is tracked in `plans/web-lp-graph-demo-2026-11-09.md`)

Related: [ADR 0103](0103-console-host-on-rune.md) (the console's Rune host over the same core),
[ADR 0076](0076-explorer-web-worker-backend.md) (the frame/transport seam the bridge uses).

## Context

The landing page should show the product working: a query editor, presets, and the graph those
queries match. Two ways to render it were on the table:

1. **Embed Rune in the page.** Rune does have a browser platform: `App::canvas_id` mounts into an
   existing canvas, `App::web_inline()` embeds, WebGPU falls back to WebGL2, `handle.quit()`
   unmounts (`crates/rune/src/platform/web.rs`). That would reuse the console's UI wholesale.
2. **A thin wasm bridge over `graph-view-core` plus a hand-written canvas host.** The core is
   renderer-agnostic by construction: `PaintFrame` is the render-side contract and ADR 0076 already
   defined its wire form.

## Decision

Option 2. The page ships a small wasm module (`crates/graph-view-web`) that owns the scene, layout
runtime, viewport, hit test and label placement, and returns the frame as flat planes; the page
draws the planes on a 2D canvas and renders the labels as DOM text.

Reasons, measured 2026-11-09 unless noted:

| Concern | Rune embed | canvas host |
| --- | --- | --- |
| Payload | wgpu + parley + winit + resvg: several MB | wasm 180,330 B, **gzip ~77 KB** |
| Typography | parley wants TTF/OTF; the page ships Geist woff2 | DOM labels use the page's Geist directly |
| Accessibility | canvas only | labels, presets and rows are DOM; the canvas carries a summary label |
| Tone | Rune palette + font plumbing to match the page | CSS custom properties, one palette |
| SDK | JS SDK behind a wasm-bindgen bridge | the page's own `@gleaph/sdk` |
| Web gaps | no cursors, no physical key codes, main-thread work | none of these |

Rune remains the right host for the desktop console (ADR 0103), and would be the right choice for a
page that shows *that console* in a browser; that page would accept the payload and font cost.

## Consequences

- The core owns the paint frame, layout, culling, LOD, hit testing and label placement
  (`label_layout`), so the page and the console cannot drift.
- The canvas host is replaceable: a WebGL renderer can consume the same planes.
- The wasm module is generated (`wasm-bindgen --target web` + `wasm-opt -Oz`) into
  `frontend/apps/web-lp/public/graph-view/` by
  `frontend/apps/web-lp/scripts/build-graph-view-wasm.sh`; the generated files are committed so the
  static site builds without a Rust toolchain.
- The module is rebuilt when `crates/graph-view-web` or `crates/graph-view-core` changes; the script
  is the single place that knows how.
