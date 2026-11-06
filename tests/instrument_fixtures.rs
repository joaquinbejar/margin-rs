//! The shared `financial_types` instrument fixtures run through
//! `Instrument::from_spec` and the ledger (feature `instruments`, #45).
//!
//! Every case has an explicit disposition, keyed by its name and operation;
//! an unknown or missing case fails the test, so a new upstream case must be
//! classified here. The shared checker (`fixtures::check`) is a canary of
//! `financial_types` itself; the independent regression is the set of hand
//! literals in `tests/instrument_spec.rs`.
//!
//! The ledger runs with BTC and USDT at 8 decimals and instrument scales of
//! 8 (18 where a case needs more digits); fills are executed at 1 and 2 ms,
//! before every expiry.

#![cfg(feature = "instruments")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::Q;
use financial_types::Side;
use financial_types::instrument::fixtures::{self, Case, Expected, Operation};
use financial_types::instrument::{ContractKind, Margining, MoneyRatio};
use margin_rs::instrument::InstrumentScales;
use margin_rs::ledger::{
    Action, Execution, ExecutionCashflow, ExpirySettlement, Ledger, OptionOutcome,
    OptionSettlement, OptionSettlementCashflow,
};
use margin_rs::{
    Asset, Decimal, ExecutionId, Instrument, InstrumentId, MarginError, SpotInstrument,
};
use rust_decimal_macros::dec;

/// What the ledger does with a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Disposition {
    /// A linear notional or PnL: booked, and equal to the exact amount.
    LinearLedger,
    /// An inverse notional or PnL: booked, and the rounded values plus
    /// their recorded rests equal the exact amount.
    InverseLedger,
    /// An option: its premium or expiry booked and equal to the exact
    /// amount, or a grid check the ledger books on or off the grid.
    OptionLedger,
    /// A grid check of the specification: the ledger checks digits only and
    /// books the value, on or off the grid.
    GridOnly,
    /// Rounding of a given ratio: a `financial_types` operation with no
    /// ledger counterpart (the ledger's own rounding has its fixtures in
    /// `tests/ledger_fixtures.rs` and `tests/inverse_derivatives.rs`).
    RatioRoundingOnly,
    /// A product `financial_types` refuses as inexact: the ledger rounds a
    /// notional to the settlement asset scale by policy and books it.
    LedgerRounds,
    /// An invalid input the ledger refuses too.
    LedgerRefuses,
}

use Disposition::*;

