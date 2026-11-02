//! Maintenance threshold and staged closeout fixtures (M3-04), hand
//! calculated with a single-tier `linear-im/1` rule (im 10 %, mm 5 % of
//! `|Qc| * M * mark`), USDC at 6 decimals, fee rate 0.0005.
//!
//! Main fixture (`long_btc`): 10 BTC-PERP contracts long, M = 1, entry 100
//! (basis 1000), mark 95, so upnl = 950 - 1000 = -50, im = 95, mm = 47.5.
//! Closing `n` at 94: fill cash 94n, released 100n, realized -6n, fee
//! 0.047n. After it, collateral = cash - 50 - 1.047n and
//! mm = 47.5 - 4.75n.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement, ProductKind, RuleId,
};
use margin_rs::ledger::{Action, DerivativeSettlement, Execution, Ledger};
use margin_rs::liquidation::{
    CloseoutIntent, CloseoutOutcome, CloseoutTarget, LiquidationAccount, LiquidationPlan,
    LiquidationPolicy, LiquidityLevel, plan_liquidation,
};
use margin_rs::maintenance::{AccountState, CollateralView, StateReason, assess};
use margin_rs::rules::{MarginTier, TieredLinearIm};
use margin_rs::valuation::{MarkSet, ValuationPolicy, value};
use margin_rs::{
    AccountId, Asset, Decimal, ExecutionId, InstrumentId, MarginError, OrderId, SpotInstrument,
    TransferId, VenueId,
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

fn rule() -> TieredLinearIm {
    TieredLinearIm::new(
        RuleId::new("linear-im", 1),
        usdc(),
        6,
        vec![MarginTier {
            floor: Decimal::ZERO,
            initial_rate: dec!(0.10),
            maintenance_rate: dec!(0.05),
        }],
    )
    .expect("rule")
}

fn perp(id: &str, contracts: Decimal, mark: Decimal) -> ExposurePosition {
    ExposurePosition {
        instrument: iid(id),
        product: ProductKind::LinearPerpetual,
        settlement: usdc(),
        contracts,
        multiplier: Decimal::ONE,
        mark,
    }
}

fn view(positions: Vec<ExposurePosition>) -> ExposureView {
    ExposureView {
        account: AccountId::new("acct").expect("id"),
        venue: VenueId::new("venue").expect("id"),
        positions,
    }
}

fn requirement(positions: Vec<ExposurePosition>) -> MarginRequirement {
    rule().requirement(&view(positions)).expect("requirement")
}

fn policy(target: CloseoutTarget) -> LiquidationPolicy {
    LiquidationPolicy {
        target,
        fee_rate: dec!(0.0005),
        contract_step: Decimal::ONE,
    }
}

fn long_btc(cash: Decimal) -> LiquidationAccount {
    LiquidationAccount {
        view: view(vec![perp("BTC-PERP", dec!(10), dec!(95))]),
        basis: BTreeMap::from([(iid("BTC-PERP"), dec!(1000))]),
        cash,
        resting_orders: vec![oid("o1"), oid("o2")],
    }
}

fn bids(levels: &[(Decimal, Decimal)]) -> BTreeMap<InstrumentId, Vec<LiquidityLevel>> {
    BTreeMap::from([(
        iid("BTC-PERP"),
        levels
            .iter()
            .map(|(price, contracts)| LiquidityLevel {
                price: *price,
                contracts: *contracts,
            })
            .collect(),
    )])
}

fn plan(
    account: &LiquidationAccount,
    liquidity: &BTreeMap<InstrumentId, Vec<LiquidityLevel>>,
    policy: &LiquidationPolicy,
) -> LiquidationPlan {
    plan_liquidation(&rule(), account, liquidity, policy).expect("plan")
}

/// A ledger holding `cash` USDC and nothing else.
fn ledger_with(cash: Decimal) -> Ledger {
    let mut ledger = Ledger::default();
    ledger.register_asset(usdc(), 6).expect("asset");
    ledger
        .deposit(TransferId::new("d1").expect("id"), usdc(), cash)
        .expect("deposit");
    ledger
}

/// Books every forced fill of `plan` as a settlement `liq-<i>`.
fn book_plan(ledger: &mut Ledger, plan: &LiquidationPlan) -> Vec<Option<Decimal>> {
    plan.reductions
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let settlement = r.settlement(eid(&format!("liq-{i}")), plan.currency.clone());
            let booking = ledger.book_settlement(&settlement).expect("settlement");
            assert!(booking.is_new);
            booking.shortfall
        })
        .collect()
}

mod maintenance {
    use super::*;

    fn btc() -> MarginRequirement {
        // notional 950: im 95, mm 47.5.
        requirement(vec![perp("BTC-PERP", dec!(10), dec!(95))])
    }

