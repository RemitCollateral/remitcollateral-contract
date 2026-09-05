#![cfg(test)]

use super::*;
use rc_guarantor_vault::{GuarantorVaultContract, GuarantorVaultContractClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, Env,
};

const DAY: u64 = 86_400;

struct Setup<'a> {
    env: Env,
    ledger: LoanLedgerContractClient<'a>,
    vault: GuarantorVaultContractClient<'a>,
    admin: Address,
    oracle: Address,
    partner: Address,
    engine: Address,
    guarantor: Address,
    beneficiary: BytesN<32>,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let oracle = Address::generate(&env);
    let partner = Address::generate(&env);
    let engine = Address::generate(&env);
    let guarantor = Address::generate(&env);
    let settlement = Address::generate(&env);
    let beneficiary = BytesN::from_array(&env, &[7u8; 32]);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    token::StellarAssetClient::new(&env, &sac.address()).mint(&guarantor, &1_000_000);

    let vault_id = env.register(GuarantorVaultContract, ());
    let vault = GuarantorVaultContractClient::new(&env, &vault_id);
    vault.initialize(&admin, &sac.address(), &settlement);

    let ledger_id = env.register(LoanLedgerContract, ());
    let ledger = LoanLedgerContractClient::new(&env, &ledger_id);
    // base 150%, floor 110%, 5% safety buffer, 14-day grace
    ledger.initialize(&admin, &vault_id, &15_000, &11_000, &500, &(14 * DAY));

    vault.set_loan_ledger(&admin, &ledger_id);
    vault.set_liquidation_engine(&admin, &engine);
    ledger.set_oracle(&admin, &oracle);
    ledger.set_liquidation_engine(&admin, &engine);
    ledger.set_partner(&admin, &partner, &true);

    vault.deposit(&guarantor, &500_000);

    Setup {
        env,
        ledger,
        vault,
        admin,
        oracle,
        partner,
        engine,
        guarantor,
        beneficiary,
    }
}

#[test]
fn test_ltv_follows_reputation() {
    let s = setup();

    // No reputation pays the base rate.
    assert_eq!(s.ledger.required_ltv_bps(&s.beneficiary), 15_000);

    // A perfect score earns the floor, and never goes below it.
    s.ledger.set_reputation(&s.oracle, &s.beneficiary, &10_000);
    assert_eq!(s.ledger.required_ltv_bps(&s.beneficiary), 11_000);

    // Half a score earns half the reduction.
    s.ledger.set_reputation(&s.oracle, &s.beneficiary, &5_000);
    assert_eq!(s.ledger.required_ltv_bps(&s.beneficiary), 13_000);

    // Scores are bounded, and only the oracle may publish them.
    assert!(s
        .ledger
        .try_set_reputation(&s.oracle, &s.beneficiary, &10_001)
        .is_err());
    let stranger = Address::generate(&s.env);
    assert!(s
        .ledger
        .try_set_reputation(&stranger, &s.beneficiary, &1_000)
        .is_err());
}

#[test]
fn test_origination_locks_collateral_at_the_required_ltv() {
    let s = setup();

    let id = s
        .ledger
        .originate(&s.guarantor, &s.beneficiary, &10_000, &4, &(30 * DAY));
    assert_eq!(id, 1);

    let loan = s.ledger.get_loan(&id).unwrap();
    assert_eq!(loan.principal_usd, 10_000);
    assert_eq!(loan.ltv_bps, 15_000);
    assert_eq!(loan.collateral_locked, 15_000); // 10_000 * 150%
    assert_eq!(loan.installment_amount, 2_500);
    assert_eq!(loan.next_due, 30 * DAY);
    assert!(matches!(loan.status, LoanStatus::Active));

    assert_eq!(s.vault.get_locked(&s.guarantor), 15_000);
    assert_eq!(s.vault.get_available(&s.guarantor), 485_000);

    // One live loan per guarantor-beneficiary pair.
    assert!(s
        .ledger
        .try_originate(&s.guarantor, &s.beneficiary, &1_000, &2, &(30 * DAY))
        .is_err());

    // A better reputation locks less collateral for the same principal.
    let other = BytesN::from_array(&s.env, &[9u8; 32]);
    s.ledger.set_reputation(&s.oracle, &other, &10_000);
    let id2 = s
        .ledger
        .originate(&s.guarantor, &other, &10_000, &4, &(30 * DAY));
    assert_eq!(s.ledger.get_loan(&id2).unwrap().collateral_locked, 11_000);
}

