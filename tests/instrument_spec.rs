//! `Instrument::from_spec`: conversion of a `financial_types` instrument
//! specification into a ledger instrument (feature `instruments`, #45).
//!
//! Expected values are literals read from the specifications or computed by
//! hand with exact fractions (shown in the comments), never with the code
//! under test. BTC and USDT are registered at 8 decimals.

#![cfg(feature = "instruments")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::Q;
use financial_types::OptionStyle;
use financial_types::instrument::reference::{binance_usdm, deribit};
use financial_types::instrument::{
    AssetCode, ContractKind, ContractSize, InstrumentDraft, InstrumentError,
    InstrumentId as SpecId, InstrumentSpec, Margining, TickBand, TickSchedule,
};
use margin_rs::instrument::InstrumentScales;
use margin_rs::ledger::{
    Action, Execution, ExecutionCashflow, ExpirySettlement, InverseResidual, Ledger,
};
use margin_rs::{Asset, Decimal, ExecutionId, Instrument, InstrumentId, MarginError, PositionKind};
use rust_decimal_macros::dec;

/// 2026-11-27 08:00 UTC, in milliseconds.
const EXPIRY_MS: u64 = 1_795_766_400_000;

fn scales(price_scale: u32, contract_scale: u32) -> InstrumentScales {
    InstrumentScales {
        price_scale,
        contract_scale,
    }
}

fn code(value: &str) -> AssetCode {
    AssetCode::new(value).unwrap()
}

fn iid(value: &str) -> InstrumentId {
    InstrumentId::new(value).unwrap()
}

fn ticks(tick: Decimal) -> TickSchedule {
    TickSchedule::fixed(tick).unwrap()
}

/// A specification of BTC against `price` (USDT for linear, USD for
/// inverse), with the given size amount, ticks, lot and expiry.
fn spec(
    id: &str,
    kind: ContractKind,
    margining: Margining,
    size: Decimal,
    price_ticks: TickSchedule,
    lot: Decimal,
    expiry_ms: Option<u64>,
) -> InstrumentSpec {
    let (price, settlement, size) = match (kind, margining) {
        (ContractKind::Spot, _) | (_, Margining::Linear) => {
            ("USDT", "USDT", ContractSize::base(size, code("BTC")))
        }
        _ => ("USD", "BTC", ContractSize::quote_value(size, code("USD"))),
    };
    InstrumentSpec::new(InstrumentDraft {
        id: SpecId::new(id).unwrap(),
        kind,
        margining,
        underlying: code("BTC"),
        price_asset: code(price),
        settlement_asset: code(settlement),
        size: size.unwrap(),
        price_ticks,
        lot,
        expiry_ms,
        option: None,
    })
    .unwrap()
}

fn linear_perpetual(price_ticks: TickSchedule, lot: Decimal) -> InstrumentSpec {
    spec(
        "TEST-LINEAR",
        ContractKind::Perpetual,
        Margining::Linear,
        dec!(1),
        price_ticks,
        lot,
        None,
    )
}

fn inverse_perpetual(cv: Decimal, tick: Decimal, lot: Decimal) -> InstrumentSpec {
    spec(
        "TEST-INVERSE",
        ContractKind::Perpetual,
        Margining::Inverse,
        cv,
        ticks(tick),
        lot,
        None,
    )
}

fn ledger(assets: &[(&str, u32)]) -> Ledger {
    let mut ledger = Ledger::default();
    for (asset, scale) in assets {
        ledger
            .register_asset(Asset::new(asset).unwrap(), *scale)
            .unwrap();
    }
    ledger
}

fn book(
    ledger: &mut Ledger,
    id: &str,
    instrument: &str,
    side: Action,
    price: Decimal,
    quantity: Decimal,
    at_ms: u64,
) -> Result<ExecutionCashflow, MarginError> {
    let execution = Execution::new(
        ExecutionId::new(id).unwrap(),
        iid(instrument),
        side,
        price,
        quantity,
    )
    .with_executed_at(at_ms);
    ledger
        .book_execution_allowing_overdraft(&execution, None)
        .map(|booked| booked.booking.cashflow().clone())
}

