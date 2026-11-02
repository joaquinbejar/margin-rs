//! Risk gate fixtures for fees charged in the base asset (#37).
//!
//! An order reserves the asset it pays. A fee is reserved when it is
//! charged in that asset (a sell with `FeeAsset::Base`: `lot +
//! round_up(r * lot)` per lot) and netted from the fill when it is charged
//! in the asset the order receives (a buy with `FeeAsset::Base`: the fill
//! credits `q` and debits the fee in one booking). Expected values are
//! written out by hand.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::ledger::{Action, Booking, Execution, Ledger};
use margin_rs::risk::{
    FeeAsset, FillReport, HaltReason, InstrumentLimits, OrderRequest, RiskDecision, RiskGate,
    RiskReason,
};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, OrderId, SpotInstrument, TransferId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}
fn oid(id: &str) -> OrderId {
    OrderId::new(id).unwrap()
}
fn pair() -> InstrumentId {
    InstrumentId::new("BTC-USDC").unwrap()
}

/// Fees in base at `rate`.
fn base_fee(rate: Decimal) -> InstrumentLimits {
    InstrumentLimits {
        max_fee_rate: rate,
        fee_asset: FeeAsset::Base,
        ..InstrumentLimits::default()
    }
}

/// USDC at 6 decimals, BTC at 8, BTC-USDC with price scale 2 and quantity
/// scale `lot_scale`; `usdc` and `btc` deposited.
fn gate(lot_scale: u32, usdc: Decimal, btc: Decimal, limits: InstrumentLimits) -> RiskGate {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).unwrap();
    ledger.register_asset(asset("BTC"), 8).unwrap();
    ledger
        .register_instrument(
            SpotInstrument::new(pair(), asset("BTC"), asset("USDC"), 2, lot_scale).unwrap(),
        )
        .unwrap();
    let mut gate = RiskGate::new(ledger);
    if usdc > Decimal::ZERO {
        gate.deposit(TransferId::new("d-usdc").unwrap(), asset("USDC"), usdc)
            .unwrap();
    }
    if btc > Decimal::ZERO {
        gate.deposit(TransferId::new("d-btc").unwrap(), asset("BTC"), btc)
            .unwrap();
    }
    gate.set_limits(&pair(), limits).unwrap();
    gate
}

fn order(id: &str, side: Action, price: Decimal, quantity: Decimal) -> OrderRequest {
    OrderRequest::new(oid(id), pair(), side, price, quantity)
}

/// A fill with its fee and the order's cumulative quantity after it.
fn fill(
    id: &str,
    side: Action,
    (price, quantity): (Decimal, Decimal),
    fee: Option<(&str, Decimal)>,
    cumulative: Decimal,
) -> FillReport {
    let mut execution =
        Execution::new(ExecutionId::new(id).unwrap(), pair(), side, price, quantity);
    if let Some((code, amount)) = fee {
        execution = execution.with_fee(asset(code), amount);
    }
    FillReport::new(execution, cumulative)
}

/// (total, reserved, available) of an asset.
fn balance(gate: &RiskGate, code: &str) -> (Decimal, Decimal, Decimal) {
    let b = gate.ledger().balance(&asset(code)).unwrap();
    (b.total, b.reserved, b.available)
}

