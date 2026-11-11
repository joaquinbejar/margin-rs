//! Initial margin: the exposure a rule reads and the requirement it returns.
//!
//! An [`InitialMarginModel`] turns one account's [`ExposureView`] at one
//! venue into a [`MarginRequirement`]. Every requirement carries the
//! [`RuleId`] (name and version) that produced it and a per position
//! breakdown, so a figure can always be traced back to the rule, the tier
//! slices and the inputs.
//!
//! A view belongs to exactly one account at one venue. No rule nets
//! exposure across accounts or venues: requirements for different views are
//! computed separately and never combined by this crate.
//!
//! Rules live in [`crate::rules`]:
//! [`TieredLinearIm`](crate::rules::TieredLinearIm) for linear futures and
//! perpetuals and [`TieredInverseIm`](crate::rules::TieredInverseIm) for
//! inverse ones.
//!
//! # Notional
//!
//! A position's notional is in its settlement asset: `|Qc| * M * mark` for
//! a linear contract (multiplier `M`, mark per unit of the underlying in
//! the settlement asset) and `|Qc| * CV / mark` for an inverse one
//! (contract value `CV` in quote units, mark in that quote currency per
//! unit of the base, which is the settlement asset). At a fixed mark both
//! are linear in `Qc`.

use core::fmt;

use rust_decimal::Decimal;

use crate::error::{MarginError, Result};
use crate::ids::{AccountId, Asset, InstrumentId, VenueId};
use crate::instrument::ContractSize;

/// Name and version of a margin rule, for example `linear-im/1`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuleId {
    /// Rule name.
    pub name: String,
    /// Rule version. A change to the formula or to its parameters is a new
    /// version.
    pub version: u32,
}

impl RuleId {
    /// Builds a rule identifier.
    #[must_use]
    pub fn new(name: impl Into<String>, version: u32) -> Self {
        Self {
            name: name.into(),
            version,
        }
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.name, self.version)
    }
}

/// Kind of product a position is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProductKind {
    /// Spot: paid in full, no margin.
    Spot,
    /// Linear (quote settled) dated future.
    LinearFuture,
    /// Linear (quote settled) perpetual.
    LinearPerpetual,
    /// Option.
    Option,
    /// Inverse (base settled) dated future.
    InverseFuture,
    /// Inverse (base settled) perpetual.
    InversePerpetual,
}

impl ProductKind {
    /// True for inverse futures and perpetuals.
    #[must_use]
    pub fn is_inverse(self) -> bool {
        matches!(self, Self::InverseFuture | Self::InversePerpetual)
    }
}

/// Checks that `size` is positive and of the kind `product` needs: a
/// multiplier for a linear product, a contract value for an inverse one.
///
/// # Errors
///
/// [`MarginError::InvalidInput`] otherwise.
pub(crate) fn check_size(product: ProductKind, size: ContractSize) -> Result<Decimal> {
    let (value, matches) = match size {
        ContractSize::Multiplier(m) => (m, !product.is_inverse()),
        ContractSize::ContractValue(cv) => (cv, product.is_inverse()),
    };
    if !matches {
        return Err(MarginError::invalid_input(
            "size",
            "a linear product takes a multiplier, an inverse one a contract value",
        ));
    }
    if value <= Decimal::ZERO {
        return Err(MarginError::invalid_input("size", "must be positive"));
    }
    Ok(value)
}

/// One position as a margin rule sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposurePosition {
    /// Instrument.
    pub instrument: InstrumentId,
    /// Product kind.
    pub product: ProductKind,
    /// Asset the instrument settles in.
    pub settlement: Asset,
    /// Signed number of contracts (`Qc`): positive long, negative short.
    pub contracts: Decimal,
    /// Contract size: the multiplier `M` (underlying units per contract) of
    /// a linear product, the contract value `CV` (quote units per contract)
    /// of an inverse one.
    pub size: ContractSize,
    /// Mark price per unit of the underlying: in the settlement asset for a
    /// linear product, in the quote currency of `CV` for an inverse one.
    pub mark: Decimal,
}

/// The positions of one account at one venue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureView {
    /// Account.
    pub account: AccountId,
    /// Venue.
    pub venue: VenueId,
    /// Positions, at most one per instrument.
    pub positions: Vec<ExposurePosition>,
}

/// The part of a position's notional that falls in one tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierSlice {
    /// Index of the tier in the rule's table.
    pub tier: usize,
    /// Notional in this tier. Informational: exact for a linear position,
    /// rounded half-even to 18 decimals for an inverse one, and `None` when
    /// it does not fit a `Decimal`. The margins are computed from the exact
    /// notional, never from this figure.
    pub notional: Option<Decimal>,
    /// Initial margin rate of the tier.
    pub initial_rate: Decimal,
    /// Maintenance margin rate of the tier.
    pub maintenance_rate: Decimal,
}