fn below_grid(
    instrument: &str,
    field: &'static str,
    grid: &'static str,
    given: u32,
    required: u32,
) -> MarginError {
    MarginError::ScaleBelowGrid {
        instrument: iid(instrument),
        field,
        grid,
        given,
        required,
    }
}

// ------------------------------------------------------------- mapping

/// Binance `BTCUSDT`: linear perpetual, multiplier 1, settled in USDT, tick
/// 0.1 (1 digit), step 0.001 (3 digits).
#[test]
fn test_from_spec_binance_btcusdt_maps_to_linear_perpetual() {
    let spec = binance_usdm::btcusdt_perpetual().unwrap();
    let instrument = Instrument::from_spec(&spec, scales(1, 3)).unwrap();
    let Instrument::LinearPerpetual(perpetual) = &instrument else {
        panic!("expected a linear perpetual, got {instrument:?}");
    };
    let terms = perpetual.terms();
    assert_eq!(terms.id(), &iid("BTCUSDT"));
    assert_eq!(terms.settlement(), &Asset::new("USDT").unwrap());
    assert_eq!(terms.multiplier(), dec!(1));
    assert_eq!((terms.price_scale(), terms.contract_scale()), (1, 3));
    assert_eq!(instrument.expiry_ms(), None);
}

/// Deribit `BTC-PERPETUAL`: inverse perpetual, CV 10 USD, settled in BTC.
#[test]
fn test_from_spec_deribit_perpetual_maps_to_inverse_perpetual() {
    let spec = deribit::btc_perpetual().unwrap();
    let instrument = Instrument::from_spec(&spec, scales(2, 0)).unwrap();
    let Instrument::InversePerpetual(perpetual) = &instrument else {
        panic!("expected an inverse perpetual, got {instrument:?}");
    };
    let terms = perpetual.terms();
    assert_eq!(terms.id(), &iid("BTC-PERPETUAL"));
    assert_eq!(terms.settlement(), &Asset::new("BTC").unwrap());
    assert_eq!(terms.contract_value(), dec!(10));
    assert_eq!((terms.price_scale(), terms.contract_scale()), (2, 0));
}

/// Deribit future: inverse future, CV 10 USD, tick 2.5, expiry unchanged.
#[test]
fn test_from_spec_deribit_future_maps_to_inverse_future_with_expiry() {
    let spec = deribit::btc_future(SpecId::new("BTC-27NOV26").unwrap(), EXPIRY_MS).unwrap();
    let instrument = Instrument::from_spec(&spec, scales(1, 0)).unwrap();
    let Instrument::InverseFuture(future) = &instrument else {
        panic!("expected an inverse future, got {instrument:?}");
    };
    assert_eq!(future.terms().contract_value(), dec!(10));
    assert_eq!(future.expiry_ms(), EXPIRY_MS);
    assert_eq!(instrument.kind(), PositionKind::InverseFuture);
}

/// Spot: base the underlying, quote the price asset.
#[test]
fn test_from_spec_spot_maps_base_and_quote() {
    let spec = spec(
        "BTC/USDT",
        ContractKind::Spot,
        Margining::Linear,
        dec!(1),
        ticks(dec!(0.01)),
        dec!(0.00001),
        None,
    );
    let instrument = Instrument::from_spec(&spec, scales(2, 5)).unwrap();
    let Instrument::Spot(spot) = &instrument else {
        panic!("expected spot, got {instrument:?}");
    };
    assert_eq!(spot.base(), &Asset::new("BTC").unwrap());
    assert_eq!(spot.quote(), &Asset::new("USDT").unwrap());
    assert_eq!((spot.price_scale(), spot.quantity_scale()), (2, 5));
}

