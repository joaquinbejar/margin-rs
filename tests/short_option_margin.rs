//! Hand computed fixtures for `short-option-im/1`, with `a` 15 %, `b` 10 % and
//! `m` 7.5 %. The expected values follow the formula in `docs/options.md`, the
//! crate's own specification, and are written out beside each case.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::RuleId;
use margin_rs::instrument::{
    ExerciseStyle, LinearTerms, OptionContract, OptionDelivery, OptionInstrument, OptionMargining,
    OptionRight,
};
use margin_rs::ledger::{Action, Execution, Ledger};
use margin_rs::maintenance::{AccountState, CollateralView, assess};
use margin_rs::rules::{OptionExposure, OptionExposureView, ShortOptionIm, ShortOptionParameters};
use margin_rs::valuation::{Mark, MarkSet, PriceSource, ValuationPolicy, value};
use margin_rs::{
    AccountId, Asset, Decimal, ExecutionId, InstrumentId, MarginError, TransferId, VenueId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}
fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}
fn account() -> AccountId {
    AccountId::new("acct").unwrap()
}
fn venue() -> VenueId {
    VenueId::new("venue").unwrap()
}

fn linear(id: &str, right: OptionRight, strike: Decimal) -> OptionInstrument {
    OptionInstrument::new(
        LinearTerms::new(iid(id), asset("USDC"), dec!(1), 4, 2).unwrap(),
        OptionContract {
            underlying: asset("BTC"),
            strike,
            expiry_ms: 1_000,
            right,
            exercise: ExerciseStyle::European,
            delivery: OptionDelivery::Cash,
            margining: OptionMargining::Linear,
        },
    )
    .unwrap()
}

fn inverse(right: OptionRight, strike: Decimal) -> OptionInstrument {
    OptionInstrument::new(
        LinearTerms::new(iid("INVERSE"), asset("BTC"), dec!(1), 4, 2).unwrap(),
        OptionContract {
            underlying: asset("BTC"),
            strike,
            expiry_ms: 1_000,
            right,
            exercise: ExerciseStyle::European,
            delivery: OptionDelivery::Cash,
            margining: OptionMargining::Inverse,
        },
    )
    .unwrap()
}

fn view(positions: Vec<OptionExposure>) -> OptionExposureView {
    OptionExposureView {
        account: account(),
        venue: venue(),
        positions,
    }
}

fn exposure(option: OptionInstrument, contracts: Decimal, underlying: Decimal) -> OptionExposure {
    OptionExposure {
        option,
        contracts,
        underlying_price: underlying,
    }
}

fn usdc_rule() -> ShortOptionIm {
    ShortOptionIm::short_option_v1(asset("USDC"), 6).unwrap()
}

/// `(initial, maintenance)` of one position under the USDC rule.
fn margins(
    option: OptionInstrument,
    contracts: Decimal,
    underlying: Decimal,
) -> (Decimal, Decimal) {
    let requirement = usdc_rule()
        .requirement(&view(vec![exposure(option, contracts, underlying)]))
        .unwrap();
    (requirement.initial, requirement.maintenance)
}

/// At the money: `2 * max(15 - 0, 10) = 30`, `2 * 7.5 = 15`.
#[test]
fn test_requirement_short_call_at_the_money_uses_full_rate() {
    let call = linear("C", OptionRight::Call, dec!(100));
    assert_eq!(margins(call, dec!(-2), dec!(100)), (dec!(30), dec!(15)));
}

/// In the money nothing is deducted: `2 * 0.15 * 120 = 36`, `2 * 9 = 18`.
#[test]
fn test_requirement_short_call_in_the_money_uses_full_rate() {
    let call = linear("C", OptionRight::Call, dec!(100));
    assert_eq!(margins(call, dec!(-2), dec!(120)), (dec!(36), dec!(18)));
}

