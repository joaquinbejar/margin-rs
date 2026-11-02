//! Pretrade risk gate fixtures (M1-05).
//!
//! Expected values are written out by hand from the rules in
//! `docs/risk-gate.md`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::ledger::{Booking, Execution, Ledger, Side};
use margin_rs::risk::{
    ExecutionAnomaly, FillReport, HaltReason, InstrumentLimits, OrderRequest, OrderState,
    RiskDecision, RiskGate, RiskReason,
};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, OrderId, ReservationId, SpotInstrument,
    TransferId,
};
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

fn asset(code: &str) -> Asset {
    Asset::new(code).expect("valid asset")
}

fn oid(id: &str) -> OrderId {
    OrderId::new(id).expect("valid order id")
}

fn pair() -> InstrumentId {
    InstrumentId::new("BTC-USDC").expect("valid instrument id")
}

/// USDC at 6 decimals, BTC at 8, BTC-USDC with price scale 2 and quantity
/// scale 8; `usdc` and `btc` deposited.
fn gate(usdc: Decimal, btc: Decimal, limits: InstrumentLimits) -> RiskGate {
    gate_with_lot_scale(8, usdc, btc, limits)
}

/// As [`gate`], with BTC-USDC quantity scale `lot_scale`.
fn gate_with_lot_scale(
    lot_scale: u32,
    usdc: Decimal,
    btc: Decimal,
    limits: InstrumentLimits,
) -> RiskGate {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).expect("register");
    ledger.register_asset(asset("BTC"), 8).expect("register");
    ledger
        .register_instrument(
            SpotInstrument::new(pair(), asset("BTC"), asset("USDC"), 2, lot_scale).expect("spot"),
        )
        .expect("register");
    let mut gate = RiskGate::new(ledger);
    if usdc > Decimal::ZERO {
        gate.deposit(TransferId::new("d-usdc").expect("id"), asset("USDC"), usdc)
            .expect("deposit");
    }
    if btc > Decimal::ZERO {
        gate.deposit(TransferId::new("d-btc").expect("id"), asset("BTC"), btc)
            .expect("deposit");
    }
    gate.set_limits(&pair(), limits).expect("limits");
    gate
}

fn buy(id: &str, price: Decimal, quantity: Decimal) -> OrderRequest {
    OrderRequest::new(oid(id), pair(), Side::Buy, price, quantity)
}

fn sell(id: &str, price: Decimal, quantity: Decimal) -> OrderRequest {
    OrderRequest::new(oid(id), pair(), Side::Sell, price, quantity)
}

fn fill(id: &str, side: Side, price: Decimal, quantity: Decimal) -> Execution {
    Execution::new(
        ExecutionId::new(id).expect("id"),
        pair(),
        side,
        price,
        quantity,
    )
}

/// A fill report with the order's cumulative quantity after the fill.
fn report(execution: Execution, cumulative: Decimal) -> FillReport {
    FillReport::new(execution, cumulative)
}

/// (total, reserved, available) of an asset.
fn balance(gate: &RiskGate, code: &str) -> (Decimal, Decimal, Decimal) {
    let b = gate.ledger().balance(&asset(code)).expect("registered");
    (b.total, b.reserved, b.available)
}

/// Acceptance 1: two orders cannot spend the same available balance.
///
/// 1000 USDC. Buy 6 at 100 reserves 600. A second buy of 6 needs 600 with
/// 400 available: rejected, or reduced to 4 when reducible. Then nothing is
/// left, and even one lot is rejected.
#[test]
fn two_orders_cannot_spend_the_same_balance() {
    let mut gate = gate(dec!(1000), dec!(0), InstrumentLimits::default());
    assert_eq!(
        gate.submit(&buy("o1", dec!(100), dec!(6)), 0),
        RiskDecision::Accepted { quantity: dec!(6) }
    );
    assert_eq!(balance(&gate, "USDC"), (dec!(1000), dec!(600), dec!(400)));

    let short_of_cash = RiskReason::InsufficientBalance {
        asset: asset("USDC"),
        required: dec!(600),
        available: dec!(400),
    };
    assert_eq!(
        gate.submit(&buy("o2", dec!(100), dec!(6)), 0),
        RiskDecision::Rejected {
            reasons: vec![short_of_cash.clone()]
        }
    );
    assert_eq!(balance(&gate, "USDC"), (dec!(1000), dec!(600), dec!(400)));
    assert_eq!(
        gate.submit(&buy("o3", dec!(100), dec!(6)).reducible(), 0),
        RiskDecision::Reduced {
            requested: dec!(6),
            quantity: dec!(4),
            reasons: vec![short_of_cash]
        }
    );
    assert_eq!(balance(&gate, "USDC"), (dec!(1000), dec!(1000), dec!(0)));
    assert_eq!(
        gate.submit(&buy("o4", dec!(100), dec!(0.00000001)).reducible(), 0),
        RiskDecision::Rejected {
            reasons: vec![
                RiskReason::InsufficientBalance {
                    asset: asset("USDC"),
                    required: dec!(0.000001),
                    available: dec!(0),
                },
                RiskReason::BelowMinimumQuantity {
                    minimum: dec!(0.00000001),
                    allowed: dec!(0),
                },
            ]
        }
    );
    gate.ledger().reconcile().expect("reconciles");
}

