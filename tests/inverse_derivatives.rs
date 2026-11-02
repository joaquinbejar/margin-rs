//! Hand checked fixtures for inverse futures and perpetuals held in the
//! ledger (#26, part A).
//!
//! BTC is the base and settlement asset, at 8 decimals; every contract is
//! worth CV = 100 (USD) and prices are in USD per BTC. A fill of `n`
//! contracts at `p` is worth `V = rhe(n * 100 / p)` BTC; a long gains when
//! that value falls: `realized = Q * CV * (1/entry - 1/exit)`.
//!
//! Every expected value is written out by hand (and checked against exact
//! fractions); none is computed by the code under test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::str::FromStr;

use margin_rs::ledger::{
    Action, Execution, ExecutionCashflow, ExpirySettlement, FundingPayment, InverseResidual, Ledger,
};
use margin_rs::valuation::{Mark, MarkSet, PriceSource, ValuationPolicy, value};
use margin_rs::{
    Asset, ContractSize, Decimal, ExecutionId, InstrumentId, InverseFuture, InversePerpetual,
    InverseTerms, MarginError, PositionKind, TransferId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}

fn btc() -> Asset {
    asset("BTC")
}

fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).unwrap()
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}

fn terms(id: &str) -> InverseTerms {
    InverseTerms::new(iid(id), btc(), dec!(100), 1, 0).unwrap()
}

/// BTC at 8 decimals holding 1 BTC, with BTC-INV (perpetual) and BTC-DEC
/// (future expiring at 1000), both CV = 100, price scale 1, whole
/// contracts.
fn ledger() -> Ledger {
    let mut ledger = Ledger::default();
    ledger.register_asset(btc(), 8).unwrap();
    ledger
        .register_instrument(InversePerpetual::new(terms("BTC-INV")))
        .unwrap();
    ledger
        .register_instrument(InverseFuture::new(terms("BTC-DEC"), 1000))
        .unwrap();
    ledger
        .deposit(TransferId::new("d").unwrap(), btc(), dec!(1))
        .unwrap();
    ledger
}

fn exec(id: &str, instrument: &str, side: Action, price: Decimal, contracts: Decimal) -> Execution {
    Execution::new(eid(id), iid(instrument), side, price, contracts).with_executed_at(500)
}

fn book(ledger: &mut Ledger, execution: &Execution) -> ExecutionCashflow {
    ledger
        .book_execution_allowing_overdraft(execution, None)
        .unwrap()
        .booking
        .cashflow()
        .clone()
}

fn position(ledger: &Ledger, instrument: &str) -> (Decimal, Decimal, Decimal) {
    let p = ledger.position(&iid(instrument)).unwrap();
    (p.quantity, p.cost_basis, p.realized_pnl)
}

fn res(numerator: Decimal, denominator: Decimal) -> InverseResidual {
    InverseResidual {
        numerator,
        denominator,
    }
}

/// Long 100 at 50000 (V = 0.2) closed at 40000 (V = 0.25: -0.05) or at
/// 62500 (V = 0.16: +0.04). The USD value of the fill is `V * p = 100 *
/// 100 = 10000`.
#[test]
fn long_round_trips() {
    for (exit, value, pnl) in [
        (dec!(40000), dec!(0.25), dec!(-0.05)),
        (dec!(62500), dec!(0.16), dec!(0.04)),
    ] {
        let mut ledger = ledger();
        let open = book(
            &mut ledger,
            &exec("o", "BTC-INV", Action::Buy, dec!(50000), dec!(100)),
        );
        let fill = open.derivative.clone().unwrap();
        assert_eq!(fill.size, ContractSize::ContractValue(dec!(100)));
        assert_eq!(fill.virtual_notional, Some(dec!(0.2)));
        assert_eq!(fill.residual, Some(res(dec!(0), dec!(50000))));
        assert_eq!(dec!(0.2) * dec!(50000), dec!(10000));
        assert_eq!(open.deltas, BTreeMap::from([(btc(), dec!(0))]));
        assert_eq!(
            position(&ledger, "BTC-INV"),
            (dec!(100), dec!(0.2), dec!(0))
        );
        let snapshot = ledger.position(&iid("BTC-INV")).unwrap();
        assert_eq!(snapshot.kind, PositionKind::InversePerpetual);
        assert_eq!(snapshot.average_price(), Some(dec!(50000)));

        let close = book(
            &mut ledger,
            &exec("c", "BTC-INV", Action::Sell, exit, dec!(100)),
        );
        let fill = close.derivative.unwrap();
        assert_eq!(fill.virtual_notional, Some(value));
        assert_eq!(fill.released_basis, dec!(0.2));
        assert_eq!(close.realized_pnl, pnl);
        assert_eq!(close.deltas, BTreeMap::from([(btc(), pnl)]));
        assert_eq!(position(&ledger, "BTC-INV"), (dec!(0), dec!(0), pnl));
        assert_eq!(ledger.balance(&btc()).unwrap().total, dec!(1) + pnl);
        assert!(ledger.reconcile().unwrap().is_clean());
    }
}