/// Every case, by `name | operation`, and its disposition.
const TABLE: &[(&str, Disposition)] = &[
    (
        "linear notional: 0.5 BTC x 60000.1 USDT = 30000.05 USDT | Notional { contracts: 0.5, price: 60000.1 }",
        LinearLedger,
    ),
    (
        "linear pnl long: 0.5 x (61000.5 - 60000) = +500.25 USDT | RealizedPnl { side: Side::Long, contracts: 0.5, entry: 60000, exit: 61000.5 }",
        LinearLedger,
    ),
    (
        "linear pnl short: the same move is -500.25 USDT | RealizedPnl { side: Side::Short, contracts: 0.5, entry: 60000, exit: 61000.5 }",
        LinearLedger,
    ),
    (
        "inverse notional: 1000 x 10 USD / 60000 = 1/6 BTC, 0.16666667 at satoshi | Notional { contracts: 1000, price: 60000 }",
        InverseLedger,
    ),
    (
        "inverse pnl long: 10000 x (1/60000 - 1/61000) = 1/366 BTC, 0.00273224 | RealizedPnl { side: Side::Long, contracts: 1000, entry: 60000, exit: 61000 }",
        InverseLedger,
    ),
    (
        "inverse pnl short: -1/366 BTC, -0.00273224 | RealizedPnl { side: Side::Short, contracts: 1000, entry: 60000, exit: 61000 }",
        InverseLedger,
    ),
    (
        "inverse future pnl long: 1000 USD from 60000 to 59000 = -1/3540 BTC, -0.00028249 | RealizedPnl { side: Side::Long, contracts: 100, entry: 60000, exit: 59000 }",
        InverseLedger,
    ),
    (
        "option premium long: 1.5 x 1 BTC x 0.0125 = -0.01875 BTC paid | Premium { side: Side::Long, contracts: 1.5, premium: 0.0125 }",
        OptionLedger,
    ),
    (
        "option premium short: +0.01875 BTC received | Premium { side: Side::Short, contracts: 1.5, premium: 0.0125 }",
        OptionLedger,
    ),
    (
        "inverse call long, S 110000, K 100000: 2 x 10000 / 110000 = 2/11 BTC | Expiry { side: Side::Long, contracts: 2, settlement_price: 110000 }",
        OptionLedger,
    ),
    (
        "inverse call short: -2/11 BTC | Expiry { side: Side::Short, contracts: 2, settlement_price: 110000 }",
        OptionLedger,
    ),
    (
        "inverse call at the money: 0 | Expiry { side: Side::Long, contracts: 2, settlement_price: 100000 }",
        OptionLedger,
    ),
    (
        "inverse call out of the money: 0 | Expiry { side: Side::Long, contracts: 2, settlement_price: 90000 }",
        OptionLedger,
    ),
    (
        "inverse put long, S 90000: 2 x 10000 / 90000 = 2/9 BTC | Expiry { side: Side::Long, contracts: 2, settlement_price: 90000 }",
        OptionLedger,
    ),
    (
        "inverse put out of the money: 0 | Expiry { side: Side::Short, contracts: 2, settlement_price: 110000 }",
        OptionLedger,
    ),
    (
        "illustrative linear cash call long, S 120, K 100: 3 x 20 = +60 USDT | Expiry { side: Side::Long, contracts: 3, settlement_price: 120 }",
        OptionLedger,
    ),
    (
        "illustrative physical call long, S 120: +3 BTC, -300 USDT | Expiry { side: Side::Long, contracts: 3, settlement_price: 120 }",
        OptionLedger,
    ),
    (
        "illustrative physical call short, S 120: -3 BTC, +300 USDT | Expiry { side: Side::Short, contracts: 3, settlement_price: 120 }",
        OptionLedger,
    ),
    (
        "illustrative physical put long, S 80: -3 BTC, +300 USDT | Expiry { side: Side::Long, contracts: 3, settlement_price: 80 }",
        OptionLedger,
    ),
    (
        "illustrative physical put short, S 80: +3 BTC, -300 USDT | Expiry { side: Side::Short, contracts: 3, settlement_price: 80 }",
        OptionLedger,
    ),
    (
        "illustrative physical call out of the money: abandoned | Expiry { side: Side::Long, contracts: 3, settlement_price: 90 }",
        OptionLedger,
    ),
    (
        "illustrative physical call at the money: abandoned | Expiry { side: Side::Short, contracts: 3, settlement_price: 100 }",
        OptionLedger,
    ),
    (
        "illustrative physical put out of the money: abandoned | Expiry { side: Side::Long, contracts: 3, settlement_price: 120 }",
        OptionLedger,
    ),
    (
        "Deribit option premium 0.005: on the 0.0001 band (inclusive) | CheckPrice { price: 0.005 }",
        OptionLedger,
    ),
    (
        "Deribit option premium 0.0049: on 0.0001 | CheckPrice { price: 0.0049 }",
        OptionLedger,
    ),
    (
        "Deribit option premium 0.00495: off 0.0001 | CheckPrice { price: 0.00495 }",
        OptionLedger,
    ),
    (
        "Deribit option premium 0.0051: above 0.005, off 0.0005 | CheckPrice { price: 0.0051 }",
        OptionLedger,
    ),
    (
        "Deribit option premium 0.0055: on 0.0005 | CheckPrice { price: 0.0055 }",
        OptionLedger,
    ),
    (
        "Deribit future price 60002.5: on 2.5 | CheckPrice { price: 60002.5 }",
        GridOnly,
    ),
    (
        "Deribit future price 60001: off 2.5 | CheckPrice { price: 60001 }",
        GridOnly,
    ),
    (
        "Binance price 60000.15: off 0.1 | CheckPrice { price: 60000.15 }",
        GridOnly,
    ),
    (
        "Binance size 0.0015 BTC: off 0.001 | CheckLot { contracts: 0.0015 }",
        GridOnly,
    ),
    (
        "Deribit option size 0.15: off 0.1 | CheckLot { contracts: 0.15 }",
        OptionLedger,
    ),
    (
        "Deribit option size 0.3: on 0.1 | CheckLot { contracts: 0.3 }",
        OptionLedger,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: Floor }",
        RatioRoundingOnly,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: Floor }",
        RatioRoundingOnly,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: Ceil }",
        RatioRoundingOnly,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: Ceil }",
        RatioRoundingOnly,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: TowardZero }",
        RatioRoundingOnly,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: TowardZero }",
        RatioRoundingOnly,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: AwayFromZero }",
        RatioRoundingOnly,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: AwayFromZero }",
        RatioRoundingOnly,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: HalfEven }",
        RatioRoundingOnly,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: HalfEven }",
        RatioRoundingOnly,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: HalfAwayFromZero }",
        RatioRoundingOnly,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: HalfAwayFromZero }",
        RatioRoundingOnly,
    ),
    (
        "tie 3/8 half even: the odd floor goes up, 0.38 | Settle { value: Ratio { num: 3, den: 8 }, scale: 2, rule: HalfEven }",
        RatioRoundingOnly,
    ),
    (
        "2/3 at scale 28 | Settle { value: Ratio { num: 2, den: 3 }, scale: 28, rule: HalfEven }",
        RatioRoundingOnly,
    ),
    (
        "scale 29 is refused | Settle { value: Ratio { num: 1, den: 3 }, scale: 29, rule: HalfEven }",
        RatioRoundingOnly,
    ),
    (
        "Decimal::MAX contracts x price 2: overflow | Notional { contracts: 79228162514264337593543950335, price: 2 }",
        LedgerRefuses,
    ),
    (
        "1.000000000000001 x 1.00000000000001: 31 digits, inexact | Notional { contracts: 1.000000000000001, price: 1.00000000000001 }",
        LedgerRounds,
    ),
    (
        "1e-28 x 1e-28: below scale 28, inexact | Notional { contracts: 0.0000000000000000000000000001, price: 0.0000000000000000000000000001 }",
        LedgerRefuses,
    ),
    (
        "negative premium is refused | Premium { side: Side::Long, contracts: 1, premium: -0.0001 }",
        OptionLedger,
    ),
    (
        "zero contracts are refused | Notional { contracts: 0, price: 60000 }",
        LedgerRefuses,
    ),
    (
        "expiry of a perpetual is unsupported | Expiry { side: Side::Long, contracts: 1, settlement_price: 60000 }",
        LedgerRefuses,
    ),
];

