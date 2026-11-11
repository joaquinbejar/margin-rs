//! Closeout of inverse futures and perpetuals, read from the ledger and
//! booked back into it (#26, part C).
//!
//! BTC at 8 decimals; CV = 100 (USD), prices and marks in USD per BTC; the
//! rule `inverse-im/1` (10 % / 5 % of the notional in BTC). Expected values
//! are written out by hand and checked against exact fractions.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::ledger::{Action, Execution, Ledger};
use margin_rs::liquidation::{
    CloseoutOutcome, CloseoutTarget, LiquidationAccount, LiquidationPlan, LiquidationPolicy,
    LiquidityLevel, plan_liquidation,
};
use margin_rs::rules::TieredInverseIm;
use margin_rs::{
    AccountId, Asset, Decimal, ExecutionId, InstrumentId, InverseFuture, InversePerpetual,
    InverseTerms, MarginError, TransferId, VenueId,
};
use rust_decimal_macros::dec;

fn btc() -> Asset {
    Asset::new("BTC").unwrap()
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}

fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).unwrap()
}

fn rule() -> TieredInverseIm {
    TieredInverseIm::inverse_v1(btc(), 8).unwrap()
}

fn terms(id: &str) -> InverseTerms {
    InverseTerms::new(iid(id), btc(), dec!(100), 1, 0).unwrap()
}

/// 0.03 BTC with `open` 100 BTC-INV contracts at 50000.
fn ledger(open: Action) -> Ledger {
    let mut ledger = Ledger::default();
    ledger.register_asset(btc(), 8).unwrap();
    ledger
        .register_instrument(InversePerpetual::new(terms("BTC-INV")))
        .unwrap();
    ledger
        .register_instrument(InverseFuture::new(terms("BTC-DEC"), 1000))
        .unwrap();
    ledger
        .deposit(TransferId::new("d").unwrap(), btc(), dec!(0.03))
        .unwrap();
    ledger
        .book_execution_allowing_overdraft(
            &Execution::new(eid("open"), iid("BTC-INV"), open, dec!(50000), dec!(100))
                .with_executed_at(1),
            None,
        )
        .unwrap();
    ledger
}

fn from_ledger(ledger: &Ledger, mark: Decimal) -> LiquidationAccount {
    LiquidationAccount::from_ledger(
        &rule(),
        &ledger.snapshot(),
        AccountId::new("acct").unwrap(),
        VenueId::new("venue").unwrap(),
        &btc(),
        &BTreeMap::from([(iid("BTC-INV"), mark)]),
        10,
    )
    .unwrap()
}

fn levels(entries: &[(Decimal, Decimal)]) -> BTreeMap<InstrumentId, Vec<LiquidityLevel>> {
    BTreeMap::from([(
        iid("BTC-INV"),
        entries
            .iter()
            .map(|(price, contracts)| LiquidityLevel {
                price: *price,
                contracts: *contracts,
            })
            .collect(),
    )])
}

fn policy() -> LiquidationPolicy {
    LiquidationPolicy {
        target: CloseoutTarget::Maintenance,
        fee_rate: dec!(0.0005),
        fee_rates: BTreeMap::new(),
        contract_step: dec!(1),
    }
}

/// Books every forced fill as `liq-<i>`, then checks that the ledger holds
/// exactly the plan's `after`, reconciles, and reads back the same account.
fn book_and_compare(ledger: &mut Ledger, plan: &LiquidationPlan, mark: Decimal) {
    for (i, reduction) in plan.reductions.iter().enumerate() {
        let execution = reduction.execution(eid(&format!("liq-{i}")), btc(), 20);
        ledger
            .book_execution_allowing_overdraft(&execution, None)
            .unwrap();
    }
    assert_eq!(ledger.balance(&btc()).unwrap().total, plan.after.cash);
    let position = ledger.position(&iid("BTC-INV")).unwrap();
    let after = plan
        .after
        .positions
        .iter()
        .find(|p| p.instrument == iid("BTC-INV"))
        .map_or((dec!(0), dec!(0)), |p| (p.contracts, p.basis));
    assert_eq!((position.quantity, position.cost_basis), after);
    let realized: Vec<Decimal> = plan.reductions.iter().map(|r| r.realized_pnl).collect();
    let booked: Vec<Decimal> = ledger
        .journal()
        .iter()
        .filter(|c| c.execution_id.as_str().starts_with("liq-"))
        .map(|c| c.realized_pnl)
        .collect();
    assert_eq!(booked, realized);
    assert!(ledger.reconcile().unwrap().is_clean());
    let again = from_ledger(ledger, mark);
    let replanned = plan_liquidation(&rule(), &again, &BTreeMap::new(), &policy()).unwrap();
    assert_eq!(replanned.before, plan.after);
}