/// Acceptance 2a: a requested cancel keeps the reservation until it takes
/// effect, and a fill racing the cancel is booked and adjusts it.
///
/// Buy 5 at 100 reserves 500. Cancel requested: still 500. A fill of 2 at
/// 99 arrives: cash 1000 - 198 = 802, reservation 3 * 100 = 300. The cancel
/// is confirmed: 300 released, 802 available.
///
/// M105-2: a fill of 1 at 99 reported after the confirmation (cumulative
/// 3) already happened at the venue: it is booked against the free
/// balance (802 - 99 = 703). Its duplicate changes nothing; an
/// inconsistent cumulative quantity or more than the canceled 2 is
/// rejected.
#[test]
fn requested_cancel_keeps_the_reservation_until_effective() {
    let mut gate = gate(dec!(1000), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(5)), 0);
    gate.on_venue_accepted(&oid("o1")).expect("ack");
    assert_eq!(gate.request_cancel(&oid("o1")), Ok(true));
    assert_eq!(gate.request_cancel(&oid("o1")), Ok(false));
    assert_eq!(balance(&gate, "USDC"), (dec!(1000), dec!(500), dec!(500)));
    assert_eq!(
        gate.order(&oid("o1")).map(|o| o.state),
        Some(OrderState::CancelPending)
    );

    let booking = gate
        .on_fill(
            &oid("o1"),
            &report(fill("e1", Side::Buy, dec!(99), dec!(2)), dec!(2)),
        )
        .expect("fill");
    assert!(booking.is_clean());
    assert_eq!(balance(&gate, "USDC"), (dec!(802), dec!(300), dec!(502)));

    assert_eq!(gate.on_cancel_confirmed(&oid("o1")), Ok(dec!(300)));
    assert_eq!(balance(&gate, "USDC"), (dec!(802), dec!(0), dec!(802)));
    assert_eq!(
        gate.order(&oid("o1")).map(|o| o.state),
        Some(OrderState::Canceled)
    );
    // A repeated confirmation changes nothing.
    assert_eq!(gate.on_cancel_confirmed(&oid("o1")), Ok(dec!(0)));

    let late = report(fill("e2", Side::Buy, dec!(99), dec!(1)), dec!(3));
    let booked = gate.on_fill(&oid("o1"), &late).expect("late fill");
    assert!(booked.is_clean());
    assert_eq!(balance(&gate, "USDC"), (dec!(703), dec!(0), dec!(703)));
    assert_eq!(
        gate.on_fill(&oid("o1"), &late).map(|o| o.booking),
        Ok(Booking::Duplicate(booked.booking.cashflow().clone()))
    );
    assert_eq!(gate.order(&oid("o1")).map(|o| o.filled), Some(dec!(3)));
    let mismatch = |reason| MarginError::ExecutionMismatch {
        id: "o1".into(),
        reason,
    };
    assert_eq!(
        gate.on_fill(
            &oid("o1"),
            &report(fill("e3", Side::Buy, dec!(99), dec!(1)), dec!(5))
        ),
        Err(mismatch(
            "cumulative quantity is not the filled quantity plus the fill"
        ))
    );
    assert_eq!(balance(&gate, "USDC"), (dec!(703), dec!(0), dec!(703)));
    assert_eq!(gate.halted(), None);
    gate.ledger().reconcile().expect("reconciles");
}

/// A late fill the free balance cannot cover is still booked: the ledger
/// shows the debt, and the gate trips an accounting breach.
///
/// 100 USDC, buy 1 at 100, cancel confirmed, 50 withdrawn. The late fill of
/// 1 at 100 leaves the balance at -50 (shortfall 50).
#[test]
fn uncovered_late_fill_is_booked_as_a_debt() {
    let mut gate = gate(dec!(100), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(1)), 0);
    gate.on_cancel_confirmed(&oid("o1")).expect("cancel");
    gate.withdraw(TransferId::new("w1").expect("id"), asset("USDC"), dec!(50))
        .expect("withdraw");
    let outcome = gate
        .on_fill(
            &oid("o1"),
            &report(fill("e1", Side::Buy, dec!(100), dec!(1)), dec!(1)),
        )
        .expect("a real fill is never lost");
    assert!(outcome.booking.is_new());
    assert_eq!(
        outcome.shortfalls,
        BTreeMap::from([(asset("USDC"), dec!(50))])
    );
    assert_eq!(balance(&gate, "USDC"), (dec!(-50), dec!(0), dec!(-50)));
    assert_eq!(balance(&gate, "BTC"), (dec!(1), dec!(0), dec!(1)));
    assert_eq!(gate.halted(), Some(HaltReason::AccountingBreach));
    let reconciliation = gate.ledger().reconcile().expect("sums agree");
    assert_eq!(
        reconciliation.overdrafts,
        BTreeMap::from([(asset("USDC"), dec!(50))])
    );
    // Nothing more can be spent while overdrawn.
    assert!(gate.submit(&buy("o2", dec!(1), dec!(1)), 0).is_rejected());
}

/// Acceptance 2b: a partial fill adjusts the reservation exactly once.
///
/// Fee reserve 0.1 %. Buy 4 at 100 reserves 400 * 1.001 = 400.4. Fill 1 at
/// 100 with a 0.1 fee: cash 1000 - 100.1 = 899.9; reservation
/// 300 * 1.001 = 300.3. The same fill message again changes nothing. The
/// last 3 fill with a 0.3 fee: cash 599.6, reservation gone.
#[test]
fn partial_fill_adjusts_the_reservation_exactly_once() {
    let limits = InstrumentLimits {
        max_fee_rate: dec!(0.001),
        ..InstrumentLimits::default()
    };
    // Lot 0.01: per lot 1 USDC plus a 0.001 fee reserve.
    let mut gate = gate_with_lot_scale(2, dec!(1000), dec!(0), limits);
    gate.submit(&buy("o1", dec!(100), dec!(4)), 0);
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(1000), dec!(400.4), dec!(599.6))
    );

    let e1 = report(
        fill("e1", Side::Buy, dec!(100), dec!(1)).with_fee(asset("USDC"), dec!(0.1)),
        dec!(1),
    );
    let first = gate.on_fill(&oid("o1"), &e1).expect("fill");
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(899.9), dec!(300.3), dec!(599.6))
    );
    for _ in 0..3 {
        assert_eq!(
            gate.on_fill(&oid("o1"), &e1).map(|o| o.booking),
            Ok(Booking::Duplicate(first.booking.cashflow().clone()))
        );
    }
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(899.9), dec!(300.3), dec!(599.6))
    );
    assert_eq!(gate.order(&oid("o1")).map(|o| o.filled), Some(dec!(1)));
    let conflicting = report(
        fill("e1", Side::Buy, dec!(100), dec!(2)).with_fee(asset("USDC"), dec!(0.1)),
        dec!(2),
    );
    assert_eq!(
        gate.on_fill(&oid("o1"), &conflicting),
        Err(MarginError::ConflictingEntry {
            kind: "execution",
            id: "e1".into(),
        })
    );

    let e2 = report(
        fill("e2", Side::Buy, dec!(100), dec!(3)).with_fee(asset("USDC"), dec!(0.3)),
        dec!(4),
    );
    gate.on_fill(&oid("o1"), &e2).expect("fill");
    assert_eq!(balance(&gate, "USDC"), (dec!(599.6), dec!(0), dec!(599.6)));
    assert_eq!(
        gate.order(&oid("o1")).map(|o| o.state),
        Some(OrderState::Filled)
    );
    assert_eq!(gate.ledger().snapshot().reservations.len(), 0);
    gate.ledger().reconcile().expect("reconciles");
}

