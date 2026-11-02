//! Hand checked fixtures for stress scenarios with open linear futures and
//! perpetuals (#31).
//!
//! `upnl = rhe(Q * M * mark) - basis` per instrument, in its settlement
//! asset, at the stressed mark `mark * (1 + shock) * (1 -+ spread)`.
//! Collateral counts `cash + sum(upnl)` per settlement asset as one net
//! amount. Every expected value is written out by hand.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::instrument::{
    InversePerpetual, InverseTerms, LinearFuture, LinearPerpetual, LinearTerms,
};
use margin_rs::ledger::{Action, Execution, ExpirySettlement, Fee, Ledger, LedgerConfig};
use margin_rs::scenarios::{HypotheticalFill, Scenario, StressResult, stress};
use margin_rs::valuation::{Mark, MarkSet, MarkStatus, PriceSource, ValuationPolicy};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, ReservationId, Rounding,
    SpotInstrument, TransferId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}
fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}
fn terms(id: &str, settlement: &str, multiplier: Decimal) -> LinearTerms {
    LinearTerms::new(iid(id), asset(settlement), multiplier, 2, 0).unwrap()
}

/// USD (reporting, 2 decimals), USDT (2 decimals) and BTC (8 decimals).
/// Perpetuals PERP-A (USD, multiplier 10), PERP-S (USD, 1), PERP-T and
/// PERP-U (USDT, 1); futures FUT-A (USD, 1, expiry 10 000 ms) and FUT-B
/// (USD, 1, expiry 5 000 ms); spot BTC-USD.
fn ledger() -> Ledger {
    let mut ledger = Ledger::new(LedgerConfig {
        rounding: Rounding::HalfEven,
    });
    ledger.register_asset(asset("USD"), 2).unwrap();
    ledger.register_asset(asset("USDT"), 2).unwrap();
    ledger.register_asset(asset("BTC"), 8).unwrap();
    for (id, settlement, multiplier) in [
        ("PERP-A", "USD", dec!(10)),
        ("PERP-S", "USD", dec!(1)),
        ("PERP-T", "USDT", dec!(1)),
        ("PERP-U", "USDT", dec!(1)),
    ] {
        ledger
            .register_instrument(LinearPerpetual::new(terms(id, settlement, multiplier)))
            .unwrap();
    }
    ledger
        .register_instrument(LinearFuture::new(terms("FUT-A", "USD", dec!(1)), 10_000))
        .unwrap();
    ledger
        .register_instrument(LinearFuture::new(terms("FUT-B", "USD", dec!(1)), 5_000))
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
    (price, n): (Decimal, Decimal),
    fee: Option<(&str, Decimal)>,
) {
    let mut execution = Execution::new(
        ExecutionId::new(id).unwrap(),
        iid(instrument),
        side,
        price,
        n,
    )
    .with_executed_at(1);
    if let Some((code, amount)) = fee {
        execution = execution.with_fee(asset(code), amount);
    }
    ledger
        .book_execution_allowing_overdraft(&execution, None)
        .unwrap();
}

/// Valid up to 1 000 ms old.
fn policy() -> ValuationPolicy {
    ValuationPolicy::new(asset("USD"), 1000)
}
fn mark(price: Decimal, at: u64) -> Mark {
    Mark::new(price, at, PriceSource::Mid)
}
/// Marks at 10 000 ms, USDT at 1 USD.
fn marks() -> MarkSet {
    MarkSet::new(10_000).with(asset("USDT"), mark(dec!(1), 9_900))
}
fn fill(instrument: &str, side: Action, price: Decimal, quantity: Decimal) -> HypotheticalFill {
    HypotheticalFill {
        instrument: iid(instrument),
        side,
        price,
        quantity,
        fee: None,
    }
}
fn no_instruments() -> Vec<InstrumentId> {
    Vec::new()
}
fn run(ledger: &Ledger, marks: &MarkSet, scenario: &Scenario) -> StressResult {
    stress(&ledger.snapshot(), marks, &policy(), scenario).unwrap()
}
fn upnl(result: &StressResult, id: &str) -> Decimal {
    result.stressed.derivatives[&iid(id)].unrealized_pnl
}

