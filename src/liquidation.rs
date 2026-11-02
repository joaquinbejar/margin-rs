//! Staged closeout plan for an account below its margin threshold.
//!
//! [`plan_liquidation`] simulates one closeout of an account's linear
//! futures and perpetuals against supplied scenario liquidity, under one
//! [`PiecewiseLinearMargin`] rule (any
//! [`TieredLinearIm`](crate::rules::TieredLinearIm)), and returns a
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
//! linear futures and perpetuals, [`LiquidationAccount::from_ledger`] reads
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
//! So `g(n) = min(cash(n), excess(n))` is concave and the counts that meet
//! the target (`g(n) >= 0`) form an interval `[a, b]`, which need not
//! reach the end of the level: closing more can push cash below zero.
//!
//! The plan finds it exactly, up to the rounding of single cash quanta:
//!
//! 1. it evaluates `g` exactly at the candidates where a concave piecewise
//!    linear function can peak: the ends of the level, the grid points
//!    around each margin breakpoint, and the grid points where
//!    `excess - cash` changes sign inside each linear segment (found by
//!    bisection on its sign);
//! 2. if no candidate meets the target, no count on the level does, and
//!    the plan takes the whole level;
//! 3. otherwise it bisects between 0 (which does not meet the target) and
//!    the smallest candidate that does, and takes `a`: a count that meets
//!    the target and whose predecessor on the grid does not.
//!
//! Counts live on the grid of [`LiquidationPolicy::contract_step`],
//! indexed in wide integers, so no product of a step count or bisection
//! midpoint can overflow.

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;

