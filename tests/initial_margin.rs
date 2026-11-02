//! Initial margin fixtures (M3-03B), hand calculated.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, ProductKind, RuleId, TierSlice,
};
use margin_rs::rules::{MarginTier, TieredLinearIm};
use margin_rs::{AccountId, Asset, Decimal, InstrumentId, MarginError, VenueId};
use rust_decimal_macros::dec;

fn usdc() -> Asset {
    Asset::new("USDC").expect("asset")
}

fn linear() -> TieredLinearIm {
    TieredLinearIm::linear_v1(usdc(), 6).expect("rule")
}

fn position(
    id: &str,
    product: ProductKind,
    contracts: Decimal,
    multiplier: Decimal,
    mark: Decimal,
) -> ExposurePosition {
    ExposurePosition {
        instrument: InstrumentId::new(id).expect("id"),
        product,
        settlement: usdc(),
        contracts,
        multiplier,
        mark,
    }
}

fn view(positions: Vec<ExposurePosition>) -> ExposureView {
    ExposureView {
        account: AccountId::new("acct-1").expect("id"),
        venue: VenueId::new("venue").expect("id"),
        positions,
    }
}

/// Synthetic two tier table (not any venue's rule): 5 % / 2.5 % below
/// 50000 USDC of notional, 10 % / 5 % above.
fn synthetic_two_tier() -> TieredLinearIm {
    TieredLinearIm::new(
        RuleId::new("synthetic-two-tier", 1),
        usdc(),
        6,
        vec![
            MarginTier {
                floor: dec!(0),
                initial_rate: dec!(0.05),
                maintenance_rate: dec!(0.025),
            },
            MarginTier {
                floor: dec!(50000),
                initial_rate: dec!(0.10),
                maintenance_rate: dec!(0.05),
            },
        ],
    )
    .expect("rule")
}

/// The rule `linear-im/1`: im = 10 % and mm = 5 % of
/// |Qc| * M * mark.
///
/// Long 3 perpetual contracts, M = 10, mark 2000.5: notional 60015,
/// im 6001.5, mm 3000.75. The short of the same size needs the same.
#[test]
fn linear_v1_long_and_short() {
    let rule = linear();
    assert_eq!(rule.rule(), &RuleId::new("linear-im", 1));
    assert_eq!(rule.rule().to_string(), "linear-im/1");
    for contracts in [dec!(3), dec!(-3)] {
        let requirement = rule
            .requirement(&view(vec![position(
                "XYZ-PERP",
                ProductKind::LinearPerpetual,
                contracts,
                dec!(10),
                dec!(2000.5),
            )]))
            .expect("requirement");
        assert_eq!(requirement.rule.to_string(), "linear-im/1");
        assert_eq!(requirement.currency, usdc());
        assert_eq!(requirement.initial, dec!(6001.5));
        assert_eq!(requirement.maintenance, dec!(3000.75));
        let line = &requirement.positions[0];
        assert_eq!(line.contracts, contracts);
        assert_eq!(line.notional, dec!(60015));
        assert_eq!(
            line.slices,
            vec![TierSlice {
                tier: 0,
                notional: dec!(60015),
                initial_rate: dec!(0.10),
                maintenance_rate: dec!(0.05),
            }]
        );
    }
}

/// Positions are margined gross, without netting between instruments.
///
/// Long 1 future (M 10, mark 2000) and short 1 perpetual (M 10, mark 2001):
/// notionals 20000 and 20010, im 2000 + 2001 = 4001 even though the
/// directions offset.
#[test]
fn positions_are_not_netted() {
    let requirement = linear()
        .requirement(&view(vec![
            position(
                "XYZ-FUT-DEC",
                ProductKind::LinearFuture,
                dec!(1),
                dec!(10),
                dec!(2000),
            ),
            position(
                "XYZ-PERP",
                ProductKind::LinearPerpetual,
                dec!(-1),
                dec!(10),
                dec!(2001),
            ),
        ]))
        .expect("requirement");
    assert_eq!(requirement.initial, dec!(4001));
    assert_eq!(requirement.maintenance, dec!(2000.5));
    let lines: Vec<Decimal> = requirement.positions.iter().map(|p| p.initial).collect();
    assert_eq!(lines, vec![dec!(2000), dec!(2001)]);
}

/// Requirements round up once per position, and the total is the exact sum
/// of the rounded positions.
///
/// 0.0000015 notional: im 1.5e-7 rounds up to 0.000001, mm 7.5e-8 to
/// 0.000001. Two such positions total 0.000002.
#[test]
fn requirements_round_up_per_position() {
    let requirement = linear()
        .requirement(&view(vec![
            position(
                "A-PERP",
                ProductKind::LinearPerpetual,
                dec!(1),
                dec!(1),
                dec!(0.0000015),
            ),
            position(
                "B-PERP",
                ProductKind::LinearPerpetual,
                dec!(1),
                dec!(1),
                dec!(0.0000015),
            ),
        ]))
        .expect("requirement");
    assert_eq!(requirement.positions[0].initial, dec!(0.000001));
    assert_eq!(requirement.positions[0].maintenance, dec!(0.000001));
    assert_eq!(requirement.initial, dec!(0.000002));
    assert_eq!(requirement.maintenance, dec!(0.000002));
}

