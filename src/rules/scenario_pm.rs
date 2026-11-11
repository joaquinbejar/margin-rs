//! Scenario portfolio margin: the rule `scenario-pm/1`.
//!
//! # Formula
//!
//! The account's positions (legs) are revalued under a grid of scenarios
//! the caller supplies. Each leg has signed contracts `Q`, a multiplier
//! `M` and a reference value `X0`: the mark of a linear future or
//! perpetual, the premium mark of an option. A scenario `s` gives each leg
//! a value `X_s`: the shocked price of a linear leg, the repriced premium
//! of an option. The rule prices nothing: option values under a scenario
//! (moves of the underlying and of the volatility) come from the caller as
//! exact decimals.
//!
//! ```text
//! loss_i(s) = -Q_i * M_i * (X_s,i - X0_i)
//! R_s       = sum over legs of loss_i(s)
//! F         = f * sum over short options of |Q| * M * U
//! G         = g * sum over linear legs of |Q| * M * X0
//! figure    = round_up( max(0, max over s of R_s) + F + G )
//! ```
//!
//! `R_s` is the linear loss, plus the repricing loss of the short options,
//! minus the value change of the long options: one signed sum, so
//! offsetting legs net. `U` is the option's underlying price. The initial
//! and the maintenance figure each have their own scenario set and their
//! own floor rates `f` (short options) and `g` (linear legs)
//! ([`ScenarioPmRates`]). Every term is exact; the figure is rounded up
//! once per account to the rule's scale. On a tie the worst scenario is
//! the one with the lowest index.
//!
//! With one linear position, `g = 0` and a set whose extreme moves are
//! `X0 * (1 +- r)`, the figure is `round_up(r * |Q| * M * X0)`: the
//! single-tier `linear-im/1` figure at rate `r`.
//!
//! # Admission
//!
//! [`ScenarioPm::admission_requirement`] counts the account's resting
//! orders as filled at their limit price `L`, in the worst case over every
//! subset of them that fills. An order of signed contracts `q` (positive
//! to buy) adds `c_o(s) = -q * M * (X_s - L)` to scenario `s`; the worst
//! subset for `s` takes every order with `c_o(s) > 0`, so
//!
//! ```text
//! R'_s = R_s + sum over orders of max(0, c_o(s))
//! F'   = f * sum over options of max(0, -(Q + sum of its sell orders)) * M * U
//! G'   = g * sum over linear legs of (|Q| + sum of |q| of its orders) * M * X0
//! ```
//!
//! `F'` counts every option sell as filled and no buy; `G'` counts every
//! linear order's notional at the mark. Each is at least its figure after
//! any subset of fills, so the admission figure is never below the
//! requirement of any fill subset. Every order needs a leg for its
//! instrument (with zero contracts if the account holds none).
//!
//! # Breakdown
//!
//! [`ScenarioPmRequirement`] gives, for each figure, the worst scenario's
//! index, the exact per-leg (and per-order) terms in it and both floors.
//! In the [`MarginRequirement`], each figure is attributed whole to the leg
//! with the largest term in its worst scenario (the first in view order on
//! a tie), zero to the others, so lines add up to the totals.
//!
//! The formula and the rule id are this crate's own specification
//! (`docs/scenario-margin.md`), not a venue's rule.

use std::collections::BTreeMap;

use core::cmp::Ordering;

use rust_decimal::Decimal;

use crate::decimal::{
    Rounding, check_configured_scale, compare_exact, neg, sum, sum_products, sum_products_exact,
    sum_terms_round,
};
use crate::error::{MarginError, Result};
use crate::ids::{AccountId, Asset, InstrumentId, VenueId};
use crate::initial_margin::{
    AccountMargin, ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement,
    PositionRequirement, ProductKind, RuleId,
};
use crate::instrument::ContractSize;
use crate::rules::MarginTier;

/// Floor rates of one figure, each in `[0, 1]`. See the
/// [module docs](self#formula).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScenarioPmRates {
    /// `f`: charged on `|Q| * M * U` of every short option.
    pub short_option_floor_rate: Decimal,
    /// `g`: charged on `|Q| * M * X0` of every linear leg.
    pub linear_floor_rate: Decimal,
}

/// The scenario sets of the two figures, each non empty. A scenario maps
/// every leg's instrument to its value under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioSets {
    /// Scenarios of the initial figure.
    pub initial: Vec<BTreeMap<InstrumentId, Decimal>>,
    /// Scenarios of the maintenance figure.
    pub maintenance: Vec<BTreeMap<InstrumentId, Decimal>>,
}

