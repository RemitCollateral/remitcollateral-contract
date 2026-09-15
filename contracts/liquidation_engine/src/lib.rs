#![no_std]
//! LiquidationEngine — default detection and collateral settlement.
//!
//! Both entry points are permissionless cranks. Anyone may call them; what they
//! do is determined entirely by the loan's own state and the ledger clock, so
//! there is nothing for a caller to influence. That means a missed installment
//! cannot be left unrecorded because the platform failed to run a job.
//!
//! The engine never decides *how much* is owed — it reads that from the ledger
//! and settles exactly the outstanding amount, returning any excess collateral
//! to the guarantor rather than seizing the whole position.

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, panic_with_error, Address,
    BytesN, Env,
};

/// Ledgers per day at Stellar's 5-second ledger close time.
const DAY_IN_LEDGERS: u32 = 17_280;
/// Storage lifetimes, in ledgers. Entries are extended to about 120 days
/// whenever they fall below about 90, so they stay live while in use without
/// paying rent on every call. Both are well under the network's maximum entry
/// lifetime of about 180 days.
const EXTEND_TO: u32 = 120 * DAY_IN_LEDGERS;
const THRESHOLD: u32 = 90 * DAY_IN_LEDGERS;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 2,
    NotAuthorized = 3,
    NotOverdue = 4,
    GraceNotExpired = 5,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Vault,
    LoanLedger,
    PendingAdmin,
}

/// The slice of LoanLedger this contract calls.
#[contractclient(name = "LedgerClient")]
pub trait LedgerInterface {
    fn is_overdue(env: Env, loan_id: u64) -> bool;
    fn is_grace_expired(env: Env, loan_id: u64) -> bool;
    fn loan_guarantor(env: Env, loan_id: u64) -> Address;
    fn loan_outstanding(env: Env, loan_id: u64) -> i128;
    fn loan_collateral_remaining(env: Env, loan_id: u64) -> i128;
    fn mark_grace(env: Env, caller: Address, loan_id: u64);
    fn mark_defaulted(env: Env, caller: Address, loan_id: u64);
}

/// The slice of GuarantorVault this contract calls.
#[contractclient(name = "VaultClient")]
pub trait VaultInterface {
    fn release_collateral(env: Env, caller: Address, guarantor: Address, amount: i128);
    fn forfeit_collateral(env: Env, caller: Address, guarantor: Address, amount: i128);
}

#[contract]
pub struct LiquidationEngineContract;

#[contractimpl]
impl LiquidationEngineContract {
    /// Runs once, atomically, as part of the deploy transaction. There is no
    /// separate initialize call for anyone to front-run between deployment and
    /// setup, so nobody else can claim the admin role.
    pub fn __constructor(env: Env, admin: Address, vault: Address, loan_ledger: Address) {
        Self::extend_instance(&env);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Vault, &vault);
        env.storage()
            .instance()
            .set(&DataKey::LoanLedger, &loan_ledger);
    }

    // --- Administration ---

    /// Replace this contract's code while keeping its address and storage, so
    /// a bug found after launch can be fixed without migrating live loans or
    /// locked collateral. Admin only. The new wasm must already be uploaded.
    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) {
        Self::require_admin(&env, &admin);
        Self::extend_instance(&env);
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    /// Begin handing the admin role to `new_admin`. Nothing changes until the
    /// new admin accepts, so a mistyped address cannot lock the protocol out.
    pub fn propose_admin(env: Env, admin: Address, new_admin: Address) {
        Self::require_admin(&env, &admin);
        Self::extend_instance(&env);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
    }

    /// Complete a handover proposed by the current admin.
    pub fn accept_admin(env: Env, new_admin: Address) {
        new_admin.require_auth();
        Self::extend_instance(&env);
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotAuthorized));
        if new_admin != pending {
            panic_with_error!(&env, Error::NotAuthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
    }

    // --- Cranks ---

    /// Start the grace period on an overdue loan. Permissionless.
    pub fn flag_overdue(env: Env, loan_id: u64) {
        Self::extend_instance(&env);
        let ledger = Self::ledger(&env);
        if !ledger.is_overdue(&loan_id) {
            panic_with_error!(&env, Error::NotOverdue);
        }
        ledger.mark_grace(&env.current_contract_address(), &loan_id);
    }

    /// Liquidate a loan whose grace period has expired. Permissionless.
    ///
    /// Forfeits collateral equal to the outstanding principal — capped at what
    /// is actually locked — and returns the remainder to the guarantor. Returns
    /// the amount forfeited.
    pub fn liquidate(env: Env, loan_id: u64) -> i128 {
        Self::extend_instance(&env);
        let ledger = Self::ledger(&env);
        if !ledger.is_grace_expired(&loan_id) {
            panic_with_error!(&env, Error::GraceNotExpired);
        }

        let guarantor = ledger.loan_guarantor(&loan_id);
        let outstanding = ledger.loan_outstanding(&loan_id);
        let remaining = ledger.loan_collateral_remaining(&loan_id);

        let forfeited = if outstanding < remaining {
            outstanding
        } else {
            remaining
        };
        let returned = remaining - forfeited;

        let vault = Self::vault(&env);
        let engine = env.current_contract_address();
        if forfeited > 0 {
            vault.forfeit_collateral(&engine, &guarantor, &forfeited);
        }
        if returned > 0 {
            vault.release_collateral(&engine, &guarantor, &returned);
        }

        ledger.mark_defaulted(&engine, &loan_id);
        forfeited
    }

    /// Convenience crank: advance whichever transition the loan is due for.
    /// Returns true if it did something.
    pub fn poke(env: Env, loan_id: u64) -> bool {
        Self::extend_instance(&env);
        let ledger = Self::ledger(&env);
        if ledger.is_overdue(&loan_id) {
            ledger.mark_grace(&env.current_contract_address(), &loan_id);
            true
        } else if ledger.is_grace_expired(&loan_id) {
            Self::liquidate(env, loan_id);
            true
        } else {
            false
        }
    }

    // --- Getters ---

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn get_vault(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Vault).unwrap()
    }

    pub fn get_loan_ledger(env: Env) -> Address {
        env.storage().instance().get(&DataKey::LoanLedger).unwrap()
    }

    // --- Internals ---

    /// Keep the contract instance, and with it the configuration and the
    /// contract code, from being archived while the protocol is in use.
    fn extend_instance(env: &Env) {
        env.storage().instance().extend_ttl(THRESHOLD, EXTEND_TO);
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

    fn ledger(env: &Env) -> LedgerClient<'_> {
        let addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::LoanLedger)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        LedgerClient::new(env, &addr)
    }

    fn vault(env: &Env) -> VaultClient<'_> {
        let addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::Vault)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        VaultClient::new(env, &addr)
    }
}

#[cfg(test)]
mod test;
