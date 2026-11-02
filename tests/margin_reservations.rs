//! Margin reservation fixtures (M3-03C), hand calculated with
//! `linear-im/1` (im = 10 % of |contracts| * M * mark).
//!
//! XYZ-PERP and ABC-PERP: M = 10, mark 2000, so one contract needs 2000.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::ProductKind;
use margin_rs::ledger::Side;
use margin_rs::margin_reservations::{
    MarginDecision, MarginInstrument, MarginOrder, MarginRejection, MarginReservationService,
};
use margin_rs::rules::TieredLinearIm;
use margin_rs::{
    AccountId, Asset, Decimal, ExecutionId, InstrumentId, MarginError, OrderId, VenueId,
};
use rust_decimal_macros::dec;

fn usdc() -> Asset {
    Asset::new("USDC").expect("asset")
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).expect("id")
}

fn oid(id: &str) -> OrderId {
    OrderId::new(id).expect("id")
}

fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).expect("id")
}

fn service() -> MarginReservationService<TieredLinearIm> {
    let mut service = MarginReservationService::new(
        TieredLinearIm::linear_v1(usdc(), 6).expect("rule"),
        AccountId::new("acct").expect("id"),
        VenueId::new("venue").expect("id"),
    );
    for id in ["XYZ-PERP", "ABC-PERP"] {
        service
            .register(
                iid(id),
                MarginInstrument {
                    product: ProductKind::LinearPerpetual,
                    settlement: usdc(),
                    multiplier: dec!(10),
                },
            )
            .expect("register");
    }
    service
}

fn marks() -> BTreeMap<InstrumentId, Decimal> {
    BTreeMap::from([(iid("XYZ-PERP"), dec!(2000)), (iid("ABC-PERP"), dec!(2000))])
}

fn order(id: &str, instrument: &str, side: Side, contracts: Decimal) -> MarginOrder {
    MarginOrder {
        id: oid(id),
        instrument: iid(instrument),
        side,
        contracts,
    }
}

fn total(service: &MarginReservationService<TieredLinearIm>) -> (Decimal, Decimal, Decimal) {
    let state = service.requirement(&marks()).expect("requirement");
    (state.positions, state.pending, state.total)
}

