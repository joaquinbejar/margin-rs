//! Hand checked fixtures for account valuation and collateral with open
//! linear futures and perpetuals (M3-INT, part B).
//!
//! `upnl = rhe(Q * M * mark) - basis` per instrument, in its settlement
//! asset; collateral counts `cash + sum(upnl)` per settlement asset as one
//! net amount. Every expected value is written out by hand.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::collateral::{BalanceBasis, CollateralPolicy, StalePolicy, value_collateral};
use margin_rs::instrument::{LinearPerpetual, LinearTerms};
use margin_rs::ledger::{Action, Execution, FundingPayment, Ledger, LedgerConfig};
use margin_rs::maintenance::CollateralView;
use margin_rs::valuation::{Mark, MarkSet, MarkStatus, PriceSource, ValuationPolicy, value};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, Rounding, SpotInstrument, TransferId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}
fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}
fn perp(id: &str, settlement: &str, multiplier: Decimal) -> LinearPerpetual {
    LinearPerpetual::new(LinearTerms::new(iid(id), asset(settlement), multiplier, 2, 0).unwrap())
}

/// USD (reporting, 2 decimals), USDT (2 decimals) and BTC (8 decimals);
/// perpetuals PERP-A (USD, multiplier 1), PERP-B (USD, multiplier 0.1),
/// PERP-T (USDT, multiplier 1); spot BTC-USD.
fn ledger() -> Ledger {
    let mut ledger = Ledger::new(LedgerConfig {
        rounding: Rounding::HalfEven,
    });
    ledger.register_asset(asset("USD"), 2).unwrap();
    ledger.register_asset(asset("USDT"), 2).unwrap();
    ledger.register_asset(asset("BTC"), 8).unwrap();
    ledger
        .register_instrument(perp("PERP-A", "USD", dec!(1)))
        .unwrap();
    ledger
        .register_instrument(perp("PERP-B", "USD", dec!(0.1)))
        .unwrap();
    ledger
        .register_instrument(perp("PERP-T", "USDT", dec!(1)))
        .unwrap();
    ledger
        .register_instrument(
            SpotInstrument::new(iid("BTC-USD"), asset("BTC"), asset("USD"), 2, 8).unwrap(),
        )
        .unwrap();
    ledger
}

fn deposit(ledger: &mut Ledger, id: &str, code: &str, amount: Decimal) {
    ledger
        .deposit(TransferId::new(id).unwrap(), asset(code), amount)
        .unwrap();
}

fn trade(
    ledger: &mut Ledger,
    id: &str,
    instrument: &str,
    side: Action,
    price: Decimal,
    n: Decimal,
) {
    let execution = Execution::new(
        ExecutionId::new(id).unwrap(),
        iid(instrument),
        side,
        price,
        n,
    )
    .with_executed_at(1);
    ledger
        .book_execution_allowing_overdraft(&execution, None)
        .unwrap();
}

/// Marks at 10 000 ms; valid up to 1 000 ms old.
fn policy() -> ValuationPolicy {
    ValuationPolicy::new(asset("USD"), 1000)
}
fn mark(price: Decimal, at: u64) -> Mark {
    Mark::new(price, at, PriceSource::Mid)
}

/// Long 2 PERP-A at 100 (basis 200) on 100 USD, marked at 105: upnl 10.
fn long_gain() -> (Ledger, MarkSet) {
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USD", dec!(100));
    trade(&mut ledger, "e1", "PERP-A", Action::Buy, dec!(100), dec!(2));
    let marks = MarkSet::new(10_000).with_instrument(iid("PERP-A"), mark(dec!(105), 9_500));
    (ledger, marks)
}

#[test]
fn an_open_long_adds_its_unrealized_pnl_to_equity_and_collateral() {
    let (ledger, marks) = long_gain();
    let valuation = value(&ledger.snapshot(), &marks, &policy()).unwrap();
    let line = &valuation.derivatives[&iid("PERP-A")];
    assert_eq!(
        (line.market_value, line.unrealized_pnl),
        (dec!(210), dec!(10))
    );
    assert_eq!(line.status, MarkStatus::Valid);
    assert_eq!(valuation.equity, Some(dec!(110)));
    assert_eq!(valuation.collateral_equity, Some(dec!(110)));
    assert_eq!(valuation.net_pnl, Some(dec!(10)));
    let attribution = valuation.attribution.unwrap();
    assert_eq!(
        (
            attribution.realized,
            attribution.unrealized,
            attribution.fees
        ),
        (dec!(0), dec!(10), dec!(0))
    );
    assert!(valuation.is_clean() && valuation.instrument_flags.is_empty());
}

