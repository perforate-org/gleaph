#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
FIXTURE_ROOT="$REPO_ROOT/crates/codegen/fixtures"
OUTPUT_ROOT=$(mktemp -d /tmp/gleaph-codegen-fixtures.XXXXXX)

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
tsc -p "$FIXTURE_ROOT/typescript-basic/tsconfig.json" --noEmit
tsc -p "$FIXTURE_ROOT/typescript-advanced/tsconfig.json" --noEmit
node --check "$FIXTURE_ROOT/typescript-basic/generated.js"
node --check "$FIXTURE_ROOT/typescript-advanced/generated.js"

cargo check --manifest-path "$FIXTURE_ROOT/rust-client-basic/Cargo.toml"
cargo check --manifest-path "$FIXTURE_ROOT/rust-canister-basic/Cargo.toml"

(cd "$FIXTURE_ROOT/motoko-basic" && mops check src/main.mo && mops check src/generated.mo)

echo "codegen fixtures are valid"
