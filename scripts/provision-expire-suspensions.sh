#!/usr/bin/env bash
# Mark grace-expired deployment suspensions. This is the only automatic grace step: it records
# GraceExpired and never deletes a canister. Run it from cron (or an operator scheduler) as the
# governance/operator identity.
#
# Environment:
#   GLEAPH_OPERATOR_IDENTITY  path to the operator PEM (required)
#   GLEAPH_PROVISION          Provision canister principal (required)
#   GLEAPH_NETWORK            network name (default: ic)
#   GLEAPH_OPERATOR_BIN       operator binary (default: gleaph-operator)
#   EXPIRE_LIMIT              records per run, 1-100 (default: 100)
set -euo pipefail

: "${GLEAPH_OPERATOR_IDENTITY:?set GLEAPH_OPERATOR_IDENTITY to the operator PEM}"
: "${GLEAPH_PROVISION:?set GLEAPH_PROVISION to the Provision canister principal}"
network="${GLEAPH_NETWORK:-ic}"
operator="${GLEAPH_OPERATOR_BIN:-gleaph-operator}"
limit="${EXPIRE_LIMIT:-100}"

if ! [[ "$limit" =~ ^[0-9]+$ ]] || [ "$limit" -lt 1 ] || [ "$limit" -gt 100 ]; then
  echo "EXPIRE_LIMIT must be an integer from 1 through 100" >&2
  exit 2
fi

printf 'provision-expire-suspensions: %s network=%s limit=%s\n' \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$network" "$limit" >&2

exec "$operator" \
  --provision "$GLEAPH_PROVISION" \
  -n "$network" \
  --identity "$GLEAPH_OPERATOR_IDENTITY" \
  deployment expire --limit "$limit"