use crate::decimal::grid::{ContractGrid, GridIndex};
use crate::decimal::{
    MAX_SCALE, Rounding, add, check_scale, mul_div_round, neg, sub, sum, sum_products,
};
use crate::direction::Dir;
use crate::error::{MarginError, Result};
use crate::ids::{AccountId, Asset, ExecutionId, InstrumentId, OrderId, VenueId};
use crate::initial_margin::{
    ExposurePosition, ExposureView, MarginRequirement, PiecewiseLinearMargin, ProductKind, RuleId,
    check_size,
};
use crate::instrument::{ContractSize, Instrument};
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
    /// - Positions: every open linear future and perpetual, with its
    ///   contracts, the multiplier of the ledger's instrument and its mark;
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
            let (product, terms) = match snapshot.instruments.get(id) {
                Some(Instrument::Spot(_)) => continue,
                Some(Instrument::LinearFuture(future)) => {
                    if future.expiry_ms() <= as_of_ms {
                        return Err(MarginError::ExpiredUnsettled {
                            instrument: id.clone(),
                            expiry_ms: future.expiry_ms(),
                            as_of_ms,
                        });
                    }
                    (ProductKind::LinearFuture, future.terms())
                }
                Some(Instrument::LinearPerpetual(perpetual)) => {
                    (ProductKind::LinearPerpetual, perpetual.terms())
                }
                Some(Instrument::InverseFuture(_) | Instrument::InversePerpetual(_)) => {
                    return Err(MarginError::UnsupportedInstrument {
                        rule: model.rule().to_string(),
                        instrument: id.clone(),
                        reason: "inverse futures and perpetuals are not planned yet",
                    });
                }
                None => {
                    return Err(MarginError::UnknownInstrument {
                        instrument: id.clone(),
                    });
                }
            };
            if terms.settlement() != currency {
                return Err(MarginError::CurrencyMismatch {
                    field: "settlement",
                    expected: currency.clone(),
                    found: terms.settlement().clone(),
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
                size: ContractSize::Multiplier(terms.multiplier()),
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
    /// This fill as a ledger execution of a linear future or perpetual,
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
            let value = mul_div_round(
                &[
                    open.exposure.contracts,
                    multiplier(&open.exposure)?,
                    open.exposure.mark,
                ],
                &[],
                self.scale,
                Rounding::HalfEven,
                "Qc * M * mark",
            )?;
            terms.push(value);
            terms.push(neg(open.basis));
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

    fn target_margin(&self, summary: &AccountSummary) -> Decimal {
        match self.policy.target {
            CloseoutTarget::Maintenance => summary.maintenance,
            CloseoutTarget::Initial => summary.initial,
            CloseoutTarget::Flat => Decimal::ZERO,
        }
    }

    /// `excess - cash = collateral - margin - cash`, exact.
    fn gap(&self, summary: &AccountSummary) -> Result<Decimal> {
        sum(
            &[
                summary.collateral,
                neg(self.target_margin(summary)),
                neg(summary.cash),
            ],
            "excess - cash",
        )
    }

    /// The fill of position `index` on one level. See the
    /// [search](self#search-within-a-level).
    fn close_on_level(
        &self,
        state: &State,
        current: &AccountSummary,
        index: usize,
        level: usize,
        offer: &LiquidityLevel,
    ) -> Result<(State, ForcedReduction, AccountSummary)> {
        let exposure = &state.open(index)?.exposure;
        let size = exposure.contracts.abs();
        let m = multiplier(exposure)?;
        let grid = ContractGrid::new(self.policy.contract_step, offer.contracts.min(size))?;
        let at = |k: GridIndex| -> Result<(State, ForcedReduction, AccountSummary)> {
            let n = grid.contracts(k)?;
            let (next, reduction) = self.reduce(state, index, level, offer.price, n)?;
            let summary = self.summary(&next)?;
            Ok((next, reduction, summary))
        };
        let summary_at = |k: GridIndex| -> Result<AccountSummary> {
            if k == GridIndex::ZERO {
                Ok(current.clone())
            } else {
                Ok(at(k)?.2)
            }
        };
        let last = grid.last();
        if self.policy.target == CloseoutTarget::Flat {
            return at(last);
        }

        // Segment ends: no fill, the grid points around each breakpoint
        // (where the notional crosses a floor: n = |Qc| - floor / (M * mark)),
        // and the whole level.
        let mut ends = vec![GridIndex::ZERO, last];
        for floor in self.model.notional_breakpoints(exposure) {
            let numerator = sum_products(
                &[&[size, m, exposure.mark], &[neg(floor)]],
                "|Qc| * M * mark - floor",
            )?;
            let k = grid.floor_index_of(numerator, &[m, exposure.mark])?;
            ends.push(k);
            if let Some(next) = grid.next(k) {
                ends.push(next);
            }
        }
        ends.sort();
        ends.dedup();

        // Where excess - cash changes sign inside a segment, min(cash,
        // excess) can peak: bisect to the two grid points around it.
        let mut candidates = ends.clone();
        for pair in ends.windows(2) {
            let &[mut low, mut high] = pair else {
                continue;
            };
            let low_sign = self.gap(&summary_at(low)?)? >= Decimal::ZERO;
            let high_sign = self.gap(&summary_at(high)?)? >= Decimal::ZERO;
            if low_sign == high_sign {
                continue;
            }
            while !ContractGrid::adjacent(low, high) {
                let middle = ContractGrid::midpoint(low, high);
                if (self.gap(&summary_at(middle)?)? >= Decimal::ZERO) == low_sign {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            candidates.push(low);
            candidates.push(high);
        }
        candidates.retain(|k| *k != GridIndex::ZERO);
        candidates.sort();
        candidates.dedup();

        let mut found = None;
        for k in candidates {
            let attempt = at(k)?;
            if self.met(&attempt.2) {
                found = Some((k, attempt));
                break;
            }
        }
        let Some((mut high, mut chosen)) = found else {
            return at(last);
        };
        // 0 does not meet the target (the caller checked), `high` does.
        let mut low = GridIndex::ZERO;
        while !ContractGrid::adjacent(low, high) {
            let middle = ContractGrid::midpoint(low, high);
            let attempt = at(middle)?;
            if self.met(&attempt.2) {
                high = middle;
                chosen = attempt;
            } else {
                low = middle;
            }
        }
        Ok(chosen)
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
        let m = multiplier(&open.exposure)?;
        let held = open.exposure.contracts;
        let held_abs = held.abs();
        let notional = mul_div_round(
            &[price, contracts, m],
            &[],
            scale,
            Rounding::HalfEven,
            "p * n * M",
        )?;
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
        let realized_pnl = sub(fill_cash, released_basis, "fill cash - released")?;
        let fee = mul_div_round(
            &[self.policy.fee_rate, price, contracts, m],
            &[],
            scale,
            Rounding::TowardZero,
            "fee",
        )?;
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
            if position.product.is_inverse() {
                return Err(MarginError::UnsupportedInstrument {
                    rule: self.model.rule().to_string(),
                    instrument: position.instrument.clone(),
                    reason: "inverse futures and perpetuals are not planned yet",
                });
            }
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
///   `[0, 1)`, non positive step), a basis missing for an open position,
///   given for a closed one, or of the opposite sign to the position,
///   repeated resting orders, or liquidity levels that are not positive or
///   not ordered from the best price.
/// - [`MarginError::PrecisionExceeded`] if cash or a basis has more digits
///   than the rule's [`cash_scale`](PiecewiseLinearMargin::cash_scale).
/// - Every error of the model's
///   [`requirement`](crate::initial_margin::InitialMarginModel::requirement)
///   on the view.
/// - [`MarginError::Arithmetic`] if an amount cannot be represented.
#[expect(
    clippy::too_many_lines,
    reason = "the staged closeout reads as one sequence: validate, cancel, reduce level by level, report the residual"
)]
pub fn plan_liquidation<M: PiecewiseLinearMargin>(
    model: &M,
    account: &LiquidationAccount,
    liquidity: &BTreeMap<InstrumentId, Vec<LiquidityLevel>>,
    policy: &LiquidationPolicy,
) -> Result<LiquidationPlan> {
    let planner = Planner {
        model,
        policy,
        account,
        scale: model.cash_scale(),
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
                planner.close_on_level(&state, &current, index, level, available)?;
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

/// The multiplier of a linear position; the plan covers linear positions
/// only ([`plan_liquidation`] refuses the others before planning).
fn multiplier(exposure: &ExposurePosition) -> Result<Decimal> {
    match exposure.size {
        ContractSize::Multiplier(m) => Ok(m),
        ContractSize::ContractValue(_) => Err(MarginError::invalid_input(
            "size",
            "the plan covers linear positions only",
        )),
    }
}
