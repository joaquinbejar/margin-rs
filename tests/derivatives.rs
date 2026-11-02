//! Hand checked fixtures for linear futures and perpetuals held in the
//! ledger (M3-INT).
//!
//! Every expected value is written out by hand from the rules in
//! `docs/ledger.md` (derivative positions); none is computed by the code
//! under test. `rhe` is half-even rounding to the settlement asset scale.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::instrument::{LinearFuture, LinearPerpetual, LinearTerms, PositionKind};
use margin_rs::ledger::{
    Action, Booking, Execution, ExpirySettlement, FundingPayment, Ledger, LedgerConfig,
};
use margin_rs::risk::{InstrumentLimits, OrderRequest, RiskDecision, RiskGate, RiskReason};
use margin_rs::valuation::{Mark, MarkSet, PriceSource, ValuationPolicy, value};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, OrderId, ReservationId, Rounding,
    TransferId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}
fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).unwrap()
}
fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}
fn terms(
    id: &str,
    settlement: &str,
    multiplier: Decimal,
    price: u32,
    contracts: u32,
) -> LinearTerms {
    LinearTerms::new(iid(id), asset(settlement), multiplier, price, contracts).unwrap()
}

/// USD (2 decimals) and BIG (0 decimals), with:
/// - BTC-PERP: perpetual in USD, multiplier 0.1, price scale 2, whole contracts;
/// - Q-PERP: perpetual in USD, multiplier 1, price scale 3, whole contracts;
/// - BTC-DEC: future in USD expiring at 1000, multiplier 1, price and
///   contract scale 2;
/// - BIG-PERP: perpetual in BIG, multiplier 1, scales 0;
/// - BIG-FUT: future in BIG expiring at 1000, multiplier 1, scales 0.
fn ledger() -> Ledger {
    let mut ledger = Ledger::new(LedgerConfig {
        // Derivatives ignore it: they are always half-even.
        rounding: Rounding::TowardZero,
    });
    ledger.register_asset(asset("USD"), 2).unwrap();
    ledger.register_asset(asset("BIG"), 0).unwrap();
    ledger
        .register_instrument(LinearPerpetual::new(terms(
            "BTC-PERP",
            "USD",
            dec!(0.1),
            2,
            0,
        )))
        .unwrap();
    ledger
        .register_instrument(LinearPerpetual::new(terms("Q-PERP", "USD", dec!(1), 3, 0)))
        .unwrap();
    ledger
        .register_instrument(LinearFuture::new(
            terms("BTC-DEC", "USD", dec!(1), 2, 2),
            1000,
        ))
        .unwrap();
    ledger
        .register_instrument(LinearPerpetual::new(terms(
            "BIG-PERP",
            "BIG",
            dec!(1),
            0,
            0,
        )))
        .unwrap();
    ledger
        .register_instrument(LinearFuture::new(
            terms("BIG-FUT", "BIG", dec!(1), 0, 0),
            1000,
        ))
        .unwrap();
    ledger
}

fn deposit(ledger: &mut Ledger, id: &str, amount: Decimal) {
    ledger
        .deposit(TransferId::new(id).unwrap(), asset("USD"), amount)
        .unwrap();
}

/// A derivative fill, executed at 500 unless set otherwise (BTC-DEC
/// expires at 1000).
fn exec(id: &str, instrument: &str, side: Action, price: Decimal, contracts: Decimal) -> Execution {
    Execution::new(eid(id), iid(instrument), side, price, contracts).with_executed_at(500)
}

/// Books through the mandatory path and returns the booking.
fn book(ledger: &mut Ledger, execution: &Execution) -> Booking {
    ledger
        .book_execution_allowing_overdraft(execution, None)
        .unwrap()
        .booking
}

fn cash(ledger: &Ledger) -> Decimal {
    ledger.balance(&asset("USD")).unwrap().total
}

