//! Staged closeout plan for an account below its margin threshold.
//!
//! [`plan_liquidation`] simulates one closeout of an account's linear or
//! inverse futures and perpetuals against supplied scenario liquidity,
//! under one [`PiecewiseLinearMargin`] rule (any
//! [`TieredLinearIm`](crate::rules::TieredLinearIm) or
//! [`TieredInverseIm`](crate::rules::TieredInverseIm)), and returns a
//! [`LiquidationPlan`]. It is a pure function: it books nothing and changes
//! no state. There is no live liquidator.
//!
//! # Responsibilities
//!
//! When a venue keeps its own account state, it owns the trigger, that
//! state and the cancellation of resting orders; margin-rs owns the plan: what to close, in what order, how much, and
//! what is left. The [maintenance assessment](crate::maintenance) tells the
//! core where the account stands; the plan tells it what to do.
//!
//! # From and back to the ledger
//!
//! When the positions live in the [`Ledger`](crate::ledger::Ledger) as
//! futures and perpetuals, linear or inverse, [`LiquidationAccount::from_ledger`] reads
//! the account from a [`LedgerSnapshot`] and each forced fill is booked back
//! as an [`Execution`] ([`ForcedReduction::execution`]) with
//! [`Ledger::book_execution_allowing_overdraft`](crate::ledger::Ledger::book_execution_allowing_overdraft).
//! The ledger books derivatives with the
//! [cash conventions](self#cash-conventions) below, so booking the plan
//! leaves exactly the cash, contracts and basis of
//! [`LiquidationPlan::after`]. For positions kept outside the ledger, the
//! account is built by hand and the cash of each fill is posted as a
//! [`DerivativeSettlement`] ([`ForcedReduction::settlement`]).
//!
//! # Stages
//!
//! 1. **Cancel.** Every resting order of the account is cancelled first, so
//!    the closeout never races the account's own orders.
//! 2. **Reduce.** Positions are reduced one instrument at a time, the
//!    largest maintenance requirement first (ties by instrument id),
//!    walking that instrument's liquidity from the best level. On each
//!    level the plan takes the smallest number of contract steps after
//!    which the target is met (see [search](self#search-within-a-level)),
//!    or the whole level when no count on it meets the target. Partial
//!    closeouts are the normal case: a position is only closed as far as
//!    the target needs.
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
//! maintenance or initial margin, with `cash >= 0` (the exit condition of
//! [`AccountState::Default`](crate::maintenance::AccountState::Default)),
//! or every position closed. Equality
//! meets the target, as it does in [`crate::maintenance`].
//!
//! # Cash conventions
//!
//! With `c` the rule's
//! [`cash_scale`](PiecewiseLinearMargin::cash_scale) (so cash, PnL and
//! margins share one quantum):
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
//! An inverse position (contract value `CV`, settled in the base, which is
//! then the plan's currency) uses its value in the base instead:
//!
//! - `basis` is the signed value at entry prices (a long is positive);
//! - unrealized PnL: `basis - round_half_even(Qc * CV / mark, c)`;
//! - a reduction of `n` at `p` has the signed fill value
//!   `+round_half_even(n * CV / p, c)` when selling and the negative when
//!   buying (in [`ForcedReduction::fill_cash`]); it releases the basis as
//!   above; its realized PnL is `released - fill value`;
//! - its fee is `trunc(fee_rate * n * CV / p, c)`, in the base.
//!
//! Marks stay those of the [`ExposureView`] for the whole plan: the
//! scenario's prices for the closeout are the liquidity levels.
//!
//! # Search within a level
//!
//! On one level, with `n` the contracts closed there, `cash(n)` is linear
//! (price, released basis and fee are all proportional to `n`) and
//! `excess(n) = collateral(n) - margin(n)` is concave: collateral is linear
//! and, by the [`PiecewiseLinearMargin`] contract, the margin is a convex
//! piecewise linear function of the notional, which falls linearly in `n`.
//! This holds for inverse positions too: their PnL is convex in the price,
//! but within a level the price is fixed, so the fill value `n * CV / p`,
//! the fee and the remaining notional `(|Qc| - n) * CV / mark` are all
//! linear in `n`.
//! Unrounded, the counts that meet the target therefore form one interval.
//! The booked figures are rounded (half-even fill value and released
//! basis, a truncated fee, a margin rounded up), so with a coarse cash
//! quantum rounding can open windows of counts that meet the target where
//! the unrounded figures do not, and close others. The plan finds the
//! smallest count that meets the target **as booked**, with a certified
//! search:
//!
//! 1. With `q` the cash quantum, the booked cash differs from the
//!    unrounded one by less than `2 q` (fill value and released basis half
//!    a quantum each, fee under one) and the booked excess by less than
//!    `3 q` (fill value and the remaining position's value half a quantum
//!    each, fee and margin under one; the released basis cancels). Only
//!    the position being reduced changes; the others enter with their
//!    booked figures.
//! 2. The unrounded cash and excess are evaluated exactly, in wide
//!    integers, from the contract terms and the rule's tiers
//!    ([`PiecewiseLinearMargin::margin_tiers`]), never from the rounded
//!    margin. So `J = {cash >= -2q and excess >= -3q}` contains every count
//!    that meets the target and `J' = {cash >= 2q and excess >= 3q}` only
//!    counts that do. Both are intervals of the grid (the cash is linear,
//!    the excess concave); their ends are found by exact bisection, and the
//!    excess peaks at an end of the level or next to a tier floor.
//! 3. The plan walks `J` in increasing order, from its start to the start
//!    of `J'`, evaluating each count exactly as it would be booked, and
//!    takes the first that meets the target: the smallest on the level.
//!    If `J` is empty no count meets the target, and the plan takes the
//!    whole level. A count that would leave a position a `Decimal` cannot
//!    hold cannot be booked: the walk jumps over such counts
//!    arithmetically to the next one that can be, without visiting them
//!    (a congruence per number of decimals the contracts left can have;
//!    a step far finer than the position's precision can leave whole
//!    contracts as the only such counts).
//!
//! The walk is short when the quantum is fine against what one step
//! changes (a few counts for real cash quanta). It shares one budget per
//! plan, [`SEARCH_BUDGET`] exact evaluations. When the budget runs out:
//!
//! - if `J'` is not empty the plan takes its first count that can be
//!   booked, which meets the target, and reports
//!   [`LiquidationPlan::minimal`] `== false`. The first count of `J'` is
//!   preceded by one that is not certified: unrounded, its cash is below
//!   `2q` or its excess below `3q`. This bounds the money, not the
//!   contracts: with a step that changes almost nothing, the certified
//!   count can be many contracts above the smallest one. When no count
//!   that can be booked lies between the last one evaluated and the
//!   certified one, the certified count is the smallest and `minimal`
//!   stays true. The certified count is evaluated once more and must meet
//!   the target;
//! - otherwise (`J'` empty, or no count of it can be booked) the plan fails
//!   with [`MarginError::CloseoutSearchLimit`]; nothing is planned.
//!
//! If the exact evaluation itself does not fit the wide integers, `J` is
//! the whole level and no count is certified in advance. The search is
//! checked against an exhaustive oracle that tries every count
//! (`tests/closeout_oracle.rs`: linear and inverse, cash at 0, 1, 2 and 8
//! decimals, one to three levels, one and two tiers, with and without fees,
//! long and short, cash swept across the target).
//!
//! Counts live on the grid of [`LiquidationPolicy::contract_step`],
//! indexed in wide integers, so no product of a step count or bisection
//! midpoint can overflow. Every count a level can take, `k * step` below
//! the level's size (or the position's, if smaller), must be exact at the
//! step's scale: [`plan_liquidation`] refuses the policy with
//! [`MarginError::InvalidInput`] for `contract_step` before planning
//! anything otherwise. The check is exact and immediate: with the step
//! normalized (`m` its smallest mantissa), the last regular index,
//! `ceil(size / step) - 1`, times `m` must not exceed `2^96 - 1`. It is a
//! sufficient condition, needing no product to be normalized. So counts are
//! exact multiples of the step, and grow with the index; the last count of
//! a level is its size itself, a multiple of the step or not.

