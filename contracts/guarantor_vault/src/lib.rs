#![no_std]
//! GuarantorVault — isolated USDC collateral vaults.
//!
//! Every guarantor gets their own vault. Collateral is never pooled across
//! guarantors, so a default on one relationship can never touch another
//! guarantor's funds.
//!
//! The vault is deliberately passive: it holds USDC and tracks how much of it
//! is spoken for, but it does not know what a loan is. The LoanLedger decides
//! when to lock and release; the LiquidationEngine decides when to forfeit.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token, Address, Env,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAuthorized = 3,
    InvalidAmount = 4,
    InsufficientAvailable = 5,
    InsufficientLocked = 6,
    LedgerNotSet = 7,
    EngineNotSet = 8,
}

/// A single guarantor's collateral position.
///
/// `locked_amount` is the portion backing active loans. `collateral_balance`
/// minus `locked_amount` is what the guarantor may withdraw.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vault {
    pub guarantor: Address,
    pub collateral_balance: i128,
    pub locked_amount: i128,
}

#[contracttype]
pub enum DataKey {
    Admin,
    UsdcToken,
    SettlementAddress,
    LoanLedger,
    LiquidationEngine,
    Vault(Address),
}

#[contract]
pub struct GuarantorVaultContract;

#[contractimpl]
impl GuarantorVaultContract {
    pub fn initialize(env: Env, admin: Address, usdc_token: Address, settlement_address: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::UsdcToken, &usdc_token);
        env.storage()
            .instance()
            .set(&DataKey::SettlementAddress, &settlement_address);
    }

    // --- Administration ---

    /// Register the LoanLedger contract, the only caller allowed to lock collateral.
    pub fn set_loan_ledger(env: Env, admin: Address, ledger: Address) {
        Self::require_admin(&env, &admin);
        env.storage().instance().set(&DataKey::LoanLedger, &ledger);
    }

    /// Register the LiquidationEngine contract, the only caller allowed to forfeit collateral.
    pub fn set_liquidation_engine(env: Env, admin: Address, engine: Address) {
        Self::require_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::LiquidationEngine, &engine);
    }

    /// Change where forfeited collateral is sent.
    pub fn set_settlement_address(env: Env, admin: Address, settlement_address: Address) {
        Self::require_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::SettlementAddress, &settlement_address);
    }

    // --- Guarantor operations ---

    /// Deposit USDC into the caller's own vault.
    pub fn deposit(env: Env, guarantor: Address, amount: i128) {
        guarantor.require_auth();
        if amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        let usdc = Self::usdc_client(&env);
        usdc.transfer(&guarantor, &env.current_contract_address(), &amount);

        let mut vault = Self::vault_of(&env, &guarantor);
        vault.collateral_balance += amount;
        Self::save(&env, &vault);
    }

    /// Withdraw unlocked collateral. Collateral backing an active loan cannot be withdrawn.
    pub fn withdraw(env: Env, guarantor: Address, amount: i128) {
        guarantor.require_auth();
        if amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        let mut vault = Self::vault_of(&env, &guarantor);
        let available = vault.collateral_balance - vault.locked_amount;
        if amount > available {
            panic_with_error!(&env, Error::InsufficientAvailable);
        }

        vault.collateral_balance -= amount;
        Self::save(&env, &vault);

        let usdc = Self::usdc_client(&env);
        usdc.transfer(&env.current_contract_address(), &guarantor, &amount);
    }

    // --- Protocol operations ---

    /// Reserve collateral against a new loan. LoanLedger only.
    pub fn lock_collateral(env: Env, caller: Address, guarantor: Address, amount: i128) {
        caller.require_auth();
        Self::require_ledger(&env, &caller);
        if amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        let mut vault = Self::vault_of(&env, &guarantor);
        let available = vault.collateral_balance - vault.locked_amount;
        if amount > available {
            panic_with_error!(&env, Error::InsufficientAvailable);
        }

        vault.locked_amount += amount;
        Self::save(&env, &vault);
    }

    /// Return collateral to the guarantor's available balance. LoanLedger or
    /// LiquidationEngine — the ledger releases as repayments land, the engine
    /// releases whatever a liquidation did not consume.
    pub fn release_collateral(env: Env, caller: Address, guarantor: Address, amount: i128) {
        caller.require_auth();
        Self::require_ledger_or_engine(&env, &caller);
        if amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        let mut vault = Self::vault_of(&env, &guarantor);
        if amount > vault.locked_amount {
            panic_with_error!(&env, Error::InsufficientLocked);
        }

        vault.locked_amount -= amount;
        Self::save(&env, &vault);
    }

    /// Seize locked collateral and move the USDC to the settlement address.
    /// LiquidationEngine only.
    pub fn forfeit_collateral(env: Env, caller: Address, guarantor: Address, amount: i128) {
        caller.require_auth();
        Self::require_engine(&env, &caller);
        if amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        let mut vault = Self::vault_of(&env, &guarantor);
        if amount > vault.locked_amount {
            panic_with_error!(&env, Error::InsufficientLocked);
        }

        vault.locked_amount -= amount;
        vault.collateral_balance -= amount;
        Self::save(&env, &vault);

        let settlement: Address = env
            .storage()
            .instance()
            .get(&DataKey::SettlementAddress)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotInitialized));
        let usdc = Self::usdc_client(&env);
        usdc.transfer(&env.current_contract_address(), &settlement, &amount);
    }

    // --- Getters ---

    pub fn get_vault(env: Env, guarantor: Address) -> Vault {
        Self::vault_of(&env, &guarantor)
    }

    pub fn get_balance(env: Env, guarantor: Address) -> i128 {
        Self::vault_of(&env, &guarantor).collateral_balance
    }

    pub fn get_locked(env: Env, guarantor: Address) -> i128 {
        Self::vault_of(&env, &guarantor).locked_amount
    }

    pub fn get_available(env: Env, guarantor: Address) -> i128 {
        let vault = Self::vault_of(&env, &guarantor);
        vault.collateral_balance - vault.locked_amount
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn get_usdc_token(env: Env) -> Address {
        env.storage().instance().get(&DataKey::UsdcToken).unwrap()
    }

    pub fn get_settlement_address(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::SettlementAddress)
            .unwrap()
    }

    pub fn get_loan_ledger(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::LoanLedger)
    }

    pub fn get_liquidation_engine(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::LiquidationEngine)
    }

    // --- Internals ---

    fn vault_of(env: &Env, guarantor: &Address) -> Vault {
        env.storage()
            .persistent()
            .get(&DataKey::Vault(guarantor.clone()))
            .unwrap_or(Vault {
                guarantor: guarantor.clone(),
                collateral_balance: 0,
                locked_amount: 0,
            })
    }

    fn save(env: &Env, vault: &Vault) {
        env.storage()
            .persistent()
            .set(&DataKey::Vault(vault.guarantor.clone()), vault);
    }

    fn usdc_client(env: &Env) -> token::Client<'_> {
        let usdc: Address = env
            .storage()
            .instance()
            .get(&DataKey::UsdcToken)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        token::Client::new(env, &usdc)
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

    fn require_ledger(env: &Env, caller: &Address) {
        let ledger: Address = env
            .storage()
            .instance()
            .get(&DataKey::LoanLedger)
            .unwrap_or_else(|| panic_with_error!(env, Error::LedgerNotSet));
        if *caller != ledger {
            panic_with_error!(env, Error::NotAuthorized);
        }
    }

    fn require_engine(env: &Env, caller: &Address) {
        let engine: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidationEngine)
            .unwrap_or_else(|| panic_with_error!(env, Error::EngineNotSet));
        if *caller != engine {
            panic_with_error!(env, Error::NotAuthorized);
        }
    }

    fn require_ledger_or_engine(env: &Env, caller: &Address) {
        let ledger: Option<Address> = env.storage().instance().get(&DataKey::LoanLedger);
        let engine: Option<Address> = env.storage().instance().get(&DataKey::LiquidationEngine);
        let ok = ledger.is_some_and(|l| l == *caller) || engine.is_some_and(|e| e == *caller);
        if !ok {
            panic_with_error!(env, Error::NotAuthorized);
        }
    }
}

#[cfg(test)]
mod test;
