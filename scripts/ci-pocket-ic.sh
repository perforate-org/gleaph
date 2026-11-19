#!/usr/bin/env bash
# CI/dedicated-machine PocketIC gate. Runs every test binary independently so one hang cannot
# block the rest, with a per-binary timeout. Dictionary analyzer tests are ignored in the fast
# binary; set CI_INCLUDE_DICTIONARY=1 to run them after the main sweep.
#
# Do not run this on a laptop concurrently with other builds: PocketIC tests are sensitive to CPU
# contention and the wall time can double.
set -euo pipefail
cd "$(dirname "$0")/.."

out="${CI_POCKETIC_OUT:-target/audit/ci-pocket-ic}"
mkdir -p "$out"
summary="$out/summary.txt"
: > "$summary"
failures=0

for file in crates/pocket-ic-tests/tests/*.rs; do
  name=$(basename "$file" .rs)
  start=$(date +%s)
  if command -v timeout >/dev/null 2>&1; then
    timeout 900 cargo test -p gleaph-pocket-ic-tests --test "$name" > "$out/$name.log" 2>&1 || rc=$?
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout 900 cargo test -p gleaph-pocket-ic-tests --test "$name" > "$out/$name.log" 2>&1 || rc=$?
  else
    cargo test -p gleaph-pocket-ic-tests --test "$name" > "$out/$name.log" 2>&1 || rc=$?
  fi
  rc=${rc:-0}
  secs=$(( $(date +%s) - start ))
  printf '%s rc=%s secs=%s\n' "$name" "$rc" "$secs" | tee -a "$summary"
  if [ "$rc" -ne 0 ]; then
    failures=$((failures + 1))
  fi
done

if [ "${CI_INCLUDE_DICTIONARY:-0}" = "1" ]; then
  start=$(date +%s)
  TEXT_DICT_TEST_THREADS="${TEXT_DICT_TEST_THREADS:-4}" scripts/text-dictionary-e2e.sh > "$out/text-dictionary.log" 2>&1 || rc=$?
  rc=${rc:-0}
  secs=$(( $(date +%s) - start ))
  printf 'text-dictionary rc=%s secs=%s\n' "$rc" "$secs" | tee -a "$summary"
  if [ "$rc" -ne 0 ]; then
    failures=$((failures + 1))
  fi
fi

printf 'DONE failures=%s\n' "$failures" | tee -a "$summary"
exit "$failures"