/// Short 100 at 50000 (basis -0.2), bought back at 40000 (+0.05) or at
/// 62500 (-0.04).
#[test]
fn short_round_trips() {
    for (exit, pnl) in [(dec!(40000), dec!(0.05)), (dec!(62500), dec!(-0.04))] {
        let mut ledger = ledger();
        book(
            &mut ledger,
            &exec("o", "BTC-INV", Action::Sell, dec!(50000), dec!(100)),
        );
        assert_eq!(
            position(&ledger, "BTC-INV"),
            (dec!(-100), dec!(-0.2), dec!(0))
        );
        let close = book(
            &mut ledger,
            &exec("c", "BTC-INV", Action::Buy, exit, dec!(100)),
        );
        assert_eq!(close.derivative.unwrap().released_basis, dec!(-0.2));
        assert_eq!(close.realized_pnl, pnl);
        assert_eq!(position(&ledger, "BTC-INV"), (dec!(0), dec!(0), pnl));
        assert!(ledger.reconcile().unwrap().is_clean());
    }
}

/// Equal moves of ±10 % from 50000 give unequal PnL on a long: at 45000
/// the value is rhe(0.2222...) = 0.22222222 (upnl -0.02222222), at 55000
/// rhe(0.181818...) = 0.18181818 (upnl +0.01818182).
#[test]
fn pnl_is_convex_in_the_price() {
    let mut ledger = ledger();
    book(
        &mut ledger,
        &exec("o", "BTC-INV", Action::Buy, dec!(50000), dec!(100)),
    );
    for (mark, market_value, upnl) in [
        (dec!(45000), dec!(0.22222222), dec!(-0.02222222)),
        (dec!(55000), dec!(0.18181818), dec!(0.01818182)),
    ] {
        let valuation = ledger
            .valuation(&BTreeMap::from([(iid("BTC-INV"), mark)]))
            .unwrap();
        let line = &valuation.positions[&iid("BTC-INV")];
        assert_eq!(
            (line.market_value, line.unrealized_pnl),
            (market_value, upnl)
        );
        assert_eq!(valuation.by_asset[&btc()].unrealized_pnl, upnl);

        // The account valuation agrees, and collateral is cash + upnl.
        let marks =
            MarkSet::new(10).with_instrument(iid("BTC-INV"), Mark::new(mark, 10, PriceSource::Mid));
        let account = value(
            &ledger.snapshot(),
            &marks,
            &ValuationPolicy::new(btc(), 100),
        )
        .unwrap();
        let derivative = &account.derivatives[&iid("BTC-INV")];
        assert_eq!(derivative.market_value, market_value);
        assert_eq!(derivative.unrealized_pnl, upnl);
        assert_eq!(account.collateral_equity, Some(dec!(1) + upnl));
        assert_eq!(account.equity, Some(dec!(1) + upnl));
    }
}

/// Rounding to the satoshi with the exact residual: 3 contracts at 33333
/// are worth 300 / 33333 = 0.00900009000090... BTC, booked as 0.00900009
/// with residual 0.00000003 / 33333 (300 - 0.00900009 * 33333 =
/// 0.00000003). Ties: 1 at 2e10 is 0.000000005, rounded to 0 (even), with
/// residual +100 / 2e10; 3 at 2e10 is 0.000000015, rounded to 0.00000002,
/// with residual -100 / 2e10.
#[test]
fn values_round_to_the_satoshi_with_an_exact_residual() {
    let cases = [
        (dec!(3), dec!(33333), dec!(0.00900009), dec!(0.00000003)),
        (dec!(1), dec!(20000000000), dec!(0), dec!(100)),
        (dec!(3), dec!(20000000000), dec!(0.00000002), dec!(-100)),
    ];
    for (contracts, price, booked, numerator) in cases {
        let mut ledger = ledger();
        let fill = book(
            &mut ledger,
            &exec("o", "BTC-INV", Action::Buy, price, contracts),
        )
        .derivative
        .unwrap();
        assert_eq!(fill.virtual_notional, Some(booked));
        assert_eq!(fill.residual, Some(res(numerator, price)));
        // Exact reconstruction: n * CV = V * p + numerator.
        assert_eq!(contracts * dec!(100), booked * price + numerator);
        assert_eq!(position(&ledger, "BTC-INV").1, booked);
        assert!(ledger.reconcile().unwrap().is_clean());
    }
}