fn key(case: &Case) -> String {
    format!("{} | {:?}", case.name, case.operation)
}

fn iid_of(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}

fn scales(price_scale: u32, contract_scale: u32) -> InstrumentScales {
    InstrumentScales {
        price_scale,
        contract_scale,
    }
}

/// A ledger with BTC and USDT at 8 decimals and the case's instrument at
/// `scales`.
fn ledger(case: &Case, scales: InstrumentScales) -> (Ledger, InstrumentId) {
    let mut ledger = Ledger::default();
    for asset in ["BTC", "USDT"] {
        ledger
            .register_asset(Asset::new(asset).unwrap(), 8)
            .unwrap();
    }
    let instrument = Instrument::from_spec(&case.spec, scales).unwrap();
    let id = instrument.id().clone();
    ledger.register_instrument(instrument).unwrap();
    (ledger, id)
}

fn book(
    ledger: &mut Ledger,
    id: &InstrumentId,
    name: &str,
    side: Action,
    price: Decimal,
    quantity: Decimal,
    at_ms: u64,
) -> Result<ExecutionCashflow, MarginError> {
    let execution = Execution::new(
        ExecutionId::new(name).unwrap(),
        id.clone(),
        side,
        price,
        quantity,
    )
    .with_executed_at(at_ms);
    ledger
        .book_execution_allowing_overdraft(&execution, None)
        .map(|booked| booked.booking.cashflow().clone())
}

fn exact(expected: &Expected) -> &MoneyRatio {
    match expected {
        Expected::Amount { exact, .. } => exact,
        other => panic!("expected an amount, got {other:?}"),
    }
}

fn q(ratio: &MoneyRatio) -> Q {
    Q::ratio(ratio.ratio.num(), ratio.ratio.den())
}

fn sides(side: Side) -> (Action, Action) {
    match side {
        Side::Long => (Action::Buy, Action::Sell),
        Side::Short => (Action::Sell, Action::Buy),
    }
}

