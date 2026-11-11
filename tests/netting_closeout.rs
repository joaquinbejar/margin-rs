//! The closeout plan under a rule that nets across positions.
//!
//! `NetLinear` charges `rate * |sum of signed notionals|` (im 10 %, mm 5 %),
//! rounded up to 6 decimals, and attributes the whole requirement to the
//! first open position's line. Fixture: A long 10 and B short 6, both at
//! mark 100 with entry 100 (no unrealized PnL), cash 10, no fee. Closing
//! `n` of A at 99 realizes `-n`, so collateral is `10 - n` against a
//! maintenance of `5 * |4 - n|` and an initial of `10 * |4 - n|`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::{
    AccountMargin, ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement,
    PositionRequirement, ProductKind, RuleId,
};
use margin_rs::liquidation::{
    CloseoutOutcome, CloseoutTarget, LiquidationAccount, LiquidationPolicy, LiquidityLevel,
    plan_liquidation,
};
use margin_rs::rules::{MarginTier, TieredLinearIm};
use margin_rs::{AccountId, Asset, ContractSize, Decimal, InstrumentId, VenueId};
use rust_decimal::RoundingStrategy;
use rust_decimal_macros::dec;

fn usdc() -> Asset {
    Asset::new("USDC").expect("asset")
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).expect("id")
}

struct NetLinear {
    rule: RuleId,
    /// What `plan_monotone` claims.
    monotone: bool,
}

impl NetLinear {
    fn new() -> Self {
        Self {
            rule: RuleId::new("net-linear", 1),
            monotone: false,
        }
    }

    fn monotone() -> Self {
        Self {
            monotone: true,
            ..Self::new()
        }
    }
}

impl InitialMarginModel for NetLinear {
    fn rule(&self) -> &RuleId {
        &self.rule
    }

    fn requirement(&self, view: &ExposureView) -> margin_rs::Result<MarginRequirement> {
        let net: Decimal = view
            .positions
            .iter()
            .map(|p| match p.size {
                ContractSize::Multiplier(m) => p.contracts * m * p.mark,
                ContractSize::ContractValue(_) => panic!("linear only"),
            })
            .sum();
        let up = |rate: Decimal| {
            (net.abs() * rate).round_dp_with_strategy(6, RoundingStrategy::AwayFromZero)
        };
        let (initial, maintenance) = (up(dec!(0.10)), up(dec!(0.05)));
        let positions = view
            .positions
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let share = |total| if i == 0 { total } else { Decimal::ZERO };
                PositionRequirement {
                    instrument: p.instrument.clone(),
                    contracts: p.contracts,
                    notional: None,
                    slices: vec![],
                    initial: share(initial),
                    maintenance: share(maintenance),
                }
            })
            .collect();
        Ok(MarginRequirement {
            rule: self.rule.clone(),
            account: view.account.clone(),
            venue: view.venue.clone(),
            currency: usdc(),
            initial,
            maintenance,
            positions,
        })
    }
}

impl AccountMargin for NetLinear {
    fn plan_cash_scale(&self) -> u32 {
        6
    }

    fn plan_tiers(&self, _: &ExposurePosition) -> Option<Vec<MarginTier>> {
        None
    }

    fn plan_monotone(&self, _: &ExposurePosition) -> bool {
        self.monotone
    }
}

fn perp(id: &str, contracts: Decimal) -> ExposurePosition {
    ExposurePosition {
        instrument: iid(id),
        product: ProductKind::LinearPerpetual,
        settlement: usdc(),
        contracts,
        size: ContractSize::Multiplier(Decimal::ONE),
        mark: dec!(100),
    }
}

fn hedged() -> LiquidationAccount {
    LiquidationAccount {
        view: ExposureView {
            account: AccountId::new("acct").expect("id"),
            venue: VenueId::new("venue").expect("id"),
            positions: vec![perp("A", dec!(10)), perp("B", dec!(-6))],
        },
        basis: BTreeMap::from([(iid("A"), dec!(1000)), (iid("B"), dec!(-600))]),
        cash: dec!(10),
        resting_orders: vec![],
    }
}

fn bids_on_a() -> BTreeMap<InstrumentId, Vec<LiquidityLevel>> {
    BTreeMap::from([(
        iid("A"),
        vec![LiquidityLevel {
            price: dec!(99),
            contracts: dec!(100),
        }],
    )])
}

fn policy(target: CloseoutTarget, step: Decimal) -> LiquidationPolicy {
    LiquidationPolicy {
        target,
        fee_rate: Decimal::ZERO,
        fee_rates: BTreeMap::new(),
        loss_budget: None,
        contract_step: step,
    }
}

