#![cfg(test)]

use super::*;
use rc_guarantor_vault::{GuarantorVaultContract, GuarantorVaultContractClient};
use rc_loan_ledger::{Config, LoanLedgerContract, LoanLedgerContractClient, LoanStatus};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, BytesN, Env,
};

const DAY: u64 = 86_400;
const TIMELOCK: u64 = 2 * DAY;

struct Setup<'a> {
    env: Env,
    engine: LiquidationEngineContractClient<'a>,
    ledger: LoanLedgerContractClient<'a>,
    vault: GuarantorVaultContractClient<'a>,
    usdc: token::Client<'a>,
    partner: Address,
    verifier: Address,
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
    let verifier = Address::generate(&env);
    let guarantor = Address::generate(&env);
    let settlement = Address::generate(&env);
    let beneficiary = BytesN::from_array(&env, &[3u8; 32]);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = token::Client::new(&env, &sac.address());
    token::StellarAssetClient::new(&env, &sac.address()).mint(&guarantor, &1_000_000);

    let vault_id = env.register(
        GuarantorVaultContract,
        (admin.clone(), sac.address(), settlement.clone(), TIMELOCK),
    );
    let vault = GuarantorVaultContractClient::new(&env, &vault_id);

    // base 150%, floor 110%, 5% safety buffer, 14-day grace
    let ledger_id = env.register(
        LoanLedgerContract,
        (
            admin.clone(),
            vault_id.clone(),
            Config {
                base_ltv_bps: 15_000,
                min_ltv_bps: 11_000,
                safety_buffer_bps: 500,
                grace_period_secs: 14 * DAY,
            },
            TIMELOCK,
        ),
    );
    let ledger = LoanLedgerContractClient::new(&env, &ledger_id);

    let engine_id = env.register(
        LiquidationEngineContract,
        (admin.clone(), vault_id.clone(), ledger_id.clone(), TIMELOCK),
    );
    let engine = LiquidationEngineContractClient::new(&env, &engine_id);

    vault.set_loan_ledger(&admin, &ledger_id);
    vault.set_liquidation_engine(&admin, &engine_id);
    ledger.set_oracle(&admin, &oracle);
    ledger.set_liquidation_engine(&admin, &engine_id);
    ledger.set_partner(&admin, &partner, &true);
    ledger.set_verifier(&admin, &verifier, &true);

    vault.deposit(&guarantor, &500_000);

    Setup {
        env,
        engine,
        ledger,
        vault,
        usdc,
        partner,
        verifier,
        settlement,
        guarantor,
        beneficiary,
    }
}

#[test]
fn test_full_default_to_liquidation() {
    let s = setup();
    let id = s.ledger.originate(
        &s.guarantor,
        &s.beneficiary,
        &s.partner,
        &10_000,
        &4,
        &(30 * DAY),
    );
    // 150% LTV → 15_000 locked. One installment paid → 3_562 released, 11_438 left.
    s.ledger
        .attest_repayment(&s.partner, &s.verifier, &id, &2_500);

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
    let id = s.ledger.originate(
        &s.guarantor,
        &beneficiary,
        &s.partner,
        &10_000,
        &1,
        &(30 * DAY),
    );
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
    let id = s.ledger.originate(
        &s.guarantor,
        &s.beneficiary,
        &s.partner,
        &10_000,
        &4,
        &(30 * DAY),
    );

    // Anyone can call the crank; the loan's own state decides what happens.
    s.env.ledger().set_timestamp(31 * DAY);
    s.engine.flag_overdue(&id);

    // Flagging twice is refused — the loan is already in grace.
    assert!(s.engine.try_flag_overdue(&id).is_err());

    // Paying during grace clears it, and the crank goes quiet again.
    s.ledger
        .attest_repayment(&s.partner, &s.verifier, &id, &2_500);
    assert!(!s.engine.poke(&id));

    // Liquidating a healthy loan is refused.
    assert!(s.engine.try_liquidate(&id).is_err());
    assert_eq!(s.usdc.balance(&s.settlement), 0);
}

