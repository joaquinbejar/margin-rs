//! The rule `scenario-pm/1`: figures written out by hand, the
//! `linear-im/1` equivalence, and the admission bound checked against a
//! brute force over every fill subset.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::{ExposurePosition, ExposureView, InitialMarginModel, ProductKind};
use margin_rs::liquidation::{
    CloseoutOutcome, CloseoutTarget, LiquidationAccount, LiquidationPolicy, LiquidityLevel,
    plan_liquidation,
};
use margin_rs::rules::{
    PmLeg, PmLegKind, PmOrder, ScenarioPm, ScenarioPmRates, ScenarioSets, TieredLinearIm,
};
use margin_rs::{AccountId, Asset, ContractSize, Decimal, InstrumentId, MarginError, VenueId};
use rust_decimal::RoundingStrategy;
use rust_decimal_macros::dec;

fn usdc() -> Asset {
    Asset::new("USDC").expect("asset")
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).expect("id")
}

fn acct() -> AccountId {
    AccountId::new("acct").expect("id")
}

fn venue() -> VenueId {
    VenueId::new("venue").expect("id")
}

const NO_FLOOR: ScenarioPmRates = ScenarioPmRates {
    short_option_floor_rate: Decimal::ZERO,
    linear_floor_rate: Decimal::ZERO,
};

type Scenario = BTreeMap<InstrumentId, Decimal>;

fn scenario(values: &[(&str, Decimal)]) -> Scenario {
    values.iter().map(|(id, v)| (iid(id), *v)).collect()
}

fn rule(scale: u32, rates: (ScenarioPmRates, ScenarioPmRates), sets: ScenarioSets) -> ScenarioPm {
    ScenarioPm::scenario_pm_v1(usdc(), scale, rates.0, rates.1, sets).expect("rule")
}

fn linear(id: &str, contracts: Decimal, mark: Decimal) -> PmLeg {
    PmLeg {
        instrument: iid(id),
        kind: PmLegKind::Linear,
        contracts,
        multiplier: Decimal::ONE,
        mark,
    }
}

fn option(id: &str, contracts: Decimal, premium: Decimal, underlying: Decimal) -> PmLeg {
    PmLeg {
        instrument: iid(id),
        kind: PmLegKind::Option {
            underlying_price: underlying,
        },
        contracts,
        multiplier: Decimal::ONE,
        mark: premium,
    }
}

fn exposure(contracts: Decimal, mark: Decimal) -> ExposurePosition {
    ExposurePosition {
        instrument: iid("P"),
        product: ProductKind::LinearPerpetual,
        settlement: usdc(),
        contracts,
        size: ContractSize::Multiplier(dec!(2)),
        mark,
    }
}

/// One linear position whose grid's extreme moves equal the rates gives
/// the `linear-im/1` figures, rounding included.
#[test]
fn test_single_linear_position_matches_linear_im() {
    for scale in [6, 2, 0] {
        let reference = TieredLinearIm::linear_v1(usdc(), scale).expect("rule");
        for (contracts, mark) in [
            (dec!(10), dec!(95)),
            (dec!(-3), dec!(33.333333)),
            (dec!(0.7), dec!(101.01)),
        ] {
            let at = |rate: Decimal| {
                vec![
                    scenario(&[("P", mark * (Decimal::ONE + rate))]),
                    scenario(&[("P", mark)]),
                    scenario(&[("P", mark * (Decimal::ONE - rate))]),
                ]
            };
            let pm = rule(
                scale,
                (NO_FLOOR, NO_FLOOR),
                ScenarioSets {
                    initial: at(dec!(0.10)),
                    maintenance: at(dec!(0.05)),
                },
            );
            let view = ExposureView {
                account: acct(),
                venue: venue(),
                positions: vec![exposure(contracts, mark)],
            };
            let expected = reference.requirement(&view).expect("linear");
            let got = pm.requirement(&view).expect("pm");
            assert_eq!(
                (got.initial, got.maintenance),
                (expected.initial, expected.maintenance),
                "{contracts} at {mark}, scale {scale}"
            );
            assert_eq!(got.rule.to_string(), "scenario-pm/1");
        }
    }
}

