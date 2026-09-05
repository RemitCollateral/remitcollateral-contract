#![cfg(test)]

use super::*;
use rc_guarantor_vault::{GuarantorVaultContract, GuarantorVaultContractClient};
use rc_loan_ledger::{LoanLedgerContract, LoanLedgerContractClient, LoanStatus};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, BytesN, Env,
};

const DAY: u64 = 86_400;

struct Setup<'a> {
    env: Env,
    engine: LiquidationEngineContractClient<'a>,
    ledger: LoanLedgerContractClient<'a>,
    vault: GuarantorVaultContractClient<'a>,
    usdc: token::Client<'a>,
    partner: Address,
    settlement: Address,
    guarantor: Address,
    beneficiary: BytesN<32>,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let oracle = Address::generate(&env);
    let partner = Address::generate(&env);
    let guarantor = Address::generate(&env);
    let settlement = Address::generate(&env);
    let beneficiary = BytesN::from_array(&env, &[3u8; 32]);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = token::Client::new(&env, &sac.address());
    token::StellarAssetClient::new(&env, &sac.address()).mint(&guarantor, &1_000_000);

    let vault_id = env.register(GuarantorVaultContract, ());
    let vault = GuarantorVaultContractClient::new(&env, &vault_id);
    vault.initialize(&admin, &sac.address(), &settlement);

    let ledger_id = env.register(LoanLedgerContract, ());
    let ledger = LoanLedgerContractClient::new(&env, &ledger_id);
    ledger.initialize(&admin, &vault_id, &15_000, &11_000, &500, &(14 * DAY));

    let engine_id = env.register(LiquidationEngineContract, ());
    let engine = LiquidationEngineContractClient::new(&env, &engine_id);
    engine.initialize(&admin, &vault_id, &ledger_id);

    vault.set_loan_ledger(&admin, &ledger_id);
    vault.set_liquidation_engine(&admin, &engine_id);
    ledger.set_oracle(&admin, &oracle);
    ledger.set_liquidation_engine(&admin, &engine_id);
    ledger.set_partner(&admin, &partner, &true);

    vault.deposit(&guarantor, &500_000);

    Setup {
        env,
        engine,
        ledger,
        vault,
        usdc,
        partner,
        settlement,
        guarantor,
        beneficiary,
    }
}

#[test]
fn test_full_default_to_liquidation() {
    let s = setup();
    let id = s
        .ledger
        .originate(&s.guarantor, &s.beneficiary, &10_000, &4, &(30 * DAY));
    // 150% LTV → 15_000 locked. One installment paid → 3_562 released, 11_438 left.
    s.ledger.attest_repayment(&s.partner, &id, &2_500);

    // Nothing to do while the loan is current.
    assert!(!s.engine.poke(&id));
    assert!(s.engine.try_flag_overdue(&id).is_err());
    assert!(s.engine.try_liquidate(&id).is_err());

    // Miss the second installment.
    s.env.ledger().set_timestamp(61 * DAY);
    assert!(s.engine.poke(&id));
    assert!(matches!(
        s.ledger.get_loan(&id).unwrap().status,
        LoanStatus::Grace
    ));

    // Still inside grace — liquidation is refused.
    s.env.ledger().set_timestamp(70 * DAY);
    assert!(s.engine.try_liquidate(&id).is_err());

    // Grace expires.
    s.env.ledger().set_timestamp(76 * DAY);
    let forfeited = s.engine.liquidate(&id);

    // Outstanding was 7_500 and 11_438 was still locked, so only the
    // outstanding amount is seized and the rest goes back to the guarantor.
    assert_eq!(forfeited, 7_500);
    assert_eq!(s.usdc.balance(&s.settlement), 7_500);
    assert!(matches!(
        s.ledger.get_loan(&id).unwrap().status,
        LoanStatus::Defaulted
    ));

    // The guarantor keeps the excess collateral and can withdraw it.
    assert_eq!(s.vault.get_locked(&s.guarantor), 0);
    assert_eq!(s.vault.get_balance(&s.guarantor), 500_000 - 7_500);
    assert_eq!(s.vault.get_available(&s.guarantor), 492_500);
}

#[test]
fn test_default_with_no_repayments_seizes_only_the_principal() {
    let s = setup();
    let beneficiary = BytesN::from_array(&s.env, &[5u8; 32]);
    let id = s
        .ledger
        .originate(&s.guarantor, &beneficiary, &10_000, &1, &(30 * DAY));
    assert_eq!(s.ledger.get_loan(&id).unwrap().collateral_locked, 15_000);

    // Default with nothing repaid: outstanding 10_000 < locked 15_000.
    s.env.ledger().set_timestamp(31 * DAY);
    s.engine.flag_overdue(&id);
    s.env.ledger().set_timestamp(31 * DAY + 15 * DAY);
    let forfeited = s.engine.liquidate(&id);

    assert_eq!(forfeited, 10_000);
    assert_eq!(s.usdc.balance(&s.settlement), 10_000);
    // The 5_000 of over-collateralisation is returned, not seized.
    assert_eq!(s.vault.get_available(&s.guarantor), 490_000);
}

#[test]
fn test_cranks_are_permissionless_but_state_driven() {
    let s = setup();
    let id = s
        .ledger
        .originate(&s.guarantor, &s.beneficiary, &10_000, &4, &(30 * DAY));

    // Anyone can call the crank; the loan's own state decides what happens.
    s.env.ledger().set_timestamp(31 * DAY);
    s.engine.flag_overdue(&id);

    // Flagging twice is refused — the loan is already in grace.
    assert!(s.engine.try_flag_overdue(&id).is_err());

    // Paying during grace clears it, and the crank goes quiet again.
    s.ledger.attest_repayment(&s.partner, &id, &2_500);
    assert!(!s.engine.poke(&id));

    // Liquidating a healthy loan is refused.
    assert!(s.engine.try_liquidate(&id).is_err());
    assert_eq!(s.usdc.balance(&s.settlement), 0);
}
