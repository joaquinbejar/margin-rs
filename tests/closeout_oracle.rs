//! The closeout search against an exhaustive oracle (#26, part C).
//!
//! The planner finds the smallest count on each liquidity level that meets
//! the target with a search that relies on the concavity of the excess in
//! the count. Rounding (half-even fill values and released basis, a
//! truncated fee) makes the functions step at single quanta, so this file
//! checks the search against an independent oracle that tries **every**
//! count on the grid, on small grids: one to three levels, one and two
//! tiers, with and without fees, long and short, and cash swept across the
//! target.
//!
//! The oracle reimplements the cash conventions of `docs/liquidation.md`,
//! linear and inverse, with plain `Decimal` arithmetic (the values here are
//! small enough for each figure to round exactly) and takes the margins
//! from the rule.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // The oracle is deliberately written with plain Decimal operators, on
    // values far from any overflow, so that it shares no code with the
    // exact arithmetic it checks.
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, PiecewiseLinearMargin, ProductKind, RuleId,
};
use margin_rs::liquidation::{
    CloseoutTarget, LiquidationAccount, LiquidationPolicy, LiquidityLevel, plan_liquidation,
};
use margin_rs::rules::{MarginTier, TieredInverseIm, TieredLinearIm};
use margin_rs::{AccountId, Asset, ContractSize, Decimal, InstrumentId, VenueId};
use rust_decimal::RoundingStrategy;
use rust_decimal_macros::dec;

fn asset() -> Asset {
    Asset::new("USD").unwrap()
}

fn btc() -> Asset {
    Asset::new("BTC").unwrap()
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}

fn table(tiers: usize, floor: Decimal) -> Vec<MarginTier> {
    let first = MarginTier {
        floor: dec!(0),
        initial_rate: dec!(0.05),
        maintenance_rate: dec!(0.025),
    };
    let rest = MarginTier {
        floor,
        initial_rate: dec!(0.10),
        maintenance_rate: dec!(0.05),
    };
    if tiers == 1 {
        vec![MarginTier {
            floor: dec!(0),
            ..rest
        }]
    } else {
        vec![first, rest]
    }
}

/// One or two tiers (the second from a notional of 500), cash at 2
/// decimals.
fn rule(tiers: usize) -> TieredLinearIm {
    TieredLinearIm::new(
        RuleId::new("oracle", 1),
        asset(),
        2,
        table(tiers, dec!(500)),
    )
    .unwrap()
}

/// One or two tiers (the second from 0.01 BTC), in BTC at 8 decimals.
fn inverse_rule(tiers: usize) -> TieredInverseIm {
    TieredInverseIm::new(
        RuleId::new("oracle-inverse", 1),
        btc(),
        8,
        table(tiers, dec!(0.01)),
    )
    .unwrap()
}

#[derive(Debug, Clone)]
struct Open {
    id: InstrumentId,
    contracts: Decimal,
    basis: Decimal,
    mark: Decimal,
    size: ContractSize,
}

#[derive(Debug, Clone)]
struct State {
    /// The cash quantum (2 for USD, 8 for BTC).
    scale: u32,
    currency: Asset,
    cash: Decimal,
    positions: Vec<Open>,
}

impl State {
    fn rhe(&self, x: Decimal) -> Decimal {
        x.round_dp_with_strategy(self.scale, RoundingStrategy::MidpointNearestEven)
    }

    fn trunc(&self, x: Decimal) -> Decimal {
        x.round_dp_with_strategy(self.scale, RoundingStrategy::ToZero)
    }

    /// rhe(n * M * p), or rhe(n * CV / p) for an inverse contract.
    fn value(&self, size: ContractSize, n: Decimal, price: Decimal) -> Decimal {
        match size {
            ContractSize::Multiplier(m) => self.rhe(n * m * price),
            ContractSize::ContractValue(cv) => self.rhe(n * cv / price),
        }
    }

    /// Linear: value - basis; inverse: basis - value.
    fn signed(size: ContractSize, value: Decimal, basis: Decimal) -> Decimal {
        match size {
            ContractSize::Multiplier(_) => value - basis,
            ContractSize::ContractValue(_) => basis - value,
        }
    }
}