#[test]
fn open_increase_reduce_and_close_a_perpetual() {
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", dec!(1000));
    let perp = iid("BTC-PERP");

    // Buy 3 at 100.05: N = rhe(100.05 * 3 * 0.1 = 30.015) = 30.02 (half
    // even). The basis is 30.02; only the 0.03 fee moves cash.
    let first = book(
        &mut ledger,
        &exec("e1", "BTC-PERP", Action::Buy, dec!(100.05), dec!(3))
            .with_fee(asset("USD"), dec!(0.03)),
    );
    let flow = first.cashflow();
    assert_eq!(flow.notional, dec!(0));
    assert_eq!(flow.notional_exact, None);
    assert_eq!(flow.realized_pnl, dec!(0));
    assert_eq!(flow.deltas[&asset("USD")], dec!(-0.03));
    let fill = flow.derivative.as_ref().unwrap();
    assert_eq!(fill.virtual_notional, Some(dec!(30.02)));
    assert_eq!(fill.released_basis, dec!(0));
    assert_eq!(cash(&ledger), dec!(999.97));

    // Buy 2 at 101: N = 20.20, basis 50.22, 5 contracts.
    book(
        &mut ledger,
        &exec("e2", "BTC-PERP", Action::Buy, dec!(101), dec!(2)),
    );
    let position = ledger.position(&perp).unwrap();
    assert_eq!(
        (position.quantity, position.cost_basis),
        (dec!(5), dec!(50.22))
    );
    assert_eq!(position.kind, PositionKind::LinearPerpetual);
    assert_eq!(position.multiplier, dec!(0.1));

    // Sell 2 at 102: N = 20.40, released = rhe(50.22 * 2 / 5 = 20.088) =
    // 20.09, realized = 20.40 - 20.09 = 0.31.
    let reduce = book(
        &mut ledger,
        &exec("e3", "BTC-PERP", Action::Sell, dec!(102), dec!(2)),
    );
    assert_eq!(reduce.cashflow().realized_pnl, dec!(0.31));
    assert_eq!(
        reduce
            .cashflow()
            .derivative
            .as_ref()
            .unwrap()
            .released_basis,
        dec!(20.09)
    );
    let position = ledger.position(&perp).unwrap();
    assert_eq!(
        (position.quantity, position.cost_basis),
        (dec!(3), dec!(30.13))
    );
    // Average price per unit of the underlying: 30.13 / (3 * 0.1).
    assert_eq!(
        position.average_price(),
        Some(dec!(100.43333333333333333333333333))
    );

    // The open position at a mark of 101.04: rhe(3 * 0.1 * 101.04 =
    // 30.312) = 30.31, unrealized 30.31 - 30.13 = 0.18.
    let marks = BTreeMap::from([(perp.clone(), dec!(101.04))]);
    let valuation = ledger.valuation(&marks).unwrap();
    assert_eq!(valuation.positions[&perp].market_value, dec!(30.31));
    assert_eq!(valuation.positions[&perp].unrealized_pnl, dec!(0.18));

    // Sell 3 at 99, a full close: N = 29.70, the whole basis 30.13 is
    // released, realized = -0.43.
    book(
        &mut ledger,
        &exec("e4", "BTC-PERP", Action::Sell, dec!(99), dec!(3)),
    );
    let position = ledger.position(&perp).unwrap();
    assert_eq!((position.quantity, position.cost_basis), (dec!(0), dec!(0)));
    assert_eq!(position.realized_pnl, dec!(-0.12));
    assert_eq!(position.average_price(), None);
    // 1000 - 0.03 + 0.31 - 0.43.
    assert_eq!(cash(&ledger), dec!(999.85));
    ledger.reconcile().unwrap();
}

