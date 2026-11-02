//! Regressions for the closeout search (#26, part C): windows that only
//! rounding opens with a coarse cash quantum, breakpoints whose numerator
//! does not fit a `Decimal`, and the exact minimum near `Decimal::MAX`.
//!
//! "Single count" checks plan one count alone (a level of exactly that
//! many contracts, stepped by all of them), so they evaluate the target at
//! that count through the public API.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::{ExposurePosition, ExposureView, ProductKind, RuleId};
use margin_rs::ledger::{Execution, Ledger};
use margin_rs::liquidation::{
    CloseoutOutcome, CloseoutTarget, LiquidationAccount, LiquidationPlan, LiquidationPolicy,
    LiquidityLevel, plan_liquidation,
};
use margin_rs::rules::{MarginTier, TieredInverseIm, TieredLinearIm};
use margin_rs::{
    AccountId, Action, Asset, ContractSize, Decimal, ExecutionId, InstrumentId, InversePerpetual,
    InverseTerms, TransferId, VenueId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}

fn tiers(floor: Decimal, rates: [(Decimal, Decimal); 2]) -> Vec<MarginTier> {
    let [(im0, mm0), (im1, mm1)] = rates;
    vec![
        MarginTier {
            floor: dec!(0),
            initial_rate: im0,
            maintenance_rate: mm0,
        },
        MarginTier {
            floor,
            initial_rate: im1,
            maintenance_rate: mm1,
        },
    ]
}

fn one_tier() -> Vec<MarginTier> {
    vec![MarginTier {
        floor: dec!(0),
        initial_rate: dec!(0.10),
        maintenance_rate: dec!(0.05),
    }]
}

fn account(
    product: ProductKind,
    currency: &Asset,
    (contracts, size, mark): (Decimal, ContractSize, Decimal),
    basis: Decimal,
    cash: Decimal,
) -> LiquidationAccount {
    LiquidationAccount {
        view: ExposureView {
            account: AccountId::new("acct").unwrap(),
            venue: VenueId::new("venue").unwrap(),
            positions: vec![ExposurePosition {
                instrument: iid("A"),
                product,
                settlement: currency.clone(),
                contracts,
                size,
                mark,
            }],
        },
        basis: BTreeMap::from([(iid("A"), basis)]),
        cash,
        resting_orders: vec![],
    }
}

fn level(price: Decimal, contracts: Decimal) -> BTreeMap<InstrumentId, Vec<LiquidityLevel>> {
    BTreeMap::from([(iid("A"), vec![LiquidityLevel { price, contracts }])])
}

fn policy(target: CloseoutTarget, step: Decimal) -> LiquidationPolicy {
    LiquidationPolicy {
        target,
        fee_rate: dec!(0),
        contract_step: step,
    }
}

/// True when closing exactly `n` contracts at `price` meets the target.
fn single_count<M: margin_rs::initial_margin::PiecewiseLinearMargin>(
    rule: &M,
    account: &LiquidationAccount,
    price: Decimal,
    target: CloseoutTarget,
    n: Decimal,
) -> bool {
    let plan = plan_liquidation(rule, account, &level(price, n), &policy(target, n)).unwrap();
    plan.outcome == CloseoutOutcome::TargetMet && plan.reductions[0].contracts == n
}

/// Every count from 1 to `last` (step 1), the first that meets the
/// target.
fn brute_force<M: margin_rs::initial_margin::PiecewiseLinearMargin>(
    rule: &M,
    account: &LiquidationAccount,
    price: Decimal,
    target: CloseoutTarget,
    last: u32,
) -> Option<Decimal> {
    (1..=last)
        .map(Decimal::from)
        .find(|n| single_count(rule, account, price, target, *n))
}