/// Long A 10 and short B 10 that move together net to zero; the linear
/// floor at 1 % charges the gross 2000.
#[test]
fn test_offsetting_linear_legs_net() {
    let sets = ScenarioSets {
        initial: vec![
            scenario(&[("A", dec!(90)), ("B", dec!(90))]),
            scenario(&[("A", dec!(110)), ("B", dec!(110))]),
        ],
        maintenance: vec![scenario(&[("A", dec!(95)), ("B", dec!(95))])],
    };
    let floor = ScenarioPmRates {
        short_option_floor_rate: Decimal::ZERO,
        linear_floor_rate: dec!(0.01),
    };
    let legs = [
        linear("A", dec!(10), dec!(100)),
        linear("B", dec!(-10), dec!(100)),
    ];
    let unfloored = rule(6, (NO_FLOOR, NO_FLOOR), sets.clone())
        .requirement_for(acct(), venue(), &legs)
        .expect("pm");
    assert_eq!(unfloored.requirement.initial, dec!(0));
    assert_eq!(unfloored.initial.leg_terms[0].1, dec!(100));
    assert_eq!(unfloored.initial.leg_terms[1].1, dec!(-100));
    let floored = rule(6, (floor, floor), sets)
        .requirement_for(acct(), venue(), &legs)
        .expect("pm");
    assert_eq!(floored.requirement.initial, dec!(20));
    assert_eq!(floored.initial.linear_floor, dec!(20));
    assert_eq!(floored.initial.scenario_loss, dec!(0));
}

/// Short 2 calls (premium 5) and long 1 put (premium 3), U = 100.
///
/// - s0, up: call 12, put 1: loss 2 * 7 = 14, put change -2 so +2: 16.
/// - s1, down: call 1, put 9: -8 + -6 = -14.
/// - s2, also 16: the tie takes s0.
///
/// F at 10 %: 0.1 * 2 * 1 * 100 = 20. Initial: 16 + 20 = 36. Maintenance
/// (s1 only, no floor): max(0, -14) = 0.
#[test]
fn test_options_reprice_short_loss_minus_long_gain() {
    let up = scenario(&[("C", dec!(12)), ("P", dec!(1))]);
    let down = scenario(&[("C", dec!(1)), ("P", dec!(9))]);
    let sets = ScenarioSets {
        initial: vec![up.clone(), down.clone(), up],
        maintenance: vec![down],
    };
    let f = ScenarioPmRates {
        short_option_floor_rate: dec!(0.10),
        linear_floor_rate: Decimal::ZERO,
    };
    let legs = [
        option("C", dec!(-2), dec!(5), dec!(100)),
        option("P", dec!(1), dec!(3), dec!(100)),
    ];
    let got = rule(6, (f, NO_FLOOR), sets)
        .requirement_for(acct(), venue(), &legs)
        .expect("pm");
    assert_eq!(got.initial.worst, 0);
    assert_eq!(
        got.initial.leg_terms,
        vec![(iid("C"), dec!(14)), (iid("P"), dec!(2))]
    );
    assert_eq!(got.initial.scenario_loss, dec!(16));
    assert_eq!(got.initial.short_option_floor, dec!(20));
    assert_eq!(got.requirement.initial, dec!(36));
    assert_eq!(got.requirement.maintenance, dec!(0));
    // The whole initial figure sits on the leg with the largest term.
    assert_eq!(got.requirement.positions[0].initial, dec!(36));
    assert_eq!(got.requirement.positions[1].initial, dec!(0));
}

/// One round-up per account: two legs of 0.0004 each round to 0.001, not
/// 0.002.
#[test]
fn test_one_round_up_per_account() {
    let sets = ScenarioSets {
        initial: vec![scenario(&[("A", dec!(0.9996)), ("B", dec!(0.9996))])],
        maintenance: vec![scenario(&[("A", dec!(1)), ("B", dec!(1))])],
    };
    let got = rule(3, (NO_FLOOR, NO_FLOOR), sets)
        .requirement_for(
            acct(),
            venue(),
            &[linear("A", dec!(1), dec!(1)), linear("B", dec!(1), dec!(1))],
        )
        .expect("pm");
    assert_eq!(got.requirement.initial, dec!(0.001));
}

