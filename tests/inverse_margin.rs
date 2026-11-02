//! Initial and maintenance margin of inverse futures and perpetuals, and
//! margin reservations with them (#26, part B).
//!
//! BTC is the base and settlement asset, at 8 decimals; CV = 100 (USD) and
//! marks in USD per BTC, so `Q` contracts at mark `m` have notional
//! `Q * 100 / m` BTC. Every expected value is written out by hand (and
//! checked against exact fractions).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement, ProductKind, RuleId,
    TierSlice,
};
use margin_rs::ledger::{Action, Execution, Ledger};
use margin_rs::margin_reservations::{
    BookedFill, MarginDecision, MarginInstrument, MarginOrder, MarginRejection,
    MarginReservationService,
};
use margin_rs::risk::RiskGate;
use margin_rs::rules::{MarginTier, TieredInverseIm, TieredLinearIm};
use margin_rs::{
    AccountId, Asset, ContractSize, Decimal, ExecutionId, InstrumentId, InversePerpetual,
    InverseTerms, MarginError, OrderId, ReservationId, TransferId, VenueId,
};
use rust_decimal_macros::dec;

fn btc() -> Asset {
    Asset::new("BTC").unwrap()
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}

fn oid(id: &str) -> OrderId {
    OrderId::new(id).unwrap()
}

fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).unwrap()
}

fn inverse(contracts: Decimal, mark: Decimal) -> ExposurePosition {
    ExposurePosition {
        instrument: iid("BTC-INV"),
        product: ProductKind::InversePerpetual,
        settlement: btc(),
        contracts,
        size: ContractSize::ContractValue(dec!(100)),
        mark,
    }
}

fn view(positions: Vec<ExposurePosition>) -> ExposureView {
    ExposureView {
        account: AccountId::new("acct").unwrap(),
        venue: VenueId::new("venue").unwrap(),
        positions,
    }
}

fn v1() -> TieredInverseIm {
    TieredInverseIm::inverse_v1(btc(), 8).unwrap()
}

/// Two tiers in BTC: up to 1 BTC at 5 % / 2.5 %, above at 10 % / 5 %.
fn two_tiers() -> TieredInverseIm {
    TieredInverseIm::new(
        RuleId::new("inverse-two-tier", 1),
        btc(),
        8,
        vec![
            MarginTier {
                floor: dec!(0),
                initial_rate: dec!(0.05),
                maintenance_rate: dec!(0.025),
            },
            MarginTier {
                floor: dec!(1),
                initial_rate: dec!(0.10),
                maintenance_rate: dec!(0.05),
            },
        ],
    )
    .unwrap()
}

fn requirement(rule: &TieredInverseIm, position: ExposurePosition) -> MarginRequirement {
    rule.requirement(&view(vec![position])).unwrap()
}

/// 100 contracts at 50000: notional 0.2 BTC, im 0.02, mm 0.01, long or
/// short.
#[test]
fn inverse_v1_on_an_exact_notional() {
    for contracts in [dec!(100), dec!(-100)] {
        let result = requirement(&v1(), inverse(contracts, dec!(50000)));
        assert_eq!(result.rule.to_string(), "inverse-im/1");
        assert_eq!(result.currency, btc());
        assert_eq!(
            (result.initial, result.maintenance),
            (dec!(0.02), dec!(0.01))
        );
        assert_eq!(result.positions[0].notional, Some(dec!(0.2)));
    }
}

/// 3 contracts at 33333: notional 300 / 33333 = 0.0090000900009...;
/// im = 0.00090000900009... rounded up to 0.00090001, mm to 0.00045001.
/// The notional is reported rounded half-even to 18 decimals.
#[test]
fn a_periodic_notional_is_rounded_up_once() {
    let result = requirement(&v1(), inverse(dec!(3), dec!(33333)));
    assert_eq!(
        (result.initial, result.maintenance),
        (dec!(0.00090001), dec!(0.00045001))
    );
    assert_eq!(
        result.positions[0].notional,
        Some(dec!(0.009000090000900009))
    );
}