#[test]
fn test_upgrades_wait_out_the_timelock() {
    let s = setup();
    let admin = s.engine.get_admin();
    let stranger = Address::generate(&s.env);
    let not_authorized = soroban_sdk::Error::from(Error::NotAuthorized);
    let too_early = soroban_sdk::Error::from(Error::TimelockNotExpired);
    let upgrade = Action::Upgrade(BytesN::from_array(&s.env, &[0u8; 32]));
    assert_eq!(s.engine.get_timelock_secs(), TIMELOCK);

    assert!(matches!(
        s.engine.try_schedule_action(&stranger, &upgrade),
        Err(Ok(e)) if e == not_authorized
    ));
    s.engine.schedule_action(&admin, &upgrade);
    let eta = s.engine.get_scheduled_action().unwrap().eta;
    assert_eq!(eta, s.env.ledger().timestamp() + TIMELOCK);

    // Refused as too early while the timelock runs.
    s.env.ledger().set_timestamp(eta - 1);
    assert!(matches!(
        s.engine.try_execute_action(&admin),
        Err(Ok(e)) if e == too_early
    ));

    // Once it has elapsed the timelock no longer blocks it. (The host then
    // rejects this placeholder hash, which was never uploaded; a real upgrade
    // is exercised end to end by scripts/smoke-testnet.sh.)
    s.env.ledger().set_timestamp(eta);
    assert!(!matches!(
        s.engine.try_execute_action(&admin),
        Err(Ok(e)) if e == too_early
    ));

    s.engine.cancel_action(&admin);
    assert!(s.engine.get_scheduled_action().is_none());
}

#[test]
fn test_admin_handover_is_two_step() {
    let s = setup();
    let admin = s.engine.get_admin();
    let stranger = Address::generate(&s.env);
    let new_admin = Address::generate(&s.env);
    let not_authorized = soroban_sdk::Error::from(Error::NotAuthorized);
    let upgrade = Action::Upgrade(BytesN::from_array(&s.env, &[0u8; 32]));

    s.engine.propose_admin(&admin, &new_admin);
    assert_eq!(s.engine.get_admin(), admin);
    assert!(s.engine.try_accept_admin(&stranger).is_err());
    s.engine.accept_admin(&new_admin);
    assert_eq!(s.engine.get_admin(), new_admin);

    assert!(matches!(
        s.engine.try_schedule_action(&admin, &upgrade),
        Err(Ok(e)) if e == not_authorized
    ));
    s.engine.schedule_action(&new_admin, &upgrade);
}

#[test]
fn test_pause_and_unpause_circuit_breaker() {
    let s = setup();
    let admin = s.engine.get_admin();
    let stranger = Address::generate(&s.env);
    let paused_err = soroban_sdk::Error::from(Error::Paused);

    let id = s.ledger.originate(
        &s.guarantor,
        &s.beneficiary,
        &s.partner,
        &10_000,
        &4,
        &(30 * DAY),
    );

    // Initial state is unpaused
    assert!(!s.engine.is_paused());

    // Stranger cannot pause
    assert!(s.engine.try_pause(&stranger).is_err());

    // Admin pauses the circuit breaker
    s.engine.pause(&admin);
    assert!(s.engine.is_paused());

    // Miss installment to reach overdue
    s.env.ledger().set_timestamp(31 * DAY);

    // Crank poke returns false without executing when paused
    assert!(!s.engine.poke(&id));

    // flag_overdue fails with Paused error
    assert!(matches!(
        s.engine.try_flag_overdue(&id),
        Err(Ok(e)) if e == paused_err
    ));

    // Guarantor is still able to withdraw available collateral
    assert_eq!(s.vault.get_available(&s.guarantor), 500_000 - 15_000);

    // Stranger cannot unpause
    assert!(s.engine.try_unpause(&stranger).is_err());

    // Admin unpauses
    s.engine.unpause(&admin);
    assert!(!s.engine.is_paused());

    // Transition to grace via poke
    assert!(s.engine.poke(&id));
    assert!(matches!(
        s.ledger.get_loan(&id).unwrap().status,
        LoanStatus::Grace
    ));

    // Pause again when grace expires
    s.env.ledger().set_timestamp(46 * DAY);
    s.engine.pause(&admin);
    assert!(s.engine.is_paused());

    // Liquidate is blocked while paused
    assert!(matches!(
        s.engine.try_liquidate(&id),
        Err(Ok(e)) if e == paused_err
    ));

    // Unpause and liquidate succeeds
    s.engine.unpause(&admin);
    let forfeited = s.engine.liquidate(&id);
    assert_eq!(forfeited, 10_000);
}