use core::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;

use crate::decimal::grid::{ContractGrid, GridIndex};
use crate::decimal::{
    MAX_SCALE, Rounding, Wide, add, check_scale, compare_exact, mul_div_round, neg, sub, sum,
    sum_products_exact,
};
use crate::direction::Dir;
use crate::error::{MarginError, Result};
use crate::ids::{AccountId, Asset, ExecutionId, InstrumentId, OrderId, VenueId};
use crate::initial_margin::{
    ExposurePosition, ExposureView, MarginRequirement, PiecewiseLinearMargin, RuleId, check_size,
};
use crate::instrument::ContractSize;
use crate::ledger::derivative::{market_value, unrealized_pnl};
use crate::ledger::{Action, DerivativeSettlement, Execution, LedgerSnapshot};

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
    /// [cash conventions](self#cash-conventions)): zero, or of the sign of
    /// the position.
    pub basis: BTreeMap<InstrumentId, Decimal>,
    /// Cash in the rule's currency: collateral other than the unrealized
    /// PnL of the positions in `view`.
    pub cash: Decimal,
    /// The account's resting orders, all cancelled by the plan.
    pub resting_orders: Vec<OrderId>,
}

impl LiquidationAccount {
    /// The account of `snapshot`'s ledger in `currency`, at instrument
    /// `marks` (per unit of the underlying, in `currency`), with no resting
    /// orders: set [`LiquidationAccount::resting_orders`] before planning.
    ///
    /// - Positions: every open future and perpetual, linear or inverse,
    ///   with its contracts, the contract size of the ledger's instrument
    ///   and its mark (for an inverse one, the plan's currency is the
    ///   base);
    ///   the instrument definitions of the snapshot are authoritative and
    ///   marks never change them.
    /// - Basis: each position's cost basis.
    /// - Cash: the **total** balance of `currency`, reservations included
    ///   (a closeout is not limited to the available part); zero without a
    ///   balance row.
    ///
    /// Spot holdings, in `currency` or any other asset, are not part of the
    /// plan: the plan closes derivatives only, and its collateral is the
    /// cash plus their unrealized PnL. Every open derivative must settle in
    /// `currency`; one in another asset is an error, never left out.
    ///
    /// `as_of_ms` is when the closeout runs: a future whose expiry is at or
    /// before it can no longer trade (a forced fill would be
    /// [`MarginError::ExecutedAfterExpiry`]) and must be settled first with
    /// [`Ledger::settle_expiry`](crate::ledger::Ledger::settle_expiry);
    /// settled futures are flat and take no part.
    ///
    /// # Errors
    ///
    /// - [`MarginError::UnknownAsset`] if `currency` is not registered.
    /// - [`MarginError::InvalidInput`] if the scale of `currency` is not the
    ///   rule's [`cash_scale`](PiecewiseLinearMargin::cash_scale) (the plan
    ///   and the ledger must round to the same quantum), or a mark is not
    ///   positive.
    /// - [`MarginError::CurrencyMismatch`] for an open derivative settled in
    ///   another asset.
    /// - [`MarginError::ExpiredUnsettled`] for an open future whose expiry
    ///   is at or before `as_of_ms`.
    /// - [`MarginError::MissingMark`] for an open derivative without a mark.
    /// - [`MarginError::PrecisionExceeded`] for a mark with more than 18
    ///   decimals.
    /// - [`MarginError::UnknownInstrument`] for a position whose instrument
    ///   the snapshot does not define.
    pub fn from_ledger<M: PiecewiseLinearMargin>(
        model: &M,
        snapshot: &LedgerSnapshot,
        account: AccountId,
        venue: VenueId,
        currency: &Asset,
        marks: &BTreeMap<InstrumentId, Decimal>,
        as_of_ms: u64,
    ) -> Result<Self> {
        let scale = *snapshot
            .assets
            .get(currency)
            .ok_or_else(|| MarginError::UnknownAsset {
                asset: currency.clone(),
            })?;
        if scale != model.cash_scale() {
            return Err(MarginError::invalid_input(
                "currency",
                "its ledger scale must be the rule's cash scale",
            ));
        }
        let mut positions = Vec::new();
        let mut basis = BTreeMap::new();
        for (id, position) in &snapshot.positions {
            if position.quantity.is_zero() {
                continue;
            }
            let instrument =
                snapshot
                    .instruments
                    .get(id)
                    .ok_or_else(|| MarginError::UnknownInstrument {
                        instrument: id.clone(),
                    })?;
            let (Some(product), Some(contract)) =
                (instrument.derivative_product(), instrument.derivative())
            else {
                // Spot holdings are not part of the plan.
                continue;
            };
            if let Some(expiry_ms) = contract.expiry_ms
                && expiry_ms <= as_of_ms
            {
                return Err(MarginError::ExpiredUnsettled {
                    instrument: id.clone(),
                    expiry_ms,
                    as_of_ms,
                });
            }
            if contract.settlement != *currency {
                return Err(MarginError::CurrencyMismatch {
                    field: "settlement",
                    expected: currency.clone(),
                    found: contract.settlement.clone(),
                });
            }
            let mark = *marks.get(id).ok_or_else(|| MarginError::MissingMark {
                instrument: id.clone(),
            })?;
            if mark <= Decimal::ZERO {
                return Err(MarginError::invalid_input(
                    "instrument mark",
                    "must be positive",
                ));
            }
            check_scale(mark, MAX_SCALE, "instrument mark")?;
            positions.push(ExposurePosition {
                instrument: id.clone(),
                product,
                settlement: currency.clone(),
                contracts: position.quantity,
                size: contract.size,
                mark,
            });
            basis.insert(id.clone(), position.cost_basis);
        }
        let cash = snapshot
            .balances
            .get(currency)
            .map_or(Decimal::ZERO, |b| b.total);
        Ok(Self {
            view: ExposureView {
                account,
                venue,
                positions,
            },
            basis,
            cash,
            resting_orders: Vec::new(),
        })
    }
}

