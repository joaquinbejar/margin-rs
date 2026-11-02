//! Margin reservations that read their positions from the ledger (M3-INT),
//! hand calculated with `linear-im/1` (im = 10 % of |contracts| * M * mark).
//!
//! The gate's ledger holds 10000 USDC (6 decimals) and:
//! - XYZ-PERP and ABC-PERP: perpetuals in USDC, M = 10, mark 2000, so one
//!   contract needs 2000; both registered with the service;
//! - OTHER-PERP: perpetual in USDC, M = 1, not registered with the service;
//! - EUR-PERP: perpetual in EUR, M = 1;
//! - BTC-USDC: spot.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::ProductKind;
use margin_rs::instrument::{LinearPerpetual, LinearTerms};
use margin_rs::ledger::{Action, Execution, Ledger};
use margin_rs::margin_reservations::{
    MarginDecision, MarginInstrument, MarginOrder, MarginRejection, MarginReservationService,
};
use margin_rs::risk::RiskGate;
use margin_rs::rules::TieredLinearIm;
use margin_rs::{
    AccountId, Asset, Decimal, ExecutionId, InstrumentId, MarginError, OrderId, ReservationId,
    SpotInstrument, TransferId, VenueId,
};
use rust_decimal_macros::dec;

type Service = MarginReservationService<TieredLinearIm>;

fn asset(code: &str) -> Asset {
    Asset::new(code).expect("asset")
}

