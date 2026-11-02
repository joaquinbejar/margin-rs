//! Hand checked ledger fixtures (M1-04).
//!
//! Every expected value below is written out by hand from the rules in
//! `docs/ledger.md`; none is computed by the code under test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::ledger::{Booking, Execution, Ledger, LedgerConfig, Side};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, ReservationId, Rounding,
    SpotInstrument, TransferId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).expect("valid asset")
}

fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).expect("valid execution id")
}

fn tid(id: &str) -> TransferId {
    TransferId::new(id).expect("valid transfer id")
}

fn btc_usdc() -> InstrumentId {
    InstrumentId::new("BTC-USDC").expect("valid instrument id")
}

fn eth_btc() -> InstrumentId {
    InstrumentId::new("ETH-BTC").expect("valid instrument id")
}

/// USDC (6 decimals), BTC, ETH and BNB (8 decimals); BTC-USDC with price
/// scale 2 and quantity scale 8; ETH-BTC with price scale 6 and quantity
/// scale 8.
fn ledger() -> Ledger {
    let mut ledger = Ledger::new(LedgerConfig {
        rounding: Rounding::HalfEven,
    });
    ledger.register_asset(asset("USDC"), 6).expect("register");
    ledger.register_asset(asset("BTC"), 8).expect("register");
    ledger.register_asset(asset("ETH"), 8).expect("register");
    ledger.register_asset(asset("BNB"), 8).expect("register");
    ledger
        .register_instrument(
            SpotInstrument::new(btc_usdc(), asset("BTC"), asset("USDC"), 2, 8).expect("spot"),
        )
        .expect("register");
    ledger
        .register_instrument(
            SpotInstrument::new(eth_btc(), asset("ETH"), asset("BTC"), 6, 8).expect("spot"),
        )
        .expect("register");
    ledger
}

fn exec(id: &str, side: Side, price: Decimal, quantity: Decimal) -> Execution {
    Execution::new(eid(id), btc_usdc(), side, price, quantity)
}

fn total(ledger: &Ledger, code: &str) -> Decimal {
    ledger.balance(&asset(code)).expect("registered").total
}

fn book(ledger: &mut Ledger, execution: &Execution) -> margin_rs::ledger::ExecutionCashflow {
    match ledger.book_execution(execution).expect("booking succeeds") {
        Booking::Booked(cashflow) => cashflow,
        Booking::Duplicate(_) => panic!("unexpected duplicate"),
    }
}

/// Acceptance 1: a buy plus a partial sell reconciles quantity, cash and
/// fees against a hand calculation.
///
/// Deposit 1000 USDC.
/// Buy 2 BTC at 100.00, fee 0.2 USDC: USDC 1000 - 200 - 0.2 = 799.8;
/// BTC 2; cost basis 200.
/// Sell 0.5 BTC at 110.00, fee 0.055 USDC: proceeds 55; cost released
/// 200 * 0.5 / 2 = 50; realized 55 - 50 = 5; USDC 799.8 + 55 - 0.055 =
/// 854.745; BTC 1.5; cost basis 150; fees 0.255.
/// Mark 120: market value 1.5 * 120 = 180; unrealized 180 - 150 = 30;
/// net 5 + 30 - 0.255 = 34.745 = (854.745 + 180) - 1000.
#[test]
fn buy_then_partial_sell_matches_hand_calculation() {
    let mut ledger = ledger();
    let usdc = asset("USDC");
    assert_eq!(
        ledger.deposit(tid("d1"), usdc.clone(), dec!(1000)),
        Ok(true)
    );

    let buy = exec("e1", Side::Buy, dec!(100.00), dec!(2)).with_fee(usdc.clone(), dec!(0.2));
    let cf = book(&mut ledger, &buy);
    assert_eq!(cf.sequence, 0);
    assert_eq!(cf.notional, dec!(200));
    assert_eq!(cf.rounding_residual, Some(dec!(0)));
    assert_eq!(
        cf.deltas,
        BTreeMap::from([(asset("BTC"), dec!(2)), (usdc.clone(), dec!(-200.2))])
    );
    assert_eq!(cf.realized_pnl, dec!(0));

    let sell = exec("e2", Side::Sell, dec!(110.00), dec!(0.5)).with_fee(usdc.clone(), dec!(0.055));
    let cf = book(&mut ledger, &sell);
    assert_eq!(cf.sequence, 1);
    assert_eq!(cf.notional, dec!(55));
    assert_eq!(
        cf.deltas,
        BTreeMap::from([(asset("BTC"), dec!(-0.5)), (usdc.clone(), dec!(54.945))])
    );
    assert_eq!(cf.realized_pnl, dec!(5));
    assert_eq!(cf.position_after.quantity, dec!(1.5));
    assert_eq!(cf.position_after.cost_basis, dec!(150));

    assert_eq!(total(&ledger, "USDC"), dec!(854.745));
    assert_eq!(total(&ledger, "BTC"), dec!(1.5));
    assert_eq!(ledger.fees_paid(&usdc), dec!(0.255));
    let position = ledger.position(&btc_usdc()).expect("registered");
    assert_eq!(position.quantity, dec!(1.5));
    assert_eq!(position.cost_basis, dec!(150));
    assert_eq!(position.realized_pnl, dec!(5));
    assert_eq!(position.average_price(), Some(dec!(100)));
    ledger.reconcile().expect("reconciles");

    let valuation = ledger
        .valuation(&BTreeMap::from([(btc_usdc(), dec!(120))]))
        .expect("valuation");
    let line = valuation.positions[&btc_usdc()].clone();
    assert_eq!(line.market_value, dec!(180));
    assert_eq!(line.unrealized_pnl, dec!(30));
    let pnl = valuation.by_asset[&usdc];
    assert_eq!(pnl.realized_pnl, dec!(5));
    assert_eq!(pnl.unrealized_pnl, dec!(30));
    assert_eq!(pnl.fees_paid, dec!(0.255));
    assert_eq!(pnl.net_pnl, dec!(34.745));
    // Cross-check against equity: cash plus market value minus deposits.
    assert_eq!(
        pnl.net_pnl,
        total(&ledger, "USDC") + line.market_value - dec!(1000)
    );
}