fn run_linear(case: &Case) {
    let (mut ledger, id) = ledger(case, scales(8, 8));
    let expected = exact(&case.expected);
    assert_eq!(expected.asset.as_str(), "USDT", "{}", case.name);
    match &case.operation {
        Operation::Notional { contracts, price } => {
            let fill = book(&mut ledger, &id, "open", Action::Buy, *price, *contracts, 1).unwrap();
            let notional = fill.derivative.unwrap().virtual_notional.unwrap();
            assert_eq!(Q::dec(notional), q(expected), "{}", case.name);
        }
        Operation::RealizedPnl {
            side,
            contracts,
            entry,
            exit,
        } => {
            let (open, close) = sides(*side);
            book(&mut ledger, &id, "open", open, *entry, *contracts, 1).unwrap();
            let fill = book(&mut ledger, &id, "close", close, *exit, *contracts, 2).unwrap();
            assert_eq!(Q::dec(fill.realized_pnl), q(expected), "{}", case.name);
        }
        other => panic!("{}: unexpected operation {other:?}", case.name),
    }
}

fn rest(fill: &ExecutionCashflow) -> Q {
    let residual = fill.derivative.as_ref().unwrap().residual.as_ref().unwrap();
    Q::ratio(residual.numerator, residual.denominator)
}

fn run_inverse(case: &Case) {
    let (mut ledger, id) = ledger(case, scales(8, 8));
    let expected = exact(&case.expected);
    assert_eq!(expected.asset.as_str(), "BTC", "{}", case.name);
    match &case.operation {
        Operation::Notional { contracts, price } => {
            let fill = book(&mut ledger, &id, "open", Action::Buy, *price, *contracts, 1).unwrap();
            let value = fill.derivative.as_ref().unwrap().virtual_notional.unwrap();
            // n·CV/p = V + rest, exactly; V is rounded once, half even at
            // the BTC scale, as the fixture settles it.
            assert_eq!(Q::dec(value).add(rest(&fill)), q(expected), "{}", case.name);
            if let Expected::Amount {
                settled: Some(settled),
                ..
            } = &case.expected
            {
                assert_eq!(value, settled.amount, "{}", case.name);
            }
        }
        Operation::RealizedPnl {
            side,
            contracts,
            entry,
            exit,
        } => {
            let (open, close) = sides(*side);
            let opened = book(&mut ledger, &id, "open", open, *entry, *contracts, 1).unwrap();
            let closed = book(&mut ledger, &id, "close", close, *exit, *contracts, 2).unwrap();
            // realized + s·(r_open - r_close) = exact, s = +1 long, -1
            // short. The rounded realized PnL follows the per-leg rule
            // (hand literals in tests/instrument_spec.rs).
            let rests = rest(&opened).sub(rest(&closed));
            let rests = match side {
                Side::Long => rests,
                Side::Short => rests.neg(),
            };
            assert_eq!(
                Q::dec(closed.realized_pnl).add(rests),
                q(expected),
                "{}",
                case.name
            );
        }
        other => panic!("{}: unexpected operation {other:?}", case.name),
    }
}

fn settle(
    ledger: &mut Ledger,
    id: &InstrumentId,
    contracts: Decimal,
    outcome: OptionOutcome,
    as_of_ms: u64,
) -> OptionSettlementCashflow {
    ledger
        .settle_option(&OptionSettlement {
            id: ExecutionId::new("settle").unwrap(),
            instrument: id.clone(),
            contracts,
            outcome,
            as_of_ms,
        })
        .unwrap()
        .cashflow
}

