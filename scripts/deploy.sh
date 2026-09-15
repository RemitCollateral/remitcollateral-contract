#!/usr/bin/env bash
# Deploy the three RemitCollateral contracts and wire them together.
#
# Required environment:
#   USDC     contract id of the settlement asset's Stellar Asset Contract
#   PARTNER  address of the first off-ramp partner to authorize
#   ORACLE   address allowed to publish reputation scores
#
# Optional environment:
#   NETWORK            stellar network name              (default: testnet)
#   ADMIN              `stellar keys` identity that deploys
#                      and administers the contracts     (default: rc-admin)
#   SETTLEMENT         where forfeited collateral is sent (default: the admin)
#   GRACE_PERIOD_SECS  grace after a missed installment  (default: 14 days)
#
# Prints VAULT=, LEDGER= and ENGINE= lines that can be sourced by other scripts.
set -euo pipefail

NETWORK=${NETWORK:-testnet}
ADMIN=${ADMIN:-rc-admin}
: "${USDC:?set USDC to the settlement asset contract id}"
: "${PARTNER:?set PARTNER to the off-ramp partner address}"
: "${ORACLE:?set ORACLE to the reputation oracle address}"
GRACE_PERIOD_SECS=${GRACE_PERIOD_SECS:-1209600}

if [ "$NETWORK" = "mainnet" ] && [ "${CONFIRM_MAINNET:-}" != "yes" ]; then
  echo "Refusing to deploy to mainnet without CONFIRM_MAINNET=yes." >&2
  exit 1
fi

cd "$(dirname "$0")/.."
ADMIN_ADDR=$(stellar keys address "$ADMIN")
SETTLEMENT=${SETTLEMENT:-$ADMIN_ADDR}
WASM=contracts/target/wasm32v1-none/release

# wasm32v1-none, not wasm32-unknown-unknown: recent Rust enables wasm features
# on the latter (reference-types) that the Soroban VM rejects at deploy time.
cargo build --manifest-path contracts/Cargo.toml --target wasm32v1-none --release

deploy() {
  local name=$1; shift
  stellar contract deploy --wasm "$WASM/$name.wasm" \
    --source-account "$ADMIN" --network "$NETWORK" -- "$@"
}
invoke() {
  local id=$1; shift
  stellar contract invoke --id "$id" \
    --source-account "$ADMIN" --network "$NETWORK" -- "$@" >/dev/null
}

# Each constructor runs inside its deploy transaction, so there is no window
# in which an uninitialized contract could be claimed by someone else.
VAULT=$(deploy rc_guarantor_vault \
  --admin "$ADMIN_ADDR" --usdc_token "$USDC" --settlement_address "$SETTLEMENT")
LEDGER=$(deploy rc_loan_ledger \
  --admin "$ADMIN_ADDR" --vault "$VAULT" \
  --base_ltv_bps 15000 --min_ltv_bps 11000 --safety_buffer_bps 500 \
  --grace_period_secs "$GRACE_PERIOD_SECS")
ENGINE=$(deploy rc_liquidation_engine \
  --admin "$ADMIN_ADDR" --vault "$VAULT" --loan_ledger "$LEDGER")

# Wiring that needs addresses which did not exist at construction time.
# Until the first two run, origination cannot lock and liquidation cannot
# forfeit, so run this whole script rather than stopping after the deploys.
invoke "$VAULT"  set_loan_ledger        --admin "$ADMIN_ADDR" --ledger "$LEDGER"
invoke "$VAULT"  set_liquidation_engine --admin "$ADMIN_ADDR" --engine "$ENGINE"
invoke "$LEDGER" set_liquidation_engine --admin "$ADMIN_ADDR" --engine "$ENGINE"
invoke "$LEDGER" set_oracle             --admin "$ADMIN_ADDR" --oracle "$ORACLE"
invoke "$LEDGER" set_partner            --admin "$ADMIN_ADDR" --partner "$PARTNER" --authorized true

cat <<OUT
VAULT=$VAULT
LEDGER=$LEDGER
ENGINE=$ENGINE
OUT
