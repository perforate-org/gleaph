#!/usr/bin/env bash
# Run one bounded Provision operator sweep. Run it from cron (or an operator scheduler) as the
# governance/operator identity.
#
#   provision-sweep.sh expire    mark grace-expired deployment suspensions; never deletes
#   provision-sweep.sh cleanup   resume cleanup of failed jobs still holding created canisters;
#                                exits non-zero while any swept job is still CleanupPending
#
# Environment:
#   GLEAPH_OPERATOR_IDENTITY  path to the operator PEM (required)
#   GLEAPH_PROVISION          Provision canister principal (required)
#   GLEAPH_NETWORK            network name (default: ic)
#   GLEAPH_OPERATOR_BIN       operator binary (default: gleaph-operator)
#   SWEEP_LIMIT               records per run, 1-100 (default: 100)
set -euo pipefail

sweep="${1:-}"
case "$sweep" in
  expire | cleanup) ;;
  *)
    echo "usage: $0 <expire|cleanup>" >&2
    exit 2
    ;;
esac

: "${GLEAPH_OPERATOR_IDENTITY:?set GLEAPH_OPERATOR_IDENTITY to the operator PEM}"
: "${GLEAPH_PROVISION:?set GLEAPH_PROVISION to the Provision canister principal}"
network="${GLEAPH_NETWORK:-ic}"
operator="${GLEAPH_OPERATOR_BIN:-gleaph-operator}"
limit="${SWEEP_LIMIT:-100}"

if ! [[ "$limit" =~ ^[0-9]+$ ]] || [ "$limit" -lt 1 ] || [ "$limit" -gt 100 ]; then
  echo "SWEEP_LIMIT must be an integer from 1 through 100" >&2
  exit 2
fi

printf 'provision-sweep %s: %s network=%s limit=%s\n' \
  "$sweep" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$network" "$limit" >&2

exec "$operator" \
  --provision "$GLEAPH_PROVISION" \
  -n "$network" \
  --identity "$GLEAPH_OPERATOR_IDENTITY" \
  deployment "$sweep" --limit "$limit"