fn product(size: ContractSize) -> ProductKind {
    match size {
        ContractSize::Multiplier(_) => ProductKind::LinearPerpetual,
        ContractSize::ContractValue(_) => ProductKind::InversePerpetual,
    }
}

fn view(state: &State) -> ExposureView {
    ExposureView {
        account: AccountId::new("acct").unwrap(),
        venue: VenueId::new("venue").unwrap(),
        positions: state
            .positions
            .iter()
            .filter(|p| !p.contracts.is_zero())
            .map(|p| ExposurePosition {
                instrument: p.id.clone(),
                product: product(p.size),
                settlement: state.currency.clone(),
                contracts: p.contracts,
                size: p.size,
                mark: p.mark,
            })
            .collect(),
    }
}

/// (cash, collateral, initial, maintenance).
fn summary<M: InitialMarginModel>(rule: &M, state: &State) -> (Decimal, Decimal, Decimal, Decimal) {
    let mut collateral = state.cash;
    for p in &state.positions {
        if !p.contracts.is_zero() {
            let value = state.value(p.size, p.contracts, p.mark);
            collateral += State::signed(p.size, value, p.basis);
        }
    }
    let requirement = rule.requirement(&view(state)).unwrap();
    (
        state.cash,
        collateral,
        requirement.initial,
        requirement.maintenance,
    )
}

fn met<M: InitialMarginModel>(rule: &M, state: &State, target: CloseoutTarget) -> bool {
    let (cash, collateral, initial, maintenance) = summary(rule, state);
    match target {
        CloseoutTarget::Maintenance => cash >= dec!(0) && collateral >= maintenance,
        CloseoutTarget::Initial => cash >= dec!(0) && collateral >= initial,
        CloseoutTarget::Flat => state.positions.iter().all(|p| p.contracts.is_zero()),
    }
}

fn reduce(state: &State, index: usize, price: Decimal, n: Decimal, fee_rate: Decimal) -> State {
    let mut next = state.clone();
    let size = next.positions[index].size;
    let value = state.value(size, n, price);
    let fee = match size {
        ContractSize::Multiplier(m) => state.trunc(fee_rate * price * n * m),
        ContractSize::ContractValue(cv) => state.trunc(fee_rate * n * cv / price),
    };
    let open = &mut next.positions[index];
    let held = open.contracts.abs();
    let fill_cash = if open.contracts > dec!(0) {
        open.contracts -= n;
        value
    } else {
        open.contracts += n;
        -value
    };
    let released = if n == held {
        open.basis
    } else {
        state.rhe(open.basis * n / held)
    };
    // Linear: fill cash - released; inverse: released - fill value.
    let realized = State::signed(size, fill_cash, released);
    open.basis -= released;
    next.cash += realized - fee;
    next
}

/// The plan an exhaustive search gives: (instrument, level, contracts) per
/// forced fill, and the cash after.
fn oracle<M: InitialMarginModel>(
    rule: &M,
    start: &State,
    liquidity: &BTreeMap<InstrumentId, Vec<LiquidityLevel>>,
    policy: &LiquidationPolicy,
) -> (Vec<(InstrumentId, usize, Decimal)>, Decimal) {
    let requirement = rule.requirement(&view(start)).unwrap();
    let mut order: Vec<(Decimal, InstrumentId)> = requirement
        .positions
        .iter()
        .map(|p| (p.maintenance, p.instrument.clone()))
        .collect();
    order.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut state = start.clone();
    let mut fills = Vec::new();
    if met(rule, &state, policy.target) {
        return (fills, state.cash);
    }
    'instruments: for (_, id) in order {
        let index = state.positions.iter().position(|p| p.id == id).unwrap();
        for (level, offer) in liquidity
            .get(&id)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .enumerate()
        {
            if met(rule, &state, policy.target) {
                break 'instruments;
            }
            let size = state.positions[index].contracts.abs();
            if size.is_zero() {
                break;
            }
            let cap = offer.contracts.min(size);
            let mut chosen = cap;
            if policy.target != CloseoutTarget::Flat {
                let mut k = Decimal::ONE;
                loop {
                    let n = (k * policy.contract_step).min(cap);
                    let next = reduce(&state, index, offer.price, n, policy.fee_rate);
                    if met(rule, &next, policy.target) {
                        chosen = n;
                        break;
                    }
                    if n == cap {
                        break;
                    }
                    k += Decimal::ONE;
                }
            }
            state = reduce(&state, index, offer.price, chosen, policy.fee_rate);
            fills.push((id.clone(), level, chosen));
        }
        if met(rule, &state, policy.target) {
            break;
        }
    }
    (fills, state.cash)
}

