//! The stress scenarios and the risk gate do not handle inverse futures and
//! perpetuals: they refuse an open inverse position with a typed error,
//! never skipping it, so its value can never go missing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::ledger::{Action, Execution, Ledger};
use margin_rs::risk::{InstrumentLimits, OrderRequest, RiskDecision, RiskGate, RiskReason};
use margin_rs::valuation::{Mark, MarkSet, PriceSource, ValuationPolicy};
use margin_rs::{
    Asset, ExecutionId, InstrumentId, InversePerpetual, InverseTerms, LinearPerpetual, LinearTerms,
    MarginError, OrderId, TransferId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}

fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).unwrap()
}

fn oid(id: &str) -> OrderId {
    OrderId::new(id).unwrap()
}

/// USDC (6 decimals, 10000 deposited) and BTC (8 decimals, 1 deposited),
/// with XYZ-PERP (linear, USDC, M = 10) and BTC-INV (inverse, BTC, CV =
/// 100).
fn gate() -> RiskGate {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).unwrap();
    ledger.register_asset(asset("BTC"), 8).unwrap();
    ledger
        .register_instrument(LinearPerpetual::new(
            LinearTerms::new(iid("XYZ-PERP"), asset("USDC"), dec!(10), 2, 0).unwrap(),
        ))
        .unwrap();
    ledger
        .register_instrument(InversePerpetual::new(
            InverseTerms::new(iid("BTC-INV"), asset("BTC"), dec!(100), 1, 0).unwrap(),
        ))
        .unwrap();
    let mut gate = RiskGate::new(ledger);
    gate.deposit(TransferId::new("u").unwrap(), asset("USDC"), dec!(10000))
        .unwrap();
    gate.deposit(TransferId::new("b").unwrap(), asset("BTC"), dec!(1))
        .unwrap();
    gate
}

fn open_inverse(gate: &mut RiskGate) {
    gate.book_derivative_execution(
        &Execution::new(
            eid("inv"),
            iid("BTC-INV"),
            Action::Buy,
            dec!(50000),
            dec!(100),
        )
        .with_executed_at(1),
    )
    .unwrap();
}

fn unsupported(error: &MarginError) -> bool {
    matches!(
        error,
        MarginError::UnsupportedInstrument { instrument, .. } if *instrument == iid("BTC-INV")
    )
}

/// Stress scenarios refuse it, as they refuse any open derivative.
#[test]
fn stress_refuses_an_open_inverse_position() {
    let mut gate = gate();
    open_inverse(&mut gate);
    let marks = MarkSet::new(10)
        .with(asset("BTC"), Mark::new(dec!(50000), 10, PriceSource::Mid))
        .with_instrument(iid("BTC-INV"), Mark::new(dec!(50000), 10, PriceSource::Mid));
    let error = margin_rs::scenarios::stress(
        &gate.ledger().snapshot(),
        &marks,
        &ValuationPolicy::new(asset("USDC"), 100),
        &margin_rs::scenarios::Scenario::new("gap"),
    )
    .unwrap_err();
    assert!(unsupported(&error));
}

/// The risk gate handles spot orders only: an inverse instrument is refused
/// like a linear one, while its executions are booked through the gate.
#[test]
fn the_risk_gate_refuses_inverse_orders() {
    let mut gate = gate();
    assert!(unsupported(
        &gate
            .set_limits(&iid("BTC-INV"), InstrumentLimits::default())
            .unwrap_err()
    ));
    let order = OrderRequest::new(oid("o"), iid("BTC-INV"), Action::Buy, dec!(50000), dec!(1));
    let RiskDecision::Rejected { reasons } = gate.submit(&order, 1) else {
        panic!("an inverse order must be rejected");
    };
    assert!(matches!(
        reasons.as_slice(),
        [RiskReason::InvalidOrder { error }] if unsupported(error)
    ));
    open_inverse(&mut gate);
    assert_eq!(
        gate.ledger().position(&iid("BTC-INV")).unwrap().cost_basis,
        dec!(0.2)
    );
}