/// Acceptance 2a: reprocessing an execution or a transfer does not
/// duplicate positions or fees; reusing an id for different content fails
/// and changes nothing.
#[test]
fn reprocessing_is_idempotent() {
    let mut ledger = ledger();
    let usdc = asset("USDC");
    ledger
        .deposit(tid("d1"), usdc.clone(), dec!(1000))
        .expect("deposit");
    let buy = exec("e1", Side::Buy, dec!(100), dec!(2)).with_fee(usdc.clone(), dec!(0.2));
    let first = book(&mut ledger, &buy);
    let after_first = ledger.snapshot();

    for _ in 0..3 {
        assert_eq!(
            ledger.book_execution(&buy),
            Ok(Booking::Duplicate(first.clone()))
        );
    }
    assert_eq!(
        ledger.deposit(tid("d1"), usdc.clone(), dec!(1000)),
        Ok(false)
    );
    assert_eq!(ledger.snapshot(), after_first);
    assert_eq!(ledger.journal().len(), 1);
    assert_eq!(ledger.fees_paid(&usdc), dec!(0.2));

    let conflicting = exec("e1", Side::Buy, dec!(100), dec!(3)).with_fee(usdc.clone(), dec!(0.2));
    assert_eq!(
        ledger.book_execution(&conflicting),
        Err(MarginError::ConflictingEntry {
            kind: "execution",
            id: "e1".into(),
        })
    );
    assert_eq!(
        ledger.deposit(tid("d1"), usdc.clone(), dec!(999)),
        Err(MarginError::ConflictingEntry {
            kind: "transfer",
            id: "d1".into(),
        })
    );
    assert_eq!(ledger.snapshot(), after_first);
    assert_eq!(ledger.execution_cashflow(&eid("e1")), Some(&first));
    ledger.reconcile().expect("reconciles");
}

/// Acceptance 2b: fees in different currencies are kept apart and only
/// combined through an explicit conversion.
///
/// Deposit 1000 USDC and 1 BNB. Buy 1 BTC at 100 with a 0.01 BNB fee, then
/// 1 BTC at 100 with a 0.001 BTC fee. USDC 800; BNB 0.99; BTC 2 - 0.001 =
/// 1.999 while the position quantity stays 2 (gross traded quantity).
/// Mark 100: unrealized 0. Net per asset: USDC 0, BTC -0.001, BNB -0.01.
/// With BNB = 300 USDC and BTC = 100 USDC: -3 - 0.1 = -3.1 USDC.
#[test]
fn fees_in_other_currencies_are_never_summed() {
    let mut ledger = ledger();
    let (usdc, btc, bnb) = (asset("USDC"), asset("BTC"), asset("BNB"));
    ledger
        .deposit(tid("d1"), usdc.clone(), dec!(1000))
        .expect("deposit");
    ledger
        .deposit(tid("d2"), bnb.clone(), dec!(1))
        .expect("deposit");
    book(
        &mut ledger,
        &exec("e1", Side::Buy, dec!(100), dec!(1)).with_fee(bnb.clone(), dec!(0.01)),
    );
    let cf = book(
        &mut ledger,
        &exec("e2", Side::Buy, dec!(100), dec!(1)).with_fee(btc.clone(), dec!(0.001)),
    );
    assert_eq!(
        cf.deltas,
        BTreeMap::from([(btc.clone(), dec!(0.999)), (usdc.clone(), dec!(-100))])
    );

    assert_eq!(total(&ledger, "USDC"), dec!(800));
    assert_eq!(total(&ledger, "BNB"), dec!(0.99));
    assert_eq!(total(&ledger, "BTC"), dec!(1.999));
    assert_eq!(ledger.position(&btc_usdc()).expect("pos").quantity, dec!(2));
    assert_eq!(ledger.fees_paid(&usdc), dec!(0));
    assert_eq!(ledger.fees_paid(&btc), dec!(0.001));
    assert_eq!(ledger.fees_paid(&bnb), dec!(0.01));
    ledger.reconcile().expect("reconciles");

    let valuation = ledger
        .valuation(&BTreeMap::from([(btc_usdc(), dec!(100))]))
        .expect("valuation");
    assert_eq!(valuation.by_asset[&usdc].net_pnl, dec!(0));
    assert_eq!(valuation.by_asset[&btc].net_pnl, dec!(-0.001));
    assert_eq!(valuation.by_asset[&bnb].net_pnl, dec!(-0.01));

    assert_eq!(
        valuation.net_in(&usdc, &BTreeMap::new()),
        Err(MarginError::MissingConversionRate {
            from: bnb.clone(),
            to: usdc.clone(),
        })
    );
    let rates = BTreeMap::from([(bnb.clone(), dec!(300)), (btc.clone(), dec!(100))]);
    assert_eq!(valuation.net_in(&usdc, &rates), Ok(dec!(-3.1)));
}