fn run_option_expiry(case: &Case, side: Side, contracts: Decimal, settlement_price: Decimal) {
    let (mut ledger, id) = ledger(case, scales(8, 8));
    let expiry_ms = case.spec.expiry_ms().unwrap();
    let (open, _) = sides(side);
    book(&mut ledger, &id, "open", open, dec!(1), contracts, 1).unwrap();
    match &case.expected {
        Expected::Amount { exact, settled } => {
            let cashflow = settle(
                &mut ledger,
                &id,
                contracts,
                OptionOutcome::Cash {
                    price: settlement_price,
                },
                expiry_ms,
            );
            // The booked cash plus its signed rest is the exact amount.
            let rest = cashflow.residual.map_or(Q::new(0, 1), |rest| {
                Q::dec(rest.numerator).div(Q::dec(rest.denominator))
            });
            let rest = match side {
                Side::Long => rest,
                Side::Short => rest.neg(),
            };
            assert_eq!(Q::dec(cashflow.cash).add(rest), q(exact), "{}", case.name);
            assert_eq!(
                cashflow.asset.as_str(),
                exact.asset.as_str(),
                "{}",
                case.name
            );
            if let Some(settled) = settled {
                assert_eq!(cashflow.cash, settled.amount, "{}", case.name);
            }
        }
        Expected::Delivery { underlying, cash } => {
            let spot = iid_of("BTC-USDT");
            ledger
                .register_instrument(
                    SpotInstrument::new(
                        spot.clone(),
                        Asset::new("BTC").unwrap(),
                        Asset::new("USDT").unwrap(),
                        8,
                        8,
                    )
                    .unwrap(),
                )
                .unwrap();
            let cashflow = settle(
                &mut ledger,
                &id,
                contracts,
                OptionOutcome::Delivery {
                    spot,
                    delivery_id: ExecutionId::new("delivery").unwrap(),
                },
                expiry_ms,
            );
            let deltas = &cashflow.delivery.as_ref().unwrap().deltas;
            assert_eq!(
                deltas[&Asset::new(underlying.asset.as_str()).unwrap()],
                underlying.amount,
                "{}",
                case.name
            );
            assert_eq!(
                deltas[&Asset::new(cash.asset.as_str()).unwrap()],
                cash.amount,
                "{}",
                case.name
            );
        }
        Expected::Abandoned => {
            let cashflow = settle(
                &mut ledger,
                &id,
                contracts,
                OptionOutcome::Lapsed,
                expiry_ms,
            );
            assert_eq!(cashflow.cash, Decimal::ZERO, "{}", case.name);
        }
        other => panic!("{}: unexpected result {other:?}", case.name),
    }
    assert!(ledger.reconcile().is_ok(), "{}", case.name);
}

fn run_option(case: &Case) {
    assert_eq!(case.spec.kind(), ContractKind::Option, "{}", case.name);
    match &case.operation {
        Operation::Premium {
            side,
            contracts,
            premium,
        } => {
            let (mut ledger, id) = ledger(case, scales(8, 8));
            let (open, _) = sides(*side);
            let booked = book(&mut ledger, &id, "open", open, *premium, *contracts, 1);
            if let Expected::Error(_) = &case.expected {
                assert!(booked.is_err(), "{}: {booked:?}", case.name);
                return;
            }
            let fill = booked.unwrap();
            let Expected::Money(money) = &case.expected else {
                panic!("{}: unexpected result {:?}", case.name, case.expected);
            };
            assert_eq!(
                fill.deltas[&Asset::new(money.asset.as_str()).unwrap()],
                money.amount,
                "{}",
                case.name
            );
        }
        Operation::Expiry {
            side,
            contracts,
            settlement_price,
        } => run_option_expiry(case, *side, *contracts, *settlement_price),
        Operation::CheckPrice { .. } | Operation::CheckLot { .. } => run_grid(case),
        other => panic!("{}: unexpected operation {other:?}", case.name),
    }
}

fn run_grid(case: &Case) {
    let (mut ledger, id) = ledger(case, scales(8, 8));
    let fill = match &case.operation {
        Operation::CheckPrice { price } => book(
            &mut ledger,
            &id,
            "grid",
            Action::Buy,
            *price,
            case.spec.lot(),
            1,
        ),
        Operation::CheckLot { contracts } => book(
            &mut ledger,
            &id,
            "grid",
            Action::Buy,
            dec!(60000),
            *contracts,
            1,
        ),
        other => panic!("{}: unexpected operation {other:?}", case.name),
    };
    assert!(fill.is_ok(), "{}: {fill:?}", case.name);
}

fn run_rounds(case: &Case) {
    // 1.000000000000001 x 1.00000000000001 = 1.0000000000000110...01
    // exactly (31 digits); the ledger rounds the notional once to the USDT
    // scale, half even: 1.00000000.
    let (mut ledger, id) = ledger(case, scales(18, 18));
    let Operation::Notional { contracts, price } = &case.operation else {
        panic!("{}: expected a notional", case.name);
    };
    let fill = book(&mut ledger, &id, "open", Action::Buy, *price, *contracts, 1).unwrap();
    assert_eq!(
        fill.derivative.unwrap().virtual_notional,
        Some(dec!(1.00000000)),
        "{}",
        case.name
    );
}

