#!/usr/bin/env bash
# Build the ADR 0076 S4a explorer web entry into ./dist as a plain static site.
#
# Outputs two wasm modules (wasm-bindgen --target web glue):
#   - gleaph_explorer_web_app.*    main-thread GPUI application (index.html)
#   - gleaph_explorer_web_worker.* worker backend + timing harness module,
#                                  imported by worker.js / bench_worker.js
#
# Serve with any static file server; no COOP/COEP/CORP headers are used:
#   python3 -m http.server 8080 --directory dist
set -euo pipefail
cd "$(dirname "$0")"

OUT=dist

cargo build \
    -p gleaph-explorer-web-app \
    --target wasm32-unknown-unknown \
    --release
wasm-bindgen --target web --out-dir "$OUT" \
    "target/wasm32-unknown-unknown/release/gleaph_explorer_web_app.wasm"

cargo build \
    -p gleaph-explorer-web-worker \
    --target wasm32-unknown-unknown \
    --release
wasm-bindgen --target web --out-dir "$OUT" \
    "target/wasm32-unknown-unknown/release/gleaph_explorer_web_worker.wasm"

cp assets/index.html assets/harness.html assets/worker.js assets/bench_worker.js "$OUT/"

echo
echo "Built $OUT — serve it statically, e.g.:"
echo "  python3 -m http.server 8080 --directory $OUT"
echo "Then open http://127.0.0.1:8080/ (app) and /harness.html (timing)."