/// Fragmented fills: each fill's notional is rounded to 6 decimals and the
/// residual recorded, so the totals stay exact.
///
/// Three buys at 100.01 of 0.33333333, 0.33333333 and 0.33333334 BTC.
/// Exact notionals 33.3366663333, 33.3366663333, 33.3366673334 (sum 100.01);
/// rounded half even to 33.336666, 33.336666, 33.336667 (sum 100.009999);
/// residuals sum to -0.000001. One single 1 BTC fill would cost 100.01.
#[test]
fn fragmented_fills_round_per_fill_and_reconcile() {
    let mut ledger = ledger();
    ledger
        .deposit(tid("d1"), asset("USDC"), dec!(1000))
        .expect("deposit");
    let fills = [dec!(0.33333333), dec!(0.33333333), dec!(0.33333334)];
    let expected = [dec!(33.336666), dec!(33.336666), dec!(33.336667)];
    let mut exact_sum = Decimal::ZERO;
    let mut residual_sum = Decimal::ZERO;
    for (i, (quantity, notional)) in fills.iter().zip(expected).enumerate() {
        let cf = book(
            &mut ledger,
            &exec(&format!("f{i}"), Side::Buy, dec!(100.01), *quantity),
        );
        assert_eq!(cf.notional, notional);
        exact_sum += cf.notional_exact.expect("representable");
        residual_sum += cf.rounding_residual.expect("representable");
    }
    assert_eq!(exact_sum, dec!(100.01));
    assert_eq!(residual_sum, dec!(-0.000001));
    assert_eq!(total(&ledger, "USDC"), dec!(899.990001));
    let position = ledger.position(&btc_usdc()).expect("pos");
    assert_eq!(position.quantity, dec!(1));
    assert_eq!(position.cost_basis, dec!(100.009999));
    ledger.reconcile().expect("reconciles");
}

/// An exact round trip at one price returns every asset to its start.
///
/// 1.23456789 * 43210.99 = 53346.9007491111, rounded to 53346.900749.
#[test]
fn exact_round_trip_restores_balances() {
    let mut ledger = ledger();
    ledger
        .deposit(tid("d1"), asset("USDC"), dec!(100000))
        .expect("deposit");
    let before = ledger.snapshot();
    let buy = book(
        &mut ledger,
        &exec("b", Side::Buy, dec!(43210.99), dec!(1.23456789)),
    );
    assert_eq!(buy.notional_exact, Some(dec!(53346.9007491111)));
    assert_eq!(buy.notional, dec!(53346.900749));
    book(
        &mut ledger,
        &exec("s", Side::Sell, dec!(43210.99), dec!(1.23456789)),
    );

    let after = ledger.snapshot();
    assert_eq!(after.balances, before.balances);
    let position = ledger.position(&btc_usdc()).expect("pos");
    assert!(position.is_flat());
    assert_eq!(position.cost_basis, dec!(0));
    assert_eq!(position.realized_pnl, dec!(0));
    ledger.reconcile().expect("reconciles");
}

/// Partial reductions release rounded cost, and a full close releases the
/// remainder, so the cost of the position is conserved exactly.
///
/// Buy 1 at 50 and 2 at 25: quantity 3, cost 100. Sell 1 at 40 three times.
/// 1st: released round(100 / 3) = 33.333333, realized 6.666667.
/// 2nd: released round(66.666667 / 2 = 33.3333335) = 33.333334 (half even),
///      realized 6.666666.
/// 3rd: closes, releases the remaining 33.333333, realized 6.666667.
/// Total realized 20 = 3 * 40 - 100.
#[test]
fn partial_reductions_conserve_cost_exactly() {
    let mut ledger = ledger();
    ledger
        .deposit(tid("d1"), asset("USDC"), dec!(100))
        .expect("deposit");
    book(&mut ledger, &exec("b1", Side::Buy, dec!(50), dec!(1)));
    book(&mut ledger, &exec("b2", Side::Buy, dec!(25), dec!(2)));
    let realized: Vec<Decimal> = (0..3)
        .map(|i| {
            book(
                &mut ledger,
                &exec(&format!("s{i}"), Side::Sell, dec!(40), dec!(1)),
            )
            .realized_pnl
        })
        .collect();
    assert_eq!(
        realized,
        vec![dec!(6.666667), dec!(6.666666), dec!(6.666667)]
    );
    let position = ledger.position(&btc_usdc()).expect("pos");
    assert!(position.is_flat());
    assert_eq!(position.cost_basis, dec!(0));
    assert_eq!(position.realized_pnl, dec!(20));
    assert_eq!(total(&ledger, "USDC"), dec!(120));
    ledger.reconcile().expect("reconciles");
}