/// Linear future: fractional multiplier 0.001, tick 2.5, lot 0.25, and the
/// largest expiry passed unchanged.
#[test]
fn test_from_spec_linear_future_fractional_terms_map_exactly() {
    let spec = spec(
        "TEST-LF",
        ContractKind::Future,
        Margining::Linear,
        dec!(0.001),
        ticks(dec!(2.5)),
        dec!(0.25),
        Some(u64::MAX),
    );
    let instrument = Instrument::from_spec(&spec, scales(1, 2)).unwrap();
    let Instrument::LinearFuture(future) = &instrument else {
        panic!("expected a linear future, got {instrument:?}");
    };
    assert_eq!(future.terms().multiplier(), dec!(0.001));
    assert_eq!(future.expiry_ms(), u64::MAX);
    assert_eq!(
        Instrument::from_spec(&spec, scales(1, 1)).unwrap_err(),
        below_grid("TEST-LF", "contract_scale", "lot", 1, 2)
    );
}

/// Inverse perpetual with a fractional contract value.
#[test]
fn test_from_spec_inverse_fractional_contract_value_maps_exactly() {
    let spec = inverse_perpetual(dec!(0.5), dec!(0.5), dec!(0.1));
    let instrument = Instrument::from_spec(&spec, scales(1, 1)).unwrap();
    assert_eq!(
        instrument.inverse_terms().unwrap().contract_value(),
        dec!(0.5)
    );
}

/// The tuple `TryFrom` is the same conversion, errors included.
#[test]
fn test_try_from_tuple_matches_from_spec() {
    let spec = binance_usdm::btcusdt_perpetual().unwrap();
    assert_eq!(
        Instrument::try_from((&spec, scales(1, 3))),
        Instrument::from_spec(&spec, scales(1, 3))
    );
    assert_eq!(
        Instrument::try_from((&spec, scales(0, 3))),
        Instrument::from_spec(&spec, scales(0, 3))
    );
}

// -------------------------------------------------------------- errors

/// An option has no ledger instrument, whatever the scales.
#[test]
fn test_from_spec_option_is_unsupported_before_scales() {
    let spec = deribit::btc_option(
        SpecId::new("BTC-27NOV26-100000-C").unwrap(),
        EXPIRY_MS,
        dec!(100000),
        OptionStyle::Call,
    )
    .unwrap();
    let unsupported = MarginError::UnsupportedSpec {
        instrument: iid("BTC-27NOV26-100000-C"),
        kind: ContractKind::Option,
        margining: Margining::Inverse,
    };
    assert_eq!(
        Instrument::from_spec(&spec, scales(4, 1)).unwrap_err(),
        unsupported
    );
    assert_eq!(
        Instrument::from_spec(&spec, scales(0, 0)).unwrap_err(),
        unsupported
    );
}

/// Price scale below the tick, contract scale below the lot.
#[test]
fn test_from_spec_scale_below_grid_is_refused() {
    let spec = binance_usdm::btcusdt_perpetual().unwrap();
    assert_eq!(
        Instrument::from_spec(&spec, scales(0, 3)).unwrap_err(),
        below_grid("BTCUSDT", "price_scale", "price ticks", 0, 1)
    );
    assert_eq!(
        Instrument::from_spec(&spec, scales(1, 2)).unwrap_err(),
        below_grid("BTCUSDT", "contract_scale", "lot", 2, 3)
    );
}