fn usdc() -> Asset {
    asset("USDC")
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

fn hold() -> ReservationId {
    ReservationId::new("margin:acct").expect("id")
}

fn perpetual(id: &str, settlement: &str, multiplier: Decimal) -> LinearPerpetual {
    LinearPerpetual::new(
        LinearTerms::new(iid(id), asset(settlement), multiplier, 2, 0).expect("terms"),
    )
}

fn gate() -> RiskGate {
    let mut ledger = Ledger::default();
    ledger.register_asset(usdc(), 6).expect("asset");
    ledger.register_asset(asset("EUR"), 2).expect("asset");
    ledger.register_asset(asset("BTC"), 8).expect("asset");
    for instrument in [
        perpetual("XYZ-PERP", "USDC", dec!(10)),
        perpetual("ABC-PERP", "USDC", dec!(10)),
        perpetual("OTHER-PERP", "USDC", dec!(1)),
        perpetual("EUR-PERP", "EUR", dec!(1)),
    ] {
        ledger.register_instrument(instrument).expect("instrument");
    }
    ledger
        .register_instrument(
            SpotInstrument::new(iid("BTC-USDC"), asset("BTC"), usdc(), 2, 2).expect("spot"),
        )
        .expect("spot");
    let mut gate = RiskGate::new(ledger);
    gate.deposit(TransferId::new("d").expect("id"), usdc(), dec!(10000))
        .expect("deposit");
    gate
}

fn instrument() -> MarginInstrument {
    MarginInstrument {
        product: ProductKind::LinearPerpetual,
        settlement: usdc(),
        multiplier: dec!(10),
    }
}

fn register(service: &mut Service) {
    for id in ["XYZ-PERP", "ABC-PERP"] {
        service.register(iid(id), instrument()).expect("register");
    }
}

fn rule() -> TieredLinearIm {
    TieredLinearIm::linear_v1(usdc(), 6).expect("rule")
}

fn on_ledger() -> Service {
    let mut service = MarginReservationService::on_ledger(
        rule(),
        AccountId::new("acct").expect("id"),
        VenueId::new("venue").expect("id"),
        hold(),
    );
    register(&mut service);
    service
}

fn standalone() -> Service {
    let mut service = MarginReservationService::new(
        rule(),
        AccountId::new("acct").expect("id"),
        VenueId::new("venue").expect("id"),
    );
    register(&mut service);
    service
}

fn marks() -> BTreeMap<InstrumentId, Decimal> {
    BTreeMap::from([
        (iid("XYZ-PERP"), dec!(2000)),
        (iid("ABC-PERP"), dec!(2000)),
        (iid("OTHER-PERP"), dec!(100)),
    ])
}

fn order(id: &str, instrument: &str, side: Action, contracts: Decimal) -> MarginOrder {
    MarginOrder {
        id: oid(id),
        instrument: iid(instrument),
        side,
        contracts,
    }
}

fn execution(
    id: &str,
    instrument: &str,
    side: Action,
    price: Decimal,
    contracts: Decimal,
) -> Execution {
    Execution::new(eid(id), iid(instrument), side, price, contracts).with_executed_at(1)
}

/// Books a derivative fill in the gate's ledger.
fn book(gate: &mut RiskGate, execution: &Execution) {
    let booked = gate.book_derivative_execution(execution).expect("booked");
    assert!(booked.booking.is_new());
}

fn reserve(
    service: &mut Service,
    gate: &mut RiskGate,
    order: MarginOrder,
    marks: &BTreeMap<InstrumentId, Decimal>,
) -> MarginDecision {
    service
        .reserve_with_gate(order, gate, &hold(), marks)
        .expect("decision")
}

fn requirement(service: &Service, gate: &RiskGate) -> (Decimal, Decimal, Decimal) {
    let state = service
        .requirement_with_gate(gate, &marks())
        .expect("requirement");
    (state.positions, state.pending, state.total)
}

/// The same orders and fills give the same decisions and requirements with
/// the service's own positions and with positions read from the ledger.
/// Fills are at the mark, so the unrealized PnL is zero and collateral is
/// 10000 in both.
#[test]
fn own_and_ledger_positions_agree() {
    let mut gate = gate();
    let mut on = on_ledger();
    let mut own = standalone();
    let steps = [
        order("a", "XYZ-PERP", Action::Buy, dec!(3)),
        order("b", "ABC-PERP", Action::Sell, dec!(2)),
        order("c", "XYZ-PERP", Action::Buy, dec!(1)),
    ];
    for step in steps {
        let ledger_decision = reserve(&mut on, &mut gate, step.clone(), &marks());
        let standalone_decision = own
            .reserve(step, Some(dec!(10000)), &marks())
            .expect("decision");
        assert_eq!(ledger_decision, standalone_decision);
    }
    // c is rejected: 3 + 2 + 1 contracts need 12000.
    assert_eq!(requirement(&on, &gate), (dec!(0), dec!(10000), dec!(10000)));

    book(
        &mut gate,
        &execution("e1", "XYZ-PERP", Action::Buy, dec!(2000), dec!(1)),
    );
    book(
        &mut gate,
        &execution("e2", "ABC-PERP", Action::Sell, dec!(2000), dec!(2)),
    );
    assert_eq!(on.apply_booked_fill(&gate, &oid("a"), &eid("e1")), Ok(true));
    assert_eq!(on.apply_booked_fill(&gate, &oid("b"), &eid("e2")), Ok(true));
    own.apply_fill(&oid("a"), eid("e1"), dec!(1)).expect("fill");
    own.apply_fill(&oid("b"), eid("e2"), dec!(2)).expect("fill");
    on.release_on_effective_cancel(&oid("a"));
    own.release_on_effective_cancel(&oid("a"));

    let standalone_state = own.requirement(&marks()).expect("requirement");
    let ledger_state = on
        .requirement_with_gate(&gate, &marks())
        .expect("requirement");
    assert_eq!(ledger_state, standalone_state);
    assert_eq!(requirement(&on, &gate), (dec!(6000), dec!(0), dec!(6000)));
    assert_eq!(on.sync_hold(&mut gate, &hold(), &marks()), Ok(dec!(6000)));
    assert_eq!(gate.hold(&hold()), dec!(6000));
    gate.ledger().reconcile().expect("reconciles");
}

/// A loss on a derivative the service does not trade lowers collateral.
/// OTHER-PERP: long 10 at 100 (basis 1000), mark 90: upnl -100, so
/// collateral is 10000 - 100 = 9900 and 5 XYZ (10000) no longer fit.
#[test]
fn a_loss_outside_the_service_lowers_collateral() {
    let mut gate = gate();
    book(
        &mut gate,
        &execution("x", "OTHER-PERP", Action::Buy, dec!(100), dec!(10)),
    );
    let mut service = on_ledger();
    let mut losing = marks();
    losing.insert(iid("OTHER-PERP"), dec!(90));
    assert_eq!(
        reserve(
            &mut service,
            &mut gate,
            order("a", "XYZ-PERP", Action::Buy, dec!(5)),
            &losing
        ),
        MarginDecision::Rejected {
            reason: MarginRejection::InsufficientMargin {
                required: dec!(10000),
                collateral: dec!(9900),
                shortfall: dec!(100),
            },
        }
    );
    // At 100 there is no loss and the same order fits exactly.
    assert_eq!(
        reserve(
            &mut service,
            &mut gate,
            order("b", "XYZ-PERP", Action::Buy, dec!(5)),
            &marks()
        ),
        MarginDecision::Reserved {
            required: dec!(10000),
            reserved: dec!(10000),
        }
    );
    assert_eq!(gate.hold(&hold()), dec!(10000));
}

/// A gain counts as collateral but is never held: the hold is capped at
/// the cash. OTHER-PERP long 10 at 100, mark 200: upnl +1000, collateral
/// 11000; 5 XYZ + 0.5 more would need 11000, accepted, hold 10000.
#[test]
fn a_gain_counts_but_the_hold_takes_only_cash() {
    let mut gate = gate();
    book(
        &mut gate,
        &execution("x", "OTHER-PERP", Action::Buy, dec!(100), dec!(10)),
    );
    let mut service = on_ledger();
    let mut winning = marks();
    winning.insert(iid("OTHER-PERP"), dec!(200));
    winning.insert(iid("XYZ-PERP"), dec!(2200));
    // 5 XYZ at 2200: 5 * 10 * 2200 * 10 % = 11000.
    assert_eq!(
        reserve(
            &mut service,
            &mut gate,
            order("a", "XYZ-PERP", Action::Buy, dec!(5)),
            &winning
        ),
        MarginDecision::Reserved {
            required: dec!(11000),
            reserved: dec!(11000),
        }
    );
    assert_eq!(gate.hold(&hold()), dec!(10000));
}

/// Every open derivative settled in USDC needs a mark: a missing one is a
/// rejection, never a zero; a non positive one is an error. One settled in
/// EUR does not count towards USDC collateral and needs no mark.
#[test]
fn marks_of_every_open_derivative_are_required() {
    let mut gate = gate();
    book(
        &mut gate,
        &execution("x", "OTHER-PERP", Action::Buy, dec!(100), dec!(10)),
    );
    book(
        &mut gate,
        &execution("eur", "EUR-PERP", Action::Buy, dec!(10), dec!(1)),
    );
    let mut service = on_ledger();
    let mut missing = marks();
    missing.remove(&iid("OTHER-PERP"));
    assert_eq!(
        reserve(
            &mut service,
            &mut gate,
            order("a", "XYZ-PERP", Action::Buy, dec!(1)),
            &missing
        ),
        MarginDecision::Rejected {
            reason: MarginRejection::MissingMark {
                instrument: iid("OTHER-PERP"),
            },
        }
    );
    for mark in [dec!(0), dec!(-5)] {
        let mut bad = marks();
        bad.insert(iid("OTHER-PERP"), mark);
        assert_eq!(
            service.reserve_with_gate(
                order("b", "XYZ-PERP", Action::Buy, dec!(1)),
                &mut gate,
                &hold(),
                &bad
            ),
            Err(MarginError::invalid_input(
                "instrument mark",
                "must be positive"
            ))
        );
    }
    // The EUR position needs no mark: the order fits.
    assert!(matches!(
        reserve(
            &mut service,
            &mut gate,
            order("b", "XYZ-PERP", Action::Buy, dec!(1)),
            &marks()
        ),
        MarginDecision::Reserved { .. }
    ));
    assert_eq!(service.remaining(&oid("a")), None);
}

/// Collateral adds the service's own hold once and never another hold.
/// A foreign hold of 1000 leaves 9000 available. After a = 3 XYZ (6000)
/// the hold is 6000 and 3000 is available; b = 2 more (total 10000) sees
/// 3000 + 6000 = 9000 and is rejected; c = 1 more (8000) fits and the
/// hold becomes 8000, not 6000 + 8000.
#[test]
fn own_hold_counts_once_and_foreign_holds_never() {
    let mut gate = gate();
    let foreign = ReservationId::new("spot:other").expect("id");
    gate.set_hold(&foreign, usdc(), dec!(1000)).expect("hold");
    let mut service = on_ledger();
    assert!(matches!(
        reserve(
            &mut service,
            &mut gate,
            order("a", "XYZ-PERP", Action::Buy, dec!(3)),
            &marks()
        ),
        MarginDecision::Reserved { .. }
    ));
    assert_eq!(gate.hold(&hold()), dec!(6000));
    assert_eq!(
        reserve(
            &mut service,
            &mut gate,
            order("b", "XYZ-PERP", Action::Buy, dec!(2)),
            &marks()
        ),
        MarginDecision::Rejected {
            reason: MarginRejection::InsufficientMargin {
                required: dec!(10000),
                collateral: dec!(9000),
                shortfall: dec!(1000),
            },
        }
    );
    assert_eq!(
        reserve(
            &mut service,
            &mut gate,
            order("c", "XYZ-PERP", Action::Buy, dec!(1)),
            &marks()
        ),
        MarginDecision::Reserved {
            required: dec!(8000),
            reserved: dec!(2000),
        }
    );
    assert_eq!(gate.hold(&hold()), dec!(8000));
    assert_eq!(gate.hold(&foreign), dec!(1000));
    let balance = gate.ledger().balance(&usdc()).expect("balance");
    assert_eq!(
        (balance.reserved, balance.available),
        (dec!(9000), dec!(1000))
    );

    // Another id than the service's own is refused, and so is its own hold
    // in another asset.
    assert_eq!(
        service.reserve_with_gate(
            order("d", "XYZ-PERP", Action::Buy, dec!(1)),
            &mut gate,
            &foreign,
            &marks()
        ),
        Err(MarginError::invalid_input(
            "hold",
            "is not the hold of this service"
        ))
    );
    assert_eq!(
        service.sync_hold(&mut gate, &foreign, &marks()),
        Err(MarginError::invalid_input(
            "hold",
            "is not the hold of this service"
        ))
    );
    let mut eur_hold = self::gate();
    eur_hold
        .deposit(TransferId::new("e").expect("id"), asset("EUR"), dec!(10))
        .expect("deposit");
    eur_hold
        .set_hold(&hold(), asset("EUR"), dec!(5))
        .expect("hold");
    assert_eq!(
        on_ledger().reserve_with_gate(
            order("a", "XYZ-PERP", Action::Buy, dec!(1)),
            &mut eur_hold,
            &hold(),
            &marks()
        ),
        Err(MarginError::CurrencyMismatch {
            field: "hold",
            expected: usdc(),
            found: asset("EUR"),
        })
    );
}

/// Fills are booked first and reported by execution id: the contracts are
/// the booking's, a replay changes nothing, an unbooked fill is an error,
/// and the hold follows the ledger's position.
#[test]
fn booked_fills_move_only_the_remaining_quantity() {
    let mut gate = gate();
    let mut service = on_ledger();
    reserve(
        &mut service,
        &mut gate,
        order("a", "XYZ-PERP", Action::Buy, dec!(3)),
        &marks(),
    );
    reserve(
        &mut service,
        &mut gate,
        order("b", "ABC-PERP", Action::Buy, dec!(1)),
        &marks(),
    );

    assert_eq!(
        service.apply_booked_fill(&gate, &oid("a"), &eid("e1")),
        Err(MarginError::FillNotBooked { id: "e1".into() })
    );
    assert_eq!(service.remaining(&oid("a")), Some(dec!(3)));

    book(
        &mut gate,
        &execution("e1", "XYZ-PERP", Action::Buy, dec!(2000), dec!(2)),
    );
    assert_eq!(
        service.apply_booked_fill(&gate, &oid("a"), &eid("e1")),
        Ok(true)
    );
    assert_eq!(service.remaining(&oid("a")), Some(dec!(1)));
    // Replay: nothing moves twice.
    assert_eq!(
        service.apply_booked_fill(&gate, &oid("a"), &eid("e1")),
        Ok(false)
    );
    assert_eq!(service.remaining(&oid("a")), Some(dec!(1)));
    // The same execution for another order, or a booking that does not
    // match the order, is a conflict.
    let conflict = Err(MarginError::ConflictingEntry {
        kind: "fill",
        id: "e1".into(),
    });
    assert_eq!(
        service.apply_booked_fill(&gate, &oid("b"), &eid("e1")),
        conflict
    );
    book(
        &mut gate,
        &execution("e2", "XYZ-PERP", Action::Buy, dec!(2000), dec!(1)),
    );
    assert_eq!(
        service.apply_booked_fill(&gate, &oid("b"), &eid("e2")),
        Err(MarginError::ConflictingEntry {
            kind: "fill",
            id: "e2".into(),
        })
    );
    book(
        &mut gate,
        &execution("e3", "XYZ-PERP", Action::Sell, dec!(2000), dec!(1)),
    );
    assert_eq!(
        service.apply_booked_fill(&gate, &oid("a"), &eid("e3")),
        Err(MarginError::ConflictingEntry {
            kind: "fill",
            id: "e3".into(),
        })
    );
    assert_eq!(
        service.apply_booked_fill(&gate, &oid("zzz"), &eid("e2")),
        Err(MarginError::UnknownOrder { id: "zzz".into() })
    );
    // e2 belongs to a: an overfill closes it with nothing remaining.
    assert_eq!(
        service.apply_booked_fill(&gate, &oid("a"), &eid("e2")),
        Ok(true)
    );
    assert_eq!(service.remaining(&oid("a")), None);

    // Ledger position: 2 + 1 - 1 = 2 XYZ (e3 was booked though not
    // reported); b still pending 1 ABC. Requirement 4000 + 2000.
    assert_eq!(
        requirement(&service, &gate),
        (dec!(4000), dec!(2000), dec!(6000))
    );
    assert_eq!(
        service.sync_hold(&mut gate, &hold(), &marks()),
        Ok(dec!(6000))
    );
    assert_eq!(service.position(&iid("XYZ-PERP")), None);
    gate.ledger().reconcile().expect("reconciles");
}

/// A mandatory fill is booked even when its fee overdraws the account;
/// the service follows it: collateral is negative, risk increasing orders
/// are rejected and a reducing one is accepted with a hold of zero.
#[test]
fn mandatory_fills_into_debt() {
    let mut gate = gate();
    let mut service = on_ledger();
    reserve(
        &mut service,
        &mut gate,
        order("a", "XYZ-PERP", Action::Buy, dec!(1)),
        &marks(),
    );
    let costly =
        execution("e1", "XYZ-PERP", Action::Buy, dec!(2000), dec!(1)).with_fee(usdc(), dec!(12000));
    let booked = gate.book_derivative_execution(&costly).expect("booked");
    assert_eq!(booked.shortfalls, BTreeMap::from([(usdc(), dec!(4000))]));
    assert_eq!(
        service.apply_booked_fill(&gate, &oid("a"), &eid("e1")),
        Ok(true)
    );
    // Cash: 10000 - 12000 = -2000; the hold of 2000 stays reserved, so
    // available is -4000 and available + hold = -2000.
    assert_eq!(
        reserve(
            &mut service,
            &mut gate,
            order("b", "XYZ-PERP", Action::Buy, dec!(1)),
            &marks()
        ),
        MarginDecision::Rejected {
            reason: MarginRejection::InsufficientMargin {
                required: dec!(4000),
                collateral: dec!(-2000),
                shortfall: dec!(6000),
            },
        }
    );
    assert!(matches!(
        reserve(
            &mut service,
            &mut gate,
            order("c", "XYZ-PERP", Action::Sell, dec!(1)),
            &marks()
        ),
        MarginDecision::Reserved { .. }
    ));
    assert_eq!(gate.hold(&hold()), dec!(0));
}

/// Each mode refuses the other's operations.
#[test]
fn modes_are_explicit() {
    let mut gate = gate();
    let mut on = on_ledger();
    let wrong = |operation| MarginError::WrongReservationMode {
        operation,
        mode: "on ledger",
    };
    assert_eq!(
        on.reserve(
            order("a", "XYZ-PERP", Action::Buy, dec!(1)),
            Some(dec!(1)),
            &marks()
        ),
        Err(wrong("reserve"))
    );
    assert_eq!(on.requirement(&marks()), Err(wrong("requirement")));
    assert_eq!(
        on.apply_fill(&oid("a"), eid("e"), dec!(1)),
        Err(wrong("apply_fill"))
    );
    // The refused reserve did not use the id.
    assert!(matches!(
        reserve(
            &mut on,
            &mut gate,
            order("a", "XYZ-PERP", Action::Buy, dec!(1)),
            &marks()
        ),
        MarginDecision::Reserved { .. }
    ));
    let mut own = standalone();
    assert_eq!(
        own.apply_booked_fill(&gate, &oid("a"), &eid("e")),
        Err(MarginError::WrongReservationMode {
            operation: "apply_booked_fill",
            mode: "own positions",
        })
    );
}

/// On the ledger every registered instrument must match the ledger's.
#[test]
fn the_registry_is_checked_against_the_ledger() {
    let mut gate = gate();
    let cases = [
        (
            "XYZ-PERP",
            MarginInstrument {
                multiplier: dec!(5),
                ..instrument()
            },
            MarginError::invalid_input("multiplier", "differs from the ledger instrument"),
        ),
        (
            "XYZ-PERP",
            MarginInstrument {
                settlement: asset("EUR"),
                ..instrument()
            },
            MarginError::CurrencyMismatch {
                field: "settlement",
                expected: usdc(),
                found: asset("EUR"),
            },
        ),
        (
            "XYZ-PERP",
            MarginInstrument {
                product: ProductKind::LinearFuture,
                ..instrument()
            },
            MarginError::invalid_input("product", "differs from the ledger instrument"),
        ),
        (
            "BTC-USDC",
            instrument(),
            MarginError::invalid_input(
                "instrument",
                "is not a linear future or perpetual in the ledger",
            ),
        ),
        (
            "NOPE-PERP",
            instrument(),
            MarginError::UnknownInstrument {
                instrument: iid("NOPE-PERP"),
            },
        ),
    ];
    for (id, terms, error) in cases {
        let mut service = MarginReservationService::on_ledger(
            rule(),
            AccountId::new("acct").expect("id"),
            VenueId::new("venue").expect("id"),
            hold(),
        );
        service.register(iid(id), terms).expect("register");
        assert_eq!(
            service.reserve_with_gate(
                order("a", id, Action::Buy, dec!(1)),
                &mut gate,
                &hold(),
                &marks()
            ),
            Err(error.clone())
        );
        assert_eq!(service.requirement_with_gate(&gate, &marks()), Err(error));
    }
}

/// Spot executions go through the gate's orders, never the derivative
/// path.
#[test]
fn the_gate_books_only_derivative_executions_directly() {
    let mut gate = gate();
    assert_eq!(
        gate.book_derivative_execution(&execution(
            "s",
            "BTC-USDC",
            Action::Buy,
            dec!(100),
            dec!(1)
        )),
        Err(MarginError::invalid_input(
            "execution",
            "spot executions are booked through the gate's orders"
        ))
    );
    assert!(gate.ledger().execution_cashflow(&eid("s")).is_none());
}
