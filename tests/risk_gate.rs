//! Pretrade risk gate fixtures (M1-05).
//!
//! Expected values are written out by hand from the rules in
//! `docs/risk-gate.md`.

use margin_rs::ledger::{Booking, Execution, Ledger, Side};
use margin_rs::risk::{
    HaltReason, InstrumentLimits, OrderRequest, OrderState, RiskDecision, RiskGate, RiskReason,
};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, OrderId, SpotInstrument, TransferId,
};
use rust_decimal_macros::dec;

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
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).expect("register");
    ledger.register_asset(asset("BTC"), 8).expect("register");
    ledger
        .register_instrument(
            SpotInstrument::new(pair(), asset("BTC"), asset("USDC"), 2, 8).expect("spot"),
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
        .on_fill(&oid("o1"), &fill("e1", Side::Buy, dec!(99), dec!(2)))
        .expect("fill");
    assert!(booking.is_new());
    assert_eq!(balance(&gate, "USDC"), (dec!(802), dec!(300), dec!(502)));

    assert_eq!(gate.on_cancel_confirmed(&oid("o1")), Ok(dec!(300)));
    assert_eq!(balance(&gate, "USDC"), (dec!(802), dec!(0), dec!(802)));
    assert_eq!(
        gate.order(&oid("o1")).map(|o| o.state),
        Some(OrderState::Canceled)
    );
    // A repeated confirmation changes nothing.
    assert_eq!(gate.on_cancel_confirmed(&oid("o1")), Ok(dec!(0)));
    assert_eq!(
        gate.on_fill(&oid("o1"), &fill("e2", Side::Buy, dec!(99), dec!(1))),
        Err(MarginError::InvalidOrderState {
            id: "o1".into(),
            state: "canceled",
            operation: "fill",
        })
    );
    gate.ledger().reconcile().expect("reconciles");
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
    let mut gate = gate(dec!(1000), dec!(0), limits);
    gate.submit(&buy("o1", dec!(100), dec!(4)), 0);
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(1000), dec!(400.4), dec!(599.6))
    );

    let e1 = fill("e1", Side::Buy, dec!(100), dec!(1)).with_fee(asset("USDC"), dec!(0.1));
    let first = gate.on_fill(&oid("o1"), &e1).expect("fill");
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(899.9), dec!(300.3), dec!(599.6))
    );
    for _ in 0..3 {
        assert_eq!(
            gate.on_fill(&oid("o1"), &e1),
            Ok(Booking::Duplicate(first.cashflow().clone()))
        );
    }
    assert_eq!(
        balance(&gate, "USDC"),
        (dec!(899.9), dec!(300.3), dec!(599.6))
    );
    assert_eq!(gate.order(&oid("o1")).map(|o| o.filled), Some(dec!(1)));
    let conflicting = fill("e1", Side::Buy, dec!(100), dec!(2)).with_fee(asset("USDC"), dec!(0.1));
    assert_eq!(
        gate.on_fill(&oid("o1"), &conflicting),
        Err(MarginError::ConflictingEntry {
            kind: "execution",
            id: "e1".into(),
        })
    );

    let e2 = fill("e2", Side::Buy, dec!(100), dec!(3)).with_fee(asset("USDC"), dec!(0.3));
    gate.on_fill(&oid("o1"), &e2).expect("fill");
    assert_eq!(balance(&gate, "USDC"), (dec!(599.6), dec!(0), dec!(599.6)));
    assert_eq!(
        gate.order(&oid("o1")).map(|o| o.state),
        Some(OrderState::Filled)
    );
    assert_eq!(gate.ledger().snapshot().reservations.len(), 0);
    gate.ledger().reconcile().expect("reconciles");
}