#[test]
fn crossing_zero_splits_the_fill_without_rounding_twice() {
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", dec!(10));
    // Long 3 at 0.010: N = 0.03, basis 0.03.
    book(
        &mut ledger,
        &exec("e1", "Q-PERP", Action::Buy, dec!(0.010), dec!(3)),
    );
    // Sell 5 at 0.005: N = rhe(0.025) = 0.02, C = rhe(0.015) = 0.02, the
    // opening part O = N - C = 0.00 exactly (not rhe(0.010) = 0.01).
    // Realized = C - basis = -0.01; the new short of 2 has basis 0.
    let cross = book(
        &mut ledger,
        &exec("e2", "Q-PERP", Action::Sell, dec!(0.005), dec!(5)),
    );
    let flow = cross.cashflow();
    let fill = flow.derivative.as_ref().unwrap();
    assert_eq!(fill.virtual_notional, Some(dec!(0.02)));
    assert_eq!(fill.closing_notional, Some(dec!(0.02)));
    assert_eq!(fill.released_basis, dec!(0.03));
    assert_eq!(flow.realized_pnl, dec!(-0.01));
    let position = ledger.position(&iid("Q-PERP")).unwrap();
    assert_eq!(
        (position.quantity, position.cost_basis),
        (dec!(-2), dec!(0))
    );
    assert_eq!(cash(&ledger), dec!(9.99));
    ledger.reconcile().unwrap();

    // The mirror: short 3 at 0.010 (basis -0.03), buy 5 at 0.005.
    // Realized = -C - basis = -0.02 + 0.03 = 0.01; long 2 with basis 0.
    let mut mirror = ledger_with_cash(dec!(10));
    book(
        &mut mirror,
        &exec("e1", "Q-PERP", Action::Sell, dec!(0.010), dec!(3)),
    );
    let cross = book(
        &mut mirror,
        &exec("e2", "Q-PERP", Action::Buy, dec!(0.005), dec!(5)),
    );
    assert_eq!(cross.cashflow().realized_pnl, dec!(0.01));
    let position = mirror.position(&iid("Q-PERP")).unwrap();
    assert_eq!((position.quantity, position.cost_basis), (dec!(2), dec!(0)));
    assert_eq!(cash(&mirror), dec!(10.01));
    mirror.reconcile().unwrap();
}

fn ledger_with_cash(amount: Decimal) -> Ledger {
    let mut ledger = ledger();
    deposit(&mut ledger, "d1", amount);
    ledger
}

#[test]
fn a_crossing_at_decimal_max_keeps_its_intermediates_wide() {
    // Long 1 at MAX (basis MAX), then sell 2 at MAX: N = 2 * MAX does not
    // fit a Decimal, but C = MAX, realized = C - basis = 0, the new basis
    // -MAX and the cash all do.
    let max = Decimal::MAX;
    let mut long = ledger();
    book(
        &mut long,
        &exec("e1", "BIG-PERP", Action::Buy, max, dec!(1)),
    );
    let cross = book(
        &mut long,
        &exec("e2", "BIG-PERP", Action::Sell, max, dec!(2)),
    );
    let flow = cross.cashflow();
    let fill = flow.derivative.as_ref().unwrap();
    assert_eq!(fill.virtual_notional, None);
    assert_eq!(fill.closing_notional, Some(max));
    assert_eq!(flow.realized_pnl, dec!(0));
    let position = long.position(&iid("BIG-PERP")).unwrap();
    assert_eq!((position.quantity, position.cost_basis), (dec!(-1), -max));
    assert_eq!(long.balance(&asset("BIG")).unwrap().total, dec!(0));
    // The virtual identity is checked in wide integers.
    long.reconcile().unwrap();

    // The mirror: short 1 at MAX, buy 2 at MAX.
    let mut mirror = ledger();
    book(
        &mut mirror,
        &exec("e1", "BIG-PERP", Action::Sell, max, dec!(1)),
    );
    let cross = book(
        &mut mirror,
        &exec("e2", "BIG-PERP", Action::Buy, max, dec!(2)),
    );
    assert_eq!(cross.cashflow().realized_pnl, dec!(0));
    let position = mirror.position(&iid("BIG-PERP")).unwrap();
    assert_eq!((position.quantity, position.cost_basis), (dec!(1), max));
    mirror.reconcile().unwrap();
}