#[test]
fn test_repayment_releases_collateral_proportionally() {
    let s = setup();
    let id = s
        .ledger
        .originate(&s.guarantor, &s.beneficiary, &10_000, &4, &(30 * DAY));

    // First installment: 25% repaid → 25% of collateral earned, less the 5% buffer.
    // 15_000 * 2_500/10_000 = 3_750; 3_750 * 95% = 3_562
    let released = s.ledger.attest_repayment(&s.partner, &id, &2_500);
    assert_eq!(released, 3_562);
    assert_eq!(s.vault.get_locked(&s.guarantor), 15_000 - 3_562);

    let loan = s.ledger.get_loan(&id).unwrap();
    assert_eq!(loan.total_repaid_usd, 2_500);
    assert_eq!(loan.installments_paid, 1);
    assert_eq!(loan.next_due, 60 * DAY);

    // Second and third keep releasing on the same curve.
    s.ledger.attest_repayment(&s.partner, &id, &2_500);
    assert_eq!(s.ledger.get_loan(&id).unwrap().collateral_released, 7_125);
    s.ledger.attest_repayment(&s.partner, &id, &2_500);
    assert_eq!(s.ledger.get_loan(&id).unwrap().collateral_released, 10_687);

    // Final installment closes the loan and returns everything, buffer included.
    s.ledger.attest_repayment(&s.partner, &id, &2_500);
    let loan = s.ledger.get_loan(&id).unwrap();
    assert!(matches!(loan.status, LoanStatus::Repaid));
    assert_eq!(loan.collateral_released, 15_000);
    assert_eq!(s.vault.get_locked(&s.guarantor), 0);
    assert_eq!(s.vault.get_available(&s.guarantor), 500_000);

    // The pair is free to borrow again once the loan closes.
    assert!(s
        .ledger
        .get_open_loan(&s.guarantor, &s.beneficiary)
        .is_none());
}

#[test]
fn test_only_authorized_partners_can_attest() {
    let s = setup();
    let id = s
        .ledger
        .originate(&s.guarantor, &s.beneficiary, &10_000, &4, &(30 * DAY));

    let impostor = Address::generate(&s.env);
    assert!(s
        .ledger
        .try_attest_repayment(&impostor, &id, &2_500)
        .is_err());
    // Not even the guarantor may claim their own beneficiary repaid.
    assert!(s
        .ledger
        .try_attest_repayment(&s.guarantor, &id, &2_500)
        .is_err());

    // Revoking a partner takes effect immediately.
    s.ledger.set_partner(&s.admin, &s.partner, &false);
    assert!(s
        .ledger
        .try_attest_repayment(&s.partner, &id, &2_500)
        .is_err());
    s.ledger.set_partner(&s.admin, &s.partner, &true);
    assert!(s
        .ledger
        .try_attest_repayment(&s.partner, &id, &2_500)
        .is_ok());

    // Repaying more than the principal is rejected.
    assert!(s
        .ledger
        .try_attest_repayment(&s.partner, &id, &99_999)
        .is_err());
}

