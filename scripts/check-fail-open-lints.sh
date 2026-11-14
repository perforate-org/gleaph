#!/usr/bin/env bash
set -euo pipefail

# The boundary modules listed below carry module-level
#   #![deny(clippy::wildcard_enum_match_arm, clippy::let_underscore_must_use)]
# attributes. This runs clippy over the packages that contain them; the deny
# attributes (not command-line flags) keep the gate scoped, so a new wildcard in
# an unrelated module does not fail CI while a new arm in a guarded contract does.
#
# Add a module here only after its wildcard arms and discarded Results have been
# reviewed, and put the matching deny attribute at the top of that module.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo clippy \
  -p gleaph-router \
  -p gleaph-provision \
  -p text-canister \
  -p ic-stable-lara \
  -p gleaph-account \
  -p gleaph-gql-planner \
  --lib
