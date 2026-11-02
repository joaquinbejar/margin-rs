//! Staged closeout plan for an account below its margin threshold.
//!
//! [`plan_liquidation`] simulates one closeout of an account's linear
//! futures and perpetuals against supplied scenario liquidity, under one
//! [`InitialMarginModel`] (for example `xynthex-linear-im/1`), and returns a
//! [`LiquidationPlan`]. It is a pure function: it books nothing and changes
//! no state. There is no live liquidator.
//!
//! # Responsibilities
//!
//! Following Xynthex ADR 0009 (crosswalk X6), the venue core owns the
//! trigger, the account state and the cancellation of resting orders;
//! margin-rs owns the plan: what to close, in what order, how much, and
//! what is left. The [maintenance assessment](crate::maintenance) tells the
//! core where the account stands; the plan tells it what to do. The cash a
//! plan moves is posted, fill by fill, as
//! [`DerivativeSettlement`]s in the [`Ledger`](crate::ledger::Ledger)
//! ([`ForcedReduction::settlement`]).
//!
//! # Stages
//!
//! 1. **Cancel.** Every resting order of the account is cancelled first, so
//!    the closeout never races the account's own orders.
//! 2. **Reduce.** Positions are reduced one instrument at a time, the
//!    largest maintenance requirement first (ties by instrument id),
//!    walking that instrument's liquidity from the best level. On each
//!    level the plan takes the smallest number of contract steps after
//!    which the target is met, or the whole level when the level is not
//!    enough. Partial closeouts are the normal case: a position is only
//!    closed as far as the target needs.
//! 3. **Residual.** When the target is still not met (liquidity ran out,
//!    or the account is flat and still short), the remaining exposure, the
//!    deficit and the debt are reported explicitly. The plan never erases a
//!    deficit.
//!
//! [`LiquidationPlan::intents`] lists stages 1 and 2 as adapter intents.
//!
//! # Target
//!
//! [`CloseoutTarget`] fixes when the closeout stops: collateral at least
//! maintenance or initial margin, with `cash >= 0` (the exit condition of a
//! Xynthex `Default`, economics 11.1), or every position closed. Equality
//! meets the target, as it does in [`crate::maintenance`].
//!
//! # Cash conventions
//!
//! From Xynthex `docs/design/economics.md` sections 10.3 and 10.4, at
//! commit `c1b437b612b0a33610a3cbb777938c41fcdde331`, with `c` the policy
//! scale:
//!
//! - `basis` is the signed cash value of the open position at entry prices
//!   (a long has a positive basis);
//! - unrealized PnL: `round_half_even(Qc * M * mark, c) - basis`;
//! - collateral: `cash + sum(unrealized PnL)`, one exact sum;
//! - a reduction of `n` contracts at `p` has signed fill cash
//!   `+round_half_even(p * n * M, c)` when selling and the negative of that
//!   when buying; it releases `round_half_even(basis * n / |Qc|, c)` of the
//!   basis, or the whole basis when it closes the position; its realized PnL
//!   is `fill cash - released`;
//! - its fee is `trunc(fee_rate * p * n * M, c)`, one exact product
//!   truncated toward zero;
//! - cash changes by `realized PnL - fee`, booked in full even when it
//!   leaves cash negative.
//!
//! Marks stay those of the [`ExposureView`] for the whole plan: the
//! scenario's prices for the closeout are the liquidity levels.
//!
//! # Minimality
//!
//! Within one level the number of steps is found by bisection between a
//! count that does not meet the target and one that does, so the plan
//! always takes a count that meets the target and whose predecessor does
//! not. The target is monotone in the size closed up to the rounding of
//! single cash quanta, so this is the smallest such count in all practical
//! cases; it never stops short of the target.

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;

use crate::decimal::{
    MAX_SCALE, Rounding, add, check_configured_scale, check_scale, mul, mul_div_round, sub, sum,
};
use crate::error::{MarginError, Result};
use crate::ids::{AccountId, Asset, ExecutionId, InstrumentId, OrderId, VenueId};
use crate::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement, RuleId,
};
use crate::ledger::{DerivativeSettlement, Side};