#[test]
fn test_grace_period_transitions() {
    let s = setup();
    let id = s
        .ledger
        .originate(&s.guarantor, &s.beneficiary, &10_000, &4, &(30 * DAY));

    // Not yet due.
    assert!(!s.ledger.is_overdue(&id));
    assert!(s.ledger.try_mark_grace(&s.engine, &id).is_err());

    // Past the due date the loan is overdue and may enter grace.
    s.env.ledger().set_timestamp(31 * DAY);
    assert!(s.ledger.is_overdue(&id));
    s.ledger.mark_grace(&s.engine, &id);

    let loan = s.ledger.get_loan(&id).unwrap();
    assert!(matches!(loan.status, LoanStatus::Grace));
    assert_eq!(loan.grace_expires_at, 31 * DAY + 14 * DAY);
    assert!(!s.ledger.is_grace_expired(&id));

    // Only the liquidation engine may drive these transitions.
    let stranger = Address::generate(&s.env);
    assert!(s.ledger.try_mark_defaulted(&stranger, &id).is_err());

    // Paying during grace restores the loan to good standing.
    s.ledger.attest_repayment(&s.partner, &id, &2_500);
    let loan = s.ledger.get_loan(&id).unwrap();
    assert!(matches!(loan.status, LoanStatus::Active));
    assert_eq!(loan.grace_expires_at, 0);

    // Grace cannot be skipped: defaulting straight from Active is refused.
    s.env.ledger().set_timestamp(200 * DAY);
    assert!(s.ledger.try_mark_defaulted(&s.engine, &id).is_err());
}

#[test]
fn test_default_closes_the_loan() {
    let s = setup();
    let id = s
        .ledger
        .originate(&s.guarantor, &s.beneficiary, &10_000, &4, &(30 * DAY));
    s.ledger.attest_repayment(&s.partner, &id, &2_500);

    s.env.ledger().set_timestamp(61 * DAY);
    s.ledger.mark_grace(&s.engine, &id);

    // The grace period must actually expire first.
    assert!(s.ledger.try_mark_defaulted(&s.engine, &id).is_err());

    s.env.ledger().set_timestamp(61 * DAY + 15 * DAY);
    assert!(s.ledger.is_grace_expired(&id));
    assert_eq!(s.ledger.loan_outstanding(&id), 7_500);
    assert_eq!(s.ledger.loan_collateral_remaining(&id), 15_000 - 3_562);

    s.ledger.mark_defaulted(&s.engine, &id);
    let loan = s.ledger.get_loan(&id).unwrap();
    assert!(matches!(loan.status, LoanStatus::Defaulted));
    assert_eq!(s.ledger.loan_collateral_remaining(&id), 0);
    assert!(s
        .ledger
        .get_open_loan(&s.guarantor, &s.beneficiary)
        .is_none());

    // A defaulted loan accepts no further repayments.
    assert!(s
        .ledger
        .try_attest_repayment(&s.partner, &id, &1_000)
        .is_err());
}

#[test]
fn test_token_attestations_cannot_close_a_loan() {
    let s = setup();
    let id = s
        .ledger
        .originate(&s.guarantor, &s.beneficiary, &10_000, &4, &(30 * DAY));

    // A partner submitting one attestation per scheduled installment, each for a
    // trivial amount, must not close the loan or free the collateral. Only
    // principal actually repaid does that.
    for _ in 0..4 {
        s.ledger.attest_repayment(&s.partner, &id, &1);
    }

    let loan = s.ledger.get_loan(&id).unwrap();
    assert!(matches!(loan.status, LoanStatus::Active));
    assert_eq!(loan.total_repaid_usd, 4);
    assert_eq!(loan.installments_paid, 4);
    // 4 of 10_000 repaid earns essentially nothing back.
    assert_eq!(loan.collateral_released, 5);
    assert_eq!(s.vault.get_locked(&s.guarantor), 15_000 - 5);
    assert!(s
        .ledger
        .get_open_loan(&s.guarantor, &s.beneficiary)
        .is_some());

    // Repaying the rest closes it properly and returns everything.
    s.ledger.attest_repayment(&s.partner, &id, &9_996);
    let loan = s.ledger.get_loan(&id).unwrap();
    assert!(matches!(loan.status, LoanStatus::Repaid));
    assert_eq!(loan.collateral_released, 15_000);
    assert_eq!(s.vault.get_locked(&s.guarantor), 0);
}
