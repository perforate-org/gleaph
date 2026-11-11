#!/usr/bin/env bash
# Run the natural-language routing gates against one fine-tuned checkpoint.
#
#   bash scripts/eval-gates.sh                          # regression + blind (blind report-only)
#   GATES=blind bash scripts/eval-gates.sh              # blind only
#   EXIT_ON_BLIND_FAIL=1 bash scripts/eval-gates.sh     # strict: blind floor misses fail too
#   MODEL_DIR=target/laya-routing/model12 bash scripts/eval-gates.sh
#
# Policy: the hand-written set is a REGRESSION gate (must pass). The frozen blind set is a
# QUALITY signal: it is reported and recorded, but its floor misses do not fail the run unless
# EXIT_ON_BLIND_FAIL=1 is set. This keeps an honest blind number visible without lowering floors
# or silently dropping the check.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$DEMO_DIR/../.." && pwd)"

MODEL_DIR="${MODEL_DIR:-$REPO_ROOT/target/laya-routing/model20-distil14b}"
MODEL_NAME="${MODEL_NAME:-gate-checkpoint}"
PORT="${PORT:-8084}"
ENDPOINT="http://127.0.0.1:${PORT}/v1/systemone"
PYTHON="${PYTHON:-$REPO_ROOT/target/venv/bin/python}"
SERVE_SCRIPT="$REPO_ROOT/target/laya-routing/serve.py"
GATES="${GATES:-both}"
EXIT_ON_BLIND_FAIL="${EXIT_ON_BLIND_FAIL:-0}"
REPORT_DIR="$REPO_ROOT/target/laya-routing/gate-reports"
REGRESSION_INPUT="$DEMO_DIR/eval/routing/wild-handwritten-2026-11-09.jsonl"
BLIND_INPUT="$DEMO_DIR/eval/routing/blind-2026-11-09.jsonl"

if [[ ! -d "$MODEL_DIR" ]]; then
  echo "gate: checkpoint not found: $MODEL_DIR" >&2
  echo "gate: train or point MODEL_DIR at a checkpoint directory" >&2
  exit 2
fi
if [[ ! -x "$PYTHON" ]]; then
  echo "gate: python not found: $PYTHON (set PYTHON=...)" >&2
  exit 2
fi

mkdir -p "$REPORT_DIR"
SERVER_LOG="$REPORT_DIR/serve.log"
SERVER_PID=""
cleanup() {
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if [[ "$GATES" == "both" || "$GATES" == "blind" ]]; then
  echo "gate: starting $MODEL_NAME from $MODEL_DIR on :$PORT"
  HF_HOME="$REPO_ROOT/target/hf-home" \
    XDG_CACHE_HOME="$REPO_ROOT/target/cache" \
    TMPDIR="$REPO_ROOT/target/tmp" \
    "$PYTHON" "$SERVE_SCRIPT" "$PORT" "$MODEL_DIR" >"$SERVER_LOG" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 120); do
    if curl -s --max-time 2 "http://127.0.0.1:${PORT}/v1/models" >/dev/null 2>&1; then
      break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
      echo "gate: model server exited; see $SERVER_LOG" >&2
      tail -20 "$SERVER_LOG" >&2 || true
      exit 2
    fi
    sleep 2
  done
  if ! curl -s --max-time 2 "http://127.0.0.1:${PORT}/v1/models" >/dev/null 2>&1; then
    echo "gate: model server did not become ready; see $SERVER_LOG" >&2
    exit 2
  fi
fi

REG_RC=0
BLIND_RC=0
if [[ "$GATES" == "both" || "$GATES" == "regression" ]]; then
  echo "== regression gate (hand-written set; must pass) =="
  node "$DEMO_DIR/scripts/routing-gate.mjs" \
    --input "$REGRESSION_INPUT" \
    --model-url "$ENDPOINT" --model "$MODEL_NAME" \
    --json "$REPORT_DIR/regression.json" || REG_RC=$?
fi
if [[ "$GATES" == "both" || "$GATES" == "blind" ]]; then
  echo "== blind gate (frozen set; quality signal) =="
  BLIND_FLAGS=()
  if [[ "$EXIT_ON_BLIND_FAIL" != "1" ]]; then
    BLIND_FLAGS+=(--report-only)
  fi
  node "$DEMO_DIR/scripts/routing-gate.mjs" \
    --input "$BLIND_INPUT" \
    --model-url "$ENDPOINT" --model "$MODEL_NAME" \
    "${BLIND_FLAGS[@]}" \
    --json "$REPORT_DIR/blind.json" || BLIND_RC=$?
fi

echo "gate: reports in $REPORT_DIR"
if [[ "$EXIT_ON_BLIND_FAIL" == "1" && "$BLIND_RC" != "0" ]]; then
  echo "gate: strict blind gate failed" >&2
  exit "$BLIND_RC"
fi
if [[ "$REG_RC" != "0" ]]; then
  echo "gate: regression gate failed" >&2
  exit "$REG_RC"
fi
echo "gate: pass"