#[test]
fn gains_and_losses_in_one_asset_are_netted_with_the_cash() {
    // Plus short 3 PERP-B at 200 (M 0.1: basis -60), marked at 210:
    // rhe(-3 * 0.1 * 210) = -63, upnl -3. Net 100 + 10 - 3 = 107.
    let (mut ledger, marks) = long_gain();
    trade(
        &mut ledger,
        "e2",
        "PERP-B",
        Action::Sell,
        dec!(200),
        dec!(3),
    );
    let marks = marks.with_instrument(iid("PERP-B"), mark(dec!(210), 9_500));
    let valuation = value(&ledger.snapshot(), &marks, &policy()).unwrap();
    assert_eq!(
        valuation.derivatives[&iid("PERP-B")].unrealized_pnl,
        dec!(-3)
    );
    assert_eq!(valuation.equity, Some(dec!(107)));
    assert_eq!(valuation.collateral_equity, Some(dec!(107)));
    assert_eq!(valuation.attribution.unwrap().unrealized, dec!(7));

    // The same figure from the collateral module (equivalent policy:
    // totals, the reporting asset at no haircut, invalid marks excluded).
    let mut collateral_policy = CollateralPolicy::new(asset("USD"), 1000);
    collateral_policy.basis = BalanceBasis::Total;
    let collateral = value_collateral(&ledger.snapshot(), &marks, &collateral_policy).unwrap();
    assert_eq!(collateral.total, valuation.collateral_equity);
    assert_eq!(collateral.lines[&asset("USD")].balance, dec!(107));
    assert_eq!(
        collateral.lines[&asset("USD")].unrealized_pnl,
        Some(dec!(7))
    );

    // The maintenance view copies the valuation, nothing recomputed.
    let view = CollateralView::from_valuation(&valuation);
    assert_eq!(
        (view.currency, view.cash, view.collateral, view.equity),
        (asset("USD"), dec!(100), Some(dec!(107)), Some(dec!(107)))
    );
}

#[test]
fn a_loss_beyond_the_cash_makes_the_net_a_debt() {
    // 2 USD of cash, short 3 PERP-B at 200 marked at 210: net 2 - 3 = -1,
    // a debt counted in full.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USD", dec!(2));
    trade(
        &mut ledger,
        "e1",
        "PERP-B",
        Action::Sell,
        dec!(200),
        dec!(3),
    );
    let marks = MarkSet::new(10_000).with_instrument(iid("PERP-B"), mark(dec!(210), 9_500));
    let valuation = value(&ledger.snapshot(), &marks, &policy()).unwrap();
    assert_eq!(valuation.equity, Some(dec!(-1)));
    assert_eq!(valuation.collateral_equity, Some(dec!(-1)));
}

#[test]
fn a_settlement_asset_is_converted_once() {
    // 50 USDT held and long 1 PERP-T at 100 marked at 110: net 60 USDT at
    // 0.99 USD = 59.40, for equity and collateral alike.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USDT", dec!(50));
    trade(&mut ledger, "e1", "PERP-T", Action::Buy, dec!(100), dec!(1));
    let marks = MarkSet::new(10_000)
        .with(asset("USDT"), mark(dec!(0.99), 9_500))
        .with_instrument(iid("PERP-T"), mark(dec!(110), 9_500));
    let valuation = value(&ledger.snapshot(), &marks, &policy()).unwrap();
    assert_eq!(
        valuation.derivatives[&iid("PERP-T")].settlement,
        asset("USDT")
    );
    assert_eq!(valuation.equity, Some(dec!(59.40)));
    assert_eq!(valuation.collateral_equity, Some(dec!(59.40)));
    assert_eq!(
        valuation.assets[&asset("USDT")].collateral_value,
        Some(dec!(59.40))
    );
}