    fn view_at(
        cash: Decimal,
        collateral: Option<Decimal>,
        equity: Option<Decimal>,
    ) -> CollateralView {
        CollateralView {
            currency: usdc(),
            cash,
            collateral,
            equity,
        }
    }

    #[test]
    fn just_above_at_and_below_maintenance() {
        let req = btc();
        assert_eq!(req.maintenance, dec!(47.5));
        let cases = [
            (
                dec!(47.500001),
                AccountState::Healthy,
                dec!(0.000001),
                dec!(0),
            ),
            (dec!(47.5), AccountState::Healthy, dec!(0), dec!(0)),
            (
                dec!(47.499999),
                AccountState::ReduceOnly,
                dec!(-0.000001),
                dec!(0.000001),
            ),
        ];
        for (collateral, state, excess, shortfall) in cases {
            let decision = assess(
                AccountState::Healthy,
                &view_at(dec!(100), Some(collateral), Some(collateral)),
                &req,
            )
            .expect("assess");
            assert_eq!(decision.state, state, "collateral {collateral}");
            assert_eq!(decision.excess, Some(excess));
            assert_eq!(decision.maintenance_shortfall, Some(shortfall));
            assert_eq!(decision.is_transition(), state != AccountState::Healthy);
            assert_eq!(decision.rule.to_string(), "linear-im/1");
        }
        let below = assess(
            AccountState::Healthy,
            &view_at(dec!(100), Some(dec!(47.499999)), Some(dec!(47.499999))),
            &req,
        )
        .expect("assess");
        assert_eq!(below.reasons, vec![StateReason::BelowMaintenance]);
        assert!(!below.state.may_increase_risk());
    }

    #[test]
    fn negative_cash_or_equity_is_default() {
        let req = btc();
        let debt = assess(
            AccountState::Healthy,
            &view_at(dec!(-3), Some(dec!(60)), Some(dec!(60))),
            &req,
        )
        .expect("assess");
        assert_eq!(debt.state, AccountState::Default);
        assert_eq!(debt.reasons, vec![StateReason::NegativeCash]);
        assert_eq!(debt.debt, dec!(3));

        let equity = assess(
            AccountState::ReduceOnly,
            &view_at(dec!(10), Some(dec!(-1)), Some(dec!(-1))),
            &req,
        )
        .expect("assess");
        assert_eq!(equity.state, AccountState::Default);
        assert_eq!(
            equity.reasons,
            vec![StateReason::NegativeEquity, StateReason::BelowMaintenance]
        );
        assert_eq!(equity.debt, dec!(0));
    }

    #[test]
    fn default_is_left_only_with_cash_and_maintenance() {
        let req = btc();
        // Cash back to zero but collateral still below mm: stays Default.
        let stays = assess(
            AccountState::Default,
            &view_at(dec!(0), Some(dec!(47)), Some(dec!(47))),
            &req,
        )
        .expect("assess");
        assert_eq!(stays.state, AccountState::Default);
        assert_eq!(
            stays.reasons,
            vec![
                StateReason::DefaultNotCleared,
                StateReason::BelowMaintenance
            ]
        );
        assert!(!stays.is_transition());
        // Exactly at maintenance with cash 0: leaves Default.
        let leaves = assess(
            AccountState::Default,
            &view_at(dec!(0), Some(dec!(47.5)), Some(dec!(47.5))),
            &req,
        )
        .expect("assess");
        assert_eq!(leaves.state, AccountState::Healthy);
        assert!(leaves.is_transition());
        assert_eq!(
            leaves.reasons,
            [] as [margin_rs::maintenance::StateReason; 0]
        );
    }

    #[test]
    fn reduce_only_recovers_to_healthy() {
        let decision = assess(
            AccountState::ReduceOnly,
            &view_at(dec!(50), Some(dec!(50)), Some(dec!(50))),
            &btc(),
        )
        .expect("assess");
        assert_eq!(decision.state, AccountState::Healthy);
        assert!(decision.state.may_increase_risk());
    }

    #[test]
    fn unknown_figures_are_never_healthy() {
        let req = btc();
        let collateral = assess(
            AccountState::Healthy,
            &view_at(dec!(100), None, Some(dec!(100))),
            &req,
        )
        .expect("assess");
        assert_eq!(collateral.state, AccountState::ReduceOnly);
        assert_eq!(collateral.reasons, vec![StateReason::CollateralUnknown]);
        assert_eq!(collateral.excess, None);
        assert_eq!(collateral.maintenance_shortfall, None);

        let equity = assess(
            AccountState::Healthy,
            &view_at(dec!(100), Some(dec!(100)), None),
            &req,
        )
        .expect("assess");
        assert_eq!(equity.state, AccountState::ReduceOnly);
        assert_eq!(equity.reasons, vec![StateReason::EquityUnknown]);

        // A defaulted account with unknown collateral cannot prove the exit.
        let default = assess(
            AccountState::Default,
            &view_at(dec!(0), None, Some(dec!(0))),
            &req,
        )
        .expect("assess");
        assert_eq!(default.state, AccountState::Default);
    }