#[test]
fn test_negative_liquidation_scenarios() {
    let s = setup();
    let not_overdue_err = soroban_sdk::Error::from(Error::NotOverdue);
    let grace_not_expired_err = soroban_sdk::Error::from(Error::GraceNotExpired);

    let id = s.ledger.originate(
        &s.guarantor,
        &s.beneficiary,
        &s.partner,
        &10_000,
        &4,
        &(30 * DAY),
    );

    // Scenario 1: Active loan that is current cannot be flagged overdue or liquidated
    assert!(matches!(
        s.engine.try_flag_overdue(&id),
        Err(Ok(e)) if e == not_overdue_err
    ));
    assert!(matches!(
        s.engine.try_liquidate(&id),
        Err(Ok(e)) if e == grace_not_expired_err
    ));

    // Scenario 2: Active loan overdue but NOT flagged as Grace cannot be liquidated directly
    s.env.ledger().set_timestamp(35 * DAY);
    // Loan is overdue, but status is still Active, so grace is not expired (it hasn't started)
    assert!(matches!(
        s.engine.try_liquidate(&id),
        Err(Ok(e)) if e == grace_not_expired_err
    ));

    // Flag overdue moves it to Grace
    s.engine.flag_overdue(&id);
    assert!(matches!(
        s.ledger.get_loan(&id).unwrap().status,
        LoanStatus::Grace
    ));

    // Cannot flag overdue again once already in Grace
    assert!(matches!(
        s.engine.try_flag_overdue(&id),
        Err(Ok(e)) if e == not_overdue_err
    ));

    // Scenario 3: Inside grace period (e.g. 5 days in, grace is 14 days), liquidation must be rejected
    s.env.ledger().set_timestamp(40 * DAY);
    assert!(matches!(
        s.engine.try_liquidate(&id),
        Err(Ok(e)) if e == grace_not_expired_err
    ));

    // Scenario 4: Loan fully repaid before grace expires cannot be liquidated
    s.ledger.attest_repayment(&s.partner, &s.verifier, &id, &10_000);
    assert!(matches!(
        s.ledger.get_loan(&id).unwrap().status,
        LoanStatus::Repaid
    ));

    // Advancing past grace expiry on a repaid loan still fails liquidation
    s.env.ledger().set_timestamp(60 * DAY);
    assert!(matches!(
        s.engine.try_liquidate(&id),
        Err(Ok(e)) if e == grace_not_expired_err
    ));
    assert!(matches!(
        s.engine.try_flag_overdue(&id),
        Err(Ok(e)) if e == not_overdue_err
    ));
    assert!(!s.engine.poke(&id));
}

#[test]
fn test_forfeiture_and_partial_repayment_precision() {
    let s = setup();
    // Test uneven, fractional amounts: e.g. principal 333, 3 installments of 111, uneven repayments
    let beneficiary = BytesN::from_array(&s.env, &[88u8; 32]);
    let id = s.ledger.originate(
        &s.guarantor,
        &beneficiary,
        &s.partner,
        &333,
        &3,
        &(30 * DAY),
    );

    let loan = s.ledger.get_loan(&id).unwrap();
    // 150% LTV of 333 = 499 (333 * 15000 / 10000)
    assert_eq!(loan.collateral_locked, 499);

    // Uneven partial repayment of 111
    let released = s.ledger.attest_repayment(&s.partner, &s.verifier, &id, &111);
    // earned_scaled = (499 * 111) * 9500 = 526,195,500; divided by (333 * 10000) = 526195500 / 3330000 = 158
    assert_eq!(released, 158);

    // Next due was advanced by 1 installment to 60 days.
    // Default after grace on the second installment:
    s.env.ledger().set_timestamp(61 * DAY);
    s.engine.flag_overdue(&id);
    s.env.ledger().set_timestamp(80 * DAY);

    let forfeited = s.engine.liquidate(&id);
    // Outstanding principal = 333 - 111 = 222
    // Remaining locked collateral = 499 - 158 = 341
    // Forfeited = min(222, 341) = 222
    assert_eq!(forfeited, 222);
    // Returned to guarantor = 341 - 222 = 119
    assert_eq!(s.vault.get_locked(&s.guarantor), 0);
}