#[test]
fn an_invalid_instrument_mark_never_adds_collateral() {
    // A stale mark (2 000 ms old) on the 10 USD gain: equity uses it and
    // flags it; collateral excludes the gain.
    let (ledger, _) = long_gain();
    let stale = MarkSet::new(10_000).with_instrument(iid("PERP-A"), mark(dec!(105), 8_000));
    let valuation = value(&ledger.snapshot(), &stale, &policy()).unwrap();
    assert_eq!(valuation.equity, Some(dec!(110)));
    assert_eq!(valuation.collateral_equity, Some(dec!(100)));
    assert_eq!(
        valuation.instrument_flags[&iid("PERP-A")],
        MarkStatus::Stale { age_ms: 2000 }
    );
    // A future mark too.
    let future = MarkSet::new(10_000).with_instrument(iid("PERP-A"), mark(dec!(105), 10_500));
    let valuation = value(&ledger.snapshot(), &future, &policy()).unwrap();
    assert_eq!(valuation.collateral_equity, Some(dec!(100)));
    assert_eq!(
        valuation.instrument_flags[&iid("PERP-A")],
        MarkStatus::Future { ahead_ms: 500 }
    );
    // Exactly 1 000 ms old is still valid.
    let edge = MarkSet::new(10_000).with_instrument(iid("PERP-A"), mark(dec!(105), 9_000));
    let valuation = value(&ledger.snapshot(), &edge, &policy()).unwrap();
    assert_eq!(valuation.collateral_equity, Some(dec!(110)));

    // A loss at a stale mark (95: upnl -10) cannot be bounded: unknown,
    // never offset by the cash.
    let stale_loss = MarkSet::new(10_000).with_instrument(iid("PERP-A"), mark(dec!(95), 8_000));
    let valuation = value(&ledger.snapshot(), &stale_loss, &policy()).unwrap();
    assert_eq!(valuation.equity, Some(dec!(90)));
    assert_eq!(valuation.collateral_equity, None);
    let collateral = value_collateral(
        &ledger.snapshot(),
        &stale_loss,
        &CollateralPolicy::new(asset("USD"), 1000),
    )
    .unwrap();
    assert_eq!(collateral.total, None);
    assert_eq!(collateral.lines[&asset("USD")].unrealized_pnl, None);
    // Rejected outright under the strict policy.
    let mut strict = CollateralPolicy::new(asset("USD"), 1000);
    strict.stale = StalePolicy::Reject;
    assert_eq!(
        value_collateral(&ledger.snapshot(), &stale_loss, &strict),
        Err(MarginError::InvalidInstrumentMark {
            instrument: iid("PERP-A")
        })
    );
}

#[test]
fn instrument_marks_are_checked() {
    let (ledger, _) = long_gain();
    let snapshot = ledger.snapshot();
    // Missing for an open position: an error, never zero.
    assert_eq!(
        value(&snapshot, &MarkSet::new(10_000), &policy()),
        Err(MarginError::MissingMark {
            instrument: iid("PERP-A")
        })
    );
    let zero = MarkSet::new(10_000).with_instrument(iid("PERP-A"), mark(dec!(0), 9_500));
    assert!(matches!(
        value(&snapshot, &zero, &policy()),
        Err(MarginError::InvalidInput { .. })
    ));
    // A mark for a spot or unknown instrument is refused.
    let spot = MarkSet::new(10_000)
        .with_instrument(iid("PERP-A"), mark(dec!(105), 9_500))
        .with_instrument(iid("BTC-USD"), mark(dec!(1), 9_500));
    assert!(matches!(
        value(&snapshot, &spot, &policy()),
        Err(MarginError::InvalidInput { .. })
    ));
    let unknown = MarkSet::new(10_000)
        .with_instrument(iid("PERP-A"), mark(dec!(105), 9_500))
        .with_instrument(iid("NOPE"), mark(dec!(1), 9_500));
    assert!(matches!(
        value(&snapshot, &unknown, &policy()),
        Err(MarginError::UnknownInstrument { .. })
    ));
    // A mark for a flat derivative is accepted and ignored.
    let flat = MarkSet::new(10_000)
        .with_instrument(iid("PERP-A"), mark(dec!(105), 9_500))
        .with_instrument(iid("PERP-B"), mark(dec!(1), 1));
    let valuation = value(&snapshot, &flat, &policy()).unwrap();
    assert!(!valuation.derivatives.contains_key(&iid("PERP-B")));
    assert_eq!(valuation.equity, Some(dec!(110)));
}

