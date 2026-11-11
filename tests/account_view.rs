//! The account margin view agrees with `assess` built from the same
//! inputs.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::account_view::{AccountMarginView, ViewPosition, ViewReservation};
use margin_rs::initial_margin::{ExposurePosition, ExposureView, InitialMarginModel, ProductKind};
use margin_rs::maintenance::{AccountState, CollateralView, assess};
use margin_rs::rules::TieredLinearIm;
use margin_rs::{
    AccountId, Action, Asset, ContractSize, Decimal, InstrumentId, MarginError, OrderId, VenueId,
};
use rust_decimal_macros::dec;

fn usdc() -> Asset {
    Asset::new("USDC").expect("asset")
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).expect("id")
}

fn exposure() -> ExposureView {
    ExposureView {
        account: AccountId::new("acct").expect("id"),
        venue: VenueId::new("venue").expect("id"),
        positions: vec![ExposurePosition {
            instrument: iid("BTC-PERP"),
            product: ProductKind::LinearPerpetual,
            settlement: usdc(),
            contracts: dec!(10),
            size: ContractSize::Multiplier(Decimal::ONE),
            mark: dec!(95),
        }],
    }
}

fn reservations() -> BTreeMap<OrderId, ViewReservation> {
    BTreeMap::from([(
        OrderId::new("o1").expect("id"),
        ViewReservation {
            instrument: iid("BTC-PERP"),
            side: Action::Buy,
            contracts: dec!(2),
            limit_price: dec!(94),
        },
    )])
}

/// im 95, mm 47.5 on 950 of notional. Collateral 60 (healthy), 40
/// (reduce-only) and 40 with cash -1 (default).
#[test]
fn test_view_agrees_with_assess() {
    let rule = TieredLinearIm::linear_v1(usdc(), 6).expect("rule");
    let basis = BTreeMap::from([(iid("BTC-PERP"), dec!(1000))]);
    for (sequence, cash, collateral, state) in [
        (7, dec!(110), dec!(60), AccountState::Healthy),
        (8, dec!(90), dec!(40), AccountState::ReduceOnly),
        (9, dec!(-1), dec!(40), AccountState::Default),
    ] {
        let view = CollateralView {
            currency: usdc(),
            cash,
            collateral: Some(collateral),
            equity: Some(collateral),
        };
        let decision = assess(
            AccountState::Healthy,
            &view,
            &rule.requirement(&exposure()).expect("requirement"),
        )
        .expect("assess");
        let got = AccountMarginView::assess(
            sequence,
            &rule,
            AccountState::Healthy,
            &exposure(),
            &basis,
            &view,
            reservations(),
        )
        .expect("view");
        assert_eq!(got.sequence, sequence);
        assert_eq!(got.state, state);
        assert_eq!(
            (
                got.state,
                &got.reasons,
                got.collateral,
                got.initial,
                got.maintenance,
                got.excess,
                got.debt,
                &got.rule,
                &got.currency,
            ),
            (
                decision.state,
                &decision.reasons,
                decision.collateral,
                decision.initial,
                decision.maintenance,
                decision.excess,
                decision.debt,
                &decision.rule,
                &decision.currency,
            )
        );
        assert_eq!((got.initial, got.maintenance), (dec!(95), dec!(47.5)));
        assert_eq!((got.cash, got.equity), (cash, view.equity));
        assert_eq!(
            got.positions,
            BTreeMap::from([(
                iid("BTC-PERP"),
                ViewPosition {
                    product: ProductKind::LinearPerpetual,
                    contracts: dec!(10),
                    size: ContractSize::Multiplier(Decimal::ONE),
                    mark: dec!(95),
                    basis: dec!(1000),
                }
            )])
        );
        assert_eq!(got.reservations, reservations());
    }
}

#[test]
fn test_view_refuses_inconsistent_inputs() {
    let rule = TieredLinearIm::linear_v1(usdc(), 6).expect("rule");
    let view = CollateralView {
        currency: usdc(),
        cash: dec!(1),
        collateral: None,
        equity: None,
    };
    let build = |exposure: &ExposureView, basis: &BTreeMap<InstrumentId, Decimal>, orders| {
        AccountMarginView::assess(
            1,
            &rule,
            AccountState::Healthy,
            exposure,
            basis,
            &view,
            orders,
        )
    };
    let mut doubled = exposure();
    doubled.positions.push(doubled.positions[0].clone());
    assert!(matches!(
        build(&doubled, &BTreeMap::new(), BTreeMap::new()),
        Err(MarginError::InvalidInput {
            field: "positions",
            ..
        })
    ));
    assert!(matches!(
        build(
            &exposure(),
            &BTreeMap::from([(iid("ETH-PERP"), dec!(1))]),
            BTreeMap::new()
        ),
        Err(MarginError::InvalidInput { field: "basis", .. })
    ));
    let mut zero = reservations();
    for r in zero.values_mut() {
        r.contracts = Decimal::ZERO;
    }
    assert!(matches!(
        build(&exposure(), &BTreeMap::new(), zero),
        Err(MarginError::InvalidInput {
            field: "reservations",
            ..
        })
    ));
    let mut other = view.clone();
    other.currency = Asset::new("EUR").expect("asset");
    assert!(matches!(
        AccountMarginView::assess(
            1,
            &rule,
            AccountState::Healthy,
            &exposure(),
            &BTreeMap::new(),
            &other,
            BTreeMap::new()
        ),
        Err(MarginError::CurrencyMismatch { .. })
    ));
}