/// Long 2 PERP-A (multiplier 10) at 100, basis 2000, on 1000 USD, marked at
/// 105: upnl `2 * 10 * 105 - 2000 = 100`.
fn long_perp_a() -> (Ledger, MarkSet) {
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USD", dec!(1000));
    trade(
        &mut ledger,
        "e1",
        "PERP-A",
        Action::Buy,
        (dec!(100), dec!(2)),
        None,
    );
    let marks = marks().with_instrument(iid("PERP-A"), mark(dec!(105), 9_500));
    (ledger, marks)
}

#[test]
fn test_stress_long_under_an_instrument_shock_loses_the_move() {
    // -10 %: mark 94.5, market value 2 * 10 * 94.5 = 1890, upnl -110.
    let (ledger, marks) = long_perp_a();
    let snapshot = ledger.snapshot();
    let scenario = Scenario::new("perp -10%").instrument_shock(iid("PERP-A"), dec!(-0.1));
    let r = stress(&snapshot, &marks, &policy(), &scenario).unwrap();
    assert_eq!(r.base.derivatives[&iid("PERP-A")].unrealized_pnl, dec!(100));
    let line = &r.stressed.derivatives[&iid("PERP-A")];
    assert_eq!(
        (line.mark.price, line.market_value, line.unrealized_pnl),
        (dec!(94.5), dec!(1890), dec!(-110))
    );
    // The stressed mark keeps its time and source.
    assert_eq!(
        (line.mark.timestamp_ms, line.mark.source),
        (9_500, PriceSource::Mid)
    );
    assert_eq!(
        (r.base.equity, r.stressed.equity),
        (Some(dec!(1100)), Some(dec!(890)))
    );
    assert_eq!(r.stressed.collateral_equity, Some(dec!(890)));
    assert_eq!(
        (r.equity_change, r.collateral_change),
        (Some(dec!(-210)), Some(dec!(-210)))
    );
    assert_eq!(r.unshocked_instruments, no_instruments());
    assert_eq!(r.ignored_instrument_shocks, no_instruments());
    assert_eq!(r.spread_sign_conflicts, Vec::<Asset>::new());
    // Neither the snapshot nor the marks change.
    assert_eq!(snapshot, ledger.snapshot());
    assert_eq!(marks.instruments[&iid("PERP-A")].price, dec!(105));
}

#[test]
fn test_stress_short_under_an_instrument_shock_loses_the_rise() {
    // Short 3 PERP-S at 50 (basis -150) marked at 50; +20 %: mark 60,
    // market value -180, upnl -30.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USD", dec!(1000));
    trade(
        &mut ledger,
        "e1",
        "PERP-S",
        Action::Sell,
        (dec!(50), dec!(3)),
        None,
    );
    let marks = marks().with_instrument(iid("PERP-S"), mark(dec!(50), 9_500));
    let scenario = Scenario::new("perp +20%").instrument_shock(iid("PERP-S"), dec!(0.2));
    let r = run(&ledger, &marks, &scenario);
    assert_eq!(upnl(&r, "PERP-S"), dec!(-30));
    assert_eq!(r.stressed.equity, Some(dec!(970)));
    assert_eq!(r.collateral_change, Some(dec!(-30)));
}