/// A fill that crosses zero closes the old position and opens a new one.
///
/// Deposit 1000 USDC and 1 BTC. Buy 2 at 100: quantity 2, cost 200.
/// Sell 3 at 110 (notional 330): closing 2 at 220 realizes 20; the
/// remaining 110 opens a short of 1 with cost basis -110.
/// Buy 1 at 100: closes the short, realizes 110 - 100 = 10.
/// Final: USDC 1030, BTC 1, realized 30.
#[test]
fn crossing_zero_splits_the_fill() {
    let mut ledger = ledger();
    ledger
        .deposit(tid("d1"), asset("USDC"), dec!(1000))
        .expect("deposit");
    ledger
        .deposit(tid("d2"), asset("BTC"), dec!(1))
        .expect("deposit");
    book(&mut ledger, &exec("b1", Side::Buy, dec!(100), dec!(2)));
    let cf = book(&mut ledger, &exec("s1", Side::Sell, dec!(110), dec!(3)));
    assert_eq!(cf.realized_pnl, dec!(20));
    assert_eq!(cf.position_after.quantity, dec!(-1));
    assert_eq!(cf.position_after.cost_basis, dec!(-110));
    assert_eq!(total(&ledger, "BTC"), dec!(0));

    // The short gains when the mark falls: market value -1 * 100 = -100,
    // minus cost basis -110, is an unrealized PnL of 10.
    let valuation = ledger
        .valuation(&BTreeMap::from([(btc_usdc(), dec!(100))]))
        .expect("valuation");
    assert_eq!(valuation.positions[&btc_usdc()].market_value, dec!(-100));
    assert_eq!(valuation.positions[&btc_usdc()].unrealized_pnl, dec!(10));

    let cf = book(&mut ledger, &exec("b2", Side::Buy, dec!(100), dec!(1)));
    assert_eq!(cf.realized_pnl, dec!(10));
    assert!(cf.position_after.is_flat());
    assert_eq!(cf.position_after.cost_basis, dec!(0));
    assert_eq!(total(&ledger, "USDC"), dec!(1030));
    assert_eq!(total(&ledger, "BTC"), dec!(1));
    assert_eq!(
        ledger.position(&btc_usdc()).expect("pos").realized_pnl,
        dec!(30)
    );
    ledger.reconcile().expect("reconciles");
    // A flat book needs no marks.
    assert!(ledger.valuation(&BTreeMap::new()).is_ok());
}

/// Overspending is rejected atomically, and reservations reduce what is
/// available.
#[test]
fn insufficient_balance_leaves_the_ledger_unchanged() {
    let mut ledger = ledger();
    let usdc = asset("USDC");
    ledger
        .deposit(tid("d1"), usdc.clone(), dec!(100))
        .expect("deposit");
    let before = ledger.snapshot();

    assert_eq!(
        ledger.book_execution(&exec("e1", Side::Buy, dec!(100), dec!(2))),
        Err(MarginError::InsufficientBalance {
            asset: usdc.clone(),
            required: dec!(200),
            available: dec!(100),
        })
    );
    assert_eq!(
        ledger.book_execution(&exec("e2", Side::Sell, dec!(100), dec!(1))),
        Err(MarginError::InsufficientBalance {
            asset: asset("BTC"),
            required: dec!(1),
            available: dec!(0),
        })
    );
    assert_eq!(
        ledger.withdraw(tid("w1"), usdc.clone(), dec!(100.000001)),
        Err(MarginError::InsufficientBalance {
            asset: usdc.clone(),
            required: dec!(100.000001),
            available: dec!(100),
        })
    );
    assert_eq!(ledger.snapshot(), before);

    let hold = ReservationId::new("r1").expect("id");
    assert_eq!(
        ledger.reserve(hold.clone(), usdc.clone(), dec!(60)),
        Ok(true)
    );
    let balance = ledger.balance(&usdc).expect("balance");
    assert_eq!(
        (balance.total, balance.reserved, balance.available),
        (dec!(100), dec!(60), dec!(40))
    );
    assert_eq!(
        ledger.book_execution(&exec("e3", Side::Buy, dec!(100), dec!(0.5))),
        Err(MarginError::InsufficientBalance {
            asset: usdc.clone(),
            required: dec!(50),
            available: dec!(40),
        })
    );
    assert_eq!(
        ledger.reserve(hold.clone(), usdc.clone(), dec!(61)),
        Err(MarginError::ConflictingEntry {
            kind: "reservation",
            id: "r1".into(),
        })
    );
    ledger.reconcile().expect("reconciles");
    assert_eq!(ledger.release(&hold), Ok((usdc.clone(), dec!(60))));
    assert_eq!(
        ledger.release(&hold),
        Err(MarginError::UnknownReservation { id: "r1".into() })
    );
    book(&mut ledger, &exec("e3", Side::Buy, dec!(100), dec!(0.5)));
    assert_eq!(total(&ledger, "USDC"), dec!(50));
    assert_eq!(ledger.withdraw(tid("w2"), usdc.clone(), dec!(50)), Ok(true));
    assert_eq!(total(&ledger, "USDC"), dec!(0));
    ledger.reconcile().expect("reconciles");
}