fn levels(entries: &[(Decimal, Decimal)]) -> Vec<LiquidityLevel> {
    entries
        .iter()
        .map(|(price, contracts)| LiquidityLevel {
            price: *price,
            contracts: *contracts,
        })
        .collect()
}

/// The shape of one family of cases.
struct Family {
    /// Signed contracts and size of the main position.
    main: [(Decimal, ContractSize); 2],
    /// Entry price and mark of the main position.
    entry: Decimal,
    mark: Decimal,
    /// The second position (short 5) and its entry and mark.
    second: (ContractSize, Decimal, Decimal),
    /// Bids to sell a long and asks to buy back a short, two books each.
    bids: [Vec<LiquidityLevel>; 2],
    asks: [Vec<LiquidityLevel>; 2],
    second_asks: Vec<LiquidityLevel>,
    /// Cash: start and step of the sweep.
    cash: (Decimal, Decimal),
    scale: u32,
    currency: Asset,
}

/// What a family exercised.
#[derive(Debug, Default)]
struct Coverage {
    cases: u32,
    /// Plans that took part of a level (neither nothing nor all of it).
    partial: u32,
    /// Plans that took a whole level without meeting the target and went
    /// on to the next level of the same instrument.
    passed_level: u32,
}

/// Every combination of a family, with cash swept across the target; the
/// plan must equal the oracle on every one.
fn run<M: PiecewiseLinearMargin>(rule: &M, family: &Family) -> Coverage {
    let mut coverage = Coverage::default();
    for two_positions in [false, true] {
        for (contracts, size) in family.main {
            for book in 0..2 {
                for step in [dec!(1), dec!(0.5)] {
                    for fee_rate in [dec!(0), dec!(0.003)] {
                        for target in [CloseoutTarget::Maintenance, CloseoutTarget::Initial] {
                            for k in 0..25 {
                                let cash = family.cash.0 + Decimal::from(k) * family.cash.1;
                                let policy = LiquidationPolicy {
                                    target,
                                    fee_rate,
                                    fee_rates: BTreeMap::new(),
                                    loss_budget: None,
                                    contract_step: step,
                                };
                                let (partial, passed) = check_case(
                                    rule,
                                    family,
                                    two_positions,
                                    (contracts, size),
                                    book,
                                    &policy,
                                    cash,
                                );
                                coverage.cases += 1;
                                coverage.partial += u32::from(partial);
                                coverage.passed_level += u32::from(passed);
                            }
                        }
                    }
                }
            }
        }
    }
    coverage
}