    #[test]
    fn currency_must_match() {
        let mut other = view_at(dec!(1), Some(dec!(1)), Some(dec!(1)));
        other.currency = Asset::new("USDT").expect("asset");
        assert!(matches!(
            assess(AccountState::Healthy, &other, &btc()),
            Err(MarginError::CurrencyMismatch { .. })
        ));
    }
}

mod plan {
    use super::*;

    /// Cash 90: collateral 40 < mm 47.5. n = 2 gives 37.906 < 38; n = 3
    /// gives 36.859 >= 33.25. The plan closes 3 of 10 at the first level.
    #[test]
    fn partial_closeout_restores_maintenance() {
        let account = long_btc(dec!(90));
        let plan = plan(
            &account,
            &bids(&[(dec!(94), dec!(3)), (dec!(93), dec!(100))]),
            &policy(CloseoutTarget::Maintenance),
        );
        assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
        assert_eq!(plan.before.collateral, dec!(40));
        assert_eq!(plan.before.maintenance, dec!(47.5));
        assert_eq!(plan.cancels, vec![oid("o1"), oid("o2")]);
        assert_eq!(plan.reductions.len(), 1);
        let fill = &plan.reductions[0];
        assert_eq!(
            (fill.side, fill.level, fill.price, fill.contracts),
            (Action::Sell, 0, dec!(94), dec!(3))
        );
        assert_eq!(fill.fill_cash, dec!(282));
        assert_eq!(fill.released_basis, dec!(300));
        assert_eq!(fill.realized_pnl, dec!(-18));
        assert_eq!(fill.fee, dec!(0.141));
        assert_eq!(fill.contracts_after, dec!(7));
        assert_eq!(plan.after.cash, dec!(71.859));
        assert_eq!(plan.after.collateral, dec!(36.859));
        assert_eq!(plan.after.maintenance, dec!(33.25));
        assert_eq!(plan.after.positions[0].contracts, dec!(7));
        assert_eq!(plan.after.positions[0].basis, dec!(700));
        assert_eq!(plan.after.positions[0].unrealized_pnl, dec!(-35));
        assert_eq!(plan.deficit, dec!(0));
        assert_eq!(plan.debt, dec!(0));
        assert!(plan.unfilled.is_empty());

        assert_eq!(
            plan.intents().expect("intents"),
            vec![
                CloseoutIntent::Cancel { order: oid("o1") },
                CloseoutIntent::Cancel { order: oid("o2") },
                CloseoutIntent::Reduce {
                    instrument: iid("BTC-PERP"),
                    side: Action::Sell,
                    contracts: dec!(3),
                    limit_price: dec!(94),
                },
            ]
        );
    }

    /// Cash 90.094: n = 2 gives collateral 38 = mm 38 exactly, which meets
    /// the target (equality is healthy).
    #[test]
    fn equality_meets_the_target() {
        let plan = plan(
            &long_btc(dec!(90.094)),
            &bids(&[(dec!(94), dec!(100))]),
            &policy(CloseoutTarget::Maintenance),
        );
        assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
        assert_eq!(plan.reductions[0].contracts, dec!(2));
        assert_eq!(plan.after.collateral, dec!(38));
        assert_eq!(plan.after.maintenance, dec!(38));
    }

    /// Only 2 contracts at 94: collateral 37.906 against mm 38. The rest
    /// stays open and the deficit 0.094 is explicit.
    #[test]
    fn insufficient_liquidity_leaves_an_explicit_deficit() {
        let plan = plan(
            &long_btc(dec!(90)),
            &bids(&[(dec!(94), dec!(2))]),
            &policy(CloseoutTarget::Maintenance),
        );
        assert_eq!(plan.outcome, CloseoutOutcome::LiquidityExhausted);
        assert_eq!(plan.reductions.len(), 1);
        assert_eq!(plan.after.collateral, dec!(37.906));
        assert_eq!(plan.after.maintenance, dec!(38));
        assert_eq!(plan.unfilled, BTreeMap::from([(iid("BTC-PERP"), dec!(8))]));
        assert_eq!(plan.deficit, dec!(0.094));

        // No liquidity at all: nothing is filled, everything stays open.
        let none = super::plan(
            &long_btc(dec!(90)),
            &BTreeMap::new(),
            &policy(CloseoutTarget::Maintenance),
        );
        assert_eq!(none.outcome, CloseoutOutcome::LiquidityExhausted);
        assert_eq!(
            none.reductions,
            [] as [margin_rs::liquidation::ForcedReduction; 0]
        );
        assert_eq!(none.unfilled, BTreeMap::from([(iid("BTC-PERP"), dec!(10))]));
        assert_eq!(none.deficit, dec!(7.5));
        assert_eq!(none.after, none.before);
    }