/// A basis rounded to zero leaves an open position without an average
/// price.
#[test]
fn zero_basis_has_no_average_price() {
    let mut ledger = ledger();
    book(
        &mut ledger,
        &exec("o", "BTC-INV", Action::Buy, dec!(20000000000), dec!(1)),
    );
    let snapshot = ledger.position(&iid("BTC-INV")).unwrap();
    assert_eq!((snapshot.quantity, snapshot.cost_basis), (dec!(1), dec!(0)));
    assert_eq!(snapshot.average_price(), None);
}

/// The average entry price is the harmonic mean: 200 contracts bought at
/// 50000 (0.2) and 40000 (0.25) have basis 0.45 and average price
/// 20000 / 0.45 = 44444.444..., rounded to the 24 decimals a Decimal holds.
#[test]
fn average_price_is_harmonic() {
    let mut ledger = ledger();
    book(
        &mut ledger,
        &exec("a", "BTC-INV", Action::Buy, dec!(50000), dec!(100)),
    );
    book(
        &mut ledger,
        &exec("b", "BTC-INV", Action::Buy, dec!(40000), dec!(100)),
    );
    let snapshot = ledger.position(&iid("BTC-INV")).unwrap();
    assert_eq!(snapshot.cost_basis, dec!(0.45));
    assert_eq!(
        snapshot.average_price(),
        Some(Decimal::from_str("44444.444444444444444444444444").unwrap())
    );
}

/// Crossing zero in both directions, with satoshi rounding. Long 3 at
/// 33333 (basis 0.00900009), then sell 5 at 33334:
/// - V = rhe(500 / 33334) = 0.01499970 (residual 0.0000002 / 33334);
/// - C = rhe(300 / 33334) = 0.00899982 (residual 0.00000012 / 33334);
/// - O = V - C = 0.00599988;
/// - realized = basis - C = 0.00000027, new basis -O, 2 contracts short.
///
/// The mirror (short 3 at 33333, buy 5 at 33334) realizes -0.00000027
/// with new basis +0.00599988.
#[test]
fn crossing_zero_in_both_directions() {
    for (open, cross, realized, basis, contracts) in [
        (
            Action::Buy,
            Action::Sell,
            dec!(0.00000027),
            dec!(-0.00599988),
            dec!(-2),
        ),
        (
            Action::Sell,
            Action::Buy,
            dec!(-0.00000027),
            dec!(0.00599988),
            dec!(2),
        ),
    ] {
        let mut ledger = ledger();
        book(
            &mut ledger,
            &exec("o", "BTC-INV", open, dec!(33333), dec!(3)),
        );
        let flow = book(
            &mut ledger,
            &exec("x", "BTC-INV", cross, dec!(33334), dec!(5)),
        );
        let fill = flow.derivative.unwrap();
        assert_eq!(fill.virtual_notional, Some(dec!(0.0149997)));
        assert_eq!(fill.closing_notional, Some(dec!(0.00899982)));
        assert_eq!(fill.residual, Some(res(dec!(0.0000002), dec!(33334))));
        assert_eq!(
            fill.closing_residual,
            Some(res(dec!(0.00000012), dec!(33334)))
        );
        assert_eq!(flow.realized_pnl, realized);
        assert_eq!(position(&ledger, "BTC-INV"), (contracts, basis, realized));
        assert!(ledger.reconcile().unwrap().is_clean());
    }
}

/// Near the limits of a Decimal, in a base with no decimals and CV = 1 at
/// price 1: open 4e28 contracts, then cross with 7.9e28. V = 7.9e28, C =
/// 4e28, O = 3.9e28, realized 0. Then the mirror.
#[test]
fn near_decimal_max_in_both_directions() {
    for (open, cross) in [(Action::Buy, Action::Sell), (Action::Sell, Action::Buy)] {
        let mut ledger = Ledger::default();
        ledger.register_asset(asset("BIG"), 0).unwrap();
        ledger
            .register_instrument(InversePerpetual::new(
                InverseTerms::new(iid("BIG-INV"), asset("BIG"), dec!(1), 0, 0).unwrap(),
            ))
            .unwrap();
        let big = dec!(40000000000000000000000000000);
        let bigger = dec!(79000000000000000000000000000);
        book(&mut ledger, &exec("o", "BIG-INV", open, dec!(1), big));
        let flow = book(&mut ledger, &exec("x", "BIG-INV", cross, dec!(1), bigger));
        assert_eq!(flow.realized_pnl, dec!(0));
        let rest = dec!(39000000000000000000000000000);
        let (quantity, basis, _) = position(&ledger, "BIG-INV");
        assert_eq!((quantity.abs(), basis.abs()), (rest, rest));
        assert_eq!(quantity.is_sign_negative(), basis.is_sign_negative());
        assert!(ledger.reconcile().unwrap().is_clean());
    }
}

