//! Options in the ledger: premium, settlement, valuation and what refuses
//! them. Every expected value is worked out by hand in the comment beside it.
//!
//! The ledger holds USDC (6 decimals) and BTC (8 decimals). Each option is on
//! BTC in USDC, strike 100, one BTC per contract, unless a test says otherwise.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::collateral::{BalanceBasis, CollateralPolicy, StalePolicy, value_collateral};
use margin_rs::initial_margin::ProductKind;
use margin_rs::instrument::{
    ExerciseStyle, LinearPerpetual, LinearTerms, OptionContract, OptionDelivery, OptionInstrument,
    OptionMargining, OptionRight, PositionKind,
};
use margin_rs::ledger::{
    Action, DerivativeSettlement, Execution, ExpirySettlement, FundingPayment, Ledger,
    OptionOutcome, OptionSettlement, OptionSettlementBooking,
};
use margin_rs::liquidation::{
    CloseoutTarget, LiquidationAccount, LiquidationPolicy, plan_liquidation,
};
use margin_rs::margin_reservations::{MarginInstrument, MarginOrder, MarginReservationService};
use margin_rs::risk::{OrderRequest, RiskDecision, RiskGate};
use margin_rs::rules::TieredLinearIm;
use margin_rs::scenarios::{HypotheticalFill, Scenario, stress};
use margin_rs::valuation::{Mark, MarkSet, PriceSource, ValuationPolicy, value};
use margin_rs::{
    AccountId, Asset, ContractSize, Decimal, ExecutionId, Instrument, InstrumentId, MarginError,
    OrderId, ReservationId, SpotInstrument, TransferId, VenueId,
};
use rust_decimal_macros::dec;

const EXPIRY: u64 = 1_000;
const CALL: &str = "CALL";
const PUT: &str = "PUT";
const PHYSICAL_CALL: &str = "PHYSICAL-CALL";
const PHYSICAL_PUT: &str = "PHYSICAL-PUT";
const AMERICAN_CALL: &str = "AMERICAN-CALL";
const AMERICAN_CASH_CALL: &str = "AMERICAN-CASH-CALL";
const SPOT: &str = "BTC-USDC";

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}
fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}
fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).unwrap()
}

fn option_with(
    id: &str,
    right: OptionRight,
    exercise: ExerciseStyle,
    delivery: OptionDelivery,
    strike: Decimal,
    multiplier: Decimal,
) -> OptionInstrument {
    OptionInstrument::new(
        LinearTerms::new(iid(id), asset("USDC"), multiplier, 4, 2).unwrap(),
        OptionContract {
            underlying: asset("BTC"),
            strike,
            expiry_ms: EXPIRY,
            right,
            exercise,
            delivery,
            margining: OptionMargining::Linear,
        },
    )
    .unwrap()
}

fn option(
    id: &str,
    right: OptionRight,
    exercise: ExerciseStyle,
    delivery: OptionDelivery,
) -> OptionInstrument {
    option_with(id, right, exercise, delivery, dec!(100), dec!(1))
}

/// An inverse cash call on BTC settled in BTC, strike 100000.
fn inverse_call() -> OptionInstrument {
    OptionInstrument::new(
        LinearTerms::new(iid("INVERSE-CALL"), asset("BTC"), dec!(1), 4, 2).unwrap(),
        OptionContract {
            underlying: asset("BTC"),
            strike: dec!(100000),
            expiry_ms: EXPIRY,
            right: OptionRight::Call,
            exercise: ExerciseStyle::European,
            delivery: OptionDelivery::Cash,
            margining: OptionMargining::Inverse,
        },
    )
    .unwrap()
}

/// 1000 USDC and every option of this file.
fn ledger_with(usdc: Decimal) -> Ledger {
    use ExerciseStyle::{American, European};
    use OptionDelivery::{Cash, Physical};
    use OptionRight::{Call, Put};
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).unwrap();
    ledger.register_asset(asset("BTC"), 8).unwrap();
    for option in [
        option(CALL, Call, European, Cash),
        option(PUT, Put, European, Cash),
        option(PHYSICAL_CALL, Call, European, Physical),
        option(PHYSICAL_PUT, Put, European, Physical),
        option(AMERICAN_CALL, Call, American, Physical),
        option(AMERICAN_CASH_CALL, Call, American, Cash),
        inverse_call(),
    ] {
        ledger.register_instrument(option).unwrap();
    }
    ledger
        .register_instrument(
            SpotInstrument::new(iid(SPOT), asset("BTC"), asset("USDC"), 2, 8).unwrap(),
        )
        .unwrap();
    if !usdc.is_zero() {
        ledger
            .deposit(TransferId::new("d1").unwrap(), asset("USDC"), usdc)
            .unwrap();
    }
    ledger
}

fn ledger() -> Ledger {
    ledger_with(dec!(1000))
}

fn usdc(ledger: &Ledger) -> Decimal {
    ledger.balance(&asset("USDC")).map_or(dec!(0), |b| b.total)
}

fn btc(ledger: &Ledger) -> Decimal {
    ledger.balance(&asset("BTC")).map_or(dec!(0), |b| b.total)
}

fn fill(ledger: &mut Ledger, id: &str, instrument: &str, side: Action, price: Decimal, n: Decimal) {
    let execution = Execution::new(eid(id), iid(instrument), side, price, n).with_executed_at(1);
    ledger
        .book_execution_allowing_overdraft(&execution, None)
        .unwrap();
}

fn settlement(id: &str, instrument: &str, n: Decimal, outcome: OptionOutcome) -> OptionSettlement {
    OptionSettlement {
        id: eid(id),
        instrument: iid(instrument),
        contracts: n,
        outcome,
        as_of_ms: EXPIRY,
    }
}

fn cash_at(price: Decimal) -> OptionOutcome {
    OptionOutcome::Cash { price }
}

fn delivery(id: &str) -> OptionOutcome {
    OptionOutcome::Delivery {
        spot: iid(SPOT),
        delivery_id: eid(id),
    }
}

fn settle_cash(
    ledger: &mut Ledger,
    instrument: &str,
    n: Decimal,
    price: Decimal,
) -> OptionSettlementBooking {
    ledger
        .settle_option(&settlement("s1", instrument, n, cash_at(price)))
        .unwrap()
}

/// Everything a failed call must leave alone.
fn state(ledger: &Ledger) -> impl PartialEq + std::fmt::Debug + use<> {
    (
        ledger.snapshot(),
        ledger.option_settlement_journal().to_vec(),
    )
}

fn expect_unchanged(
    ledger: &mut Ledger,
    request: &OptionSettlement,
    check: impl FnOnce(&MarginError) -> bool,
) {
    let before = state(ledger);
    let error = ledger.settle_option(request).unwrap_err();
    assert!(check(&error), "unexpected error: {error:?}");
    assert!(state(ledger) == before, "the ledger changed");
    ledger.reconcile().unwrap();
}

// ------------------------------------------------------------------ fills

/// Buy 3 at 10: 30 paid, basis 30.
#[test]
fn test_fill_long_pays_premium_in_cash() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(3));
    assert_eq!(usdc(&ledger), dec!(970));
    let position = ledger.position(&iid(CALL)).unwrap();
    assert_eq!(position.quantity, dec!(3));
    assert_eq!(position.cost_basis, dec!(30));
    assert_eq!(position.realized_pnl, dec!(0));
    assert_eq!(position.kind, PositionKind::Option);
    let cashflow = &ledger.journal()[0];
    assert_eq!(cashflow.deltas[&asset("USDC")], dec!(-30));
    assert_eq!(cashflow.notional, dec!(30));
    ledger.reconcile().unwrap();
}

/// Sell 3 at 10: 30 received, basis -30.
#[test]
fn test_fill_short_receives_premium_in_cash() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(3));
    assert_eq!(usdc(&ledger), dec!(1030));
    let position = ledger.position(&iid(CALL)).unwrap();
    assert_eq!(position.quantity, dec!(-3));
    assert_eq!(position.cost_basis, dec!(-30));
    ledger.reconcile().unwrap();
}