#[test]
fn test_invalid_inputs_are_refused() {
    let sets = || ScenarioSets {
        initial: vec![scenario(&[("A", dec!(1))])],
        maintenance: vec![scenario(&[("A", dec!(1))])],
    };
    let bad_rate = ScenarioPmRates {
        short_option_floor_rate: dec!(1.1),
        linear_floor_rate: Decimal::ZERO,
    };
    assert!(ScenarioPm::scenario_pm_v1(usdc(), 6, bad_rate, NO_FLOOR, sets()).is_err());
    assert!(
        ScenarioPm::scenario_pm_v1(
            usdc(),
            6,
            NO_FLOOR,
            NO_FLOOR,
            ScenarioSets {
                initial: vec![],
                maintenance: vec![scenario(&[])],
            }
        )
        .is_err()
    );
    let pm = rule(6, (NO_FLOOR, NO_FLOOR), sets());
    assert!(matches!(
        pm.requirement_for(acct(), venue(), &[linear("Z", dec!(1), dec!(1))]),
        Err(MarginError::MissingMark { .. })
    ));
    assert!(matches!(
        pm.admission_requirement(
            acct(),
            venue(),
            &[linear("A", dec!(1), dec!(1))],
            &[PmOrder {
                instrument: iid("Z"),
                contracts: dec!(1),
                limit_price: dec!(1),
            }]
        ),
        Err(MarginError::InvalidInput {
            field: "orders",
            ..
        })
    ));
    let view = ExposureView {
        account: acct(),
        venue: venue(),
        positions: vec![ExposurePosition {
            product: ProductKind::InversePerpetual,
            size: ContractSize::ContractValue(dec!(100)),
            ..exposure(dec!(1), dec!(1))
        }],
    };
    assert!(matches!(
        pm.requirement(&view),
        Err(MarginError::UnsupportedInstrument { .. })
    ));
}

/// A small deterministic generator, so the property runs without a
/// dependency.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }

    fn signed(&mut self, n: u64) -> Decimal {
        Decimal::from(self.next(2 * n + 1)) - Decimal::from(n)
    }
}

/// The requirement after the fills of `subset`, by hand: each fill at its
/// limit, positions revalued from their marks.
fn brute(
    legs: &[PmLeg],
    orders: &[PmOrder],
    subset: u32,
    scenarios: &[Scenario],
    rates: ScenarioPmRates,
    scale: u32,
) -> Decimal {
    let filled: Vec<&PmOrder> = orders
        .iter()
        .enumerate()
        .filter(|(i, _)| subset & (1 << i) != 0)
        .map(|(_, o)| o)
        .collect();
    let mut worst = Decimal::ZERO;
    for s in scenarios {
        let mut r = Decimal::ZERO;
        for leg in legs {
            r -= leg.contracts * leg.multiplier * (s[&leg.instrument] - leg.mark);
        }
        for o in &filled {
            let leg = legs.iter().find(|l| l.instrument == o.instrument).unwrap();
            r -= o.contracts * leg.multiplier * (s[&o.instrument] - o.limit_price);
        }
        worst = worst.max(r);
    }
    let mut floors = Decimal::ZERO;
    for leg in legs {
        let after = leg.contracts
            + filled
                .iter()
                .filter(|o| o.instrument == leg.instrument)
                .map(|o| o.contracts)
                .sum::<Decimal>();
        match leg.kind {
            PmLegKind::Option { underlying_price } => {
                floors += rates.short_option_floor_rate
                    * (-after).max(Decimal::ZERO)
                    * leg.multiplier
                    * underlying_price;
            }
            PmLegKind::Linear => {
                floors += rates.linear_floor_rate * after.abs() * leg.multiplier * leg.mark;
            }
        }
    }
    (worst + floors).round_dp_with_strategy(scale, RoundingStrategy::AwayFromZero)
}