/// When the closeout stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseoutTarget {
    /// `collateral >= maintenance` and `cash >= 0`.
    Maintenance,
    /// `collateral >= initial` and `cash >= 0`.
    Initial,
    /// Every position closed.
    Flat,
}

/// How the closeout is simulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidationPolicy {
    /// When to stop.
    pub target: CloseoutTarget,
    /// Fee charged on every forced fill, as a fraction of its notional, in
    /// `[0, 1)`.
    pub fee_rate: Decimal,
    /// Smallest number of contracts the plan trades at a time, positive.
    /// The last step of a position or level may be smaller.
    pub contract_step: Decimal,
    /// Scale of the cash quantum (USDC: 6). Use the scale the margin rule
    /// rounds to, so cash, PnL and margins share one quantum.
    pub scale: u32,
}

/// Liquidity available at one price to close a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidityLevel {
    /// Execution price, positive.
    pub price: Decimal,
    /// Contracts available at that price, positive.
    pub contracts: Decimal,
}

/// The account the plan closes out, in the rule's currency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidationAccount {
    /// Positions and marks.
    pub view: ExposureView,
    /// Signed basis of each open position (see the
    /// [cash conventions](self#cash-conventions)).
    pub basis: BTreeMap<InstrumentId, Decimal>,
    /// Cash in the rule's currency: collateral other than the unrealized
    /// PnL of the positions in `view`.
    pub cash: Decimal,
    /// The account's resting orders, all cancelled by the plan.
    pub resting_orders: Vec<OrderId>,
}

/// One forced fill of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForcedReduction {
    /// Instrument.
    pub instrument: InstrumentId,
    /// `Sell` to reduce a long, `Buy` to reduce a short.
    pub side: Side,
    /// Index of the liquidity level used.
    pub level: usize,
    /// Execution price.
    pub price: Decimal,
    /// Contracts closed, positive.
    pub contracts: Decimal,
    /// Signed fill cash: positive when selling.
    pub fill_cash: Decimal,
    /// Part of the basis released.
    pub released_basis: Decimal,
    /// `fill_cash - released_basis`, gross of the fee.
    pub realized_pnl: Decimal,
    /// Fee.
    pub fee: Decimal,
    /// Signed contracts left after this fill.
    pub contracts_after: Decimal,
}

impl ForcedReduction {
    /// The ledger posting of this fill, under the execution id the venue
    /// gave it, in the plan's currency.
    #[must_use]
    pub fn settlement(&self, id: ExecutionId, currency: Asset) -> DerivativeSettlement {
        DerivativeSettlement {
            id,
            instrument: self.instrument.clone(),
            asset: currency,
            realized_pnl: self.realized_pnl,
            fee: self.fee,
        }
    }
}

/// A position as the plan values it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPosition {
    /// Instrument.
    pub instrument: InstrumentId,
    /// Signed contracts.
    pub contracts: Decimal,
    /// Signed basis.
    pub basis: Decimal,
    /// `round_half_even(contracts * multiplier * mark) - basis`.
    pub unrealized_pnl: Decimal,
}

/// Account figures before or after the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// Cash.
    pub cash: Decimal,
    /// `cash + sum(unrealized PnL)`.
    pub collateral: Decimal,
    /// Initial margin of the open positions.
    pub initial: Decimal,
    /// Maintenance margin of the open positions.
    pub maintenance: Decimal,
    /// Open positions, in the order of the view.
    pub positions: Vec<PlanPosition>,
}

/// How the plan ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseoutOutcome {
    /// The account met the target before any action: the plan is empty.
    AlreadyMet,
    /// The plan meets the target.
    TargetMet,
    /// Every position is closed and the target is still not met.
    FlatWithDeficit,
    /// Liquidity ran out with positions still open and the target not met.
    LiquidityExhausted,
}