#[test]
fn test_stress_loss_beyond_the_cash_of_its_asset_is_a_debt_across_the_spread() {
    // 100 USDT, long 1 PERP-T at 300 (basis 300) marked at 300. -50 % and a
    // 2 % spread: the long closes at 300 * 0.5 * 0.98 = 147, upnl -153. The
    // USDT net 100 - 153 = -53 is a debt, bought back at 1.02: -54.06, not
    // valued at 0.98 by the sign of the cash alone.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USDT", dec!(100));
    trade(
        &mut ledger,
        "e1",
        "PERP-T",
        Action::Buy,
        (dec!(300), dec!(1)),
        None,
    );
    let marks = marks().with_instrument(iid("PERP-T"), mark(dec!(300), 9_500));
    let scenario = Scenario::new("crash")
        .instrument_shock(iid("PERP-T"), dec!(-0.5))
        .spread(dec!(0.02));
    let r = run(&ledger, &marks, &scenario);
    assert_eq!(r.stressed.derivatives[&iid("PERP-T")].mark.price, dec!(147));
    assert_eq!(upnl(&r, "PERP-T"), dec!(-153));
    assert_eq!(
        r.stressed.assets[&asset("USDT")].mark.unwrap().price,
        dec!(1.02)
    );
    assert_eq!(r.stressed.equity, Some(dec!(-54.06)));
    assert_eq!(r.stressed.collateral_equity, Some(dec!(-54.06)));
    assert_eq!(r.base.collateral_equity, Some(dec!(100)));
    assert_eq!(r.spread_sign_conflicts, Vec::<Asset>::new());
}

#[test]
fn test_stress_gain_in_an_asset_without_cash_is_sold_across_the_spread() {
    // The mirror: short 1 PERP-T at 300 (basis -300), no USDT. -50 % and a
    // 2 % spread: the short closes at 150 * 1.02 = 153, upnl +147, valued
    // at 0.98: 144.06.
    let mut ledger = ledger();
    trade(
        &mut ledger,
        "e1",
        "PERP-T",
        Action::Sell,
        (dec!(300), dec!(1)),
        None,
    );
    let marks = marks().with_instrument(iid("PERP-T"), mark(dec!(300), 9_500));
    let scenario = Scenario::new("crash")
        .instrument_shock(iid("PERP-T"), dec!(-0.5))
        .spread(dec!(0.02));
    let r = run(&ledger, &marks, &scenario);
    assert_eq!(upnl(&r, "PERP-T"), dec!(147));
    assert_eq!(r.stressed.equity, Some(dec!(144.06)));
    assert_eq!(r.stressed.collateral_equity, Some(dec!(144.06)));
}

#[test]
fn test_stress_spread_side_follows_collateral_when_a_gain_is_at_a_stale_mark() {
    // A 100 USDT fee overdraws USDT to -100; long 1 PERP-T at 300 marked at
    // 450, but 9 000 ms old (stale). 2 % spread: the long closes at
    // 450 * 0.98 = 441, a stale gain of 141 that collateral does not count.
    // Collateral counts -100: a debt bought back at 1.02, -102 (not -98).
    // Reporting equity nets the gain, (−100 + 141) * 1.02 = 41.82, which is
    // not conservative: USDT is listed as a conflict.
    let mut ledger = ledger();
    trade(
        &mut ledger,
        "e1",
        "PERP-T",
        Action::Buy,
        (dec!(300), dec!(1)),
        Some(("USDT", dec!(100))),
    );
    let marks = marks().with_instrument(iid("PERP-T"), mark(dec!(450), 1_000));
    let r = run(&ledger, &marks, &Scenario::new("spread").spread(dec!(0.02)));
    assert_eq!(upnl(&r, "PERP-T"), dec!(141));
    assert_eq!(
        r.stressed.instrument_flags[&iid("PERP-T")],
        MarkStatus::Stale { age_ms: 9_000 }
    );
    assert_eq!(r.stressed.collateral_equity, Some(dec!(-102)));
    assert_eq!(r.stressed.equity, Some(dec!(41.82)));
    assert_eq!(r.spread_sign_conflicts, vec![asset("USDT")]);
    assert_eq!(r.unshocked_instruments, vec![iid("PERP-T")]);
}