    /// Mark 80, cash 150: collateral -50. Closing at 70 only deepens the
    /// loss; the account ends flat with cash 150 + 700 - 1000 - 0.35.
    #[test]
    fn flat_with_deficit_keeps_the_debt() {
        let mut account = long_btc(dec!(150));
        account.view.positions[0].mark = dec!(80);
        let plan = plan(
            &account,
            &bids(&[(dec!(70), dec!(100))]),
            &policy(CloseoutTarget::Initial),
        );
        assert_eq!(plan.before.collateral, dec!(-50));
        assert_eq!(plan.outcome, CloseoutOutcome::FlatWithDeficit);
        assert_eq!(plan.reductions[0].contracts, dec!(10));
        assert_eq!(plan.reductions[0].released_basis, dec!(1000));
        assert_eq!(
            plan.after.positions,
            [] as [margin_rs::liquidation::PlanPosition; 0]
        );
        assert_eq!(plan.after.cash, dec!(-150.35));
        assert_eq!(plan.debt, dec!(150.35));
        assert_eq!(plan.deficit, dec!(150.35));
    }

    /// A step of 0.5 closes 2.5 contracts: 2 does not restore mm, 2.5 does
    /// (37.3825 >= 35.625).
    #[test]
    fn contract_step_sets_the_granularity() {
        let mut fine = policy(CloseoutTarget::Maintenance);
        fine.contract_step = dec!(0.5);
        let plan = plan(&long_btc(dec!(90)), &bids(&[(dec!(94), dec!(100))]), &fine);
        assert_eq!(plan.reductions[0].contracts, dec!(2.5));
        assert_eq!(plan.reductions[0].fee, dec!(0.1175));
        assert_eq!(plan.after.collateral, dec!(37.3825));
        assert_eq!(plan.after.maintenance, dec!(35.625));
    }

    /// The initial margin target needs more: collateral 40 - 1.047n
    /// against im 95 - 9.5n, so n >= 6.6, i.e. 7.
    #[test]
    fn initial_target_closes_further() {
        let plan = plan(
            &long_btc(dec!(90)),
            &bids(&[(dec!(94), dec!(100))]),
            &policy(CloseoutTarget::Initial),
        );
        assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
        assert_eq!(plan.reductions[0].contracts, dec!(7));
        assert_eq!(plan.after.collateral, dec!(32.671));
        assert_eq!(plan.after.initial, dec!(28.5));
    }

    #[test]
    fn healthy_account_gets_an_empty_plan() {
        let plan = plan(
            &long_btc(dec!(200)),
            &bids(&[(dec!(94), dec!(100))]),
            &policy(CloseoutTarget::Maintenance),
        );
        assert_eq!(plan.outcome, CloseoutOutcome::AlreadyMet);
        assert_eq!(plan.cancels, [] as [margin_rs::OrderId; 0]);
        assert_eq!(
            plan.reductions,
            [] as [margin_rs::liquidation::ForcedReduction; 0]
        );
        assert_eq!(plan.intents().expect("intents"), vec![]);
        assert_eq!(plan.after, plan.before);
    }

    #[test]
    fn flat_target_closes_everything_across_levels() {
        let plan = plan(
            &long_btc(dec!(200)),
            &bids(&[(dec!(94), dec!(4)), (dec!(93), dec!(100))]),
            &policy(CloseoutTarget::Flat),
        );
        assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
        let sizes: Vec<_> = plan
            .reductions
            .iter()
            .map(|r| (r.level, r.contracts))
            .collect();
        assert_eq!(sizes, vec![(0, dec!(4)), (1, dec!(6))]);
        // 4 at 94 releases 400; the last 6 release the remaining 600.
        assert_eq!(plan.reductions[1].released_basis, dec!(600));
        assert_eq!(
            plan.after.positions,
            [] as [margin_rs::liquidation::PlanPosition; 0]
        );
        assert_eq!(
            plan.intents().expect("intents")[2],
            CloseoutIntent::Reduce {
                instrument: iid("BTC-PERP"),
                side: Action::Sell,
                contracts: dec!(10),
                limit_price: dec!(93),
            }
        );
    }

