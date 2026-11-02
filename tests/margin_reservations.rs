//! Margin reservation fixtures (M3-03C), hand calculated with
//! `xynthex-linear-im/1` (im = 10 % of |contracts| * M * mark).
//!
//! XYZ-PERP and ABC-PERP: M = 10, mark 2000, so one contract needs 2000.

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
        TieredLinearIm::xynthex_v1(usdc(), 6).expect("rule"),
        AccountId::new("acct").expect("id"),
        VenueId::new("xynthex").expect("id"),
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
