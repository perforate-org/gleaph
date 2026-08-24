#!/usr/bin/env bash
# Build the gpui-graph web example into ./dist as a plain static site.
#
# Outputs two wasm modules (wasm-bindgen --target web glue):
#   - gpui_graph_web_example_app.*    main-thread GPUI application (index.html)
#   - gpui_graph_web_example_worker.* worker backend module, imported by
#                                     worker.js
#
# Serve with any static file server; no COOP/COEP/CORP headers are used:
#   python3 -m http.server 8080 --directory dist
set -euo pipefail
cd "$(dirname "$0")"

OUT=dist

cargo build \
    -p gpui-graph-web-example-app \
    --target wasm32-unknown-unknown \
    --release
wasm-bindgen --target web --out-dir "$OUT" \
    "target/wasm32-unknown-unknown/release/gpui_graph_web_example_app.wasm"

cargo build \
    -p gpui-graph-web-example-worker \
    --target wasm32-unknown-unknown \
    --release
wasm-bindgen --target web --out-dir "$OUT" \
    "target/wasm32-unknown-unknown/release/gpui_graph_web_example_worker.wasm"

cp assets/index.html assets/worker.js "$OUT/"

echo
echo "Built $OUT — serve it statically, e.g.:"
echo "  python3 -m http.server 8080 --directory $OUT"
echo "Then open http://127.0.0.1:8080/?mode=worker (opt-in Worker source) or"
echo "http://127.0.0.1:8080/ (InProcess default)."