/// Every band counts: the finest tick first or last.
#[test]
fn test_from_spec_multi_band_schedule_requires_finest_tick() {
    let band = |up_to, tick| TickBand { up_to, tick };
    let finest_first = TickSchedule::new(vec![
        band(Some(dec!(10)), dec!(0.001)),
        band(None, dec!(0.5)),
    ])
    .unwrap();
    let spec = linear_perpetual(finest_first, dec!(1));
    assert!(Instrument::from_spec(&spec, scales(3, 0)).is_ok());
    assert_eq!(
        Instrument::from_spec(&spec, scales(2, 0)).unwrap_err(),
        below_grid("TEST-LINEAR", "price_scale", "price ticks", 2, 3)
    );
    let finest_last = TickSchedule::new(vec![
        band(Some(dec!(10)), dec!(0.5)),
        band(None, dec!(0.0001)),
    ])
    .unwrap();
    let spec = linear_perpetual(finest_last, dec!(1));
    assert_eq!(
        Instrument::from_spec(&spec, scales(3, 0)).unwrap_err(),
        below_grid("TEST-LINEAR", "price_scale", "price ticks", 3, 4)
    );
}

/// Trailing zeros do not count: tick 0.50 has 1 digit, lot 1.000 has 0.
#[test]
fn test_from_spec_trailing_zeros_are_ignored() {
    let spec = linear_perpetual(ticks(dec!(0.50)), dec!(1.000));
    assert!(Instrument::from_spec(&spec, scales(1, 0)).is_ok());
}

/// A tick at scale 28 needs 28 digits, above the ledger's maximum of 18.
#[test]
fn test_from_spec_scale_28_tick_is_refused_by_the_ledger_maximum() {
    let spec = linear_perpetual(ticks(Decimal::new(1, 28)), dec!(1));
    assert_eq!(
        Instrument::from_spec(&spec, scales(27, 0)).unwrap_err(),
        below_grid("TEST-LINEAR", "price_scale", "price ticks", 27, 28)
    );
    assert!(matches!(
        Instrument::from_spec(&spec, scales(28, 0)),
        Err(MarginError::InvalidInput {
            field: "price_scale",
            ..
        })
    ));
}

/// CV with 11 digits: contract scale 17 gives 28 digits (accepted), 18
/// gives 29 (refused).
#[test]
fn test_from_spec_contract_value_digits_above_28_are_refused() {
    let spec = inverse_perpetual(Decimal::new(1, 11), dec!(0.5), dec!(1));
    assert!(Instrument::from_spec(&spec, scales(1, 17)).is_ok());
    assert!(matches!(
        Instrument::from_spec(&spec, scales(1, 18)),
        Err(MarginError::InvalidInput {
            field: "contract_value",
            ..
        })
    ));
}

// ------------------------------------------------- scales and the ledger

/// Registration checks that need asset scales.
#[test]
fn test_register_converted_instruments_checks_asset_scales() {
    let binance =
        Instrument::from_spec(&binance_usdm::btcusdt_perpetual().unwrap(), scales(1, 3)).unwrap();
    let mut both = ledger(&[("BTC", 8), ("USDT", 8)]);
    both.register_instrument(binance.clone()).unwrap();
    assert!(matches!(
        ledger(&[("BTC", 8)]).register_instrument(binance),
        Err(MarginError::UnknownAsset { .. })
    ));

    let spot = spec(
        "BTC/USDT",
        ContractKind::Spot,
        Margining::Linear,
        dec!(1),
        ticks(dec!(0.01)),
        dec!(0.00001),
        None,
    );
    let spot = Instrument::from_spec(&spot, scales(2, 9)).unwrap();
    assert!(matches!(
        ledger(&[("BTC", 8), ("USDT", 8)]).register_instrument(spot),
        Err(MarginError::InvalidInput {
            field: "quantity_scale",
            ..
        })
    ));

    // Inverse: BTC scale + price scale must not exceed 28.
    let wide = Instrument::from_spec(&deribit::btc_perpetual().unwrap(), scales(11, 0)).unwrap();
    assert!(matches!(
        ledger(&[("BTC", 18)]).register_instrument(wide.clone()),
        Err(MarginError::InvalidInput {
            field: "price_scale",
            ..
        })
    ));
    assert!(ledger(&[("BTC", 17)]).register_instrument(wide).is_ok());
}

