#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{
        storage::{Instance as _, Persistent as _},
        Address as _, Ledger as _,
    },
    token, BytesN, Env,
};

const TIMELOCK: u64 = 172_800;

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

    let id = env.register(
        GuarantorVaultContract,
        (admin.clone(), sac.address(), settlement.clone(), TIMELOCK),
    );
    let vault = GuarantorVaultContractClient::new(&env, &id);
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
        .try_schedule_action(&stranger, &Action::SetSettlement(stranger.clone()))
        .is_err());
}

#[test]
fn test_constructor_sets_configuration_at_deploy() {
    let s = setup();
    // Configuration comes from the deploy transaction itself, so there is no
    // window in which an uninitialized vault could be claimed by someone else.
    assert_eq!(s.vault.get_admin(), s.admin);
    assert_eq!(s.vault.get_usdc_token(), s.usdc.address);
    assert_eq!(s.vault.get_settlement_address(), s.settlement);
}

fn vault_ttl(s: &Setup, guarantor: &Address) -> u32 {
    s.env.as_contract(&s.vault.address, || {
        s.env
            .storage()
            .persistent()
            .get_ttl(&DataKey::Vault(guarantor.clone()))
    })
}

#[test]
fn test_vault_lifetime_is_extended_on_use() {
    let s = setup();
    s.vault.deposit(&s.guarantor, &1_000);

    // First use extends the vault, and the contract instance, to the full window.
    assert_eq!(vault_ttl(&s, &s.guarantor), EXTEND_TO);
    let instance_ttl = s
        .env
        .as_contract(&s.vault.address, || s.env.storage().instance().get_ttl());
    assert_eq!(instance_ttl, EXTEND_TO);

    // Forty days on, the entry has aged below the renewal threshold...
    let aged = 40 * DAY_IN_LEDGERS;
    s.env
        .ledger()
        .set_sequence_number(s.env.ledger().sequence() + aged);
    assert_eq!(vault_ttl(&s, &s.guarantor), EXTEND_TO - aged);
    assert!(EXTEND_TO - aged < THRESHOLD);

    // ...and its next use renews it to the full window again.
    s.vault.lock_collateral(&s.ledger, &s.guarantor, &1);
    assert_eq!(vault_ttl(&s, &s.guarantor), EXTEND_TO);
}

#[test]
fn test_wiring_is_set_once() {
    let s = setup();
    // Setup already wired the vault. It cannot be pointed anywhere else.
    assert!(s.vault.try_set_loan_ledger(&s.admin, &s.engine).is_err());
    assert!(s
        .vault
        .try_set_liquidation_engine(&s.admin, &s.ledger)
        .is_err());
    assert_eq!(s.vault.get_loan_ledger(), Some(s.ledger.clone()));
    assert_eq!(s.vault.get_liquidation_engine(), Some(s.engine.clone()));
}

#[test]
fn test_sensitive_changes_wait_out_the_timelock() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    let new_settlement = Address::generate(&s.env);
    let not_authorized = soroban_sdk::Error::from(Error::NotAuthorized);
    let too_early = soroban_sdk::Error::from(Error::TimelockNotExpired);
    assert_eq!(s.vault.get_timelock_secs(), TIMELOCK);

    // Only the admin may schedule, and scheduling changes nothing yet.
    assert!(matches!(
        s.vault
            .try_schedule_action(&stranger, &Action::SetSettlement(stranger.clone())),
        Err(Ok(e)) if e == not_authorized
    ));
    s.vault
        .schedule_action(&s.admin, &Action::SetSettlement(new_settlement.clone()));
    assert_eq!(s.vault.get_settlement_address(), s.settlement);
    assert_eq!(s.vault.get_scheduled_action().unwrap().eta, TIMELOCK);

    // One change at a time.
    assert!(s
        .vault
        .try_schedule_action(&s.admin, &Action::SetSettlement(stranger.clone()))
        .is_err());

    // It cannot run early...
    s.env.ledger().set_timestamp(TIMELOCK - 1);
    assert!(matches!(
        s.vault.try_execute_action(&s.admin),
        Err(Ok(e)) if e == too_early
    ));
    assert_eq!(s.vault.get_settlement_address(), s.settlement);

    // ...and runs once the delay has passed.
    s.env.ledger().set_timestamp(TIMELOCK);
    s.vault.execute_action(&s.admin);
    assert_eq!(s.vault.get_settlement_address(), new_settlement);
    assert!(s.vault.get_scheduled_action().is_none());

    // A scheduled upgrade can be cancelled before it runs.
    s.vault.schedule_action(
        &s.admin,
        &Action::Upgrade(BytesN::from_array(&s.env, &[0u8; 32])),
    );
    s.vault.cancel_action(&s.admin);
    assert!(s.vault.get_scheduled_action().is_none());
    assert!(s.vault.try_execute_action(&s.admin).is_err());
}

#[test]
fn test_admin_handover_is_two_step() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    let new_admin = Address::generate(&s.env);
    let not_authorized = soroban_sdk::Error::from(Error::NotAuthorized);

    assert!(s.vault.try_propose_admin(&stranger, &new_admin).is_err());
    s.vault.propose_admin(&s.admin, &new_admin);
    assert_eq!(s.vault.get_admin(), s.admin);
    assert!(s.vault.try_accept_admin(&stranger).is_err());
    s.vault.accept_admin(&new_admin);
    assert_eq!(s.vault.get_admin(), new_admin);

    // The old admin loses its powers; the new one has them.
    let action = Action::SetSettlement(stranger.clone());
    assert!(matches!(
        s.vault.try_schedule_action(&s.admin, &action),
        Err(Ok(e)) if e == not_authorized
    ));
    s.vault.schedule_action(&new_admin, &action);
    // A completed handover cannot be replayed.
    assert!(s.vault.try_accept_admin(&new_admin).is_err());
}