/// 0.3333 * 0.01 * 0.1 = 0.0003333, half-even to 6 digits: 0.000333.
#[test]
fn test_fill_premium_rounds_half_even_to_settlement_scale() {
    let mut ledger = ledger();
    ledger
        .register_instrument(option_with(
            "TENTH",
            OptionRight::Call,
            ExerciseStyle::European,
            OptionDelivery::Cash,
            dec!(100),
            dec!(0.1),
        ))
        .unwrap();
    fill(
        &mut ledger,
        "e1",
        "TENTH",
        Action::Buy,
        dec!(0.3333),
        dec!(0.01),
    );
    assert_eq!(usdc(&ledger), dec!(999.999667));
    assert_eq!(
        ledger.position(&iid("TENTH")).unwrap().cost_basis,
        dec!(0.000333)
    );
    ledger.reconcile().unwrap();
}

/// Buy 2 at 10 with 0.5 of fee: 1000 - 20 - 0.5.
#[test]
fn test_fill_fee_is_charged_on_top_of_premium() {
    let mut ledger = ledger();
    let execution = Execution::new(eid("e1"), iid(CALL), Action::Buy, dec!(10), dec!(2))
        .with_executed_at(1)
        .with_fee(asset("USDC"), dec!(0.5));
    ledger
        .book_execution_allowing_overdraft(&execution, None)
        .unwrap();
    assert_eq!(usdc(&ledger), dec!(979.5));
    assert_eq!(ledger.fees_paid(&asset("USDC")), dec!(0.5));
    ledger.reconcile().unwrap();
}

/// Buy 3 at 10 (-30), sell 1 at 16 (+16): released 10, realized 6, basis 20.
#[test]
fn test_fill_partial_close_of_long_realizes_against_basis() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(3));
    fill(&mut ledger, "e2", CALL, Action::Sell, dec!(16), dec!(1));
    assert_eq!(usdc(&ledger), dec!(986));
    let position = ledger.position(&iid(CALL)).unwrap();
    assert_eq!(position.quantity, dec!(2));
    assert_eq!(position.cost_basis, dec!(20));
    assert_eq!(position.realized_pnl, dec!(6));
    ledger.reconcile().unwrap();
}

/// Sell 4 at 10 (+40), buy 1 at 7 (-7): released -10, realized 3, basis -30.
#[test]
fn test_fill_partial_close_of_short_realizes_against_basis() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(4));
    fill(&mut ledger, "e2", CALL, Action::Buy, dec!(7), dec!(1));
    assert_eq!(usdc(&ledger), dec!(1033));
    let position = ledger.position(&iid(CALL)).unwrap();
    assert_eq!(position.quantity, dec!(-3));
    assert_eq!(position.cost_basis, dec!(-30));
    assert_eq!(position.realized_pnl, dec!(3));
    ledger.reconcile().unwrap();
}

/// Buy 2 at 10 (-20), sell 5 at 12 (+60): closes 2 for 24 against a basis of
/// 20, realized 4, and opens short 3 with basis -36.
#[test]
fn test_fill_crossing_long_to_short_moves_whole_premium() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(2));
    fill(&mut ledger, "e2", CALL, Action::Sell, dec!(12), dec!(5));
    assert_eq!(usdc(&ledger), dec!(1040));
    let position = ledger.position(&iid(CALL)).unwrap();
    assert_eq!(position.quantity, dec!(-3));
    assert_eq!(position.cost_basis, dec!(-36));
    assert_eq!(position.realized_pnl, dec!(4));
    ledger.reconcile().unwrap();
}

/// A premium beyond the balance is booked and reported: 10 - 30 = -20.
#[test]
fn test_fill_beyond_balance_is_booked_with_shortfall() {
    let mut ledger = ledger_with(dec!(10));
    let execution =
        Execution::new(eid("e1"), iid(CALL), Action::Buy, dec!(10), dec!(3)).with_executed_at(1);
    let booked = ledger
        .book_execution_allowing_overdraft(&execution, None)
        .unwrap();
    assert_eq!(booked.shortfalls[&asset("USDC")], dec!(20));
    assert_eq!(usdc(&ledger), dec!(-20));
    ledger.reconcile().unwrap();
}

/// Short 2 sold at 10, bought back worthless at zero: nothing paid, the whole
/// premium realized.
#[test]
fn test_fill_at_zero_premium_is_valid() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    fill(&mut ledger, "e2", CALL, Action::Buy, dec!(0), dec!(2));
    assert_eq!(usdc(&ledger), dec!(1020));
    let position = ledger.position(&iid(CALL)).unwrap();
    assert_eq!(position.quantity, dec!(0));
    assert_eq!(position.realized_pnl, dec!(20));
    ledger.reconcile().unwrap();
}

#[test]
fn test_fill_at_negative_premium_is_refused() {
    let mut ledger = ledger();
    let execution =
        Execution::new(eid("e1"), iid(CALL), Action::Buy, dec!(-1), dec!(1)).with_executed_at(1);
    assert!(matches!(
        ledger.book_execution_allowing_overdraft(&execution, None),
        Err(MarginError::InvalidInput { field: "price", .. })
    ));
}

#[test]
fn test_fill_without_overdraft_path_is_refused() {
    let mut ledger = ledger();
    let execution =
        Execution::new(eid("e1"), iid(CALL), Action::Buy, dec!(10), dec!(3)).with_executed_at(1);
    assert!(matches!(
        ledger.book_execution(&execution),
        Err(MarginError::InvalidInput { .. })
    ));
}

#[test]
fn test_fill_without_execution_time_is_refused() {
    let mut ledger = ledger();
    let execution = Execution::new(eid("e1"), iid(CALL), Action::Buy, dec!(10), dec!(3));
    assert!(matches!(
        ledger.book_execution_allowing_overdraft(&execution, None),
        Err(MarginError::InvalidInput { .. })
    ));
}

/// Being mandatory does not let a fill through at or after the expiry.
#[test]
fn test_fill_executed_at_expiry_is_refused() {
    let mut ledger = ledger();
    let execution = Execution::new(eid("e1"), iid(CALL), Action::Buy, dec!(10), dec!(3))
        .with_executed_at(EXPIRY);
    assert!(matches!(
        ledger.book_execution_allowing_overdraft(&execution, None),
        Err(MarginError::ExecutedAfterExpiry { .. })
    ));
}

// ----------------------------------------------------------- cash settlement

/// Long 2 calls bought at 10, settled at 130: V = 2 * 30 = 60, realized
/// 60 - 20 = 40, cash 1000 - 20 + 60.
#[test]
fn test_settle_cash_long_call_in_the_money_receives_intrinsic() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(2));
    let booked = settle_cash(&mut ledger, CALL, dec!(2), dec!(130));
    let cashflow = &booked.cashflow;
    assert!(booked.is_new);
    assert_eq!(cashflow.value, dec!(60));
    assert_eq!(cashflow.cash, dec!(60));
    assert_eq!(cashflow.realized_pnl, dec!(40));
    assert_eq!(cashflow.released_basis, dec!(20));
    assert_eq!(cashflow.contracts, dec!(2));
    assert_eq!(cashflow.residual, None);
    assert_eq!(cashflow.balance_after, dec!(1040));
    assert_eq!(usdc(&ledger), dec!(1040));
    let position = ledger.position(&iid(CALL)).unwrap();
    assert_eq!(position.quantity, dec!(0));
    assert_eq!(position.cost_basis, dec!(0));
    ledger.reconcile().unwrap();
}