fn run_refuses(case: &Case) {
    let result = match (case.name, &case.operation) {
        (name, Operation::Notional { contracts, price }) if name.starts_with("Decimal::MAX") => {
            // 2 x MAX does not fit the basis.
            let (mut ledger, id) = ledger(case, scales(8, 8));
            let result = book(&mut ledger, &id, "open", Action::Buy, *price, *contracts, 1);
            assert!(
                matches!(result, Err(MarginError::Arithmetic { .. })),
                "{name}: {result:?}"
            );
            return;
        }
        (name, Operation::Notional { contracts, price }) if name.starts_with("1e-28") => {
            // 28 digits exceed the largest instrument scale, 18.
            let (mut ledger, id) = ledger(case, scales(18, 18));
            book(&mut ledger, &id, "open", Action::Buy, *price, *contracts, 1)
                .map(|_| ())
                .map_err(|error| (error, "PrecisionExceeded"))
        }
        (name, Operation::Notional { contracts, price }) if name.starts_with("zero contracts") => {
            let (mut ledger, id) = ledger(case, scales(8, 8));
            book(&mut ledger, &id, "open", Action::Buy, *price, *contracts, 1)
                .map(|_| ())
                .map_err(|error| (error, "InvalidInput"))
        }
        (
            name,
            Operation::Expiry {
                settlement_price, ..
            },
        ) if name.starts_with("expiry of a perpetual") => {
            let (mut ledger, id) = ledger(case, scales(8, 8));
            ledger
                .settle_expiry(&ExpirySettlement {
                    id: ExecutionId::new("settle").unwrap(),
                    instrument: id,
                    price: *settlement_price,
                    as_of_ms: 1,
                })
                .map(|_| ())
                .map_err(|error| (error, "InvalidInput"))
        }
        (name, operation) => panic!("{name}: no refusal defined for {operation:?}"),
    };
    match result {
        Err(
            (MarginError::PrecisionExceeded { .. }, "PrecisionExceeded")
            | (MarginError::InvalidInput { .. }, "InvalidInput"),
        ) => {}
        other => panic!("{}: {other:?}", case.name),
    }
}

#[test]
fn test_shared_fixtures_every_case_has_a_checked_disposition() {
    let table: BTreeMap<&str, Disposition> = TABLE.iter().copied().collect();
    assert_eq!(table.len(), TABLE.len(), "duplicate key in the table");
    let cases = fixtures::cases().unwrap();
    let mut seen = BTreeSet::new();
    let mut counts: BTreeMap<Disposition, usize> = BTreeMap::new();
    for case in &cases {
        // Canary: the case still passes against financial_types.
        fixtures::check(case).unwrap();
        let key = key(case);
        let disposition = *table
            .get(key.as_str())
            .unwrap_or_else(|| panic!("unclassified fixture: {key}"));
        assert!(seen.insert(key.clone()), "duplicate case: {key}");
        *counts.entry(disposition).or_default() += 1;
        match disposition {
            LinearLedger => {
                assert_eq!(case.spec.margining(), Margining::Linear, "{key}");
                run_linear(case);
            }
            InverseLedger => {
                assert_eq!(case.spec.margining(), Margining::Inverse, "{key}");
                run_inverse(case);
            }
            OptionLedger => run_option(case),
            GridOnly => run_grid(case),
            RatioRoundingOnly => {
                assert!(matches!(case.operation, Operation::Settle { .. }), "{key}");
            }
            LedgerRounds => run_rounds(case),
            LedgerRefuses => run_refuses(case),
        }
    }
    for (key, _) in TABLE {
        assert!(seen.contains(*key), "fixture no longer present: {key}");
    }
    let expected: BTreeMap<Disposition, usize> = [
        (LinearLedger, 3),
        (InverseLedger, 4),
        (OptionLedger, 24),
        (GridOnly, 4),
        (RatioRoundingOnly, 15),
        (LedgerRounds, 1),
        (LedgerRefuses, 4),
    ]
    .into_iter()
    .collect();
    assert_eq!(counts, expected);
}
