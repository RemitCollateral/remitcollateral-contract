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
3. **Repayment.** The beneficiary repays in local currency through their normal channel. The loan's own partner attests to each repayment on-chain, and the ledger releases collateral in proportion to principal repaid, less a safety buffer held back until the loan closes. The schedule advances only by installments actually covered by principal repaid, however many attestations arrive.
4. **Closing.** The final attested installment returns all remaining collateral, buffer included.
5. **Default.** If an installment is missed, anyone can crank the loan into its grace period. A partial payment that leaves the loan behind does not end or restart grace. If grace expires unpaid, liquidation forfeits collateral equal to the **outstanding balance only** and returns the rest to the guarantor.

### What is deliberately not on-chain

The beneficiary has no wallet and never appears as an `Address`. They are identified by a 32-byte handle the backend derives from their phone number and the partner's KYC reference, so no personally identifying data reaches the ledger. Reputation scoring runs off-chain over remittance and repayment history; only the resulting score is published on-chain, by a registered oracle, because it is what sets the LTV.

### Trust boundary

Repayments happen in local currency through a licensed partner, so the chain cannot observe them directly. The protocol accepts a repayment only when the partner servicing that loan authorizes the attestation — never on the beneficiary's or the guarantor's word, and never from a different partner, even a registered one. Partner registration is admin-controlled and revocable, revocation takes effect on the next invocation, and the admin can move an open loan to another registered partner when one is offboarded.

## Roles

| Role | Held by | May |
|------|---------|-----|
| **Guarantor** | Stellar wallet | Deposit, withdraw unlocked collateral, originate loans against their own vault |
| **Off-ramp partner** | Registered address | Attest repayments on the loans it services |
| **Oracle** | Registered address | Publish beneficiary reputation scores |
| **Admin** | Stellar wallet | Wire the contracts together, register and revoke partners, reassign a loan's partner, set the oracle and settlement address, upgrade contract code, hand the admin role over |
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

Each contract takes its configuration through a constructor that runs inside its own deploy transaction, so there is no window between deployment and setup in which someone else could claim the admin role. What cannot go into a constructor is the wiring between contracts that do not exist yet, so deployment is: deploy in dependency order, then wire.

`scripts/deploy.sh` does both:

```bash
USDC=<settlement asset contract id> \
PARTNER=<first off-ramp partner address> \
ORACLE=<reputation oracle address> \
./scripts/deploy.sh            # NETWORK defaults to testnet; ADMIN to the rc-admin identity
```

It deploys the vault, the ledger (production defaults: 150% base LTV, 110% floor, 5% safety buffer, 14-day grace) and the engine, then runs the wiring. Run all of it: until the vault knows the ledger, origination cannot lock collateral, and until it knows the engine, liquidation cannot forfeit. It refuses to target mainnet unless `CONFIRM_MAINNET=yes` is set.

`scripts/smoke-testnet.sh` then exercises a live deployment end to end — deposit, origination, a registered partner refused on a loan it does not service, proportional release, an overdue loan cranked into grace, full repayment, a code upgrade that keeps the vault's balances, and withdrawal through the upgraded code.

### Testnet

Deployed to Stellar testnet, against a test asset issued for the purpose rather than Circle's USDC:

| Contract | Address |
|----------|---------|
| GuarantorVault | `CBIAT5DAKX3LNZOBDJWRAEVKFPCZ5FPS7MZAG32SHTAJTV4ZMRQXV7FF` |
| LoanLedger | `CCXZ7UFSF5ZEB5ZYOQOAWIZQ4FSIOZLE2T2PDYXZHR37Y775JHSBRSKI` |
| LiquidationEngine | `CDWFP5FQY65QVGEMP7LYVT4QZGCR5SYASNQVCOU5XNLSAQLJJYNRDXI2` |
| Test USDC (SAC) | `CAWDARLC5JRSXG52Q6RWJJZ5YNEI3KJJOGVNQHEFAEQMESGPXRFCSHI4` |

### Upgrades and the admin role

Every contract has `upgrade(admin, new_wasm_hash)`, which swaps its code while keeping its address and storage, so a bug found after launch can be fixed without migrating live loans or locked collateral. Upload the new wasm with `stellar contract upload` first, then call `upgrade` with the hash it prints.

The admin role is handed over in two steps: `propose_admin(admin, new_admin)`, then `accept_admin(new_admin)` from the new address. Nothing changes until the new admin accepts, so a mistyped address cannot lock the protocol out.

### Storage lifetimes

Contract instances, vaults, loans, partner registrations and reputation scores are extended to about 120 days whenever they fall below about 90, on every call that uses them. Reading a loan renews it, so the engine's permissionless cranks double as a keep-alive for idle loans. Anything left entirely untouched for longer than that is archived rather than lost, and can be restored with a standard `RestoreFootprint` operation.

## Known limitations

These are the protocol's current trust assumptions and gaps, stated plainly. They are the things to resolve, or accept knowingly, before mainnet.

* **A partner is still trusted for the loans it services.** Binding each loan to its partner stops one compromised key from touching the whole protocol, but on its own loans an attestation still releases collateral immediately. A compromised partner key can free collateral on every loan it services without any money having moved. A release delay with a dispute window, or corroboration from a second source, would close this.
* **The admin key is all-powerful.** It can upgrade every contract's code, which is equivalent to full control of all collateral. In production it should be a multisig with a timelock on upgrades, not a single key.
* **The oracle sets collateral requirements.** A compromised oracle can publish perfect scores and push every loan's LTV down to the 110% floor. The floor bounds the damage but does not remove it.
* **Liquidation proceeds go to a platform-controlled address**, not through a market. Recovery of the outstanding balance off-chain is outside the protocol.
* **One grace period for every loan**, fixed when the ledger is deployed.
* **Not audited.** The contracts have unit tests and a testnet smoke test, not an independent security review. Do not hold real funds in them until they have had one.

## Roadmap

* **Multi-partner attestation:** require corroborating attestations from more than one partner before a repayment counts.
* **Per-loan grace configuration:** let the grace period vary with loan size or beneficiary reputation instead of being global.
* **DEX-based liquidation:** settle forfeited collateral through a swap rather than transferring USDC to a platform-controlled address.
* **On-chain reputation derivation:** move part of the scoring on-chain so the LTV is reproducible without trusting the oracle.
* **Admin hardening:** move the admin role to a multisig and put upgrades behind a timelock.
* **Delayed collateral release:** hold released collateral for a dispute window before it becomes withdrawable.

## License
MIT
