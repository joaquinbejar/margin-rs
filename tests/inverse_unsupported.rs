//! Until closeout plans support inverse futures and perpetuals (#26, part
//! C), every consumer that meets an open inverse position and cannot value
//! or margin it refuses it with a typed error: it is never skipped, so its
//! requirement or its value can never go missing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::ProductKind;
use margin_rs::ledger::{Action, Execution, Ledger};
use margin_rs::liquidation::LiquidationAccount;
use margin_rs::risk::{InstrumentLimits, OrderRequest, RiskDecision, RiskGate, RiskReason};
use margin_rs::rules::TieredLinearIm;
use margin_rs::valuation::{Mark, MarkSet, PriceSource, ValuationPolicy};
use margin_rs::{
    AccountId, Asset, ContractSize, Decimal, ExecutionId, InstrumentId, InversePerpetual,
    InverseTerms, LinearPerpetual, LinearTerms, MarginError, OrderId, TransferId, VenueId,
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

fn rule() -> TieredLinearIm {
    TieredLinearIm::linear_v1(asset("USDC"), 6).unwrap()
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

fn marks() -> BTreeMap<InstrumentId, Decimal> {
    BTreeMap::from([(iid("XYZ-PERP"), dec!(2000)), (iid("BTC-INV"), dec!(50000))])
}

fn unsupported(error: &MarginError) -> bool {
    matches!(
        error,
        MarginError::UnsupportedInstrument { instrument, .. } if *instrument == iid("BTC-INV")
    )
}

/// The closeout planner refuses an inverse position given by hand.
#[test]
fn closeout_plans_refuse_an_inverse_position() {
    let account = LiquidationAccount {
        view: margin_rs::initial_margin::ExposureView {
            account: AccountId::new("acct").unwrap(),
            venue: VenueId::new("venue").unwrap(),
            positions: vec![margin_rs::initial_margin::ExposurePosition {
                instrument: iid("BTC-INV"),
                product: ProductKind::InversePerpetual,
                settlement: asset("BTC"),
                contracts: dec!(100),
                size: ContractSize::ContractValue(dec!(100)),
                mark: dec!(50000),
            }],
        },
        basis: BTreeMap::from([(iid("BTC-INV"), dec!(0.2))]),
        cash: dec!(0),
        resting_orders: vec![],
    };
    let error = margin_rs::liquidation::plan_liquidation(
        &margin_rs::rules::TieredLinearIm::linear_v1(asset("BTC"), 8).unwrap(),
        &account,
        &BTreeMap::new(),
        &margin_rs::liquidation::LiquidationPolicy {
            target: margin_rs::liquidation::CloseoutTarget::Flat,
            fee_rate: dec!(0),
            contract_step: dec!(1),
        },
    )
    .unwrap_err();
    assert!(unsupported(&error));
}

/// A closeout plan is never read from a ledger with an open inverse
/// position.
#[test]
fn closeout_accounts_refuse_an_open_inverse_position() {
    let mut gate = gate();
    open_inverse(&mut gate);
    let error = LiquidationAccount::from_ledger(
        &rule(),
        &gate.ledger().snapshot(),
        AccountId::new("acct").unwrap(),
        VenueId::new("venue").unwrap(),
        &asset("USDC"),
        &marks(),
        10,
    )
    .unwrap_err();
    assert!(unsupported(&error));
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