#[test]
fn test_multi_guarantor_cascade_liquidation() {
    let s = setup();

    // Setup 3 distinct guarantors with deposits
    let g1 = s.guarantor.clone();
    let g2 = Address::generate(&s.env);
    let g3 = Address::generate(&s.env);

    // Fund g2 and g3
    let admin = s.engine.get_admin();
    let sac = s.vault.get_usdc_token();
    token::StellarAssetClient::new(&s.env, &sac).mint(&g2, &100_000);
    token::StellarAssetClient::new(&s.env, &sac).mint(&g3, &100_000);

    s.vault.deposit(&g2, &100_000);
    s.vault.deposit(&g3, &100_000);

    // Beneficiaries
    let b1 = BytesN::from_array(&s.env, &[11u8; 32]);
    let b2 = BytesN::from_array(&s.env, &[12u8; 32]);
    let b3 = BytesN::from_array(&s.env, &[13u8; 32]);
    let b4 = BytesN::from_array(&s.env, &[14u8; 32]);
    let b5 = BytesN::from_array(&s.env, &[15u8; 32]);

    // Originate 5 loans across 3 guarantors:
    // G1 has 2 loans: loan1 (repaying), loan2 (defaulting)
    let l1 = s.ledger.originate(&g1, &b1, &s.partner, &10_000, &4, &(30 * DAY));
    let l2 = s.ledger.originate(&g1, &b2, &s.partner, &20_000, &4, &(30 * DAY));

    // G2 has 2 loans: loan3 (fully paying), loan4 (defaulting)
    let l3 = s.ledger.originate(&g2, &b3, &s.partner, &15_000, &3, &(30 * DAY));
    let l4 = s.ledger.originate(&g2, &b4, &s.partner, &25_000, &2, &(30 * DAY));

    // G3 has 1 loan: loan5 (partial paying then defaulting)
    let l5 = s.ledger.originate(&g3, &b5, &s.partner, &30_000, &3, &(30 * DAY));

    // Initial balances and locks verified
    assert_eq!(s.vault.get_locked(&g1), 15_000 + 30_000); // 45_000 locked
    assert_eq!(s.vault.get_locked(&g2), 22_500 + 37_500); // 60_000 locked
    assert_eq!(s.vault.get_locked(&g3), 45_000);

    // Loan 1 and 3 repay
    s.ledger.attest_repayment(&s.partner, &s.verifier, &l1, &10_000);
    s.ledger.attest_repayment(&s.partner, &s.verifier, &l3, &15_000);
    // Loan 5 repays 1 installment
    s.ledger.attest_repayment(&s.partner, &s.verifier, &l5, &10_000);

    // Loan 1 and 3 are repaid and released their collateral completely
    assert!(matches!(s.ledger.get_loan(&l1).unwrap().status, LoanStatus::Repaid));
    assert!(matches!(s.ledger.get_loan(&l3).unwrap().status, LoanStatus::Repaid));

    // Advance time to trigger default on loan 2, 4, 5
    s.env.ledger().set_timestamp(35 * DAY);
    s.engine.flag_overdue(&l2);
    s.engine.flag_overdue(&l4);

    // Loan 5 was paid for first installment (due date = 60 days), so miss second installment at 65 days
    s.env.ledger().set_timestamp(65 * DAY);
    s.engine.flag_overdue(&l5);

    // Liquidate loans after grace
    s.env.ledger().set_timestamp(85 * DAY);
    let f2 = s.engine.liquidate(&l2);
    let f4 = s.engine.liquidate(&l4);
    let f5 = s.engine.liquidate(&l5);

    assert_eq!(f2, 20_000);
    assert_eq!(f4, 25_000);
    assert_eq!(f5, 20_000);

    // Invariant check: G1, G2, G3 balances are completely preserved without cross-contamination
    assert_eq!(s.vault.get_locked(&g1), 0);
    assert_eq!(s.vault.get_balance(&g1), 500_000 - 20_000);

    assert_eq!(s.vault.get_locked(&g2), 0);
    assert_eq!(s.vault.get_balance(&g2), 100_000 - 25_000);

    assert_eq!(s.vault.get_locked(&g3), 0);
    assert_eq!(s.vault.get_balance(&g3), 100_000 - 20_000);
}