/// Inputs finer than the configured precision are rejected, never
/// truncated; overflow is an error, never a saturation.
#[test]
fn precision_and_numeric_limits() {
    let mut ledger = ledger();
    let usdc = asset("USDC");
    ledger
        .deposit(tid("d1"), usdc.clone(), dec!(1000))
        .expect("deposit");
    let before = ledger.snapshot();

    assert_eq!(
        ledger.book_execution(&exec("p", Side::Buy, dec!(100.001), dec!(1))),
        Err(MarginError::PrecisionExceeded {
            field: "price",
            scale: 3,
            max_scale: 2,
        })
    );
    assert_eq!(
        ledger.book_execution(&exec("q", Side::Buy, dec!(100), dec!(0.000000001))),
        Err(MarginError::PrecisionExceeded {
            field: "quantity",
            scale: 9,
            max_scale: 8,
        })
    );
    assert_eq!(
        ledger.book_execution(
            &exec("f", Side::Buy, dec!(100), dec!(1)).with_fee(usdc.clone(), dec!(0.0000001))
        ),
        Err(MarginError::PrecisionExceeded {
            field: "fee",
            scale: 7,
            max_scale: 6,
        })
    );
    // Trailing zeros are not extra precision.
    assert!(
        ledger
            .book_execution(&exec("z", Side::Buy, dec!(100.0000), dec!(0.100000000)))
            .is_ok()
    );
    let before_errors = ledger.snapshot();

    assert_eq!(
        ledger.book_execution(&exec("n0", Side::Buy, dec!(0), dec!(1))),
        Err(MarginError::invalid_input("price", "must be positive"))
    );
    assert_eq!(
        ledger.book_execution(&exec("n1", Side::Buy, dec!(100), dec!(-1))),
        Err(MarginError::invalid_input("quantity", "must be positive"))
    );
    assert_eq!(
        ledger.book_execution(&exec("big", Side::Buy, Decimal::MAX, dec!(2))),
        Err(MarginError::arithmetic("price * quantity"))
    );
    assert_eq!(
        ledger.book_execution(
            &exec("bnb", Side::Buy, dec!(1), dec!(1)).with_fee(asset("XRP"), dec!(1))
        ),
        Err(MarginError::UnknownAsset {
            asset: asset("XRP")
        })
    );
    let unknown = InstrumentId::new("SOL-USDC").expect("id");
    assert_eq!(
        ledger.book_execution(&Execution::new(
            eid("u"),
            unknown.clone(),
            Side::Buy,
            dec!(1),
            dec!(1)
        )),
        Err(MarginError::UnknownInstrument {
            instrument: unknown
        })
    );
    assert_eq!(
        ledger.deposit(tid("max"), usdc.clone(), Decimal::MAX),
        Err(MarginError::arithmetic("balance + transfer"))
    );
    assert_eq!(
        ledger.deposit(tid("neg"), usdc.clone(), dec!(-1)),
        Err(MarginError::invalid_input("amount", "must be positive"))
    );
    assert_eq!(ledger.snapshot(), before_errors);
    assert_ne!(before, before_errors);
    ledger.reconcile().expect("reconciles");
}

/// A negative fee is a rebate: it credits the fee asset and counts as
/// negative fees in the PnL.
#[test]
fn maker_rebate_credits_the_fee_asset() {
    let mut ledger = ledger();
    let usdc = asset("USDC");
    ledger
        .deposit(tid("d1"), usdc.clone(), dec!(1000))
        .expect("deposit");
    let cf = book(
        &mut ledger,
        &exec("e1", Side::Buy, dec!(100), dec!(1)).with_fee(usdc.clone(), dec!(-0.01)),
    );
    assert_eq!(cf.deltas[&usdc], dec!(-99.99));
    assert_eq!(total(&ledger, "USDC"), dec!(900.01));
    assert_eq!(ledger.fees_paid(&usdc), dec!(-0.01));
    let valuation = ledger
        .valuation(&BTreeMap::from([(btc_usdc(), dec!(100))]))
        .expect("valuation");
    assert_eq!(valuation.by_asset[&usdc].net_pnl, dec!(0.01));
    ledger.reconcile().expect("reconciles");
}

/// Positions quoted in different assets report PnL in their own asset.
///
/// Deposit 10 BTC. Buy 10 ETH at 0.05 BTC: cost 0.5 BTC. Mark 0.06:
/// market value 0.6 BTC, unrealized 0.1 BTC, in the BTC line only. Open
/// positions need a mark.
#[test]
fn pnl_is_reported_in_each_quote_asset() {
    let mut ledger = ledger();
    let btc = asset("BTC");
    ledger
        .deposit(tid("d1"), btc.clone(), dec!(10))
        .expect("deposit");
    book(
        &mut ledger,
        &Execution::new(eid("e1"), eth_btc(), Side::Buy, dec!(0.05), dec!(10)),
    );
    assert_eq!(total(&ledger, "BTC"), dec!(9.5));
    assert_eq!(total(&ledger, "ETH"), dec!(10));

    assert_eq!(
        ledger.valuation(&BTreeMap::new()),
        Err(MarginError::MissingMark {
            instrument: eth_btc()
        })
    );
    let valuation = ledger
        .valuation(&BTreeMap::from([(eth_btc(), dec!(0.06))]))
        .expect("valuation");
    assert_eq!(valuation.by_asset[&btc].unrealized_pnl, dec!(0.1));
    assert_eq!(valuation.by_asset[&asset("USDC")].unrealized_pnl, dec!(0));
    assert_eq!(
        valuation.net_in(
            &asset("USDC"),
            &BTreeMap::from([(btc.clone(), dec!(100000))])
        ),
        Ok(dec!(10000))
    );
    ledger.reconcile().expect("reconciles");
}