/// M105-1: the buy reservation covers any fragmentation of the order.
///
/// Lot 1e-8 at 150.01 costs 1.5001e-6, booked as 2e-6 on the 6 digit
/// grid, so each lot reserves 2e-6. With 5e-6 USDC a buy of 3 lots
/// (6e-6) does not fit; with 6e-6 it does, and three one lot fills (2e-6
/// each) are all covered with nothing left free.
#[test]
fn reservation_covers_single_lot_fills() {
    let mut gate_short = gate(dec!(0.000005), dec!(0), InstrumentLimits::default());
    assert_eq!(
        gate_short.submit(&buy("o0", dec!(150.01), dec!(0.00000003)), 0),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::InsufficientBalance {
                asset: asset("USDC"),
                required: dec!(0.000006),
                available: dec!(0.000005),
            }]
        }
    );

    let mut gate = gate(dec!(0.000006), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(150.01), dec!(0.00000003)), 0);
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(0.000006), dec!(0.000006), dec!(0))
    );
    for i in 1..=3_i64 {
        gate.on_fill(
            &oid("o1"),
            &report(
                fill(&format!("e{i}"), Side::Buy, dec!(150.01), dec!(0.00000001)),
                Decimal::new(i, 8),
            ),
        )
        .expect("covered");
    }
    assert_eq!(balance(&gate, "USDC"), (dec!(0), dec!(0), dec!(0)));
    assert_eq!(gate.halted(), None);
    gate.ledger().reconcile().expect("reconciles");
}

/// M105-1: mixed fill sizes with no free balance.
///
/// 1e-5 USDC, buy 5 lots at 150.01: reserved 5 * 2e-6 = 1e-5, nothing
/// free. Fills of 2, 1 and 2 lots debit 3e-6, 2e-6 and 3e-6 (the
/// notionals 3.0002e-6, 1.5001e-6 and 3.0002e-6 rounded half even); after
/// each the reservation is what the rest needs (6e-6, 4e-6, 0) and the
/// unused 2e-6 ends up free.
#[test]
fn reservation_covers_mixed_fills_without_free_balance() {
    let mut gate = gate(dec!(0.00001), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(150.01), dec!(0.00000005)), 0);
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(0.00001), dec!(0.00001), dec!(0))
    );
    let steps = [
        (
            dec!(0.00000002),
            dec!(0.00000002),
            (dec!(0.000007), dec!(0.000006), dec!(0.000001)),
        ),
        (
            dec!(0.00000001),
            dec!(0.00000003),
            (dec!(0.000005), dec!(0.000004), dec!(0.000001)),
        ),
        (
            dec!(0.00000002),
            dec!(0.00000005),
            (dec!(0.000002), dec!(0), dec!(0.000002)),
        ),
    ];
    for (i, (quantity, cumulative, expected)) in steps.into_iter().enumerate() {
        gate.on_fill(
            &oid("o1"),
            &report(
                fill(&format!("e{i}"), Side::Buy, dec!(150.01), quantity),
                cumulative,
            ),
        )
        .expect("covered");
        assert_eq!(balance(&gate, "USDC"), expected);
    }
    assert_eq!(gate.halted(), None);
    gate.ledger().reconcile().expect("reconciles");
}

/// With a fee bound, each lot also reserves the fee rounded up: at 150.01
/// and 0.1 %, 2e-6 + round_up(2e-9) = 3e-6 per lot.
#[test]
fn reservation_includes_the_rounded_up_fee_per_lot() {
    let limits = InstrumentLimits {
        max_fee_rate: dec!(0.001),
        ..InstrumentLimits::default()
    };
    let mut gate = gate(dec!(1), dec!(0), limits);
    gate.submit(&buy("o1", dec!(150.01), dec!(0.00000002)), 0);
    assert_eq!(balance(&gate, "USDC").1, dec!(0.000006));
    let terms = gate
        .order(&oid("o1"))
        .and_then(|o| o.terms.clone())
        .expect("terms");
    assert_eq!(terms.per_lot, dec!(0.000003));
}