#[test]
fn funding_and_realized_pnl_are_counted_once() {
    // Plus 1 USD of funding received: cash 101, equity 111. Realized 1
    // (the funding) and unrealized 10; net PnL 11.
    let (mut ledger, marks) = long_gain();
    ledger
        .book_funding(&FundingPayment {
            id: ExecutionId::new("f1").unwrap(),
            instrument: iid("PERP-A"),
            amount: dec!(1),
            as_of_ms: 2,
        })
        .unwrap();
    let valuation = value(&ledger.snapshot(), &marks, &policy()).unwrap();
    assert_eq!(valuation.equity, Some(dec!(111)));
    let attribution = valuation.attribution.unwrap();
    assert_eq!(
        (
            attribution.realized,
            attribution.unrealized,
            attribution.rounding_difference
        ),
        (dec!(1), dec!(10), dec!(0))
    );
}

#[test]
fn large_gains_and_losses_net_in_wide_integers() {
    // BIG (0 decimals) is the reporting asset. Long 1 X at 1 and short 1 Y
    // at 1, both marked at MAX: upnl MAX - 1 and -(MAX - 1). With 1 BIG of
    // cash, equity and collateral are 1.
    let max = Decimal::MAX;
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("BIG"), 0).unwrap();
    for id in ["X", "Y"] {
        ledger
            .register_instrument(LinearPerpetual::new(
                LinearTerms::new(iid(id), asset("BIG"), dec!(1), 0, 0).unwrap(),
            ))
            .unwrap();
    }
    deposit(&mut ledger, "d1", "BIG", dec!(1));
    trade(&mut ledger, "e1", "X", Action::Buy, dec!(1), dec!(1));
    trade(&mut ledger, "e2", "Y", Action::Sell, dec!(1), dec!(1));
    let marks = MarkSet::new(10_000)
        .with_instrument(iid("X"), mark(max, 9_500))
        .with_instrument(iid("Y"), mark(max, 9_500));
    let valuation = value(
        &ledger.snapshot(),
        &marks,
        &ValuationPolicy::new(asset("BIG"), 1000),
    )
    .unwrap();
    assert_eq!(
        valuation.derivatives[&iid("X")].unrealized_pnl,
        max - dec!(1)
    );
    assert_eq!(valuation.equity, Some(dec!(1)));
    assert_eq!(valuation.collateral_equity, Some(dec!(1)));
}

#[test]
fn valuation_does_not_change_the_snapshot() {
    let (ledger, marks) = long_gain();
    let snapshot = ledger.snapshot();
    let copy = snapshot.clone();
    value(&snapshot, &marks, &policy()).unwrap();
    assert_eq!(snapshot, copy);
}

#[test]
fn stress_refuses_an_open_derivative() {
    // Scenarios shock asset marks, not instrument marks: an open derivative
    // would stay unstressed, so it is refused.
    let (ledger, marks) = long_gain();
    assert!(matches!(
        margin_rs::scenarios::stress(
            &ledger.snapshot(),
            &marks,
            &policy(),
            &margin_rs::scenarios::Scenario::new("gap"),
        ),
        Err(MarginError::UnsupportedInstrument { .. })
    ));
}

#[test]
fn a_settlement_asset_without_a_balance_row_still_counts() {
    // 100 USD, long 2 PERP-T (settled in USDT) at 100 marked at 90: upnl
    // -20 USDT at 1 USD. Equity and collateral are 80, also when a
    // hand-built snapshot has no USDT balance row (it counts as zero).
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USD", dec!(100));
    trade(&mut ledger, "e1", "PERP-T", Action::Buy, dec!(100), dec!(2));
    let marks = MarkSet::new(10_000)
        .with(asset("USDT"), mark(dec!(1), 9_500))
        .with_instrument(iid("PERP-T"), mark(dec!(90), 9_500));
    let mut collateral_policy = CollateralPolicy::new(asset("USD"), 1000);
    collateral_policy.basis = BalanceBasis::Total;
    let natural = ledger.snapshot();
    let mut without_row = natural.clone();
    without_row.balances.remove(&asset("USDT"));
    for snapshot in [natural, without_row] {
        let valuation = value(&snapshot, &marks, &policy()).unwrap();
        assert_eq!(valuation.equity, Some(dec!(80)));
        assert_eq!(valuation.collateral_equity, Some(dec!(80)));
        let collateral = value_collateral(&snapshot, &marks, &collateral_policy).unwrap();
        assert_eq!(collateral.total, Some(dec!(80)));
        assert_eq!(collateral.lines[&asset("USDT")].balance, dec!(-20));
    }
}

