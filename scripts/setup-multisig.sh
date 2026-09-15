#!/usr/bin/env bash
# Turn a Stellar account into an M-of-N multisig, for use as the contracts' admin.
#
#   scripts/setup-multisig.sh <account-identity> <threshold> <signer-identity>...
#
# Every require_auth() on the account's address is then enforced by the network
# itself: no single key can act for it. The account's own key is removed as a
# signer at the end, so afterwards only <threshold> of the listed signers,
# together, can act for it. If those keys are lost, so is the account. Keep them
# on separate devices, held by separate people.
set -euo pipefail

NETWORK=${NETWORK:-testnet}
ACCOUNT=${1:?account identity}; THRESHOLD=${2:?threshold}; shift 2
[ $# -ge "$THRESHOLD" ] || { echo "need at least $THRESHOLD signers, got $#" >&2; exit 1; }
[ "$THRESHOLD" -ge 2 ] || { echo "a threshold below 2 is not a multisig" >&2; exit 1; }
if [ "$NETWORK" = "mainnet" ] && [ "${CONFIRM_MAINNET:-}" != "yes" ]; then
  echo "Refusing to change a mainnet account's signers without CONFIRM_MAINNET=yes." >&2
  exit 1
fi

for signer in "$@"; do
  stellar tx new set-options --source-account "$ACCOUNT" --network "$NETWORK" \
    --signer "$(stellar keys address "$signer")" --signer-weight 1 >/dev/null
done

# Thresholds and the master key go last, while the master key can still sign.
stellar tx new set-options --source-account "$ACCOUNT" --network "$NETWORK" \
  --low-threshold "$THRESHOLD" --med-threshold "$THRESHOLD" --high-threshold "$THRESHOLD" \
  --master-weight 0 >/dev/null

echo "$(stellar keys address "$ACCOUNT") now needs $THRESHOLD of $# signatures"