/// Long 100 at 50000 (basis 0.2), cash 0.03, mark 45000:
/// - value rhe(10000 / 45000) = 0.22222222, upnl -0.02222222, collateral
///   0.00777778 against mm 0.01111112 (0.05 * 0.2222... rounded up);
/// - bids 30 at 44900, then 100 at 44500. The first level is taken whole:
///   V = rhe(3000 / 44900) = 0.06681514, released 0.06, realized
///   -0.00681514, fee trunc(0.0005 * 3000 / 44900) = 0.0000334;
/// - on the second level 3 contracts meet the target: V = 0.00674157,
///   released 0.006, realized -0.00074157, fee 0.00000337;
/// - after: 67 contracts, basis 0.134, cash 0.02240652, collateral
///   0.00751763 >= mm 0.00744445.
#[test]
fn a_long_inverse_closeout_round_trips_through_the_ledger() {
    let mut ledger = ledger(Action::Buy);
    let account = from_ledger(&ledger, dec!(45000));
    let plan = plan_liquidation(
        &rule(),
        &account,
        &levels(&[(dec!(44900), dec!(30)), (dec!(44500), dec!(100))]),
        &policy(),
    )
    .unwrap();
    assert_eq!(plan.before.collateral, dec!(0.00777778));
    assert_eq!(plan.before.maintenance, dec!(0.01111112));
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    let figures: Vec<_> = plan
        .reductions
        .iter()
        .map(|r| {
            (
                r.contracts,
                r.fill_cash,
                r.released_basis,
                r.realized_pnl,
                r.fee,
            )
        })
        .collect();
    assert_eq!(
        figures,
        vec![
            (
                dec!(30),
                dec!(0.06681514),
                dec!(0.06),
                dec!(-0.00681514),
                dec!(0.0000334)
            ),
            (
                dec!(3),
                dec!(0.00674157),
                dec!(0.006),
                dec!(-0.00074157),
                dec!(0.00000337)
            ),
        ]
    );
    assert_eq!(plan.after.cash, dec!(0.02240652));
    assert_eq!(plan.after.collateral, dec!(0.00751763));
    assert_eq!(plan.after.maintenance, dec!(0.00744445));
    book_and_compare(&mut ledger, &plan, dec!(45000));
    assert_eq!(
        ledger.position(&iid("BTC-INV")).unwrap().cost_basis,
        dec!(0.134)
    );
}

/// The mirror: short 100 at 50000 (basis -0.2), mark 57000 (value
/// rhe(0.1754385964...) = 0.1754386, upnl -0.0245614, collateral
/// 0.0054386 against mm 0.00877193), bought back at 57100 then 57500. The
/// plan meets the target and the ledger books it to the same state.
#[test]
fn a_short_inverse_closeout_round_trips_through_the_ledger() {
    let mut ledger = ledger(Action::Sell);
    let account = from_ledger(&ledger, dec!(57000));
    assert_eq!(account.basis[&iid("BTC-INV")], dec!(-0.2));
    let plan = plan_liquidation(
        &rule(),
        &account,
        &levels(&[(dec!(57100), dec!(30)), (dec!(57500), dec!(100))]),
        &policy(),
    )
    .unwrap();
    assert_eq!(plan.before.positions[0].unrealized_pnl, dec!(-0.0245614));
    assert_eq!(plan.before.maintenance, dec!(0.00877193));
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    assert_ne!(plan.reductions.len(), 0);
    assert!(plan.reductions.iter().all(|r| r.side == Action::Buy));
    book_and_compare(&mut ledger, &plan, dec!(57000));
}

/// An open inverse future past its expiry is settled, not liquidated.
#[test]
fn an_expired_inverse_future_is_refused() {
    let mut ledger = ledger(Action::Buy);
    ledger
        .book_execution_allowing_overdraft(
            &Execution::new(eid("f"), iid("BTC-DEC"), Action::Buy, dec!(50000), dec!(1))
                .with_executed_at(1),
            None,
        )
        .unwrap();
    let error = LiquidationAccount::from_ledger(
        &rule(),
        &ledger.snapshot(),
        AccountId::new("acct").unwrap(),
        VenueId::new("venue").unwrap(),
        &btc(),
        &BTreeMap::from([(iid("BTC-INV"), dec!(45000)), (iid("BTC-DEC"), dec!(45000))]),
        1000,
    )
    .unwrap_err();
    assert_eq!(
        error,
        MarginError::ExpiredUnsettled {
            instrument: iid("BTC-DEC"),
            expiry_ms: 1000,
            as_of_ms: 1000,
        }
    );
}