/// Short 2 calls sold at 10, settled at 130: pays 60, realized -60 + 20.
#[test]
fn test_settle_cash_short_call_in_the_money_pays_intrinsic() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    let booked = settle_cash(&mut ledger, CALL, dec!(2), dec!(130));
    assert_eq!(booked.cashflow.cash, dec!(-60));
    assert_eq!(booked.cashflow.realized_pnl, dec!(-40));
    assert_eq!(booked.cashflow.contracts, dec!(-2));
    assert_eq!(usdc(&ledger), dec!(960));
    ledger.reconcile().unwrap();
}

/// Long 2 puts bought at 10, settled at 80: V = 2 * 20 = 40, realized 20.
#[test]
fn test_settle_cash_long_put_in_the_money_receives_intrinsic() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", PUT, Action::Buy, dec!(10), dec!(2));
    let booked = settle_cash(&mut ledger, PUT, dec!(2), dec!(80));
    assert_eq!(booked.cashflow.cash, dec!(40));
    assert_eq!(booked.cashflow.realized_pnl, dec!(20));
    assert_eq!(usdc(&ledger), dec!(1020));
    ledger.reconcile().unwrap();
}

/// Short 2 puts sold at 10, settled at 80: pays 40, realized -20.
#[test]
fn test_settle_cash_short_put_in_the_money_pays_intrinsic() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", PUT, Action::Sell, dec!(10), dec!(2));
    let booked = settle_cash(&mut ledger, PUT, dec!(2), dec!(80));
    assert_eq!(booked.cashflow.cash, dec!(-40));
    assert_eq!(booked.cashflow.realized_pnl, dec!(-20));
    assert_eq!(usdc(&ledger), dec!(980));
    ledger.reconcile().unwrap();
}

/// At and out of the money nothing is paid: the long loses its premium.
#[test]
fn test_settle_cash_long_without_intrinsic_loses_premium() {
    for (instrument, price) in [
        (CALL, dec!(100)),
        (CALL, dec!(90)),
        (PUT, dec!(100)),
        (PUT, dec!(110)),
    ] {
        let mut ledger = ledger();
        fill(
            &mut ledger,
            "e1",
            instrument,
            Action::Buy,
            dec!(10),
            dec!(2),
        );
        let booked = settle_cash(&mut ledger, instrument, dec!(2), price);
        assert_eq!(booked.cashflow.value, dec!(0));
        assert_eq!(booked.cashflow.realized_pnl, dec!(-20));
        assert_eq!(usdc(&ledger), dec!(980));
        ledger.reconcile().unwrap();
    }
}

/// Out of the money the short keeps its premium.
#[test]
fn test_settle_cash_short_without_intrinsic_keeps_premium() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    let booked = settle_cash(&mut ledger, CALL, dec!(2), dec!(90));
    assert_eq!(booked.cashflow.realized_pnl, dec!(20));
    assert_eq!(usdc(&ledger), dec!(1020));
    ledger.reconcile().unwrap();
}

/// Long 3 at 10, settle 1 at 130: V = 30, released 10, realized 20, 2 left
/// with basis 20.
#[test]
fn test_settle_cash_partial_releases_a_share_of_basis() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(3));
    let booked = settle_cash(&mut ledger, CALL, dec!(1), dec!(130));
    assert_eq!(booked.cashflow.realized_pnl, dec!(20));
    assert_eq!(booked.cashflow.released_basis, dec!(10));
    assert_eq!(usdc(&ledger), dec!(1000));
    let position = ledger.position(&iid(CALL)).unwrap();
    assert_eq!(position.quantity, dec!(2));
    assert_eq!(position.cost_basis, dec!(20));
    ledger.reconcile().unwrap();
}

/// A short's payoff beyond the balance is booked and reported: 20 - 60.
#[test]
fn test_settle_cash_beyond_balance_reports_shortfall() {
    let mut ledger = ledger_with(dec!(0));
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    let booked = settle_cash(&mut ledger, CALL, dec!(2), dec!(130));
    assert_eq!(booked.shortfalls[&asset("USDC")], dec!(40));
    assert_eq!(usdc(&ledger), dec!(-40));
    ledger.reconcile().unwrap();
}

/// Inverse call, K 100000, long 2 settled at 110000: 2 * 10000 / 110000 = 2/11
/// BTC, 0.18181818 at 8 digits, with the exact rest
/// 20000 - 0.18181818 * 110000 = 0.0002 over 110000. Premium 0.05: 0.1 paid.
#[test]
fn test_settle_cash_inverse_long_pays_quotient_with_its_rest() {
    let mut ledger = ledger();
    ledger
        .deposit(TransferId::new("d2").unwrap(), asset("BTC"), dec!(1))
        .unwrap();
    fill(
        &mut ledger,
        "e1",
        "INVERSE-CALL",
        Action::Buy,
        dec!(0.05),
        dec!(2),
    );
    assert_eq!(btc(&ledger), dec!(0.9));
    let booked = settle_cash(&mut ledger, "INVERSE-CALL", dec!(2), dec!(110000));
    assert_eq!(booked.cashflow.value, dec!(0.18181818));
    assert_eq!(booked.cashflow.realized_pnl, dec!(0.08181818));
    let rest = booked.cashflow.residual.unwrap();
    assert_eq!(rest.numerator, dec!(0.0002));
    assert_eq!(rest.denominator, dec!(110000));
    assert_eq!(btc(&ledger), dec!(1.08181818));
    ledger.reconcile().unwrap();
}

// ---------------------------------------------------------------- delivery

/// Long 3 bought at 10 (-30), delivered: +3 BTC, -300 USDC, realized -30, and
/// the premium stays out of the cost of the BTC.
#[test]
fn test_settle_delivery_long_call_buys_underlying_at_strike() {
    let mut ledger = ledger();
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let booked = ledger
        .settle_option(&settlement("s1", PHYSICAL_CALL, dec!(3), delivery("x1")))
        .unwrap();
    let cashflow = &booked.cashflow;
    assert_eq!(cashflow.value, dec!(0));
    assert_eq!(cashflow.cash, dec!(0));
    assert_eq!(cashflow.realized_pnl, dec!(-30));
    assert_eq!(cashflow.balance_after, dec!(670));
    let spot = cashflow.delivery.as_ref().unwrap();
    assert_eq!(spot.side, Action::Buy);
    assert_eq!(spot.price, dec!(100));
    assert_eq!(spot.quantity, dec!(3));
    assert_eq!(usdc(&ledger), dec!(670));
    assert_eq!(btc(&ledger), dec!(3));
    assert_eq!(
        ledger.position(&iid(PHYSICAL_CALL)).unwrap().quantity,
        dec!(0)
    );
    assert_eq!(ledger.position(&iid(SPOT)).unwrap().cost_basis, dec!(300));
    assert!(ledger.execution_cashflow(&eid("x1")).is_some());
    ledger.reconcile().unwrap();
}

/// Short 3 sold at 10 (+30), assigned: -3 BTC, +300 USDC, realized +30. The
/// missing BTC is an overdraft, reported.
#[test]
fn test_settle_delivery_short_call_sells_underlying_at_strike() {
    let mut ledger = ledger();
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_CALL,
        Action::Sell,
        dec!(10),
        dec!(3),
    );
    let booked = ledger
        .settle_option(&settlement("s1", PHYSICAL_CALL, dec!(3), delivery("x1")))
        .unwrap();
    assert_eq!(
        booked.cashflow.delivery.as_ref().unwrap().side,
        Action::Sell
    );
    assert_eq!(booked.cashflow.realized_pnl, dec!(30));
    assert_eq!(booked.shortfalls[&asset("BTC")], dec!(3));
    assert_eq!(usdc(&ledger), dec!(1330));
    assert_eq!(btc(&ledger), dec!(-3));
    ledger.reconcile().unwrap();
}