/// What a leg is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PmLegKind {
    /// A linear future or perpetual: values are prices, positive.
    Linear,
    /// An option: values are premiums, at least zero.
    Option {
        /// Price of the underlying, positive, for the short option floor.
        underlying_price: Decimal,
    },
}

/// One position of the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmLeg {
    /// Instrument, unique within the account.
    pub instrument: InstrumentId,
    /// Linear or option.
    pub kind: PmLegKind,
    /// Signed contracts; zero for an instrument held only through orders.
    pub contracts: Decimal,
    /// Units of the underlying per contract, positive.
    pub multiplier: Decimal,
    /// Reference value `X0`: the mark price or premium.
    pub mark: Decimal,
}

/// A resting order, for [`ScenarioPm::admission_requirement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmOrder {
    /// Instrument; the account needs a leg for it.
    pub instrument: InstrumentId,
    /// Signed contracts left: positive to buy, negative to sell, not zero.
    pub contracts: Decimal,
    /// Limit price (premium for an option), at least zero; positive for a
    /// linear leg.
    pub limit_price: Decimal,
}

/// How one figure was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmBreakdown {
    /// Index of the worst scenario in its set (lowest on a tie).
    pub worst: usize,
    /// `loss_i` of every leg in the worst scenario, in view order.
    pub leg_terms: Vec<(InstrumentId, Decimal)>,
    /// `max(0, c_o)` of every order in the worst scenario, in order;
    /// empty outside admission.
    pub order_terms: Vec<(InstrumentId, Decimal)>,
    /// `max(0, R_s)` of the worst scenario, exact.
    pub scenario_loss: Decimal,
    /// `F`, exact.
    pub short_option_floor: Decimal,
    /// `G`, exact.
    pub linear_floor: Decimal,
    /// The figure, rounded up once.
    pub figure: Decimal,
}

/// A requirement with its breakdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioPmRequirement {
    /// The requirement, as every other rule reports it.
    pub requirement: MarginRequirement,
    /// The initial figure.
    pub initial: PmBreakdown,
    /// The maintenance figure.
    pub maintenance: PmBreakdown,
}

/// The rule `scenario-pm/1` over one grid of scenarios. See the
/// [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioPm {
    id: RuleId,
    currency: Asset,
    scale: u32,
    initial: ScenarioPmRates,
    maintenance: ScenarioPmRates,
    scenarios: ScenarioSets,
}

impl ScenarioPm {
    /// The rule `scenario-pm/1` in `currency` at `scale`, with the floor
    /// rates of each figure and the scenarios of the current evaluation.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if `scale` exceeds 18, a rate is
    /// outside `[0, 1]` or a scenario set is empty.
    pub fn scenario_pm_v1(
        currency: Asset,
        scale: u32,
        initial: ScenarioPmRates,
        maintenance: ScenarioPmRates,
        scenarios: ScenarioSets,
    ) -> Result<Self> {
        check_configured_scale(scale, "scale")?;
        for rates in [initial, maintenance] {
            for rate in [rates.short_option_floor_rate, rates.linear_floor_rate] {
                if rate < Decimal::ZERO || rate > Decimal::ONE {
                    return Err(MarginError::invalid_input("rates", "must be in [0, 1]"));
                }
            }
        }
        if scenarios.initial.is_empty() || scenarios.maintenance.is_empty() {
            return Err(MarginError::invalid_input(
                "scenarios",
                "each set needs at least one scenario",
            ));
        }
        Ok(Self {
            id: RuleId::new("scenario-pm", 1),
            currency,
            scale,
            initial,
            maintenance,
            scenarios,
        })
    }

    /// Name and version of the rule.
    #[must_use]
    pub fn rule_id(&self) -> &RuleId {
        &self.id
    }

    /// Currency of every amount.
    #[must_use]
    pub fn currency(&self) -> &Asset {
        &self.currency
    }

    /// Scale every figure is rounded up to.
    #[must_use]
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// The requirement of `legs`.
    ///
    /// # Errors
    ///
    /// - [`MarginError::InvalidInput`] for a repeated instrument, a
    ///   multiplier that is not positive, a linear mark or scenario price
    ///   that is not positive, a negative premium, or an underlying price
    ///   that is not positive.
    /// - [`MarginError::MissingMark`] for a leg without a value in a
    ///   scenario.
    /// - [`MarginError::Arithmetic`] if a figure or a term cannot be
    ///   represented.
    pub fn requirement_for(
        &self,
        account: AccountId,
        venue: VenueId,
        legs: &[PmLeg],
    ) -> Result<ScenarioPmRequirement> {
        self.evaluate(account, venue, legs, &[])
    }