/// Tier boundaries of the synthetic table (progressive bands).
///
/// | notional | slices | im | mm |
/// |---|---|---|---|
/// | 40000 | 40000 @ 5 % | 2000 | 1000 |
/// | 50000 | 50000 @ 5 % (boundary, nothing in tier 2) | 2500 | 1250 |
/// | 50000.01 | 50000 @ 5 % + 0.01 @ 10 % | 2500.001 | 1250.0005 |
/// | 80000 | 50000 @ 5 % + 30000 @ 10 % | 5500 | 2750 |
#[test]
fn tier_boundaries() {
    let rule = synthetic_two_tier();
    let cases = [
        (dec!(40000), 1, dec!(2000), dec!(1000)),
        (dec!(50000), 1, dec!(2500), dec!(1250)),
        (dec!(50000.01), 2, dec!(2500.001), dec!(1250.0005)),
        (dec!(80000), 2, dec!(5500), dec!(2750)),
    ];
    for (notional, slices, initial, maintenance) in cases {
        let requirement = rule
            .requirement(&view(vec![position(
                "XYZ-PERP",
                ProductKind::LinearPerpetual,
                dec!(-1),
                dec!(1),
                notional,
            )]))
            .expect("requirement");
        let line = &requirement.positions[0];
        assert_eq!(line.notional, notional);
        assert_eq!(line.slices.len(), slices, "{notional}");
        assert_eq!(requirement.initial, initial, "{notional}");
        assert_eq!(requirement.maintenance, maintenance, "{notional}");
        assert_eq!(requirement.rule.to_string(), "synthetic-two-tier/1");
    }
    let line = rule
        .requirement(&view(vec![position(
            "XYZ-PERP",
            ProductKind::LinearPerpetual,
            dec!(1),
            dec!(1),
            dec!(80000),
        )]))
        .expect("requirement")
        .positions
        .remove(0);
    assert_eq!(
        line.slices,
        vec![
            TierSlice {
                tier: 0,
                notional: dec!(50000),
                initial_rate: dec!(0.05),
                maintenance_rate: dec!(0.025),
            },
            TierSlice {
                tier: 1,
                notional: dec!(30000),
                initial_rate: dec!(0.10),
                maintenance_rate: dec!(0.05),
            },
        ]
    );
}

/// A flat position needs nothing; an empty view needs nothing.
#[test]
fn flat_and_empty() {
    let requirement = linear()
        .requirement(&view(vec![position(
            "XYZ-PERP",
            ProductKind::LinearPerpetual,
            dec!(0),
            dec!(10),
            dec!(2000),
        )]))
        .expect("requirement");
    assert_eq!(
        (requirement.initial, requirement.maintenance),
        (dec!(0), dec!(0))
    );
    assert_eq!(
        requirement.positions[0].slices,
        [] as [margin_rs::initial_margin::TierSlice; 0]
    );
    let empty = linear().requirement(&view(vec![])).expect("requirement");
    assert_eq!((empty.initial, empty.maintenance), (dec!(0), dec!(0)));
}

/// Each view is one account at one venue; requirements for two accounts are
/// computed separately and never combined.
#[test]
fn no_netting_across_accounts_or_venues() {
    let rule = linear();
    let long = position(
        "XYZ-PERP",
        ProductKind::LinearPerpetual,
        dec!(1),
        dec!(10),
        dec!(2000),
    );
    let short = position(
        "XYZ-PERP",
        ProductKind::LinearPerpetual,
        dec!(-1),
        dec!(10),
        dec!(2000),
    );
    let a = rule.requirement(&view(vec![long])).expect("a");
    let b = rule
        .requirement(&ExposureView {
            account: AccountId::new("acct-2").expect("id"),
            venue: VenueId::new("other-venue").expect("id"),
            positions: vec![short.clone()],
        })
        .expect("b");
    assert_eq!((a.initial, b.initial), (dec!(2000), dec!(2000)));
    assert_eq!(b.account.as_str(), "acct-2");
    assert_eq!(b.venue.as_str(), "other-venue");
    // Two positions in one instrument in a single view are ambiguous.
    assert_eq!(
        rule.requirement(&view(vec![
            position(
                "XYZ-PERP",
                ProductKind::LinearPerpetual,
                dec!(1),
                dec!(10),
                dec!(2000)
            ),
            short,
        ])),
        Err(MarginError::invalid_input(
            "positions",
            "at most one position per instrument"
        ))
    );
}

