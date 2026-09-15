#!/usr/bin/env bash
# End-to-end smoke test against contracts deployed by scripts/deploy.sh.
#
# Exercises the full loan lifecycle on a live network, and the checks that
# matter most on-chain: attestations need the loan's partner and a verifier
# together, admin power moves to a multisig that one key cannot operate, and a
# code upgrade must wait out the timelock yet keeps the vault's balances.
#
# Required environment: VAULT, LEDGER, ENGINE, USDC (e.g. `source` deploy output).
# Run it against a fresh deployment: its balance checks assume an empty vault.
# Deploy with a short TIMELOCK_SECS (the script waits it out), for example 30.
# Identities (`stellar keys`): rc-admin, rc-guarantor, rc-partner, rc-verifier,
# rc-oracle, and a 2-of-3 multisig rc-council with signers rc-signer-1..3
# (see scripts/setup-multisig.sh). The guarantor must hold at least 200 of the
# settlement asset, and `npm install` must have been run in scripts/.
set -euo pipefail

NETWORK=${NETWORK:-testnet}
: "${VAULT:?}" "${LEDGER:?}" "${ENGINE:?}" "${USDC:?}"
[ "$NETWORK" = "mainnet" ] && { echo "Smoke test is not for mainnet." >&2; exit 1; }

cd "$(dirname "$0")/.."
ADMIN=$(stellar keys address rc-admin)
GUAR=$(stellar keys address rc-guarantor)
PARTNER=$(stellar keys address rc-partner)
VERIFIER=$(stellar keys address rc-verifier)
OTHER=$(stellar keys address rc-oracle)          # doubles as a second partner
COUNCIL=$(stellar keys address rc-council)
BENEFICIARY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')  # fresh 32-byte handle per run

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
# `stellar contract invoke` signs authorization entries for any address whose
# key it holds, so it cannot show that one key is insufficient: it would quietly
# add the other signature. solo builds, simulates, and signs with exactly one
# key, so nothing else is signed.
solo() {   # solo <identity> <contract> <fn> [args...]
  local who=$1 id=$2 addr xdr; shift 2
  addr=$(stellar keys address "$who")
  xdr=$(stellar contract invoke --id "$id" --source-account "$addr" --network "$NETWORK" --build-only -- "$@") || return 1
  xdr=$(stellar tx simulate --source-account "$addr" --network "$NETWORK" <<<"$xdr") || return 1
  xdr=$(stellar tx sign --sign-with-key "$who" --network "$NETWORK" <<<"$xdr") || return 1
  stellar tx send --network "$NETWORK" <<<"$xdr"
}
secret_of() { stellar keys secret "$1" 2>/dev/null || stellar keys show "$1"; }
attest() { # attest <partner-identity> <verifier-identity> <loan> <amount>   (co-signed)
  PARTNER_SECRET=$(secret_of "$1") VERIFIER_SECRET=$(secret_of "$2") \
    node scripts/attest.mjs "$LEDGER" "$3" "$4"
}
council() {       # council "<signers>" <contract> <fn> [args...]; prints the reason if it fails
  local out
  if out=$(SIGNERS="$1" ./scripts/council-invoke.sh "$COUNCIL" "${@:2}" 2>&1); then return 0; fi
  printf '    %s\n' "$(printf '%s' "$out" | tail -c 300)" >&2; return 1
}
council_raw() { SIGNERS="$1" ./scripts/council-invoke.sh "$COUNCIL" "${@:2}"; }
try_call() {      # like call, but keeps the error output for refused_with
  local who=$1 id=$2; shift 2
  stellar contract invoke --id "$id" --source-account "$who" --network "$NETWORK" -- "$@"
}
num() { tr -d '"' ; }
pass() { printf '  \342\234\223 %s\n' "$1"; }
fail() { printf '  \342\234\227 %s\n' "$1"; exit 1; }
expect() { [ "$2" = "$3" ] && pass "$1 ($2)" || fail "$1: expected $3, got $2"; }
# refused_with <pattern> <command...>: the command must fail, and for the
# expected reason. A command that fails for any other reason, a network error
# say, is a test failure rather than a pass.
refused_with() {
  local pattern=$1 out; shift
  if out=$("$@" 2>&1); then return 1; fi
  printf '%s' "$out" | grep -qE "$pattern" && return 0
  printf '    failed, but not with %s: %s\n' "$pattern" "$(printf '%s' "$out" | tail -c 300)" >&2
  return 1
}

echo "== deposit and origination"
call rc-guarantor "$VAULT" deposit --guarantor "$GUAR" --amount $DEPOSIT >/dev/null
expect "vault balance after deposit" "$(view "$VAULT" get_balance --guarantor "$GUAR" | num)" $DEPOSIT
ID=$(call rc-guarantor "$LEDGER" originate --guarantor "$GUAR" --beneficiary "$BENEFICIARY" \
  --partner "$PARTNER" --principal_usd $PRINCIPAL --installment_count 4 --interval_secs $INTERVAL | num)
pass "loan originated (id $ID)"
expect "collateral locked at 150% LTV" "$(view "$VAULT" get_locked --guarantor "$GUAR" | num)" 1500000000