    /// The admission requirement of `legs` with `orders` resting. See the
    /// [module docs](self#admission).
    ///
    /// # Errors
    ///
    /// As [`ScenarioPm::requirement_for`], and
    /// [`MarginError::InvalidInput`] for an order without a leg, with zero
    /// contracts or with an invalid limit price.
    pub fn admission_requirement(
        &self,
        account: AccountId,
        venue: VenueId,
        legs: &[PmLeg],
        orders: &[PmOrder],
    ) -> Result<ScenarioPmRequirement> {
        self.evaluate(account, venue, legs, orders)
    }

    fn evaluate(
        &self,
        account: AccountId,
        venue: VenueId,
        legs: &[PmLeg],
        orders: &[PmOrder],
    ) -> Result<ScenarioPmRequirement> {
        let by_id = validate(legs, orders)?;
        let initial = self.figure(legs, orders, &by_id, &self.scenarios.initial, self.initial)?;
        let maintenance = self.figure(
            legs,
            orders,
            &by_id,
            &self.scenarios.maintenance,
            self.maintenance,
        )?;
        let carrier = |breakdown: &PmBreakdown| {
            let mut best: Option<(usize, Decimal)> = None;
            for (index, (_, term)) in breakdown.leg_terms.iter().enumerate() {
                if best.is_none_or(|(_, value)| *term > value) {
                    best = Some((index, *term));
                }
            }
            best.map(|(index, _)| index)
        };
        let (initial_line, maintenance_line) = (carrier(&initial), carrier(&maintenance));
        let positions = legs
            .iter()
            .enumerate()
            .map(|(index, leg)| PositionRequirement {
                instrument: leg.instrument.clone(),
                contracts: leg.contracts,
                notional: None,
                slices: Vec::new(),
                initial: if initial_line == Some(index) {
                    initial.figure
                } else {
                    Decimal::ZERO
                },
                maintenance: if maintenance_line == Some(index) {
                    maintenance.figure
                } else {
                    Decimal::ZERO
                },
            })
            .collect();
        Ok(ScenarioPmRequirement {
            requirement: MarginRequirement {
                rule: self.id.clone(),
                account,
                venue,
                currency: self.currency.clone(),
                initial: initial.figure,
                maintenance: maintenance.figure,
                positions,
            },
            initial,
            maintenance,
        })
    }

    /// One figure over one scenario set.
    fn figure(
        &self,
        legs: &[PmLeg],
        orders: &[PmOrder],
        by_id: &BTreeMap<&InstrumentId, &PmLeg>,
        scenarios: &[BTreeMap<InstrumentId, Decimal>],
        rates: ScenarioPmRates,
    ) -> Result<PmBreakdown> {
        let overflow = || MarginError::arithmetic("scenario-pm");
        // The worst scenario: its index, its exact total and its terms.
        let mut worst: Option<(usize, Exact, Terms)> = None;
        for (index, scenario) in scenarios.iter().enumerate() {
            let value = |instrument: &InstrumentId, kind: PmLegKind| -> Result<Decimal> {
                let x = *scenario
                    .get(instrument)
                    .ok_or_else(|| MarginError::MissingMark {
                        instrument: instrument.clone(),
                    })?;
                check_value(kind, x, "scenario value")?;
                Ok(x)
            };
            // -Q M X_s + Q M X0 for every leg.
            let mut terms: Vec<Vec<Decimal>> = Vec::new();
            for leg in legs {
                let x = value(&leg.instrument, leg.kind)?;
                terms.push(vec![neg(leg.contracts), leg.multiplier, x]);
                terms.push(vec![leg.contracts, leg.multiplier, leg.mark]);
            }
            for order in orders {
                let leg = by_id
                    .get(&order.instrument)
                    .ok_or_else(|| MarginError::invalid_input("orders", "an order needs a leg"))?;
                let x = value(&order.instrument, leg.kind)?;
                let contribution = [
                    vec![neg(order.contracts), leg.multiplier, x],
                    vec![order.contracts, leg.multiplier, order.limit_price],
                ];
                let refs: Vec<&[Decimal]> = contribution.iter().map(Vec::as_slice).collect();
                let exact = sum_products_exact(&refs).ok_or_else(overflow)?;
                if exact.0.cmp_signed(&crate::decimal::Wide::ZERO) == Ordering::Greater {
                    terms.extend(contribution);
                }
            }
            let refs: Vec<&[Decimal]> = terms.iter().map(Vec::as_slice).collect();
            let total = sum_products_exact(&refs).ok_or_else(overflow)?;
            let replace = match &worst {
                None => true,
                Some((_, best, _)) => {
                    compare_exact(total, *best).ok_or_else(overflow)? == Ordering::Greater
                }
            };
            if replace {
                worst = Some((index, total, terms));
            }
        }
        let (index, total, terms) = worst.ok_or_else(|| {
            MarginError::invalid_input("scenarios", "each set needs at least one scenario")
        })?;
        let scenario = scenarios.get(index).ok_or_else(overflow)?;
        let positive = total.0.cmp_signed(&crate::decimal::Wide::ZERO) == Ordering::Greater;

        let (leg_terms, order_terms) = worst_terms(legs, orders, by_id, scenario)?;
        let (option_floor, linear_floor) = floor_terms(legs, orders, rates)?;
        let exact = |terms: &[Vec<Decimal>], operation| -> Result<Decimal> {
            let refs: Vec<&[Decimal]> = terms.iter().map(Vec::as_slice).collect();
            sum_products(&refs, operation)
        };
        let short_option_floor = exact(&option_floor, "short option floor")?;
        let linear = exact(&linear_floor, "linear floor")?;
        let scenario_loss = if positive {
            exact(&terms, "scenario loss")?
        } else {
            Decimal::ZERO
        };

        let mut all: Vec<&[Decimal]> = Vec::new();
        if positive {
            all.extend(terms.iter().map(Vec::as_slice));
        }
        all.extend(option_floor.iter().map(Vec::as_slice));
        all.extend(linear_floor.iter().map(Vec::as_slice));
        let figure = sum_terms_round(&all, self.scale, Rounding::AwayFromZero, "scenario-pm")?;
        Ok(PmBreakdown {
            worst: index,
            leg_terms,
            order_terms,
            scenario_loss,
            short_option_floor,
            linear_floor: linear,
            figure,
        })
    }
}