/// Tick 0.5 against a delivery price with 2 decimals: the price scale must
/// cover the settlement price, not only the tick.
#[test]
fn test_settle_expiry_finer_than_tick_needs_price_scale() {
    let future = spec(
        "TEST-INV-F",
        ContractKind::Future,
        Margining::Inverse,
        dec!(10),
        ticks(dec!(0.5)),
        dec!(1),
        Some(EXPIRY_MS),
    );
    let settle = |price_scale| {
        let mut ledger = ledger(&[("BTC", 8)]);
        ledger
            .register_instrument(Instrument::from_spec(&future, scales(price_scale, 0)).unwrap())
            .unwrap();
        ledger.settle_expiry(&ExpirySettlement {
            id: ExecutionId::new("settle").unwrap(),
            instrument: iid("TEST-INV-F"),
            price: dec!(61234.57),
            as_of_ms: EXPIRY_MS,
        })
    };
    assert!(matches!(
        settle(1),
        Err(MarginError::PrecisionExceeded {
            scale: 2,
            max_scale: 1,
            ..
        })
    ));
    assert!(settle(2).is_ok());
}

/// The scales cap digits only: 60000.3 has one digit, so the ledger books
/// it, while the specification refuses it off the 0.5 grid. The same for
/// a size of 0.01 against a lot of 0.25 at contract scale 2.
#[test]
fn test_ledger_books_off_grid_values_within_scale() {
    let spec = linear_perpetual(ticks(dec!(0.5)), dec!(0.25));
    assert!(matches!(
        spec.check_price(dec!(60000.3)),
        Err(InstrumentError::NotOnGrid { .. })
    ));
    assert!(matches!(
        spec.check_lot(dec!(0.01)),
        Err(InstrumentError::NotOnGrid { .. })
    ));
    let mut ledger = ledger(&[("USDT", 8)]);
    ledger
        .register_instrument(Instrument::from_spec(&spec, scales(1, 2)).unwrap())
        .unwrap();
    let fill = book(
        &mut ledger,
        "off-grid",
        "TEST-LINEAR",
        Action::Buy,
        dec!(60000.3),
        dec!(0.01),
        1,
    )
    .unwrap();
    // 0.01 x 60000.3 x 1 = 600.003 USDT, exact at 8 decimals.
    assert_eq!(
        fill.derivative.unwrap().virtual_notional,
        Some(dec!(600.003))
    );
}

/// A converted future refuses a fill at or after its expiry and settles
/// only at or after it.
#[test]
fn test_converted_future_expiry_boundaries() {
    let spec = deribit::btc_future(SpecId::new("BTC-27NOV26").unwrap(), EXPIRY_MS).unwrap();
    let mut ledger = ledger(&[("BTC", 8)]);
    ledger
        .register_instrument(Instrument::from_spec(&spec, scales(1, 0)).unwrap())
        .unwrap();
    let before = book(
        &mut ledger,
        "f1",
        "BTC-27NOV26",
        Action::Buy,
        dec!(60000),
        dec!(1),
        EXPIRY_MS - 1,
    );
    assert!(before.is_ok());
    assert!(matches!(
        book(
            &mut ledger,
            "f2",
            "BTC-27NOV26",
            Action::Buy,
            dec!(60000),
            dec!(1),
            EXPIRY_MS
        ),
        Err(MarginError::ExecutedAfterExpiry { .. })
    ));
    let settlement = |as_of_ms| ExpirySettlement {
        id: ExecutionId::new("settle").unwrap(),
        instrument: iid("BTC-27NOV26"),
        price: dec!(61000),
        as_of_ms,
    };
    assert!(matches!(
        ledger.settle_expiry(&settlement(EXPIRY_MS - 1)),
        Err(MarginError::InvalidInput {
            field: "as_of_ms",
            ..
        })
    ));
    assert!(ledger.settle_expiry(&settlement(EXPIRY_MS)).is_ok());
}