    /// ETH short (-20, entry 50, mark 55: mm 55) is closed before the BTC
    /// long (10, entry 100, mark 100: mm 50), buying back from the asks.
    #[test]
    fn largest_maintenance_first_and_shorts_buy_back() {
        let account = LiquidationAccount {
            view: view(vec![
                perp("BTC-PERP", dec!(10), dec!(100)),
                perp("ETH-PERP", dec!(-20), dec!(55)),
            ]),
            basis: BTreeMap::from([
                (iid("BTC-PERP"), dec!(1000)),
                (iid("ETH-PERP"), dec!(-1000)),
            ]),
            cash: dec!(120),
            resting_orders: vec![],
        };
        let liquidity = BTreeMap::from([
            (
                iid("ETH-PERP"),
                vec![
                    LiquidityLevel {
                        price: dec!(56),
                        contracts: dec!(5),
                    },
                    LiquidityLevel {
                        price: dec!(57),
                        contracts: dec!(100),
                    },
                ],
            ),
            (
                iid("BTC-PERP"),
                vec![LiquidityLevel {
                    price: dec!(99),
                    contracts: dec!(100),
                }],
            ),
        ]);
        let plan = plan(&account, &liquidity, &policy(CloseoutTarget::Maintenance));
        assert_eq!(plan.before.collateral, dec!(20));
        assert_eq!(plan.before.maintenance, dec!(105));
        let fills: Vec<_> = plan
            .reductions
            .iter()
            .map(|r| (r.instrument.as_str().to_owned(), r.side, r.contracts))
            .collect();
        assert_eq!(
            fills,
            vec![
                ("ETH-PERP".to_owned(), Action::Buy, dec!(5)),
                ("ETH-PERP".to_owned(), Action::Buy, dec!(15)),
                ("BTC-PERP".to_owned(), Action::Sell, dec!(10)),
            ]
        );
        // Buying back 5 at 56 against a basis of -1000 for 20.
        let first = &plan.reductions[0];
        assert_eq!(first.fill_cash, dec!(-280));
        assert_eq!(first.released_basis, dec!(-250));
        assert_eq!(first.realized_pnl, dec!(-30));
        assert_eq!(first.fee, dec!(0.14));
        assert_eq!(plan.outcome, CloseoutOutcome::FlatWithDeficit);
        assert_eq!(plan.after.cash, dec!(-26.0625));
        assert_eq!(plan.debt, dec!(26.0625));
    }

    /// For many cash levels, the count taken meets the target and one step
    /// less does not.
    #[test]
    fn the_plan_takes_the_smallest_count_that_meets_the_target() {
        let liquidity = bids(&[(dec!(94), dec!(1000))]);
        let target = policy(CloseoutTarget::Maintenance);
        let mut cash = dec!(60);
        while cash < dec!(97.5) {
            let result = plan(&long_btc(cash), &liquidity, &target);
            if result.outcome == CloseoutOutcome::TargetMet {
                let taken = result.reductions[0].contracts;
                assert!(result.after.collateral >= result.after.maintenance);
                let less = if taken > Decimal::ONE {
                    bids(&[(dec!(94), taken - Decimal::ONE)])
                } else {
                    BTreeMap::new()
                };
                let short = plan(&long_btc(cash), &less, &target);
                assert_eq!(
                    short.outcome,
                    CloseoutOutcome::LiquidityExhausted,
                    "cash {cash}"
                );
            }
            cash += dec!(0.731);
        }
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        let account = long_btc(dec!(90));
        let liquidity = bids(&[(dec!(94), dec!(10))]);
        let invalid = |result: margin_rs::Result<LiquidationPlan>| {
            matches!(result, Err(MarginError::InvalidInput { .. }))
        };
        let mut bad = policy(CloseoutTarget::Maintenance);
        bad.fee_rate = Decimal::ONE;
        assert!(invalid(plan_liquidation(
            &rule(),
            &account,
            &liquidity,
            &bad
        )));
        let mut bad = policy(CloseoutTarget::Maintenance);
        bad.contract_step = Decimal::ZERO;
        assert!(invalid(plan_liquidation(
            &rule(),
            &account,
            &liquidity,
            &bad
        )));

        let good = policy(CloseoutTarget::Maintenance);
        let mut missing = account.clone();
        missing.basis.clear();
        assert!(invalid(plan_liquidation(
            &rule(),
            &missing,
            &liquidity,
            &good
        )));
        let mut extra = account.clone();
        extra.basis.insert(iid("ETH-PERP"), dec!(1));
        assert!(invalid(plan_liquidation(
            &rule(),
            &extra,
            &liquidity,
            &good
        )));
        let mut repeated = account.clone();
        repeated.resting_orders.push(oid("o1"));
        assert!(invalid(plan_liquidation(
            &rule(),
            &repeated,
            &liquidity,
            &good
        )));

        // Bids must go from the highest price down.
        let unordered = bids(&[(dec!(93), dec!(1)), (dec!(94), dec!(1))]);
        assert!(invalid(plan_liquidation(
            &rule(),
            &account,
            &unordered,
            &good
        )));
        let empty_level = bids(&[(dec!(94), dec!(0))]);
        assert!(invalid(plan_liquidation(
            &rule(),
            &account,
            &empty_level,
            &good
        )));

        let mut precise = account.clone();
        precise.cash = dec!(90.0000001);
        assert!(matches!(
            plan_liquidation(&rule(), &precise, &liquidity, &good),
            Err(MarginError::PrecisionExceeded { .. })
        ));
    }
}