/// Acceptance 1: concurrent plans cannot reserve the same capital twice.
///
/// Collateral 6000. A buy of 2 needs 4000 and is reserved. A second buy of
/// 2 on another instrument would make 8000 > 6000: rejected with a
/// shortfall of 2000. A buy of 1 (total 6000) still fits.
#[test]
fn two_orders_cannot_reserve_the_same_capital() {
    let mut service = service();
    let collateral = Some(dec!(6000));
    assert_eq!(
        service
            .reserve(
                order("o1", "XYZ-PERP", Side::Buy, dec!(2)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Reserved {
            required: dec!(4000),
            reserved: dec!(4000),
        }
    );
    assert_eq!(
        service
            .reserve(
                order("o2", "ABC-PERP", Side::Buy, dec!(2)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Rejected {
            reason: MarginRejection::InsufficientMargin {
                required: dec!(8000),
                collateral: dec!(6000),
                shortfall: dec!(2000),
            },
        }
    );
    assert_eq!(total(&service), (dec!(0), dec!(4000), dec!(4000)));
    assert_eq!(
        service
            .reserve(
                order("o3", "ABC-PERP", Side::Buy, dec!(1)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Reserved {
            required: dec!(6000),
            reserved: dec!(2000),
        }
    );
}

/// Acceptance 2: partial fills and effective cancels leave the exact
/// remaining requirement; a rejected plan leaves none.
///
/// Buy 3 (requirement 6000). Fill 1: position 1 (2000) plus 2 pending
/// (4000), total still 6000. The same fill again changes nothing. Cancel
/// takes effect: 2 contracts released, requirement 2000 for the position.
/// Repeating the cancel releases nothing.
#[test]
fn partial_fill_and_cancel_leave_the_exact_requirement() {
    let mut service = service();
    let collateral = Some(dec!(10000));
    service
        .reserve(
            order("o1", "XYZ-PERP", Side::Buy, dec!(3)),
            collateral,
            &marks(),
        )
        .expect("decision");
    assert_eq!(total(&service), (dec!(0), dec!(6000), dec!(6000)));

    assert_eq!(service.apply_fill(&oid("o1"), eid("f1"), dec!(1)), Ok(true));
    assert_eq!(
        service.apply_fill(&oid("o1"), eid("f1"), dec!(1)),
        Ok(false)
    );
    assert_eq!(service.position(&iid("XYZ-PERP")), Some(dec!(1)));
    assert_eq!(service.remaining(&oid("o1")), Some(dec!(2)));
    assert_eq!(total(&service), (dec!(2000), dec!(4000), dec!(6000)));

    assert_eq!(service.release_on_effective_cancel(&oid("o1")), dec!(2));
    assert_eq!(service.release_on_effective_cancel(&oid("o1")), dec!(0));
    assert_eq!(total(&service), (dec!(2000), dec!(0), dec!(2000)));

    // A rejected plan reserves nothing.
    let before = total(&service);
    let rejected = service
        .reserve(
            order("big", "XYZ-PERP", Side::Buy, dec!(10)),
            Some(dec!(3000)),
            &marks(),
        )
        .expect("decision");
    assert!(matches!(rejected, MarginDecision::Rejected { .. }));
    assert_eq!(total(&service), before);
}

/// Orders that reduce a position need no extra margin; the worst case is
/// taken per side, without portfolio offsets across instruments.
///
/// Long 3 XYZ (6000). Sells of 1 and then 5 keep the worst case at
/// max(|3|, |3 - 6|) = 3, so they reserve nothing. A further sell of 4
/// makes it |3 - 10| = 7 (14000). A short of 3 on ABC is margined in full
/// (6000 more): nothing offsets it against the XYZ long.
#[test]
fn reducing_orders_and_no_offsets() {
    let mut service = service();
    let collateral = Some(dec!(100000));
    service
        .reserve(
            order("b", "XYZ-PERP", Side::Buy, dec!(3)),
            collateral,
            &marks(),
        )
        .expect("decision");
    service
        .apply_fill(&oid("b"), eid("f"), dec!(3))
        .expect("fill");
    assert_eq!(total(&service), (dec!(6000), dec!(0), dec!(6000)));

    assert_eq!(
        service
            .reserve(
                order("s1", "XYZ-PERP", Side::Sell, dec!(1)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Reserved {
            required: dec!(6000),
            reserved: dec!(0),
        }
    );
    assert_eq!(
        service
            .reserve(
                order("s2", "XYZ-PERP", Side::Sell, dec!(5)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Reserved {
            required: dec!(6000),
            reserved: dec!(0),
        }
    );
    assert_eq!(
        service
            .reserve(
                order("s3", "XYZ-PERP", Side::Sell, dec!(4)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Reserved {
            required: dec!(14000),
            reserved: dec!(8000),
        }
    );
    // A short on another instrument is not offset against the XYZ long.
    assert_eq!(
        service
            .reserve(
                order("a", "ABC-PERP", Side::Sell, dec!(3)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Reserved {
            required: dec!(20000),
            reserved: dec!(6000),
        }
    );
}

/// A real fill is never lost: an overfill closes the order and moves the
/// whole fill into the position; a fill after the cancel took effect is
/// still applied.
#[test]
fn late_and_over_fills_reach_the_position() {
    let mut service = service();
    let collateral = Some(dec!(100000));
    service
        .reserve(
            order("o1", "XYZ-PERP", Side::Buy, dec!(2)),
            collateral,
            &marks(),
        )
        .expect("decision");
    assert_eq!(service.apply_fill(&oid("o1"), eid("f1"), dec!(3)), Ok(true));
    assert_eq!(service.position(&iid("XYZ-PERP")), Some(dec!(3)));
    assert_eq!(service.remaining(&oid("o1")), None);
    assert_eq!(total(&service), (dec!(6000), dec!(0), dec!(6000)));

    service
        .reserve(
            order("o2", "XYZ-PERP", Side::Sell, dec!(1)),
            collateral,
            &marks(),
        )
        .expect("decision");
    service.release_on_effective_cancel(&oid("o2"));
    assert_eq!(service.apply_fill(&oid("o2"), eid("f2"), dec!(1)), Ok(true));
    assert_eq!(service.position(&iid("XYZ-PERP")), Some(dec!(2)));
    assert_eq!(total(&service), (dec!(4000), dec!(0), dec!(4000)));
    assert_eq!(
        service.apply_fill(&oid("never"), eid("f3"), dec!(1)),
        Err(MarginError::UnknownOrder { id: "never".into() })
    );
}

/// Rejection reasons are exposed.
#[test]
fn rejection_reasons() {
    let mut service = service();
    let collateral = Some(dec!(100000));
    let reject = |decision: MarginDecision| match decision {
        MarginDecision::Rejected { reason } => reason,
        MarginDecision::Reserved { .. } => panic!("expected a rejection"),
    };
    let decide = |service: &mut MarginReservationService<TieredLinearIm>,
                  order,
                  collateral,
                  marks: &BTreeMap<InstrumentId, Decimal>| {
        reject(service.reserve(order, collateral, marks).expect("decision"))
    };
    assert_eq!(
        decide(
            &mut service,
            order("u", "SOL-PERP", Side::Buy, dec!(1)),
            collateral,
            &marks()
        ),
        MarginRejection::UnknownInstrument
    );
    assert_eq!(
        decide(
            &mut service,
            order("z", "XYZ-PERP", Side::Buy, dec!(0)),
            collateral,
            &marks()
        ),
        MarginRejection::InvalidQuantity
    );
    assert_eq!(
        decide(
            &mut service,
            order("c", "XYZ-PERP", Side::Buy, dec!(1)),
            None,
            &marks()
        ),
        MarginRejection::CollateralUnknown
    );
    assert_eq!(
        decide(
            &mut service,
            order("m", "XYZ-PERP", Side::Buy, dec!(1)),
            collateral,
            &BTreeMap::new()
        ),
        MarginRejection::MissingMark {
            instrument: iid("XYZ-PERP"),
        }
    );
    // Every id above was recorded: reusing one is a duplicate.
    assert_eq!(
        decide(
            &mut service,
            order("c", "XYZ-PERP", Side::Buy, dec!(1)),
            collateral,
            &marks()
        ),
        MarginRejection::DuplicateOrderId
    );
    assert_eq!(total(&service), (dec!(0), dec!(0), dec!(0)));
}

/// C1: an execution id reused for another order or quantity is a typed
/// conflict and changes nothing; the identical repeat is a no-op.
#[test]
fn conflicting_fill_ids_are_rejected() {
    let mut service = service();
    service
        .reserve(
            order("a", "XYZ-PERP", Side::Buy, dec!(3)),
            Some(dec!(100000)),
            &marks(),
        )
        .expect("decision");
    service
        .reserve(
            order("b", "XYZ-PERP", Side::Buy, dec!(3)),
            Some(dec!(100000)),
            &marks(),
        )
        .expect("decision");
    assert_eq!(service.apply_fill(&oid("a"), eid("e"), dec!(1)), Ok(true));
    let conflict = Err(MarginError::ConflictingEntry {
        kind: "fill",
        id: "e".into(),
    });
    assert_eq!(service.apply_fill(&oid("a"), eid("e"), dec!(2)), conflict);
    assert_eq!(service.apply_fill(&oid("b"), eid("e"), dec!(1)), conflict);
    assert_eq!(service.apply_fill(&oid("a"), eid("e"), dec!(1)), Ok(false));
    assert_eq!(service.position(&iid("XYZ-PERP")), Some(dec!(1)));
    assert_eq!(service.remaining(&oid("a")), Some(dec!(2)));
    assert_eq!(service.remaining(&oid("b")), Some(dec!(3)));
}

mod with_gate {
    use super::*;
    use margin_rs::ledger::Ledger;
    use margin_rs::risk::{InstrumentLimits, OrderRequest, RiskDecision, RiskGate, RiskReason};
    use margin_rs::{ReservationId, SpotInstrument, TransferId};

    /// 10000 USDC; spot BTC-USDC (lot 0.01) on the gate.
    fn gate() -> RiskGate {
        let mut ledger = Ledger::default();
        ledger.register_asset(usdc(), 6).expect("register");
        ledger
            .register_asset(Asset::new("BTC").expect("asset"), 8)
            .expect("register");
        ledger
            .register_instrument(
                SpotInstrument::new(
                    iid("BTC-USDC"),
                    Asset::new("BTC").expect("asset"),
                    usdc(),
                    2,
                    2,
                )
                .expect("spot"),
            )
            .expect("register");
        let mut gate = RiskGate::new(ledger);
        gate.deposit(TransferId::new("d").expect("id"), usdc(), dec!(10000))
            .expect("deposit");
        gate.set_limits(&iid("BTC-USDC"), InstrumentLimits::default())
            .expect("limits");
        gate
    }

    fn spot_buy(id: &str, quantity: Decimal) -> OrderRequest {
        OrderRequest::new(oid(id), iid("BTC-USDC"), Side::Buy, dec!(100), quantity)
    }

    fn hold() -> ReservationId {
        ReservationId::new("margin:acct").expect("id")
    }

    /// C2, margin first: a margin plan of 6000 becomes a ledger hold, so a
    /// spot buy of 5000 then finds only 4000 available.
    #[test]
    fn margin_first_blocks_the_same_cash_for_spot() {
        let mut gate = gate();
        let mut service = service();
        assert_eq!(
            service
                .reserve_with_gate(
                    order("m1", "XYZ-PERP", Side::Buy, dec!(3)),
                    &mut gate,
                    &hold(),
                    &marks()
                )
                .expect("decision"),
            MarginDecision::Reserved {
                required: dec!(6000),
                reserved: dec!(6000),
            }
        );
        assert_eq!(gate.hold(&hold()), dec!(6000));
        assert_eq!(
            gate.submit(&spot_buy("s1", dec!(50)), 0),
            RiskDecision::Rejected {
                reasons: vec![RiskReason::InsufficientBalance {
                    asset: usdc(),
                    required: dec!(5000),
                    available: dec!(4000),
                }]
            }
        );
        // The margin plan is cancelled: the hold goes and the cash is free.
        service.release_on_effective_cancel(&oid("m1"));
        assert_eq!(service.sync_hold(&mut gate, &hold(), &marks()), Ok(dec!(0)));
        assert!(!gate.submit(&spot_buy("s2", dec!(50)), 0).is_rejected());
        gate.ledger().reconcile().expect("reconciles");
    }

    /// C2, spot first: a spot buy reserves 5000, so a margin plan of 6000
    /// sees 5000 of collateral and is rejected.
    #[test]
    fn spot_first_blocks_the_same_cash_for_margin() {
        let mut gate = gate();
        let mut service = service();
        assert!(!gate.submit(&spot_buy("s1", dec!(50)), 0).is_rejected());
        assert_eq!(
            service
                .reserve_with_gate(
                    order("m1", "XYZ-PERP", Side::Buy, dec!(3)),
                    &mut gate,
                    &hold(),
                    &marks()
                )
                .expect("decision"),
            MarginDecision::Rejected {
                reason: MarginRejection::InsufficientMargin {
                    required: dec!(6000),
                    collateral: dec!(5000),
                    shortfall: dec!(1000),
                },
            }
        );
        assert_eq!(gate.hold(&hold()), dec!(0));
        // A plan that fits the rest (4000) is held.
        assert!(matches!(
            service
                .reserve_with_gate(
                    order("m2", "XYZ-PERP", Side::Buy, dec!(2)),
                    &mut gate,
                    &hold(),
                    &marks()
                )
                .expect("decision"),
            MarginDecision::Reserved { .. }
        ));
        let usdc_balance = gate.ledger().balance(&usdc()).expect("balance");
        assert_eq!(
            (
                usdc_balance.total,
                usdc_balance.reserved,
                usdc_balance.available
            ),
            (dec!(10000), dec!(9000), dec!(1000))
        );
    }

    /// C3: `reserve_with_gate` is atomic. If the hold cannot be set (here
    /// its id collides with an order id of the gate), neither the service
    /// nor the gate changes and the order id stays unused.
    #[test]
    fn failed_hold_changes_nothing() {
        let mut gate = gate();
        let unknown = OrderRequest::new(oid("hold"), iid("UNKNOWN"), Side::Buy, dec!(1), dec!(1));
        assert!(gate.submit(&unknown, 0).is_rejected());
        let mut service = service();
        let colliding = ReservationId::new("hold").expect("id");
        assert_eq!(
            service.reserve_with_gate(
                order("a", "XYZ-PERP", Side::Buy, dec!(3)),
                &mut gate,
                &colliding,
                &marks()
            ),
            Err(MarginError::invalid_input(
                "hold",
                "the id belongs to an order"
            ))
        );
        assert_eq!(service.remaining(&oid("a")), None);
        assert_eq!(gate.hold(&colliding), dec!(0));
        assert_eq!(total(&service), (dec!(0), dec!(0), dec!(0)));
        // The id was not consumed.
        assert!(matches!(
            service
                .reserve_with_gate(
                    order("a", "XYZ-PERP", Side::Buy, dec!(3)),
                    &mut gate,
                    &hold(),
                    &marks()
                )
                .expect("decision"),
            MarginDecision::Reserved { .. }
        ));
    }

    /// E3 with debt: the account is overdrawn (USDC -20), so collateral is
    /// negative. A reducing sell is still accepted and the hold is 0, never
    /// negative; a risk increasing buy is rejected. Same with a previous
    /// hold of 80 (total -20, reserved 80): the hold shrinks to 0.
    #[test]
    fn reducing_orders_are_accepted_in_debt() {
        for previous_hold in [dec!(0), dec!(80)] {
            let mut ledger = Ledger::default();
            ledger.register_asset(usdc(), 6).expect("register");
            ledger
                .register_asset(Asset::new("BTC").expect("asset"), 8)
                .expect("register");
            ledger
                .register_instrument(
                    SpotInstrument::new(
                        iid("BTC-USDC"),
                        Asset::new("BTC").expect("asset"),
                        usdc(),
                        2,
                        2,
                    )
                    .expect("spot"),
                )
                .expect("register");
            if !previous_hold.is_zero() {
                ledger
                    .deposit(TransferId::new("d").expect("id"), usdc(), dec!(100))
                    .expect("deposit");
                ledger
                    .set_reservation(&hold(), usdc(), previous_hold)
                    .expect("hold");
            }
            let debt = if previous_hold.is_zero() {
                dec!(20)
            } else {
                dec!(120)
            };
            let buy = margin_rs::ledger::Execution::new(
                ExecutionId::new("x").expect("id"),
                iid("BTC-USDC"),
                Side::Buy,
                debt,
                dec!(1),
            );
            ledger
                .book_execution_allowing_overdraft(&buy, None)
                .expect("booked");
            assert_eq!(ledger.balance(&usdc()).expect("balance").total, dec!(-20));
            let mut gate = RiskGate::new(ledger);

            let mut service = service();
            service
                .reserve(
                    order("b", "XYZ-PERP", Side::Buy, dec!(10)),
                    Some(dec!(100000)),
                    &marks(),
                )
                .expect("decision");
            service
                .apply_fill(&oid("b"), eid("f"), dec!(10))
                .expect("fill");

            assert!(matches!(
                service
                    .reserve_with_gate(
                        order("s", "XYZ-PERP", Side::Sell, dec!(5)),
                        &mut gate,
                        &hold(),
                        &marks()
                    )
                    .expect("decision"),
                MarginDecision::Reserved { .. }
            ));
            assert_eq!(gate.hold(&hold()), dec!(0));
            assert!(matches!(
                service
                    .reserve_with_gate(
                        order("b2", "XYZ-PERP", Side::Buy, dec!(1)),
                        &mut gate,
                        &hold(),
                        &marks()
                    )
                    .expect("decision"),
                MarginDecision::Rejected {
                    reason: MarginRejection::InsufficientMargin { .. }
                }
            ));
        }
    }

    /// The hold id cannot be an order id of the gate.
    #[test]
    fn hold_ids_do_not_collide_with_orders() {
        let mut gate = gate();
        assert!(!gate.submit(&spot_buy("x", dec!(1)), 0).is_rejected());
        assert_eq!(
            gate.set_hold(&ReservationId::new("x").expect("id"), usdc(), dec!(1)),
            Err(MarginError::invalid_input(
                "hold",
                "the id belongs to an order"
            ))
        );
        // A hold larger than what is available is refused and the previous
        // hold is kept.
        gate.set_hold(&hold(), usdc(), dec!(100)).expect("hold");
        assert!(gate.set_hold(&hold(), usdc(), dec!(20000)).is_err());
        assert_eq!(gate.hold(&hold()), dec!(100));
    }
}

/// M303C-1: risk reducing orders are accepted
/// when under margined; flips and aggregate over-reduction are not.
///
/// Long 10 XYZ (20000) with collateral 10000.
/// - Sell 5: opposite side, 5 <= 10: accepted.
/// - Sell 25 on top: 25 > 10 - 5, and the worst case |10 - 30| = 20
///   (40000) exceeds collateral: rejected.
/// - Sell 20 alone would flip to -10: rejected although the worst case
///   (|10| or |-10|) needs no more margin.
/// - Two sells of 6: the first fits, the second exceeds 10 - 6 = 4.
#[test]
fn reducing_orders_are_accepted_when_under_margined() {
    let mut service = service();
    service
        .reserve(
            order("b", "XYZ-PERP", Side::Buy, dec!(10)),
            Some(dec!(100000)),
            &marks(),
        )
        .expect("decision");
    service
        .apply_fill(&oid("b"), eid("f"), dec!(10))
        .expect("fill");
    let collateral = Some(dec!(10000));
    assert_eq!(
        service
            .reserve(
                order("s1", "XYZ-PERP", Side::Sell, dec!(5)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Reserved {
            required: dec!(20000),
            reserved: dec!(0),
        }
    );
    assert_eq!(
        service
            .reserve(
                order("s2", "XYZ-PERP", Side::Sell, dec!(25)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Rejected {
            reason: MarginRejection::InsufficientMargin {
                required: dec!(40000),
                collateral: dec!(10000),
                shortfall: dec!(30000),
            },
        }
    );
}

fn long_ten_under_margined() -> MarginReservationService<TieredLinearIm> {
    let mut service = service();
    service
        .reserve(
            order("b", "XYZ-PERP", Side::Buy, dec!(10)),
            Some(dec!(100000)),
            &marks(),
        )
        .expect("decision");
    service
        .apply_fill(&oid("b"), eid("f"), dec!(10))
        .expect("fill");
    service
}

#[test]
fn flips_are_not_risk_reducing() {
    let mut service = long_ten_under_margined();
    assert_eq!(
        service
            .reserve(
                order("s", "XYZ-PERP", Side::Sell, dec!(20)),
                Some(dec!(10000)),
                &marks()
            )
            .expect("decision"),
        MarginDecision::Rejected {
            reason: MarginRejection::InsufficientMargin {
                required: dec!(20000),
                collateral: dec!(10000),
                shortfall: dec!(10000),
            },
        }
    );
    // A buy on the same side as the long is never reducing.
    assert!(matches!(
        service
            .reserve(
                order("b2", "XYZ-PERP", Side::Buy, dec!(1)),
                Some(dec!(10000)),
                &marks()
            )
            .expect("decision"),
        MarginDecision::Rejected { .. }
    ));
}

#[test]
fn reducing_orders_count_against_each_other() {
    let mut service = long_ten_under_margined();
    let collateral = Some(dec!(10000));
    assert!(matches!(
        service
            .reserve(
                order("s1", "XYZ-PERP", Side::Sell, dec!(6)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Reserved { .. }
    ));
    assert!(matches!(
        service
            .reserve(
                order("s2", "XYZ-PERP", Side::Sell, dec!(6)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Rejected {
            reason: MarginRejection::InsufficientMargin { .. }
        }
    ));
    // Exactly the rest (4) still fits.
    assert!(matches!(
        service
            .reserve(
                order("s3", "XYZ-PERP", Side::Sell, dec!(4)),
                collateral,
                &marks()
            )
            .expect("decision"),
        MarginDecision::Reserved { .. }
    ));
}