/// Products and currencies outside the rule fail with typed errors.
#[test]
fn unsupported_inputs() {
    let rule = linear();
    for product in [ProductKind::Spot, ProductKind::Option] {
        assert_eq!(
            rule.requirement(&view(vec![position(
                "XYZ",
                product,
                dec!(1),
                dec!(1),
                dec!(100)
            )])),
            Err(MarginError::UnsupportedInstrument {
                rule: "linear-im/1".into(),
                instrument: InstrumentId::new("XYZ").expect("id"),
                reason: "only linear futures and perpetuals are covered",
            })
        );
    }
    let mut usdt = position(
        "XYZ-PERP",
        ProductKind::LinearPerpetual,
        dec!(1),
        dec!(1),
        dec!(100),
    );
    usdt.settlement = Asset::new("USDT").expect("asset");
    assert_eq!(
        rule.requirement(&view(vec![usdt])),
        Err(MarginError::CurrencyMismatch {
            field: "settlement",
            expected: usdc(),
            found: Asset::new("USDT").expect("asset"),
        })
    );
    assert!(
        rule.requirement(&view(vec![position(
            "P",
            ProductKind::LinearPerpetual,
            dec!(1),
            dec!(0),
            dec!(100)
        )]))
        .is_err()
    );
    assert!(
        rule.requirement(&view(vec![position(
            "P",
            ProductKind::LinearPerpetual,
            dec!(1),
            dec!(1),
            dec!(-1)
        )]))
        .is_err()
    );
    assert_eq!(
        rule.requirement(&view(vec![position(
            "P",
            ProductKind::LinearPerpetual,
            Decimal::MAX,
            dec!(10),
            dec!(10)
        )])),
        Err(MarginError::arithmetic("|Qc| * M * mark"))
    );
}

/// Tier tables are validated.
#[test]
fn tier_table_validation() {
    let tier = |floor, im, mm| MarginTier {
        floor,
        initial_rate: im,
        maintenance_rate: mm,
    };
    let make = |tiers| TieredLinearIm::new(RuleId::new("t", 1), usdc(), 6, tiers);
    assert!(make(vec![]).is_err());
    assert!(make(vec![tier(dec!(1), dec!(0.1), dec!(0.05))]).is_err());
    assert!(make(vec![tier(dec!(0), dec!(0.1), dec!(0.2))]).is_err());
    assert!(make(vec![tier(dec!(0), dec!(1.1), dec!(0.5))]).is_err());
    assert!(make(vec![tier(dec!(0), dec!(0.1), dec!(0))]).is_err());
    assert!(
        make(vec![
            tier(dec!(0), dec!(0.1), dec!(0.05)),
            tier(dec!(0), dec!(0.2), dec!(0.1))
        ])
        .is_err()
    );
    assert!(
        make(vec![
            tier(dec!(0), dec!(0.2), dec!(0.1)),
            tier(dec!(10), dec!(0.1), dec!(0.05))
        ])
        .is_err()
    );
    assert!(
        make(vec![
            tier(dec!(0), dec!(0.1), dec!(0.05)),
            tier(dec!(10), dec!(0.2), dec!(0.1))
        ])
        .is_ok()
    );
    assert!(
        TieredLinearIm::new(
            RuleId::new("t", 1),
            usdc(),
            19,
            vec![tier(dec!(0), dec!(0.1), dec!(0.05))]
        )
        .is_err()
    );
}

/// The worked numbers of the risk-reducing exception: an open position with IM 100, halved to
/// IM 50. Under `linear-im/1` that is 10 contracts at M = 1 and
/// mark 100 (notional 1000), then 5 contracts.
#[test]
fn primary_specification_example() {
    let rule = linear();
    let im = |contracts| {
        rule.requirement(&view(vec![position(
            "XYZ-PERP",
            ProductKind::LinearPerpetual,
            contracts,
            dec!(1),
            dec!(100),
        )]))
        .expect("requirement")
        .initial
    };
    assert_eq!(im(dec!(10)), dec!(100));
    assert_eq!(im(dec!(5)), dec!(50));
}

/// M303B-C1: only the full notional must be representable.
///
/// Qc = MAX, M = 2, mark = 0.1: |Qc| * M overflows but the notional is
/// MAX / 5 = 15845632502852867518708790067, im 1584563250285286751870879006.7.
/// Qc = 0.1, M = 1e-28, mark = 10: 0.1 * 1e-28 needs 29 digits, but the
/// notional is 1e-28, and im and mm round up to 0.000001.
#[test]
fn intermediate_products_need_not_be_representable() {
    let rule = linear();
    let big = rule
        .requirement(&view(vec![position(
            "BIG-PERP",
            ProductKind::LinearPerpetual,
            Decimal::MAX,
            dec!(2),
            dec!(0.1),
        )]))
        .expect("representable notional");
    assert_eq!(
        big.positions[0].notional,
        dec!(15845632502852867518708790067)
    );
    assert_eq!(big.initial, dec!(1584563250285286751870879006.7));
    let tiny = rule
        .requirement(&view(vec![position(
            "TINY-PERP",
            ProductKind::LinearPerpetual,
            dec!(0.1),
            Decimal::new(1, 28),
            dec!(10),
        )]))
        .expect("representable notional");
    assert_eq!(tiny.positions[0].notional, Decimal::new(1, 28));
    assert_eq!(
        (tiny.initial, tiny.maintenance),
        (dec!(0.000001), dec!(0.000001))
    );
}