/// Registration is idempotent for identical definitions only.
#[test]
fn registration_rules() {
    let mut ledger = ledger();
    assert_eq!(ledger.register_asset(asset("USDC"), 6), Ok(()));
    assert_eq!(
        ledger.register_asset(asset("USDC"), 2),
        Err(MarginError::AlreadyRegistered {
            kind: "asset",
            id: "USDC".into(),
        })
    );
    let spot = SpotInstrument::new(btc_usdc(), asset("BTC"), asset("USDC"), 2, 8).expect("spot");
    assert_eq!(ledger.register_instrument(spot), Ok(()));
    let other = SpotInstrument::new(btc_usdc(), asset("BTC"), asset("USDC"), 1, 8).expect("spot");
    assert_eq!(
        ledger.register_instrument(other),
        Err(MarginError::AlreadyRegistered {
            kind: "instrument",
            id: "BTC-USDC".into(),
        })
    );
    let fine = SpotInstrument::new(
        InstrumentId::new("BTC-USDC-FINE").expect("id"),
        asset("BTC"),
        asset("USDC"),
        2,
        9,
    )
    .expect("spot");
    assert_eq!(
        ledger.register_instrument(fine),
        Err(MarginError::invalid_input(
            "quantity_scale",
            "must not exceed the base asset scale"
        ))
    );
    let unknown = SpotInstrument::new(
        InstrumentId::new("SOL-USDC").expect("id"),
        asset("SOL"),
        asset("USDC"),
        2,
        8,
    )
    .expect("spot");
    assert_eq!(
        ledger.register_instrument(unknown),
        Err(MarginError::UnknownAsset {
            asset: asset("SOL")
        })
    );
}

/// M104-1: a deposit whose sum `rust_decimal` would round is rejected and
/// leaves the balance unchanged, in both orders.
#[test]
fn deposits_that_would_lose_digits_are_rejected() {
    for (first, second) in [(dec!(0.1), Decimal::MAX), (Decimal::MAX, dec!(0.1))] {
        let mut ledger = Ledger::default();
        let usd = asset("USD");
        ledger.register_asset(usd.clone(), 1).expect("register");
        assert_eq!(ledger.deposit(tid("a"), usd.clone(), first), Ok(true));
        let before = ledger.snapshot();
        assert_eq!(
            ledger.deposit(tid("b"), usd.clone(), second),
            Err(MarginError::arithmetic("balance + transfer"))
        );
        assert_eq!(ledger.snapshot(), before);
        assert_eq!(total(&ledger, "USD"), first);
        ledger.reconcile().expect("reconciles");
    }
}

/// M104-2: the cost released by a partial close is rounded once from the
/// exact ratio.
///
/// Quote scale 18, base scale 0, price scale 18. Buy 2e18 units at 1e-18
/// (cost 2) and 1 unit at 1 (cost 3, quantity 2000000000000000001). Sell 1
/// at 1e-18: the exact released cost 3 / 2000000000000000001 is just below
/// 1.5e-18, so half even releases 1e-18 and the realized PnL is 0.
#[test]
fn partial_close_releases_cost_rounded_once() {
    let mut ledger = Ledger::default();
    let (quote, base) = (asset("QUOTE"), asset("UNIT"));
    let id = InstrumentId::new("UNIT-QUOTE").expect("id");
    ledger.register_asset(quote.clone(), 18).expect("register");
    ledger.register_asset(base.clone(), 0).expect("register");
    ledger
        .register_instrument(
            SpotInstrument::new(id.clone(), base, quote.clone(), 18, 0).expect("spot"),
        )
        .expect("register");
    ledger.deposit(tid("d"), quote, dec!(3)).expect("deposit");
    let tick = dec!(0.000000000000000001);
    let fill = |name: &str, side, price, quantity| {
        Execution::new(eid(name), id.clone(), side, price, quantity)
    };
    book(
        &mut ledger,
        &fill("b1", Side::Buy, tick, dec!(2000000000000000000)),
    );
    book(&mut ledger, &fill("b2", Side::Buy, dec!(1), dec!(1)));
    let cf = book(&mut ledger, &fill("s1", Side::Sell, tick, dec!(1)));
    assert_eq!(cf.realized_pnl, dec!(0));
    assert_eq!(cf.position_after.quantity, dec!(2000000000000000000));
    assert_eq!(cf.position_after.cost_basis, dec!(2.999999999999999999));
    ledger.reconcile().expect("reconciles");
}

