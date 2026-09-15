# RemitCollateral — Smart Contracts

> Crypto-collateralized lending for local beneficiaries who never touch crypto.

A diaspora member locks USDC as collateral on Stellar to guarantee a loan. The beneficiary — a relative or business contact back home — receives local currency via bank transfer or mobile money, repays through the same channel, and never needs a wallet or any blockchain literacy. The guarantor's collateral secures the loan; the beneficiary's repayment behavior determines whether that collateral is returned.

This repository holds the **Soroban smart contracts only** — the settlement layer that custodies collateral and records loan state. The orchestration backend, the off-ramp partner adapters and the guarantor dashboard live in their own repositories.

## The Contracts

| Crate | Contract | Responsibility |
|-------|----------|----------------|
| `rc-guarantor-vault` | `GuarantorVaultContract` | An isolated USDC vault per guarantor. Tracks total collateral against the portion locked behind active loans, and moves forfeited collateral to the settlement address. Never pools funds across guarantors. |
| `rc-loan-ledger` | `LoanLedgerContract` | Loan records, repayment schedules, reputation-adjusted LTV, and partner-attested repayments. Releases collateral proportionally as principal is repaid. |
| `rc-liquidation-engine` | `LiquidationEngineContract` | Permissionless cranks that move an overdue loan into grace and, once grace expires, seize the outstanding balance and return the excess. |

## How it works

1. **Collateral.** The guarantor deposits USDC into their own vault. Nothing is pooled.
2. **Origination.** The guarantor opens a loan and names the registered off-ramp partner that will disburse and collect it. The ledger computes the required LTV from the beneficiary's reputation — 150% by default, down to a 110% floor for a well-established relationship — and locks that multiple of the principal in the vault. The backend then instructs the off-ramp partner to disburse local currency.
3. **Repayment.** The beneficiary repays in local currency through their normal channel. The loan's own partner and an independent verifier co-sign each repayment attestation on-chain, and the ledger releases collateral in proportion to principal repaid, less a safety buffer held back until the loan closes. The schedule advances only by installments actually covered by principal repaid, however many attestations arrive.
4. **Closing.** The final attested installment returns all remaining collateral, buffer included.
5. **Default.** If an installment is missed, anyone can crank the loan into its grace period. A partial payment that leaves the loan behind does not end or restart grace. If grace expires unpaid, liquidation forfeits collateral equal to the **outstanding balance only** and returns the rest to the guarantor.

### What is deliberately not on-chain

The beneficiary has no wallet and never appears as an `Address`. They are identified by a 32-byte handle the backend derives from their phone number and the partner's KYC reference with a keyed hash (HMAC), so no personally identifying data reaches the ledger and the handle cannot be reversed by guessing phone numbers. Reputation scoring runs off-chain over remittance and repayment history; only the resulting score is published on-chain, by a registered oracle, because it is what sets the LTV.

### Trust boundary

Repayments happen in local currency through a licensed partner, so the chain cannot observe them directly. The protocol accepts a repayment only when two parties sign the same attestation: the partner servicing that loan, which collected the money, and a registered verifier — in practice the platform's backend, which checks each partner report before co-signing. It never accepts a repayment on the beneficiary's or the guarantor's word, from a different partner, or on either signature alone. A verifier can never also be a partner, so no single key holds both halves. Partners and verifiers are registered and revoked by the admin, revocation takes effect on the next invocation, and the admin can move an open loan to another registered partner when one is offboarded.

## Roles

| Role | Held by | May |
|------|---------|-----|
| **Guarantor** | Stellar wallet | Deposit, withdraw unlocked collateral, originate loans against their own vault |
| **Off-ramp partner** | Registered address | Co-sign repayment attestations on the loans it services |
| **Verifier** | Registered address, never also a partner | Co-sign every repayment attestation after checking it |
| **Oracle** | Registered address | Publish beneficiary reputation scores |
| **Admin** | Multisig account (2-of-3 on testnet) | Wire the contracts together once, register and revoke partners and verifiers, reassign a loan's partner, set the oracle, schedule upgrades and settlement changes behind the timelock, hand the admin role over |
| **Anyone** | — | Run the liquidation cranks; what they do is fixed by loan state |

## Stack

* **Language:** Rust (edition 2021), `#![no_std]`
* **SDK:** Soroban SDK v22
* **Build target:** `wasm32v1-none`
* **Settlement asset:** USDC via the Stellar Asset Contract

## Running it locally