/// Review round 2: C1 (step arithmetic near the Decimal limits),
/// C2 (the feasible counts on a level are an interval that need not reach
/// its end) and n1 (basis sign).
mod review {
    use super::*;

    fn tiny(contracts: Decimal, basis: Decimal, cash: Decimal) -> LiquidationAccount {
        LiquidationAccount {
            view: view(vec![perp(
                "BTC-PERP",
                contracts,
                dec!(0.000000000000000001),
            )]),
            basis: BTreeMap::from([(iid("BTC-PERP"), basis)]),
            cash,
            resting_orders: vec![],
        }
    }

    fn at_mark(contracts: Decimal) -> BTreeMap<InstrumentId, Vec<LiquidityLevel>> {
        bids(&[(dec!(0.000000000000000001), contracts)])
    }

    /// C1: closing `Decimal::MAX` contracts with a step of 2 used to compute
    /// ceil(MAX / 2) * 2 and overflow; the last count is the cap.
    #[test]
    fn step_two_closes_decimal_max() {
        let mut flat = policy(CloseoutTarget::Flat);
        flat.fee_rate = Decimal::ZERO;
        flat.contract_step = dec!(2);
        let plan = plan(
            &tiny(Decimal::MAX, dec!(1), dec!(0)),
            &at_mark(Decimal::MAX),
            &flat,
        );
        assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
        assert_eq!(plan.reductions[0].contracts, Decimal::MAX);
        assert_eq!(
            plan.after.positions,
            [] as [margin_rs::liquidation::PlanPosition; 0]
        );
    }

    /// C1: a step of 0.5 on `Decimal::MAX` contracts has more grid points
    /// than a Decimal can count, and bisection midpoints above MAX / 2.
    /// Notional 79228162514.264338 (basis equal, so upnl 0), mm 5 %; with
    /// cash 2e9 the plan must close about MAX - 4e28 contracts, the
    /// smallest count that meets the target.
    #[test]
    fn half_steps_on_decimal_max_find_the_smallest_count() {
        let mut target = policy(CloseoutTarget::Maintenance);
        target.fee_rate = Decimal::ZERO;
        target.contract_step = dec!(0.5);
        let account = tiny(Decimal::MAX, dec!(79228162514.264338), dec!(2000000000));
        let result = plan(&account, &at_mark(Decimal::MAX), &target);
        assert_eq!(result.before.collateral, dec!(2000000000));
        assert_eq!(result.outcome, CloseoutOutcome::TargetMet);
        let taken = result.reductions[0].contracts;
        assert!(taken > dec!(39000000000000000000000000000));
        assert!(taken < dec!(40000000000000000000000000000));
        assert!(result.after.collateral >= result.after.maintenance);
        // One count less on the grid (rounded to an integer at this size)
        // does not meet the target.
        let short = plan(&account, &at_mark(taken - Decimal::ONE), &target);
        assert_eq!(short.outcome, CloseoutOutcome::LiquidityExhausted);
    }

    /// C2 (single tier): A long 10 at 110 marked
    /// 100, B long 1 at 31 marked 100, cash 67: collateral 36 < mm 55.
    /// Selling n of A at 99: cash 67 - 11.0495n, collateral 36 - 1.0495n,
    /// mm 55 - 5n. The target holds for n in [5, 6]; closing all 10 would
    /// leave a debt of 43.495.
    #[test]
    fn feasible_window_inside_the_level_flat_rule() {
        let account = LiquidationAccount {
            view: view(vec![
                perp("A-PERP", dec!(10), dec!(100)),
                perp("B-PERP", dec!(1), dec!(100)),
            ]),
            basis: BTreeMap::from([(iid("A-PERP"), dec!(1100)), (iid("B-PERP"), dec!(31))]),
            cash: dec!(67),
            resting_orders: vec![],
        };
        let liquidity = BTreeMap::from([(
            iid("A-PERP"),
            vec![LiquidityLevel {
                price: dec!(99),
                contracts: dec!(10),
            }],
        )]);
        let plan = plan(&account, &liquidity, &policy(CloseoutTarget::Maintenance));
        assert_eq!(plan.before.collateral, dec!(36));
        assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
        assert_eq!(plan.reductions.len(), 1);
        assert_eq!(plan.reductions[0].contracts, dec!(5));
        assert_eq!(plan.after.cash, dec!(11.7525));
        assert_eq!(plan.after.collateral, dec!(30.7525));
        assert_eq!(plan.after.maintenance, dec!(30));
        assert_eq!(plan.debt, dec!(0));
    }