/// M104-4: an 18/18 position (ETH quantity scale 18 against DAI scale 18)
/// can be opened, reduced and valued, with price scales 2 and 18.
///
/// Buy 1.123456789012345678 ETH at 3000.12: notional 3370.505181851718515481
/// (exact 3370.50518185171851548136). Sell 0.123456789012345678 at 3000.12:
/// proceeds 370.385181851718515481, released cost the same, realized 0,
/// cost basis 3000.12 for 1 ETH. Mark 3000.123456789012345678: market value
/// 3000.123456789012345678, unrealized 0.003456789012345678.
#[test]
fn eighteen_digit_scales_open_reduce_and_value() {
    let mut ledger = Ledger::default();
    let (dai, eth) = (asset("DAI"), asset("ETH"));
    let pair = InstrumentId::new("ETH-DAI").expect("id");
    let fine = InstrumentId::new("ETH-DAI-FINE").expect("id");
    ledger.register_asset(dai.clone(), 18).expect("register");
    ledger.register_asset(eth.clone(), 18).expect("register");
    ledger
        .register_instrument(
            SpotInstrument::new(pair.clone(), eth.clone(), dai.clone(), 2, 18).expect("spot"),
        )
        .expect("register");
    ledger
        .register_instrument(
            SpotInstrument::new(fine.clone(), eth.clone(), dai.clone(), 18, 18).expect("spot"),
        )
        .expect("register");
    ledger
        .deposit(tid("d"), dai.clone(), dec!(10000))
        .expect("deposit");

    let buy = book(
        &mut ledger,
        &Execution::new(
            eid("b"),
            pair.clone(),
            Side::Buy,
            dec!(3000.12),
            dec!(1.123456789012345678),
        ),
    );
    assert_eq!(buy.notional, dec!(3370.505181851718515481));
    assert_eq!(buy.notional_exact, Some(dec!(3370.50518185171851548136)));
    assert_eq!(buy.rounding_residual, Some(dec!(-0.00000000000000000036)));
    let sell = book(
        &mut ledger,
        &Execution::new(
            eid("s"),
            pair.clone(),
            Side::Sell,
            dec!(3000.12),
            dec!(0.123456789012345678),
        ),
    );
    assert_eq!(sell.notional, dec!(370.385181851718515481));
    assert_eq!(sell.realized_pnl, dec!(0));
    assert_eq!(sell.position_after.quantity, dec!(1));
    assert_eq!(sell.position_after.cost_basis, dec!(3000.12));
    assert_eq!(total(&ledger, "DAI"), dec!(6999.88));

    let valuation = ledger
        .valuation(&BTreeMap::from([(
            pair.clone(),
            dec!(3000.123456789012345678),
        )]))
        .expect("valuation");
    assert_eq!(
        valuation.positions[&pair].market_value,
        dec!(3000.123456789012345678)
    );
    assert_eq!(
        valuation.positions[&pair].unrealized_pnl,
        dec!(0.003456789012345678)
    );

    // Price scale 18 too: 1.123456789012345678 * 3000.123456789012345678
    // has 36 fractional digits; rounded half even at 18 it is
    // 3370.509065404802618515.
    let fine_buy = book(
        &mut ledger,
        &Execution::new(
            eid("fb"),
            fine.clone(),
            Side::Buy,
            dec!(3000.123456789012345678),
            dec!(1.123456789012345678),
        ),
    );
    assert_eq!(fine_buy.notional, dec!(3370.509065404802618515));
    // The exact product needs 36 fractional digits: not representable.
    assert_eq!(fine_buy.notional_exact, None);
    assert_eq!(fine_buy.rounding_residual, None);
    ledger.reconcile().expect("reconciles");
}

/// `book_execution_with_reservation` books against the reservation it
/// shrinks, atomically, and validates the new reservation amount.
///
/// 100 USDC with 100 reserved: a plain buy of 1 at 60 fails, but booked
/// against the reservation (shrunk to 40) it succeeds: total 40, reserved
/// 40, available 0. A duplicate leaves the reservation at 40.
#[test]
fn booking_against_a_reservation_is_atomic() {
    let mut ledger = ledger();
    let usdc = asset("USDC");
    ledger
        .deposit(tid("d1"), usdc.clone(), dec!(100))
        .expect("deposit");
    let hold = ReservationId::new("r1").expect("id");
    ledger
        .reserve(hold.clone(), usdc.clone(), dec!(100))
        .expect("reserve");
    let e1 = exec("e1", Side::Buy, dec!(60), dec!(1));
    assert!(matches!(
        ledger.book_execution(&e1),
        Err(MarginError::InsufficientBalance { .. })
    ));
    let before = ledger.snapshot();
    assert_eq!(
        ledger.book_execution_with_reservation(&e1, &hold, dec!(101)),
        Err(MarginError::invalid_input(
            "remaining_reserved",
            "must be between zero and the current reservation"
        ))
    );
    assert_eq!(
        ledger.book_execution_with_reservation(&e1, &hold, dec!(0.0000001)),
        Err(MarginError::PrecisionExceeded {
            field: "remaining_reserved",
            scale: 7,
            max_scale: 6,
        })
    );
    let other = ReservationId::new("r2").expect("id");
    assert_eq!(
        ledger.book_execution_with_reservation(&e1, &other, dec!(0)),
        Err(MarginError::UnknownReservation { id: "r2".into() })
    );
    // Shrinking to 50 frees only 50: not enough for 60.
    assert!(matches!(
        ledger.book_execution_with_reservation(&e1, &hold, dec!(50)),
        Err(MarginError::InsufficientBalance { .. })
    ));
    assert_eq!(ledger.snapshot(), before);

    let first = ledger
        .book_execution_with_reservation(&e1, &hold, dec!(40))
        .expect("books");
    let b = ledger.balance(&usdc).expect("balance");
    assert_eq!(
        (b.total, b.reserved, b.available),
        (dec!(40), dec!(40), dec!(0))
    );
    assert_eq!(
        ledger.book_execution_with_reservation(&e1, &hold, dec!(0)),
        Ok(Booking::Duplicate(first.cashflow().clone()))
    );
    assert_eq!(ledger.balance(&usdc).expect("balance").reserved, dec!(40));
    ledger.reconcile().expect("reconciles");
}