/// Product terms summed exactly.
type Terms = Vec<Vec<Decimal>>;

/// An exact wide value at its scale.
type Exact = (crate::decimal::Wide, u32);

/// Each leg's loss and each order's positive contribution in `scenario`.
#[expect(clippy::type_complexity, reason = "two parallel breakdown lists")]
fn worst_terms(
    legs: &[PmLeg],
    orders: &[PmOrder],
    by_id: &BTreeMap<&InstrumentId, &PmLeg>,
    scenario: &BTreeMap<InstrumentId, Decimal>,
) -> Result<(Vec<(InstrumentId, Decimal)>, Vec<(InstrumentId, Decimal)>)> {
    let mut leg_terms = Vec::with_capacity(legs.len());
    for leg in legs {
        let x = *scenario
            .get(&leg.instrument)
            .ok_or_else(|| MarginError::MissingMark {
                instrument: leg.instrument.clone(),
            })?;
        let loss = sum_products(
            &[
                &[neg(leg.contracts), leg.multiplier, x],
                &[leg.contracts, leg.multiplier, leg.mark],
            ],
            "leg loss",
        )?;
        leg_terms.push((leg.instrument.clone(), loss));
    }
    let mut order_terms = Vec::with_capacity(orders.len());
    for order in orders {
        let leg = by_id
            .get(&order.instrument)
            .ok_or_else(|| MarginError::invalid_input("orders", "an order needs a leg"))?;
        let x = *scenario
            .get(&order.instrument)
            .ok_or_else(|| MarginError::MissingMark {
                instrument: order.instrument.clone(),
            })?;
        let contribution = sum_products(
            &[
                &[neg(order.contracts), leg.multiplier, x],
                &[order.contracts, leg.multiplier, order.limit_price],
            ],
            "order term",
        )?;
        order_terms.push((order.instrument.clone(), contribution.max(Decimal::ZERO)));
    }
    Ok((leg_terms, order_terms))
}

/// The terms of `F` (every option sell filled, no buy) and of `G` (the
/// linear legs with their orders' contracts).
fn floor_terms(
    legs: &[PmLeg],
    orders: &[PmOrder],
    rates: ScenarioPmRates,
) -> Result<(Terms, Terms)> {
    let mut option_floor: Vec<Vec<Decimal>> = Vec::new();
    let mut linear_floor: Vec<Vec<Decimal>> = Vec::new();
    for leg in legs {
        let mine = orders.iter().filter(|o| o.instrument == leg.instrument);
        match leg.kind {
            PmLegKind::Option { underlying_price } => {
                let mut worst_contracts = vec![leg.contracts];
                worst_contracts.extend(mine.map(|o| o.contracts).filter(|q| *q < Decimal::ZERO));
                let short = neg(sum(&worst_contracts, "worst short")?).max(Decimal::ZERO);
                if !short.is_zero() {
                    option_floor.push(vec![
                        rates.short_option_floor_rate,
                        short,
                        leg.multiplier,
                        underlying_price,
                    ]);
                }
            }
            PmLegKind::Linear => {
                let mut gross = vec![leg.contracts.abs()];
                gross.extend(mine.map(|o| o.contracts.abs()));
                let gross = sum(&gross, "gross contracts")?;
                linear_floor.push(vec![
                    rates.linear_floor_rate,
                    gross,
                    leg.multiplier,
                    leg.mark,
                ]);
            }
        }
    }
    Ok((option_floor, linear_floor))
}