    /// C2 (two tiers): rates 10 % below a notional of 500 and 90 %
    /// above. Long 10 at 100 marked 100, cash 140, bid 85: closing 4 gives
    /// collateral 80 < mm 140, closing 5 gives 65 >= 50.
    #[test]
    fn feasible_window_with_increasing_tiers() {
        let tiered = TieredLinearIm::new(
            RuleId::new("two-tier-test", 1),
            usdc(),
            6,
            vec![
                MarginTier {
                    floor: Decimal::ZERO,
                    initial_rate: dec!(0.1),
                    maintenance_rate: dec!(0.1),
                },
                MarginTier {
                    floor: dec!(500),
                    initial_rate: dec!(0.9),
                    maintenance_rate: dec!(0.9),
                },
            ],
        )
        .expect("rule");
        let account = LiquidationAccount {
            view: view(vec![perp("BTC-PERP", dec!(10), dec!(100))]),
            basis: BTreeMap::from([(iid("BTC-PERP"), dec!(1000))]),
            cash: dec!(140),
            resting_orders: vec![],
        };
        let mut target = policy(CloseoutTarget::Maintenance);
        target.fee_rate = Decimal::ZERO;
        let plan = plan_liquidation(&tiered, &account, &bids(&[(dec!(85), dec!(10))]), &target)
            .expect("plan");
        assert_eq!(plan.before.maintenance, dec!(500));
        assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
        assert_eq!(plan.reductions[0].contracts, dec!(5));
        assert_eq!(plan.after.cash, dec!(65));
        assert_eq!(plan.after.collateral, dec!(65));
        assert_eq!(plan.after.maintenance, dec!(50));
        assert_eq!(plan.debt, dec!(0));
    }

    /// n1: the basis must be non zero and have the sign of the position.
    #[test]
    fn basis_sign_must_match_the_position() {
        let liquidity = bids(&[(dec!(94), dec!(10))]);
        let good = policy(CloseoutTarget::Maintenance);
        for basis in [dec!(0), dec!(-1000)] {
            let mut account = long_btc(dec!(90));
            account.basis.insert(iid("BTC-PERP"), basis);
            assert!(matches!(
                plan_liquidation(&rule(), &account, &liquidity, &good),
                Err(MarginError::InvalidInput { field: "basis", .. })
            ));
        }
        let mut short = long_btc(dec!(90));
        short.view.positions[0].contracts = dec!(-10);
        assert!(matches!(
            plan_liquidation(&rule(), &short, &liquidity, &good),
            Err(MarginError::InvalidInput { field: "basis", .. })
        ));
    }
}

mod ledger_fixtures {
    use super::*;