/// M105-3: the reservation terms are fixed when an order is admitted.
///
/// Lot 0.01. 110 USDC, fee bound 10 %: a buy of 1 at 100 reserves
/// 100 lots times 1.1, that is 110. Lowering the bound to 0 does not free the fee buffer:
/// after a fill of 0.5 with a 5 fee (debit 55) the reservation is 55 for
/// the rest and nothing is free, so a withdrawal fails and the second
/// identical fill is covered. Raising the bound later applies only to new
/// orders.
#[test]
fn reservation_terms_are_fixed_at_admission() {
    let limits = InstrumentLimits {
        max_fee_rate: dec!(0.1),
        ..InstrumentLimits::default()
    };
    let mut gate = gate_with_lot_scale(2, dec!(110), dec!(0), limits);
    gate.submit(&buy("o1", dec!(100), dec!(1)), 0);
    assert_eq!(balance(&gate, "USDC"), (dec!(110), dec!(110), dec!(0)));
    gate.set_limits(&pair(), InstrumentLimits::default())
        .expect("lower the bound");

    let with_fee = |id: &str, cumulative| {
        report(
            fill(id, Side::Buy, dec!(100), dec!(0.5)).with_fee(asset("USDC"), dec!(5)),
            cumulative,
        )
    };
    gate.on_fill(&oid("o1"), &with_fee("e1", dec!(0.5)))
        .expect("fill");
    assert_eq!(balance(&gate, "USDC"), (dec!(55), dec!(55), dec!(0)));
    assert_eq!(
        gate.withdraw(TransferId::new("w1").expect("id"), asset("USDC"), dec!(5)),
        Err(MarginError::InsufficientBalance {
            asset: asset("USDC"),
            required: dec!(5),
            available: dec!(0),
        })
    );
    gate.on_fill(&oid("o1"), &with_fee("e2", dec!(1)))
        .expect("second fill covered");
    assert_eq!(balance(&gate, "USDC"), (dec!(0), dec!(0), dec!(0)));
    assert_eq!(gate.halted(), None);

    // Raising the bound affects new orders only.
    let mut gate = gate_with_lot_scale(2, dec!(1000), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("old", dec!(100), dec!(1)), 0);
    gate.set_limits(
        &pair(),
        InstrumentLimits {
            max_fee_rate: dec!(0.1),
            ..InstrumentLimits::default()
        },
    )
    .expect("raise the bound");
    gate.submit(&buy("new", dec!(100), dec!(1)), 0);
    assert_eq!(balance(&gate, "USDC"), (dec!(1000), dec!(210), dec!(790)));
    gate.on_fill(
        &oid("old"),
        &report(fill("e1", Side::Buy, dec!(100), dec!(1)), dec!(1)),
    )
    .expect("fill");
    assert_eq!(balance(&gate, "USDC"), (dec!(900), dec!(110), dec!(790)));
    gate.ledger().reconcile().expect("reconciles");
}

/// A mark timestamped after the decision time is look ahead.
#[test]
fn future_marks_are_rejected_as_look_ahead() {
    let limits = InstrumentLimits {
        max_data_age_ms: Some(1000),
        ..InstrumentLimits::default()
    };
    let mut gate = gate(dec!(1000), dec!(0), limits);
    gate.on_market_data(&pair(), dec!(100), 2000).expect("mark");
    assert_eq!(
        gate.submit(&buy("o1", dec!(100), dec!(1)), 1500),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::LookAheadMarketData {
                mark_ms: 2000,
                now_ms: 1500,
            }]
        }
    );
    assert!(
        !gate
            .submit(&buy("o2", dec!(100), dec!(1)), 2000)
            .is_rejected()
    );
}

/// Limits apply to the joint exposure of the position and every live
/// order on the same side.
///
/// Max position 10, max order 8. A buy of 9 is cut to 8. With a buy of 6
/// open, another buy of 6 is reduced to 4 (6 + 6 > 10). After the first buy
/// fills (position 6, 4 still open), any further buy is rejected. A sell of
/// 20 is cut to 8 by the order limit (the position limit would allow a
/// short of 10, that is 16), and to 6 because only 6 BTC are held.
#[test]
fn limits_apply_to_joint_exposure() {
    let limits = InstrumentLimits {
        max_position: Some(dec!(10)),
        max_order_quantity: Some(dec!(8)),
        ..InstrumentLimits::default()
    };
    let mut gate = gate(dec!(100000), dec!(0), limits);
    assert_eq!(
        gate.submit(&buy("b0", dec!(100), dec!(9)).reducible(), 0),
        RiskDecision::Reduced {
            requested: dec!(9),
            quantity: dec!(8),
            reasons: vec![RiskReason::MaxOrderQuantity { limit: dec!(8) }]
        }
    );
    gate.on_cancel_confirmed(&oid("b0")).expect("cancel");

    assert!(!gate.submit(&buy("b1", dec!(100), dec!(6)), 0).is_rejected());
    assert_eq!(
        gate.submit(&buy("b2", dec!(100), dec!(6)).reducible(), 0),
        RiskDecision::Reduced {
            requested: dec!(6),
            quantity: dec!(4),
            reasons: vec![RiskReason::MaxPosition {
                limit: dec!(10),
                headroom: dec!(4),
            }]
        }
    );
    gate.on_fill(
        &oid("b1"),
        &report(fill("f1", Side::Buy, dec!(100), dec!(6)), dec!(6)),
    )
    .expect("fill");
    assert_eq!(
        gate.submit(&buy("b3", dec!(100), dec!(1)), 0),
        RiskDecision::Rejected {
            reasons: vec![
                RiskReason::MaxPosition {
                    limit: dec!(10),
                    headroom: dec!(0),
                },
                RiskReason::BelowMinimumQuantity {
                    minimum: dec!(0.00000001),
                    allowed: dec!(0),
                },
            ]
        }
    );
    assert_eq!(
        gate.submit(&sell("s1", dec!(100), dec!(20)).reducible(), 0),
        RiskDecision::Reduced {
            requested: dec!(20),
            quantity: dec!(6),
            reasons: vec![
                RiskReason::MaxOrderQuantity { limit: dec!(8) },
                RiskReason::InsufficientBalance {
                    asset: asset("BTC"),
                    required: dec!(8),
                    available: dec!(6),
                },
            ]
        }
    );
    assert_eq!(balance(&gate, "BTC"), (dec!(6), dec!(6), dec!(0)));
    gate.ledger().reconcile().expect("reconciles");
}

/// The notional limit bounds the worst case position valued at the order
/// price: 1000 USDC at 100 is 10 BTC.
#[test]
fn position_notional_limit() {
    let limits = InstrumentLimits {
        max_position_notional: Some(dec!(1000)),
        ..InstrumentLimits::default()
    };
    let mut gate = gate(dec!(100000), dec!(0), limits);
    gate.submit(&buy("b1", dec!(100), dec!(7)), 0);
    assert_eq!(
        gate.submit(&buy("b2", dec!(100), dec!(5)).reducible(), 0),
        RiskDecision::Reduced {
            requested: dec!(5),
            quantity: dec!(3),
            reasons: vec![RiskReason::MaxPositionNotional {
                limit: dec!(1000),
                headroom: dec!(3),
            }]
        }
    );
}