#[test]
fn test_stress_loss_at_a_stale_mark_makes_collateral_unknown() {
    // 100 USDT, long 1 PERP-T at 300 marked at 300, 9 000 ms old. -10 %:
    // upnl -30 at a stale mark, so collateral is unknown; equity 70. At a
    // valid mark a +10 % gain counts: 130.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USDT", dec!(100));
    trade(
        &mut ledger,
        "e1",
        "PERP-T",
        Action::Buy,
        (dec!(300), dec!(1)),
        None,
    );
    let stale = marks().with_instrument(iid("PERP-T"), mark(dec!(300), 1_000));
    let loss = Scenario::new("loss").instrument_shock(iid("PERP-T"), dec!(-0.1));
    let r = run(&ledger, &stale, &loss);
    assert_eq!(upnl(&r, "PERP-T"), dec!(-30));
    assert_eq!(r.base.collateral_equity, Some(dec!(100)));
    assert_eq!(r.stressed.collateral_equity, None);
    assert_eq!(r.collateral_change, None);
    assert_eq!(r.stressed.equity, Some(dec!(70)));

    let valid = marks().with_instrument(iid("PERP-T"), mark(dec!(300), 9_500));
    let gain = Scenario::new("gain").instrument_shock(iid("PERP-T"), dec!(0.1));
    let r = run(&ledger, &valid, &gain);
    assert_eq!(r.stressed.collateral_equity, Some(dec!(130)));

    // A stale gain is not counted: collateral stays at the cash, 100.
    let r = run(&ledger, &stale, &gain);
    assert_eq!(
        (r.stressed.equity, r.stressed.collateral_equity),
        (Some(dec!(130)), Some(dec!(100)))
    );
}

#[test]
fn test_stress_loss_at_a_stale_mark_takes_the_spread_side_of_the_full_net() {
    // As the debt fixture, at a stale mark: collateral is unknown, and
    // equity takes the side of the full net 100 - 153 = -53: -54.06.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USDT", dec!(100));
    trade(
        &mut ledger,
        "e1",
        "PERP-T",
        Action::Buy,
        (dec!(300), dec!(1)),
        None,
    );
    let marks = marks().with_instrument(iid("PERP-T"), mark(dec!(300), 1_000));
    let scenario = Scenario::new("crash")
        .instrument_shock(iid("PERP-T"), dec!(-0.5))
        .spread(dec!(0.02));
    let r = run(&ledger, &marks, &scenario);
    assert_eq!(r.stressed.collateral_equity, None);
    assert_eq!(r.stressed.equity, Some(dec!(-54.06)));
    assert_eq!(r.spread_sign_conflicts, Vec::<Asset>::new());
}

#[test]
fn test_stress_zero_spread_is_the_scenario_without_spread() {
    let (ledger, marks) = long_perp_a();
    let shock = Scenario::new("s").instrument_shock(iid("PERP-A"), dec!(-0.1));
    let without = run(&ledger, &marks, &shock);
    let with_zero = run(&ledger, &marks, &shock.clone().spread(dec!(0)));
    assert_eq!(without, with_zero);
}

#[test]
fn test_stress_hedge_that_crosses_zero_closes_the_short_at_the_ask() {
    // Long 2 PERP-A at 100 (basis 2000); a hypothetical sell of 5 at 105
    // closes 2 (2100, realized +100 in cash) and opens 3 short with basis
    // -(5250 - 2100) = -3150. A 2 % spread closes the short at
    // 105 * 1.02 = 107.1: market value -3 * 10 * 107.1 = -3213, upnl -63.
    // Equity 1100 - 63 = 1037 (at the bid side it would be +63).
    let (ledger, marks) = long_perp_a();
    let scenario = Scenario::new("over-hedge")
        .fill(fill("PERP-A", Action::Sell, dec!(105), dec!(5)))
        .spread(dec!(0.02));
    let r = run(&ledger, &marks, &scenario);
    let line = &r.stressed.derivatives[&iid("PERP-A")];
    assert_eq!(
        (line.contracts, line.basis, line.mark.price),
        (dec!(-3), dec!(-3150), dec!(107.1))
    );
    assert_eq!(line.unrealized_pnl, dec!(-63));
    assert_eq!(r.stressed.assets[&asset("USD")].total, dec!(1100));
    assert_eq!(r.stressed.equity, Some(dec!(1037)));
    assert_eq!(r.equity_change, Some(dec!(-63)));
}

