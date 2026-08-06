#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
FIXTURE_ROOT="$REPO_ROOT/crates/codegen/fixtures"

OUTPUT_ROOT=$(CDPATH='' mktemp -d /tmp/gleaph-codegen-fixtures.XXXXXX)

cleanup() {
  rm -rf "$OUTPUT_ROOT"
}
trap cleanup EXIT HUP INT TERM

check_generated() {
  manifest=$1
  target=$2
  expected=$3
  output="$OUTPUT_ROOT/$(basename -- "$expected")"

  format_args=
  case "$target" in
    rust|rust-canister) format_args="--format rust=never" ;;
  esac

  cargo run -p gleaph-codegen -- \
    --manifest "$FIXTURE_ROOT/$manifest" \
    --target "$target" \
    $format_args \
    --output "$output"

  comparison_expected="$FIXTURE_ROOT/$expected"

  if ! cmp -s "$output" "$comparison_expected"; then
    echo "generated output is out of sync: $expected" >&2
    diff -u "$comparison_expected" "$output" || true
    exit 1
  fi
}

cd "$REPO_ROOT"

check_generated typescript-basic/manifest.json typescript \
  typescript-basic/generated.ts
check_generated typescript-basic/manifest.json javascript \
  typescript-basic/generated.js
check_generated typescript-advanced/manifest.json typescript \
  typescript-advanced/generated.ts
check_generated typescript-advanced/manifest.json javascript \
  typescript-advanced/generated.js
check_generated typescript-basic/manifest.json rust \
  rust-client-basic/src/lib.rs
check_generated typescript-basic/manifest.json rust-canister \
  rust-canister-basic/src/lib.rs
check_generated typescript-basic/manifest.json motoko \
  motoko-basic/src/generated.mo

pnpm sdk:build

# Example app: the generated adapter must stay in sync, type-check, and pass its smoke test.
example_generated="$OUTPUT_ROOT/example-generated.ts"
cargo run -q -p gleaph-codegen -- \
  --manifest "$REPO_ROOT/examples/typescript-app/manifest.json" \
  --target typescript \
  --output "$example_generated"
cmp -s "$example_generated" "$REPO_ROOT/examples/typescript-app/generated.ts" || {
  echo "example generated output is out of sync: examples/typescript-app/generated.ts" >&2
  diff -u "$REPO_ROOT/examples/typescript-app/generated.ts" "$example_generated" || true
  exit 1
}
tsc -p "$REPO_ROOT/examples/typescript-app/tsconfig.json" --noEmit
(cd "$REPO_ROOT/examples/typescript-app" && node --experimental-strip-types scripts/smoke.mjs)

tsc -p "$FIXTURE_ROOT/typescript-basic/tsconfig.json" --noEmit
tsc -p "$FIXTURE_ROOT/typescript-advanced/tsconfig.json" --noEmit
node --check "$FIXTURE_ROOT/typescript-basic/generated.js"
node --check "$FIXTURE_ROOT/typescript-advanced/generated.js"

cargo check --manifest-path "$FIXTURE_ROOT/rust-client-basic/Cargo.toml"
cargo check --manifest-path "$FIXTURE_ROOT/rust-canister-basic/Cargo.toml"

(cd "$FIXTURE_ROOT/motoko-basic" && mops check src/main.mo && mops check src/generated.mo)

echo "codegen fixtures are valid"
