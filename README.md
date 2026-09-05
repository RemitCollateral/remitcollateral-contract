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
2. **Origination.** The guarantor opens a loan. The ledger computes the required LTV from the beneficiary's reputation — 150% by default, down to a 110% floor for a well-established relationship — and locks that multiple of the principal in the vault. The backend then instructs the off-ramp partner to disburse local currency.
3. **Repayment.** The beneficiary repays in local currency through their normal channel. A registered off-ramp partner attests to each repayment on-chain, and the ledger releases collateral in proportion to principal repaid, less a safety buffer held back until the loan closes.
4. **Closing.** The final attested installment returns all remaining collateral, buffer included.
5. **Default.** If an installment is missed, anyone can crank the loan into its grace period. If grace expires unpaid, liquidation forfeits collateral equal to the **outstanding balance only** and returns the rest to the guarantor.

### What is deliberately not on-chain

The beneficiary has no wallet and never appears as an `Address`. They are identified by a 32-byte handle the backend derives from their phone number and the partner's KYC reference, so no personally identifying data reaches the ledger. Reputation scoring runs off-chain over remittance and repayment history; only the resulting score is published on-chain, by a registered oracle, because it is what sets the LTV.

### Trust boundary

Repayments happen in local currency through a licensed partner, so the chain cannot observe them directly. The protocol accepts a repayment only when a registered off-ramp partner authorizes the attestation — never on the beneficiary's or the guarantor's word. Partner registration is admin-controlled and revocable, and revocation takes effect on the next invocation.

## Roles

| Role | Held by | May |
|------|---------|-----|
| **Guarantor** | Stellar wallet | Deposit, withdraw unlocked collateral, originate loans against their own vault |
| **Off-ramp partner** | Registered address | Attest repayments |
| **Oracle** | Registered address | Publish beneficiary reputation scores |
| **Admin** | Stellar wallet | Wire the contracts together, register and revoke partners, set the oracle and settlement address |
| **Anyone** | — | Run the liquidation cranks; what they do is fixed by loan state |

## Stack

* **Language:** Rust (edition 2021), `#![no_std]`
* **SDK:** Soroban SDK v22
* **Build target:** `wasm32-unknown-unknown`
* **Settlement asset:** USDC via the Stellar Asset Contract

## Running it locally

### Prerequisites
* Rust (latest stable)
* WASM target — `rustup target add wasm32-unknown-unknown`
* Stellar CLI — `cargo install --locked stellar-cli`

### Build

```bash
cd contracts
cargo build --target wasm32-unknown-unknown --release
```

Artifacts land in `contracts/target/wasm32-unknown-unknown/release/`:

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

The three contracts reference each other by address, so they must be wired after deployment. Order matters:

1. Deploy all three contracts.
2. `GuarantorVault::initialize(admin, usdc_token, settlement_address)`
3. `LoanLedger::initialize(admin, vault, base_ltv_bps, min_ltv_bps, safety_buffer_bps, grace_period_secs)` — production defaults are `15000`, `11000`, `500`, and 14 days.
4. `LiquidationEngine::initialize(admin, vault, loan_ledger)`
5. `GuarantorVault::set_loan_ledger(admin, ledger)` — **without this, origination cannot lock collateral.**
6. `GuarantorVault::set_liquidation_engine(admin, engine)` — **without this, liquidation cannot forfeit.**
7. `LoanLedger::set_liquidation_engine(admin, engine)` — without this, no loan can leave `Active`.
8. `LoanLedger::set_oracle(admin, oracle)` and `LoanLedger::set_partner(admin, partner, true)` for each partner.

## Roadmap

* **Multi-partner attestation:** require corroborating attestations from more than one partner before a repayment counts.
* **Per-loan grace configuration:** let the grace period vary with loan size or beneficiary reputation instead of being global.
* **DEX-based liquidation:** settle forfeited collateral through a swap rather than transferring USDC to a platform-controlled address.
* **On-chain reputation derivation:** move part of the scoring on-chain so the LTV is reproducible without trusting the oracle.
* **TTL management:** extend the lifetime of long-lived vault and loan entries to avoid state archival.

## License
MIT
