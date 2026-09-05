#![no_std]
//! LoanLedger — loan records, repayment schedules and attested repayments.
//!
//! The beneficiary has no wallet and never appears on-chain as an `Address`.
//! They are identified by a 32-byte handle the backend derives from their phone
//! number and the off-ramp partner's KYC reference, so no personally
//! identifying data reaches the ledger.
//!
//! Repayments happen off-chain in local currency. The protocol learns about them
//! only through a signed attestation from a registered off-ramp partner, which
//! on-chain means an invocation authorized by that partner's address.

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, panic_with_error, Address,
    BytesN, Env,
};

const BPS: i128 = 10_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAuthorized = 3,
    InvalidAmount = 4,
    InvalidSchedule = 5,
    InvalidConfig = 6,
    InvalidScore = 7,
    LoanNotFound = 8,
    LoanNotActive = 9,
    LoanAlreadyOpen = 10,
    NotOverdue = 11,
    GraceNotExpired = 12,
    Overpayment = 13,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoanStatus {
    /// Repayments are on schedule.
    Active,
    /// An installment is overdue but the grace period has not expired.
    Grace,
    /// Fully repaid; all collateral returned.
    Repaid,
    /// Grace expired without payment; collateral forfeited.
    Defaulted,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Loan {
    pub id: u64,
    pub guarantor: Address,
    /// Off-chain handle for the beneficiary; never a wallet address.
    pub beneficiary: BytesN<32>,
    pub principal_usd: i128,
    /// Loan-to-value applied at origination, in basis points (15000 = 150%).
    pub ltv_bps: u32,
    pub collateral_locked: i128,
    pub collateral_released: i128,
    pub installment_count: u32,
    pub installment_amount: i128,
    pub interval_secs: u64,
    pub installments_paid: u32,
    pub total_repaid_usd: i128,
    pub next_due: u64,
    pub grace_expires_at: u64,
    pub status: LoanStatus,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// LTV required of a beneficiary with no reputation (15000 = 150%).
    pub base_ltv_bps: u32,
    /// Floor the LTV can never go below however good the reputation (11000 = 110%).
    pub min_ltv_bps: u32,
    /// Share of collateral withheld until the final installment (500 = 5%).
    pub safety_buffer_bps: u32,
    /// How long after a missed installment before the loan may be liquidated.
    pub grace_period_secs: u64,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Oracle,
    Vault,
    LiquidationEngine,
    Config,
    LoanCount,
    Loan(u64),
    Partner(Address),
    Reputation(BytesN<32>),
    OpenLoan(Address, BytesN<32>),
}

/// The slice of GuarantorVault this contract calls.
#[contractclient(name = "VaultClient")]
pub trait VaultInterface {
    fn lock_collateral(env: Env, caller: Address, guarantor: Address, amount: i128);
    fn release_collateral(env: Env, caller: Address, guarantor: Address, amount: i128);
}

#[contract]
pub struct LoanLedgerContract;

#[contractimpl]
impl LoanLedgerContract {
    pub fn initialize(
        env: Env,
        admin: Address,
        vault: Address,
        base_ltv_bps: u32,
        min_ltv_bps: u32,
        safety_buffer_bps: u32,
        grace_period_secs: u64,
    ) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        if min_ltv_bps > base_ltv_bps || safety_buffer_bps >= BPS as u32 || grace_period_secs == 0 {
            panic_with_error!(&env, Error::InvalidConfig);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Vault, &vault);
        env.storage().instance().set(&DataKey::LoanCount, &0u64);
        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                base_ltv_bps,
                min_ltv_bps,
                safety_buffer_bps,
                grace_period_secs,
            },
        );
    }

    // --- Administration ---

    /// Register the oracle allowed to publish reputation scores.
    pub fn set_oracle(env: Env, admin: Address, oracle: Address) {
        Self::require_admin(&env, &admin);
        env.storage().instance().set(&DataKey::Oracle, &oracle);
    }

    /// Register the LiquidationEngine allowed to drive default transitions.
    pub fn set_liquidation_engine(env: Env, admin: Address, engine: Address) {
        Self::require_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::LiquidationEngine, &engine);
    }

    /// Authorize or revoke an off-ramp partner. Only an authorized partner may
    /// attest that a repayment happened.
    pub fn set_partner(env: Env, admin: Address, partner: Address, authorized: bool) {
        Self::require_admin(&env, &admin);
        env.storage()
            .persistent()
            .set(&DataKey::Partner(partner), &authorized);
    }

    /// Publish a beneficiary's composite reputation score, in basis points of a
    /// perfect score. Computed off-chain from remittance and repayment history.
    pub fn set_reputation(env: Env, oracle: Address, beneficiary: BytesN<32>, score_bps: u32) {
        oracle.require_auth();
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Oracle)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotInitialized));
        if oracle != stored {
            panic_with_error!(&env, Error::NotAuthorized);
        }
        if score_bps > BPS as u32 {
            panic_with_error!(&env, Error::InvalidScore);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Reputation(beneficiary), &score_bps);
    }

    // --- Origination ---

    /// The LTV a given beneficiary currently qualifies for. A perfect reputation
    /// earns the configured floor; no reputation pays the base rate.
    pub fn required_ltv_bps(env: Env, beneficiary: BytesN<32>) -> u32 {
        let config = Self::config(&env);
        let score = Self::get_reputation(env.clone(), beneficiary) as i128;
        let span = (config.base_ltv_bps - config.min_ltv_bps) as i128;
        let reduction = span * score / BPS;
        (config.base_ltv_bps as i128 - reduction) as u32
    }

    /// Open a loan. Locks `principal * ltv` of the guarantor's collateral before
    /// the off-ramp partner is instructed to disburse local currency.
    pub fn originate(
        env: Env,
        guarantor: Address,
        beneficiary: BytesN<32>,
        principal_usd: i128,
        installment_count: u32,
        interval_secs: u64,
    ) -> u64 {
        guarantor.require_auth();
        if principal_usd <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        if installment_count == 0 || interval_secs == 0 {
            panic_with_error!(&env, Error::InvalidSchedule);
        }

        // One live loan per guarantor-beneficiary pair at a time.
        let open_key = DataKey::OpenLoan(guarantor.clone(), beneficiary.clone());
        if env.storage().persistent().has(&open_key) {
            panic_with_error!(&env, Error::LoanAlreadyOpen);
        }

        let ltv_bps = Self::required_ltv_bps(env.clone(), beneficiary.clone());
        let collateral = principal_usd * ltv_bps as i128 / BPS;

        Self::vault(&env).lock_collateral(&env.current_contract_address(), &guarantor, &collateral);

        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LoanCount)
            .unwrap_or(0);
        let id = count + 1;
        env.storage().instance().set(&DataKey::LoanCount, &id);

        let loan = Loan {
            id,
            guarantor: guarantor.clone(),
            beneficiary: beneficiary.clone(),
            principal_usd,
            ltv_bps,
            collateral_locked: collateral,
            collateral_released: 0,
            installment_count,
            installment_amount: principal_usd / installment_count as i128,
            interval_secs,
            installments_paid: 0,
            total_repaid_usd: 0,
            next_due: env.ledger().timestamp() + interval_secs,
            grace_expires_at: 0,
            status: LoanStatus::Active,
        };

        env.storage().persistent().set(&DataKey::Loan(id), &loan);
        env.storage().persistent().set(&open_key, &id);
        id
    }

    // --- Repayment ---

    /// Record a repayment the off-ramp partner collected in local currency, and
    /// release the collateral it earns back. Returns the amount released.
    ///
    /// Release is proportional to principal repaid, less the safety buffer that
    /// is withheld until the loan closes:
    ///
    /// ```text
    /// releasable = collateral * (repaid / principal) * (1 - safety_buffer)
    /// ```
    pub fn attest_repayment(env: Env, partner: Address, loan_id: u64, amount_usd: i128) -> i128 {
        partner.require_auth();
        if !Self::is_partner(env.clone(), partner) {
            panic_with_error!(&env, Error::NotAuthorized);
        }
        if amount_usd <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        let mut loan = Self::loan_of(&env, loan_id);
        if !matches!(loan.status, LoanStatus::Active | LoanStatus::Grace) {
            panic_with_error!(&env, Error::LoanNotActive);
        }
        if loan.total_repaid_usd + amount_usd > loan.principal_usd {
            panic_with_error!(&env, Error::Overpayment);
        }

        loan.total_repaid_usd += amount_usd;
        loan.installments_paid += 1;

        // Closing is decided by principal actually repaid, never by how many
        // attestations have arrived. Counting attestations would let a partner
        // close a loan — and release all of its collateral — with a handful of
        // token payments.
        let fully_repaid = loan.total_repaid_usd >= loan.principal_usd;

        let releasable = if fully_repaid {
            // Closing the loan returns everything, safety buffer included.
            loan.collateral_locked
        } else {
            let config = Self::config(&env);
            let earned = loan.collateral_locked * loan.total_repaid_usd / loan.principal_usd;
            earned * (BPS - config.safety_buffer_bps as i128) / BPS
        };

        let release_now = releasable - loan.collateral_released;
        if release_now > 0 {
            Self::vault(&env).release_collateral(
                &env.current_contract_address(),
                &loan.guarantor,
                &release_now,
            );
            loan.collateral_released += release_now;
        }

        if fully_repaid {
            loan.status = LoanStatus::Repaid;
            loan.grace_expires_at = 0;
            env.storage().persistent().remove(&DataKey::OpenLoan(
                loan.guarantor.clone(),
                loan.beneficiary.clone(),
            ));
        } else {
            // A payment clears any grace period and advances the schedule.
            loan.status = LoanStatus::Active;
            loan.grace_expires_at = 0;
            loan.next_due += loan.interval_secs;
        }

        env.storage()
            .persistent()
            .set(&DataKey::Loan(loan_id), &loan);
        release_now
    }

    // --- Default transitions (LiquidationEngine only) ---

    /// Move an overdue loan into its grace period.
    pub fn mark_grace(env: Env, caller: Address, loan_id: u64) {
        caller.require_auth();
        Self::require_engine(&env, &caller);

        let mut loan = Self::loan_of(&env, loan_id);
        if !matches!(loan.status, LoanStatus::Active) {
            panic_with_error!(&env, Error::LoanNotActive);
        }
        if env.ledger().timestamp() <= loan.next_due {
            panic_with_error!(&env, Error::NotOverdue);
        }

        let config = Self::config(&env);
        loan.status = LoanStatus::Grace;
        loan.grace_expires_at = env.ledger().timestamp() + config.grace_period_secs;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(loan_id), &loan);
    }

    /// Close a loan as defaulted. The engine calls this after it has settled the
    /// collateral, so the ledger records the full locked amount as accounted for.
    pub fn mark_defaulted(env: Env, caller: Address, loan_id: u64) {
        caller.require_auth();
        Self::require_engine(&env, &caller);

        let mut loan = Self::loan_of(&env, loan_id);
        if !matches!(loan.status, LoanStatus::Grace) {
            panic_with_error!(&env, Error::LoanNotActive);
        }
        if env.ledger().timestamp() <= loan.grace_expires_at {
            panic_with_error!(&env, Error::GraceNotExpired);
        }

        loan.status = LoanStatus::Defaulted;
        loan.collateral_released = loan.collateral_locked;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(loan_id), &loan);
        env.storage().persistent().remove(&DataKey::OpenLoan(
            loan.guarantor.clone(),
            loan.beneficiary.clone(),
        ));
    }

    // --- Views used by the LiquidationEngine ---

    pub fn is_overdue(env: Env, loan_id: u64) -> bool {
        let loan = Self::loan_of(&env, loan_id);
        matches!(loan.status, LoanStatus::Active) && env.ledger().timestamp() > loan.next_due
    }

    pub fn is_grace_expired(env: Env, loan_id: u64) -> bool {
        let loan = Self::loan_of(&env, loan_id);
        matches!(loan.status, LoanStatus::Grace) && env.ledger().timestamp() > loan.grace_expires_at
    }

    pub fn loan_guarantor(env: Env, loan_id: u64) -> Address {
        Self::loan_of(&env, loan_id).guarantor
    }

    /// Principal still owed.
    pub fn loan_outstanding(env: Env, loan_id: u64) -> i128 {
        let loan = Self::loan_of(&env, loan_id);
        let outstanding = loan.principal_usd - loan.total_repaid_usd;
        if outstanding > 0 {
            outstanding
        } else {
            0
        }
    }

    /// Collateral still locked against this loan.
    pub fn loan_collateral_remaining(env: Env, loan_id: u64) -> i128 {
        let loan = Self::loan_of(&env, loan_id);
        loan.collateral_locked - loan.collateral_released
    }

    // --- Getters ---

    pub fn get_loan(env: Env, loan_id: u64) -> Option<Loan> {
        env.storage().persistent().get(&DataKey::Loan(loan_id))
    }

    pub fn get_config(env: Env) -> Config {
        Self::config(&env)
    }

    pub fn get_reputation(env: Env, beneficiary: BytesN<32>) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::Reputation(beneficiary))
            .unwrap_or(0)
    }

    pub fn is_partner(env: Env, partner: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Partner(partner))
            .unwrap_or(false)
    }

    pub fn get_open_loan(env: Env, guarantor: Address, beneficiary: BytesN<32>) -> Option<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::OpenLoan(guarantor, beneficiary))
    }

    pub fn get_loan_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::LoanCount)
            .unwrap_or(0)
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn get_vault(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Vault).unwrap()
    }

    // --- Internals ---

    fn config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized))
    }

    fn loan_of(env: &Env, loan_id: u64) -> Loan {
        env.storage()
            .persistent()
            .get(&DataKey::Loan(loan_id))
            .unwrap_or_else(|| panic_with_error!(env, Error::LoanNotFound))
    }

    fn vault(env: &Env) -> VaultClient<'_> {
        let addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::Vault)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        VaultClient::new(env, &addr)
    }

    fn require_admin(env: &Env, admin: &Address) {
        admin.require_auth();
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        if *admin != stored {
            panic_with_error!(env, Error::NotAuthorized);
        }
    }

    fn require_engine(env: &Env, caller: &Address) {
        let engine: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidationEngine)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        if *caller != engine {
            panic_with_error!(env, Error::NotAuthorized);
        }
    }
}

#[cfg(test)]
mod test;