/// `U` 98: `max(14.7 - 2, 9.8) = 12.7`, times 2; `2 * 7.35 = 14.7`.
#[test]
fn test_requirement_short_call_out_of_the_money_deducts_otm() {
    let call = linear("C", OptionRight::Call, dec!(100));
    assert_eq!(margins(call, dec!(-2), dec!(98)), (dec!(25.4), dec!(14.7)));
}

/// `U` 90: `max(13.5 - 10, 9) = 9`, times 2; `2 * 6.75 = 13.5`.
#[test]
fn test_requirement_short_call_far_out_of_the_money_hits_floor() {
    let call = linear("C", OptionRight::Call, dec!(100));
    assert_eq!(margins(call, dec!(-2), dec!(90)), (dec!(18), dec!(13.5)));
}

/// `K` 105, `U` 100: `15 - 5 = 10 = 0.1 * 100`, the two branches meet.
#[test]
fn test_requirement_short_call_at_floor_boundary_is_continuous() {
    let call = linear("C", OptionRight::Call, dec!(105));
    assert_eq!(margins(call, dec!(-2), dec!(100)).0, dec!(20));
}

/// `U` 102: `max(15.3 - 2, 10.2) = 13.3`, times 2; `2 * 7.65 = 15.3`.
#[test]
fn test_requirement_short_put_out_of_the_money_deducts_otm() {
    let put = linear("P", OptionRight::Put, dec!(100));
    assert_eq!(margins(put, dec!(-2), dec!(102)), (dec!(26.6), dec!(15.3)));
}

/// `U` 110: `max(16.5 - 10, 11) = 11`, times 2; `2 * 8.25 = 16.5`.
#[test]
fn test_requirement_short_put_far_out_of_the_money_hits_floor() {
    let put = linear("P", OptionRight::Put, dec!(100));
    assert_eq!(margins(put, dec!(-2), dec!(110)), (dec!(22), dec!(16.5)));
}

/// `U` 80, in the money: `2 * 0.15 * 80 = 24`, `2 * 6 = 12`.
#[test]
fn test_requirement_short_put_in_the_money_uses_full_rate() {
    let put = linear("P", OptionRight::Put, dec!(100));
    assert_eq!(margins(put, dec!(-2), dec!(80)), (dec!(24), dec!(12)));
}

/// The multiplier scales both: `0.1 * 3 * 0.15 * 100 = 4.5`, `2.25`.
#[test]
fn test_requirement_multiplier_scales_both_margins() {
    let option = OptionInstrument::new(
        LinearTerms::new(iid("TENTH"), asset("USDC"), dec!(0.1), 4, 2).unwrap(),
        OptionContract {
            underlying: asset("BTC"),
            strike: dec!(100),
            expiry_ms: 1_000,
            right: OptionRight::Call,
            exercise: ExerciseStyle::European,
            delivery: OptionDelivery::Cash,
            margining: OptionMargining::Linear,
        },
    )
    .unwrap();
    assert_eq!(
        margins(option, dec!(-3), dec!(100)),
        (dec!(4.5), dec!(2.25))
    );
}

/// A long's premium is paid: nothing required, the position still reported.
#[test]
fn test_requirement_long_requires_nothing() {
    let call = linear("C", OptionRight::Call, dec!(100));
    let requirement = usdc_rule()
        .requirement(&view(vec![exposure(call, dec!(2), dec!(100))]))
        .unwrap();
    assert_eq!(requirement.initial, dec!(0));
    assert_eq!(requirement.maintenance, dec!(0));
    assert_eq!(requirement.positions[0].contracts, dec!(2));
    assert_eq!(requirement.positions[0].notional, Some(dec!(200)));
}

/// `0.15 * 33.33 = 4.9995` and `0.075 * 33.33 = 2.49975` round up at 2 digits.
#[test]
fn test_requirement_rounds_up_once_to_scale() {
    let rule = ShortOptionIm::short_option_v1(asset("USDC"), 2).unwrap();
    let call = linear("C", OptionRight::Call, dec!(10));
    let requirement = rule
        .requirement(&view(vec![exposure(call, dec!(-1), dec!(33.33))]))
        .unwrap();
    assert_eq!(requirement.initial, dec!(5));
    assert_eq!(requirement.maintenance, dec!(2.5));
}

