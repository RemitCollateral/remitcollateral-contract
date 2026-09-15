#!/usr/bin/env bash
# Deploy the three RemitCollateral contracts and wire them together.
#
# Required environment:
#   USDC      contract id of the settlement asset's Stellar Asset Contract
#   PARTNER   address of the first off-ramp partner to authorize
#   VERIFIER  address of the repayment verifier that co-signs attestations
#   ORACLE    address allowed to publish reputation scores
#
# Optional environment:
#   NETWORK            stellar network name                  (default: testnet)
#   ADMIN              `stellar keys` identity that deploys the contracts and
#                      is their first admin; hand the role to a multisig
#                      afterwards with scripts/handover-to-multisig.sh (default: rc-admin)
#   SETTLEMENT         where forfeited collateral is sent     (default: the admin)
#   GRACE_PERIOD_SECS  grace after a missed installment      (default: 14 days)
#   TIMELOCK_SECS      delay before a scheduled upgrade or settlement change
#                      can run                               (default: 48 hours)
#
# Prints VAULT=, LEDGER= and ENGINE= lines that other scripts can source.
set -euo pipefail

NETWORK=${NETWORK:-testnet}
ADMIN=${ADMIN:-rc-admin}
: "${USDC:?set USDC to the settlement asset contract id}"
: "${PARTNER:?set PARTNER to the off-ramp partner address}"
: "${VERIFIER:?set VERIFIER to the repayment verifier address}"
: "${ORACLE:?set ORACLE to the reputation oracle address}"
GRACE_PERIOD_SECS=${GRACE_PERIOD_SECS:-1209600}
TIMELOCK_SECS=${TIMELOCK_SECS:-172800}

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
CONFIG="{\"base_ltv_bps\":15000,\"min_ltv_bps\":11000,\"safety_buffer_bps\":500,\"grace_period_secs\":$GRACE_PERIOD_SECS}"
VAULT=$(deploy rc_guarantor_vault \
  --admin "$ADMIN_ADDR" --usdc_token "$USDC" --settlement_address "$SETTLEMENT" \
  --timelock_secs "$TIMELOCK_SECS")
LEDGER=$(deploy rc_loan_ledger \
  --admin "$ADMIN_ADDR" --vault "$VAULT" --config "$CONFIG" --timelock_secs "$TIMELOCK_SECS")
ENGINE=$(deploy rc_liquidation_engine \
  --admin "$ADMIN_ADDR" --vault "$VAULT" --loan_ledger "$LEDGER" --timelock_secs "$TIMELOCK_SECS")

# Wiring that needs addresses which did not exist at construction time. It
# can be set only once, so run the whole script rather than stopping midway.
invoke "$VAULT"  set_loan_ledger        --admin "$ADMIN_ADDR" --ledger "$LEDGER"
invoke "$VAULT"  set_liquidation_engine --admin "$ADMIN_ADDR" --engine "$ENGINE"
invoke "$LEDGER" set_liquidation_engine --admin "$ADMIN_ADDR" --engine "$ENGINE"
invoke "$LEDGER" set_oracle             --admin "$ADMIN_ADDR" --oracle "$ORACLE"
invoke "$LEDGER" set_partner            --admin "$ADMIN_ADDR" --partner "$PARTNER" --authorized true
invoke "$LEDGER" set_verifier           --admin "$ADMIN_ADDR" --verifier "$VERIFIER" --authorized true

cat <<OUT
VAULT=$VAULT
LEDGER=$LEDGER
ENGINE=$ENGINE
OUT