#[test]
fn gains_net_with_the_cash_in_one_exact_sum() {
    // BIG (0 decimals) is the reporting asset. Long 1 X and long 1 Y at 1,
    // both marked at MAX: two gains of MAX - 1, whose sum does not fit a
    // Decimal. With a cash balance of -MAX (a hand-built snapshot), the net
    // MAX - 2 does, and is what equity and collateral report.
    let max = Decimal::MAX;
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("BIG"), 0).unwrap();
    for id in ["X", "Y"] {
        ledger
            .register_instrument(LinearPerpetual::new(
                LinearTerms::new(iid(id), asset("BIG"), dec!(1), 0, 0).unwrap(),
            ))
            .unwrap();
    }
    trade(&mut ledger, "e1", "X", Action::Buy, dec!(1), dec!(1));
    trade(&mut ledger, "e2", "Y", Action::Buy, dec!(1), dec!(1));
    let mut snapshot = ledger.snapshot();
    let cash = snapshot.balances.get_mut(&asset("BIG")).unwrap();
    cash.total = -max;
    cash.available = -max;
    let marks = MarkSet::new(10_000)
        .with_instrument(iid("X"), mark(max, 9_500))
        .with_instrument(iid("Y"), mark(max, 9_500));
    let valuation = value(&snapshot, &marks, &ValuationPolicy::new(asset("BIG"), 1000)).unwrap();
    assert_eq!(valuation.equity, Some(max - dec!(2)));
    assert_eq!(valuation.collateral_equity, Some(max - dec!(2)));
    // The unrealized figure alone (2 * MAX - 2) does not fit: no split.
    assert_eq!(valuation.attribution, None);
    let mut collateral_policy = CollateralPolicy::new(asset("BIG"), 1000);
    collateral_policy.basis = BalanceBasis::Total;
    let collateral = value_collateral(&snapshot, &marks, &collateral_policy).unwrap();
    let line = &collateral.lines[&asset("BIG")];
    assert_eq!(line.balance, max - dec!(2));
    assert_eq!(line.unrealized_pnl, None);
    assert_eq!(collateral.total, Some(max - dec!(2)));
}

#[test]
fn a_missing_balance_row_follows_every_collateral_rule() {
    // Long 2 PERP-T at 100 with 100 USD and no USDT row (hand-built), USDT
    // at 1 USD. Per instrument mark: a valid gain at 110 counts (+20, the
    // USDT held is not eligible in value_collateral by default, so only in
    // value), a stale gain is excluded, a stale loss makes it unknown.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USD", dec!(100));
    trade(&mut ledger, "e1", "PERP-T", Action::Buy, dec!(100), dec!(2));
    let mut snapshot = ledger.snapshot();
    snapshot.balances.remove(&asset("USDT"));
    let marks_at = |price, at| {
        MarkSet::new(10_000)
            .with(asset("USDT"), mark(dec!(1), 9_500))
            .with_instrument(iid("PERP-T"), mark(price, at))
    };
    let mut collateral_policy = CollateralPolicy::new(asset("USD"), 1000);
    collateral_policy.basis = BalanceBasis::Total;
    for (marks, collateral_equity, collateral_total) in [
        (marks_at(dec!(110), 9_500), Some(dec!(120)), Some(dec!(100))),
        (marks_at(dec!(110), 8_000), Some(dec!(100)), Some(dec!(100))),
        (marks_at(dec!(90), 8_000), None, None),
    ] {
        let valuation = value(&snapshot, &marks, &policy()).unwrap();
        assert_eq!(valuation.collateral_equity, collateral_equity);
        let collateral = value_collateral(&snapshot, &marks, &collateral_policy).unwrap();
        assert_eq!(collateral.total, collateral_total);
    }
}