/// Acceptance 3: invalid orders are rejected every time an agent proposes
/// them, and a repeated order id never reserves twice.
#[test]
fn repeated_invalid_proposals_are_always_rejected() {
    let limits = InstrumentLimits {
        max_order_quantity: Some(dec!(5)),
        ..InstrumentLimits::default()
    };
    let mut gate = gate(dec!(10000), dec!(0), limits);
    let before = gate.ledger().snapshot();
    for i in 0..5 {
        assert_eq!(
            gate.submit(&buy(&format!("agent-{i}"), dec!(100), dec!(50)), 0),
            RiskDecision::Rejected {
                reasons: vec![RiskReason::MaxOrderQuantity { limit: dec!(5) }]
            }
        );
    }
    assert_eq!(
        gate.submit(&buy("bad-price", dec!(0), dec!(1)), 0),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::InvalidOrder {
                error: MarginError::invalid_input("price", "must be positive"),
            }]
        }
    );
    assert_eq!(
        gate.submit(&buy("fine-tick", dec!(100.001), dec!(1)), 0),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::InvalidOrder {
                error: MarginError::PrecisionExceeded {
                    field: "price",
                    scale: 3,
                    max_scale: 2,
                },
            }]
        }
    );
    assert_eq!(gate.ledger().snapshot(), before);

    // The same id again, whether it was rejected or accepted.
    let duplicate = RiskDecision::Rejected {
        reasons: vec![RiskReason::DuplicateOrderId],
    };
    assert_eq!(
        gate.submit(&buy("agent-0", dec!(100), dec!(1)), 0),
        duplicate
    );
    assert!(!gate.submit(&buy("o1", dec!(100), dec!(1)), 0).is_rejected());
    assert_eq!(gate.submit(&buy("o1", dec!(100), dec!(1)), 0), duplicate);
    assert_eq!(balance(&gate, "USDC"), (dec!(10000), dec!(100), dec!(9900)));
}

/// Market data checks, the circuit breaker and quote withdrawal.
///
/// Max data age 1000 ms and a 5 % band around the mark. Without data,
/// orders are rejected. Mark 100 at t = 1000: at t = 1500 a buy at 100 and
/// one at 105 pass, one at 106 does not; at t = 2500 the data is stale.
/// Tripping on a data disconnect withdraws the two live orders and keeps
/// their reservations until each cancel is confirmed; a fill racing the
/// withdrawal is still booked. The buy at 100 reserves 100. At 105 a 1e-8
/// lot costs 1.05e-6, off the 6 digit grid: the per lot bound is
/// min(round_up(1.05e-6), 1.05e-6 + 0.5e-6) = 1.55e-6, so 155 is reserved.
#[test]
fn data_disconnect_trips_and_withdraws_quotes() {
    let limits = InstrumentLimits {
        max_data_age_ms: Some(1000),
        price_band: Some(dec!(0.05)),
        ..InstrumentLimits::default()
    };
    let mut gate = gate(dec!(10000), dec!(0), limits);
    assert_eq!(
        gate.submit(&buy("o0", dec!(100), dec!(1)), 0),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::NoMarketData]
        }
    );
    gate.on_market_data(&pair(), dec!(100), 1000).expect("mark");
    assert!(
        !gate
            .submit(&buy("o1", dec!(100), dec!(1)), 1500)
            .is_rejected()
    );
    assert!(
        !gate
            .submit(&buy("o3", dec!(105), dec!(1)), 1500)
            .is_rejected()
    );
    assert_eq!(
        gate.submit(&buy("o2", dec!(106), dec!(1)), 1500),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::PriceOutsideBand {
                mark: dec!(100),
                band: dec!(0.05),
            }]
        }
    );
    assert_eq!(
        gate.submit(&buy("o4", dec!(100), dec!(1)), 2500),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::StaleMarketData {
                age_ms: 1500,
                max_age_ms: 1000,
            }]
        }
    );

    assert_eq!(
        gate.trip(HaltReason::DataDisconnect),
        vec![oid("o1"), oid("o3")]
    );
    assert_eq!(gate.trip(HaltReason::Manual), Vec::<OrderId>::new());
    assert_eq!(gate.halted(), Some(HaltReason::DataDisconnect));
    assert_eq!(balance(&gate, "USDC"), (dec!(10000), dec!(255), dec!(9745)));
    gate.on_market_data(&pair(), dec!(100), 2600).expect("mark");
    assert_eq!(
        gate.submit(&buy("o5", dec!(100), dec!(1)), 2600),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::Halted {
                reason: HaltReason::DataDisconnect,
            }]
        }
    );

    gate.on_fill(
        &oid("o1"),
        &report(fill("e1", Side::Buy, dec!(100), dec!(1)), dec!(1)),
    )
    .expect("fill races the cancel");
    assert_eq!(gate.on_cancel_confirmed(&oid("o3")), Ok(dec!(155)));
    assert_eq!(gate.on_cancel_confirmed(&oid("o1")), Ok(dec!(0)));
    assert_eq!(balance(&gate, "USDC"), (dec!(9900), dec!(0), dec!(9900)));

    gate.resume();
    assert!(
        !gate
            .submit(&buy("o6", dec!(100), dec!(1)), 2600)
            .is_rejected()
    );
    gate.ledger().reconcile().expect("reconciles");
}

/// A venue reject releases the reservation; live orders count against the
/// open order limit.
#[test]
fn venue_reject_and_open_order_limit() {
    let limits = InstrumentLimits {
        max_open_orders: Some(2),
        ..InstrumentLimits::default()
    };
    let mut gate = gate(dec!(10000), dec!(0), limits);
    gate.submit(&buy("o1", dec!(100), dec!(1)), 0);
    gate.submit(&buy("o2", dec!(100), dec!(2)), 0);
    assert_eq!(
        gate.submit(&buy("o3", dec!(100), dec!(1)), 0),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::MaxOpenOrders { limit: 2 }]
        }
    );
    assert_eq!(gate.on_venue_rejected(&oid("o1")), Ok(dec!(100)));
    assert_eq!(gate.on_venue_rejected(&oid("o1")), Ok(dec!(0)));
    assert!(!gate.submit(&buy("o4", dec!(100), dec!(1)), 0).is_rejected());

    gate.on_venue_accepted(&oid("o2")).expect("ack");
    gate.on_fill(
        &oid("o2"),
        &report(fill("e1", Side::Buy, dec!(100), dec!(1)), dec!(1)),
    )
    .expect("fill");
    assert_eq!(
        gate.on_venue_rejected(&oid("o2")),
        Err(MarginError::InvalidOrderState {
            id: "o2".into(),
            state: "open",
            operation: "reject",
        })
    );
    assert_eq!(
        gate.on_venue_accepted(&oid("o3")),
        Err(MarginError::InvalidOrderState {
            id: "o3".into(),
            state: "rejected",
            operation: "accept",
        })
    );
    gate.ledger().reconcile().expect("reconciles");
}