/// A real execution can be booked past the available balance: the
/// shortfall is returned, kept in `overdrafts` and reported by `reconcile`,
/// whose sums still agree. Ordinary booking still refuses it.
#[test]
fn real_execution_can_overdraw() {
    let mut ledger = ledger();
    let usdc = asset("USDC");
    ledger
        .deposit(tid("d1"), usdc.clone(), dec!(50))
        .expect("deposit");
    let e1 = exec("e1", Side::Buy, dec!(100), dec!(1));
    assert!(matches!(
        ledger.book_execution(&e1),
        Err(MarginError::InsufficientBalance { .. })
    ));
    let booked = ledger
        .book_execution_allowing_overdraft(&e1, None)
        .expect("booked");
    assert!(booked.booking.is_new());
    assert_eq!(
        booked.shortfalls,
        BTreeMap::from([(usdc.clone(), dec!(50))])
    );
    assert_eq!(total(&ledger, "USDC"), dec!(-50));
    assert_eq!(
        ledger.overdrafts(),
        BTreeMap::from([(usdc.clone(), dec!(50))])
    );
    let reconciliation = ledger.reconcile().expect("sums agree");
    assert!(!reconciliation.is_clean());
    // Repeating it is a duplicate with no new shortfall.
    let again = ledger
        .book_execution_allowing_overdraft(&e1, None)
        .expect("duplicate");
    assert!(!again.booking.is_new());
    assert!(again.shortfalls.is_empty());
    // A deposit clears the overdraft.
    ledger.deposit(tid("d2"), usdc, dec!(50)).expect("deposit");
    assert!(ledger.reconcile().expect("reconciles").is_clean());
}

/// `set_reservation` is atomic: it resizes in one step, shrinking always
/// works even on an overdrawn balance, and an uncovered increase changes
/// nothing.
#[test]
fn set_reservation_is_atomic_even_when_overdrawn() {
    let mut ledger = ledger();
    let usdc = asset("USDC");
    let hold = ReservationId::new("hold").expect("id");
    ledger
        .deposit(tid("d1"), usdc.clone(), dec!(100))
        .expect("deposit");
    ledger
        .set_reservation(&hold, usdc.clone(), dec!(80))
        .expect("hold");
    // A real execution overdraws: total -20, reserved 80.
    ledger
        .book_execution_allowing_overdraft(&exec("e1", Side::Buy, dec!(120), dec!(1)), None)
        .expect("booked");
    let b = ledger.balance(&usdc).expect("balance");
    assert_eq!((b.total, b.reserved), (dec!(-20), dec!(80)));
    // Shrinking works.
    ledger
        .set_reservation(&hold, usdc.clone(), dec!(30))
        .expect("shrink");
    assert_eq!(ledger.reservation(&hold).map(|(_, a)| a), Some(dec!(30)));
    // Growing is not covered: nothing changes.
    let before = ledger.snapshot();
    assert!(matches!(
        ledger.set_reservation(&hold, usdc.clone(), dec!(40)),
        Err(MarginError::InsufficientBalance { .. })
    ));
    assert_eq!(ledger.snapshot(), before);
    // Zero removes it.
    ledger
        .set_reservation(&hold, usdc.clone(), dec!(0))
        .expect("remove");
    assert_eq!(ledger.reservation(&hold), None);
    assert_eq!(ledger.balance(&usdc).expect("balance").reserved, dec!(0));
    ledger.reconcile().expect("reconciles");
}

/// A balance whose `total - reserved` cannot be represented is refused when
/// it is booked, with the ledger unchanged; reading balances never does
/// arithmetic.
#[test]
fn test_reserve_unrepresentable_available_is_arithmetic_error() {
    let mut ledger = Ledger::new(LedgerConfig {
        rounding: Rounding::HalfEven,
    });
    let usd = asset("USD");
    ledger.register_asset(usd.clone(), 1).expect("register");
    ledger
        .deposit(tid("d1"), usd.clone(), Decimal::MAX)
        .expect("deposit");
    let before = ledger.snapshot();

    // MAX - 0.1 needs 29 digits: it is not a Decimal.
    let hold = ReservationId::new("r1").expect("id");
    assert!(matches!(
        ledger.reserve(hold, usd.clone(), dec!(0.1)),
        Err(MarginError::Arithmetic { .. })
    ));
    assert_eq!(ledger.snapshot(), before);
    let balance = ledger.balance(&usd).expect("balance");
    assert_eq!(
        (balance.total, balance.reserved, balance.available),
        (Decimal::MAX, dec!(0), Decimal::MAX)
    );
    assert!(ledger.overdrafts().is_empty());
    ledger.reconcile().expect("reconciles");
}
