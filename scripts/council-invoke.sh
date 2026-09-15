#!/usr/bin/env bash
# Invoke a contract as a multisig account, collecting several signatures on a
# single transaction.
#
#   SIGNERS="signer-1 signer-2" scripts/council-invoke.sh <account> <contract-id> <fn> [args...]
#
# <account> is the multisig's identity or G-address; it is the transaction
# source, so its authorization is carried by the envelope signatures, and the
# network accepts them only if they meet the account's threshold.
set -euo pipefail

NETWORK=${NETWORK:-testnet}
ACCOUNT=${1:?multisig account}; ID=${2:?contract id}; shift 2
: "${SIGNERS:?set SIGNERS to the identities that will sign}"
case "$ACCOUNT" in G*) ADDR=$ACCOUNT ;; *) ADDR=$(stellar keys address "$ACCOUNT") ;; esac

XDR=$(stellar contract invoke --id "$ID" --source-account "$ADDR" --network "$NETWORK" \
  --build-only -- "$@")
XDR=$(stellar tx simulate --source-account "$ADDR" --network "$NETWORK" <<<"$XDR")
for key in $SIGNERS; do
  XDR=$(stellar tx sign --sign-with-key "$key" --network "$NETWORK" <<<"$XDR")
done
stellar tx send --network "$NETWORK" <<<"$XDR"