/// Reports that cannot be true are rejected and change nothing: another
/// instrument or side, a broken cumulative sequence, an unknown order, or
/// an order the gate rejected.
#[test]
fn impossible_reports_are_rejected() {
    let mut gate = gate(dec!(1000), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(2)), 0);
    gate.submit(&buy("big", dec!(100), dec!(100)), 0);
    let before = gate.ledger().snapshot();
    let mismatch = |reason| MarginError::ExecutionMismatch {
        id: "o1".into(),
        reason,
    };
    assert_eq!(
        gate.on_fill(
            &oid("o1"),
            &report(fill("e1", Side::Sell, dec!(100), dec!(1)), dec!(1))
        ),
        Err(mismatch("side differs from the order"))
    );
    let other = Execution::new(
        ExecutionId::new("e1").expect("id"),
        InstrumentId::new("ETH-USDC").expect("id"),
        Side::Buy,
        dec!(100),
        dec!(1),
    );
    assert_eq!(
        gate.on_fill(&oid("o1"), &report(other, dec!(1))),
        Err(mismatch("instrument differs from the order"))
    );
    assert_eq!(
        gate.on_fill(
            &oid("o1"),
            &report(fill("e1", Side::Buy, dec!(100), dec!(1)), dec!(2))
        ),
        Err(mismatch(
            "cumulative quantity is not the filled quantity plus the fill"
        ))
    );
    assert_eq!(
        gate.on_fill(
            &oid("nope"),
            &report(fill("e1", Side::Buy, dec!(100), dec!(1)), dec!(1))
        ),
        Err(MarginError::UnknownOrder { id: "nope".into() })
    );
    assert_eq!(
        gate.on_fill(
            &oid("big"),
            &report(fill("e1", Side::Buy, dec!(100), dec!(1)), dec!(1))
        ),
        Err(MarginError::InvalidOrderState {
            id: "big".into(),
            state: "rejected",
            operation: "fill",
        })
    );
    assert_eq!(gate.ledger().snapshot(), before);
    assert_eq!(gate.order(&oid("o1")).map(|o| o.filled), Some(dec!(0)));
    assert_eq!(gate.halted(), None);
}

/// An overfill is booked, only the reserved part is released, and the gate
/// halts with an execution anomaly.
///
/// 1000 USDC, buy 2 at 100 (reserved 200). The venue reports a fill of 3:
/// cash 1000 - 300 = 700, reservation gone, 3 BTC held.
#[test]
fn overfill_is_booked_and_halts() {
    let mut gate = gate(dec!(1000), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(2)), 0);
    let outcome = gate
        .on_fill(
            &oid("o1"),
            &report(fill("e1", Side::Buy, dec!(100), dec!(3)), dec!(3)),
        )
        .expect("booked");
    assert_eq!(
        outcome.anomalies,
        vec![ExecutionAnomaly::Overfill {
            remaining: dec!(2),
            quantity: dec!(3),
        }]
    );
    assert!(outcome.shortfalls.is_empty());
    assert_eq!(balance(&gate, "USDC"), (dec!(700), dec!(0), dec!(700)));
    assert_eq!(balance(&gate, "BTC"), (dec!(3), dec!(0), dec!(3)));
    assert_eq!(
        gate.halted(),
        Some(HaltReason::ExecutionAnomaly {
            order: oid("o1"),
            anomaly: ExecutionAnomaly::Overfill {
                remaining: dec!(2),
                quantity: dec!(3),
            },
        })
    );
    assert_eq!(
        gate.order(&oid("o1")).map(|o| (o.filled, o.state)),
        Some((dec!(3), OrderState::Filled))
    );
    assert!(gate.ledger().reconcile().expect("reconciles").is_clean());
}

/// A fill on an order that is already filled is booked against the free
/// balance and halts; so is a fill at a price worse than the limit.
#[test]
fn fill_after_filled_and_worse_price_are_booked_and_halt() {
    let mut gate = gate(dec!(1000), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(1)), 0);
    gate.on_fill(
        &oid("o1"),
        &report(fill("e1", Side::Buy, dec!(100), dec!(1)), dec!(1)),
    )
    .expect("fill");
    let after = gate
        .on_fill(
            &oid("o1"),
            &report(fill("e2", Side::Buy, dec!(100), dec!(1)), dec!(2)),
        )
        .expect("booked");
    assert_eq!(after.anomalies, vec![ExecutionAnomaly::FillAfterFilled]);
    assert_eq!(balance(&gate, "USDC"), (dec!(800), dec!(0), dec!(800)));
    assert!(matches!(
        gate.halted(),
        Some(HaltReason::ExecutionAnomaly {
            anomaly: ExecutionAnomaly::FillAfterFilled,
            ..
        })
    ));

    let mut gate = gate_fixture_worse_price();
    let worse = gate
        .on_fill(
            &oid("o1"),
            &report(fill("e1", Side::Buy, dec!(100.01), dec!(1)), dec!(1)),
        )
        .expect("booked");
    assert_eq!(
        worse.anomalies,
        vec![ExecutionAnomaly::WorsePrice {
            limit: dec!(100),
            price: dec!(100.01),
        }]
    );
    // Reserved 200 for 2 at 100; after the fill 100 stays reserved for the
    // last unit, and the 100.01 debit leaves 1000 - 100.01 = 899.99.
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(899.99), dec!(100), dec!(799.99))
    );
    assert!(matches!(
        gate.halted(),
        Some(HaltReason::ExecutionAnomaly {
            anomaly: ExecutionAnomaly::WorsePrice { .. },
            ..
        })
    ));
    gate.ledger().reconcile().expect("reconciles");
}