#[test]
fn test_sell_base_fee_reserves_quantity_and_fee_then_releases_on_cancel() {
    // Lot 0.001, r 0.001: per lot 0.001 + round_up(0.000001) = 0.001001.
    // Sell 2 (2000 lots) on 3 BTC reserves 2.002. A fill of 1 at 100 with a
    // 0.001 BTC fee: BTC 3 - 1.001 = 1.999, reservation 1.001 for the rest;
    // USDC +100. The same fill again changes nothing; the cancel releases
    // 1.001.
    let mut gate = gate(3, dec!(0), dec!(3), base_fee(dec!(0.001)));
    assert_eq!(
        gate.submit(&order("s1", Action::Sell, dec!(100), dec!(2)), 0),
        RiskDecision::Accepted { quantity: dec!(2) }
    );
    assert_eq!(
        gate.order(&oid("s1"))
            .unwrap()
            .terms
            .as_ref()
            .unwrap()
            .per_lot,
        dec!(0.001001)
    );
    assert_eq!(balance(&gate, "BTC"), (dec!(3), dec!(2.002), dec!(0.998)));
    let f1 = fill(
        "e1",
        Action::Sell,
        (dec!(100), dec!(1)),
        Some(("BTC", dec!(0.001))),
        dec!(1),
    );
    let first = gate.on_fill(&oid("s1"), &f1).unwrap();
    assert!(first.shortfalls.is_empty() && first.anomalies.is_empty());
    assert_eq!(
        balance(&gate, "BTC"),
        (dec!(1.999), dec!(1.001), dec!(0.998))
    );
    assert_eq!(balance(&gate, "USDC"), (dec!(100), dec!(0), dec!(100)));
    // A replay books nothing twice.
    assert!(matches!(
        gate.on_fill(&oid("s1"), &f1).unwrap().booking,
        Booking::Duplicate(_)
    ));
    assert_eq!(
        balance(&gate, "BTC"),
        (dec!(1.999), dec!(1.001), dec!(0.998))
    );
    assert_eq!(gate.on_cancel_confirmed(&oid("s1")), Ok(dec!(1.001)));
    assert_eq!(balance(&gate, "BTC"), (dec!(1.999), dec!(0), dec!(1.999)));
    assert_eq!(gate.halted(), None);
    gate.ledger().reconcile().unwrap();
}

#[test]
fn test_sell_base_fee_is_released_on_a_venue_reject() {
    let mut gate = gate(3, dec!(0), dec!(3), base_fee(dec!(0.001)));
    gate.submit(&order("s1", Action::Sell, dec!(100), dec!(2)), 0);
    assert_eq!(gate.on_venue_rejected(&oid("s1")), Ok(dec!(2.002)));
    assert_eq!(balance(&gate, "BTC"), (dec!(3), dec!(0), dec!(3)));
}

#[test]
fn test_sell_base_fee_covers_one_lot_fills_at_the_maximum_fee() {
    // Lot 0.001, r 0.000015: r * lot = 1.5e-8, rounded up to 2e-8, per lot
    // 0.00100002. Sell 5 lots on exactly 0.0050001 BTC: nothing free. Five
    // one lot fills, each charged the maximum round_up(1.5e-8) = 2e-8,
    // debit 0.00100002 each; the reservation follows the rest down to 0
    // with no shortfall and nothing left.
    let mut gate = gate(3, dec!(0), dec!(0.0050001), base_fee(dec!(0.000015)));
    assert_eq!(
        gate.submit(&order("s1", Action::Sell, dec!(100), dec!(0.005)), 0),
        RiskDecision::Accepted {
            quantity: dec!(0.005)
        }
    );
    assert_eq!(
        balance(&gate, "BTC"),
        (dec!(0.0050001), dec!(0.0050001), dec!(0))
    );
    for i in 1..=5_i64 {
        let outcome = gate
            .on_fill(
                &oid("s1"),
                &fill(
                    &format!("e{i}"),
                    Action::Sell,
                    (dec!(100), dec!(0.001)),
                    Some(("BTC", dec!(0.00000002))),
                    Decimal::new(i, 3),
                ),
            )
            .unwrap();
        assert!(outcome.shortfalls.is_empty());
        let rest = Decimal::new(5 - i, 0) * dec!(0.00100002);
        assert_eq!(balance(&gate, "BTC"), (rest, rest, dec!(0)));
    }
    assert_eq!(gate.halted(), None);
    gate.ledger().reconcile().unwrap();
}

#[test]
fn test_sell_base_fee_mixed_fills_free_what_rounding_spares() {
    // As above, fills of 2 and 3 lots at their maximum fees round_up(3e-8)
    // = 3e-8 and round_up(4.5e-8) = 5e-8: debits 0.00200003 and 0.00300005
    // against reservations of 0.00200004 and 0.00300006; 2e-8 ends free.
    let mut gate = gate(3, dec!(0), dec!(0.0050001), base_fee(dec!(0.000015)));
    gate.submit(&order("s1", Action::Sell, dec!(100), dec!(0.005)), 0);
    for (id, n, fee, cumulative) in [
        ("e1", dec!(0.002), dec!(0.00000003), dec!(0.002)),
        ("e2", dec!(0.003), dec!(0.00000005), dec!(0.005)),
    ] {
        let outcome = gate
            .on_fill(
                &oid("s1"),
                &fill(
                    id,
                    Action::Sell,
                    (dec!(100), n),
                    Some(("BTC", fee)),
                    cumulative,
                ),
            )
            .unwrap();
        assert!(outcome.shortfalls.is_empty());
    }
    assert_eq!(
        balance(&gate, "BTC"),
        (dec!(0.00000002), dec!(0), dec!(0.00000002))
    );
}