/// The admission figure is never below the requirement of any subset of
/// fills, and equals the plain requirement without orders.
#[test]
fn test_admission_bounds_every_fill_subset() {
    let mut rng = Lcg(7);
    let rates = ScenarioPmRates {
        short_option_floor_rate: dec!(0.05),
        linear_floor_rate: dec!(0.01),
    };
    for _ in 0..300 {
        let legs = vec![
            linear("L", rng.signed(5), Decimal::from(90 + rng.next(20))),
            option("O", rng.signed(5), Decimal::from(rng.next(10)), dec!(100)),
        ];
        let scenarios: Vec<Scenario> = (0..=rng.next(4))
            .map(|_| {
                scenario(&[
                    ("L", Decimal::from(80 + rng.next(40))),
                    ("O", Decimal::from(rng.next(20))),
                ])
            })
            .collect();
        let orders: Vec<PmOrder> = (0..rng.next(5))
            .map(|_| {
                let (instrument, limit) = if rng.next(2) == 0 {
                    ("L", Decimal::from(85 + rng.next(30)))
                } else {
                    ("O", Decimal::from(rng.next(15)))
                };
                let mut contracts = rng.signed(3);
                if contracts.is_zero() {
                    contracts = Decimal::ONE;
                }
                PmOrder {
                    instrument: iid(instrument),
                    contracts,
                    limit_price: limit,
                }
            })
            .collect();
        let pm = rule(
            2,
            (rates, rates),
            ScenarioSets {
                initial: scenarios.clone(),
                maintenance: scenarios.clone(),
            },
        );
        let admission = pm
            .admission_requirement(acct(), venue(), &legs, &orders)
            .expect("admission")
            .requirement
            .initial;
        let mut highest = Decimal::ZERO;
        for subset in 0..(1_u32 << orders.len()) {
            let b = brute(&legs, &orders, subset, &scenarios, rates, 2);
            assert!(admission >= b, "{admission} < {b} for {subset:b}");
            highest = highest.max(b);
        }
        let plain = pm
            .requirement_for(acct(), venue(), &legs)
            .expect("plain")
            .requirement
            .initial;
        assert_eq!(plain, brute(&legs, &[], 0, &scenarios, rates, 2));
        assert!(admission >= highest);
    }
}

/// The rule drives the closeout plan: a hedged pair needs only its net
/// leg reduced. A long 10, B short 6, scenarios +-5 % on both; mm = 5 %
/// of |net| * 100. Cash 10, A sold at 99: `10 - n >= 5 * |4 - n|`, n =
/// 2.5 at a step of 0.5.
#[test]
fn test_closeout_plan_under_scenario_pm() {
    let moves = |rate: Decimal| {
        vec![
            scenario(&[
                ("A", dec!(100) * (Decimal::ONE + rate)),
                ("B", dec!(100) * (Decimal::ONE + rate)),
            ]),
            scenario(&[
                ("A", dec!(100) * (Decimal::ONE - rate)),
                ("B", dec!(100) * (Decimal::ONE - rate)),
            ]),
        ]
    };
    let pm = rule(
        6,
        (NO_FLOOR, NO_FLOOR),
        ScenarioSets {
            initial: moves(dec!(0.10)),
            maintenance: moves(dec!(0.05)),
        },
    );
    let perp = |id: &str, contracts| ExposurePosition {
        instrument: iid(id),
        product: ProductKind::LinearPerpetual,
        settlement: usdc(),
        contracts,
        size: ContractSize::Multiplier(Decimal::ONE),
        mark: dec!(100),
    };
    let account = LiquidationAccount {
        view: ExposureView {
            account: acct(),
            venue: venue(),
            positions: vec![perp("A", dec!(10)), perp("B", dec!(-6))],
        },
        basis: BTreeMap::from([(iid("A"), dec!(1000)), (iid("B"), dec!(-600))]),
        cash: dec!(10),
        resting_orders: vec![],
    };
    let plan = plan_liquidation(
        &pm,
        &account,
        &BTreeMap::from([(
            iid("A"),
            vec![LiquidityLevel {
                price: dec!(99),
                contracts: dec!(100),
            }],
        )]),
        &LiquidationPolicy {
            target: CloseoutTarget::Maintenance,
            fee_rate: Decimal::ZERO,
            fee_rates: BTreeMap::new(),
            loss_budget: None,
            contract_step: dec!(0.5),
        },
    )
    .expect("plan");
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    assert_eq!(plan.reductions[0].contracts, dec!(2.5));
    assert_eq!(plan.after.maintenance, dec!(7.5));
}