#[test]
fn test_stress_hedge_that_closes_the_position_ignores_its_shock() {
    // A hypothetical sell of 2 at 105 closes the long: realized +100 in
    // cash, flat, so the -50 % shock has no effect and is reported.
    let (ledger, marks) = long_perp_a();
    let scenario = Scenario::new("hedged")
        .fill(fill("PERP-A", Action::Sell, dec!(105), dec!(2)))
        .instrument_shock(iid("PERP-A"), dec!(-0.5));
    let r = run(&ledger, &marks, &scenario);
    assert_eq!(r.stressed.derivatives.len(), 0);
    assert_eq!(r.stressed.assets[&asset("USD")].total, dec!(1100));
    assert_eq!(r.stressed.equity, Some(dec!(1100)));
    assert_eq!(r.equity_change, Some(dec!(0)));
    assert_eq!(r.ignored_instrument_shocks, vec![iid("PERP-A")]);
    assert_eq!(r.unshocked_instruments, no_instruments());
}

#[test]
fn test_stress_two_derivatives_in_one_asset_net_with_its_cash() {
    // 100 USDT; long 1 PERP-T at 300 marked at 310 (+10) and short 2 PERP-U
    // at 50 marked at 55 (-10): net 100. PERP-T -10 % (279, upnl -21) and
    // PERP-U +10 % (60.5, market value -121, upnl -21): net 58.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USDT", dec!(100));
    trade(
        &mut ledger,
        "e1",
        "PERP-T",
        Action::Buy,
        (dec!(300), dec!(1)),
        None,
    );
    trade(
        &mut ledger,
        "e2",
        "PERP-U",
        Action::Sell,
        (dec!(50), dec!(2)),
        None,
    );
    let marks = marks()
        .with_instrument(iid("PERP-T"), mark(dec!(310), 9_500))
        .with_instrument(iid("PERP-U"), mark(dec!(55), 9_500));
    let scenario = Scenario::new("both")
        .instrument_shock(iid("PERP-T"), dec!(-0.1))
        .instrument_shock(iid("PERP-U"), dec!(0.1));
    let r = run(&ledger, &marks, &scenario);
    assert_eq!(r.base.collateral_equity, Some(dec!(100)));
    assert_eq!(
        (upnl(&r, "PERP-T"), upnl(&r, "PERP-U")),
        (dec!(-21), dec!(-21))
    );
    assert_eq!(
        r.stressed.assets[&asset("USDT")].collateral_value,
        Some(dec!(58))
    );
    assert_eq!(
        (r.stressed.equity, r.stressed.collateral_equity),
        (Some(dec!(58)), Some(dec!(58)))
    );
}

#[test]
fn test_stress_derivative_fill_moves_total_and_available_once() {
    // 1000 USD with 300 reserved; a hypothetical buy of 1 PERP-A at 100 with
    // a 5 USD fee: no cash for the open, the fee once from total and
    // available, the reservation kept.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USD", dec!(1000));
    ledger
        .reserve(ReservationId::new("r1").unwrap(), asset("USD"), dec!(300))
        .unwrap();
    let marks = marks().with_instrument(iid("PERP-A"), mark(dec!(100), 9_500));
    let mut buy = fill("PERP-A", Action::Buy, dec!(100), dec!(1));
    buy.fee = Some(Fee {
        asset: asset("USD"),
        amount: dec!(5),
    });
    let r = run(&ledger, &marks, &Scenario::new("open").fill(buy));
    let usd = &r.stressed.assets[&asset("USD")];
    assert_eq!((usd.total, usd.available), (dec!(995), dec!(695)));
    assert_eq!(r.base.assets[&asset("USD")].available, dec!(700));
    let line = &r.stressed.derivatives[&iid("PERP-A")];
    assert_eq!((line.contracts, line.basis), (dec!(1), dec!(1000)));
    assert_eq!(r.equity_change, Some(dec!(-5)));
    assert_eq!(r.unshocked_instruments, vec![iid("PERP-A")]);
}

