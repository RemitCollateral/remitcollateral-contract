#!/usr/bin/env bash
# Hand the admin role of every contract to a multisig account.
#
#   SIGNERS="signer-1 signer-2" scripts/handover-to-multisig.sh <multisig> <contract-id>...
#
# The current admin (ADMIN, default rc-admin) proposes the multisig, and the
# multisig accepts with SIGNERS' signatures. Nothing changes until it accepts,
# so a mistyped address cannot lock the contracts out.
set -euo pipefail

NETWORK=${NETWORK:-testnet}
ADMIN=${ADMIN:-rc-admin}
MULTISIG=${1:?multisig identity or address}; shift
case "$MULTISIG" in G*) MS_ADDR=$MULTISIG ;; *) MS_ADDR=$(stellar keys address "$MULTISIG") ;; esac
cd "$(dirname "$0")/.."

for id in "$@"; do
  stellar contract invoke --id "$id" --source-account "$ADMIN" --network "$NETWORK" -- \
    propose_admin --admin "$(stellar keys address "$ADMIN")" --new_admin "$MS_ADDR" >/dev/null
  ./scripts/council-invoke.sh "$MS_ADDR" "$id" accept_admin --new_admin "$MS_ADDR" >/dev/null
  echo "$id: admin is now $MS_ADDR"
done