// ------------------------------------------ inverse PnL: legs and residuals

fn residual(r: &InverseResidual) -> Q {
    Q::ratio(r.numerator, r.denominator)
}

/// Opens and closes `n` contracts of a converted inverse perpetual (BTC at
/// 8 decimals); returns the two fills.
fn round_trip(
    cv: Decimal,
    lot: Decimal,
    side: Action,
    n: Decimal,
    entry: Decimal,
    exit: Decimal,
) -> (ExecutionCashflow, ExecutionCashflow) {
    let spec = inverse_perpetual(cv, dec!(0.5), lot);
    let mut ledger = ledger(&[("BTC", 8)]);
    ledger
        .register_instrument(Instrument::from_spec(&spec, scales(1, 1)).unwrap())
        .unwrap();
    let close = match side {
        Action::Buy => Action::Sell,
        _ => Action::Buy,
    };
    let open = book(&mut ledger, "open", "TEST-INVERSE", side, entry, n, 1).unwrap();
    let close = book(&mut ledger, "close", "TEST-INVERSE", close, exit, n, 2).unwrap();
    (open, close)
}

/// Checks the hand literals of both legs and the exact identity.
///
/// Each fill's `InverseResidual` is the exact rest of that fill's value,
/// in BTC: `n·CV/p = V + numerator/denominator`, with `numerator` in USD
/// (`n·CV - V·p`) and `denominator` the fill price in USD per BTC. The open
/// fill's residual belongs to the open leg, the close fill's to the close
/// leg. Then, with `s = +1` for a long and `-1` for a short:
/// `s·n·CV·(1/entry - 1/exit) = realized + s·(r_open - r_close)`.
#[allow(clippy::too_many_arguments)]
fn check_legs(
    legs: &(ExecutionCashflow, ExecutionCashflow),
    long: bool,
    v_open: Decimal,
    r_open: Q,
    v_close: Decimal,
    r_close: Q,
    realized: Decimal,
    exact: Q,
) {
    let (open, close) = legs;
    let open_fill = open.derivative.as_ref().unwrap();
    let close_fill = close.derivative.as_ref().unwrap();
    assert_eq!(open_fill.virtual_notional, Some(v_open));
    assert_eq!(close_fill.virtual_notional, Some(v_close));
    let (ro, rc) = (
        residual(open_fill.residual.as_ref().unwrap()),
        residual(close_fill.residual.as_ref().unwrap()),
    );
    assert_eq!(ro, r_open);
    assert_eq!(rc, r_close);
    assert_eq!(close.realized_pnl, realized);
    let rests = ro.sub(rc);
    let rests = if long { rests } else { rests.neg() };
    assert_eq!(Q::dec(realized).add(rests), exact);
}

/// Long 1000 x CV 10 from 60000 to 61000: the ledger and the net
/// settlement agree.
///
/// `V_open` = rhe(10000/60000) = 0.16666667, rest -0.0002/60000;
/// `V_close` = rhe(10000/61000) = 0.16393443, rest -0.00023/61000;
/// realized 0.00273224; exact 1/366, settled once 0.00273224.
#[test]
fn test_inverse_long_round_trip_per_leg_equals_net_settlement() {
    let legs = round_trip(
        dec!(10),
        dec!(1),
        Action::Buy,
        dec!(1000),
        dec!(60000),
        dec!(61000),
    );
    check_legs(
        &legs,
        true,
        dec!(0.16666667),
        Q::ratio(dec!(-0.0002), dec!(60000)),
        dec!(0.16393443),
        Q::ratio(dec!(-0.00023), dec!(61000)),
        dec!(0.00273224),
        Q::new(1, 366),
    );
}