#[test]
fn derivative_executions_are_mandatory() {
    // A loss beyond the balance is booked with its shortfall.
    let mut ledger = ledger_with_cash(dec!(1));
    // Long 10 at 100 (basis 100), sold at 80: realized -20.
    book(
        &mut ledger,
        &exec("e1", "BTC-PERP", Action::Buy, dec!(100), dec!(10)),
    );
    let loss = ledger
        .book_execution_allowing_overdraft(
            &exec("e2", "BTC-PERP", Action::Sell, dec!(80), dec!(10)),
            None,
        )
        .unwrap();
    assert_eq!(loss.booking.cashflow().realized_pnl, dec!(-20));
    assert_eq!(loss.shortfalls, BTreeMap::from([(asset("USD"), dec!(19))]));
    assert_eq!(cash(&ledger), dec!(-19));
    ledger.reconcile().unwrap();

    // The strict calls refuse a derivative, with no effect.
    let before = ledger.snapshot();
    let strict = exec("e3", "BTC-PERP", Action::Buy, dec!(100), dec!(1));
    assert!(matches!(
        ledger.book_execution(&strict),
        Err(MarginError::InvalidInput { .. })
    ));
    let hold = ReservationId::new("h1").unwrap();
    assert!(matches!(
        ledger.book_execution_with_reservation(&strict, &hold, dec!(0)),
        Err(MarginError::InvalidInput { .. })
    ));
    // A derivative fill consumes no reservation.
    assert!(matches!(
        ledger.book_execution_allowing_overdraft(&strict, Some((&hold, dec!(0)))),
        Err(MarginError::InvalidInput { .. })
    ));
    assert_eq!(ledger.snapshot(), before);
}

#[test]
fn grids_are_checked_before_any_effect() {
    let mut ledger = ledger_with_cash(dec!(100));
    let before = ledger.snapshot();
    for bad in [
        exec("e1", "BTC-PERP", Action::Buy, dec!(100.001), dec!(1)),
        exec("e1", "BTC-PERP", Action::Buy, dec!(100), dec!(1.5)),
        exec("e1", "BTC-PERP", Action::Buy, dec!(100), dec!(1)).with_fee(asset("USD"), dec!(0.001)),
    ] {
        assert!(matches!(
            ledger.book_execution_allowing_overdraft(&bad, None),
            Err(MarginError::PrecisionExceeded { .. })
        ));
    }
    assert_eq!(ledger.snapshot(), before);
}