echo "== attestations need the loan's partner and a verifier together"
refused_with 'failed account authentication' solo rc-partner "$LEDGER" attest_repayment --partner "$PARTNER" --verifier "$VERIFIER" \
  --loan_id "$ID" --amount_usd $INSTALLMENT && pass "the partner's signature alone is refused" || fail "the partner alone attested"
refused_with 'failed account authentication' solo rc-verifier "$LEDGER" attest_repayment --partner "$PARTNER" --verifier "$VERIFIER" \
  --loan_id "$ID" --amount_usd $INSTALLMENT && pass "the verifier's signature alone is refused" || fail "the verifier alone attested"
call rc-admin "$LEDGER" set_partner --admin "$ADMIN" --partner "$OTHER" --authorized true >/dev/null
refused_with 'Error\(Contract, #3\)' attest rc-oracle rc-verifier "$ID" $INSTALLMENT \
  && pass "another partner, even co-signed, is refused on this loan" || fail "another partner attested"
expect "nothing released by the refused attempts" "$(view "$VAULT" get_locked --guarantor "$GUAR" | num)" 1500000000

echo "== co-signed repayment releases collateral"
expect "released for one installment (25% less 5% buffer)" "$(attest rc-partner rc-verifier "$ID" $INSTALLMENT)" 356250000
expect "collateral still locked" "$(view "$VAULT" get_locked --guarantor "$GUAR" | num)" 1143750000

echo "== overdue loan enters grace through the permissionless crank"
until [ "$(view "$LEDGER" is_overdue --loan_id "$ID")" = "true" ]; do sleep 2; done
call rc-oracle "$ENGINE" flag_overdue --loan_id "$ID" >/dev/null
view "$LEDGER" get_loan --loan_id "$ID" | grep -q '"Grace"' && pass "loan is in grace" || fail "loan not in grace"

echo "== full co-signed repayment during grace closes the loan"
attest rc-partner rc-verifier "$ID" $((PRINCIPAL - INSTALLMENT)) >/dev/null
view "$LEDGER" get_loan --loan_id "$ID" | grep -q '"Repaid"' && pass "loan is repaid" || fail "loan not repaid"
expect "all collateral released" "$(view "$VAULT" get_locked --guarantor "$GUAR" | num)" 0

echo "== admin moves to the 2-of-3 council"
# The admin checks straight after this verify the handover, so its output is not needed.
SIGNERS="rc-signer-1 rc-signer-2" ./scripts/handover-to-multisig.sh rc-council "$VAULT" "$LEDGER" "$ENGINE" >/dev/null 2>&1
expect "vault admin" "$(view "$VAULT" get_admin | num)" "$COUNCIL"
expect "ledger admin" "$(view "$LEDGER" get_admin | num)" "$COUNCIL"
expect "engine admin" "$(view "$ENGINE" get_admin | num)" "$COUNCIL"
refused_with 'Error\(Contract, #3\)' try_call rc-admin "$LEDGER" set_partner --admin "$ADMIN" --partner "$OTHER" --authorized false \
  && pass "the old single-key admin is refused" || fail "the old admin still acts"

echo "== a timelocked upgrade, run by the council"
cargo build --manifest-path contracts/Cargo.toml --target wasm32v1-none \
  --profile release-with-logs -p rc-guarantor-vault -q
NEW_WASM=contracts/target/wasm32v1-none/release-with-logs/rc_guarantor_vault.wasm
HASH=$(stellar contract upload --wasm "$NEW_WASM" --source-account rc-admin --network "$NETWORK" 2>/dev/null)
OLD_HASH=$(sha256sum contracts/target/wasm32v1-none/release/rc_guarantor_vault.wasm | cut -d' ' -f1)
[ "$HASH" != "$OLD_HASH" ] || fail "upgrade wasm is identical to the deployed one"
ACTION="{\"Upgrade\":\"$HASH\"}"
refused_with 'TxBadAuth' council_raw "rc-signer-1" "$VAULT" schedule_action --admin "$COUNCIL" --action "$ACTION" \
  && pass "one council signature is refused" || fail "one signature acted for the council"
council "rc-signer-1 rc-signer-3" "$VAULT" schedule_action --admin "$COUNCIL" --action "$ACTION" \
  && pass "two council signatures schedule the upgrade" || fail "council could not schedule"
refused_with 'Error\(Contract, #12\)' council_raw "rc-signer-1 rc-signer-2" "$VAULT" execute_action --admin "$COUNCIL" \
  && pass "the upgrade cannot run before its timelock" || fail "the upgrade ran early"
ETA=$(view "$VAULT" get_scheduled_action | python3 -c 'import sys,json; print(json.load(sys.stdin)["eta"])')
until [ "$(date +%s)" -gt $((ETA + 6)) ]; do sleep 2; done
council "rc-signer-2 rc-signer-3" "$VAULT" execute_action --admin "$COUNCIL" \
  && pass "after the timelock, the council executes it" || fail "council could not execute"
expect "vault balance survives the upgrade" "$(view "$VAULT" get_balance --guarantor "$GUAR" | num)" $DEPOSIT

echo "== withdrawal through the upgraded code"
call rc-guarantor "$VAULT" withdraw --guarantor "$GUAR" --amount $DEPOSIT >/dev/null
expect "vault emptied" "$(view "$VAULT" get_balance --guarantor "$GUAR" | num)" 0

echo "smoke test passed"