/// Long 3 puts bought at 10, delivered: -3 BTC, +300 USDC.
#[test]
fn test_settle_delivery_long_put_sells_underlying_at_strike() {
    let mut ledger = ledger();
    ledger
        .deposit(TransferId::new("d2").unwrap(), asset("BTC"), dec!(3))
        .unwrap();
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_PUT,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let booked = ledger
        .settle_option(&settlement("s1", PHYSICAL_PUT, dec!(3), delivery("x1")))
        .unwrap();
    assert_eq!(
        booked.cashflow.delivery.as_ref().unwrap().side,
        Action::Sell
    );
    assert!(booked.shortfalls.is_empty());
    assert_eq!(usdc(&ledger), dec!(1270));
    assert_eq!(btc(&ledger), dec!(0));
    ledger.reconcile().unwrap();
}

/// Short 3 puts sold at 10 on 100 USDC, assigned: +3 BTC, -300 USDC, so 130
/// becomes -170, all of it a shortfall.
#[test]
fn test_settle_delivery_short_put_beyond_balance_reports_shortfall() {
    let mut ledger = ledger_with(dec!(100));
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_PUT,
        Action::Sell,
        dec!(10),
        dec!(3),
    );
    let booked = ledger
        .settle_option(&settlement("s1", PHYSICAL_PUT, dec!(3), delivery("x1")))
        .unwrap();
    assert_eq!(booked.cashflow.delivery.as_ref().unwrap().side, Action::Buy);
    assert_eq!(booked.shortfalls[&asset("USDC")], dec!(170));
    assert_eq!(booked.cashflow.balance_after, dec!(-170));
    assert_eq!(usdc(&ledger), dec!(-170));
    assert_eq!(btc(&ledger), dec!(3));
    ledger.reconcile().unwrap();
}

/// Long 3, deliver 1: 1 BTC at 100, released 10, 2 left with basis 20.
#[test]
fn test_settle_delivery_partial_moves_only_those_contracts() {
    let mut ledger = ledger();
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let booked = ledger
        .settle_option(&settlement("s1", PHYSICAL_CALL, dec!(1), delivery("x1")))
        .unwrap();
    assert_eq!(booked.cashflow.realized_pnl, dec!(-10));
    assert_eq!(booked.cashflow.released_basis, dec!(10));
    assert_eq!(usdc(&ledger), dec!(870));
    assert_eq!(btc(&ledger), dec!(1));
    let position = ledger.position(&iid(PHYSICAL_CALL)).unwrap();
    assert_eq!(position.quantity, dec!(2));
    assert_eq!(position.cost_basis, dec!(20));
    ledger.reconcile().unwrap();
}

/// Delivery of 0.5 contracts on a spot of whole units: refused, not rounded,
/// and the option is untouched.
#[test]
fn test_settle_delivery_quantity_off_spot_grid_changes_nothing() {
    let mut ledger = ledger();
    ledger
        .register_instrument(
            SpotInstrument::new(iid("WHOLE"), asset("BTC"), asset("USDC"), 2, 0).unwrap(),
        )
        .unwrap();
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let request = settlement(
        "s1",
        PHYSICAL_CALL,
        dec!(0.5),
        OptionOutcome::Delivery {
            spot: iid("WHOLE"),
            delivery_id: eid("x1"),
        },
    );
    expect_unchanged(&mut ledger, &request, |error| {
        matches!(error, MarginError::PrecisionExceeded { .. })
    });
}

/// A strike of 100.5 on a spot priced in whole units: refused, not rounded.
#[test]
fn test_settle_delivery_strike_off_spot_grid_changes_nothing() {
    let mut ledger = ledger();
    ledger
        .register_instrument(
            SpotInstrument::new(iid("WHOLE-PRICE"), asset("BTC"), asset("USDC"), 0, 8).unwrap(),
        )
        .unwrap();
    ledger
        .register_instrument(option_with(
            "ODD-STRIKE",
            OptionRight::Call,
            ExerciseStyle::European,
            OptionDelivery::Physical,
            dec!(100.5),
            dec!(1),
        ))
        .unwrap();
    fill(
        &mut ledger,
        "e1",
        "ODD-STRIKE",
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let request = settlement(
        "s1",
        "ODD-STRIKE",
        dec!(3),
        OptionOutcome::Delivery {
            spot: iid("WHOLE-PRICE"),
            delivery_id: eid("x1"),
        },
    );
    expect_unchanged(&mut ledger, &request, |error| {
        matches!(error, MarginError::PrecisionExceeded { .. })
    });
}

/// Strike 100.25 and 0.01 contracts move 1.0025, finer than a 2 digit asset:
/// refused, so the cash moved is exactly `K * n * M` or nothing moves.
#[test]
fn test_settle_delivery_cash_finer_than_asset_changes_nothing() {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 2).unwrap();
    ledger.register_asset(asset("BTC"), 8).unwrap();
    ledger
        .register_instrument(
            SpotInstrument::new(iid(SPOT), asset("BTC"), asset("USDC"), 2, 8).unwrap(),
        )
        .unwrap();
    ledger
        .register_instrument(option_with(
            "ODD-STRIKE",
            OptionRight::Call,
            ExerciseStyle::European,
            OptionDelivery::Physical,
            dec!(100.25),
            dec!(1),
        ))
        .unwrap();
    fill(
        &mut ledger,
        "e1",
        "ODD-STRIKE",
        Action::Buy,
        dec!(100),
        dec!(1),
    );
    let request = settlement("s1", "ODD-STRIKE", dec!(0.01), delivery("x1"));
    expect_unchanged(&mut ledger, &request, |error| {
        matches!(error, MarginError::PrecisionExceeded { .. })
    });
}

#[test]
fn test_settle_delivery_on_other_pair_changes_nothing() {
    let mut ledger = ledger();
    ledger.register_asset(asset("ETH"), 8).unwrap();
    ledger
        .register_instrument(
            SpotInstrument::new(iid("ETH-USDC"), asset("ETH"), asset("USDC"), 2, 8).unwrap(),
        )
        .unwrap();
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let request = settlement(
        "s1",
        PHYSICAL_CALL,
        dec!(3),
        OptionOutcome::Delivery {
            spot: iid("ETH-USDC"),
            delivery_id: eid("x1"),
        },
    );
    expect_unchanged(&mut ledger, &request, |error| {
        matches!(error, MarginError::InvalidInput { field: "spot", .. })
    });
}

#[test]
fn test_settle_delivery_on_non_spot_or_unknown_changes_nothing() {
    let mut ledger = ledger();
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let on_option = settlement(
        "s1",
        PHYSICAL_CALL,
        dec!(3),
        OptionOutcome::Delivery {
            spot: iid(CALL),
            delivery_id: eid("x1"),
        },
    );
    expect_unchanged(&mut ledger, &on_option, |error| {
        matches!(error, MarginError::InvalidInput { field: "spot", .. })
    });
    let on_unknown = settlement(
        "s1",
        PHYSICAL_CALL,
        dec!(3),
        OptionOutcome::Delivery {
            spot: iid("NOPE"),
            delivery_id: eid("x1"),
        },
    );
    expect_unchanged(&mut ledger, &on_unknown, |error| {
        matches!(error, MarginError::UnknownInstrument { .. })
    });
}

/// The delivery id must be free, or a repeat of a real fill would be taken
/// for the delivery and the underlying would never move.
#[test]
fn test_settle_delivery_id_already_used_changes_nothing() {
    let mut ledger = ledger();
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let reused = settlement("s1", PHYSICAL_CALL, dec!(3), delivery("e1"));
    expect_unchanged(&mut ledger, &reused, |error| {
        matches!(error, MarginError::ConflictingEntry { .. })
    });
    let same_as_settlement = settlement("s1", PHYSICAL_CALL, dec!(3), delivery("s1"));
    expect_unchanged(&mut ledger, &same_as_settlement, |error| {
        matches!(error, MarginError::ConflictingEntry { .. })
    });
}