fn gate_fixture_worse_price() -> RiskGate {
    let mut gate = gate(dec!(1000), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(2)), 0);
    gate
}

/// A live fill the reservation and the free balance do not cover is booked
/// as a debt and trips an accounting breach.
///
/// 100 USDC, buy 1 at 100 with no fee reserve: 100 reserved, 0 free. The
/// venue charges a 1 USDC fee: the balance ends at -1.
#[test]
fn uncovered_fill_is_booked_as_a_debt() {
    let mut gate = gate(dec!(100), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(1)), 0);
    let e1 = fill("e1", Side::Buy, dec!(100), dec!(1)).with_fee(asset("USDC"), dec!(1));
    let outcome = gate
        .on_fill(&oid("o1"), &report(e1.clone(), dec!(1)))
        .expect("booked");
    assert_eq!(
        outcome.shortfalls,
        BTreeMap::from([(asset("USDC"), dec!(1))])
    );
    assert_eq!(balance(&gate, "USDC"), (dec!(-1), dec!(0), dec!(-1)));
    assert_eq!(gate.halted(), Some(HaltReason::AccountingBreach));
    assert_eq!(gate.order(&oid("o1")).map(|o| o.filled), Some(dec!(1)));
    // The duplicate changes nothing and reports no new shortfall.
    let again = gate.on_fill(&oid("o1"), &report(e1, dec!(1))).expect("dup");
    assert!(!again.booking.is_new());
    assert!(again.shortfalls.is_empty());
    assert_eq!(balance(&gate, "USDC"), (dec!(-1), dec!(0), dec!(-1)));
}

/// Property: with no free balance, no fragmentation of a buy at the limit
/// price can exceed its reservation, with or without a fee charged at the
/// bound and rounded up. Deterministic pseudo random cases: prices from
/// 0.01 to 999.99, 1 to 60 lots of 1e-8, random fill sizes.
#[test]
fn no_fragmentation_exceeds_the_reservation() {
    let mut state = 0x5DEE_CE66_D1CE_4E5B_u64;
    let mut next = move |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    for case in 0..400 {
        let price = Decimal::new(1 + i64::try_from(next(99_999)).unwrap(), 2);
        let lots = 1 + i64::try_from(next(60)).unwrap();
        let rate = if case % 2 == 0 { dec!(0) } else { dec!(0.001) };
        let limits = InstrumentLimits {
            max_fee_rate: rate,
            ..InstrumentLimits::default()
        };
        // Fund exactly the reservation, so nothing is free.
        let probe = gate(dec!(1000000), dec!(0), limits.clone());
        let mut probe = probe;
        probe.submit(&buy("p", price, Decimal::new(lots, 8)), 0);
        let needed = balance(&probe, "USDC").1;
        let mut gate = gate(needed, dec!(0), limits);
        assert!(
            !gate
                .submit(&buy("o", price, Decimal::new(lots, 8)), 0)
                .is_rejected()
        );
        assert_eq!(balance(&gate, "USDC").2, dec!(0));

        let mut done = 0_i64;
        let mut n = 0;
        while done < lots {
            let k = 1 + i64::try_from(next(u64::try_from(lots - done).unwrap())).unwrap();
            done += k;
            n += 1;
            let quantity = Decimal::new(k, 8);
            let mut execution = fill(&format!("e{n}"), Side::Buy, price, quantity);
            if !rate.is_zero() {
                // Fee at the bound on the booked notional, rounded up.
                let notional = (price * quantity)
                    .round_dp_with_strategy(6, rust_decimal::RoundingStrategy::MidpointNearestEven);
                let fee = (rate * notional)
                    .round_dp_with_strategy(6, rust_decimal::RoundingStrategy::AwayFromZero);
                execution = execution.with_fee(asset("USDC"), fee);
            }
            let outcome = gate
                .on_fill(&oid("o"), &report(execution, Decimal::new(done, 8)))
                .unwrap_or_else(|e| panic!("case {case}: price {price}, {lots} lots: {e}"));
            assert!(
                outcome.is_clean(),
                "case {case}: price {price}, {lots} lots: {outcome:?}"
            );
        }
        assert_eq!(gate.halted(), None);
        assert_eq!(balance(&gate, "USDC").1, dec!(0));
        gate.ledger().reconcile().expect("reconciles");
    }
}

/// When one fill is both an anomaly and an overdraft, both reasons are
/// recorded and the more severe one, the accounting breach, is reported.
///
/// 100 USDC, buy 1 at 100 (100 reserved). The venue fills 2: debit 200,
/// balance -100.
#[test]
fn anomaly_and_overdraft_are_both_recorded() {
    let mut gate = gate(dec!(100), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(1)), 0);
    let outcome = gate
        .on_fill(
            &oid("o1"),
            &report(fill("e1", Side::Buy, dec!(100), dec!(2)), dec!(2)),
        )
        .expect("booked");
    let overfill = ExecutionAnomaly::Overfill {
        remaining: dec!(1),
        quantity: dec!(2),
    };
    assert_eq!(outcome.anomalies, vec![overfill.clone()]);
    assert_eq!(
        outcome.shortfalls,
        BTreeMap::from([(asset("USDC"), dec!(100))])
    );
    assert_eq!(balance(&gate, "USDC"), (dec!(-100), dec!(0), dec!(-100)));
    assert_eq!(gate.halted(), Some(HaltReason::AccountingBreach));
    assert_eq!(
        gate.halt_reasons(),
        &[
            HaltReason::ExecutionAnomaly {
                order: oid("o1"),
                anomaly: overfill,
            },
            HaltReason::AccountingBreach,
        ]
    );
    assert_eq!(
        gate.order(&oid("o1"))
            .map(margin_rs::risk::OrderRecord::remaining),
        Some(dec!(0))
    );
    gate.resume();
    assert_eq!(gate.halted(), None);
    assert_eq!(gate.halt_reasons(), []);
}