/// Maintenance: 2 leaves 8 against 10, 3 leaves 7 against 5. Initial: 3
/// leaves 7 against 10, 4 leaves 6 against 0.
#[test]
fn test_netting_rule_stops_at_the_smallest_count_meeting_the_target() {
    for (target, contracts, collateral, margin) in [
        (CloseoutTarget::Maintenance, dec!(3), dec!(7), dec!(5)),
        (CloseoutTarget::Initial, dec!(4), dec!(6), dec!(0)),
    ] {
        let plan = plan_liquidation(
            &NetLinear::new(),
            &hedged(),
            &bids_on_a(),
            &policy(target, Decimal::ONE),
        )
        .expect("plan");
        assert_eq!(plan.outcome, CloseoutOutcome::TargetMet, "{target:?}");
        assert_eq!(plan.reductions.len(), 1);
        assert_eq!(plan.reductions[0].instrument, iid("A"));
        assert_eq!(plan.reductions[0].contracts, contracts);
        assert_eq!(plan.after.collateral, collateral);
        let after_margin = match target {
            CloseoutTarget::Initial => plan.after.initial,
            _ => plan.after.maintenance,
        };
        assert_eq!(after_margin, margin);
        assert!(plan.minimal);
    }
}

/// The same account under a gross per-position rule (mm 5 % of 1000 +
/// 600) cannot meet maintenance by closing A at all.
#[test]
fn test_gross_rule_on_the_same_account_runs_out_of_liquidity() {
    let gross = TieredLinearIm::new(
        RuleId::new("linear-im", 1),
        usdc(),
        6,
        vec![MarginTier {
            floor: Decimal::ZERO,
            initial_rate: dec!(0.10),
            maintenance_rate: dec!(0.05),
        }],
    )
    .expect("rule");
    let plan = plan_liquidation(
        &gross,
        &hedged(),
        &bids_on_a(),
        &policy(CloseoutTarget::Maintenance, Decimal::ONE),
    )
    .expect("plan");
    assert_eq!(plan.outcome, CloseoutOutcome::LiquidityExhausted);
    assert_eq!(plan.reductions[0].contracts, dec!(10));
}

/// Without tiers each count costs one evaluation; the target needs
/// `10 - n >= 5 (4 - n)`, n >= 2.5: 2500 counts at a step of 0.001 fit the
/// budget of 4096 and the walk proves 2.5 the smallest. 5000 at 0.0005 do
/// not: once the walk reaches the reserve it gallops and bisects, finds
/// 2.5 again, and reports it as not proven minimal.
#[test]
fn test_netting_rule_walk_is_bounded_by_the_search_budget() {
    let at = |step| {
        plan_liquidation(
            &NetLinear::new(),
            &hedged(),
            &bids_on_a(),
            &policy(CloseoutTarget::Maintenance, step),
        )
    };
    let walked = at(dec!(0.001)).expect("plan");
    assert_eq!(walked.reductions[0].contracts, dec!(2.5));
    assert!(walked.minimal);
    let galloped = at(dec!(0.0005)).expect("plan");
    assert_eq!(galloped.outcome, CloseoutOutcome::TargetMet);
    assert_eq!(galloped.reductions[0].contracts, dec!(2.5));
    assert!(!galloped.minimal);
}

/// A lone long A 10 (no hedge), cash 10, sold at 99.5: collateral
/// `10 - 0.5n` against a maintenance of `5 * (10 - n)`, met from
/// n = 8.888..., so 8.889 at a step of 0.0005: 17778 counts, beyond the
/// walk's budget. Declared monotone, the level is bisected and the count
/// is proven minimal; undeclared, the walk gallops to the same count but
/// cannot prove it minimal.
#[test]
fn test_monotone_rule_is_bisected() {
    let mut account = hedged();
    account.view.positions.truncate(1);
    account.basis.remove(&iid("B"));
    let liquidity = BTreeMap::from([(
        iid("A"),
        vec![LiquidityLevel {
            price: dec!(99.5),
            contracts: dec!(100),
        }],
    )]);
    let step = policy(CloseoutTarget::Maintenance, dec!(0.0005));
    let galloped = plan_liquidation(&NetLinear::new(), &account, &liquidity, &step).expect("plan");
    assert_eq!(galloped.reductions[0].contracts, dec!(8.889));
    assert!(!galloped.minimal);
    let plan = plan_liquidation(&NetLinear::monotone(), &account, &liquidity, &step).expect("plan");
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    assert_eq!(plan.reductions[0].contracts, dec!(8.889));
    // The count before it does not meet the target: 8.8885 leaves
    // 5.55575 against 5.5575.
    assert!(plan.minimal);
    // At a step of 1 the walk and the bisection agree.
    let unit = policy(CloseoutTarget::Maintenance, Decimal::ONE);
    assert_eq!(
        plan_liquidation(&NetLinear::monotone(), &account, &liquidity, &unit).expect("plan"),
        plan_liquidation(&NetLinear::new(), &account, &liquidity, &unit).expect("plan"),
    );
}

/// A wrong claim keeps the plan valid: on the hedged pair the target is
/// met only between 2.5 and 5 contracts, so the whole level (10) does not
/// meet it and the bisection takes the whole level, as the walk does when
/// no count meets the target.
#[test]
fn test_wrong_monotone_claim_takes_the_whole_level() {
    let plan = plan_liquidation(
        &NetLinear::monotone(),
        &hedged(),
        &bids_on_a(),
        &policy(CloseoutTarget::Maintenance, Decimal::ONE),
    )
    .expect("plan");
    assert_eq!(plan.reductions[0].contracts, dec!(10));
    assert_ne!(plan.outcome, CloseoutOutcome::TargetMet);
}