/// Linear: entry 100.13 against a mark of 95.37 (a long loses, a short
/// gains), cash at 2 decimals.
#[test]
fn the_linear_search_matches_exhaustive_enumeration() {
    let levels_of = |entries: &[(Decimal, Decimal)]| levels(entries);
    let family = Family {
        main: [
            (dec!(13), ContractSize::Multiplier(dec!(1))),
            (dec!(-11), ContractSize::Multiplier(dec!(3.7))),
        ],
        entry: dec!(100.13),
        mark: dec!(95.37),
        second: (ContractSize::Multiplier(dec!(1)), dec!(50.29), dec!(53.11)),
        bids: [
            levels_of(&[(dec!(94.71), dec!(3)), (dec!(93.33), dec!(100))]),
            levels_of(&[
                (dec!(95.01), dec!(2.5)),
                (dec!(94.07), dec!(4)),
                (dec!(90.99), dec!(7)),
            ]),
        ],
        asks: [
            levels_of(&[(dec!(96.03), dec!(3)), (dec!(97.17), dec!(100))]),
            levels_of(&[
                (dec!(95.77), dec!(2.5)),
                (dec!(96.41), dec!(4)),
                (dec!(99.99), dec!(9)),
            ]),
        ],
        second_asks: levels_of(&[(dec!(53.89), dec!(2)), (dec!(55.01), dec!(10))]),
        cash: (dec!(-30), dec!(3.37)),
        scale: 2,
        currency: asset(),
    };
    for tiers in [1, 2] {
        let coverage = run(&rule(tiers), &family);
        assert_eq!(coverage.cases, 1600);
        assert!(coverage.partial > 100, "{coverage:?}");
        assert!(coverage.passed_level > 0, "{coverage:?}");
    }
}

/// Inverse: CV 100 or 10, entry 50013 against a mark of 45000 (a long
/// loses, a short gains), in BTC at 8 decimals.
#[test]
fn the_inverse_search_matches_exhaustive_enumeration() {
    let levels_of = |entries: &[(Decimal, Decimal)]| levels(entries);
    let family = Family {
        main: [
            (dec!(13), ContractSize::ContractValue(dec!(100))),
            (dec!(-11), ContractSize::ContractValue(dec!(370))),
        ],
        entry: dec!(50013),
        mark: dec!(45000),
        second: (
            ContractSize::ContractValue(dec!(100)),
            dec!(47011),
            dec!(45000),
        ),
        bids: [
            levels_of(&[(dec!(44900), dec!(3)), (dec!(44500), dec!(100))]),
            levels_of(&[
                (dec!(44971), dec!(2.5)),
                (dec!(44407), dec!(4)),
                (dec!(43001), dec!(7)),
            ]),
        ],
        asks: [
            levels_of(&[(dec!(45100), dec!(3)), (dec!(45900), dec!(100))]),
            levels_of(&[
                (dec!(45031), dec!(2.5)),
                (dec!(45603), dec!(4)),
                (dec!(47003), dec!(9)),
            ]),
        ],
        second_asks: levels_of(&[(dec!(45077), dec!(2)), (dec!(45999), dec!(10))]),
        cash: (dec!(-0.003), dec!(0.00037)),
        scale: 8,
        currency: btc(),
    };
    for tiers in [1, 2] {
        let coverage = run(&inverse_rule(tiers), &family);
        assert_eq!(coverage.cases, 1600);
        assert!(coverage.partial > 100, "{coverage:?}");
        assert!(coverage.passed_level > 0, "{coverage:?}");
    }
}

/// Plans one case and compares it with the oracle. Returns whether the
/// plan took part of a level, and whether it took a whole level and went
/// on to the next one.
fn check_case<M: PiecewiseLinearMargin>(
    rule: &M,
    family: &Family,
    two_positions: bool,
    (contracts, size): (Decimal, ContractSize),
    book: usize,
    policy: &LiquidationPolicy,
    cash: Decimal,
) -> (bool, bool) {
    let long = contracts > dec!(0);
    let rounding = State {
        scale: family.scale,
        currency: family.currency.clone(),
        cash,
        positions: vec![],
    };
    let mut positions = vec![Open {
        id: iid("A"),
        contracts,
        basis: rounding.value(size, contracts, family.entry),
        mark: family.mark,
        size,
    }];
    if two_positions {
        let (second_size, entry, mark) = family.second;
        positions.push(Open {
            id: iid("B"),
            contracts: dec!(-5),
            basis: rounding.value(second_size, dec!(-5), entry),
            mark,
            size: second_size,
        });
    }
    let a_levels = if long {
        family.bids[book].clone()
    } else {
        family.asks[book].clone()
    };
    let mut liquidity = BTreeMap::from([(iid("A"), a_levels)]);
    if two_positions {
        liquidity.insert(iid("B"), family.second_asks.clone());
    }
    let state = State {
        positions: positions.clone(),
        ..rounding
    };
    let account = LiquidationAccount {
        view: view(&state),
        basis: positions.iter().map(|p| (p.id.clone(), p.basis)).collect(),
        cash,
        resting_orders: vec![],
    };
    let plan = plan_liquidation(rule, &account, &liquidity, policy).unwrap();
    let (expected, expected_cash) = oracle(rule, &state, &liquidity, policy);
    let actual: Vec<_> = plan
        .reductions
        .iter()
        .map(|r| (r.instrument.clone(), r.level, r.contracts))
        .collect();
    assert_eq!(
        actual, expected,
        "cash {cash}, {contracts} x {size:?}, book {book}, two {two_positions}, {policy:?}"
    );
    assert_eq!(plan.after.cash, expected_cash);
    // Small grids are always searched to the end.
    assert!(plan.minimal);
    let partial = plan.reductions.iter().any(|r| {
        let cap = liquidity[&r.instrument][r.level].contracts;
        r.contracts < cap && !r.contracts_after.is_zero()
    });
    let passed = plan
        .reductions
        .windows(2)
        .any(|pair| pair[0].instrument == pair[1].instrument && pair[1].level > pair[0].level);
    (partial, passed)
}

