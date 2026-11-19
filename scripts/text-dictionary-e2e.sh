#!/usr/bin/env bash
# Dictionary/analyzer lane: the five text_score_query tests that need MeCab/ko-dic fixtures.
# These are ignored in the fast lane because their dictionary upload dominates runtime.
# Set TEXT_DICT_BUDGET=1 to also run the Korean full-container cycle budget leg.
set -euo pipefail
cd "$(dirname "$0")/.."
threads="${TEXT_DICT_TEST_THREADS:-4}"
cargo test -p gleaph-pocket-ic-tests --test text_score_query -- \
  --ignored --test-threads="$threads" "$@"
