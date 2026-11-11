#!/usr/bin/env bash
# Regenerate the demo's wasm module from crates/graph-view-web.
#
# The page commits the generated files under public/graph-view/ so the static site builds without a
# Rust toolchain; run this whenever the bridge or the core changes.
#
# Requires: the wasm32-unknown-unknown target, a `wasm-bindgen` CLI matching the crate version in
# Cargo.lock, and `wasm-opt` (binaryen) on PATH.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo="$(cd "$here/../../.." && pwd)"
out="$here/public/graph-view"

cd "$repo"
cargo build -p graph-view-web --target wasm32-unknown-unknown --release
rm -rf "$out"
wasm-bindgen target/wasm32-unknown-unknown/release/graph_view_web.wasm \
  --out-dir "$out" --target web
wasm-opt -Oz "$out/graph_view_web_bg.wasm" -o "$out/graph_view_web_bg.wasm"
echo "wrote $out ($(wc -c < "$out/graph_view_web_bg.wasm") bytes)"