/// No netting: a long and a short add nothing against each other, and totals
/// are the exact sums. `30 + 0`, `15 + 0`, plus a put at `U` 100: `30`, `15`.
#[test]
fn test_requirement_positions_add_with_no_netting() {
    let requirement = usdc_rule()
        .requirement(&view(vec![
            exposure(
                linear("C1", OptionRight::Call, dec!(100)),
                dec!(-2),
                dec!(100),
            ),
            exposure(
                linear("C2", OptionRight::Call, dec!(100)),
                dec!(2),
                dec!(100),
            ),
            exposure(
                linear("P1", OptionRight::Put, dec!(100)),
                dec!(-2),
                dec!(100),
            ),
        ]))
        .unwrap();
    assert_eq!(requirement.initial, dec!(60));
    assert_eq!(requirement.maintenance, dec!(30));
    assert_eq!(requirement.positions.len(), 3);
    assert_eq!(requirement.positions[1].initial, dec!(0));
    assert_eq!(requirement.rule.to_string(), "short-option-im/1");
    assert_eq!(requirement.currency, asset("USDC"));
}

fn btc_margins(
    option: OptionInstrument,
    contracts: Decimal,
    underlying: Decimal,
) -> (Decimal, Decimal) {
    let rule = ShortOptionIm::short_option_v1(asset("BTC"), 8).unwrap();
    let requirement = rule
        .requirement(&view(vec![exposure(option, contracts, underlying)]))
        .unwrap();
    (requirement.initial, requirement.maintenance)
}

/// Inverse, in BTC: `2 * 0.15 = 0.3`, `2 * 0.075 = 0.15`.
#[test]
fn test_requirement_inverse_at_the_money_is_in_the_underlying() {
    let call = inverse(OptionRight::Call, dec!(100000));
    assert_eq!(
        btc_margins(call, dec!(-2), dec!(100000)),
        (dec!(0.3), dec!(0.15))
    );
}

/// `U` 98000: `2 * (0.15 - 2000/98000) = 2 * 12700/98000 = 0.259183673...`,
/// up to `0.25918368`. The maintenance margin does not depend on `U`.
#[test]
fn test_requirement_inverse_out_of_the_money_rounds_quotient_up() {
    let call = inverse(OptionRight::Call, dec!(100000));
    assert_eq!(
        btc_margins(call, dec!(-2), dec!(98000)),
        (dec!(0.25918368), dec!(0.15))
    );
}

/// `U` 90000: `0.15 - 10000/90000 = 0.0388...`, below `0.1`: `2 * 0.1`.
#[test]
fn test_requirement_inverse_far_out_of_the_money_hits_floor() {
    let call = inverse(OptionRight::Call, dec!(100000));
    assert_eq!(
        btc_margins(call, dec!(-2), dec!(90000)),
        (dec!(0.2), dec!(0.15))
    );
}

/// `U` 110000 on a put: `0.15 - 10000/110000 = 0.0590...`, below `0.1`.
#[test]
fn test_requirement_inverse_put_hits_floor() {
    let put = inverse(OptionRight::Put, dec!(100000));
    assert_eq!(
        btc_margins(put, dec!(-2), dec!(110000)),
        (dec!(0.2), dec!(0.15))
    );
}

