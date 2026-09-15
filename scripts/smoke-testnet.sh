#!/usr/bin/env bash
# End-to-end smoke test against contracts deployed by scripts/deploy.sh.
#
# Exercises the full happy path on a live network, plus the checks that
# matter most on-chain: a registered partner cannot attest for a loan it does
# not service, the permissionless crank moves an overdue loan into grace, and
# a code upgrade keeps the vault's address and balances.
#
# Required environment: VAULT, LEDGER, ENGINE, USDC (e.g. `source` deploy output)
# Identities (`stellar keys`): rc-admin, rc-guarantor, rc-partner, rc-oracle.
# The guarantor must hold at least 200 of the settlement asset.
set -euo pipefail

NETWORK=${NETWORK:-testnet}
: "${VAULT:?}" "${LEDGER:?}" "${ENGINE:?}" "${USDC:?}"
[ "$NETWORK" = "mainnet" ] && { echo "Smoke test is not for mainnet." >&2; exit 1; }

cd "$(dirname "$0")/.."
ADMIN=$(stellar keys address rc-admin)
GUAR=$(stellar keys address rc-guarantor)
PARTNER=$(stellar keys address rc-partner)
OTHER=$(stellar keys address rc-oracle)          # doubles as a second partner
BENEFICIARY=$(printf '07%.0s' {1..32})            # 32-byte off-chain handle

# Amounts use the asset's 7 decimals.
DEPOSIT=2000000000        # 200
PRINCIPAL=1000000000      # 100
INSTALLMENT=250000000     #  25
INTERVAL=20               # seconds, so an installment falls due during the test

call() {   # call <identity> <contract> <fn> [args...]   (submits a transaction)
  local who=$1 id=$2; shift 2
  stellar contract invoke --id "$id" --source-account "$who" --network "$NETWORK" -- "$@" 2>/dev/null
}
view() {   # view <contract> <fn> [args...]              (simulation only)
  local id=$1; shift
  stellar contract invoke --id "$id" --source-account rc-admin --network "$NETWORK" --send=no -- "$@" 2>/dev/null
}
num() { tr -d '"' ; }
pass() { printf '  \342\234\223 %s\n' "$1"; }
fail() { printf '  \342\234\227 %s\n' "$1"; exit 1; }
expect() { [ "$2" = "$3" ] && pass "$1 ($2)" || fail "$1: expected $3, got $2"; }

echo "== deposit and origination"
call rc-guarantor "$VAULT" deposit --guarantor "$GUAR" --amount $DEPOSIT >/dev/null
expect "vault balance after deposit" "$(view "$VAULT" get_balance --guarantor "$GUAR" | num)" $DEPOSIT

ID=$(call rc-guarantor "$LEDGER" originate --guarantor "$GUAR" --beneficiary "$BENEFICIARY" \
  --partner "$PARTNER" --principal_usd $PRINCIPAL --installment_count 4 --interval_secs $INTERVAL | num)
pass "loan originated (id $ID)"
expect "collateral locked at 150% LTV" "$(view "$VAULT" get_locked --guarantor "$GUAR" | num)" 1500000000

echo "== partner binding"
call rc-admin "$LEDGER" set_partner --admin "$ADMIN" --partner "$OTHER" --authorized true >/dev/null
if call rc-oracle "$LEDGER" attest_repayment --partner "$OTHER" --loan_id "$ID" --amount_usd $INSTALLMENT >/dev/null; then
  fail "a registered partner attested for a loan it does not service"
fi
pass "another registered partner cannot attest for this loan"

echo "== repayment releases collateral"
REL=$(call rc-partner "$LEDGER" attest_repayment --partner "$PARTNER" --loan_id "$ID" --amount_usd $INSTALLMENT | num)
expect "released for one installment (25% less 5% buffer)" "$REL" 356250000
expect "collateral still locked" "$(view "$VAULT" get_locked --guarantor "$GUAR" | num)" 1143750000

echo "== overdue loan enters grace through the permissionless crank"
for _ in $(seq 1 40); do
  [ "$(view "$LEDGER" is_overdue --loan_id "$ID")" = "true" ] && break
done
[ "$(view "$LEDGER" is_overdue --loan_id "$ID")" = "true" ] || fail "loan never became overdue"
call rc-oracle "$ENGINE" flag_overdue --loan_id "$ID" >/dev/null
view "$LEDGER" get_loan --loan_id "$ID" | grep -q '"Grace"' && pass "loan is in grace" || fail "loan not in grace"

echo "== full repayment during grace closes the loan"
call rc-partner "$LEDGER" attest_repayment --partner "$PARTNER" --loan_id "$ID" --amount_usd $((PRINCIPAL - INSTALLMENT)) >/dev/null
view "$LEDGER" get_loan --loan_id "$ID" | grep -q '"Repaid"' && pass "loan is repaid" || fail "loan not repaid"
expect "all collateral released" "$(view "$VAULT" get_locked --guarantor "$GUAR" | num)" 0

echo "== code upgrade keeps address and state"
if call rc-guarantor "$VAULT" upgrade --admin "$GUAR" --new_wasm_hash "$(printf '00%.0s' {1..32})" >/dev/null; then
  fail "a non-admin upgraded the vault"
fi
pass "a non-admin cannot upgrade"
cargo build --manifest-path contracts/Cargo.toml --target wasm32v1-none \
  --profile release-with-logs -p rc-guarantor-vault -q
NEW_WASM=contracts/target/wasm32v1-none/release-with-logs/rc_guarantor_vault.wasm
HASH=$(stellar contract upload --wasm "$NEW_WASM" --source-account rc-admin --network "$NETWORK" 2>/dev/null)
OLD_HASH=$(sha256sum contracts/target/wasm32v1-none/release/rc_guarantor_vault.wasm | cut -d' ' -f1)
[ "$HASH" != "$OLD_HASH" ] || fail "upgrade wasm is identical to the deployed one"
call rc-admin "$VAULT" upgrade --admin "$ADMIN" --new_wasm_hash "$HASH" >/dev/null
pass "vault upgraded to $HASH"
expect "vault balance survives the upgrade" "$(view "$VAULT" get_balance --guarantor "$GUAR" | num)" $DEPOSIT

echo "== withdrawal through the upgraded code"
call rc-guarantor "$VAULT" withdraw --guarantor "$GUAR" --amount $DEPOSIT >/dev/null
expect "vault emptied" "$(view "$VAULT" get_balance --guarantor "$GUAR" | num)" 0

echo "smoke test passed"