### Prerequisites
* Rust (latest stable)
* WASM target — `rustup target add wasm32v1-none`
* Stellar CLI — `cargo install --locked stellar-cli`, or a prebuilt release

### Build

```bash
cd contracts
cargo build --target wasm32v1-none --release
```

Use `wasm32v1-none`, not `wasm32-unknown-unknown`. Recent Rust enables wasm features on the latter (`reference-types`) that the Soroban VM rejects, and the failure only shows up at deploy time.

Artifacts land in `contracts/target/wasm32v1-none/release/`:

```
rc_guarantor_vault.wasm
rc_loan_ledger.wasm
rc_liquidation_engine.wasm
```

### Test

```bash
cd contracts
cargo test                          # all suites
cargo test -p rc-loan-ledger        # a single crate
```

### Format & lint

```bash
cd contracts
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

## Deployment

Each contract takes its configuration through a constructor that runs inside its own deploy transaction, so there is no window between deployment and setup in which someone else could claim the admin role. The wiring between contracts that do not exist yet at construction time is set once, straight afterwards, and can never be changed except by a timelocked upgrade.

```bash
USDC=<settlement asset contract id> \
PARTNER=<first off-ramp partner address> \
VERIFIER=<repayment verifier address> \
ORACLE=<reputation oracle address> \
./scripts/deploy.sh      # NETWORK defaults to testnet, TIMELOCK_SECS to 48 hours
```

`deploy.sh` deploys the vault, the ledger (production defaults: 150% base LTV, 110% floor, 5% safety buffer, 14-day grace) and the engine, then runs the one-time wiring and registers the first partner and verifier. It refuses to target mainnet unless `CONFIRM_MAINNET=yes` is set. The deploying key is only the first admin: hand the role to a multisig straight away (see below). Set `SETTLEMENT` to that multisig as well, so forfeited collateral never lands in a single-key account.

`scripts/smoke-testnet.sh` then exercises a live deployment end to end, and passes against the current testnet code: deposit and origination; the partner alone, the verifier alone, and a different co-signed partner all refused; a co-signed repayment releasing collateral; an overdue loan cranked into grace; full repayment; the admin role moved to the council, with the old key refused; a single council signature refused; an upgrade that cannot run before its timelock, then runs once the council executes it, with the vault's balances intact; and a withdrawal through the upgraded code.

The co-signing helper, `scripts/attest.mjs`, shows the attestation flow: the verifier sends the transaction and the partner signs its own authorization entry. The backend's chain client (`src/chain` in `remitcollateral-backend`) does the same. Run `npm install` in `scripts/` before using the helper.

### Multisig admin

Stellar has multisig built into the protocol: an account can require several of its signers to authorize anything it does, and every `require_auth()` on its address is enforced by the network itself. The admin should be such an account, not a single key.

```bash
# Once: make an account a 2-of-3 multisig. Its own key is removed as a signer.
scripts/setup-multisig.sh rc-council 2 rc-signer-1 rc-signer-2 rc-signer-3

# Hand every contract's admin role to it (propose, then accept with two signatures).
SIGNERS="rc-signer-1 rc-signer-2" scripts/handover-to-multisig.sh rc-council $VAULT $LEDGER $ENGINE

# Any admin action afterwards collects the signatures on a single transaction.
SIGNERS="rc-signer-1 rc-signer-3" scripts/council-invoke.sh rc-council $LEDGER set_partner \
  --admin <council address> --partner <address> --authorized true
