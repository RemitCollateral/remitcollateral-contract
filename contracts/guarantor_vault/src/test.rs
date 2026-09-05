#![cfg(test)]

use super::*;
use soroban_sdk::{testutils::Address as _, token, Env};

struct Setup<'a> {
    env: Env,
    vault: GuarantorVaultContractClient<'a>,
    usdc: token::Client<'a>,
    admin: Address,
    ledger: Address,
    engine: Address,
    settlement: Address,
    guarantor: Address,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let ledger = Address::generate(&env);
    let engine = Address::generate(&env);
    let settlement = Address::generate(&env);
    let guarantor = Address::generate(&env);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = token::Client::new(&env, &sac.address());
    token::StellarAssetClient::new(&env, &sac.address()).mint(&guarantor, &10_000);

    let id = env.register(GuarantorVaultContract, ());
    let vault = GuarantorVaultContractClient::new(&env, &id);
    vault.initialize(&admin, &sac.address(), &settlement);
    vault.set_loan_ledger(&admin, &ledger);
    vault.set_liquidation_engine(&admin, &engine);

    Setup {
        env,
        vault,
        usdc,
        admin,
        ledger,
        engine,
        settlement,
        guarantor,
    }
}

#[test]
fn test_deposit_and_withdraw() {
    let s = setup();

    s.vault.deposit(&s.guarantor, &1_000);
    assert_eq!(s.vault.get_balance(&s.guarantor), 1_000);
    assert_eq!(s.vault.get_available(&s.guarantor), 1_000);
    assert_eq!(s.usdc.balance(&s.guarantor), 9_000);

    s.vault.withdraw(&s.guarantor, &400);
    assert_eq!(s.vault.get_balance(&s.guarantor), 600);
    assert_eq!(s.usdc.balance(&s.guarantor), 9_400);

    // Cannot withdraw more than the balance.
    assert!(s.vault.try_withdraw(&s.guarantor, &601).is_err());
    // Zero and negative amounts are rejected.
    assert!(s.vault.try_deposit(&s.guarantor, &0).is_err());
}

#[test]
fn test_vaults_are_isolated() {
    let s = setup();
    let other = Address::generate(&s.env);
    token::StellarAssetClient::new(&s.env, &s.usdc.address).mint(&other, &5_000);

    s.vault.deposit(&s.guarantor, &1_000);
    s.vault.deposit(&other, &2_000);

    // One guarantor's locked collateral never constrains another's.
    s.vault.lock_collateral(&s.ledger, &s.guarantor, &1_000);
    assert_eq!(s.vault.get_available(&s.guarantor), 0);
    assert_eq!(s.vault.get_available(&other), 2_000);

    s.vault.withdraw(&other, &2_000);
    assert_eq!(s.vault.get_balance(&other), 0);
    assert_eq!(s.vault.get_balance(&s.guarantor), 1_000);
}

#[test]
fn test_locked_collateral_cannot_be_withdrawn() {
    let s = setup();
    s.vault.deposit(&s.guarantor, &1_000);

    s.vault.lock_collateral(&s.ledger, &s.guarantor, &750);
    assert_eq!(s.vault.get_locked(&s.guarantor), 750);
    assert_eq!(s.vault.get_available(&s.guarantor), 250);

    assert!(s.vault.try_withdraw(&s.guarantor, &251).is_err());
    s.vault.withdraw(&s.guarantor, &250);

    // Cannot lock what is not available.
    assert!(s
        .vault
        .try_lock_collateral(&s.ledger, &s.guarantor, &1)
        .is_err());

    s.vault.release_collateral(&s.ledger, &s.guarantor, &750);
    assert_eq!(s.vault.get_available(&s.guarantor), 750);
    s.vault.withdraw(&s.guarantor, &750);
    assert_eq!(s.vault.get_balance(&s.guarantor), 0);
}

#[test]
fn test_forfeit_sends_usdc_to_settlement() {
    let s = setup();
    s.vault.deposit(&s.guarantor, &1_000);
    s.vault.lock_collateral(&s.ledger, &s.guarantor, &900);

    s.vault.forfeit_collateral(&s.engine, &s.guarantor, &600);

    assert_eq!(s.usdc.balance(&s.settlement), 600);
    assert_eq!(s.vault.get_balance(&s.guarantor), 400);
    assert_eq!(s.vault.get_locked(&s.guarantor), 300);
    assert_eq!(s.vault.get_available(&s.guarantor), 100);

    // Cannot forfeit beyond what is locked.
    assert!(s
        .vault
        .try_forfeit_collateral(&s.engine, &s.guarantor, &301)
        .is_err());
}

#[test]
fn test_authorization_is_enforced() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    s.vault.deposit(&s.guarantor, &1_000);

    // Only the ledger may lock.
    assert!(s
        .vault
        .try_lock_collateral(&stranger, &s.guarantor, &100)
        .is_err());
    assert!(s
        .vault
        .try_lock_collateral(&s.engine, &s.guarantor, &100)
        .is_err());

    // Only the engine may forfeit.
    s.vault.lock_collateral(&s.ledger, &s.guarantor, &100);
    assert!(s
        .vault
        .try_forfeit_collateral(&s.ledger, &s.guarantor, &100)
        .is_err());
    assert!(s
        .vault
        .try_forfeit_collateral(&stranger, &s.guarantor, &100)
        .is_err());

    // Only the admin may rewire the contract.
    assert!(s.vault.try_set_loan_ledger(&stranger, &stranger).is_err());
    assert!(s
        .vault
        .try_set_settlement_address(&stranger, &stranger)
        .is_err());
    assert!(s
        .vault
        .try_initialize(&s.admin, &s.usdc.address, &s.settlement)
        .is_err());
}