/// Checks the legs and orders; the legs by instrument.
fn validate<'a>(
    legs: &'a [PmLeg],
    orders: &[PmOrder],
) -> Result<BTreeMap<&'a InstrumentId, &'a PmLeg>> {
    let mut by_id = BTreeMap::new();
    for leg in legs {
        if by_id.insert(&leg.instrument, leg).is_some() {
            return Err(MarginError::invalid_input(
                "legs",
                "must not repeat an instrument",
            ));
        }
        if leg.multiplier <= Decimal::ZERO {
            return Err(MarginError::invalid_input("multiplier", "must be positive"));
        }
        check_value(leg.kind, leg.mark, "mark")?;
        if let PmLegKind::Option { underlying_price } = leg.kind
            && underlying_price <= Decimal::ZERO
        {
            return Err(MarginError::invalid_input(
                "underlying_price",
                "must be positive",
            ));
        }
    }
    for order in orders {
        let leg = by_id
            .get(&order.instrument)
            .ok_or_else(|| MarginError::invalid_input("orders", "an order needs a leg"))?;
        if order.contracts.is_zero() {
            return Err(MarginError::invalid_input(
                "orders",
                "contracts must not be zero",
            ));
        }
        check_value(leg.kind, order.limit_price, "limit_price")?;
    }
    Ok(by_id)
}

/// A linear value is a positive price; an option value a premium of at
/// least zero.
fn check_value(kind: PmLegKind, value: Decimal, field: &'static str) -> Result<()> {
    let valid = match kind {
        PmLegKind::Linear => value > Decimal::ZERO,
        PmLegKind::Option { .. } => value >= Decimal::ZERO,
    };
    if valid {
        Ok(())
    } else {
        Err(MarginError::invalid_input(
            field,
            "a price must be positive and a premium at least zero",
        ))
    }
}

impl InitialMarginModel for ScenarioPm {
    fn rule(&self) -> &RuleId {
        &self.id
    }

    /// The requirement of a view of linear futures and perpetuals, each a
    /// [`PmLegKind::Linear`] leg at its mark.
    ///
    /// # Errors
    ///
    /// As [`ScenarioPm::requirement_for`], and
    /// [`MarginError::UnsupportedInstrument`] for an inverse position or an
    /// option (an option needs its underlying price: use
    /// [`ScenarioPm::requirement_for`]).
    fn requirement(&self, view: &ExposureView) -> Result<MarginRequirement> {
        let legs = view
            .positions
            .iter()
            .map(|position| self.leg_of(position))
            .collect::<Result<Vec<_>>>()?;
        Ok(self
            .requirement_for(view.account.clone(), view.venue.clone(), &legs)?
            .requirement)
    }
}

impl ScenarioPm {
    fn leg_of(&self, position: &ExposurePosition) -> Result<PmLeg> {
        let unsupported = |reason| MarginError::UnsupportedInstrument {
            rule: self.id.to_string(),
            instrument: position.instrument.clone(),
            reason,
        };
        match (position.product, position.size) {
            (
                ProductKind::LinearFuture | ProductKind::LinearPerpetual,
                ContractSize::Multiplier(multiplier),
            ) => Ok(PmLeg {
                instrument: position.instrument.clone(),
                kind: PmLegKind::Linear,
                contracts: position.contracts,
                multiplier,
                mark: position.mark,
            }),
            (ProductKind::Option, _) => {
                Err(unsupported("an option leg needs its underlying price"))
            }
            _ => Err(unsupported("only linear futures and perpetuals")),
        }
    }
}

/// A rule that nets: the closeout plan evaluates it count by count.
impl AccountMargin for ScenarioPm {
    fn plan_cash_scale(&self) -> u32 {
        self.scale
    }

    fn plan_tiers(&self, _position: &ExposurePosition) -> Option<Vec<MarginTier>> {
        None
    }
}