/// What an adapter must do to execute the plan, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseoutIntent {
    /// Cancel a resting order.
    Cancel {
        /// Order to cancel.
        order: OrderId,
    },
    /// Reduce-only order: it may only shrink the position, never flip it.
    Reduce {
        /// Instrument.
        instrument: InstrumentId,
        /// Side.
        side: Side,
        /// Contracts, positive.
        contracts: Decimal,
        /// Worst acceptable price: the last level the plan uses.
        limit_price: Decimal,
    },
}

/// Result of [`plan_liquidation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidationPlan {
    /// Rule the margins come from.
    pub rule: RuleId,
    /// Account.
    pub account: AccountId,
    /// Venue.
    pub venue: VenueId,
    /// Currency of every amount.
    pub currency: Asset,
    /// Target of the closeout.
    pub target: CloseoutTarget,
    /// Stage 1: resting orders to cancel.
    pub cancels: Vec<OrderId>,
    /// Stage 2: forced fills, in execution order.
    pub reductions: Vec<ForcedReduction>,
    /// The account before the plan.
    pub before: AccountSummary,
    /// The account after every forced fill.
    pub after: AccountSummary,
    /// Contracts (absolute) left open in instruments whose liquidity ran
    /// out before the target was met.
    pub unfilled: BTreeMap<InstrumentId, Decimal>,
    /// How the plan ends.
    pub outcome: CloseoutOutcome,
    /// `max(0, target margin - collateral)` after the plan (the target
    /// margin is zero for [`CloseoutTarget::Flat`]).
    pub deficit: Decimal,
    /// `max(0, -cash)` after the plan: the debt the account still owes.
    pub debt: Decimal,
}

impl LiquidationPlan {
    /// Stages 1 and 2 as adapter intents: every cancel, then one
    /// reduce-only order per instrument, in the order the plan reduces
    /// them, for the exact sum of that instrument's forced fills.
    ///
    /// # Errors
    ///
    /// [`MarginError::Arithmetic`] if a sum cannot be represented (it is
    /// at most the position the plan started from).
    pub fn intents(&self) -> Result<Vec<CloseoutIntent>> {
        let mut intents: Vec<CloseoutIntent> = self
            .cancels
            .iter()
            .map(|order| CloseoutIntent::Cancel {
                order: order.clone(),
            })
            .collect();
        // Per instrument, in first reduction order: side, contracts of every
        // fill and the last (worst) price.
        let mut order: Vec<&InstrumentId> = Vec::new();
        let mut fills: BTreeMap<&InstrumentId, (Side, Vec<Decimal>, Decimal)> = BTreeMap::new();
        for reduction in &self.reductions {
            let entry = fills.entry(&reduction.instrument).or_insert_with(|| {
                order.push(&reduction.instrument);
                (reduction.side, Vec::new(), reduction.price)
            });
            entry.1.push(reduction.contracts);
            entry.2 = reduction.price;
        }
        for instrument in order {
            let (side, contracts, limit_price) = &fills[instrument];
            intents.push(CloseoutIntent::Reduce {
                instrument: instrument.clone(),
                side: *side,
                contracts: sum(contracts, "intent contracts")?,
                limit_price: *limit_price,
            });
        }
        Ok(intents)
    }
}

#[derive(Debug, Clone)]
struct Open {
    exposure: ExposurePosition,
    basis: Decimal,
}

#[derive(Debug, Clone)]
struct State {
    cash: Decimal,
    positions: Vec<Open>,
}

struct Planner<'a, M> {
    model: &'a M,
    policy: &'a LiquidationPolicy,
    account: &'a LiquidationAccount,
}