/// Short 10 CALL (premium mark 5, basis -50) hedged by 3 PERP long (mark
/// 100, basis 300), cash 60, underlying 100. Scenarios: flat, and up
/// (PERP 110, CALL 12): `R_up = -3 * 10 + 10 * 7 = 40`, so mm 40.
/// Collateral counts min(-50, 0): 10.
///
/// Buying back `n` CALL at 5.5: collateral 10 - 0.5n against
/// 40 - 7n, met at n = 5 (7.5 against 5).
#[test]
fn test_closeout_plan_over_options_under_scenario_pm() {
    let up = scenario(&[("PERP", dec!(110)), ("CALL", dec!(12))]);
    let flat = scenario(&[("PERP", dec!(100)), ("CALL", dec!(5))]);
    let pm = rule(
        6,
        (NO_FLOOR, NO_FLOOR),
        ScenarioSets {
            initial: vec![flat.clone(), up.clone()],
            maintenance: vec![flat, up],
        },
    );
    let call = ExposurePosition {
        instrument: iid("CALL"),
        product: ProductKind::Option,
        settlement: usdc(),
        contracts: dec!(-10),
        size: ContractSize::Multiplier(Decimal::ONE),
        mark: dec!(5),
    };
    let perp = ExposurePosition {
        instrument: iid("PERP"),
        product: ProductKind::LinearPerpetual,
        settlement: usdc(),
        contracts: dec!(3),
        size: ContractSize::Multiplier(Decimal::ONE),
        mark: dec!(100),
    };
    let account = LiquidationAccount {
        view: ExposureView {
            account: acct(),
            venue: venue(),
            positions: vec![call, perp],
        },
        basis: BTreeMap::from([(iid("CALL"), dec!(-50)), (iid("PERP"), dec!(300))]),
        cash: dec!(60),
        resting_orders: vec![],
    };
    let liquidity = BTreeMap::from([(
        iid("CALL"),
        vec![LiquidityLevel {
            price: dec!(5.5),
            contracts: dec!(100),
        }],
    )]);
    let policy = LiquidationPolicy {
        target: CloseoutTarget::Maintenance,
        fee_rate: Decimal::ZERO,
        fee_rates: BTreeMap::new(),
        loss_budget: None,
        contract_step: Decimal::ONE,
    };
    // Without the underlying price the option cannot be margined.
    assert!(matches!(
        plan_liquidation(&pm, &account, &liquidity, &policy),
        Err(MarginError::MissingMark { .. })
    ));
    let pm = pm
        .with_underlying_prices(BTreeMap::from([(iid("CALL"), dec!(100))]))
        .expect("prices");
    let plan = plan_liquidation(&pm, &account, &liquidity, &policy).expect("plan");
    assert_eq!(plan.before.collateral, dec!(10));
    assert_eq!(plan.before.maintenance, dec!(40));
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    assert_eq!(plan.reductions.len(), 1);
    assert_eq!(plan.reductions[0].instrument, iid("CALL"));
    assert_eq!(plan.reductions[0].contracts, dec!(5));
    assert_eq!(plan.reductions[0].fill_cash, dec!(-27.5));
    assert_eq!(plan.after.cash, dec!(32.5));
    assert_eq!(plan.after.collateral, dec!(7.5));
    assert_eq!(plan.after.maintenance, dec!(5));
    assert!(
        pm.clone()
            .with_underlying_prices(BTreeMap::from([(iid("CALL"), Decimal::ZERO)]))
            .is_err()
    );
}

// ---- scenario-pm/2 ----

fn rule_v2(
    scale: u32,
    rates: (ScenarioPmRates, ScenarioPmRates),
    sets: ScenarioSets,
) -> ScenarioPm {
    ScenarioPm::scenario_pm_v2(usdc(), scale, rates.0, rates.1, sets).expect("rule")
}

fn spread_floor(im: Decimal, mm: Decimal) -> (ScenarioPmRates, ScenarioPmRates) {
    let at = |g| ScenarioPmRates {
        short_option_floor_rate: Decimal::ZERO,
        linear_floor_rate: g,
    };
    (at(im), at(mm))
}