/// The inverse counterexample: BTC in whole units, 13 long with CV 1
/// bought at 1.13 (basis rhe(11.50...) = 12), mark 0.97, cash 2, sold at
/// 0.91, initial 10 %. The unrounded figures never meet the target on
/// this level, but closing 4 does once rounded: value rhe(4 / 0.91) = 4,
/// released rhe(12 * 4 / 13) = 4, cash 2, basis 8, the 9 left worth
/// rhe(9 / 0.97) = 9, collateral 1 against im ceil(0.928) = 1. 6 does
/// not, 13 does again.
#[test]
fn a_rounding_window_is_found_inverse() {
    let rule = TieredInverseIm::new(RuleId::new("coarse", 1), asset("BTC"), 0, one_tier()).unwrap();
    let account = account(
        ProductKind::InversePerpetual,
        &asset("BTC"),
        (dec!(13), ContractSize::ContractValue(dec!(1)), dec!(0.97)),
        dec!(12),
        dec!(2),
    );
    let plan = plan_liquidation(
        &rule,
        &account,
        &level(dec!(0.91), dec!(100)),
        &policy(CloseoutTarget::Initial, dec!(1)),
    )
    .unwrap();
    assert!(plan.minimal);
    assert_eq!(plan.reductions[0].contracts, dec!(4));
    assert_eq!(plan.after.cash, dec!(2));
    assert_eq!(plan.after.collateral, dec!(1));
    assert_eq!(plan.after.initial, dec!(1));
    assert_eq!(
        brute_force(&rule, &account, dec!(0.91), CloseoutTarget::Initial, 13),
        Some(dec!(4))
    );
}

/// The linear mirror: USD in whole units, 13 long bought at 1.13 (basis
/// rhe(14.69) = 15), mark 0.97, cash 3, sold at 0.91. Rounding lets 3, 4,
/// 5 and 13 meet the target; the plan closes 3: value rhe(2.73) = 3,
/// released rhe(3.46) = 3, cash 3, collateral 3 + rhe(9.7) - 12 = 1
/// against im ceil(0.97) = 1.
#[test]
fn a_rounding_window_is_found_linear() {
    let rule = TieredLinearIm::new(RuleId::new("coarse", 1), asset("USD"), 0, one_tier()).unwrap();
    let account = account(
        ProductKind::LinearPerpetual,
        &asset("USD"),
        (dec!(13), ContractSize::Multiplier(dec!(1)), dec!(0.97)),
        dec!(15),
        dec!(3),
    );
    let plan = plan_liquidation(
        &rule,
        &account,
        &level(dec!(0.91), dec!(100)),
        &policy(CloseoutTarget::Initial, dec!(1)),
    )
    .unwrap();
    assert!(plan.minimal);
    assert_eq!(plan.reductions[0].contracts, dec!(3));
    assert_eq!(
        (plan.after.cash, plan.after.collateral, plan.after.initial),
        (dec!(3), dec!(1), dec!(1))
    );
    assert_eq!(
        brute_force(&rule, &account, dec!(0.91), CloseoutTarget::Initial, 13),
        Some(dec!(3))
    );
}

/// A tier floor crossed by a notional whose numerator `|Qc| * CV - floor *
/// mark` does not fit a Decimal: 1e27 contracts of CV 100 at mark 1e9 are
/// worth 1e20 BTC (two tiers from 1e19, maintenance 2.5 % / 5 %), cash
/// 1e18. The plan works and its count is the smallest: one fewer does not
/// meet the target.
#[test]
fn a_wide_breakpoint_numerator_inverse() {
    let rule = TieredInverseIm::new(
        RuleId::new("wide", 1),
        asset("BTC"),
        8,
        tiers(
            dec!(10000000000000000000),
            [(dec!(0.05), dec!(0.025)), (dec!(0.10), dec!(0.05))],
        ),
    )
    .unwrap();
    let account = account(
        ProductKind::InversePerpetual,
        &asset("BTC"),
        (
            dec!(1000000000000000000000000000),
            ContractSize::ContractValue(dec!(100)),
            dec!(1000000000),
        ),
        dec!(100000000000000000000),
        dec!(1000000000000000000),
    );
    let plan = plan_liquidation(
        &rule,
        &account,
        &level(dec!(1000000000), dec!(1000000000000000000000000000)),
        &policy(CloseoutTarget::Maintenance, dec!(1)),
    )
    .unwrap();
    assert!(plan.minimal);
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    let taken = plan.reductions[0].contracts;
    assert!(single_count(
        &rule,
        &account,
        dec!(1000000000),
        CloseoutTarget::Maintenance,
        taken
    ));
    assert!(!single_count(
        &rule,
        &account,
        dec!(1000000000),
        CloseoutTarget::Maintenance,
        taken - dec!(1)
    ));
}

