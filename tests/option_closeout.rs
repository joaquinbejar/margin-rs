//! Closeout plans over linear and option positions, booked back into the
//! ledger.
//!
//! Model: `linear-im` (im 10 %, mm 5 %) for BTC-PERP and
//! `short-option-im/1` (a 15 %, b 10 %, m 7.5 %) for options, underlying
//! at 100, USDC at 2 decimals, no fee.
//!
//! Fixture: deposit 100, sell 10 CALL (strike 100) at 5 (+50), buy 4 PUT
//! at 3 (-12), buy 5 BTC-PERP at 100: cash 138. Marks: BTC-PERP 98, CALL
//! 6, PUT 2. Collateral counts the perp's upnl (490 - 500 = -10) and
//! `min(value, 0)` of each option (CALL -60, PUT 0): 68. Maintenance:
//! perp 24.5, CALL 10 * 0.075 * 100 = 75, PUT 0: 99.5. Initial: perp 49,
//! CALL 10 * max(15 - 0, 10) = 150: 199.
//!
//! Buying back `n` CALL at 6.5 debits 6.5n of premium and lifts the CALL's
//! value from -60 to -6(10 - n): collateral 68 - 0.5n against a
//! maintenance of 99.5 - 7.5n (n >= 4.5, so 5) and an initial of
//! 199 - 15n (n >= 9.03, so 10).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::RuleId;
use margin_rs::instrument::{
    ExerciseStyle, LinearPerpetual, LinearTerms, OptionContract, OptionDelivery, OptionInstrument,
    OptionMargining, OptionRight,
};
use margin_rs::ledger::{Action, Execution, Ledger};
use margin_rs::liquidation::{
    CloseoutOutcome, CloseoutTarget, LiquidationAccount, LiquidationPlan, LiquidationPolicy,
    LiquidityLevel, plan_liquidation,
};
use margin_rs::rules::{LinearWithOptionsIm, MarginTier, ShortOptionIm, TieredLinearIm};
use margin_rs::{AccountId, Asset, Decimal, ExecutionId, InstrumentId, TransferId, VenueId};
use rust_decimal_macros::dec;

fn usdc() -> Asset {
    Asset::new("USDC").expect("asset")
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).expect("id")
}

fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).expect("id")
}

fn option(id: &str, right: OptionRight) -> OptionInstrument {
    OptionInstrument::new(
        LinearTerms::new(iid(id), usdc(), Decimal::ONE, 2, 0).expect("terms"),
        OptionContract {
            underlying: Asset::new("BTC").expect("asset"),
            strike: dec!(100),
            expiry_ms: 10_000,
            right,
            exercise: ExerciseStyle::European,
            delivery: OptionDelivery::Cash,
            margining: OptionMargining::Linear,
        },
    )
    .expect("option")
}

fn model() -> LinearWithOptionsIm<TieredLinearIm> {
    let linear = TieredLinearIm::new(
        RuleId::new("linear-im", 1),
        usdc(),
        2,
        vec![MarginTier {
            floor: Decimal::ZERO,
            initial_rate: dec!(0.10),
            maintenance_rate: dec!(0.05),
        }],
    )
    .expect("rule");
    LinearWithOptionsIm::new(
        RuleId::new("linear-with-options", 1),
        linear,
        ShortOptionIm::short_option_v1(usdc(), 2).expect("rule"),
        vec![
            (option("CALL", OptionRight::Call), dec!(100)),
            (option("PUT", OptionRight::Put), dec!(100)),
        ],
    )
    .expect("model")
}

fn book(ledger: &mut Ledger, execution: &Execution) {
    ledger
        .book_execution_allowing_overdraft(execution, None)
        .expect("booked");
}

fn fixture() -> Ledger {
    let mut ledger = Ledger::default();
    ledger.register_asset(usdc(), 2).expect("asset");
    ledger
        .register_instrument(LinearPerpetual::new(
            LinearTerms::new(iid("BTC-PERP"), usdc(), Decimal::ONE, 2, 0).expect("terms"),
        ))
        .expect("perp");
    for (id, right) in [("CALL", OptionRight::Call), ("PUT", OptionRight::Put)] {
        ledger
            .register_instrument(option(id, right))
            .expect("option");
    }
    ledger
        .deposit(TransferId::new("d1").expect("id"), usdc(), dec!(100))
        .expect("deposit");
    for (id, instrument, side, price, contracts) in [
        ("o1", "CALL", Action::Sell, dec!(5), dec!(10)),
        ("o2", "PUT", Action::Buy, dec!(3), dec!(4)),
        ("o3", "BTC-PERP", Action::Buy, dec!(100), dec!(5)),
    ] {
        book(
            &mut ledger,
            &Execution::new(eid(id), iid(instrument), side, price, contracts).with_executed_at(500),
        );
    }
    ledger
}

fn marks() -> BTreeMap<InstrumentId, Decimal> {
    BTreeMap::from([
        (iid("BTC-PERP"), dec!(98)),
        (iid("CALL"), dec!(6)),
        (iid("PUT"), dec!(2)),
    ])
}