#[test]
fn test_stress_derivative_fill_fee_in_another_asset_is_a_debt() {
    // A hypothetical sell of 1 PERP-A at 100 with a 0.001 BTC fee: BTC goes
    // to -0.001, a debt of 50 USD at 50 000.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USD", dec!(1000));
    let marks = marks()
        .with(asset("BTC"), mark(dec!(50000), 9_900))
        .with_instrument(iid("PERP-A"), mark(dec!(100), 9_500));
    let mut sell = fill("PERP-A", Action::Sell, dec!(100), dec!(1));
    sell.fee = Some(Fee {
        asset: asset("BTC"),
        amount: dec!(0.001),
    });
    let r = run(&ledger, &marks, &Scenario::new("fee").fill(sell));
    assert_eq!(r.stressed.assets[&asset("BTC")].total, dec!(-0.001));
    assert_eq!(upnl(&r, "PERP-A"), dec!(0));
    assert_eq!(
        (r.equity_change, r.collateral_change),
        (Some(dec!(-50)), Some(dec!(-50)))
    );
}

#[test]
fn test_stress_instrument_mark_is_one_exact_product() {
    // 7e28 * 1.5 does not fit, 7e28 * 1.5 * 0.5 = 5.25e28 does. Long 1
    // contract of multiplier 1e-24 bought at 1e28 (basis 10 000): market
    // value 1e-24 * 5.25e28 = 52 500, upnl 42 500.
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USD"), 2).unwrap();
    ledger
        .register_instrument(LinearPerpetual::new(terms(
            "PERP-X",
            "USD",
            dec!(0.000000000000000000000001),
        )))
        .unwrap();
    trade(
        &mut ledger,
        "e1",
        "PERP-X",
        Action::Buy,
        (dec!(10000000000000000000000000000), dec!(1)),
        None,
    );
    let marks = MarkSet::new(10_000).with_instrument(
        iid("PERP-X"),
        mark(dec!(70000000000000000000000000000), 9_500),
    );
    let scenario = Scenario::new("x")
        .instrument_shock(iid("PERP-X"), dec!(0.5))
        .spread(dec!(0.5));
    let r = run(&ledger, &marks, &scenario);
    assert_eq!(upnl(&r, "PERP-X"), dec!(42500));
    assert_eq!(r.stressed.equity, Some(dec!(42500)));
}

#[test]
fn test_stress_explicit_zero_shock_is_not_unshocked() {
    let (ledger, marks) = long_perp_a();
    let r = run(
        &ledger,
        &marks,
        &Scenario::new("zero").instrument_shock(iid("PERP-A"), dec!(0)),
    );
    assert_eq!(r.unshocked_instruments, no_instruments());
    assert_eq!(r.equity_change, Some(dec!(0)));
    let r = run(&ledger, &marks, &Scenario::new("none"));
    assert_eq!(r.unshocked_instruments, vec![iid("PERP-A")]);
}

#[test]
fn test_stress_is_deterministic() {
    let (ledger, marks) = long_perp_a();
    let scenario = Scenario::new("d")
        .fill(fill("PERP-A", Action::Sell, dec!(105), dec!(5)))
        .instrument_shock(iid("PERP-A"), dec!(0.03))
        .spread(dec!(0.01));
    let first = run(&ledger, &marks, &scenario);
    let second = run(&ledger, &marks, &scenario);
    assert_eq!(first, second);
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
}