/// Around the 1 BTC tier floor (mark 50000, so the notional is Q / 500):
///
/// | Q | notional | im | mm |
/// |---|---|---|---|
/// | 499 | 0.998 | 0.0499 | 0.02495 |
/// | 500 | 1 (on the floor: one slice) | 0.05 | 0.025 |
/// | 501 | 1.002 | 0.05 + 0.0002 = 0.0502 | 0.025 + 0.0001 = 0.0251 |
///
/// and above the floor with a periodic notional: 501 at 50001 is
/// 1.0019799604..., im 0.05 + 9.9 / 50001 = 0.0501979960... rounded up to
/// 0.050198, mm 0.025 + 4.95 / 50001 rounded up to 0.025099.
#[test]
fn tier_floors_are_compared_exactly() {
    let slice = |tier, notional, initial_rate, maintenance_rate| TierSlice {
        tier,
        notional: Some(notional),
        initial_rate,
        maintenance_rate,
    };
    let cases = [
        (
            dec!(499),
            dec!(50000),
            dec!(0.0499),
            dec!(0.02495),
            vec![slice(0, dec!(0.998), dec!(0.05), dec!(0.025))],
        ),
        (
            dec!(500),
            dec!(50000),
            dec!(0.05),
            dec!(0.025),
            vec![slice(0, dec!(1), dec!(0.05), dec!(0.025))],
        ),
        (
            dec!(501),
            dec!(50000),
            dec!(0.0502),
            dec!(0.0251),
            vec![
                slice(0, dec!(1), dec!(0.05), dec!(0.025)),
                slice(1, dec!(0.002), dec!(0.10), dec!(0.05)),
            ],
        ),
        (
            dec!(501),
            dec!(50001),
            dec!(0.050198),
            dec!(0.025099),
            vec![
                slice(0, dec!(1), dec!(0.05), dec!(0.025)),
                slice(1, dec!(0.001979960400791984), dec!(0.10), dec!(0.05)),
            ],
        ),
    ];
    for (contracts, mark, initial, maintenance, slices) in cases {
        let result = requirement(&two_tiers(), inverse(contracts, mark));
        assert_eq!(
            (result.initial, result.maintenance),
            (initial, maintenance),
            "{contracts} at {mark}"
        );
        assert_eq!(result.positions[0].slices, slices, "{contracts} at {mark}");
    }
}

/// A notional beyond `Decimal::MAX` (MAX contracts of CV 2 at mark 1, in a
/// base with no decimals) is reported as None and does not block the
/// representable margins 0.2 * MAX and 0.1 * MAX (rounded up).
#[test]
fn a_notional_beyond_max_does_not_block_the_margin() {
    let rule = TieredInverseIm::inverse_v1(btc(), 0).unwrap();
    let mut position = inverse(Decimal::MAX, dec!(1));
    position.size = ContractSize::ContractValue(dec!(2));
    let result = requirement(&rule, position);
    assert_eq!(result.positions[0].notional, None);
    assert_eq!(result.initial, dec!(15845632502852867518708790067));
    assert_eq!(result.maintenance, dec!(7922816251426433759354395034));
}

/// Products, sizes and currencies outside each rule fail with typed
/// errors.
#[test]
fn rules_refuse_the_other_kind() {
    let linear_position = ExposurePosition {
        product: ProductKind::LinearPerpetual,
        size: ContractSize::Multiplier(dec!(1)),
        ..inverse(dec!(1), dec!(100))
    };
    assert!(matches!(
        v1().requirement(&view(vec![linear_position])),
        Err(MarginError::UnsupportedInstrument { .. })
    ));
    let linear_rule = TieredLinearIm::linear_v1(btc(), 8).unwrap();
    assert!(matches!(
        linear_rule.requirement(&view(vec![inverse(dec!(1), dec!(100))])),
        Err(MarginError::UnsupportedInstrument { .. })
    ));
    let mismatched = ExposurePosition {
        size: ContractSize::Multiplier(dec!(100)),
        ..inverse(dec!(1), dec!(100))
    };
    assert!(matches!(
        v1().requirement(&view(vec![mismatched])),
        Err(MarginError::InvalidInput { field: "size", .. })
    ));
    let mut usd = inverse(dec!(1), dec!(100));
    usd.settlement = Asset::new("USD").unwrap();
    assert!(matches!(
        v1().requirement(&view(vec![usd])),
        Err(MarginError::CurrencyMismatch { .. })
    ));
    for bad in [dec!(0), dec!(-1)] {
        assert!(
            v1().requirement(&view(vec![inverse(dec!(1), bad)]))
                .is_err()
        );
    }
}

fn instrument() -> MarginInstrument {
    MarginInstrument {
        product: ProductKind::InversePerpetual,
        settlement: btc(),
        size: ContractSize::ContractValue(dec!(100)),
    }
}

fn buy(id: &str, contracts: Decimal) -> MarginOrder {
    MarginOrder {
        id: oid(id),
        instrument: iid("BTC-INV"),
        side: Action::Buy,
        contracts,
    }
}

fn marks(mark: Decimal) -> BTreeMap<InstrumentId, Decimal> {
    BTreeMap::from([(iid("BTC-INV"), mark)])
}