/// A breakpoint numerator whose scale does not fit: a mark of 50000 / 3
/// (24 decimals) against a floor of 1e-8. 1000 contracts of CV 100 are
/// worth 6 BTC; cash 0.001, sold at 16666.67. The plan equals the brute
/// force over every count, and booking it leaves the ledger at its
/// `after`.
#[test]
fn a_breakpoint_numerator_beyond_scale_28_inverse() {
    let rule = TieredInverseIm::new(
        RuleId::new("fine", 1),
        asset("BTC"),
        8,
        tiers(
            dec!(0.00000001),
            [(dec!(0.05), dec!(0.025)), (dec!(0.10), dec!(0.05))],
        ),
    )
    .unwrap();
    let mark = dec!(50000) / dec!(3);
    assert!(mark.scale() > 20);
    // Opened at 16666.67: basis rhe(100000 / 16666.67) = rhe(5.9999988000...) = 5.9999988.
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("BTC"), 8).unwrap();
    ledger
        .register_instrument(InversePerpetual::new(
            InverseTerms::new(iid("A"), asset("BTC"), dec!(100), 2, 0).unwrap(),
        ))
        .unwrap();
    ledger
        .deposit(TransferId::new("d").unwrap(), asset("BTC"), dec!(0.001))
        .unwrap();
    ledger
        .book_execution_allowing_overdraft(
            &Execution::new(
                ExecutionId::new("open").unwrap(),
                iid("A"),
                Action::Buy,
                dec!(16666.67),
                dec!(1000),
            )
            .with_executed_at(1),
            None,
        )
        .unwrap();
    let basis = ledger.position(&iid("A")).unwrap().cost_basis;
    assert_eq!(basis, dec!(5.9999988));
    let account = account(
        ProductKind::InversePerpetual,
        &asset("BTC"),
        (dec!(1000), ContractSize::ContractValue(dec!(100)), mark),
        basis,
        dec!(0.001),
    );
    let plan = plan_liquidation(
        &rule,
        &account,
        &level(dec!(16666.67), dec!(1000)),
        &policy(CloseoutTarget::Maintenance, dec!(1)),
    )
    .unwrap();
    assert!(plan.minimal);
    let taken = plan.reductions[0].contracts;
    assert_eq!(
        brute_force(
            &rule,
            &account,
            dec!(16666.67),
            CloseoutTarget::Maintenance,
            1000
        ),
        Some(taken)
    );
    book_and_compare(&mut ledger, &plan);
}

fn book_and_compare(ledger: &mut Ledger, plan: &LiquidationPlan) {
    for (i, reduction) in plan.reductions.iter().enumerate() {
        let execution = reduction.execution(
            ExecutionId::new(format!("liq-{i}")).unwrap(),
            asset("BTC"),
            2,
        );
        ledger
            .book_execution_allowing_overdraft(&execution, None)
            .unwrap();
    }
    assert_eq!(
        ledger.balance(&asset("BTC")).unwrap().total,
        plan.after.cash
    );
    let position = ledger.position(&iid("A")).unwrap();
    let after = plan
        .after
        .positions
        .first()
        .map_or((dec!(0), dec!(0)), |p| (p.contracts, p.basis));
    assert_eq!((position.quantity, position.cost_basis), after);
    assert!(ledger.reconcile().unwrap().is_clean());
}

/// The linear form of the same: |Qc| * M * mark needs 6 + 24 decimals
/// (1001 contracts, so no trailing zero shortens it). 1001 contracts of M
/// 0.000001 marked at 50000 / 3 are worth 16.68... USD (scale 6), two
/// tiers from 0.000001, basis 17, cash 0.5, sold at 16666.66; the plan
/// equals the brute force.
#[test]
fn a_breakpoint_numerator_beyond_scale_28_linear() {
    let rule = TieredLinearIm::new(
        RuleId::new("fine", 1),
        asset("USD"),
        6,
        tiers(
            dec!(0.000001),
            [(dec!(0.05), dec!(0.025)), (dec!(0.10), dec!(0.05))],
        ),
    )
    .unwrap();
    let account = account(
        ProductKind::LinearPerpetual,
        &asset("USD"),
        (
            dec!(1001),
            ContractSize::Multiplier(dec!(0.000001)),
            dec!(50000) / dec!(3),
        ),
        dec!(17),
        dec!(0.5),
    );
    let plan = plan_liquidation(
        &rule,
        &account,
        &level(dec!(16666.66), dec!(1001)),
        &policy(CloseoutTarget::Maintenance, dec!(1)),
    )
    .unwrap();
    assert!(plan.minimal);
    assert_eq!(
        brute_force(
            &rule,
            &account,
            dec!(16666.66),
            CloseoutTarget::Maintenance,
            1001
        ),
        Some(plan.reductions[0].contracts)
    );
}