#[test]
fn funding_is_signed_cash_counted_once_as_realized() {
    let mut ledger = ledger_with_cash(dec!(100));
    let pay = |id: &str, amount: Decimal| FundingPayment {
        id: eid(id),
        instrument: iid("BTC-PERP"),
        amount,
        as_of_ms: 10,
    };
    // Received 1.25 then paid 2.50, with a flat position: no exposure is
    // created.
    let received = ledger.book_funding(&pay("f1", dec!(1.25))).unwrap();
    assert!(received.is_new);
    assert_eq!(received.cashflow.balance_after, dec!(101.25));
    ledger.book_funding(&pay("f2", dec!(-2.50))).unwrap();
    assert_eq!(cash(&ledger), dec!(98.75));
    assert_eq!(ledger.funding(&asset("USD")), dec!(-1.25));
    assert_eq!(ledger.position(&iid("BTC-PERP")).unwrap().quantity, dec!(0));
    // Counted once as realized PnL, net of nothing else.
    let valuation = ledger.valuation(&BTreeMap::new()).unwrap();
    assert_eq!(valuation.by_asset[&asset("USD")].realized_pnl, dec!(-1.25));
    assert_eq!(valuation.by_asset[&asset("USD")].net_pnl, dec!(-1.25));

    // Idempotent by id; the same id with another amount conflicts.
    assert!(!ledger.book_funding(&pay("f1", dec!(1.25))).unwrap().is_new);
    assert!(matches!(
        ledger.book_funding(&pay("f1", dec!(1.30))),
        Err(MarginError::ConflictingEntry { .. })
    ));
    // Mandatory: a payment beyond the balance reports the shortfall.
    let short = ledger.book_funding(&pay("f3", dec!(-200))).unwrap();
    assert_eq!(short.shortfall, Some(dec!(101.25)));
    ledger.reconcile().unwrap();

    // Not a perpetual, unknown, zero, too fine, or an id already used: no
    // effect.
    let before = ledger.snapshot();
    let mut future = pay("f4", dec!(1));
    future.instrument = iid("BTC-DEC");
    assert!(matches!(
        ledger.book_funding(&future),
        Err(MarginError::InvalidInput { .. })
    ));
    let mut unknown = pay("f4", dec!(1));
    unknown.instrument = iid("NOPE");
    assert!(matches!(
        ledger.book_funding(&unknown),
        Err(MarginError::UnknownInstrument { .. })
    ));
    assert!(matches!(
        ledger.book_funding(&pay("f4", dec!(0))),
        Err(MarginError::InvalidInput { .. })
    ));
    assert!(matches!(
        ledger.book_funding(&pay("f4", dec!(0.001))),
        Err(MarginError::PrecisionExceeded { .. })
    ));
    assert_eq!(ledger.snapshot(), before);
    book(
        &mut ledger,
        &exec("x1", "BTC-PERP", Action::Buy, dec!(100), dec!(1)),
    );
    let before_conflict = ledger.snapshot();
    assert!(matches!(
        ledger.book_funding(&pay("x1", dec!(1))),
        Err(MarginError::ConflictingEntry { .. })
    ));
    assert_eq!(ledger.snapshot(), before_conflict);
}

#[test]
fn a_future_settles_once_at_expiry() {
    let mut ledger = ledger_with_cash(dec!(100));
    let future = iid("BTC-DEC");
    let buy = exec("e1", "BTC-DEC", Action::Buy, dec!(50), dec!(2)).with_executed_at(500);
    book(&mut ledger, &buy);
    let settle = |id: &str, price: Decimal, as_of_ms: u64| ExpirySettlement {
        id: eid(id),
        instrument: future.clone(),
        price,
        as_of_ms,
    };
    // Not before the expiry (1000).
    assert!(matches!(
        ledger.settle_expiry(&settle("s1", dec!(52.5), 999)),
        Err(MarginError::InvalidInput { .. })
    ));
    // At 52.50: N = 105.00, the basis 100.00 is released, realized 5.00.
    let settled = ledger
        .settle_expiry(&settle("s1", dec!(52.5), 1000))
        .unwrap();
    assert!(settled.is_new);
    assert_eq!(settled.cashflow.contracts, dec!(2));
    assert_eq!(settled.cashflow.realized_pnl, dec!(5));
    assert_eq!(cash(&ledger), dec!(105));
    let position = ledger.position(&future).unwrap();
    assert_eq!((position.quantity, position.cost_basis), (dec!(0), dec!(0)));
    assert!(ledger.is_settled(&future));
    ledger.reconcile().unwrap();

    // Idempotent; another id for the same instrument conflicts.
    assert!(
        !ledger
            .settle_expiry(&settle("s1", dec!(52.5), 1000))
            .unwrap()
            .is_new
    );
    assert!(matches!(
        ledger.settle_expiry(&settle("s2", dec!(52.5), 1000)),
        Err(MarginError::ConflictingEntry { .. })
    ));
    // An identical historical fill is still a duplicate.
    assert!(!book(&mut ledger, &buy).is_new());

    // A late buy of 1 at 51, executed at 900 (before the expiry at 1000)
    // but delivered after the settlement at 52.50: it is booked, never
    // rejected, and closed at once at the settlement price. Realized
    // (52.50 - 51) * 1 * 1 = 1.50, the position stays flat.
    let late = exec("e2", "BTC-DEC", Action::Buy, dec!(51), dec!(1))
        .with_fee(asset("USD"), dec!(0.05))
        .with_executed_at(900);
    let booked = book(&mut ledger, &late);
    let flow = booked.cashflow();
    assert_eq!(flow.realized_pnl, dec!(1.5));
    let fill = flow.derivative.as_ref().unwrap();
    assert_eq!(fill.closed_at_settlement, Some(dec!(52.5)));
    // The fill at 51 and its close at the settlement price 52.50.
    assert_eq!(fill.virtual_notional, Some(dec!(51)));
    assert_eq!(fill.closing_notional, Some(dec!(52.5)));
    assert_eq!(fill.released_basis, dec!(0));
    assert_eq!(flow.deltas[&asset("USD")], dec!(1.45));
    let position = ledger.position(&future).unwrap();
    assert_eq!((position.quantity, position.cost_basis), (dec!(0), dec!(0)));
    assert_eq!(position.realized_pnl, dec!(6.5));
    assert_eq!(cash(&ledger), dec!(106.45));
    // Its replay is a duplicate.
    assert!(!book(&mut ledger, &late).is_new());
    ledger.reconcile().unwrap();
}

