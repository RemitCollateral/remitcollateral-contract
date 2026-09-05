# RemitCollateral — Contract Architecture

> The settlement layer for crypto-collateralized lending to beneficiaries who never touch crypto.

This document describes the three Soroban contracts in this repository. The orchestration backend, the off-ramp partner adapters, the reputation engine and the guarantor dashboard are documented in their own repositories; what follows is only what the chain enforces.

---

## Table of Contents

1. [Scope: What the Chain Enforces](#1-scope-what-the-chain-enforces)
2. [System Overview](#2-system-overview)
3. [Actors & Roles](#3-actors--roles)
4. [Identity & Privacy](#4-identity--privacy)
5. [Contract Specifications](#5-contract-specifications)
6. [Cross-Contract Interfaces](#6-cross-contract-interfaces)
7. [Authorization Model](#7-authorization-model)
8. [Collateral Mathematics](#8-collateral-mathematics)
9. [Storage Model](#9-storage-model)
10. [Lifecycle Sequences](#10-lifecycle-sequences)
11. [Invariants](#11-invariants)
12. [Testing Strategy](#12-testing-strategy)
13. [Deployment & Wire-Up](#13-deployment--wire-up)
14. [Trust Model](#14-trust-model)
15. [Known Limitations](#15-known-limitations)

---

## 1. Scope: What the Chain Enforces

The protocol spans two worlds. Money moves to and from the beneficiary in local currency through a licensed off-ramp partner; the chain never sees it. What the chain does hold is the guarantor's USDC and the authoritative record of what that collateral is backing.

**On-chain, and therefore enforced:**

- Custody of guarantor collateral, and the rule that locked collateral cannot be withdrawn.
- The loan record: principal, LTV applied, schedule, repayment progress, status.
- That only a registered off-ramp partner can assert a repayment happened.
- Proportional collateral release as principal is repaid, less a withheld safety buffer.
- That a default seizes no more than the outstanding balance, and returns the rest.

**Off-chain, and therefore trusted:**

- The disbursement and collection of local currency.
- KYC on the beneficiary — the chain stores only an opaque handle.
- The reputation score itself; the chain stores the published result and derives the LTV from it.

The dividing line is deliberate: the chain arbitrates the guarantor's money, because that is the asset it can actually hold.

---

## 2. System Overview

```
    Guarantor                                              Off-Ramp Partner
    (Stellar wallet)                                       (registered address)
         │                                                          │
         │ deposit / withdraw / originate                           │ attest_repayment
         ▼                                                          ▼
 ┌─────────────────────┐   lock / release    ┌──────────────────────────────┐
 │  GuarantorVault     │◀────────────────────│  LoanLedger                  │
 │                     │                     │                              │
 │ • per-guarantor     │                     │ • loan records + schedule    │
 │   USDC custody      │                     │ • reputation → LTV           │
 │ • balance vs locked │                     │ • attested repayments        │
 │ • forfeit → settle  │                     │ • status machine             │
 └─────────┬───────────┘                     └──────────────┬───────────────┘
           │  forfeit / release                             │ views +
           │                                                │ mark_grace / mark_defaulted
           │              ┌─────────────────────────────────┴──┐
           └──────────────│  LiquidationEngine                  │
                          │                                     │
                          │ • permissionless cranks             │
                          │ • seizes outstanding, returns rest  │
                          └─────────────────────────────────────┘
                                            ▲
                                            │ anyone may call
                            ┌───────────────┴───────────────┐
                            │  Oracle ──▶ set_reputation     │
                            │  Admin  ──▶ wiring, partners   │
                            └────────────────────────────────┘

                    USDC (Stellar Asset Contract) underlies all custody
```

The vault is the only contract that holds funds. The ledger owns the loan state machine but never touches USDC. The engine owns neither — it reads the ledger, instructs the vault, and closes the loan.

---

## 3. Actors & Roles

| Actor | Identified by | May |
|-------|---------------|-----|
| **Guarantor** | Stellar wallet address | Deposit USDC, withdraw unlocked collateral, originate a loan against their own vault |
| **Beneficiary** | 32-byte handle, never an address | Nothing on-chain. They have no wallet and cannot invoke anything. |
| **Off-ramp partner** | Registered address on the ledger | Attest that a repayment was collected |
| **Oracle** | Registered address on the ledger | Publish a beneficiary's reputation score |
| **Admin** | Stellar wallet address | Wire the three contracts together, register and revoke partners, set the oracle, change the settlement address |
| **Anyone** | — | Call `flag_overdue`, `liquidate` and `poke` on the engine |

The admin cannot move a guarantor's collateral. It can only change *where forfeited* collateral settles and *who* is trusted to attest — meaningful power, but not custody.

---

## 4. Identity & Privacy

The beneficiary is the reason this protocol exists and the one participant who never appears on it. They are represented throughout by a `BytesN<32>` handle that the backend derives from their phone number and the off-ramp partner's KYC reference.

This matters for three reasons:

1. **No PII on a public ledger.** Phone numbers and KYC references stay in the backend database. The chain sees an opaque 32 bytes.
2. **No wallet required.** Nothing in the contracts asks the beneficiary to sign, hold, or receive anything. Their side of the loan is entirely local currency.
3. **Stable across loans.** The same handle links a beneficiary's reputation and their open-loan record across repeat borrowing, without ever resolving to a person on-chain.

The handle is only as private as its derivation. A predictable input space — a phone number alone — is enumerable, so the backend is expected to include the partner's KYC reference in the hash.

---

## 5. Contract Specifications

### 5.1 `rc-guarantor-vault` — GuarantorVault

Isolated USDC custody. Deliberately passive: it holds funds and tracks how much is spoken for, but does not know what a loan is.

**Storage**

`Vault { guarantor, collateral_balance, locked_amount }`, one per guarantor. `collateral_balance - locked_amount` is what may be withdrawn.

**Entry points**

| Function | Caller | Effect |
|----------|--------|--------|
| `initialize(admin, usdc_token, settlement_address)` | once | Sets custody configuration |
| `set_loan_ledger(admin, ledger)` | admin | Names the only contract that may lock |
| `set_liquidation_engine(admin, engine)` | admin | Names the only contract that may forfeit |
| `set_settlement_address(admin, addr)` | admin | Changes where forfeited collateral goes |
| `deposit(guarantor, amount)` | guarantor | Transfers USDC in, credits the vault |
| `withdraw(guarantor, amount)` | guarantor | Only up to `balance - locked` |
| `lock_collateral(caller, guarantor, amount)` | ledger | Reserves collateral against a new loan |
| `release_collateral(caller, guarantor, amount)` | ledger **or** engine | Returns collateral to available |
| `forfeit_collateral(caller, guarantor, amount)` | engine | Debits balance **and** locked, sends USDC to settlement |

**Isolation.** Vaults are keyed by guarantor address and never aggregated. One guarantor's default cannot reach another's collateral, and the test suite asserts this directly.

---

### 5.2 `rc-loan-ledger` — LoanLedger

The loan state machine and the reputation-to-LTV rule. Holds no USDC; it instructs the vault.

**Status machine** — `LoanStatus { Active, Grace, Repaid, Defaulted }`

```
   originate()
   (locks principal × LTV)
        │
        ▼
    [Active] ──── attest_repayment() ────▶ [Active]      (schedule advances)
        │                                      │
        │  mark_grace()                        │ final installment
        │  (engine, past next_due)             ▼
        ▼                                  [Repaid]      (all collateral returned)
     [Grace] ──── attest_repayment() ────▶ [Active]      (grace cleared)
        │
        │  mark_defaulted()
        │  (engine, past grace_expires_at)
        ▼
   [Defaulted]
```

`Repaid` and `Defaulted` are terminal. A payment during grace always restores `Active` — the loan is not penalised for having been late once it is caught up.

**Entry points**

| Function | Caller | Guards |
|----------|--------|--------|
| `initialize(admin, vault, base_ltv_bps, min_ltv_bps, safety_buffer_bps, grace_period_secs)` | once | `min ≤ base`, buffer < 100%, grace > 0 |
| `set_oracle` / `set_liquidation_engine` / `set_partner` | admin | Wiring and partner registration |
| `set_reputation(oracle, beneficiary, score_bps)` | oracle | Score ≤ 10000 |
| `required_ltv_bps(beneficiary)` | view | Current LTV for this beneficiary |
| `originate(guarantor, beneficiary, principal, installment_count, interval_secs)` | guarantor | Positive principal, non-zero schedule, **no open loan for this pair**. Locks `principal × LTV` in the vault before returning the loan id. |
| `attest_repayment(partner, loan_id, amount)` | registered partner | Loan `Active` or `Grace`; total repaid may not exceed principal. Releases the collateral earned and advances the schedule. Returns the amount released. |
| `mark_grace(caller, loan_id)` | engine | Loan `Active` and past `next_due` |
| `mark_defaulted(caller, loan_id)` | engine | Loan `Grace` and past `grace_expires_at` |

**Views for the engine** — `is_overdue`, `is_grace_expired`, `loan_guarantor`, `loan_outstanding`, `loan_collateral_remaining`. All primitives, which is what lets the engine avoid sharing struct definitions.

**One open loan per pair.** `OpenLoan(guarantor, beneficiary)` is set at origination and removed when the loan reaches `Repaid` or `Defaulted`, enforcing the architectural rule that a guarantor-beneficiary relationship carries one loan at a time.

---

### 5.3 `rc-liquidation-engine` — LiquidationEngine

Default detection and settlement. Holds no funds and no loan state.

**Entry points**

| Function | Caller | Effect |
|----------|--------|--------|
| `initialize(admin, vault, loan_ledger)` | once | Wiring |
| `flag_overdue(loan_id)` | **anyone** | Asserts the loan is overdue, then moves it into grace |
| `liquidate(loan_id)` | **anyone** | Asserts grace has expired, settles collateral, closes the loan. Returns the amount forfeited. |
| `poke(loan_id)` | **anyone** | Whichever of the two the loan is due for; returns `false` if neither |

**Why permissionless.** Every branch is decided by loan state and the ledger clock, so a caller has nothing to influence — they can only make the protocol do what it was already owed. The alternative, a privileged keeper, means a missed installment goes unrecorded whenever the platform's job runner is down. Anyone can pay the fee to keep the ledger honest.

**Settlement is proportional, not punitive.** Liquidation forfeits `min(outstanding, remaining_locked)` and returns the remainder to the guarantor. A guarantor who over-collateralised, or who is most of the way through repayment, does not lose the excess.

---

## 6. Cross-Contract Interfaces

Contracts reference each other by address supplied at wiring time and call through `#[contractclient]` traits that declare only what the caller actually uses:

| Caller | Callee | Declared functions |
|--------|--------|--------------------|
| `rc-loan-ledger` | vault | `lock_collateral`, `release_collateral` |
| `rc-liquidation-engine` | ledger | `is_overdue`, `is_grace_expired`, `loan_guarantor`, `loan_outstanding`, `loan_collateral_remaining`, `mark_grace`, `mark_defaulted` |
| `rc-liquidation-engine` | vault | `release_collateral`, `forfeit_collateral` |

Every declared function takes and returns primitives or `Address`. That is a deliberate constraint: it means no contract needs to share a struct definition with another, so none of them link against another's crate and there is no risk of a dependency's `#[contractimpl]` exports leaking into the wrong wasm. The cost is a slightly chattier engine, which reads five small views instead of one loan struct.

Cross-contract calls pass `env.current_contract_address()` as the `caller` argument, and the callee checks it against its stored wiring.

---

## 7. Authorization Model

Four checks stack on a privileged call:

```
  ┌────────────────────────────────────────────────────┐
  │ require_auth()      — is the actor really them?    │
  ├────────────────────────────────────────────────────┤
  │ role check          — admin / oracle / partner /   │
  │                       ledger / engine?             │
  ├────────────────────────────────────────────────────┤
  │ ownership check     — is this their own vault      │
  │                       or their own loan?           │
  ├────────────────────────────────────────────────────┤
  │ state guard         — is the loan in a status      │
  │                       and at a time that allows    │
  │                       this transition?             │
  └───────────────────────┬────────────────────────────┘
                          ▼
                effect + storage write
```

Failures are raised through `#[contracterror]` enums with `panic_with_error!`, so clients receive a typed, matchable error code rather than a generic invocation failure. Each contract defines its own `Error` enum with stable discriminants.

Three separations are worth stating explicitly, because they are the ones that carry the protocol's integrity:

- **The ledger locks; only the engine forfeits.** The contract that decides a loan has defaulted is not the contract that can seize funds without that decision.
- **A guarantor cannot attest their own beneficiary's repayment.** Nor can the beneficiary, who has no address. Only a registered partner.
- **The admin cannot withdraw collateral.** It can rewire and revoke, but the withdrawal path is gated on the guarantor's own signature and the locked-amount check.

---

## 8. Collateral Mathematics

All ratios are basis points; `BPS = 10_000`.

**Required LTV from reputation.** A perfect score earns the floor exactly; no score pays the base rate:

```
span      = base_ltv_bps - min_ltv_bps
reduction = span × score_bps / BPS
ltv_bps   = base_ltv_bps - reduction
```

With the production defaults (`base = 15000`, `min = 11000`), a score of 0 gives 150%, 5000 gives 130%, and 10000 gives 110%.

**Collateral locked at origination:**

```
collateral_locked = principal_usd × ltv_bps / BPS
```

**Proportional release as repayments land.** Collateral is earned back in proportion to principal repaid, less the safety buffer withheld until the loan closes:

```
earned      = collateral_locked × total_repaid / principal
releasable  = earned × (BPS - safety_buffer_bps) / BPS
release_now = releasable - collateral_released
```

On the final installment `releasable` is set to `collateral_locked` outright, so the buffer is returned along with the last tranche.

**Liquidation:**

```
remaining = collateral_locked - collateral_released
forfeited = min(outstanding, remaining)
returned  = remaining - forfeited
```

Note the interaction between the buffer and over-collateralisation: because collateral is locked at 110–150% of principal but released only in proportion to principal repaid, `remaining` stays above `outstanding` throughout a normally-progressing loan. The `min` is therefore defensive rather than routine — it matters only if the configuration is changed such that LTV approaches or drops below 100%.

All arithmetic is integer and truncating, and the release schedule truncates in the protocol's favour: a guarantor may be owed a few stroops more than they receive on any given installment, recovered in full when the loan closes and the whole locked amount is released.

---

## 9. Storage Model

**Instance storage** — singleton configuration, sharing the contract's TTL: wired addresses (`Vault`, `LoanLedger`, `LiquidationEngine`), `Admin`, `Oracle`, `UsdcToken`, `SettlementAddress`, the ledger `Config`, and `LoanCount`.

**Persistent storage** — per-entity records that grow unbounded:

| Contract | Persistent keys |
|----------|-----------------|
| `rc-guarantor-vault` | `Vault(Address)` |
| `rc-loan-ledger` | `Loan(u64)`, `Partner(Address)`, `Reputation(BytesN<32>)`, `OpenLoan(Address, BytesN<32>)` |
| `rc-liquidation-engine` | none — the engine is stateless beyond its wiring |

Loan ids are assigned from a monotonic counter, start at 1, and are never reused. The composite `OpenLoan(guarantor, beneficiary)` key enforces the one-loan-per-pair rule without maintaining a list.

No contract calls `extend_ttl`. Long-lived entries — a vault, a reputation score, an open loan — are subject to standard Soroban state archival and would need restoring if they expire.

---

## 10. Lifecycle Sequences

### 10.1 Deposit → Origination → Disbursement

```
Guarantor            LoanLedger              GuarantorVault           USDC SAC
    │                     │                        │                     │
    │  deposit(amount)    │                        │                     │
    ├──────────────────────────────────────────────▶│  transfer in       │
    │                     │                        ├────────────────────▶│
    │                     │                        │  balance += amount  │
    │                     │                        │                     │
    │  originate(beneficiary, principal, schedule) │                     │
    ├────────────────────▶│                        │                     │
    │                     │  required_ltv_bps()    │                     │
    │                     │  from reputation       │                     │
    │                     │  collateral =          │                     │
    │                     │    principal × ltv     │                     │
    │                     │  lock_collateral(self, guarantor, collateral)│
    │                     ├───────────────────────▶│                     │
    │                     │                        │  locked += amount   │
    │                     │                        │  (fails if not      │
    │                     │                        │   available)        │
    │                     │  create Loan{Active}   │                     │
    │◀────────────────────┤  loan_id               │                     │
    │                     │                        │                     │
    │      ┄┄ backend instructs the off-ramp partner to disburse ┄┄      │
```

### 10.2 Repayment → Proportional Release

```
Beneficiary      Off-Ramp Partner        LoanLedger            GuarantorVault
    │                   │                     │                       │
    │ repays locally    │                     │                       │
    ├──────────────────▶│                     │                       │
    │                   │ attest_repayment(loan_id, amount)           │
    │                   ├────────────────────▶│                       │
    │                   │                     │ partner registered?   │
    │                   │                     │ status Active|Grace?  │
    │                   │                     │ total ≤ principal?    │
    │                   │                     │                       │
    │                   │                     │ releasable = earned × │
    │                   │                     │   (1 - buffer)        │
    │                   │                     │ release_collateral()  │
    │                   │                     ├──────────────────────▶│
    │                   │                     │                       │ locked -= amt
    │                   │                     │ status = Active       │
    │                   │                     │ next_due += interval  │
    │                   │◀────────────────────┤ released              │
    │                   │                     │                       │
    │       ┄┄ final installment: status = Repaid, all collateral returned ┄┄
```

### 10.3 Missed Installment → Grace → Liquidation

```
Anyone        LiquidationEngine         LoanLedger           GuarantorVault      Settlement
  │                  │                       │                     │                │
  │ flag_overdue(id) │                       │                     │                │
  ├─────────────────▶│  is_overdue()?        │                     │                │
  │                  ├──────────────────────▶│                     │                │
  │                  │  mark_grace(self, id) │                     │                │
  │                  ├──────────────────────▶│ status = Grace      │                │
  │                  │                       │ grace_expires_at =  │                │
  │                  │                       │   now + grace       │                │
  │                                                                                  │
  │       ┄┄ a repayment during grace restores Active and cancels this ┄┄            │
  │                                                                                  │
  │ liquidate(id)    │                       │                     │                │
  ├─────────────────▶│  is_grace_expired()?  │                     │                │
  │                  ├──────────────────────▶│                     │                │
  │                  │  outstanding, remaining, guarantor          │                │
  │                  ├──────────────────────▶│                     │                │
  │                  │  forfeited = min(outstanding, remaining)    │                │
  │                  │  forfeit_collateral(self, guarantor, forfeited)               │
  │                  ├────────────────────────────────────────────▶│  USDC transfer │
  │                  │                       │                     ├───────────────▶│
  │                  │  release_collateral(self, guarantor, returned)                │
  │                  ├────────────────────────────────────────────▶│                │
  │                  │  mark_defaulted(self, id)                   │                │
  │                  ├──────────────────────▶│ status = Defaulted  │                │
  │◀─────────────────┤  forfeited                                  │                │
```

---

## 11. Invariants

These are the properties the contracts hold and the test suites assert:

1. **Vault isolation.** A guarantor's collateral is reachable only through operations naming that guarantor. No operation aggregates across vaults.
2. **Locked collateral is unwithdrawable.** `withdraw` is bounded by `collateral_balance - locked_amount` at all times.
3. **Custody conservation.** `collateral_balance` changes only on deposit, withdrawal and forfeiture. Locking and releasing move the boundary between locked and available without changing the total.
4. **Only a registered partner can advance repayment.** Neither the guarantor, the admin, nor anyone else can record a repayment; revocation takes effect immediately.
5. **Repayment never exceeds principal.** Attestations totalling more than `principal_usd` are refused.
6. **Terminal states are terminal.** A `Repaid` or `Defaulted` loan accepts no further repayments and no further transitions.
7. **Grace cannot be skipped.** `Active` cannot go directly to `Defaulted`; the loan must pass through `Grace`, and grace must actually expire.
8. **Liquidation is bounded by the debt.** Forfeiture never exceeds `min(outstanding, remaining_locked)`; the excess is returned to the guarantor.
9. **One open loan per guarantor-beneficiary pair.** Enforced by the `OpenLoan` key, cleared only on a terminal status.
10. **LTV respects the floor.** `required_ltv_bps` never returns below `min_ltv_bps`, however high the published score.

---

## 12. Testing Strategy

Each crate carries a `src/test.rs` compiled under `#[cfg(test)]`, run against an in-memory `Env`. Fourteen tests across the three crates.

| Suite | Scenarios |
|-------|-----------|
| `rc-guarantor-vault` | deposit/withdraw bounds; **vault isolation** across two guarantors; locked collateral cannot be withdrawn; forfeiture debits both counters and reaches the settlement address; ledger-vs-engine authorization separation |
| `rc-loan-ledger` | LTV across the reputation range including the floor; origination locks the right multiple and refuses a second open loan for the pair; proportional release across four installments including buffer return on close; partner authorization and revocation; grace entry, payment-clears-grace, and refusal to skip grace; default closes the loan |
| `rc-liquidation-engine` | full overdue → grace → liquidation with excess returned; default with nothing repaid; cranks are permissionless but refuse to act on a healthy loan |

Conventions:

- `env.mock_all_auths()` so tests exercise role and state logic rather than the host's signature machinery.
- Satellite suites register **real** `GuarantorVaultContract` and `LoanLedgerContract` instances, so cross-contract calls are integration-tested rather than mocked.
- `env.ledger().set_timestamp()` advances time across due dates and grace expiry.
- Negative paths use the generated `try_*` methods and assert the invocation errors.

```bash
cd contracts
cargo test
cargo test -p rc-loan-ledger
```

---

## 13. Deployment & Wire-Up

The three contracts hold each other's addresses, so deployment is only half the job. Wiring order matters, and two steps fail silently later if skipped.

```
   ① deploy all three          ② GuarantorVault::initialize(admin, usdc, settlement)
            │                              │
            ▼                              ▼
   ③ LoanLedger::initialize(admin, vault, 15000, 11000, 500, 14 days)
            │
            ▼
   ④ LiquidationEngine::initialize(admin, vault, ledger)
            │
            ▼
   ⑤ GuarantorVault::set_loan_ledger(admin, ledger)        ← origination fails without this
   ⑥ GuarantorVault::set_liquidation_engine(admin, engine) ← liquidation fails without this
   ⑦ LoanLedger::set_liquidation_engine(admin, engine)     ← no loan can leave Active
            │
            ▼
   ⑧ LoanLedger::set_oracle(admin, oracle)
     LoanLedger::set_partner(admin, partner, true)   — once per off-ramp partner
```

Production configuration defaults: `base_ltv_bps = 15000` (150%), `min_ltv_bps = 11000` (110%), `safety_buffer_bps = 500` (5%), `grace_period_secs = 1209600` (14 days).

---

## 14. Trust Model

### What the contracts trust

| Assumption | Why it is acceptable | Mitigation |
|------------|---------------------|------------|
| A registered off-ramp partner attests honestly. | The partner is a licensed entity under contract; the chain cannot observe a mobile-money transfer. | Admin registration is revocable and takes effect immediately. Attestations are individually attributable on-chain. Multi-partner corroboration is on the roadmap. |
| The oracle publishes an honest reputation score. | Scoring needs remittance history the chain does not hold. | The score only moves LTV within a bounded range — between the floor and the base rate. A dishonest oracle cannot push collateral below `min_ltv_bps`. |
| The admin wires the contracts correctly. | Wiring is one-time and observable on-chain. | Misconfiguration fails closed: an unset ledger or engine makes the relevant operation revert rather than proceed unchecked. |

### What the contracts do not trust

- **The beneficiary.** They have no address and cannot invoke anything.
- **The guarantor's account of repayment.** Only partner attestations advance a loan; a guarantor cannot free their own collateral by asserting they were repaid.
- **The platform's liveness.** The liquidation cranks are permissionless, so default handling does not depend on the operator's job runner staying up.
- **The admin with custody.** No admin function moves a guarantor's collateral to the admin.

---

## 15. Known Limitations

Stated plainly, because these are boundaries of the current version rather than hidden defects.

| Limitation | Detail |
|------------|--------|
| **Single-partner attestation** | One registered partner's authorization is sufficient to record a repayment and release collateral. A compromised or dishonest partner can free a guarantor's collateral without money having moved. Corroboration across partners is the intended fix. |
| **Oracle sets the LTV** | Reputation is published wholesale rather than derived on-chain. The damage is bounded by `min_ltv_bps`, but a wrong score still under-collateralises a loan within that band. |
| **Installment counting is per-attestation** | `installments_paid` increments once per attestation regardless of amount, so a partner submitting several partial payments advances `next_due` faster than the real schedule. Collateral release and loan closure are both driven by principal actually repaid, so the exposure is limited to due-date drift, which can delay when a genuinely late loan becomes liquidatable. |
| **No interest or fees** | Loans are principal-only. There is no interest accrual, origination fee, or late penalty in the contracts. |
| **Global grace period** | `grace_period_secs` is one value for every loan, not varied by size or reputation. |
| **Liquidation is a transfer, not a swap** | Forfeited collateral moves to a platform-controlled settlement address. DEX-based liquidation is deferred. |
| **Reputation penalty is off-chain** | A default does not itself change the stored score; the oracle is expected to republish. Until it does, the beneficiary's LTV is unchanged. |
| **No TTL management** | No contract extends the lifetime of vaults, loans, or reputation entries against state archival. |
| **Immutable wiring targets** | `initialize` is one-shot per contract and the admin cannot be rotated. Changing the admin requires a fresh deployment. |
| **Truncating arithmetic** | Integer division rounds against the guarantor on intermediate releases. Full settlement at closing makes it whole, but a defaulted loan keeps the rounding dust. |

---

## Related Documents

- [README.md](README.md) — protocol overview, build, test and deployment quick start