/// Requirement of one position, with how it was computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionRequirement {
    /// Instrument.
    pub instrument: InstrumentId,
    /// Signed contracts.
    pub contracts: Decimal,
    /// The notional (see the [module docs](self#notional)). Informational:
    /// exact for a linear position, rounded half-even to 18 decimals for an
    /// inverse one, and `None` when it does not fit a `Decimal`.
    pub notional: Option<Decimal>,
    /// How the notional splits across tiers.
    pub slices: Vec<TierSlice>,
    /// `sum(slice notional * initial rate)`, rounded up once to the
    /// settlement asset scale.
    pub initial: Decimal,
    /// `sum(slice notional * maintenance rate)`, rounded up once.
    pub maintenance: Decimal,
}

/// Margin required for one account at one venue under one rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginRequirement {
    /// Rule and version that produced this requirement.
    pub rule: RuleId,
    /// Account.
    pub account: AccountId,
    /// Venue.
    pub venue: VenueId,
    /// Asset of every amount.
    pub currency: Asset,
    /// Total initial margin: the exact sum of the positions' `initial`.
    pub initial: Decimal,
    /// Total maintenance margin: the exact sum of the positions'
    /// `maintenance`.
    pub maintenance: Decimal,
    /// Breakdown per position, in the order of the view.
    pub positions: Vec<PositionRequirement>,
}

/// A versioned initial margin rule.
pub trait InitialMarginModel {
    /// Name and version of the rule.
    fn rule(&self) -> &RuleId;

    /// Margin required by `view`.
    ///
    /// # Errors
    ///
    /// Rule specific; at least
    /// [`MarginError::UnsupportedInstrument`]
    /// for a product the rule does not cover.
    fn requirement(&self, view: &ExposureView) -> Result<MarginRequirement>;
}

/// A margin rule the closeout planner
/// ([`crate::liquidation::plan_liquidation`]) can drive: its requirement is
/// evaluated on the whole projected account, so it may net across
/// positions.
///
/// Contract:
///
/// - [`InitialMarginModel::requirement`] is deterministic and lists one
///   [`PositionRequirement`] per position of the view; the planner closes
///   instruments in decreasing order of their line's `maintenance` (ties by
///   instrument id), so a netting rule chooses that order by how it
///   attributes the requirement to its lines;
/// - every amount is rounded to [`AccountMargin::plan_cash_scale`].
///
/// Every [`PiecewiseLinearMargin`] is an `AccountMargin` whose tiers the
/// planner searches exactly. Any other rule gets no
/// [`AccountMargin::plan_tiers`]: the planner then evaluates the counts of
/// a level one by one, in increasing order, within
/// [`SEARCH_BUDGET`](crate::liquidation::SEARCH_BUDGET).
pub trait AccountMargin: InitialMarginModel {
    /// Number of decimals every requirement is rounded to: the cash
    /// quantum of the rule's currency.
    fn plan_cash_scale(&self) -> u32;

    /// The exact tiers of `position`'s margin when the requirement is the
    /// sum of per-position piecewise linear margins (see
    /// [`PiecewiseLinearMargin`]); `None` for a rule that nets.
    fn plan_tiers(&self, position: &ExposurePosition) -> Option<Vec<crate::rules::MarginTier>>;
}

impl<T: PiecewiseLinearMargin> AccountMargin for T {
    fn plan_cash_scale(&self) -> u32 {
        self.cash_scale()
    }

    fn plan_tiers(&self, position: &ExposurePosition) -> Option<Vec<crate::rules::MarginTier>> {
        Some(self.margin_tiers(position))
    }
}

/// A margin rule whose requirement the closeout planner can search
/// exactly ([`crate::liquidation::plan_liquidation`]).
///
/// Contract, for every position the rule covers:
///
/// - the requirement of an account is the sum of the requirements of its
///   positions (no netting between positions);
/// - a position's initial and maintenance margin are, up to the final
///   rounding to [`PiecewiseLinearMargin::cash_scale`], continuous, convex,
///   non decreasing and piecewise linear functions of its notional (see
///   the [module docs](self#notional)), given exactly by
///   [`PiecewiseLinearMargin::margin_tiers`]: before the final rounding,
///   each margin is `sum over tiers k of rate_k * slice_k` with
///   `slice_k = max(0, min(notional, floor_{k+1}) - floor_k)`.
pub trait PiecewiseLinearMargin: InitialMarginModel {
    /// Number of decimals every requirement is rounded to: the cash
    /// quantum of the rule's currency.
    fn cash_scale(&self) -> u32;

    /// The tiers that define the margins of `position` before rounding:
    /// floors ascending from 0, with rates that never decrease. The
    /// closeout planner reads the exact, unrounded margin from them.
    fn margin_tiers(&self, position: &ExposurePosition) -> Vec<crate::rules::MarginTier>;
}