/// The buy reservation is rounded up: 0.33333333 * 100.01 * 1.001 =
/// 33.3700029996333, reserved as 33.370003.
#[test]
fn buy_reservation_rounds_up_to_the_quote_grid() {
    let limits = InstrumentLimits {
        max_fee_rate: dec!(0.001),
        ..InstrumentLimits::default()
    };
    let mut gate = gate(dec!(1000), dec!(0), limits);
    gate.submit(&buy("o1", dec!(100.01), dec!(0.33333333)), 0);
    assert_eq!(balance(&gate, "USDC").1, dec!(33.370003));
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
    gate.on_fill(&oid("b1"), &fill("f1", Side::Buy, dec!(100), dec!(6)))
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
/// their reservations (100 + 105) until each cancel is confirmed; a fill
/// racing the withdrawal is still booked.
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
    assert_eq!(balance(&gate, "USDC"), (dec!(10000), dec!(205), dec!(9795)));
    gate.on_market_data(&pair(), dec!(100), 2600).expect("mark");
    assert_eq!(
        gate.submit(&buy("o5", dec!(100), dec!(1)), 2600),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::Halted {
                reason: HaltReason::DataDisconnect,
            }]
        }
    );

    gate.on_fill(&oid("o1"), &fill("e1", Side::Buy, dec!(100), dec!(1)))
        .expect("fill races the cancel");
    assert_eq!(gate.on_cancel_confirmed(&oid("o3")), Ok(dec!(105)));
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
    gate.on_fill(&oid("o2"), &fill("e1", Side::Buy, dec!(100), dec!(1)))
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

/// A fill must match its order; a mismatch changes nothing.
#[test]
fn fills_must_match_their_order() {
    let mut gate = gate(dec!(1000), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(2)), 0);
    let before = gate.ledger().snapshot();
    let mismatch = |reason| MarginError::ExecutionMismatch {
        id: "o1".into(),
        reason,
    };
    assert_eq!(
        gate.on_fill(&oid("o1"), &fill("e1", Side::Buy, dec!(100.01), dec!(1))),
        Err(mismatch("price is worse than the limit"))
    );
    assert_eq!(
        gate.on_fill(&oid("o1"), &fill("e1", Side::Buy, dec!(100), dec!(3))),
        Err(mismatch("quantity exceeds the remaining quantity"))
    );
    assert_eq!(
        gate.on_fill(&oid("o1"), &fill("e1", Side::Sell, dec!(100), dec!(1))),
        Err(mismatch("side differs from the order"))
    );
    assert_eq!(
        gate.on_fill(&oid("nope"), &fill("e1", Side::Buy, dec!(100), dec!(1))),
        Err(MarginError::UnknownOrder { id: "nope".into() })
    );
    assert_eq!(gate.ledger().snapshot(), before);
    assert_eq!(gate.order(&oid("o1")).map(|o| o.filled), Some(dec!(0)));
}

/// A fill the reservations do not cover cannot be booked: the gate trips
/// itself and nothing changes.
///
/// 100 USDC, buy 1 at 100 with no fee reserve: 100 reserved, 0 available.
/// The venue charges a 1 USDC fee: the fill needs 101.
#[test]
fn uncovered_fill_trips_an_accounting_breach() {
    let mut gate = gate(dec!(100), dec!(0), InstrumentLimits::default());
    gate.submit(&buy("o1", dec!(100), dec!(1)), 0);
    let before = gate.ledger().snapshot();
    let e1 = fill("e1", Side::Buy, dec!(100), dec!(1)).with_fee(asset("USDC"), dec!(1));
    assert_eq!(
        gate.on_fill(&oid("o1"), &e1),
        Err(MarginError::InsufficientBalance {
            asset: asset("USDC"),
            required: dec!(101),
            available: dec!(100),
        })
    );
    assert_eq!(gate.halted(), Some(HaltReason::AccountingBreach));
    assert_eq!(gate.ledger().snapshot(), before);
    assert_eq!(gate.order(&oid("o1")).map(|o| o.filled), Some(dec!(0)));
}