#[test]
fn a_late_fill_on_a_settled_short_is_closed_at_the_settlement_price() {
    // Short 2 at 50, settled at 52.50 (realized -5), then a late sell of 1
    // at 51: realized (51 - 52.50) * 1 = -1.50, flat.
    let mut ledger = ledger_with_cash(dec!(100));
    book(
        &mut ledger,
        &exec("e1", "BTC-DEC", Action::Sell, dec!(50), dec!(2)).with_executed_at(500),
    );
    let settled = ledger
        .settle_expiry(&ExpirySettlement {
            id: eid("s1"),
            instrument: iid("BTC-DEC"),
            price: dec!(52.5),
            as_of_ms: 1000,
        })
        .unwrap();
    assert_eq!(settled.cashflow.realized_pnl, dec!(-5));
    let late = book(
        &mut ledger,
        &exec("e2", "BTC-DEC", Action::Sell, dec!(51), dec!(1)).with_executed_at(900),
    );
    assert_eq!(late.cashflow().realized_pnl, dec!(-1.5));
    assert_eq!(
        late.cashflow()
            .derivative
            .as_ref()
            .unwrap()
            .closed_at_settlement,
        Some(dec!(52.5))
    );
    let position = ledger.position(&iid("BTC-DEC")).unwrap();
    assert_eq!((position.quantity, position.cost_basis), (dec!(0), dec!(0)));
    assert_eq!(cash(&ledger), dec!(93.5));
    ledger.reconcile().unwrap();
}

#[test]
fn a_flat_future_settles_with_no_cash() {
    let mut ledger = ledger_with_cash(dec!(100));
    let settled = ledger
        .settle_expiry(&ExpirySettlement {
            id: eid("s1"),
            instrument: iid("BTC-DEC"),
            price: dec!(52.5),
            as_of_ms: 1000,
        })
        .unwrap();
    assert_eq!(settled.cashflow.contracts, dec!(0));
    assert_eq!(settled.cashflow.realized_pnl, dec!(0));
    assert_eq!(cash(&ledger), dec!(100));
    assert!(ledger.is_settled(&iid("BTC-DEC")));
    // A perpetual does not expire.
    assert!(matches!(
        ledger.settle_expiry(&ExpirySettlement {
            id: eid("s2"),
            instrument: iid("BTC-PERP"),
            price: dec!(1),
            as_of_ms: 1000,
        }),
        Err(MarginError::InvalidInput { .. })
    ));
    ledger.reconcile().unwrap();
}