/// Near `Decimal::MAX` with a coarse step the minimum is exact: MAX
/// contracts marked at 1e-18 (notional 79228162514.264338, basis equal),
/// mm 5 %, cash 2e9, stepped by 1e25 contracts (1e7 of value per step,
/// far above the 1e-6 quantum). One step fewer does not meet the target.
#[test]
fn near_decimal_max_a_coarse_step_is_exact() {
    let rule = TieredLinearIm::new(RuleId::new("max", 1), asset("USD"), 6, one_tier()).unwrap();
    let tiny = dec!(0.000000000000000001);
    let account = account(
        ProductKind::LinearPerpetual,
        &asset("USD"),
        (Decimal::MAX, ContractSize::Multiplier(dec!(1)), tiny),
        dec!(79228162514.264338),
        dec!(2000000000),
    );
    let step = dec!(10000000000000000000000000);
    let plan = plan_liquidation(
        &rule,
        &account,
        &level(tiny, Decimal::MAX),
        &policy(CloseoutTarget::Maintenance, step),
    )
    .unwrap();
    assert!(plan.minimal);
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    let taken = plan.reductions[0].contracts;
    assert!(single_count(
        &rule,
        &account,
        tiny,
        CloseoutTarget::Maintenance,
        taken
    ));
    assert!(!single_count(
        &rule,
        &account,
        tiny,
        CloseoutTarget::Maintenance,
        taken - step
    ));
}

/// `Decimal::MAX` contracts at a step of 1e-26: the counts that can be
/// booked (whole contracts, since `MAX - n` must fit) are 1e26 steps
/// apart, and the search jumps to them arithmetically instead of walking
/// the counts in between (which once took about 1.3e25 iterations).
///
/// - Long, marked at 1e-28 (worth 8 after rounding, basis 8), cash 0,
///   initial margin 1, sold at 1: one whole contract, leaving cash and
///   collateral of 1, is the smallest count that meets the target, with a
///   level of either 100 or 2.5.
/// - The short mirror, bought back at 1e-18: no count of the 100 meets the
///   target, and the plan takes the whole level.
#[test]
fn counts_that_cannot_be_booked_are_jumped_over() {
    let rule = TieredLinearIm::new(RuleId::new("coarse", 1), asset("USD"), 0, one_tier()).unwrap();
    let tiny = dec!(0.0000000000000000000000000001);
    let step = dec!(0.00000000000000000000000001);
    let cases = [
        (Decimal::MAX, dec!(8), dec!(1), dec!(100), dec!(1)),
        (Decimal::MAX, dec!(8), dec!(1), dec!(2.5), dec!(1)),
        (
            -Decimal::MAX,
            dec!(-8),
            dec!(0.000000000000000001),
            dec!(100),
            dec!(100),
        ),
    ];
    for (contracts, basis, price, cap, taken) in cases {
        let account = account(
            ProductKind::LinearPerpetual,
            &asset("USD"),
            (contracts, ContractSize::Multiplier(dec!(1)), tiny),
            basis,
            dec!(0),
        );
        let started = std::time::Instant::now();
        let plan = plan_liquidation(
            &rule,
            &account,
            &level(price, cap),
            &policy(CloseoutTarget::Initial, step),
        )
        .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "{:?}",
            started.elapsed()
        );
        assert!(plan.minimal);
        assert_eq!(plan.reductions[0].contracts, taken);
        let expected = if contracts > dec!(0) {
            CloseoutOutcome::TargetMet
        } else {
            CloseoutOutcome::LiquidityExhausted
        };
        assert_eq!(plan.outcome, expected);
    }
}
