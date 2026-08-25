#!/usr/bin/env bash
#
# Broadcast an emergency pause (or lift one) across every GuildWorkman
# contract that carries the circuit breaker.
#
# The five contracts are separate deployments with separate storage, so this
# is N transactions, not one. They do not have to land in the same ledger and
# nothing breaks if they land out of order or if one fails — each contract's
# guard reads only its own record. That also means a partial sweep is a
# *valid* state, not a corrupt one: re-run the script to finish it.
#
# Scopes (bitmask, combine by adding):
#   1  INTAKE       new value / new obligations entering the system
#   2  SETTLEMENT   discretionary happy-path payouts (never recovery)
#   4  ATTESTATION  reputation writes
#   7  ALL_SCOPES
#
# A scope a given contract has no entrypoints for is a well-formed no-op, so
# the same mask goes to all five without special-casing.
#
# Fund-recovery paths are NEVER affected by any mask: escrow refunds and
# disputes, token transfer/transfer_from/burn of held balances, and every
# read-only view stay callable throughout.
#
# Usage:
#   SIGNER=my-key ./scripts/broadcast-pause.sh pause   7 21600 "INC-412 milestone accounting"
#   SIGNER=my-key ./scripts/broadcast-pause.sh unpause 1
#   SIGNER=my-key ./scripts/broadcast-pause.sh status
#
# Required environment:
#   SIGNER      stellar-cli key name or secret; must be a governance signer
#               on each contract it is used against
#   NETWORK     defaults to testnet
#   ESCROW, REPUTATION, LOYALTY_TOKEN, LOYALTY_EMISSIONS, SETTLEMENT_ROUTER
#               contract ids; any left unset is skipped with a warning
#
# Note each contract has its OWN governance signer set. If they differ, run
# the script once per key with only the matching contract ids exported.

set -euo pipefail

NETWORK="${NETWORK:-testnet}"
ACTION="${1:-}"

if [[ -z "${SIGNER:-}" ]]; then
  echo "error: SIGNER is not set (stellar-cli key name or secret)" >&2
  exit 2
fi

# Resolve the signer's public key once; `pause` takes the caller address as
# an explicit argument in addition to authorizing the transaction.
CALLER="$(stellar keys address "$SIGNER" 2>/dev/null || echo "${SIGNER_ADDRESS:-}")"
if [[ -z "$CALLER" ]]; then
  echo "error: could not resolve an address for SIGNER; set SIGNER_ADDRESS" >&2
  exit 2
fi

targets() {
  local name id
  for name in ESCROW REPUTATION LOYALTY_TOKEN LOYALTY_EMISSIONS SETTLEMENT_ROUTER; do
    id="${!name:-}"
    if [[ -z "$id" ]]; then
      echo "warning: $name is unset — skipping" >&2
      continue
    fi
    printf '%s\t%s\n' "$name" "$id"
  done
}

invoke() {
  local id="$1"; shift
  stellar contract invoke --id "$id" --source "$SIGNER" --network "$NETWORK" -- "$@"
}

case "$ACTION" in
  pause)
    SCOPES="${2:?usage: pause <scopes> <duration_secs> [reason]}"
    DURATION="${3:?usage: pause <scopes> <duration_secs> [reason]}"
    REASON="${4:-}"
    # MAX_PAUSE_DURATION is 7 days; the contract rejects anything longer, but
    # failing here is friendlier than five rejected transactions.
    if (( DURATION <= 0 || DURATION > 604800 )); then
      echo "error: duration must be 1..604800 seconds (7 days)" >&2
      exit 2
    fi
    if (( ${#REASON} > 64 )); then
      echo "error: reason must be at most 64 bytes" >&2
      exit 2
    fi
    while IFS=$'\t' read -r name id; do
      echo "==> pausing $name ($id) scopes=$SCOPES for ${DURATION}s"
      invoke "$id" pause --caller "$CALLER" --scopes "$SCOPES" \
        --duration_secs "$DURATION" --reason "$REASON" || \
        echo "warning: $name failed — other contracts are unaffected, re-run to retry" >&2
    done < <(targets)
    ;;

  unpause)
    SCOPES="${2:?usage: unpause <scopes>}"
    while IFS=$'\t' read -r name id; do
      echo "==> unpausing $name ($id) scopes=$SCOPES"
      invoke "$id" unpause --caller "$CALLER" --scopes "$SCOPES" || \
        echo "warning: $name failed (already open?) — re-run to retry" >&2
    done < <(targets)
    ;;

  status)
    while IFS=$'\t' read -r name id; do
      echo "==> $name ($id)"
      invoke "$id" get_pause_state || true
    done < <(targets)
    ;;

  *)
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