/// A service with its own positions margins inverse orders: 100 contracts
/// at 50000 need 0.02 BTC.
#[test]
fn standalone_reservations_margin_inverse_orders() {
    let mut service = MarginReservationService::new(
        v1(),
        AccountId::new("acct").unwrap(),
        VenueId::new("venue").unwrap(),
    );
    service.register(iid("BTC-INV"), instrument()).unwrap();
    assert_eq!(
        service
            .reserve(buy("a", dec!(100)), Some(dec!(0.01)), &marks(dec!(50000)))
            .unwrap(),
        MarginDecision::Rejected {
            reason: MarginRejection::InsufficientMargin {
                required: dec!(0.02),
                collateral: dec!(0.01),
                shortfall: dec!(0.01),
            },
        }
    );
    assert_eq!(
        service
            .reserve(buy("b", dec!(100)), Some(dec!(0.02)), &marks(dec!(50000)))
            .unwrap(),
        MarginDecision::Reserved {
            required: dec!(0.02),
            reserved: dec!(0.02),
        }
    );
    // The size must be a contract value for an inverse product.
    assert!(matches!(
        service.register(
            iid("BAD"),
            MarginInstrument {
                size: ContractSize::Multiplier(dec!(100)),
                ..instrument()
            }
        ),
        Err(MarginError::InvalidInput { field: "size", .. })
    ));
}

fn hold() -> ReservationId {
    ReservationId::new("margin").unwrap()
}

/// 0.05 BTC deposited; BTC-INV registered in the ledger and the service.
fn on_ledger() -> (RiskGate, MarginReservationService<TieredInverseIm>) {
    let mut ledger = Ledger::default();
    ledger.register_asset(btc(), 8).unwrap();
    ledger
        .register_instrument(InversePerpetual::new(
            InverseTerms::new(iid("BTC-INV"), btc(), dec!(100), 1, 0).unwrap(),
        ))
        .unwrap();
    let mut gate = RiskGate::new(ledger);
    gate.deposit(TransferId::new("d").unwrap(), btc(), dec!(0.05))
        .unwrap();
    let mut service = MarginReservationService::on_ledger(
        v1(),
        AccountId::new("acct").unwrap(),
        VenueId::new("venue").unwrap(),
        hold(),
    );
    service.register(iid("BTC-INV"), instrument()).unwrap();
    (gate, service)
}

/// On the ledger: reserve 100 at 50000 (0.02 held), book and report the
/// fill, then move the mark. Collateral is available + hold + upnl with the
/// inverse upnl `basis - rhe(Q * CV / mark)`:
///
/// | mark | requirement | upnl | collateral | 1 more contract (+0.0002 / 0.00025 / 0.00016) |
/// |---|---|---|---|---|
/// | 40000 | 0.025 | 0.2 - 0.25 = -0.05 | 0 | rejected |
/// | 62500 | 0.016 | 0.2 - 0.16 = +0.04 | 0.09 | accepted |
#[test]
fn ledger_reservations_value_and_margin_inverse_positions() {
    let (mut gate, mut service) = on_ledger();
    assert_eq!(
        service
            .reserve_with_gate(buy("a", dec!(100)), &mut gate, &hold(), &marks(dec!(50000)))
            .unwrap(),
        MarginDecision::Reserved {
            required: dec!(0.02),
            reserved: dec!(0.02),
        }
    );
    assert_eq!(gate.hold(&hold()), dec!(0.02));
    gate.book_derivative_execution(
        &Execution::new(
            eid("f"),
            iid("BTC-INV"),
            Action::Buy,
            dec!(50000),
            dec!(100),
        )
        .with_executed_at(1),
    )
    .unwrap();
    assert_eq!(
        service.apply_booked_fill(&mut gate, &oid("a"), &eid("f"), &marks(dec!(50000))),
        Ok(BookedFill::Applied {
            contracts: dec!(100),
            remaining: dec!(0),
            overfill: None,
            required: dec!(0.02),
            hold: dec!(0.02),
        })
    );
    let state = service
        .requirement_with_gate(&gate, &marks(dec!(40000)))
        .unwrap();
    assert_eq!(state.total, dec!(0.025));
    assert_eq!(
        service
            .reserve_with_gate(buy("b", dec!(1)), &mut gate, &hold(), &marks(dec!(40000)))
            .unwrap(),
        MarginDecision::Rejected {
            reason: MarginRejection::InsufficientMargin {
                required: dec!(0.02525),
                collateral: dec!(0),
                shortfall: dec!(0.02525),
            },
        }
    );
    assert_eq!(
        service
            .reserve_with_gate(buy("c", dec!(1)), &mut gate, &hold(), &marks(dec!(62500)))
            .unwrap(),
        MarginDecision::Reserved {
            required: dec!(0.01616),
            reserved: dec!(0.00016),
        }
    );
    assert_eq!(gate.hold(&hold()), dec!(0.01616));
    gate.ledger().reconcile().unwrap();
}