/// Never a division by zero, and no margin from a price that is not one.
#[test]
fn test_requirement_underlying_not_positive_is_refused() {
    for underlying in [dec!(0), dec!(-1)] {
        for (rule, option) in [
            (usdc_rule(), linear("C", OptionRight::Call, dec!(100))),
            (
                ShortOptionIm::short_option_v1(asset("BTC"), 8).unwrap(),
                inverse(OptionRight::Call, dec!(100000)),
            ),
        ] {
            let error = rule
                .requirement(&view(vec![exposure(option, dec!(-1), underlying)]))
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    MarginError::InvalidInput {
                        field: "underlying_price",
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }
}

/// An overflow is an error, never a smaller or zero requirement.
#[test]
fn test_requirement_overflow_is_an_error() {
    let call = linear("C", OptionRight::Call, dec!(100));
    let error = usdc_rule()
        .requirement(&view(vec![exposure(call, Decimal::MIN, Decimal::MAX)]))
        .unwrap_err();
    assert!(matches!(error, MarginError::Arithmetic { .. }), "{error:?}");
}

#[test]
fn test_requirement_other_currency_is_refused() {
    let call = inverse(OptionRight::Call, dec!(100000));
    let error = usdc_rule()
        .requirement(&view(vec![exposure(call, dec!(-1), dec!(100000))]))
        .unwrap_err();
    assert!(
        matches!(error, MarginError::CurrencyMismatch { .. }),
        "{error:?}"
    );
}

#[test]
fn test_requirement_same_instrument_twice_is_refused() {
    let call = linear("C", OptionRight::Call, dec!(100));
    let error = usdc_rule()
        .requirement(&view(vec![
            exposure(call.clone(), dec!(-1), dec!(100)),
            exposure(call, dec!(-1), dec!(100)),
        ]))
        .unwrap_err();
    assert!(matches!(
        error,
        MarginError::InvalidInput {
            field: "positions",
            ..
        }
    ));
}

/// The same inputs give the same requirement.
#[test]
fn test_requirement_is_deterministic() {
    let inputs = view(vec![
        exposure(
            linear("C", OptionRight::Call, dec!(100)),
            dec!(-2),
            dec!(98),
        ),
        exposure(linear("P", OptionRight::Put, dec!(100)), dec!(1), dec!(98)),
    ]);
    assert_eq!(
        usdc_rule().requirement(&inputs).unwrap(),
        usdc_rule().requirement(&inputs).unwrap()
    );
}

#[test]
fn test_new_invalid_parameters_are_refused() {
    let parameters = |a: Decimal, b: Decimal, m: Decimal| ShortOptionParameters {
        initial_rate: a,
        floor_rate: b,
        maintenance_rate: m,
    };
    for invalid in [
        parameters(dec!(0.1), dec!(0.15), dec!(0.075)),
        parameters(dec!(1.1), dec!(0.1), dec!(0.075)),
        parameters(dec!(0.15), dec!(0), dec!(0)),
        parameters(dec!(0.15), dec!(-0.1), dec!(0.075)),
        parameters(dec!(0.15), dec!(0.1), dec!(0)),
        parameters(dec!(0.15), dec!(0.1), dec!(0.11)),
    ] {
        assert!(matches!(
            ShortOptionIm::new(RuleId::new("x", 1), asset("USDC"), 6, invalid),
            Err(MarginError::InvalidInput {
                field: "parameters",
                ..
            })
        ));
    }
    let valid = parameters(dec!(0.2), dec!(0.2), dec!(0.2));
    let rule = ShortOptionIm::new(RuleId::new("x", 1), asset("USDC"), 6, valid).unwrap();
    assert_eq!(rule.parameters(), valid);
    assert_eq!(rule.currency(), &asset("USDC"));
    assert!(matches!(
        ShortOptionIm::new(RuleId::new("x", 1), asset("USDC"), 19, valid),
        Err(MarginError::InvalidInput { .. })
    ));
}

/// A ledger with a short and a long option, a flat one and one in BTC.
fn ledger(deposit: Decimal) -> Ledger {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).unwrap();
    ledger.register_asset(asset("BTC"), 8).unwrap();
    for option in [
        linear("SHORT", OptionRight::Call, dec!(100)),
        linear("LONG", OptionRight::Put, dec!(100)),
        linear("FLAT", OptionRight::Put, dec!(100)),
        inverse(OptionRight::Call, dec!(100000)),
    ] {
        ledger.register_instrument(option).unwrap();
    }
    if !deposit.is_zero() {
        ledger
            .deposit(TransferId::new("d").unwrap(), asset("USDC"), deposit)
            .unwrap();
    }
    let execution = Execution::new(
        ExecutionId::new("e1").unwrap(),
        iid("SHORT"),
        Action::Sell,
        dec!(5),
        dec!(2),
    )
    .with_executed_at(1);
    ledger
        .book_execution_allowing_overdraft(&execution, None)
        .unwrap();
    ledger
}

#[test]
fn test_from_snapshot_takes_open_options_in_currency() {
    let mut ledger = ledger(dec!(100));
    for (id, instrument, side, n) in [
        ("e2", "LONG", Action::Buy, dec!(1)),
        ("e3", "FLAT", Action::Buy, dec!(1)),
        ("e4", "FLAT", Action::Sell, dec!(1)),
        ("e5", "INVERSE", Action::Sell, dec!(1)),
    ] {
        let execution = Execution::new(
            ExecutionId::new(id).unwrap(),
            iid(instrument),
            side,
            dec!(1),
            n,
        )
        .with_executed_at(1);
        ledger
            .book_execution_allowing_overdraft(&execution, None)
            .unwrap();
    }
    let prices = BTreeMap::from([(iid("SHORT"), dec!(98)), (iid("LONG"), dec!(98))]);
    let view = OptionExposureView::from_snapshot(
        &ledger.snapshot(),
        account(),
        venue(),
        &asset("USDC"),
        &prices,
    )
    .unwrap();
    assert_eq!(view.positions.len(), 2);
    let requirement = usdc_rule().requirement(&view).unwrap();
    // The short: as in the fixture at `U` 98; the long adds nothing.
    assert_eq!(requirement.initial, dec!(25.4));
    assert_eq!(requirement.maintenance, dec!(14.7));
}

#[test]
fn test_from_snapshot_missing_underlying_price_is_an_error() {
    let ledger = ledger(dec!(100));
    assert!(matches!(
        OptionExposureView::from_snapshot(
            &ledger.snapshot(),
            account(),
            venue(),
            &asset("USDC"),
            &BTreeMap::new(),
        ),
        Err(MarginError::MissingMark { .. })
    ));
}

/// The state of a short from its deposit: premium +10, mark 8 so value -16,
/// `U` 98 so `im` 25.4 and `mm` 14.7, and the mark counted only in collateral.
fn state_with(deposit: Decimal) -> (AccountState, Option<Decimal>) {
    let ledger = ledger(deposit);
    let marks = MarkSet::new(10_000)
        .with_instrument(iid("SHORT"), Mark::new(dec!(8), 9_500, PriceSource::Mid));
    let valuation = value(
        &ledger.snapshot(),
        &marks,
        &ValuationPolicy::new(asset("USDC"), 1_000),
    )
    .unwrap();
    let prices = BTreeMap::from([(iid("SHORT"), dec!(98))]);
    let view = OptionExposureView::from_snapshot(
        &ledger.snapshot(),
        account(),
        venue(),
        &asset("USDC"),
        &prices,
    )
    .unwrap();
    let requirement = usdc_rule().requirement(&view).unwrap();
    let decision = assess(
        AccountState::Healthy,
        &CollateralView::from_valuation(&valuation),
        &requirement,
    )
    .unwrap();
    (decision.state, valuation.collateral_equity)
}

/// Collateral is `deposit + 10 - 16`, against a maintenance margin of 14.7.
#[test]
fn test_assess_short_option_follows_collateral_against_maintenance() {
    assert_eq!(
        state_with(dec!(100)),
        (AccountState::Healthy, Some(dec!(94)))
    );
    // 20.7 - 6 = 14.7: exactly at maintenance.
    assert_eq!(
        state_with(dec!(20.7)),
        (AccountState::Healthy, Some(dec!(14.7)))
    );
    assert_eq!(
        state_with(dec!(15)),
        (AccountState::ReduceOnly, Some(dec!(9)))
    );
}
