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
//! Rules live in [`crate::rules`]. The first one is
//! [`TieredLinearIm::linear_v1`](crate::rules::TieredLinearIm::linear_v1).

use core::fmt;

use rust_decimal::Decimal;

use crate::error::Result;
use crate::ids::{AccountId, Asset, InstrumentId, VenueId};

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
    /// Contract multiplier (`M`): underlying units per contract.
    pub multiplier: Decimal,
    /// Mark price, in the settlement asset per underlying unit.
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
    /// Notional in this tier.
    pub notional: Decimal,
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
    /// `|contracts| * multiplier * mark`, exact.
    pub notional: Decimal,
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
    /// [`MarginError::UnsupportedInstrument`](crate::MarginError::UnsupportedInstrument)
    /// for a product the rule does not cover.
    fn requirement(&self, view: &ExposureView) -> Result<MarginRequirement>;
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
///   non decreasing and piecewise linear functions of its notional
///   `|Qc| * M * mark`, with slope changes only at
///   [`PiecewiseLinearMargin::notional_breakpoints`].
pub trait PiecewiseLinearMargin: InitialMarginModel {
    /// Number of decimals every requirement is rounded to: the cash
    /// quantum of the rule's currency.
    fn cash_scale(&self) -> u32;

    /// Notionals at which the margin of `position` changes slope,
    /// ascending and positive; empty for a single linear rate.
    fn notional_breakpoints(&self, position: &ExposurePosition) -> Vec<Decimal>;
}