/// A repeat of a delivery moves nothing twice.
#[test]
fn test_settle_delivery_repeat_is_duplicate() {
    let mut ledger = ledger();
    fill(
        &mut ledger,
        "e1",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let request = settlement("s1", PHYSICAL_CALL, dec!(3), delivery("x1"));
    let first = ledger.settle_option(&request).unwrap();
    let again = ledger.settle_option(&request).unwrap();
    assert!(first.is_new);
    assert!(!again.is_new);
    assert_eq!(again.cashflow, first.cashflow);
    assert!(again.shortfalls.is_empty());
    assert_eq!(btc(&ledger), dec!(3));
    assert_eq!(usdc(&ledger), dec!(670));
    ledger.reconcile().unwrap();
}

// ------------------------------------------------------ lapse and identities

/// A lapse closes at no value: the long loses its premium, the short keeps it.
#[test]
fn test_settle_lapsed_closes_at_no_value() {
    let mut long = ledger();
    fill(
        &mut long,
        "e1",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let booked = long
        .settle_option(&settlement(
            "s1",
            PHYSICAL_CALL,
            dec!(3),
            OptionOutcome::Lapsed,
        ))
        .unwrap();
    assert_eq!(booked.cashflow.realized_pnl, dec!(-30));
    assert_eq!(booked.cashflow.delivery, None);
    assert_eq!(usdc(&long), dec!(970));
    long.reconcile().unwrap();

    let mut short = ledger();
    fill(
        &mut short,
        "e1",
        PHYSICAL_CALL,
        Action::Sell,
        dec!(10),
        dec!(3),
    );
    let booked = short
        .settle_option(&settlement(
            "s1",
            PHYSICAL_CALL,
            dec!(3),
            OptionOutcome::Lapsed,
        ))
        .unwrap();
    assert_eq!(booked.cashflow.realized_pnl, dec!(30));
    assert_eq!(usdc(&short), dec!(1030));
    short.reconcile().unwrap();
}

/// Repeats are duplicates also when nothing was paid, and different content
/// under the same id is a conflict.
#[test]
fn test_settle_repeat_with_no_value_is_duplicate_and_change_conflicts() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(3));
    let request = settlement("s1", CALL, dec!(3), cash_at(dec!(90)));
    let first = ledger.settle_option(&request).unwrap();
    let again = ledger.settle_option(&request).unwrap();
    assert!(!again.is_new);
    assert_eq!(again.cashflow, first.cashflow);
    let changed = settlement("s1", CALL, dec!(3), cash_at(dec!(91)));
    expect_unchanged(&mut ledger, &changed, |error| {
        matches!(error, MarginError::ConflictingEntry { .. })
    });

    let mut lapsing = self::ledger();
    fill(
        &mut lapsing,
        "e1",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let request = settlement("s1", PHYSICAL_CALL, dec!(3), OptionOutcome::Lapsed);
    lapsing.settle_option(&request).unwrap();
    assert!(!lapsing.settle_option(&request).unwrap().is_new);
    lapsing.reconcile().unwrap();
}

/// One namespace: a settlement cannot reuse an execution's id, nor the other
/// way round, nor any other posting's.
#[test]
fn test_settle_shares_identifier_namespace() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(3));
    let reused = settlement("e1", CALL, dec!(1), cash_at(dec!(130)));
    expect_unchanged(&mut ledger, &reused, |error| {
        matches!(error, MarginError::ConflictingEntry { .. })
    });
    ledger
        .settle_option(&settlement("s1", CALL, dec!(1), cash_at(dec!(130))))
        .unwrap();
    let execution =
        Execution::new(eid("s1"), iid(CALL), Action::Buy, dec!(10), dec!(1)).with_executed_at(1);
    assert!(matches!(
        ledger.book_execution_allowing_overdraft(&execution, None),
        Err(MarginError::ConflictingEntry { .. })
    ));
    let posting = DerivativeSettlement {
        id: eid("s1"),
        instrument: iid("OUTSIDE"),
        asset: asset("USDC"),
        realized_pnl: dec!(1),
        fee: dec!(0),
    };
    assert!(matches!(
        ledger.book_settlement(&posting),
        Err(MarginError::ConflictingEntry { .. })
    ));
}

// -------------------------------------------------------------- refusals

#[test]
fn test_settle_outcome_of_other_delivery_kind_is_refused() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(3));
    fill(
        &mut ledger,
        "e2",
        PHYSICAL_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    // A cash option cannot drop its payoff by lapsing.
    let lapsed = settlement("s1", CALL, dec!(3), OptionOutcome::Lapsed);
    let delivered = settlement("s1", CALL, dec!(3), delivery("x1"));
    let priced = settlement("s1", PHYSICAL_CALL, dec!(3), cash_at(dec!(130)));
    for request in [lapsed, delivered, priced] {
        expect_unchanged(&mut ledger, &request, |error| {
            matches!(
                error,
                MarginError::InvalidInput {
                    field: "outcome",
                    ..
                }
            )
        });
    }
}

#[test]
fn test_settle_contracts_out_of_range_are_refused() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(3));
    for contracts in [dec!(0), dec!(-1), dec!(3.01)] {
        let request = settlement("s1", CALL, contracts, cash_at(dec!(130)));
        expect_unchanged(&mut ledger, &request, |error| {
            matches!(
                error,
                MarginError::InvalidInput {
                    field: "contracts",
                    ..
                }
            )
        });
    }
    let too_fine = settlement("s1", CALL, dec!(0.001), cash_at(dec!(130)));
    expect_unchanged(&mut ledger, &too_fine, |error| {
        matches!(error, MarginError::PrecisionExceeded { .. })
    });
    let flat = settlement("s1", PUT, dec!(1), cash_at(dec!(80)));
    expect_unchanged(&mut ledger, &flat, |error| {
        matches!(
            error,
            MarginError::InvalidInput {
                field: "contracts",
                ..
            }
        )
    });
}

/// No division by a zero price, and no payoff from a negative one.
#[test]
fn test_settle_price_not_positive_is_refused() {
    let mut ledger = ledger();
    ledger
        .deposit(TransferId::new("d2").unwrap(), asset("BTC"), dec!(1))
        .unwrap();
    fill(
        &mut ledger,
        "e1",
        "INVERSE-CALL",
        Action::Buy,
        dec!(0.05),
        dec!(2),
    );
    fill(&mut ledger, "e2", PUT, Action::Buy, dec!(10), dec!(2));
    for (instrument, price) in [
        ("INVERSE-CALL", dec!(0)),
        ("INVERSE-CALL", dec!(-1)),
        (PUT, dec!(0)),
    ] {
        let request = settlement("s1", instrument, dec!(1), cash_at(price));
        expect_unchanged(&mut ledger, &request, |error| {
            matches!(error, MarginError::InvalidInput { field: "price", .. })
        });
    }
}

#[test]
fn test_settle_before_expiry_only_american_exercises() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(3));
    fill(
        &mut ledger,
        "e2",
        AMERICAN_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    fill(
        &mut ledger,
        "e3",
        AMERICAN_CASH_CALL,
        Action::Buy,
        dec!(10),
        dec!(3),
    );
    let early = |id: &str, instrument: &str, outcome: OptionOutcome| OptionSettlement {
        as_of_ms: EXPIRY - 1,
        ..settlement(id, instrument, dec!(1), outcome)
    };

    let european = early("s1", CALL, cash_at(dec!(130)));
    expect_unchanged(&mut ledger, &european, |error| {
        matches!(
            error,
            MarginError::InvalidInput {
                field: "as_of_ms",
                ..
            }
        )
    });
    let lapse = early("s1", AMERICAN_CALL, OptionOutcome::Lapsed);
    expect_unchanged(&mut ledger, &lapse, |error| {
        matches!(
            error,
            MarginError::InvalidInput {
                field: "as_of_ms",
                ..
            }
        )
    });

    ledger
        .settle_option(&early("s2", AMERICAN_CALL, delivery("x2")))
        .unwrap();
    ledger
        .settle_option(&early("s3", AMERICAN_CASH_CALL, cash_at(dec!(130))))
        .unwrap();
    assert_eq!(btc(&ledger), dec!(1));
    ledger.reconcile().unwrap();
}

