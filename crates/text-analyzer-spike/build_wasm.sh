#!/usr/bin/env bash
# Plan 0330 spike: build each candidate feature for wasm32-unknown-unknown and report
# the total wasm size + data-section split (via wasm-opt's output size and
# `wasm-opt --emit-text` section accounting when useful). A 1 KB "empty-analyze"
# baseline (no candidate feature) anchors the deltas.
#
# Usage: ./build_wasm.sh
# Results are printed as `wasm32[<candidate>]: <bytes> bytes` lines for the plan Audit.
set -euo pipefail
cd "$(dirname "$0")"

PROFILE=release
TARGET=wasm32-unknown-unknown
OUT_DIR="$(pwd)/wasm-out"
mkdir -p "$OUT_DIR"

build() {
  local name="$1"; shift
  echo "== building wasm32 ($name)" >&2
  if cargo build --target "$TARGET" --profile "$PROFILE" "$@" >/dev/null 2>&1; then
    local wasm
    wasm=$(ls -S ../../target/wasm32-unknown-unknown/$PROFILE/*.wasm 2>/dev/null | head -1)
    if [ -z "${wasm}" ]; then
      # library builds emit rlibs, not wasm; use a cdylib scratch target instead
      echo "no .wasm artifact for $name (library crate) — see spike-lib build" >&2
      return
    fi
    cp "$wasm" "$OUT_DIR/$name.wasm"
    echo "wasm32[$name]: $(wc -c < "$OUT_DIR/$name.wasm") bytes"
  else
    echo "wasm32[$name]: BUILD FAILED" | tee -a "$OUT_DIR/failures.txt"
  fi
}

# The spike crate is a library, so add a cdylib profile artifact for size accounting.
# cargo builds --lib produce rlibs only; we measure via `cargo build --lib` + the
# rlib->wasm path is not meaningful. Instead each candidate is built as a cdylib
# through the `cdylib-shim` (src/bin wasm shim? we build the lib with --crate-type
# cdylib through RFLAGS below).
export RUSTFLAGS="${RUSTFLAGS:-}"
for candidate in "" rule vibrato sudachi lindera; do
  features_arg=""
  if [ -n "$candidate" ]; then
    features_arg="--features $candidate"
  fi
  name="${candidate:-baseline}"
  echo "== building wasm32 ($name)" >&2
  if cargo rustc --target "$TARGET" --profile "$PROFILE" $features_arg \
      --crate-type cdylib >/dev/null 2>&1; then
    wasm=$(ls -t ../../target/wasm32-unknown-unknown/$PROFILE/text_analyzer_spike*.wasm 2>/dev/null | head -1)
    if [ -n "$wasm" ]; then
      cp "$wasm" "$OUT_DIR/$name.wasm"
      echo "wasm32[$name]: $(wc -c < "$OUT_DIR/$name.wasm") bytes"
    else
      echo "wasm32[$name]: NO ARTIFACT" | tee -a "$OUT_DIR/failures.txt"
    fi
  else
    echo "wasm32[$name]: BUILD FAILED" | tee -a "$OUT_DIR/failures.txt"
  fi
done

echo "wasm32 size table (bytes):" >&2
ls -l "$OUT_DIR"/*.wasm 2>/dev/null | awk '{print $9, $5}'
