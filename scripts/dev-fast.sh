#!/usr/bin/env bash
# Development loop: formatting, unit suites for the changed surface, and a small PocketIC smoke
# set. Target: a few minutes on an idle machine. The full 81-binary PocketIC suite belongs to CI
# (scripts/ci-pocket-ic.sh), not to every edit.
#
# Environment:
#   DEV_FAST_LIBS_ONLY=1    skip PocketIC entirely (seconds)
#   DEV_FAST_SKIP_POCKETIC=1  alias for the same skip
#   DEV_FAST_POCKETIC=...   replace the default --test list (space-separated names)
set -euo pipefail
cd "$(dirname "$0")/.."

fmt_start=$(date +%s)
cargo fmt --all --check
printf 'fmt: %ss\n' "$(( $(date +%s) - fmt_start ))"

libs_start=$(date +%s)
cargo test --quiet \
  -p gleaph-graph-kernel \
  -p gleaph-router \
  -p gleaph-graph \
  -p gleaph-provision \
  -p gleaph-account \
  -p gleaph-ingress-client \
  -p gleaph-operator \
  --lib
printf 'libs: %ss\n' "$(( $(date +%s) - libs_start ))"

if [ "${DEV_FAST_LIBS_ONLY:-0}" = "1" ] || [ "${DEV_FAST_SKIP_POCKETIC:-0}" = "1" ]; then
  exit 0
fi

pocketic_start=$(date +%s)
if [ -n "${DEV_FAST_POCKETIC:-}" ]; then
  # shellcheck disable=SC2086
  tests=$(printf -- '--test %s ' $DEV_FAST_POCKETIC)
else
  tests="--test smoke --test general_router_route --test adr0035_provision_callable_endpoints --test adr0068_router_issuance"
fi
# shellcheck disable=SC2086
cargo test --quiet -p gleaph-pocket-ic-tests $tests
printf 'pocketic: %ss\n' "$(( $(date +%s) - pocketic_start ))"