/// One forced fill of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForcedReduction {
    /// Instrument.
    pub instrument: InstrumentId,
    /// `Sell` to reduce a long, `Buy` to reduce a short.
    pub side: Action,
    /// Index of the liquidity level used.
    pub level: usize,
    /// Execution price.
    pub price: Decimal,
    /// Contracts closed, positive.
    pub contracts: Decimal,
    /// Signed fill cash, positive when selling: `p * n * M` for a linear
    /// position; for an inverse one, the fill value `n * CV / p` in the
    /// base, with the same sign.
    pub fill_cash: Decimal,
    /// Part of the basis released.
    pub released_basis: Decimal,
    /// Gross of the fee: `fill_cash - released_basis` for a linear
    /// position, `released_basis - fill_cash` for an inverse one.
    pub realized_pnl: Decimal,
    /// Fee.
    pub fee: Decimal,
    /// Signed contracts left after this fill.
    pub contracts_after: Decimal,
}

impl ForcedReduction {
    /// This fill as a ledger execution of a future or perpetual,
    /// under the execution id the venue gave it: side, price, contracts and
    /// a fee in the plan's `currency`, executed at `executed_at_ms`. Booked
    /// with
    /// [`Ledger::book_execution_allowing_overdraft`](crate::ledger::Ledger::book_execution_allowing_overdraft)
    /// (it is mandatory), it moves cash by exactly `realized_pnl - fee` and
    /// releases `released_basis`, as the plan does. See
    /// [from and back to the ledger](self#from-and-back-to-the-ledger).
    #[must_use]
    pub fn execution(&self, id: ExecutionId, currency: Asset, executed_at_ms: u64) -> Execution {
        Execution::new(
            id,
            self.instrument.clone(),
            self.side,
            self.price,
            self.contracts,
        )
        .with_fee(currency, self.fee)
        .with_executed_at(executed_at_ms)
    }

    /// The cash of this fill as a [`DerivativeSettlement`], for positions
    /// kept outside the ledger, under the execution id the venue gave it,
    /// in the plan's currency.
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
        /// Action.
        side: Action,
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
    /// True when every forced fill closes the smallest count of its level
    /// that meets the target. False when the search ran out of its budget
    /// ([`SEARCH_BUDGET`]) on a level and took the first count certified to
    /// meet the target instead (see the
    /// [search](self#search-within-a-level)).
    pub minimal: bool,
}