#[test]
fn the_risk_gate_refuses_derivatives() {
    let mut gate = RiskGate::new(ledger_with_cash(dec!(100)));
    let perp = iid("BTC-PERP");
    assert!(matches!(
        gate.set_limits(&perp, InstrumentLimits::default()),
        Err(MarginError::UnsupportedInstrument { .. })
    ));
    assert!(matches!(
        gate.on_market_data(&perp, dec!(100), 1),
        Err(MarginError::UnsupportedInstrument { .. })
    ));
    let order = OrderRequest::new(
        OrderId::new("o1").unwrap(),
        perp,
        Action::Buy,
        dec!(100),
        dec!(1),
    );
    let RiskDecision::Rejected { reasons } = gate.submit(&order, 1) else {
        panic!("a derivative order must be rejected");
    };
    assert!(matches!(
        reasons.as_slice(),
        [RiskReason::InvalidOrder {
            error: MarginError::UnsupportedInstrument { .. }
        }]
    ));
}

#[test]
fn account_valuation_refuses_an_open_derivative() {
    let mut ledger = ledger_with_cash(dec!(100));
    let policy = ValuationPolicy::new(asset("USD"), 1000);
    let marks = MarkSet::new(10);
    // Flat: valued, with the funding as realized PnL.
    ledger
        .book_funding(&FundingPayment {
            id: eid("f1"),
            instrument: iid("BTC-PERP"),
            amount: dec!(2),
            as_of_ms: 1,
        })
        .unwrap();
    let flat = value(&ledger.snapshot(), &marks, &policy).unwrap();
    assert_eq!(flat.equity, Some(dec!(102)));
    assert_eq!(flat.attribution.unwrap().realized, dec!(2));
    // Open: refused, never valued without its unrealized PnL.
    book(
        &mut ledger,
        &exec("e1", "BTC-PERP", Action::Buy, dec!(100), dec!(1)),
    );
    assert!(matches!(
        value(&ledger.snapshot(), &marks, &policy),
        Err(MarginError::UnsupportedInstrument { .. })
    ));
    // Asset marks do not help: the instrument mark is what is missing.
    let usd_marks = MarkSet::new(10).with(asset("BIG"), Mark::new(dec!(1), 5, PriceSource::Mid));
    assert!(matches!(
        value(&ledger.snapshot(), &usd_marks, &policy),
        Err(MarginError::UnsupportedInstrument { .. })
    ));
}

#[test]
fn ids_are_shared_across_executions_funding_and_expiry() {
    let mut ledger = ledger_with_cash(dec!(100));
    book(
        &mut ledger,
        &exec("x", "BTC-PERP", Action::Buy, dec!(100), dec!(1)),
    );
    ledger
        .book_funding(&FundingPayment {
            id: eid("f"),
            instrument: iid("BTC-PERP"),
            amount: dec!(1),
            as_of_ms: 1,
        })
        .unwrap();
    // An execution with the funding id, and an expiry with the execution id.
    assert!(matches!(
        ledger.book_execution_allowing_overdraft(
            &exec("f", "BTC-PERP", Action::Buy, dec!(100), dec!(1)),
            None
        ),
        Err(MarginError::ConflictingEntry { .. })
    ));
    assert!(matches!(
        ledger.settle_expiry(&ExpirySettlement {
            id: eid("x"),
            instrument: iid("BTC-DEC"),
            price: dec!(1),
            as_of_ms: 1000,
        }),
        Err(MarginError::ConflictingEntry { .. })
    ));
    ledger.reconcile().unwrap();
}