/// Coarse quanta: cash in whole units (scale 0) or tenths (scale 1), where
/// rounding opens windows the unrounded figures do not have. Entry 1.13
/// against a mark of 0.97, a second position short from 0.93, a tier floor
/// at 5, cash from -10 to 14.
fn coarse_family(scale: u32, currency: Asset, sizes: [ContractSize; 2]) -> Family {
    let levels_of = |entries: &[(Decimal, Decimal)]| levels(entries);
    let [first, second] = sizes;
    Family {
        main: [(dec!(13), first), (dec!(-11), second)],
        entry: dec!(1.13),
        mark: dec!(0.97),
        second: (first, dec!(0.93), dec!(0.97)),
        bids: [
            levels_of(&[(dec!(0.95), dec!(3)), (dec!(0.91), dec!(100))]),
            levels_of(&[
                (dec!(0.95), dec!(2.5)),
                (dec!(0.91), dec!(4)),
                (dec!(0.89), dec!(7)),
            ]),
        ],
        asks: [
            levels_of(&[(dec!(0.99), dec!(3)), (dec!(1.03), dec!(100))]),
            levels_of(&[
                (dec!(0.99), dec!(2.5)),
                (dec!(1.03), dec!(4)),
                (dec!(1.09), dec!(9)),
            ]),
        ],
        second_asks: levels_of(&[(dec!(0.99), dec!(2)), (dec!(1.03), dec!(10))]),
        cash: (dec!(-10), dec!(1)),
        scale,
        currency,
    }
}

#[test]
fn the_linear_search_matches_enumeration_with_coarse_quanta() {
    for scale in [0, 1] {
        let family = coarse_family(
            scale,
            asset(),
            [
                ContractSize::Multiplier(dec!(1)),
                ContractSize::Multiplier(dec!(3.7)),
            ],
        );
        for tiers in [1, 2] {
            let rule = TieredLinearIm::new(
                RuleId::new("coarse", 1),
                asset(),
                scale,
                table(tiers, dec!(5)),
            )
            .unwrap();
            let coverage = run(&rule, &family);
            assert_eq!(coverage.cases, 1600);
            assert!(coverage.partial > 50, "{coverage:?}");
        }
    }
}

#[test]
fn the_inverse_search_matches_enumeration_with_coarse_quanta() {
    for scale in [0, 1] {
        let family = coarse_family(
            scale,
            btc(),
            [
                ContractSize::ContractValue(dec!(1)),
                ContractSize::ContractValue(dec!(3.7)),
            ],
        );
        for tiers in [1, 2] {
            let rule = TieredInverseIm::new(
                RuleId::new("coarse-inverse", 1),
                btc(),
                scale,
                table(tiers, dec!(5)),
            )
            .unwrap();
            let coverage = run(&rule, &family);
            assert_eq!(coverage.cases, 1600);
            assert!(coverage.partial > 50, "{coverage:?}");
        }
    }
}