impl<M: InitialMarginModel> Planner<'_, M> {
    fn requirement(&self, state: &State) -> Result<MarginRequirement> {
        self.model.requirement(&ExposureView {
            account: self.account.view.account.clone(),
            venue: self.account.view.venue.clone(),
            positions: state
                .positions
                .iter()
                .filter(|p| !p.exposure.contracts.is_zero())
                .map(|p| p.exposure.clone())
                .collect(),
        })
    }

    fn summary(&self, state: &State) -> Result<AccountSummary> {
        let requirement = self.requirement(state)?;
        let mut positions = Vec::new();
        let mut terms = vec![state.cash];
        for open in &state.positions {
            if open.exposure.contracts.is_zero() {
                continue;
            }
            let value = mul_div_round(
                &[
                    open.exposure.contracts,
                    open.exposure.multiplier,
                    open.exposure.mark,
                ],
                &[],
                self.policy.scale,
                Rounding::HalfEven,
                "Qc * M * mark",
            )?;
            terms.push(value);
            terms.push(-open.basis);
            positions.push(PlanPosition {
                instrument: open.exposure.instrument.clone(),
                contracts: open.exposure.contracts,
                basis: open.basis,
                unrealized_pnl: sub(value, open.basis, "value - basis")?,
            });
        }
        Ok(AccountSummary {
            cash: state.cash,
            collateral: sum(&terms, "collateral")?,
            initial: requirement.initial,
            maintenance: requirement.maintenance,
            positions,
        })
    }

    fn met(&self, summary: &AccountSummary) -> bool {
        let solvent = summary.cash >= Decimal::ZERO;
        match self.policy.target {
            CloseoutTarget::Maintenance => solvent && summary.collateral >= summary.maintenance,
            CloseoutTarget::Initial => solvent && summary.collateral >= summary.initial,
            CloseoutTarget::Flat => summary.positions.is_empty(),
        }
    }

    /// Closes `contracts` of position `index` at `price`.
    fn reduce(
        &self,
        state: &State,
        index: usize,
        level: usize,
        price: Decimal,
        contracts: Decimal,
    ) -> Result<(State, ForcedReduction)> {
        let scale = self.policy.scale;
        let open = &state.positions[index];
        let position = open.exposure.contracts;
        let size = position.abs();
        let notional = mul_div_round(
            &[price, contracts, open.exposure.multiplier],
            &[],
            scale,
            Rounding::HalfEven,
            "p * n * M",
        )?;
        let (side, fill_cash, contracts_after) = if position > Decimal::ZERO {
            (Side::Sell, notional, sub(position, contracts, "Qc - n")?)
        } else {
            (Side::Buy, -notional, add(position, contracts, "Qc + n")?)
        };
        let released_basis = if contracts == size {
            open.basis
        } else {
            mul_div_round(
                &[open.basis, contracts],
                &[size],
                scale,
                Rounding::HalfEven,
                "basis * n / |Qc|",
            )?
        };
        let realized_pnl = sub(fill_cash, released_basis, "fill cash - released")?;
        let fee = mul_div_round(
            &[
                self.policy.fee_rate,
                price,
                contracts,
                open.exposure.multiplier,
            ],
            &[],
            scale,
            Rounding::TowardZero,
            "fee",
        )?;
        let mut next = state.clone();
        next.cash = sum(&[state.cash, realized_pnl, -fee], "cash + realized - fee")?;
        let updated = &mut next.positions[index];
        updated.exposure.contracts = contracts_after;
        updated.basis = sub(open.basis, released_basis, "basis - released")?;
        let reduction = ForcedReduction {
            instrument: open.exposure.instrument.clone(),
            side,
            level,
            price,
            contracts,
            fill_cash,
            released_basis,
            realized_pnl,
            fee,
            contracts_after,
        };
        Ok((next, reduction))
    }

    fn validate(&self, liquidity: &BTreeMap<InstrumentId, Vec<LiquidityLevel>>) -> Result<()> {
        let policy = self.policy;
        check_configured_scale(policy.scale, "scale")?;
        if policy.fee_rate < Decimal::ZERO || policy.fee_rate >= Decimal::ONE {
            return Err(MarginError::invalid_input("fee_rate", "must be in [0, 1)"));
        }
        if policy.contract_step <= Decimal::ZERO {
            return Err(MarginError::invalid_input(
                "contract_step",
                "must be positive",
            ));
        }
        let account = self.account;
        check_scale(account.cash, policy.scale, "cash")?;
        let mut orders = BTreeSet::new();
        if !account.resting_orders.iter().all(|o| orders.insert(o)) {
            return Err(MarginError::invalid_input(
                "resting_orders",
                "must not repeat an order",
            ));
        }
        let open: BTreeMap<&InstrumentId, Decimal> = account
            .view
            .positions
            .iter()
            .filter(|p| !p.contracts.is_zero())
            .map(|p| (&p.instrument, p.contracts))
            .collect();
        for (instrument, basis) in &account.basis {
            if !open.contains_key(instrument) {
                return Err(MarginError::invalid_input(
                    "basis",
                    "given for an instrument without an open position",
                ));
            }
            check_scale(*basis, policy.scale, "basis")?;
        }
        if open.keys().any(|i| !account.basis.contains_key(*i)) {
            return Err(MarginError::invalid_input(
                "basis",
                "missing for an open position",
            ));
        }
        for (instrument, levels) in liquidity {
            let Some(contracts) = open.get(instrument) else {
                continue;
            };
            for level in levels {
                if level.price <= Decimal::ZERO || level.contracts <= Decimal::ZERO {
                    return Err(MarginError::invalid_input(
                        "liquidity",
                        "prices and contracts must be positive",
                    ));
                }
                check_scale(level.price, MAX_SCALE, "liquidity price")?;
            }
            // Best level first: highest bid to sell a long, lowest ask to
            // buy back a short.
            let ordered = levels.windows(2).all(|w| {
                if *contracts > Decimal::ZERO {
                    w[0].price >= w[1].price
                } else {
                    w[0].price <= w[1].price
                }
            });
            if !ordered {
                return Err(MarginError::invalid_input(
                    "liquidity",
                    "levels must go from the best price to the worst",
                ));
            }
        }
        Ok(())
    }
}

