#!/usr/bin/env bash
# Compare LabelBucketStore bucket-row growth policies via canbench (instruction counts).
# Run from repo root or this crate directory. Requires `canbench` and PocketIC (see canbench.yml).
set -euo pipefail
cd "$(dirname "$0")/.."
PATTERN="${1:-bench_labeled_insert_fresh_label_each_edge_256}"
RUNTIME_FLAGS=()
if [[ -n "${POCKET_IC_BIN:-}" ]]; then
  RUNTIME_FLAGS+=(--runtime-path "$POCKET_IC_BIN")
elif [[ -x "${HOME}/.cache/pocket-ic/pocket-ic" ]]; then
  RUNTIME_FLAGS+=(--runtime-path "${HOME}/.cache/pocket-ic/pocket-ic")
fi
if [[ "${CANBENCH_NO_RUNTIME_INTEGRITY_CHECK:-1}" == 1 ]]; then
  RUNTIME_FLAGS+=(--no-runtime-integrity-check)
fi

run_one() {
  local name="$1"
  local extra="${2:-}"
  echo ""
  echo "========== ${name} =========="
  export CANBENCH_IC_STABLE_LARA_FEATURES="${extra}"
  canbench --less-verbose --show-summary "${RUNTIME_FLAGS[@]}" "${PATTERN}"
}

run_one "grow 1.25x (default)" ""
run_one "grow 1.5x ceil" "bucket_row_grow_150"
run_one "grow 2x (historical)" "bucket_row_grow_double"