```

Anyone can check the arrangement on-chain: the council's signers and thresholds are public, and each contract's `get_admin` names the council.

### Timelock

A multisig limits who can act as admin; the timelock limits how fast. Upgrading any contract, and changing where forfeited collateral is sent, happen in two steps with a public delay between them:

1. `schedule_action(admin, action)` records an `Upgrade(wasm_hash)` or `SetSettlement(address)` and its earliest execution time.
2. `execute_action(admin)` runs it once that time has passed. `cancel_action(admin)` withdraws it before then.

The delay is fixed at deployment (48 hours by default) and readable from `get_timelock_secs`, and the pending change from `get_scheduled_action`, so guarantors and the admin's other signers see it coming. Upload new code with `stellar contract upload` first, and schedule the hash it prints.

### Testnet

Deployed to Stellar testnet with the production defaults, a 48-hour timelock, the admin role held by a 2-of-3 council, and forfeited collateral sent to that council, against a test asset issued for the purpose rather than Circle's USDC:

| Contract | Address |
|----------|---------|
| GuarantorVault | `CD6TYOKK74XIACIS423QJ2XW3Z646AMMHEAPAIZR2SWKFTRA5F3FL3QR` |
| LoanLedger | `CDCS5WKQPSQKA65HNDT6MS3OFS36VCZDMBJZ575REFDZCSABEUQFQSIL` |
| LiquidationEngine | `CC25FFHO6CFCBZPV5J7IJV4LJWDIN2X2LIELKBBBZBAYQV42CKXWC4NU` |
| Test USDC (SAC) | `CAWDARLC5JRSXG52Q6RWJJZ5YNEI3KJJOGVNQHEFAEQMESGPXRFCSHI4` |
| Admin council (2-of-3) | `GAFDAOJ6UE3VIJ43W6WEW6D6T3MVDE5PISA74AS7AEIJE4KURQAJ7VMT` |

### Storage lifetimes

Contract instances, vaults, loans, partner and verifier registrations and reputation scores are extended to about 120 days whenever they fall below about 90, on every call that uses them. Reading a loan renews it, so the engine's permissionless cranks double as a keep-alive for idle loans. Anything left entirely untouched for longer than that is archived rather than lost, and can be restored with a standard `RestoreFootprint` operation.

## Known limitations

These are the protocol's remaining trust assumptions and gaps, stated plainly. They are the things to resolve, or accept knowingly, before mainnet.

* **A repayment still rests on two parties' word.** No single key can release collateral any more, but the loan's partner and a verifier together can, immediately, with no money having moved. Collusion between a partner and the platform, or both keys stolen, defeats it. A release delay with a dispute window would add a chance to catch that.
* **Some admin powers are still instant.** The council can, without a timelock, register or revoke partners and verifiers, reassign a loan's partner, and change the oracle. None of these can move collateral to the council, but revoking every verifier would freeze repayments, and a new oracle can lower collateral requirements on new loans down to the floor.
* **The multisig is a deployment choice, not a contract rule.** The contracts accept any admin address. That the admin is a 2-of-3 council is enforced by the account's own configuration, which anyone can inspect on-chain but which the contracts do not check.
* **No events yet.** Scheduled changes and attestations are visible by reading contract state, not by subscribing to events, so monitoring the timelock means polling `get_scheduled_action`.
* **The oracle sets collateral requirements.** A compromised oracle can push every new loan's LTV down to the 110% floor. The floor bounds the damage but does not remove it.
* **Liquidation proceeds go to a platform-controlled address**, not through a market. Recovering the outstanding balance off-chain is outside the protocol.
* **One grace period for every loan**, fixed when the ledger is deployed (14 days on the testnet deployment). The backend's `GRACE_PERIOD_DAYS` must match it.
* **No way to cancel a loan whose disbursement fails.** Collateral is locked when a loan is originated, before the partner pays out. If the payout then fails, nothing can release it: the loan would eventually go overdue and be liquidated although no money reached the beneficiary. A cancellation co-signed by the partner and a verifier, allowed only before any repayment, would close this.
* **Not audited.** The contracts have unit tests and a testnet smoke test, not an independent security review. Do not hold real funds in them until they have had one.

## Roadmap

* **Delayed collateral release:** hold released collateral for a dispute window before it becomes withdrawable, so a colluding partner and verifier can be caught.
* **Events:** emit events for attestations, scheduled and executed admin actions, and defaults, so the timelock can be monitored rather than polled.
* **Timelock the remaining admin powers**, or split them across roles with narrower keys.
* **Cancel undisbursed loans:** a partner-and-verifier co-signed cancellation that releases the collateral of a loan whose payout failed.
* **Per-loan grace configuration:** let the grace period vary with loan size or beneficiary reputation instead of being global.
* **DEX-based liquidation:** settle forfeited collateral through a swap rather than transferring USDC to a platform-controlled address.
* **On-chain reputation derivation:** move part of the scoring on-chain so the LTV is reproducible without trusting the oracle.

## Contributing

Start with the [contributing guide](https://github.com/RemitCollateral/remitcollateral-docs/blob/main/CONTRIBUTING.md) in `remitcollateral-docs`. It covers how to pick an issue, which repository a change belongs in, how to build, test and deploy the contracts, the protocol rules every change must keep, and how to open a pull request. Report vulnerabilities privately, as the [security policy](https://github.com/RemitCollateral/remitcollateral-docs/blob/main/SECURITY.md) describes, not in a public issue.

## License
MIT