#[test]
fn test_sell_base_fee_on_a_fine_lot_reserves_twice_the_quantity() {
    // Lot 1e-8 (the base grid), r 0.001: round_up(1e-11) = 1e-8, so per lot
    // 2e-8. Sell 2 BTC needs 4 BTC: with 3, it is rejected, or reduced to
    // floor(3 / 2e-8) lots = 1.5 BTC.
    let mut gate = gate(8, dec!(0), dec!(3), base_fee(dec!(0.001)));
    assert_eq!(
        gate.submit(&order("s1", Action::Sell, dec!(100), dec!(2)), 0),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::InsufficientBalance {
                asset: asset("BTC"),
                required: dec!(4),
                available: dec!(3),
            }]
        }
    );
    assert_eq!(
        gate.submit(
            &order("s2", Action::Sell, dec!(100), dec!(2)).reducible(),
            0
        ),
        RiskDecision::Reduced {
            requested: dec!(2),
            quantity: dec!(1.5),
            reasons: vec![RiskReason::InsufficientBalance {
                asset: asset("BTC"),
                required: dec!(4),
                available: dec!(3),
            }],
        }
    );
    assert_eq!(balance(&gate, "BTC"), (dec!(3), dec!(3), dec!(0)));
}

#[test]
fn test_sell_base_fee_at_the_bound_fits_and_one_unit_over_is_a_shortfall() {
    // Lot 0.001, r 0.001, sell 2 on exactly 2.002 BTC. One fill of 2 with
    // the maximum fee round_up(0.002) = 0.002 leaves 0 and no shortfall.
    let mut at_bound = gate(3, dec!(0), dec!(2.002), base_fee(dec!(0.001)));
    at_bound.submit(&order("s1", Action::Sell, dec!(100), dec!(2)), 0);
    let fee_at = fill(
        "e1",
        Action::Sell,
        (dec!(100), dec!(2)),
        Some(("BTC", dec!(0.002))),
        dec!(2),
    );
    assert!(
        at_bound
            .on_fill(&oid("s1"), &fee_at)
            .unwrap()
            .shortfalls
            .is_empty()
    );
    assert_eq!(balance(&at_bound, "BTC"), (dec!(0), dec!(0), dec!(0)));
    assert_eq!(at_bound.halted(), None);

    // One unit over the bound is still booked (mandatory): with no free
    // BTC it overdraws by 1e-8 and halts.
    let mut over = gate(3, dec!(0), dec!(2.002), base_fee(dec!(0.001)));
    over.submit(&order("s1", Action::Sell, dec!(100), dec!(2)), 0);
    let fee_over = fill(
        "e1",
        Action::Sell,
        (dec!(100), dec!(2)),
        Some(("BTC", dec!(0.00200001))),
        dec!(2),
    );
    let outcome = over.on_fill(&oid("s1"), &fee_over).unwrap();
    assert_eq!(
        outcome.shortfalls.get(&asset("BTC")),
        Some(&dec!(0.00000001))
    );
    assert_eq!(balance(&over, "BTC").0, dec!(-0.00000001));
    assert_eq!(over.halted(), Some(HaltReason::AccountingBreach));

    // With one free unit it is covered and nothing halts.
    let mut free = gate(3, dec!(0), dec!(2.00200001), base_fee(dec!(0.001)));
    free.submit(&order("s1", Action::Sell, dec!(100), dec!(2)), 0);
    assert!(
        free.on_fill(&oid("s1"), &fee_over)
            .unwrap()
            .shortfalls
            .is_empty()
    );
    assert_eq!(free.halted(), None);
}