/// Issue #16: an internal halt withdraws quotes like `trip()`.
///
/// 10000 USDC and 5 BTC. o1 buys 2 at 100, o2 buys 1 at 99 (accepted by the
/// venue), o3 sells 1 at 110. An overfill of o1 halts the gate: o1 is
/// filled, so the other live orders o2 and o3 are withdrawn and keep their
/// reservations until each cancel is confirmed.
#[test]
fn internal_halt_withdraws_the_other_live_orders() {
    let mut gate = gate(dec!(10000), dec!(5), InstrumentLimits::default());
    for request in [
        buy("o1", dec!(100), dec!(2)),
        buy("o2", dec!(99), dec!(1)),
        sell("o3", dec!(110), dec!(1)),
    ] {
        assert!(!gate.submit(&request, 0).is_rejected());
    }
    gate.on_venue_accepted(&oid("o2")).expect("accepted");
    let held = |gate: &RiskGate, id: &str| {
        gate.ledger()
            .reservation(&ReservationId::new(id).expect("id"))
            .map(|(asset, amount)| (asset.clone(), amount))
    };
    let (o2_held, o3_held) = (held(&gate, "o2"), held(&gate, "o3"));
    assert!(o2_held.is_some() && o3_held.is_some());
    let outcome = gate
        .on_fill(
            &oid("o1"),
            &report(fill("e1", Side::Buy, dec!(100), dec!(3)), dec!(3)),
        )
        .expect("booked");
    assert_ne!(
        outcome.anomalies,
        [] as [margin_rs::risk::ExecutionAnomaly; 0]
    );
    assert_eq!(outcome.withdraw, vec![oid("o2"), oid("o3")]);
    for id in ["o2", "o3"] {
        assert_eq!(
            gate.order(&oid(id)).map(|o| o.state),
            Some(OrderState::CancelPending)
        );
    }
    assert_eq!(
        gate.order(&oid("o1")).map(|o| o.state),
        Some(OrderState::Filled)
    );
    // Reservations stay until the cancels are confirmed.
    assert_eq!((held(&gate, "o2"), held(&gate, "o3")), (o2_held, o3_held));
    assert_eq!(balance(&gate, "BTC"), (dec!(8), dec!(1), dec!(7)));
    assert_eq!(
        gate.submit(&buy("o4", dec!(100), dec!(1)), 0),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::Halted {
                reason: gate.halted().expect("halted"),
            }]
        }
    );

    // A fill racing the cancel is booked normally and withdraws nothing.
    let raced = gate
        .on_fill(
            &oid("o2"),
            &report(fill("e2", Side::Buy, dec!(99), dec!(1)), dec!(1)),
        )
        .expect("booked");
    assert!(raced.is_clean());
    assert_eq!(raced.withdraw, [] as [margin_rs::OrderId; 0]);
    assert_eq!(balance(&gate, "USDC"), (dec!(9601), dec!(0), dec!(9601)));

    // A second internal halt while halted has nothing left to withdraw.
    let worse = gate
        .on_fill(
            &oid("o3"),
            &report(fill("e3", Side::Sell, dec!(109), dec!(1)), dec!(1)),
        )
        .expect("booked");
    assert_eq!(
        worse.anomalies,
        vec![ExecutionAnomaly::WorsePrice {
            limit: dec!(110),
            price: dec!(109),
        }]
    );
    assert_eq!(worse.withdraw, [] as [margin_rs::OrderId; 0]);
    assert_eq!(gate.trip(HaltReason::Manual), Vec::<OrderId>::new());
    assert_eq!(gate.on_cancel_confirmed(&oid("o2")), Ok(dec!(0)));
    assert_eq!(gate.on_cancel_confirmed(&oid("o3")), Ok(dec!(0)));
    assert!(gate.ledger().reconcile().expect("reconciles").is_clean());
}

/// A partly filled order whose fill halts the gate is itself withdrawn,
/// with the reservation of its remaining quantity kept.
#[test]
fn internal_halt_withdraws_the_partly_filled_order() {
    let mut gate = gate_fixture_worse_price();
    let outcome = gate
        .on_fill(
            &oid("o1"),
            &report(fill("e1", Side::Buy, dec!(100.01), dec!(1)), dec!(1)),
        )
        .expect("booked");
    assert_eq!(outcome.withdraw, vec![oid("o1")]);
    assert_eq!(
        gate.order(&oid("o1")).map(|o| o.state),
        Some(OrderState::CancelPending)
    );
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(899.99), dec!(100), dec!(799.99))
    );
    assert_eq!(gate.on_cancel_confirmed(&oid("o1")), Ok(dec!(100)));
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(899.99), dec!(0), dec!(899.99))
    );
}

/// An overdraft halts the gate and withdraws the live orders; the duplicate
/// of the same fill withdraws nothing.
///
/// 100 USDC and 1 BTC. o1 buys 1 at 100, o2 sells 1 at 200. A 1 USDC fee on
/// o1's fill overdraws USDC by 1: o2 is withdrawn.
#[test]
fn overdraft_halt_withdraws_and_duplicates_do_not() {
    let mut gate = gate(dec!(100), dec!(1), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(1)), 0);
    gate.submit(&sell("o2", dec!(200), dec!(1)), 0);
    let e1 = fill("e1", Side::Buy, dec!(100), dec!(1)).with_fee(asset("USDC"), dec!(1));
    let outcome = gate
        .on_fill(&oid("o1"), &report(e1.clone(), dec!(1)))
        .expect("booked");
    assert_eq!(gate.halted(), Some(HaltReason::AccountingBreach));
    assert_eq!(outcome.withdraw, vec![oid("o2")]);
    assert_eq!(
        gate.order(&oid("o2")).map(|o| o.state),
        Some(OrderState::CancelPending)
    );
    assert_eq!(balance(&gate, "BTC"), (dec!(2), dec!(1), dec!(1)));

    let again = gate.on_fill(&oid("o1"), &report(e1, dec!(1))).expect("dup");
    assert!(!again.booking.is_new());
    assert_eq!(again.withdraw, [] as [margin_rs::OrderId; 0]);
}