/// Long 100 x CV 10 from 60000 to 61000: the ledger books one satoshi more
/// than the net settlement.
///
/// `V_open` = 0.01666667, rest -0.0002/60000; `V_close` = rhe(1000/61000) =
/// 0.01639344, rest +0.00016/61000; realized 0.00027323; exact 1/3660,
/// settled once 0.00027322 (difference +0.00000001).
#[test]
fn test_inverse_long_gain_per_leg_is_one_satoshi_above_net() {
    let legs = round_trip(
        dec!(10),
        dec!(1),
        Action::Buy,
        dec!(100),
        dec!(60000),
        dec!(61000),
    );
    check_legs(
        &legs,
        true,
        dec!(0.01666667),
        Q::ratio(dec!(-0.0002), dec!(60000)),
        dec!(0.01639344),
        Q::ratio(dec!(0.00016), dec!(61000)),
        dec!(0.00027323),
        Q::new(1, 3660),
    );
    assert_eq!(dec!(0.00027323) - dec!(0.00027322), dec!(0.00000001));
}

/// Short 100 x CV 10 from 60000 to 61000: one satoshi below the net
/// settlement. Same legs as the long; realized -0.00027323; exact -1/3660,
/// settled once -0.00027322 (difference -0.00000001).
#[test]
fn test_inverse_short_loss_per_leg_is_one_satoshi_below_net() {
    let legs = round_trip(
        dec!(10),
        dec!(1),
        Action::Sell,
        dec!(100),
        dec!(60000),
        dec!(61000),
    );
    check_legs(
        &legs,
        false,
        dec!(0.01666667),
        Q::ratio(dec!(-0.0002), dec!(60000)),
        dec!(0.01639344),
        Q::ratio(dec!(0.00016), dec!(61000)),
        dec!(-0.00027323),
        Q::new(-1, 3660),
    );
    assert_eq!(dec!(-0.00027323) - dec!(-0.00027322), dec!(-0.00000001));
}

/// Long 100 x CV 10 from 60000 down to 59000, a loss.
///
/// `V_close` = rhe(1000/59000) = 0.01694915, rest +0.00015/59000; realized
/// -0.00028248; exact -1/3540, settled once -0.00028249.
#[test]
fn test_inverse_long_loss_per_leg_and_exact_identity() {
    let legs = round_trip(
        dec!(10),
        dec!(1),
        Action::Buy,
        dec!(100),
        dec!(60000),
        dec!(59000),
    );
    check_legs(
        &legs,
        true,
        dec!(0.01666667),
        Q::ratio(dec!(-0.0002), dec!(60000)),
        dec!(0.01694915),
        Q::ratio(dec!(0.00015), dec!(59000)),
        dec!(-0.00028248),
        Q::new(-1, 3540),
    );
}

/// Fractional contract value and quantity: 2.5 contracts x CV 0.5 USD from
/// 60000 to 61000.5, long and short.
///
/// `V_open` = rhe(1.25/60000) = 0.00002083, rest +0.0002/60000; `V_close` =
/// rhe(1.25/61000.5) = 0.00002049, rest +0.000099755/61000.5; realized
/// ±0.00000034; exact ±1.25·1000.5/(60000·61000.5) = ±667/1952016000.
#[test]
fn test_inverse_fractional_contracts_exact_identity_both_sides() {
    for (side, long, realized, exact) in [
        (
            Action::Buy,
            true,
            dec!(0.00000034),
            Q::new(667, 1_952_016_000),
        ),
        (
            Action::Sell,
            false,
            dec!(-0.00000034),
            Q::new(-667, 1_952_016_000),
        ),
    ] {
        let legs = round_trip(
            dec!(0.5),
            dec!(0.1),
            side,
            dec!(2.5),
            dec!(60000),
            dec!(61000.5),
        );
        check_legs(
            &legs,
            long,
            dec!(0.00002083),
            Q::ratio(dec!(0.0002), dec!(60000)),
            dec!(0.00002049),
            Q::ratio(dec!(0.000099755), dec!(61000.5)),
            realized,
            exact,
        );
    }
}