    /// Partial closeout booked in the ledger: the cash after the plan is
    /// the ledger balance, and the ledger reconciles before and after.
    #[test]
    fn partial_closeout_reconciles() {
        let plan = plan(
            &long_btc(dec!(90)),
            &bids(&[(dec!(94), dec!(3)), (dec!(93), dec!(100))]),
            &policy(CloseoutTarget::Maintenance),
        );
        let mut ledger = ledger_with(plan.before.cash);
        assert!(ledger.reconcile().expect("before").is_clean());

        assert_eq!(book_plan(&mut ledger, &plan), vec![None]);
        let balance = ledger.balance(&usdc()).expect("balance");
        assert_eq!(balance.total, plan.after.cash);
        assert_eq!(ledger.fees_paid(&usdc()), dec!(0.141));
        assert_eq!(ledger.settled_pnl(&usdc()), dec!(-18));
        assert!(ledger.reconcile().expect("after").is_clean());

        let journal = ledger.settlement_journal();
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].delta, dec!(-18.141));
        assert_eq!(journal[0].balance_after, dec!(71.859));

        // PnL: equity - transfers = realized - fees, with no rounding gap.
        let valuation = value(
            &ledger.snapshot(),
            &MarkSet::new(0),
            &ValuationPolicy::new(usdc(), 1_000),
        )
        .expect("value");
        assert_eq!(valuation.net_pnl, Some(dec!(-18.141)));
        let attribution = valuation.attribution.expect("attribution");
        assert_eq!(attribution.realized, dec!(-18));
        assert_eq!(attribution.fees, dec!(0.141));
        assert_eq!(attribution.rounding_difference, dec!(0));

        let pnl = ledger.valuation(&BTreeMap::new()).expect("valuation");
        assert_eq!(pnl.by_asset[&usdc()].realized_pnl, dec!(-18));
        assert_eq!(pnl.by_asset[&usdc()].net_pnl, dec!(-18.141));
    }

    /// A closeout that ends in debt is booked in full: the balance goes
    /// negative, the shortfall is reported and the ledger still reconciles.
    #[test]
    fn closeout_into_debt_is_booked_in_full() {
        let mut account = long_btc(dec!(150));
        account.view.positions[0].mark = dec!(80);
        let plan = plan(
            &account,
            &bids(&[(dec!(70), dec!(100))]),
            &policy(CloseoutTarget::Initial),
        );
        let mut ledger = ledger_with(dec!(150));
        assert_eq!(book_plan(&mut ledger, &plan), vec![Some(dec!(150.35))]);
        assert_eq!(
            ledger.balance(&usdc()).expect("balance").total,
            dec!(-150.35)
        );
        assert_eq!(
            ledger.overdrafts(),
            BTreeMap::from([(usdc(), dec!(150.35))])
        );
        let reconciliation = ledger.reconcile().expect("reconcile");
        assert_eq!(reconciliation.overdrafts[&usdc()], plan.debt);
    }

    #[test]
    fn settlements_are_idempotent_by_execution_id() {
        let mut ledger = ledger_with(dec!(100));
        let settlement = DerivativeSettlement {
            id: eid("x1"),
            instrument: iid("BTC-PERP"),
            asset: usdc(),
            realized_pnl: dec!(5),
            fee: dec!(0.5),
        };
        assert!(ledger.book_settlement(&settlement).expect("first").is_new);
        let again = ledger.book_settlement(&settlement).expect("again");
        assert!(!again.is_new);
        assert_eq!(again.cashflow.delta, dec!(4.5));
        assert_eq!(ledger.balance(&usdc()).expect("balance").total, dec!(104.5));

        let mut different = settlement.clone();
        different.fee = dec!(0.6);
        assert!(matches!(
            ledger.book_settlement(&different),
            Err(MarginError::ConflictingEntry { .. })
        ));
        assert_eq!(ledger.balance(&usdc()).expect("balance").total, dec!(104.5));
        assert!(ledger.reconcile().expect("reconcile").is_clean());
    }

    #[test]
    fn settlement_and_spot_ids_share_one_namespace() {
        let btc = Asset::new("BTC").expect("asset");
        let pair = iid("BTC-USDC");
        let mut ledger = ledger_with(dec!(1000));
        ledger.register_asset(btc.clone(), 8).expect("asset");
        ledger
            .register_instrument(
                SpotInstrument::new(pair.clone(), btc, usdc(), 2, 8).expect("spot"),
            )
            .expect("instrument");
        let spot = Execution::new(eid("e1"), pair.clone(), Action::Buy, dec!(100), dec!(1));
        ledger.book_execution(&spot).expect("spot");

        let settlement = DerivativeSettlement {
            id: eid("e1"),
            instrument: iid("BTC-PERP"),
            asset: usdc(),
            realized_pnl: dec!(1),
            fee: dec!(0),
        };
        assert!(matches!(
            ledger.book_settlement(&settlement),
            Err(MarginError::ConflictingEntry { .. })
        ));
        let settlement = DerivativeSettlement {
            id: eid("s1"),
            ..settlement
        };
        ledger.book_settlement(&settlement).expect("settlement");
        let reused = Execution::new(eid("s1"), pair.clone(), Action::Sell, dec!(100), dec!(1));
        assert!(matches!(
            ledger.book_execution(&reused),
            Err(MarginError::ConflictingEntry { .. })
        ));

        // A settlement cannot target a spot instrument, and a settled
        // derivative id cannot be registered later as a spot instrument.
        let on_spot = DerivativeSettlement {
            id: eid("s2"),
            instrument: pair,
            ..settlement
        };
        assert!(matches!(
            ledger.book_settlement(&on_spot),
            Err(MarginError::InvalidInput { .. })
        ));
        let late = SpotInstrument::new(
            iid("BTC-PERP"),
            Asset::new("BTC").expect("asset"),
            usdc(),
            2,
            8,
        )
        .expect("spot");
        assert!(matches!(
            ledger.register_instrument(late),
            Err(MarginError::AlreadyRegistered { .. })
        ));
        assert!(ledger.reconcile().expect("reconcile").is_clean());
    }

    #[test]
    fn settlement_amounts_respect_the_asset_scale() {
        let mut ledger = ledger_with(dec!(100));
        let settlement = DerivativeSettlement {
            id: eid("x1"),
            instrument: iid("BTC-PERP"),
            asset: usdc(),
            realized_pnl: dec!(0.0000001),
            fee: dec!(0),
        };
        assert!(matches!(
            ledger.book_settlement(&settlement),
            Err(MarginError::PrecisionExceeded { .. })
        ));
        let unknown = DerivativeSettlement {
            asset: Asset::new("EUR").expect("asset"),
            realized_pnl: dec!(1),
            ..settlement
        };
        assert!(matches!(
            ledger.book_settlement(&unknown),
            Err(MarginError::UnknownAsset { .. })
        ));
        assert_eq!(ledger.settlement_journal(), []);
    }
}