/// The other postings stay with the instruments they are for, both ways.
#[test]
fn test_postings_of_other_instruments_are_refused() {
    let mut ledger = ledger();
    ledger
        .register_instrument(LinearPerpetual::new(
            LinearTerms::new(iid("PERP"), asset("USDC"), dec!(1), 2, 0).unwrap(),
        ))
        .unwrap();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(3));
    let expiry = ExpirySettlement {
        id: eid("x"),
        instrument: iid(CALL),
        price: dec!(130),
        as_of_ms: EXPIRY,
    };
    assert!(matches!(
        ledger.settle_expiry(&expiry),
        Err(MarginError::InvalidInput {
            field: "instrument",
            ..
        })
    ));
    let funding = FundingPayment {
        id: eid("f"),
        instrument: iid(CALL),
        amount: dec!(1),
        as_of_ms: 1,
    };
    assert!(matches!(
        ledger.book_funding(&funding),
        Err(MarginError::InvalidInput {
            field: "instrument",
            ..
        })
    ));
    let held_here = DerivativeSettlement {
        id: eid("o"),
        instrument: iid(CALL),
        asset: asset("USDC"),
        realized_pnl: dec!(1),
        fee: dec!(0),
    };
    assert!(matches!(
        ledger.book_settlement(&held_here),
        Err(MarginError::InvalidInput {
            field: "instrument",
            ..
        })
    ));
    let on_perpetual = settlement("s1", "PERP", dec!(1), cash_at(dec!(130)));
    expect_unchanged(&mut ledger, &on_perpetual, |error| {
        matches!(
            error,
            MarginError::InvalidInput {
                field: "instrument",
                ..
            }
        )
    });
    let unknown = settlement("s1", "NOPE", dec!(1), cash_at(dec!(130)));
    expect_unchanged(&mut ledger, &unknown, |error| {
        matches!(error, MarginError::UnknownInstrument { .. })
    });
}

/// A fill executed before the expiry and delivered after the settlement
/// reopens the position, which settles again under a new id; a settlement of
/// what is no longer open is refused.
#[test]
fn test_settle_late_fill_reopens_and_settles_again() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(2));
    settle_cash(&mut ledger, CALL, dec!(2), dec!(130));
    let beyond = settlement("s2", CALL, dec!(1), cash_at(dec!(130)));
    expect_unchanged(&mut ledger, &beyond, |error| {
        matches!(
            error,
            MarginError::InvalidInput {
                field: "contracts",
                ..
            }
        )
    });
    let late = Execution::new(eid("e2"), iid(CALL), Action::Buy, dec!(11), dec!(1))
        .with_executed_at(EXPIRY - 1);
    ledger
        .book_execution_allowing_overdraft(&late, None)
        .unwrap();
    let booked = ledger.settle_option(&beyond).unwrap();
    assert_eq!(booked.cashflow.realized_pnl, dec!(19));
    assert!(!ledger.is_settled(&iid(CALL)));
    // 1000 - 20 + 60 - 11 + 30
    assert_eq!(usdc(&ledger), dec!(1059));
    ledger.reconcile().unwrap();
}

// ------------------------------------------------------------- valuation

fn policy() -> ValuationPolicy {
    ValuationPolicy::new(asset("USDC"), 1000)
}

fn marks_of(instrument: &str, price: Decimal, at_ms: u64) -> MarkSet {
    MarkSet::new(10_000).with_instrument(iid(instrument), Mark::new(price, at_ms, PriceSource::Mid))
}

/// Long 2 at 10 on 1000, marked 13: cash 980, value 26, equity 1006, PnL 6.
/// No collateral credit for the long.
#[test]
fn test_value_long_adds_market_value_to_equity_only() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(2));
    let valuation = value(
        &ledger.snapshot(),
        &marks_of(CALL, dec!(13), 9_500),
        &policy(),
    )
    .unwrap();
    let line = &valuation.derivatives[&iid(CALL)];
    assert_eq!(line.market_value, dec!(26));
    assert_eq!(line.unrealized_pnl, dec!(6));
    assert!(line.is_option);
    assert_eq!(valuation.equity, Some(dec!(1006)));
    assert_eq!(valuation.net_pnl, Some(dec!(6)));
    assert_eq!(valuation.collateral_equity, Some(dec!(980)));
    let attribution = valuation.attribution.unwrap();
    assert_eq!(attribution.realized, dec!(0));
    assert_eq!(attribution.unrealized, dec!(6));
    assert_eq!(attribution.rounding_difference, dec!(0));
}

/// Short 2 at 10 on 1000, marked 13: cash 1020, value -26, equity 994, PnL -6.
/// Collateral deducts the mark: 994.
#[test]
fn test_value_short_deducts_market_value_from_equity_and_collateral() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    let valuation = value(
        &ledger.snapshot(),
        &marks_of(CALL, dec!(13), 9_500),
        &policy(),
    )
    .unwrap();
    assert_eq!(valuation.derivatives[&iid(CALL)].unrealized_pnl, dec!(-6));
    assert_eq!(valuation.equity, Some(dec!(994)));
    assert_eq!(valuation.net_pnl, Some(dec!(-6)));
    assert_eq!(valuation.collateral_equity, Some(dec!(994)));
    assert_eq!(valuation.attribution.unwrap().unrealized, dec!(-6));
}

/// Realized, unrealized and fees add up to the PnL across a partial close and
/// a fee: buy 3 at 10 with 1 of fee (-31), sell 1 at 16 (+16), marked 12: cash
/// 985, value 24, equity 1009; realized 6, unrealized 24 - 20 = 4, fees 1.
#[test]
fn test_value_attribution_adds_up_with_partial_close_and_fee() {
    let mut ledger = ledger();
    let execution = Execution::new(eid("e1"), iid(CALL), Action::Buy, dec!(10), dec!(3))
        .with_executed_at(1)
        .with_fee(asset("USDC"), dec!(1));
    ledger
        .book_execution_allowing_overdraft(&execution, None)
        .unwrap();
    fill(&mut ledger, "e2", CALL, Action::Sell, dec!(16), dec!(1));
    let valuation = value(
        &ledger.snapshot(),
        &marks_of(CALL, dec!(12), 9_500),
        &policy(),
    )
    .unwrap();
    assert_eq!(valuation.equity, Some(dec!(1009)));
    assert_eq!(valuation.net_pnl, Some(dec!(9)));
    let attribution = valuation.attribution.unwrap();
    assert_eq!(attribution.realized, dec!(6));
    assert_eq!(attribution.unrealized, dec!(4));
    assert_eq!(attribution.fees, dec!(1));
    assert_eq!(attribution.rounding_difference, dec!(0));
}

/// After settling, the PnL is all realized and equity is the cash.
#[test]
fn test_value_after_settlement_is_cash_and_realized() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    settle_cash(&mut ledger, CALL, dec!(2), dec!(130));
    let valuation = value(&ledger.snapshot(), &MarkSet::new(10_000), &policy()).unwrap();
    assert_eq!(valuation.equity, Some(dec!(960)));
    assert_eq!(valuation.net_pnl, Some(dec!(-40)));
    assert_eq!(valuation.attribution.unwrap().realized, dec!(-40));
}

/// A worthless option has a mark of zero: valid, and the long is worth nothing.
#[test]
fn test_value_zero_mark_is_valid_for_an_option() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(2));
    let valuation = value(
        &ledger.snapshot(),
        &marks_of(CALL, dec!(0), 9_500),
        &policy(),
    )
    .unwrap();
    assert_eq!(valuation.equity, Some(dec!(980)));
    assert_eq!(valuation.derivatives[&iid(CALL)].unrealized_pnl, dec!(-20));
    assert!(valuation.instrument_flags.is_empty());
    let simple = ledger
        .valuation(&BTreeMap::from([(iid(CALL), dec!(0))]))
        .unwrap();
    assert_eq!(simple.positions[&iid(CALL)].unrealized_pnl, dec!(-20));
}