#[test]
fn test_buy_base_fee_reserves_the_notional_only_and_nets_the_fee() {
    // Lot 0.001, r 0.001. With a fee in quote, buy 2 at 100 reserves
    // 200 * 1.001 = 200.2; with a fee in base, 200. A fill of 2 with a
    // 0.002 BTC fee: BTC 2 - 0.002 = 1.998 with none held before.
    let quote = gate(
        3,
        dec!(1000),
        dec!(0),
        InstrumentLimits {
            max_fee_rate: dec!(0.001),
            ..InstrumentLimits::default()
        },
    );
    let mut quote = quote;
    quote.submit(&order("b1", Action::Buy, dec!(100), dec!(2)), 0);
    assert_eq!(balance(&quote, "USDC").1, dec!(200.2));

    let mut gate = gate(3, dec!(1000), dec!(0), base_fee(dec!(0.001)));
    gate.submit(&order("b1", Action::Buy, dec!(100), dec!(2)), 0);
    assert_eq!(balance(&gate, "USDC"), (dec!(1000), dec!(200), dec!(800)));
    assert_eq!(balance(&gate, "BTC"), (dec!(0), dec!(0), dec!(0)));
    let outcome = gate
        .on_fill(
            &oid("b1"),
            &fill(
                "e1",
                Action::Buy,
                (dec!(100), dec!(2)),
                Some(("BTC", dec!(0.002))),
                dec!(2),
            ),
        )
        .unwrap();
    assert!(outcome.shortfalls.is_empty());
    assert_eq!(balance(&gate, "BTC"), (dec!(1.998), dec!(0), dec!(1.998)));
    assert_eq!(balance(&gate, "USDC"), (dec!(800), dec!(0), dec!(800)));
    gate.ledger().reconcile().unwrap();
}

#[test]
fn test_buy_base_fee_never_touches_base_held_by_another_order() {
    // 0.999999 BTC all held by a sell of 0.999 (999 lots * 0.001001): free
    // BTC is 0. A buy of 1 at 100 with a 0.001 BTC fee nets +0.999 BTC in
    // one booking: no shortfall, and the sell's hold is unchanged.
    let mut gate = gate(3, dec!(1000), dec!(0.999999), base_fee(dec!(0.001)));
    gate.submit(&order("s1", Action::Sell, dec!(200), dec!(0.999)), 0);
    assert_eq!(
        balance(&gate, "BTC"),
        (dec!(0.999999), dec!(0.999999), dec!(0))
    );
    gate.submit(&order("b1", Action::Buy, dec!(100), dec!(1)), 0);
    let outcome = gate
        .on_fill(
            &oid("b1"),
            &fill(
                "e1",
                Action::Buy,
                (dec!(100), dec!(1)),
                Some(("BTC", dec!(0.001))),
                dec!(1),
            ),
        )
        .unwrap();
    assert!(outcome.shortfalls.is_empty());
    assert_eq!(
        balance(&gate, "BTC"),
        (dec!(1.998999), dec!(0.999999), dec!(0.999))
    );
    assert_eq!(gate.halted(), None);
}

#[test]
fn test_buy_base_fee_equal_to_a_one_lot_fill_nets_to_zero() {
    // Lot 1e-8, r 0.5: a one lot fill may be charged round_up(0.5e-8) = 1e-8,
    // the whole lot. With no BTC the buy nets to 0, no shortfall. A fee
    // above the quantity (2e-8) is still booked and overdraws.
    let mut gate = gate(8, dec!(1), dec!(0), base_fee(dec!(0.5)));
    gate.submit(&order("b1", Action::Buy, dec!(100), dec!(0.00000002)), 0);
    let at = fill(
        "e1",
        Action::Buy,
        (dec!(100), dec!(0.00000001)),
        Some(("BTC", dec!(0.00000001))),
        dec!(0.00000001),
    );
    assert!(gate.on_fill(&oid("b1"), &at).unwrap().shortfalls.is_empty());
    assert_eq!(balance(&gate, "BTC").0, dec!(0));
    let over = fill(
        "e2",
        Action::Buy,
        (dec!(100), dec!(0.00000001)),
        Some(("BTC", dec!(0.00000002))),
        dec!(0.00000002),
    );
    let outcome = gate.on_fill(&oid("b1"), &over).unwrap();
    assert_eq!(
        outcome.shortfalls.get(&asset("BTC")),
        Some(&dec!(0.00000001))
    );
    assert_eq!(gate.halted(), Some(HaltReason::AccountingBreach));
}