/// Number of contracts in `k` steps, capped at `available`.
fn steps(k: Decimal, step: Decimal, available: Decimal) -> Result<Decimal> {
    Ok(mul(k, step, "k * step")?.min(available))
}

/// Plans the closeout of `account`. See the [module docs](self).
///
/// `liquidity` gives, per instrument, the levels available on the closing
/// side, best first; an open position without an entry has no liquidity.
/// Entries for instruments without an open position are ignored.
///
/// # Errors
///
/// - [`MarginError::InvalidInput`] for an invalid policy (fee rate outside
///   `[0, 1)`, non positive step), a basis missing for an open position or
///   given for a closed one, repeated resting orders, or liquidity levels
///   that are not positive or not ordered from the best price.
/// - [`MarginError::PrecisionExceeded`] if cash or a basis has more digits
///   than the policy scale.
/// - Every error of the model's
///   [`requirement`](InitialMarginModel::requirement) on the view.
/// - [`MarginError::Arithmetic`] if an amount cannot be represented.
pub fn plan_liquidation<M: InitialMarginModel>(
    model: &M,
    account: &LiquidationAccount,
    liquidity: &BTreeMap<InstrumentId, Vec<LiquidityLevel>>,
    policy: &LiquidationPolicy,
) -> Result<LiquidationPlan> {
    let planner = Planner {
        model,
        policy,
        account,
    };
    planner.validate(liquidity)?;
    let initial_requirement = model.requirement(&account.view)?;

    let mut state = State {
        cash: account.cash,
        positions: account
            .view
            .positions
            .iter()
            .map(|p| Open {
                exposure: p.clone(),
                basis: account
                    .basis
                    .get(&p.instrument)
                    .copied()
                    .unwrap_or_default(),
            })
            .collect(),
    };
    let before = planner.summary(&state)?;
    let currency = initial_requirement.currency.clone();
    let plan = |cancels, reductions, after: AccountSummary, unfilled, outcome| {
        let target_margin = match policy.target {
            CloseoutTarget::Maintenance => after.maintenance,
            CloseoutTarget::Initial => after.initial,
            CloseoutTarget::Flat => Decimal::ZERO,
        };
        let deficit = sub(target_margin, after.collateral, "target - collateral")?;
        Ok(LiquidationPlan {
            rule: initial_requirement.rule.clone(),
            account: account.view.account.clone(),
            venue: account.view.venue.clone(),
            currency: currency.clone(),
            target: policy.target,
            cancels,
            reductions,
            before: before.clone(),
            debt: (-after.cash).max(Decimal::ZERO),
            after,
            unfilled,
            outcome,
            deficit: deficit.max(Decimal::ZERO),
        })
    };
    if planner.met(&before) {
        return plan(
            Vec::new(),
            Vec::new(),
            before.clone(),
            BTreeMap::new(),
            CloseoutOutcome::AlreadyMet,
        );
    }

    // Largest maintenance first, ties by instrument id.
    let mut order: Vec<(Decimal, &InstrumentId)> = initial_requirement
        .positions
        .iter()
        .filter(|p| !p.contracts.is_zero())
        .map(|p| (p.maintenance, &p.instrument))
        .collect();
    order.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

    let mut reductions = Vec::new();
    let mut unfilled = BTreeMap::new();
    let mut current = before.clone();
    'instruments: for (_, instrument) in order {
        let index = state
            .positions
            .iter()
            .position(|p| &p.exposure.instrument == instrument)
            .ok_or_else(|| MarginError::invalid_input("view", "position not found"))?;
        let levels = liquidity.get(instrument).map(Vec::as_slice).unwrap_or(&[]);
        for (level, available) in levels.iter().enumerate() {
            if planner.met(&current) {
                break 'instruments;
            }
            let size = state.positions[index].exposure.contracts.abs();
            if size.is_zero() {
                break;
            }
            let available_here = available.contracts.min(size);
            let max_steps = mul_div_round(
                &[available_here],
                &[policy.contract_step],
                0,
                Rounding::AwayFromZero,
                "available / step",
            )?;
            let try_steps = |k: Decimal| -> Result<(State, ForcedReduction, AccountSummary)> {
                let n = steps(k, policy.contract_step, available_here)?;
                let (next, reduction) = planner.reduce(&state, index, level, available.price, n)?;
                let summary = planner.summary(&next)?;
                Ok((next, reduction, summary))
            };
            let mut chosen = try_steps(max_steps)?;
            if planner.met(&chosen.2) {
                // Bisection: `low` does not meet the target, `high` does.
                let mut low = Decimal::ZERO;
                let mut high = max_steps;
                while sub(high, low, "high - low")? > Decimal::ONE {
                    let middle = mul_div_round(
                        &[add(low, high, "low + high")?],
                        &[Decimal::TWO],
                        0,
                        Rounding::TowardZero,
                        "(low + high) / 2",
                    )?;
                    let attempt = try_steps(middle)?;
                    if planner.met(&attempt.2) {
                        high = middle;
                        chosen = attempt;
                    } else {
                        low = middle;
                    }
                }
            }
            let (next, reduction, summary) = chosen;
            state = next;
            reductions.push(reduction);
            current = summary;
        }
        if planner.met(&current) {
            break;
        }
        let left = state.positions[index].exposure.contracts.abs();
        if !left.is_zero() {
            unfilled.insert(instrument.clone(), left);
        }
    }

    let outcome = if planner.met(&current) {
        CloseoutOutcome::TargetMet
    } else if current.positions.is_empty() {
        CloseoutOutcome::FlatWithDeficit
    } else {
        CloseoutOutcome::LiquidityExhausted
    };
    plan(
        account.resting_orders.clone(),
        reductions,
        current,
        unfilled,
        outcome,
    )
}