#[test]
fn test_value_negative_mark_is_refused() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(2));
    assert!(matches!(
        value(
            &ledger.snapshot(),
            &marks_of(CALL, dec!(-1), 9_500),
            &policy()
        ),
        Err(MarginError::InvalidInput { .. })
    ));
    assert!(matches!(
        ledger.valuation(&BTreeMap::from([(iid(CALL), dec!(-1))])),
        Err(MarginError::InvalidInput { .. })
    ));
}

/// A zero mark stays invalid for a future.
#[test]
fn test_value_zero_mark_stays_refused_for_a_perpetual() {
    let mut ledger = ledger();
    ledger
        .register_instrument(LinearPerpetual::new(
            LinearTerms::new(iid("PERP"), asset("USDC"), dec!(1), 2, 0).unwrap(),
        ))
        .unwrap();
    fill(&mut ledger, "e1", "PERP", Action::Buy, dec!(10), dec!(2));
    assert!(matches!(
        value(
            &ledger.snapshot(),
            &marks_of("PERP", dec!(0), 9_500),
            &policy()
        ),
        Err(MarginError::InvalidInput { .. })
    ));
}

/// An open option is never valued at zero for want of a mark.
#[test]
fn test_value_missing_mark_is_an_error() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    assert!(matches!(
        value(&ledger.snapshot(), &MarkSet::new(10_000), &policy()),
        Err(MarginError::MissingMark { .. })
    ));
}

/// What a short costs to close cannot be known at a stale mark, even one of
/// zero: collateral is unknown. Reporting equity still uses it, flagged.
#[test]
fn test_value_short_at_stale_mark_leaves_collateral_unknown() {
    for mark in [dec!(13), dec!(0)] {
        let mut ledger = ledger();
        fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
        let valuation = value(&ledger.snapshot(), &marks_of(CALL, mark, 1), &policy()).unwrap();
        assert_eq!(valuation.collateral_equity, None, "mark {mark}");
        assert!(valuation.equity.is_some());
        assert!(valuation.instrument_flags.contains_key(&iid(CALL)));
    }
}

/// A long counts nothing, so a stale mark on it leaves collateral known.
#[test]
fn test_value_long_at_stale_mark_keeps_collateral_known() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Buy, dec!(10), dec!(2));
    let valuation = value(&ledger.snapshot(), &marks_of(CALL, dec!(13), 1), &policy()).unwrap();
    assert_eq!(valuation.collateral_equity, Some(dec!(980)));
}

fn collateral_policy(stale: StalePolicy) -> CollateralPolicy {
    CollateralPolicy {
        stale,
        basis: BalanceBasis::Total,
        ..CollateralPolicy::new(asset("USDC"), 1000)
    }
}

/// Collateral with its own policy agrees: 1020 - 26 for a short, and a stale
/// mark on one is refused under `Reject`, even at a mark of zero.
#[test]
fn test_collateral_short_deducts_mark_and_stale_is_refused() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    let report = value_collateral(
        &ledger.snapshot(),
        &marks_of(CALL, dec!(13), 9_500),
        &collateral_policy(StalePolicy::Reject),
    )
    .unwrap();
    assert_eq!(report.total, Some(dec!(994)));
    for mark in [dec!(13), dec!(0)] {
        assert!(matches!(
            value_collateral(
                &ledger.snapshot(),
                &marks_of(CALL, mark, 1),
                &collateral_policy(StalePolicy::Reject),
            ),
            Err(MarginError::InvalidInstrumentMark { .. })
        ));
    }
}

// ----------------------------------------------- what does not cover options

fn is_unsupported(result: &MarginError) -> bool {
    matches!(result, MarginError::UnsupportedInstrument { .. })
}

/// Stress does not leave an option out: it refuses it, however it is named.
#[test]
fn test_stress_open_option_is_refused() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    let error = stress(
        &ledger.snapshot(),
        &marks_of(CALL, dec!(13), 9_500),
        &policy(),
        &Scenario::new("empty"),
    )
    .unwrap_err();
    assert!(is_unsupported(&error), "{error:?}");
}

#[test]
fn test_stress_shock_or_fill_on_option_is_refused() {
    let ledger = ledger();
    let mut shocked = Scenario::new("shock");
    shocked.instrument_shocks.insert(iid(CALL), dec!(0.5));
    let error = stress(
        &ledger.snapshot(),
        &MarkSet::new(10_000),
        &policy(),
        &shocked,
    )
    .unwrap_err();
    assert!(is_unsupported(&error), "{error:?}");
    let mut filled = Scenario::new("fill");
    filled.fills.push(HypotheticalFill {
        instrument: iid(CALL),
        side: Action::Buy,
        price: dec!(10),
        quantity: dec!(1),
        fee: None,
    });
    let error = stress(
        &ledger.snapshot(),
        &MarkSet::new(10_000),
        &policy(),
        &filled,
    )
    .unwrap_err();
    assert!(is_unsupported(&error), "{error:?}");
}

/// A flat option does not stop a stress of the rest.
#[test]
fn test_stress_flat_option_is_ignored() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    settle_cash(&mut ledger, CALL, dec!(2), dec!(90));
    stress(
        &ledger.snapshot(),
        &MarkSet::new(10_000),
        &policy(),
        &Scenario::new("empty"),
    )
    .unwrap();
}

fn account() -> AccountId {
    AccountId::new("acct").unwrap()
}
fn venue() -> VenueId {
    VenueId::new("venue").unwrap()
}

/// No closeout plan is better than one that leaves the short option out:
/// a rule that does not margin options refuses the plan.
#[test]
fn test_liquidation_open_option_needs_a_rule_for_options() {
    let mut ledger = ledger();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(10), dec!(2));
    let rule = TieredLinearIm::linear_v1(asset("USDC"), 6).unwrap();
    let account = LiquidationAccount::from_ledger(
        &rule,
        &ledger.snapshot(),
        account(),
        venue(),
        &asset("USDC"),
        &BTreeMap::from([(iid(CALL), dec!(13))]),
        1,
    )
    .unwrap();
    let error = plan_liquidation(
        &rule,
        &account,
        &BTreeMap::new(),
        &LiquidationPolicy {
            target: CloseoutTarget::Flat,
            fee_rate: Decimal::ZERO,
            fee_rates: BTreeMap::new(),
            loss_budget: None,
            contract_step: Decimal::ONE,
        },
    )
    .unwrap_err();
    assert!(is_unsupported(&error), "{error:?}");
}

/// American and inverse options have no closeout plan.
#[test]
fn test_liquidation_unsupported_option_styles_are_refused() {
    for (id, mark) in [(AMERICAN_CASH_CALL, dec!(13)), ("INVERSE-CALL", dec!(0.01))] {
        let mut ledger = ledger();
        if id == "INVERSE-CALL" {
            ledger
                .deposit(TransferId::new("btc").unwrap(), asset("BTC"), dec!(1))
                .unwrap();
        }
        fill(&mut ledger, "e1", id, Action::Buy, mark, dec!(1));
        let error = LiquidationAccount::from_ledger(
            &TieredLinearIm::linear_v1(asset("USDC"), 6).unwrap(),
            &ledger.snapshot(),
            account(),
            venue(),
            &asset("USDC"),
            &BTreeMap::from([(iid(id), mark)]),
            1,
        )
        .unwrap_err();
        assert!(is_unsupported(&error), "{id}: {error:?}");
    }
}

