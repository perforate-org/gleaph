#!/usr/bin/env bash
# Register+publish a probe op (owner), then execute as dev; print dev-run result.
# Usage: ./register-probe.sh <name>
set -uo pipefail
DEMO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export HOME="$DEMO/.icp/home"
export ICP_COREPACK_HOME="$DEMO/.icp/corepack-home"
export ICP_XDG_CACHE_HOME="$DEMO/.icp/xdg-cache"
export ICP_XDG_DATA_HOME="$DEMO/.icp/xdg-data"
export DO_NOT_TRACK=1
export GLEAPH_NETWORK="${GLEAPH_NETWORK:-http://localhost:32773}"
export GLEAPH_FETCH_ROOT_KEY=true
export GLEAPH_CANISTER="${GLEAPH_CANISTER:?set GLEAPH_CANISTER}"
NAME="$1"
GLEEAPH="$DEMO/../../target/debug/gleaph"
OWNER_PEM="$DEMO/.icp/home/Library/Application Support/org.dfinity.icp-cli/identity/keys/gleaph-demo-deployer.pem"

"$GLEEAPH" prepared apply --identity "$OWNER_PEM" >/dev/null || { echo "apply failed"; exit 1; }
"$GLEEAPH" prepared publish "$NAME" --identity "$OWNER_PEM" >/dev/null || { echo "publish failed"; exit 1; }
echo "--- dev run: $NAME ---"
"$GLEEAPH" prepared run "$NAME" --identity /Users/yota/.config/gleaph/identity/keys/dev.pem
echo "dev_run exit=$?"