fn account(ledger: &Ledger) -> LiquidationAccount {
    LiquidationAccount::from_ledger(
        &model(),
        &ledger.snapshot(),
        AccountId::new("acct").expect("id"),
        VenueId::new("venue").expect("id"),
        &usdc(),
        &marks(),
        600,
    )
    .expect("account")
}

fn book_of() -> BTreeMap<InstrumentId, Vec<LiquidityLevel>> {
    let level = |price, contracts| LiquidityLevel { price, contracts };
    BTreeMap::from([
        (iid("CALL"), vec![level(dec!(6.5), dec!(100))]),
        (iid("BTC-PERP"), vec![level(dec!(97), dec!(100))]),
        (iid("PUT"), vec![level(dec!(1.5), dec!(100))]),
    ])
}

fn plan(target: CloseoutTarget) -> LiquidationPlan {
    plan_liquidation(
        &model(),
        &account(&fixture()),
        &book_of(),
        &LiquidationPolicy {
            target,
            fee_rate: Decimal::ZERO,
            fee_rates: BTreeMap::new(),
            loss_budget: None,
            contract_step: Decimal::ONE,
        },
    )
    .expect("plan")
}

/// Books the plan's fills and checks that the ledger, read back as an
/// account, is the plan's projected state.
fn assert_parity(plan: &LiquidationPlan) {
    let mut ledger = fixture();
    for (i, reduction) in plan.reductions.iter().enumerate() {
        book(
            &mut ledger,
            &reduction.execution(eid(&format!("liq-{i}")), usdc(), 600),
        );
    }
    assert_eq!(
        ledger.balance(&usdc()).expect("balance").total,
        plan.after.cash
    );
    let replanned = plan_liquidation(
        &model(),
        &account(&ledger),
        &BTreeMap::new(),
        &LiquidationPolicy {
            target: plan.target,
            fee_rate: Decimal::ZERO,
            fee_rates: BTreeMap::new(),
            loss_budget: None,
            contract_step: Decimal::ONE,
        },
    )
    .expect("replan");
    assert_eq!(replanned.before, plan.after);
}

#[test]
fn test_fixture_summary() {
    let plan = plan(CloseoutTarget::Maintenance);
    assert_eq!(plan.before.cash, dec!(138));
    assert_eq!(plan.before.collateral, dec!(68));
    assert_eq!(plan.before.maintenance, dec!(99.5));
    assert_eq!(plan.before.initial, dec!(199));
}

/// Maintenance: 5 CALL bought back at 6.5 (-32.5): cash 105.5, collateral
/// 65.5 against 62.
#[test]
fn test_maintenance_buys_back_the_short_option() {
    let plan = plan(CloseoutTarget::Maintenance);
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    assert_eq!(plan.reductions.len(), 1);
    let call = &plan.reductions[0];
    assert_eq!(
        (&call.instrument, call.side, call.contracts),
        (&iid("CALL"), Action::Buy, dec!(5))
    );
    assert_eq!(call.fill_cash, dec!(-32.5));
    assert_eq!(plan.after.cash, dec!(105.5));
    assert_eq!(plan.after.collateral, dec!(65.5));
    assert_eq!(plan.after.maintenance, dec!(62));
    assert_parity(&plan);
}

/// Initial: all 10 CALL (-65): cash 73, collateral 63 against 49.
#[test]
fn test_initial_closes_the_short_option() {
    let plan = plan(CloseoutTarget::Initial);
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    assert_eq!(plan.reductions[0].contracts, dec!(10));
    assert_eq!(plan.after.cash, dec!(73));
    assert_eq!(plan.after.collateral, dec!(63));
    assert_eq!(plan.after.initial, dec!(49));
    assert_parity(&plan);
}

/// Flat: CALL bought back (-65), BTC-PERP sold at 97 (realized -15), PUT
/// sold at 1.5 (+6): cash 138 - 65 - 15 + 6 = 64, nothing open.
#[test]
fn test_flat_closes_every_position_and_credits_the_long_premium() {
    let plan = plan(CloseoutTarget::Flat);
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    let fills: Vec<(InstrumentId, Action, Decimal)> = plan
        .reductions
        .iter()
        .map(|r| (r.instrument.clone(), r.side, r.fill_cash))
        .collect();
    assert_eq!(
        fills,
        vec![
            (iid("CALL"), Action::Buy, dec!(-65)),
            (iid("BTC-PERP"), Action::Sell, dec!(485)),
            (iid("PUT"), Action::Sell, dec!(6)),
        ]
    );
    assert_eq!(plan.after.cash, dec!(64));
    assert_eq!(plan.after.positions, vec![]);
    assert_parity(&plan);
}

/// A premium mark of zero is valid; a negative one is not.
#[test]
fn test_option_premium_mark_may_be_zero() {
    let at = |put_mark| {
        let mut marks = marks();
        marks.insert(iid("PUT"), put_mark);
        LiquidationAccount::from_ledger(
            &model(),
            &fixture().snapshot(),
            AccountId::new("acct").expect("id"),
            VenueId::new("venue").expect("id"),
            &usdc(),
            &marks,
            600,
        )
    };
    assert!(at(Decimal::ZERO).is_ok());
    assert!(matches!(
        at(dec!(-1)),
        Err(margin_rs::MarginError::InvalidInput {
            field: "instrument mark",
            ..
        })
    ));
}