/// Settlement at expiry closes at the settlement price with the reduce
/// rule: long 3 at 33333 settled at 40000 realizes 0.00900009 - 0.0075 =
/// 0.00150009. A late fill (executed before the expiry, delivered after
/// the settlement) is closed at once at that price, with both residuals.
#[test]
fn expiry_and_late_fills() {
    let mut ledger = ledger();
    book(
        &mut ledger,
        &exec("o", "BTC-DEC", Action::Buy, dec!(33333), dec!(3)),
    );
    let settled = ledger
        .settle_expiry(&ExpirySettlement {
            id: eid("expiry"),
            instrument: iid("BTC-DEC"),
            price: dec!(40000),
            as_of_ms: 1000,
        })
        .unwrap()
        .cashflow;
    assert_eq!(settled.realized_pnl, dec!(0.00150009));
    assert_eq!(settled.derivative.virtual_notional, Some(dec!(0.0075)));
    assert_eq!(settled.derivative.residual, Some(res(dec!(0), dec!(40000))));
    assert_eq!(
        position(&ledger, "BTC-DEC"),
        (dec!(0), dec!(0), dec!(0.00150009))
    );

    let late = book(
        &mut ledger,
        &Execution::new(
            eid("late"),
            iid("BTC-DEC"),
            Action::Buy,
            dec!(33333),
            dec!(3),
        )
        .with_executed_at(900),
    );
    let fill = late.derivative.unwrap();
    assert_eq!(fill.closed_at_settlement, Some(dec!(40000)));
    assert_eq!(late.realized_pnl, dec!(0.00150009));
    assert_eq!(fill.residual, Some(res(dec!(0.00000003), dec!(33333))));
    assert_eq!(fill.closing_residual, Some(res(dec!(0), dec!(40000))));
    assert_eq!(position(&ledger, "BTC-DEC").0, dec!(0));
    assert!(ledger.reconcile().unwrap().is_clean());

    // A fill executed at the expiry is causally impossible.
    assert!(matches!(
        ledger.book_execution_allowing_overdraft(
            &Execution::new(eid("after"), iid("BTC-DEC"), Action::Buy, dec!(1), dec!(1))
                .with_executed_at(1000),
            None
        ),
        Err(MarginError::ExecutedAfterExpiry { .. })
    ));
}

/// Funding in the base. The funding formula used by the arbitrage crate,
/// `Qc * CV * rate / mark` with positive = paid, gives for a long 100 at
/// mark 50000 and rate 0.0001: 100 * 100 * 0.0001 / 50000 = 0.00002 paid.
/// This ledger books positive = received, so the payment is -0.00002.
#[test]
fn funding_crosses_the_paid_and_received_conventions() {
    let mut ledger = ledger();
    book(
        &mut ledger,
        &exec("o", "BTC-INV", Action::Buy, dec!(50000), dec!(100)),
    );
    let paid = dec!(100) * dec!(100) * dec!(0.0001) / dec!(50000);
    assert_eq!(paid, dec!(0.00002));
    let booking = ledger
        .book_funding(&FundingPayment {
            id: eid("f"),
            instrument: iid("BTC-INV"),
            amount: -paid,
            as_of_ms: 600,
        })
        .unwrap();
    assert_eq!(booking.cashflow.asset, btc());
    assert_eq!(ledger.funding(&btc()), dec!(-0.00002));
    assert_eq!(ledger.balance(&btc()).unwrap().total, dec!(0.99998));
    assert!(ledger.reconcile().unwrap().is_clean());
    // Funding is paid on perpetuals only.
    assert!(matches!(
        ledger.book_funding(&FundingPayment {
            id: eid("g"),
            instrument: iid("BTC-DEC"),
            amount: dec!(1),
            as_of_ms: 600,
        }),
        Err(MarginError::InvalidInput { .. })
    ));
}

/// The settlement scale plus the price scale must not exceed 28, so every
/// residual is an exact Decimal.
#[test]
fn registration_bounds_the_residual_scale() {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("FINE"), 18).unwrap();
    let at = |price_scale| {
        InversePerpetual::new(
            InverseTerms::new(iid("FINE-INV"), asset("FINE"), dec!(100), price_scale, 0).unwrap(),
        )
    };
    assert!(matches!(
        ledger.register_instrument(at(11)),
        Err(MarginError::InvalidInput {
            field: "price_scale",
            ..
        })
    ));
    assert!(ledger.register_instrument(at(10)).is_ok());
}