impl LiquidationPlan {
    /// Stages 1 and 2 as adapter intents: every cancel, then one
    /// reduce-only order per instrument, in the order the plan reduces
    /// them, for the exact sum of that instrument's forced fills.
    ///
    /// # Errors
    ///
    /// - [`MarginError::UnsupportedAction`] if a reduction's side is not
    ///   `Buy` or `Sell`. Plans built by [`plan_liquidation`] never have one,
    ///   but the fields are public; every reduction is checked before any
    ///   intent is built.
    /// - [`MarginError::Arithmetic`] if a sum cannot be represented (it is
    ///   at most the position the plan started from).
    pub fn intents(&self) -> Result<Vec<CloseoutIntent>> {
        for reduction in &self.reductions {
            Dir::from_action(reduction.side, "reduction side")?;
        }
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
        let mut fills: BTreeMap<&InstrumentId, (Action, Vec<Decimal>, Decimal)> = BTreeMap::new();
        for reduction in &self.reductions {
            let entry = fills.entry(&reduction.instrument).or_insert_with(|| {
                order.push(&reduction.instrument);
                (reduction.side, Vec::new(), reduction.price)
            });
            entry.1.push(reduction.contracts);
            entry.2 = reduction.price;
        }
        for instrument in order {
            let (side, contracts, limit_price) = fills
                .get(instrument)
                .ok_or_else(|| MarginError::invalid_input("plan", "reduction not grouped"))?;
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

impl State {
    /// The position at `index`, which the planner found in this state.
    fn open(&self, index: usize) -> Result<&Open> {
        self.positions
            .get(index)
            .ok_or_else(|| MarginError::invalid_input("view", "position not found"))
    }

    fn open_mut(&mut self, index: usize) -> Result<&mut Open> {
        self.positions
            .get_mut(index)
            .ok_or_else(|| MarginError::invalid_input("view", "position not found"))
    }
}

struct Planner<'a, M> {
    model: &'a M,
    policy: &'a LiquidationPolicy,
    account: &'a LiquidationAccount,
    scale: u32,
    /// Exact evaluations left for the whole plan.
    budget: Cell<u32>,
    /// The budget it started with, reported in the error.
    limit: u32,
    /// False once a level's count was certified valid but not proven the
    /// smallest.
    minimal: Cell<bool>,
}

impl<M: PiecewiseLinearMargin> Planner<'_, M> {
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
            let size = open.exposure.size;
            let value = market_value(
                open.exposure.contracts,
                size,
                open.exposure.mark,
                self.scale,
            )?;
            // upnl: value - basis (linear), basis - value (inverse), summed
            // with the cash in one exact step.
            match size {
                ContractSize::Multiplier(_) => {
                    terms.push(value);
                    terms.push(neg(open.basis));
                }
                ContractSize::ContractValue(_) => {
                    terms.push(open.basis);
                    terms.push(neg(value));
                }
            }
            positions.push(PlanPosition {
                instrument: open.exposure.instrument.clone(),
                contracts: open.exposure.contracts,
                basis: open.basis,
                unrealized_pnl: unrealized_pnl(size, value, open.basis)?,
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

    /// The fill of position `index` on one level. See the
    /// [search](self#search-within-a-level).
    fn close_on_level(
        &self,
        state: &State,
        index: usize,
        level: usize,
        offer: &LiquidityLevel,
    ) -> Result<(State, ForcedReduction, AccountSummary)> {
        let exposure = &state.open(index)?.exposure;
        let size = exposure.contracts.abs();
        let grid = ContractGrid::new(self.policy.contract_step, offer.contracts.min(size))?;
        let at = |k: GridIndex| -> Result<(State, ForcedReduction, AccountSummary)> {
            let n = grid.contracts(k)?;
            let (next, reduction) = self.reduce(state, index, level, offer.price, n)?;
            let summary = self.summary(&next)?;
            Ok((next, reduction, summary))
        };
        let last = grid.last();
        if self.policy.target == CloseoutTarget::Flat {
            return at(last);
        }
        let first = grid.next(GridIndex::ZERO).unwrap_or(last);
        let continuous = self.continuous(state, index, offer.price)?;
        // J contains every count that meets the target, J' only counts that
        // do; without a certificate (an overflow of the wide integers), J
        // is the whole level.
        let (possible, certain) = match continuous.certify(&grid, first, last, self.scale) {
            Some((possible, certain)) => (possible.span(), certain.span()),
            None => (Some((first, last)), None),
        };
        let Some((start, end)) = possible else {
            // No count on the level meets the target.
            return Self::whole_level(&grid, size, last, &at);
        };
        // Only counts whose remaining position a Decimal can hold can be
        // booked; the walk jumps to them arithmetically.
        let limit = || MarginError::CloseoutSearchLimit {
            instrument: exposure.instrument.clone(),
            level,
            limit: self.limit,
        };
        let holding = |from: GridIndex, to: GridIndex| grid.first_holding(size, (from, to));
        let certified = |k: GridIndex| certain.is_some_and(|(low, high)| low <= k && k <= high);
        let mut k = start;
        while let Some(next) = holding(k, end)? {
            k = next;
            let left = self.budget.get();
            if left == 0 {
                // Every count of J' that can be booked meets the target.
                let Some((low, high)) = certain else {
                    return Err(limit());
                };
                let Some(count) = holding(k.max(low), high)? else {
                    return Err(limit());
                };
                // Minimal when no count that can be booked lies between
                // the last one evaluated and this one.
                if holding(k, count)? != Some(count) {
                    self.minimal.set(false);
                }
                return self.certified_count(at(count)?);
            }
            self.budget.set(left.saturating_sub(1));
            let attempt = at(k)?;
            if certified(k) {
                return self.certified_count(attempt);
            }
            if self.met(&attempt.2) {
                return Ok(attempt);
            }
            match grid.next(k) {
                Some(next) if next <= end => k = next,
                _ => break,
            }
        }
        // No count of J that can be booked meets the target.
        Self::whole_level(&grid, size, last, &at)
    }

    /// A count certified to meet the target, checked.
    fn certified_count(
        &self,
        attempt: (State, ForcedReduction, AccountSummary),
    ) -> Result<(State, ForcedReduction, AccountSummary)> {
        if self.met(&attempt.2) {
            Ok(attempt)
        } else {
            // The margin tiers disagree with the rule's requirement.
            Err(MarginError::arithmetic("certified closeout count"))
        }
    }

    /// The whole level, when no smaller count meets the target.
    fn whole_level(
        grid: &ContractGrid,
        size: Decimal,
        last: GridIndex,
        at: &dyn Fn(GridIndex) -> Result<(State, ForcedReduction, AccountSummary)>,
    ) -> Result<(State, ForcedReduction, AccountSummary)> {
        if grid.first_holding(size, (last, last))? != Some(last) {
            return Err(MarginError::invalid_input(
                "liquidity",
                "closing the whole level leaves a position no Decimal can hold",
            ));
        }
        at(last)
    }

    /// The unrounded cash and excess of the account after closing part of
    /// position `index` at `price`, the other positions unchanged.
    fn continuous(&self, state: &State, index: usize, price: Decimal) -> Result<Continuous> {
        let open = state.open(index)?;
        let requirement = self.requirement(state)?;
        let mut others = Vec::new();
        for (position, line) in state.positions.iter().enumerate() {
            if position == index || line.exposure.contracts.is_zero() {
                continue;
            }
            let value = market_value(
                line.exposure.contracts,
                line.exposure.size,
                line.exposure.mark,
                self.scale,
            )?;
            others.push(unrealized_pnl(line.exposure.size, value, line.basis)?);
        }
        for line in &requirement.positions {
            if line.instrument == open.exposure.instrument {
                continue;
            }
            others.push(neg(match self.policy.target {
                CloseoutTarget::Initial => line.initial,
                CloseoutTarget::Maintenance | CloseoutTarget::Flat => line.maintenance,
            }));
        }
        let tiers = self
            .model
            .margin_tiers(&open.exposure)
            .iter()
            .map(|tier| {
                (
                    tier.floor,
                    match self.policy.target {
                        CloseoutTarget::Initial => tier.initial_rate,
                        CloseoutTarget::Maintenance | CloseoutTarget::Flat => tier.maintenance_rate,
                    },
                )
            })
            .collect();
        Ok(Continuous {
            size: open.exposure.size,
            sign: if open.exposure.contracts > Decimal::ZERO {
                Decimal::ONE
            } else {
                Decimal::NEGATIVE_ONE
            },
            held: open.exposure.contracts.abs(),
            basis: open.basis,
            mark: open.exposure.mark,
            price,
            fee_rate: self.policy.fee_rate,
            cash: state.cash,
            others,
            tiers,
        })
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
        let scale = self.scale;
        let open = state.open(index)?;
        let contract = open.exposure.size;
        let held = open.exposure.contracts;
        let held_abs = held.abs();
        // rhe(p * n * M), or rhe(n * CV / p) in the base.
        let notional = market_value(contracts, contract, price, scale)?;
        let (side, fill_cash, contracts_after) = if held > Decimal::ZERO {
            (Action::Sell, notional, sub(held, contracts, "Qc - n")?)
        } else {
            (Action::Buy, neg(notional), add(held, contracts, "Qc + n")?)
        };
        let released_basis = if contracts == held_abs {
            open.basis
        } else {
            mul_div_round(
                &[open.basis, contracts],
                &[held_abs],
                scale,
                Rounding::HalfEven,
                "basis * n / |Qc|",
            )?
        };
        // Linear: fill cash - released; inverse: released - fill value.
        let realized_pnl = unrealized_pnl(contract, fill_cash, released_basis)?;
        let fee = match contract {
            ContractSize::Multiplier(m) => mul_div_round(
                &[self.policy.fee_rate, price, contracts, m],
                &[],
                scale,
                Rounding::TowardZero,
                "fee",
            )?,
            ContractSize::ContractValue(cv) => mul_div_round(
                &[self.policy.fee_rate, contracts, cv],
                &[price],
                scale,
                Rounding::TowardZero,
                "fee",
            )?,
        };
        let mut next = state.clone();
        next.cash = sum(
            &[state.cash, realized_pnl, neg(fee)],
            "cash + realized - fee",
        )?;
        let updated = next.open_mut(index)?;
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
        for position in &account.view.positions {
            check_size(position.product, position.size)?;
        }
        check_scale(account.cash, self.scale, "cash")?;
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
            let Some(contracts) = open.get(instrument) else {
                return Err(MarginError::invalid_input(
                    "basis",
                    "given for an instrument without an open position",
                ));
            };
            // A long is bought at positive prices, a short sold at them; a
            // basis rounded to zero is possible.
            if !basis.is_zero() && basis.is_sign_negative() != contracts.is_sign_negative() {
                return Err(MarginError::invalid_input(
                    "basis",
                    "must be zero or have the sign of the position",
                ));
            }
            check_scale(*basis, self.scale, "basis")?;
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
                // Every count the level can take must be exact at the step's
                // scale; the largest cap a level can have is the position.
                ContractGrid::new(policy.contract_step, level.contracts.min(contracts.abs()))?;
            }
            // Best level first: highest bid to sell a long, lowest ask to
            // buy back a short.
            let ordered = levels.windows(2).all(|w| match w {
                [first, second] if *contracts > Decimal::ZERO => first.price >= second.price,
                [first, second] => first.price <= second.price,
                _ => true,
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

/// Plans the closeout of `account`. See the [module docs](self).
///
/// `liquidity` gives, per instrument, the levels available on the closing
/// side, best first; an open position without an entry has no liquidity.
/// Entries for instruments without an open position are ignored.
///
/// # Errors
///
/// - [`MarginError::InvalidInput`] for an invalid policy (fee rate outside
///   `[0, 1)`, non positive step, or a step whose multiples are not exact
///   below a level's size: field `contract_step`), a basis missing for an
///   open position,
///   given for a closed one, or of the opposite sign to the position,
///   repeated resting orders, or liquidity levels that are not positive or
///   not ordered from the best price.
/// - [`MarginError::PrecisionExceeded`] if cash or a basis has more digits
///   than the rule's [`cash_scale`](PiecewiseLinearMargin::cash_scale).
/// - Every error of the model's
///   [`requirement`](crate::initial_margin::InitialMarginModel::requirement)
///   on the view.
/// - [`MarginError::Arithmetic`] if an amount cannot be represented.
pub fn plan_liquidation<M: PiecewiseLinearMargin>(
    model: &M,
    account: &LiquidationAccount,
    liquidity: &BTreeMap<InstrumentId, Vec<LiquidityLevel>>,
    policy: &LiquidationPolicy,
) -> Result<LiquidationPlan> {
    plan_with_budget(model, account, liquidity, policy, SEARCH_BUDGET)
}

/// [`plan_liquidation`] with an explicit search budget.
#[expect(
    clippy::too_many_lines,
    reason = "the staged closeout reads as one sequence: validate, cancel, reduce level by level, report the residual"
)]
fn plan_with_budget<M: PiecewiseLinearMargin>(
    model: &M,
    account: &LiquidationAccount,
    liquidity: &BTreeMap<InstrumentId, Vec<LiquidityLevel>>,
    policy: &LiquidationPolicy,
    budget: u32,
) -> Result<LiquidationPlan> {
    let planner = Planner {
        model,
        policy,
        account,
        scale: model.cash_scale(),
        budget: Cell::new(budget),
        limit: budget,
        minimal: Cell::new(true),
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
            debt: neg(after.cash).max(Decimal::ZERO),
            after,
            unfilled,
            outcome,
            deficit: deficit.max(Decimal::ZERO),
            minimal: planner.minimal.get(),
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
        let levels = liquidity.get(instrument).map_or(&[][..], Vec::as_slice);
        for (level, available) in levels.iter().enumerate() {
            if planner.met(&current) {
                break 'instruments;
            }
            let size = state.open(index)?.exposure.contracts.abs();
            if size.is_zero() {
                break;
            }
            let (next, reduction, summary) =
                planner.close_on_level(&state, index, level, available)?;
            state = next;
            reductions.push(reduction);
            current = summary;
        }
        if planner.met(&current) {
            break;
        }
        let left = state.open(index)?.exposure.contracts.abs();
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

/// The work budget of [`plan_liquidation`]: the exact evaluations of
/// candidate counts it may make in total, over every level and position of
/// one call (see the [search](self#search-within-a-level)).
///
/// One exact evaluation books a candidate count and values the account:
/// about 11 microseconds in an optimized build with values near
/// `Decimal::MAX`, so the whole budget costs a few tens of milliseconds.
/// With real cash quanta a level needs a handful of evaluations; the
/// budget only binds when the quantum is coarse against what one step
/// changes.
pub const SEARCH_BUDGET: u32 = 4096;

/// A grid interval `[start, end]`, both ends included.
type Span = (GridIndex, GridIndex);

/// The counts of a level that satisfy a condition: none, or an interval.
#[derive(Debug, Clone, Copy)]
enum Counts {
    Empty,
    Range(Span),
}

impl Counts {
    fn span(self) -> Option<Span> {
        match self {
            Self::Empty => None,
            Self::Range(span) => Some(span),
        }
    }
}

/// The unrounded cash and excess (collateral minus the target margin) of
/// the account after closing `n` contracts of one position at one price,
/// the other positions unchanged. With the cash quantum `q`, the booked
/// figures differ from these by less than `2 q` (cash: the fill value and
/// the released basis round half-even, the fee truncates) and `3 q`
/// (excess: the fill value, the fee, the remaining position's value and
/// its margin, rounded up; the released basis cancels). The other
/// positions enter with their booked figures.
///
/// Each figure is evaluated exactly, multiplied by a positive constant
/// denominator `D`: `|Q|` (linear cash), 1 (linear excess), `|Q| * p`
/// (inverse cash) and `p * mark` (inverse excess).
struct Continuous {
    size: ContractSize,
    /// +1 to reduce a long, -1 to reduce a short.
    sign: Decimal,
    held: Decimal,
    basis: Decimal,
    mark: Decimal,
    price: Decimal,
    fee_rate: Decimal,
    cash: Decimal,
    /// The other positions: their unrealized PnL and minus their margins.
    others: Vec<Decimal>,
    /// The position's tiers: floor and the target's rate.
    tiers: Vec<(Decimal, Decimal)>,
}

impl Continuous {
    /// `(cash(n) - threshold) * D`, as product terms, for a count `n`.
    fn cash_terms(&self, count: Decimal, threshold: Decimal) -> Vec<Vec<Decimal>> {
        let (held, basis, price, fee, sign) =
            (self.held, self.basis, self.price, self.fee_rate, self.sign);
        match self.size {
            // cash + s n p M - f n p M - n B / |Q|, times |Q| (s the side sign,
            // n the count, p the price, f the fee rate).
            ContractSize::Multiplier(multiplier) => vec![
                vec![self.cash, held],
                vec![neg(threshold), held],
                vec![sign, count, held, price, multiplier],
                vec![neg(fee), count, held, price, multiplier],
                vec![neg(count), basis],
            ],
            // cash + n B / |Q| - s n CV / p - f n CV / p, times |Q| p.
            ContractSize::ContractValue(cv) => vec![
                vec![self.cash, held, price],
                vec![neg(threshold), held, price],
                vec![count, basis, price],
                vec![neg(sign), count, cv, held],
                vec![neg(fee), count, cv, held],
            ],
        }
    }

    /// `(excess(n) - threshold) * D`, as product terms; `None` on an
    /// overflow of the wide integers. The contracts left, `|Q| - n`, are
    /// never formed: they need not be representable.
    fn excess_terms(&self, count: Decimal, threshold: Decimal) -> Option<Vec<Vec<Decimal>>> {
        let (held, basis, price, fee, sign, mark) = (
            self.held,
            self.basis,
            self.price,
            self.fee_rate,
            self.sign,
            self.mark,
        );
        // The constant factor D, and the remaining notional per contract as
        // num / den: N = (|Q| - n) num / den.
        let (scale_by, unit, unit_den): (Vec<Decimal>, Vec<Decimal>, Vec<Decimal>) = match self.size
        {
            ContractSize::Multiplier(multiplier) => (vec![], vec![multiplier, mark], vec![]),
            ContractSize::ContractValue(cv) => (vec![price, mark], vec![cv], vec![mark]),
        };
        let times = |head: &[Decimal]| -> Vec<Decimal> {
            let mut term = head.to_vec();
            term.extend_from_slice(&scale_by);
            term
        };
        let with = |head: &[Decimal], tail: &[Decimal]| -> Vec<Decimal> {
            let mut term = head.to_vec();
            term.extend_from_slice(tail);
            term
        };
        let mut terms = vec![times(&[self.cash]), times(&[neg(threshold)])];
        for other in &self.others {
            terms.push(times(&[*other]));
        }
        match self.size {
            // cash + s n p M - f n p M + s (|Q| - n) M mark - B.
            ContractSize::Multiplier(multiplier) => {
                terms.push(vec![neg(basis)]);
                terms.push(vec![sign, count, price, multiplier]);
                terms.push(vec![neg(fee), count, price, multiplier]);
                terms.push(vec![sign, held, multiplier, mark]);
                terms.push(vec![neg(sign), count, multiplier, mark]);
            }
            // (cash + B - s n CV / p - f n CV / p - s (|Q| - n) CV / mark) times
            // p mark.
            ContractSize::ContractValue(cv) => {
                terms.push(times(&[basis]));
                terms.push(vec![neg(sign), count, cv, mark]);
                terms.push(vec![neg(fee), count, cv, mark]);
                terms.push(vec![neg(sign), held, cv, price]);
                terms.push(vec![sign, count, cv, price]);
            }
        }
        // Minus the margin: full tiers below the top one, then the top
        // slice N - floor_k. A tier is reached when N > floor, compared
        // exactly: |Q| num - n num - floor den > 0.
        let mut reached = 0_usize;
        for (floor, _) in &self.tiers {
            let crossing = [
                with(&[held], &unit),
                with(&[neg(count)], &unit),
                with(&[neg(*floor)], &unit_den),
            ];
            let crossing: Vec<&[Decimal]> = crossing.iter().map(Vec::as_slice).collect();
            let (above, _) = sum_products_exact(&crossing)?;
            if above.negative || above.magnitude.is_zero() {
                break;
            }
            reached = reached.saturating_add(1);
        }
        // rate N D = rate (|Q| - n) num (D / den); D / den is p for an inverse
        // contract and 1 for a linear one.
        let per_contract = match self.size {
            ContractSize::Multiplier(_) => unit.clone(),
            ContractSize::ContractValue(_) => with(&unit, &[price]),
        };
        for (i, (floor, rate)) in self.tiers.iter().enumerate().take(reached) {
            terms.push(times(&[*rate, *floor]));
            if i.saturating_add(1) < reached {
                let (next_floor, _) = self.tiers.get(i.saturating_add(1))?;
                terms.push(times(&[neg(*rate), *next_floor]));
            } else {
                terms.push(with(&[neg(*rate), held], &per_contract));
                terms.push(with(&[*rate, count], &per_contract));
            }
        }
        Some(terms)
    }

    /// True when `cash(n) >= threshold`; `None` on an overflow.
    fn cash_at_least(&self, n: Decimal, threshold: Decimal) -> Option<bool> {
        let terms = self.cash_terms(n, threshold);
        let terms: Vec<&[Decimal]> = terms.iter().map(Vec::as_slice).collect();
        let (total, _) = sum_products_exact(&terms)?;
        Some(!total.negative || total.magnitude.is_zero())
    }

    /// `(excess(n) - threshold) * D` exactly, with its scale.
    fn excess_value(&self, n: Decimal, threshold: Decimal) -> Option<(Wide, u32)> {
        let terms = self.excess_terms(n, threshold)?;
        let terms: Vec<&[Decimal]> = terms.iter().map(Vec::as_slice).collect();
        sum_products_exact(&terms)
    }

    fn excess_at_least(&self, n: Decimal, threshold: Decimal) -> Option<bool> {
        let (total, _) = self.excess_value(n, threshold)?;
        Some(!total.negative || total.magnitude.is_zero())
    }

    /// The counts of the level that may meet the target (J: cash >= -2q
    /// and excess >= -3q) and the counts certain to (J': cash >= 2q and
    /// excess >= 3q), each an interval of the grid or empty. `None` on an
    /// overflow of the wide integers.
    fn certify(
        &self,
        grid: &ContractGrid,
        first: GridIndex,
        last: GridIndex,
        scale: u32,
    ) -> Option<(Counts, Counts)> {
        let (two, three) = (Decimal::new(2, scale), Decimal::new(3, scale));
        let possible = self.span(grid, (first, last), neg(two), neg(three))?;
        let certain = self.span(grid, (first, last), two, three)?;
        Some((possible, certain))
    }

    /// The counts in `range` with `cash >= cash_threshold` and `excess >=
    /// excess_threshold`: an interval, since the cash is linear and the
    /// excess concave in the count.
    fn span(
        &self,
        grid: &ContractGrid,
        (first, last): Span,
        cash_threshold: Decimal,
        excess_threshold: Decimal,
    ) -> Option<Counts> {
        let n = |k: GridIndex| grid.contracts(k).ok();
        // Cash, linear: monotone in the count.
        let cash = |k: GridIndex| self.cash_at_least(n(k)?, cash_threshold);
        let cash_span = match (cash(first)?, cash(last)?) {
            (true, true) => Some((first, last)),
            (false, false) => None,
            (true, false) => Some((first, last_true(first, last, &cash)?)),
            (false, true) => Some((first_true(first, last, &cash)?, last)),
        };
        let Some(cash_span) = cash_span else {
            return Some(Counts::Empty);
        };
        // Excess, concave: its maximum on the grid is at an end or next to a
        // tier floor; the superlevel set is an interval around it.
        let mut candidates = vec![first, last];
        for (floor, _) in self.tiers.iter().skip(1) {
            let (terms, denominators) = match self.size {
                ContractSize::Multiplier(m) => (
                    vec![vec![self.held, m, self.mark], vec![neg(*floor)]],
                    vec![m, self.mark],
                ),
                ContractSize::ContractValue(cv) => (
                    vec![vec![self.held, cv], vec![neg(*floor), self.mark]],
                    vec![cv],
                ),
            };
            let terms: Vec<&[Decimal]> = terms.iter().map(Vec::as_slice).collect();
            let k = grid.floor_index_of(&terms, &denominators).ok()?;
            for candidate in [Some(k), grid.next(k)].into_iter().flatten() {
                if candidate >= first && candidate <= last {
                    candidates.push(candidate);
                }
            }
        }
        let mut best = first;
        let mut best_value = self.excess_value(n(first)?, excess_threshold)?;
        for candidate in candidates {
            let value = self.excess_value(n(candidate)?, excess_threshold)?;
            if compare_exact(value, best_value)? == core::cmp::Ordering::Greater {
                best = candidate;
                best_value = value;
            }
        }
        if best_value.0.negative && !best_value.0.magnitude.is_zero() {
            return Some(Counts::Empty);
        }
        let excess = |k: GridIndex| self.excess_at_least(n(k)?, excess_threshold);
        let excess_start = if excess(first)? {
            first
        } else {
            first_true(first, best, &excess)?
        };
        let excess_end = if excess(last)? {
            last
        } else {
            last_true(best, last, &excess)?
        };
        let start = excess_start.max(cash_span.0);
        let end = excess_end.min(cash_span.1);
        Some(if start <= end {
            Counts::Range((start, end))
        } else {
            Counts::Empty
        })
    }
}

/// The first index in `[low, high]` where `holds` is true, given that it is
/// false at `low` and true at `high` and switches once.
fn first_true(
    mut low: GridIndex,
    mut high: GridIndex,
    holds: &dyn Fn(GridIndex) -> Option<bool>,
) -> Option<GridIndex> {
    while !ContractGrid::adjacent(low, high) {
        let middle = ContractGrid::midpoint(low, high);
        if holds(middle)? {
            high = middle;
        } else {
            low = middle;
        }
    }
    Some(high)
}

/// The last index in `[low, high]` where `holds` is true, given that it is
/// true at `low` and false at `high` and switches once.
fn last_true(
    mut low: GridIndex,
    mut high: GridIndex,
    holds: &dyn Fn(GridIndex) -> Option<bool>,
) -> Option<GridIndex> {
    while !ContractGrid::adjacent(low, high) {
        let middle = ContractGrid::midpoint(low, high);
        if holds(middle)? {
            low = middle;
        } else {
            high = middle;
        }
    }
    Some(low)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::initial_margin::ProductKind;
    use crate::rules::{MarginTier, TieredLinearIm};
    use rust_decimal_macros::dec;

    fn iid(id: &str) -> InstrumentId {
        InstrumentId::new(id).unwrap()
    }

    /// One tier, initial 10 %, cash in whole units (a coarse quantum).
    fn coarse_rule() -> TieredLinearIm {
        TieredLinearIm::new(
            RuleId::new("coarse", 1),
            Asset::new("USD").unwrap(),
            0,
            vec![MarginTier {
                floor: dec!(0),
                initial_rate: dec!(0.10),
                maintenance_rate: dec!(0.05),
            }],
        )
        .unwrap()
    }

    /// Long `contracts` (M = 1) with `basis`, marked at 0.97, and `cash`.
    fn account(contracts: Decimal, basis: Decimal, cash: Decimal) -> LiquidationAccount {
        LiquidationAccount {
            view: ExposureView {
                account: AccountId::new("acct").unwrap(),
                venue: VenueId::new("venue").unwrap(),
                positions: vec![ExposurePosition {
                    instrument: iid("A"),
                    product: ProductKind::LinearPerpetual,
                    settlement: Asset::new("USD").unwrap(),
                    contracts,
                    size: ContractSize::Multiplier(dec!(1)),
                    mark: dec!(0.97),
                }],
            },
            basis: BTreeMap::from([(iid("A"), basis)]),
            cash,
            resting_orders: vec![],
        }
    }

    fn bid(price: Decimal) -> BTreeMap<InstrumentId, Vec<LiquidityLevel>> {
        BTreeMap::from([(
            iid("A"),
            vec![LiquidityLevel {
                price,
                contracts: dec!(100),
            }],
        )])
    }

    fn initial() -> LiquidationPolicy {
        LiquidationPolicy {
            target: CloseoutTarget::Initial,
            fee_rate: dec!(0),
            contract_step: dec!(1),
        }
    }

    /// The unrounded cash and excess of `account` after closing `n` of
    /// its position at `price`, as signs against `2q` and `3q`: whether
    /// the count is certified to meet the target.
    fn certified(account: &LiquidationAccount, price: Decimal, n: Decimal) -> bool {
        let model = coarse_rule();
        let policy = initial();
        let planner = Planner {
            model: &model,
            policy: &policy,
            account,
            scale: 0,
            budget: Cell::new(0),
            limit: 0,
            minimal: Cell::new(true),
        };
        let state = State {
            cash: account.cash,
            positions: vec![Open {
                exposure: account.view.positions[0].clone(),
                basis: account.basis[&iid("A")],
            }],
        };
        let continuous = planner.continuous(&state, 0, price).unwrap();
        continuous.cash_at_least(n, dec!(2)).unwrap()
            && continuous.excess_at_least(n, dec!(3)).unwrap()
    }

    /// The same 40 long with a step of 0.002215: the smallest count is
    /// found with exactly 4096 evaluations, the whole budget; with 4095 the
    /// plan falls back. With a step of 0.0022146 it needs 4097, so
    /// `plan_liquidation` falls back to the first certified count, which
    /// meets the target; the count one step before it is not certified:
    /// unrounded, its cash is below 2q or its excess below 3q (a bound in
    /// money, not in contracts).
    #[test]
    fn the_budget_boundary_is_exact() {
        let account = account(dec!(40), dec!(45), dec!(9));
        let with = |step| LiquidationPolicy {
            contract_step: step,
            ..initial()
        };
        let model = coarse_rule();
        let exact =
            plan_liquidation(&model, &account, &bid(dec!(0.99)), &with(dec!(0.002215))).unwrap();
        assert_eq!(SEARCH_BUDGET, 4096);
        assert!(exact.minimal);
        let short = plan_with_budget(
            &model,
            &account,
            &bid(dec!(0.99)),
            &with(dec!(0.002215)),
            4095,
        )
        .unwrap();
        assert!(!short.minimal);

        let step = dec!(0.0022146);
        let fallback = plan_liquidation(&model, &account, &bid(dec!(0.99)), &with(step)).unwrap();
        assert!(!fallback.minimal);
        assert_eq!(fallback.outcome, CloseoutOutcome::TargetMet);
        assert!(fallback.after.cash >= dec!(0));
        assert!(fallback.after.collateral >= fallback.after.initial);
        let taken = fallback.reductions[0].contracts;
        assert!(certified(&account, dec!(0.99), taken));
        assert!(!certified(&account, dec!(0.99), taken - step));
        let enough =
            plan_with_budget(&model, &account, &bid(dec!(0.99)), &with(step), 4097).unwrap();
        assert!(enough.minimal);
        assert!(enough.reductions[0].contracts < taken);
    }

    /// 1e28 long marked at 0.001 (basis 1e25, upnl 0), cash 5e23 - 5e11 in
    /// whole units, maintenance 5 %, sold at 0.0010007. Each count changes
    /// almost nothing against the quantum of 1, so a small budget runs out
    /// and the plan falls back to J'. With a step of 0.5 the first count of
    /// J' (9861932938915187.5) leaves 1e28 - n.5 contracts, which no
    /// `Decimal` holds: it cannot be booked, and the plan takes the next
    /// count. At a step of 1e-11 the jump finds the same count; when no
    /// count of J' can be booked, the plan is refused.
    #[test]
    fn the_fallback_skips_counts_it_cannot_book() {
        let held = dec!(10000000000000000000000000000);
        let account = LiquidationAccount {
            cash: dec!(499999999999500000000000),
            basis: BTreeMap::from([(iid("A"), dec!(10000000000000000000000000))]),
            ..account(held, dec!(0), dec!(0))
        };
        let mut account = account;
        account.view.positions[0].mark = dec!(0.001);
        let price = dec!(0.0010007);
        let model = TieredLinearIm::new(
            RuleId::new("coarse", 1),
            Asset::new("USD").unwrap(),
            0,
            vec![MarginTier {
                floor: dec!(0),
                initial_rate: dec!(0.10),
                maintenance_rate: dec!(0.05),
            }],
        )
        .unwrap();
        // A level of 1e17 contracts: every count it allows is exact at the
        // steps used here, but not the contracts left from 1e28.
        let level_size = dec!(100000000000000000);
        let liquidity = BTreeMap::from([(
            iid("A"),
            vec![LiquidityLevel {
                price,
                contracts: level_size,
            }],
        )]);
        let policy = |step| LiquidationPolicy {
            target: CloseoutTarget::Maintenance,
            fee_rate: dec!(0),
            contract_step: step,
        };

        // The first count of J' at a step of 0.5.
        let half = policy(dec!(0.5));
        let planner = Planner {
            model: &model,
            policy: &half,
            account: &account,
            scale: 0,
            budget: Cell::new(0),
            limit: 0,
            minimal: Cell::new(true),
        };
        let state = State {
            cash: account.cash,
            positions: vec![Open {
                exposure: account.view.positions[0].clone(),
                basis: account.basis[&iid("A")],
            }],
        };
        let grid = ContractGrid::new(dec!(0.5), level_size).unwrap();
        let first = grid.next(GridIndex::ZERO).unwrap();
        let (_, certain) = planner
            .continuous(&state, 0, price)
            .unwrap()
            .certify(&grid, first, grid.last(), 0)
            .unwrap();
        let first_certified = certain.span().unwrap().0;
        let start = grid.contracts(first_certified).unwrap();
        assert_eq!(start.fract(), dec!(0.5));
        assert_eq!(
            grid.first_holding(held, (first_certified, first_certified))
                .unwrap(),
            None
        );

        let plan = plan_with_budget(&model, &account, &liquidity, &half, 16).unwrap();
        assert!(!plan.minimal);
        assert_eq!(plan.reductions[0].contracts, start + dec!(0.5));
        assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);

        // At a step of 1e-11 the counts that can be booked are 1e11 steps
        // apart: the walk jumps to them arithmetically and finds the same
        // count, with no loop over the others.
        let fine = plan_with_budget(
            &model,
            &account,
            &liquidity,
            &policy(dec!(0.00000000001)),
            16,
        )
        .unwrap();
        assert!(!fine.minimal);
        assert_eq!(fine.reductions[0].contracts, start + dec!(0.5));

        // A level of ...187.75 at a step of 0.25: every count of J' (and the
        // whole level) leaves a fraction of 1e28 contracts that no Decimal
        // holds. Nothing certified can be booked: the plan is refused.
        let short_level = BTreeMap::from([(
            iid("A"),
            vec![LiquidityLevel {
                price,
                contracts: start + dec!(0.25),
            }],
        )]);
        assert_eq!(
            plan_with_budget(&model, &account, &short_level, &policy(dec!(0.25)), 16),
            Err(MarginError::CloseoutSearchLimit {
                instrument: iid("A"),
                level: 0,
                limit: 16,
            })
        );
    }

    /// 40 long, basis 45, cash 9, sold at 0.99, whole units. The counts
    /// that may meet the target start at 1 and the first certified one is
    /// 35; the smallest that meets it is 10, found with 10 evaluations.
    /// With 9 the budget runs out and the plan takes 35, which meets the
    /// target, with `minimal == false`.
    #[test]
    fn the_budget_falls_back_to_the_first_certified_count() {
        let account = account(dec!(40), dec!(45), dec!(9));
        let full =
            plan_with_budget(&coarse_rule(), &account, &bid(dec!(0.99)), &initial(), 10).unwrap();
        assert!(full.minimal);
        assert_eq!(full.reductions[0].contracts, dec!(10));
        let short =
            plan_with_budget(&coarse_rule(), &account, &bid(dec!(0.99)), &initial(), 9).unwrap();
        assert!(!short.minimal);
        assert_eq!(short.reductions[0].contracts, dec!(35));
        assert_eq!(short.outcome, CloseoutOutcome::TargetMet);
        assert!(short.after.cash >= dec!(0));
        assert!(short.after.collateral >= short.after.initial);
    }

    /// 13 long, basis 15, cash 3, sold at 0.91, whole units: only rounding
    /// lets 3, 4 and 5 contracts meet the target (no count is certified).
    /// Three evaluations find 3; with two the budget runs out and, with no
    /// certified count, the plan is refused.
    #[test]
    fn without_a_certified_count_the_budget_is_an_error() {
        let account = account(dec!(13), dec!(15), dec!(3));
        let found =
            plan_with_budget(&coarse_rule(), &account, &bid(dec!(0.91)), &initial(), 3).unwrap();
        assert!(found.minimal);
        assert_eq!(found.reductions[0].contracts, dec!(3));
        assert_eq!(
            plan_with_budget(&coarse_rule(), &account, &bid(dec!(0.91)), &initial(), 2),
            Err(MarginError::CloseoutSearchLimit {
                instrument: iid("A"),
                level: 0,
                limit: 2,
            })
        );
    }
}