/// A single linear position pays no spread floor: with a floor of 1 % /
/// 0.5 % it still gives the `linear-im/1` figures, rounding included.
#[test]
fn test_v2_single_linear_position_matches_linear_im_with_a_floor() {
    for scale in [6, 2, 0] {
        let reference = TieredLinearIm::linear_v1(usdc(), scale).expect("rule");
        for (contracts, mark) in [(dec!(10), dec!(95)), (dec!(-3), dec!(33.333333))] {
            let at = |rate: Decimal| {
                vec![
                    scenario(&[("P", mark * (Decimal::ONE + rate))]),
                    scenario(&[("P", mark * (Decimal::ONE - rate))]),
                ]
            };
            let pm = rule_v2(
                scale,
                spread_floor(dec!(0.01), dec!(0.005)),
                ScenarioSets {
                    initial: at(dec!(0.10)),
                    maintenance: at(dec!(0.05)),
                },
            );
            let view = ExposureView {
                account: acct(),
                venue: venue(),
                positions: vec![exposure(contracts, mark)],
            };
            let expected = reference.requirement(&view).expect("linear");
            let got = pm.requirement(&view).expect("pm");
            assert_eq!(
                (got.initial, got.maintenance),
                (expected.initial, expected.maintenance)
            );
            assert_eq!(got.rule.to_string(), "scenario-pm/2");
        }
    }
}

/// Long A 10 and short B 10 at 100 move together: no scenario loss, and
/// the spread floor charges `g * 2 * 1000`: 20 at 1 % (im), 10 at 0.5 %
/// (mm), so im > mm. Version 1 would charge the same here (gross 2000),
/// but a single leg shows the difference: version 2 charges it nothing.
#[test]
fn test_v2_calendar_spread_pays_the_spread_floor() {
    let sets = ScenarioSets {
        initial: vec![
            scenario(&[("A", dec!(90)), ("B", dec!(90))]),
            scenario(&[("A", dec!(110)), ("B", dec!(110))]),
        ],
        maintenance: vec![scenario(&[("A", dec!(95)), ("B", dec!(95))])],
    };
    let legs = [
        linear("A", dec!(10), dec!(100)),
        linear("B", dec!(-10), dec!(100)),
    ];
    let got = rule_v2(6, spread_floor(dec!(0.01), dec!(0.005)), sets.clone())
        .requirement_for(acct(), venue(), &legs)
        .expect("pm");
    assert_eq!(got.requirement.initial, dec!(20));
    assert_eq!(got.requirement.maintenance, dec!(10));
    assert_eq!(got.initial.linear_floor, dec!(20));
    let one_leg = rule_v2(6, spread_floor(dec!(0.01), dec!(0.005)), sets)
        .requirement_for(acct(), venue(), &legs[..1])
        .expect("pm");
    assert_eq!(one_leg.initial.linear_floor, dec!(0));
}

/// A long option alone requires nothing: its full value only lowers
/// `R_s`.
#[test]
fn test_v2_long_option_alone_requires_nothing() {
    let sets = ScenarioSets {
        initial: vec![scenario(&[("C", dec!(1))]), scenario(&[("C", dec!(12))])],
        maintenance: vec![scenario(&[("C", dec!(1))])],
    };
    let f = ScenarioPmRates {
        short_option_floor_rate: dec!(0.01),
        linear_floor_rate: dec!(0.01),
    };
    let got = rule_v2(6, (f, f), sets)
        .requirement_for(acct(), venue(), &[option("C", dec!(5), dec!(3), dec!(100))])
        .expect("pm");
    assert_eq!(got.requirement.initial, dec!(0));
    assert_eq!(got.requirement.maintenance, dec!(0));
    // Worst scenario s0: -5 * 1.
    assert_eq!(got.initial.leg_terms, vec![(iid("C"), dec!(-5))]);
}