#[test]
fn test_stress_invalid_instrument_shocks_and_fills_are_refused() {
    let (mut ledger, marks) = long_perp_a();
    let snapshot = ledger.snapshot();
    let refuse = |scenario: Scenario| stress(&snapshot, &marks, &policy(), &scenario);
    assert!(matches!(
        refuse(Scenario::new("x").instrument_shock(iid("NOPE"), dec!(0.1))),
        Err(MarginError::UnknownInstrument { .. })
    ));
    assert!(matches!(
        refuse(Scenario::new("x").instrument_shock(iid("BTC-USD"), dec!(0.1))),
        Err(MarginError::InvalidInput { .. })
    ));
    assert!(matches!(
        refuse(Scenario::new("x").instrument_shock(iid("PERP-A"), dec!(-1))),
        Err(MarginError::InvalidInput { .. })
    ));
    // FUT-A expires at 10 000 ms, the valuation time.
    assert!(matches!(
        refuse(Scenario::new("x").fill(fill("FUT-A", Action::Buy, dec!(100), dec!(1)))),
        Err(MarginError::ExecutedAfterExpiry {
            executed_at_ms: 10_000,
            expiry_ms: 10_000,
            ..
        })
    ));
    // Off the price scale (2) and the contract scale (0).
    for bad in [
        fill("PERP-A", Action::Buy, dec!(100.001), dec!(1)),
        fill("PERP-A", Action::Buy, dec!(100), dec!(0.5)),
    ] {
        assert!(refuse(Scenario::new("x").fill(bad)).is_err());
    }
    assert!(matches!(
        refuse(Scenario::new("x").fill(fill("PERP-A", Action::Buy, dec!(0), dec!(1)))),
        Err(MarginError::InvalidInput { .. })
    ));
    // A derivative opened without an instrument mark.
    assert!(matches!(
        refuse(Scenario::new("x").fill(fill("PERP-S", Action::Buy, dec!(50), dec!(1)))),
        Err(MarginError::MissingMark { .. })
    ));

    // A settled future takes no hypothetical fill.
    ledger
        .settle_expiry(&ExpirySettlement {
            id: ExecutionId::new("x1").unwrap(),
            instrument: iid("FUT-B"),
            price: dec!(100),
            as_of_ms: 6_000,
        })
        .unwrap();
    let settled = ledger.snapshot();
    assert!(matches!(
        stress(
            &settled,
            &marks,
            &policy(),
            &Scenario::new("x").fill(fill("FUT-B", Action::Buy, dec!(100), dec!(1))),
        ),
        Err(MarginError::InvalidInput { .. })
    ));
}

#[test]
fn test_stress_future_before_expiry_is_shocked_like_a_perpetual() {
    // Long 4 FUT-A at 25 (basis 100) on 1000 USD, valued at 9 000 ms before
    // its 10 000 ms expiry, marked at 25; -20 %: 20, upnl -20.
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", "USD", dec!(1000));
    trade(
        &mut ledger,
        "e1",
        "FUT-A",
        Action::Buy,
        (dec!(25), dec!(4)),
        None,
    );
    let marks = MarkSet::new(9_000).with_instrument(iid("FUT-A"), mark(dec!(25), 8_500));
    let scenario = Scenario::new("fut").instrument_shock(iid("FUT-A"), dec!(-0.2));
    let r = run(&ledger, &marks, &scenario);
    assert_eq!(upnl(&r, "FUT-A"), dec!(-20));
    assert_eq!(r.equity_change, Some(dec!(-20)));
}

#[test]
fn test_stress_inverse_positions_shocks_and_fills_are_unsupported() {
    // INV-B settles in BTC, 100 quote units per contract.
    let mut ledger = ledger();
    ledger
        .register_instrument(InversePerpetual::new(
            InverseTerms::new(iid("INV-B"), asset("BTC"), dec!(100), 1, 0).unwrap(),
        ))
        .unwrap();
    deposit(&mut ledger, "d1", "USD", dec!(1000));
    let marks = marks()
        .with(asset("BTC"), mark(dec!(50000), 9_900))
        .with_instrument(iid("INV-B"), mark(dec!(50000), 9_500));
    let unsupported = |ledger: &Ledger, scenario: Scenario| {
        matches!(
            stress(&ledger.snapshot(), &marks, &policy(), &scenario),
            Err(MarginError::UnsupportedInstrument { .. })
        )
    };
    // A flat inverse instrument does not stop a stress.
    assert!(
        stress(
            &ledger.snapshot(),
            &marks,
            &policy(),
            &Scenario::new("flat")
        )
        .is_ok()
    );
    assert!(unsupported(
        &ledger,
        Scenario::new("shock").instrument_shock(iid("INV-B"), dec!(-0.1))
    ));
    assert!(unsupported(
        &ledger,
        Scenario::new("fill").fill(fill("INV-B", Action::Buy, dec!(50000), dec!(1)))
    ));
    // An open inverse position is refused rather than left unstressed.
    trade(
        &mut ledger,
        "e1",
        "INV-B",
        Action::Buy,
        (dec!(50000), dec!(1)),
        None,
    );
    assert!(unsupported(&ledger, Scenario::new("open")));
}