#[test]
fn a_future_fill_needs_a_time_before_the_expiry() {
    let mut ledger = ledger_with_cash(dec!(100));
    let fill = |id: &str| Execution::new(eid(id), iid("BTC-DEC"), Action::Buy, dec!(50), dec!(1));
    let before = ledger.snapshot();
    // No time: refused, no implicit assumption, for a future and a
    // perpetual alike.
    for instrument in ["BTC-DEC", "BTC-PERP"] {
        let untimed = Execution::new(eid("e1"), iid(instrument), Action::Buy, dec!(50), dec!(1));
        assert!(matches!(
            ledger.book_execution_allowing_overdraft(&untimed, None),
            Err(MarginError::InvalidInput {
                field: "executed_at_ms",
                ..
            })
        ));
    }
    // At the expiry (1000) or after: causally impossible, even before the
    // future is settled.
    for at in [1000, 1001] {
        assert_eq!(
            ledger.book_execution_allowing_overdraft(&fill("e1").with_executed_at(at), None),
            Err(MarginError::ExecutedAfterExpiry {
                instrument: iid("BTC-DEC"),
                executed_at_ms: at,
                expiry_ms: 1000,
            })
        );
    }
    assert_eq!(ledger.snapshot(), before);

    // Just before the expiry it books.
    book(&mut ledger, &fill("e1").with_executed_at(999));
    // The time is part of the execution: the same id at another time
    // conflicts, the identical repeat is a duplicate.
    assert!(matches!(
        ledger.book_execution_allowing_overdraft(&fill("e1").with_executed_at(998), None),
        Err(MarginError::ConflictingEntry { .. })
    ));
    assert!(!book(&mut ledger, &fill("e1").with_executed_at(999)).is_new());

    // After the settlement, a fill executed after the expiry is still
    // refused, with no effect.
    ledger
        .settle_expiry(&ExpirySettlement {
            id: eid("s1"),
            instrument: iid("BTC-DEC"),
            price: dec!(52.5),
            as_of_ms: 1000,
        })
        .unwrap();
    let settled = ledger.snapshot();
    assert!(matches!(
        ledger.book_execution_allowing_overdraft(&fill("e2").with_executed_at(1200), None),
        Err(MarginError::ExecutedAfterExpiry { .. })
    ));
    assert_eq!(ledger.snapshot(), settled);
    ledger.reconcile().unwrap();
}

#[test]
fn a_late_fill_at_decimal_max_keeps_its_close_wide() {
    // BIG-FUT settled flat at MAX; a late buy of 2 at MAX executed at 999:
    // the provisional basis 2 * MAX never fits, but the realized PnL
    // rhe(MAX * 2) - rhe(MAX * 2) = 0 does. The mirror sell, too.
    let max = Decimal::MAX;
    for side in [Action::Buy, Action::Sell] {
        let mut ledger = ledger();
        ledger
            .settle_expiry(&ExpirySettlement {
                id: eid("s1"),
                instrument: iid("BIG-FUT"),
                price: max,
                as_of_ms: 1000,
            })
            .unwrap();
        let late =
            Execution::new(eid("e1"), iid("BIG-FUT"), side, max, dec!(2)).with_executed_at(999);
        let booked = book(&mut ledger, &late);
        let flow = booked.cashflow();
        assert_eq!(flow.realized_pnl, dec!(0));
        let fill = flow.derivative.as_ref().unwrap();
        assert_eq!(fill.virtual_notional, None);
        assert_eq!(fill.closing_notional, None);
        assert_eq!(fill.released_basis, dec!(0));
        assert_eq!(fill.closed_at_settlement, Some(max));
        let position = ledger.position(&iid("BIG-FUT")).unwrap();
        assert_eq!(
            (
                position.quantity,
                position.cost_basis,
                position.realized_pnl
            ),
            (dec!(0), dec!(0), dec!(0))
        );
        assert_eq!(ledger.balance(&asset("BIG")).unwrap().total, dec!(0));
        assert!(!book(&mut ledger, &late).is_new());
        ledger.reconcile().unwrap();
    }
}