/// The fixture of the version 1 option test under version 2: the long put
/// counts its full value. Up: 2 * 7 - 1 * 1 = 13; down: -8 - 9 = -17; the
/// tie (s0 and s2) takes s0. With F = 20: im 33 (36 under version 1).
#[test]
fn test_v2_long_option_at_full_scenario_value() {
    let up = scenario(&[("C", dec!(12)), ("P", dec!(1))]);
    let down = scenario(&[("C", dec!(1)), ("P", dec!(9))]);
    let sets = ScenarioSets {
        initial: vec![up.clone(), down.clone(), up],
        maintenance: vec![down],
    };
    let f = ScenarioPmRates {
        short_option_floor_rate: dec!(0.10),
        linear_floor_rate: Decimal::ZERO,
    };
    let legs = [
        option("C", dec!(-2), dec!(5), dec!(100)),
        option("P", dec!(1), dec!(3), dec!(100)),
    ];
    let got = rule_v2(6, (f, NO_FLOOR), sets.clone())
        .requirement_for(acct(), venue(), &legs)
        .expect("pm");
    assert_eq!(got.initial.worst, 0);
    assert_eq!(
        got.initial.leg_terms,
        vec![(iid("C"), dec!(14)), (iid("P"), dec!(-1))]
    );
    assert_eq!(got.requirement.initial, dec!(33));
    let v1 = rule(6, (f, NO_FLOOR), sets)
        .requirement_for(acct(), venue(), &legs)
        .expect("pm");
    assert_eq!(v1.requirement.initial, dec!(36));
}

/// The version 2 figure after the fills of `subset`, by hand, as the
/// admission rule defines it: each scenario's `R_s` over the current legs
/// plus every filled order's loss `-q * M * (X_s - L)`; both floors on the
/// positions after the fills.
fn brute_v2(
    legs: &[PmLeg],
    orders: &[PmOrder],
    subset: u32,
    scenarios: &[Scenario],
    rates: ScenarioPmRates,
    scale: u32,
) -> Decimal {
    let filled: Vec<&PmOrder> = orders
        .iter()
        .enumerate()
        .filter(|(i, _)| subset & (1 << i) != 0)
        .map(|(_, o)| o)
        .collect();
    let mut worst = Decimal::ZERO;
    for s in scenarios {
        let mut r = Decimal::ZERO;
        for leg in legs {
            let x = s[&leg.instrument];
            r -= leg.contracts * leg.multiplier * x;
            let long_option =
                matches!(leg.kind, PmLegKind::Option { .. }) && leg.contracts > Decimal::ZERO;
            if !long_option {
                r += leg.contracts * leg.multiplier * leg.mark;
            }
        }
        for o in &filled {
            let leg = legs.iter().find(|l| l.instrument == o.instrument).unwrap();
            r -= o.contracts * leg.multiplier * (s[&o.instrument] - o.limit_price);
        }
        worst = worst.max(r);
    }
    let (mut floors, mut gross, mut net) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
    for leg in legs {
        let q = leg.contracts
            + filled
                .iter()
                .filter(|o| o.instrument == leg.instrument)
                .map(|o| o.contracts)
                .sum::<Decimal>();
        match leg.kind {
            PmLegKind::Option { underlying_price } => {
                floors += rates.short_option_floor_rate
                    * (-q).max(Decimal::ZERO)
                    * leg.multiplier
                    * underlying_price;
            }
            PmLegKind::Linear => {
                gross += q.abs() * leg.multiplier * leg.mark;
                net += q * leg.multiplier * leg.mark;
            }
        }
    }
    floors += rates.linear_floor_rate * (gross - net.abs());
    (worst + floors).round_dp_with_strategy(scale, RoundingStrategy::AwayFromZero)
}