#[test]
fn test_fee_in_quote_on_a_base_fee_buy_comes_from_free_quote() {
    // Fees declared in base, but the venue charges 0.1 USDC: the buy's
    // reservation (the notional, 100) does not cover it. With 100.1 USDC it
    // comes from the free 0.1; with exactly 100 it overdraws and halts.
    for (usdc, short) in [(dec!(100.1), None), (dec!(100), Some(dec!(0.1)))] {
        let mut gate = gate(3, usdc, dec!(0), base_fee(dec!(0.001)));
        gate.submit(&order("b1", Action::Buy, dec!(100), dec!(1)), 0);
        let outcome = gate
            .on_fill(
                &oid("b1"),
                &fill(
                    "e1",
                    Action::Buy,
                    (dec!(100), dec!(1)),
                    Some(("USDC", dec!(0.1))),
                    dec!(1),
                ),
            )
            .unwrap();
        assert_eq!(outcome.shortfalls.get(&asset("USDC")).copied(), short);
        assert_eq!(gate.halted().is_some(), short.is_some());
    }
}

#[test]
fn test_base_fee_rate_must_be_below_one_and_zero_is_accepted() {
    let mut gate = gate(3, dec!(0), dec!(3), base_fee(dec!(0)));
    // Rate 0: a sell reserves its quantity.
    gate.submit(&order("s1", Action::Sell, dec!(100), dec!(1)), 0);
    assert_eq!(balance(&gate, "BTC").1, dec!(1));
    for rate in [dec!(1), dec!(1.5)] {
        assert!(matches!(
            gate.set_limits(&pair(), base_fee(rate)),
            Err(MarginError::InvalidInput { .. })
        ));
        // Refused before anything changes.
        assert_eq!(gate.limits(&pair()), base_fee(dec!(0)));
    }
    // In quote a rate of 1 or more is still accepted, as before.
    assert!(
        gate.set_limits(
            &pair(),
            InstrumentLimits {
                max_fee_rate: dec!(1),
                ..InstrumentLimits::default()
            }
        )
        .is_ok()
    );
    assert_eq!(InstrumentLimits::default().fee_asset, FeeAsset::Quote);
}

#[test]
fn test_base_fee_terms_are_fixed_at_admission() {
    // Admitted with fees in base at 0.001 (per lot 0.001001); the limits
    // then move to quote at 0: the live order keeps its terms, and a fill
    // of 1 leaves 1.001 reserved for the rest.
    let mut gate = gate(3, dec!(0), dec!(3), base_fee(dec!(0.001)));
    gate.submit(&order("s1", Action::Sell, dec!(100), dec!(2)), 0);
    gate.set_limits(&pair(), InstrumentLimits::default())
        .unwrap();
    gate.on_fill(
        &oid("s1"),
        &fill(
            "e1",
            Action::Sell,
            (dec!(100), dec!(1)),
            Some(("BTC", dec!(0.001))),
            dec!(1),
        ),
    )
    .unwrap();
    assert_eq!(
        balance(&gate, "BTC"),
        (dec!(1.999), dec!(1.001), dec!(0.998))
    );
    // A new sell uses the new limits: its quantity only.
    gate.submit(&order("s2", Action::Sell, dec!(100), dec!(0.5)), 0);
    assert_eq!(balance(&gate, "BTC").1, dec!(1.501));
}

#[test]
fn test_sell_base_fee_rebate_adds_to_the_base_balance() {
    // A negative fee (a rebate) of 0.001 BTC on a fill of 1: BTC
    // 3 - 1 + 0.001 = 2.001, the reservation 1.001 for the rest.
    let mut gate = gate(3, dec!(0), dec!(3), base_fee(dec!(0.001)));
    gate.submit(&order("s1", Action::Sell, dec!(100), dec!(2)), 0);
    let rebate = fill(
        "e1",
        Action::Sell,
        (dec!(100), dec!(1)),
        Some(("BTC", dec!(-0.001))),
        dec!(1),
    );
    assert!(
        gate.on_fill(&oid("s1"), &rebate)
            .unwrap()
            .shortfalls
            .is_empty()
    );
    assert_eq!(balance(&gate, "BTC"), (dec!(2.001), dec!(1.001), dec!(1)));
    gate.ledger().reconcile().unwrap();
}