fn perp_gate(fill_option: Option<&str>) -> RiskGate {
    let mut ledger = ledger();
    ledger
        .register_instrument(LinearPerpetual::new(
            LinearTerms::new(iid("PERP"), asset("USDC"), dec!(1), 2, 0).unwrap(),
        ))
        .unwrap();
    if let Some(instrument) = fill_option {
        fill(
            &mut ledger,
            "e1",
            instrument,
            Action::Sell,
            dec!(0.5),
            dec!(1),
        );
    }
    RiskGate::new(ledger)
}

fn perp_service() -> MarginReservationService<TieredLinearIm> {
    let mut service = MarginReservationService::on_ledger(
        TieredLinearIm::linear_v1(asset("USDC"), 6).unwrap(),
        account(),
        venue(),
        ReservationId::new("hold").unwrap(),
    );
    service
        .register(
            iid("PERP"),
            MarginInstrument {
                product: ProductKind::LinearPerpetual,
                settlement: asset("USDC"),
                size: ContractSize::Multiplier(dec!(1)),
            },
        )
        .unwrap();
    service
}

fn perp_order() -> MarginOrder {
    MarginOrder {
        id: OrderId::new("o1").unwrap(),
        instrument: iid("PERP"),
        side: Action::Buy,
        contracts: dec!(1),
    }
}

/// An open option in the service's currency blocks it, rather than leaving the
/// option out of the requirement and under reserving.
#[test]
fn test_reservation_open_option_in_currency_blocks_service() {
    let mut gate = perp_gate(Some(CALL));
    let mut service = perp_service();
    let marks = BTreeMap::from([(iid("PERP"), dec!(10)), (iid(CALL), dec!(1))]);
    let hold = ReservationId::new("hold").unwrap();
    let error = service
        .reserve_with_gate(perp_order(), &mut gate, &hold, &marks)
        .unwrap_err();
    assert!(is_unsupported(&error), "{error:?}");
    let error = service.requirement_with_gate(&gate, &marks).unwrap_err();
    assert!(is_unsupported(&error), "{error:?}");
    // Nothing was held, and the order id stays free.
    assert_eq!(gate.ledger().reservation(&hold), None);
}

/// The block is per currency: an option settled elsewhere is not this rule's.
#[test]
fn test_reservation_open_option_in_other_currency_does_not_block() {
    let mut ledger = ledger();
    ledger
        .register_instrument(LinearPerpetual::new(
            LinearTerms::new(iid("PERP"), asset("USDC"), dec!(1), 2, 0).unwrap(),
        ))
        .unwrap();
    ledger
        .deposit(TransferId::new("d2").unwrap(), asset("BTC"), dec!(1))
        .unwrap();
    fill(
        &mut ledger,
        "e1",
        "INVERSE-CALL",
        Action::Sell,
        dec!(0.05),
        dec!(1),
    );
    let mut gate = RiskGate::new(ledger);
    let mut service = perp_service();
    let marks = BTreeMap::from([(iid("PERP"), dec!(10))]);
    let hold = ReservationId::new("hold").unwrap();
    service
        .reserve_with_gate(perp_order(), &mut gate, &hold, &marks)
        .unwrap();
    // 10 % of 1 * 1 * 10.
    assert_eq!(gate.ledger().reservation(&hold).map(|r| r.1), Some(dec!(1)));
}

/// A flat option in the currency blocks nothing.
#[test]
fn test_reservation_flat_option_does_not_block() {
    let mut ledger = ledger();
    ledger
        .register_instrument(LinearPerpetual::new(
            LinearTerms::new(iid("PERP"), asset("USDC"), dec!(1), 2, 0).unwrap(),
        ))
        .unwrap();
    fill(&mut ledger, "e1", CALL, Action::Sell, dec!(0.5), dec!(1));
    settle_cash(&mut ledger, CALL, dec!(1), dec!(90));
    let mut gate = RiskGate::new(ledger);
    let mut service = perp_service();
    let marks = BTreeMap::from([(iid("PERP"), dec!(10))]);
    let hold = ReservationId::new("hold").unwrap();
    service
        .reserve_with_gate(perp_order(), &mut gate, &hold, &marks)
        .unwrap();
}

#[test]
fn test_risk_gate_order_on_option_is_rejected() {
    let mut gate = perp_gate(None);
    let order = OrderRequest::new(
        OrderId::new("o1").unwrap(),
        iid(CALL),
        Action::Buy,
        dec!(10),
        dec!(1),
    );
    assert!(matches!(
        gate.submit(&order, 0),
        RiskDecision::Rejected { .. }
    ));
}

/// Through the gate, a fill and a delivery are mandatory postings like any
/// other derivative's.
#[test]
fn test_risk_gate_books_fill_and_settlement_of_option() {
    let mut gate = perp_gate(None);
    let execution = Execution::new(
        eid("e1"),
        iid(PHYSICAL_CALL),
        Action::Buy,
        dec!(10),
        dec!(3),
    )
    .with_executed_at(1);
    gate.book_derivative_execution(&execution).unwrap();
    let request = settlement("s1", PHYSICAL_CALL, dec!(3), delivery("x1"));
    gate.settle_option(&request).unwrap();
    assert_eq!(usdc(gate.ledger()), dec!(670));
    assert_eq!(btc(gate.ledger()), dec!(3));
    gate.ledger().reconcile().unwrap();
}

// ------------------------------------------------------------- the instrument

fn terms(id: &str, settlement: &str) -> LinearTerms {
    LinearTerms::new(iid(id), asset(settlement), dec!(1), 4, 2).unwrap()
}

fn contract() -> OptionContract {
    OptionContract {
        underlying: asset("BTC"),
        strike: dec!(100),
        expiry_ms: EXPIRY,
        right: OptionRight::Call,
        exercise: ExerciseStyle::European,
        delivery: OptionDelivery::Cash,
        margining: OptionMargining::Linear,
    }
}

#[test]
fn test_option_new_invalid_terms_are_refused() {
    let invalid = |contract: OptionContract, settlement: &str, field: &str| {
        let error = OptionInstrument::new(terms("O", settlement), contract).unwrap_err();
        assert!(
            matches!(&error, MarginError::InvalidInput { field: f, .. } if *f == field),
            "{error:?}"
        );
    };
    for strike in [dec!(0), dec!(-1)] {
        invalid(
            OptionContract {
                strike,
                ..contract()
            },
            "USDC",
            "strike",
        );
    }
    // A linear option settles away from its underlying, an inverse one in it.
    invalid(contract(), "BTC", "settlement");
    invalid(
        OptionContract {
            margining: OptionMargining::Inverse,
            ..contract()
        },
        "USDC",
        "settlement",
    );
    invalid(
        OptionContract {
            margining: OptionMargining::Inverse,
            delivery: OptionDelivery::Physical,
            ..contract()
        },
        "BTC",
        "delivery",
    );
}

#[test]
fn test_option_accessors_describe_the_instrument() {
    let instrument: Instrument = OptionInstrument::new(terms("O", "USDC"), contract())
        .unwrap()
        .into();
    assert_eq!(instrument.id(), &iid("O"));
    assert_eq!(instrument.quote(), &asset("USDC"));
    assert_eq!(instrument.kind(), PositionKind::Option);
    assert_eq!(
        instrument.contract_size(),
        ContractSize::Multiplier(dec!(1))
    );
    assert_eq!(instrument.expiry_ms(), Some(EXPIRY));
    assert_eq!(instrument.linear_terms(), None);
    assert_eq!(instrument.inverse_terms(), None);
    let option = instrument.option_terms().unwrap();
    assert_eq!(option.contract().strike, dec!(100));
    assert_eq!(option.terms().settlement(), &asset("USDC"));
}

#[test]
fn test_register_option_with_unknown_settlement_asset_is_refused() {
    let mut ledger = Ledger::default();
    let option = OptionInstrument::new(terms("O", "USDC"), contract()).unwrap();
    assert!(matches!(
        ledger.register_instrument(option),
        Err(MarginError::UnknownAsset { .. })
    ));
}