/// The version 2 admission figure is never below the figure of any fill
/// subset, floors included; with both floors at zero it equals the largest
/// one; without orders it equals the plain figure.
#[test]
fn test_v2_admission_bounds_every_fill_subset() {
    let mut rng = Lcg(11);
    let rates = ScenarioPmRates {
        short_option_floor_rate: dec!(0.05),
        linear_floor_rate: dec!(0.02),
    };
    for _ in 0..300 {
        let legs = vec![
            linear("L", rng.signed(5), Decimal::from(90 + rng.next(20))),
            linear("M", rng.signed(5), Decimal::from(90 + rng.next(20))),
            option("O", rng.signed(5), Decimal::from(rng.next(10)), dec!(100)),
        ];
        let scenarios: Vec<Scenario> = (0..=rng.next(4))
            .map(|_| {
                scenario(&[
                    ("L", Decimal::from(80 + rng.next(40))),
                    ("M", Decimal::from(80 + rng.next(40))),
                    ("O", Decimal::from(rng.next(20))),
                ])
            })
            .collect();
        let orders: Vec<PmOrder> = (0..rng.next(5))
            .map(|_| {
                let (instrument, limit) = match rng.next(3) {
                    0 => ("L", Decimal::from(85 + rng.next(30))),
                    1 => ("M", Decimal::from(85 + rng.next(30))),
                    _ => ("O", Decimal::from(rng.next(15))),
                };
                let mut contracts = rng.signed(3);
                if contracts.is_zero() {
                    contracts = Decimal::ONE;
                }
                PmOrder {
                    instrument: iid(instrument),
                    contracts,
                    limit_price: limit,
                }
            })
            .collect();
        let pm = rule_v2(
            2,
            (rates, rates),
            ScenarioSets {
                initial: scenarios.clone(),
                maintenance: scenarios.clone(),
            },
        );
        let admission = pm
            .admission_requirement(acct(), venue(), &legs, &orders)
            .expect("admission")
            .requirement
            .initial;
        for subset in 0..(1_u32 << orders.len()) {
            let b = brute_v2(&legs, &orders, subset, &scenarios, rates, 2);
            assert!(admission >= b, "{admission} < {b} for {subset:b}");
        }
        let unfloored = rule_v2(
            2,
            (NO_FLOOR, NO_FLOOR),
            ScenarioSets {
                initial: scenarios.clone(),
                maintenance: scenarios.clone(),
            },
        )
        .admission_requirement(acct(), venue(), &legs, &orders)
        .expect("admission")
        .requirement
        .initial;
        let highest = (0..(1_u32 << orders.len()))
            .map(|subset| brute_v2(&legs, &orders, subset, &scenarios, NO_FLOOR, 2))
            .max()
            .unwrap();
        assert_eq!(unfloored, highest);
        let plain = pm
            .requirement_for(acct(), venue(), &legs)
            .expect("plain")
            .requirement
            .initial;
        assert_eq!(plain, brute_v2(&legs, &[], 0, &scenarios, rates, 2));
    }
}

/// A maker short 10000 calls (premium mark 5, underlying 100) under
/// scenario-pm/2, cash 81000: collateral 31000 against `R_up = 7 * 10000`.
/// Bought back at 5.5, `81000 - 50000 - 0.5n >= 7 (10000 - n)` from
/// n = 6000: more counts than the walk's budget, so the plan gallops and
/// bisects to 6000 within the budget.
#[test]
fn test_v2_large_option_position_plans_within_the_budget() {
    let up = scenario(&[("CALL", dec!(12))]);
    let flat = scenario(&[("CALL", dec!(5))]);
    let pm = rule_v2(
        6,
        (NO_FLOOR, NO_FLOOR),
        ScenarioSets {
            initial: vec![flat.clone(), up.clone()],
            maintenance: vec![flat, up],
        },
    )
    .with_underlying_prices(BTreeMap::from([(iid("CALL"), dec!(100))]))
    .expect("prices");
    let account = LiquidationAccount {
        view: ExposureView {
            account: acct(),
            venue: venue(),
            positions: vec![ExposurePosition {
                instrument: iid("CALL"),
                product: ProductKind::Option,
                settlement: usdc(),
                contracts: dec!(-10000),
                size: ContractSize::Multiplier(Decimal::ONE),
                mark: dec!(5),
            }],
        },
        basis: BTreeMap::from([(iid("CALL"), dec!(-50000))]),
        cash: dec!(81000),
        resting_orders: vec![],
    };
    let plan = plan_liquidation(
        &pm,
        &account,
        &BTreeMap::from([(
            iid("CALL"),
            vec![LiquidityLevel {
                price: dec!(5.5),
                contracts: dec!(20000),
            }],
        )]),
        &LiquidationPolicy {
            target: CloseoutTarget::Maintenance,
            fee_rate: Decimal::ZERO,
            fee_rates: BTreeMap::new(),
            loss_budget: None,
            contract_step: Decimal::ONE,
        },
    )
    .expect("plan within the budget");
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    assert_eq!(plan.reductions[0].contracts, dec!(6000));
    assert!(!plan.minimal);
}
