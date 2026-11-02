//! Risk factors and exposures with explicit units.
//!
//! A factor is something that moves: a price, an implied volatility, a
//! rate. Its [`FactorKind`] fixes the unit of one move. An exposure is the
//! change in value, in one currency, per **one unit move** of its factor,
//! so it carries the kind it was measured against:
//!
//! | Kind | One unit move | Exposure example |
//! |---|---|---|
//! | [`FactorKind::PriceReturn`] | a relative price change of 1 (100 %) | cash delta: 1000 USDC per unit return = 10 USDC per 1 % |
//! | [`FactorKind::VolatilityPoint`] | implied volatility up one point (0.01) | vega: 50 USDC per vol point |
//! | [`FactorKind::RatePoint`] | a rate up one basis point (0.0001) | DV01: 20 USDC per basis point |
//!
//! [`portfolio_variance`](crate::covariance::portfolio_variance) refuses an
//! exposure whose kind differs from its factor's, so a vega exposure can
//! never be read as price risk.

use core::fmt;

use rust_decimal::Decimal;

use crate::ids::{Asset, FactorId};

/// What one unit move of a factor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FactorKind {
    /// Relative price change; one unit is a return of 1 (100 %).
    PriceReturn,
    /// Implied volatility change; one unit is one volatility point (0.01).
    VolatilityPoint,
    /// Interest rate change; one unit is one basis point (0.0001).
    RatePoint,
}

impl FactorKind {
    /// Name of the unit, used in errors.
    #[must_use]
    pub fn unit(self) -> &'static str {
        match self {
            Self::PriceReturn => "price return",
            Self::VolatilityPoint => "volatility point",
            Self::RatePoint => "rate basis point",
        }
    }
}

impl fmt::Display for FactorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.unit())
    }
}

/// A risk factor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Factor {
    /// Identifier.
    pub id: FactorId,
    /// Unit of one move.
    pub kind: FactorKind,
}

impl Factor {
    /// Builds a factor.
    #[must_use]
    pub fn new(id: FactorId, kind: FactorKind) -> Self {
        Self { id, kind }
    }
}

/// Change in value per one unit move of a factor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactorExposure {
    /// Factor the exposure is measured against.
    pub factor: FactorId,
    /// Unit the exposure was measured in; must equal the factor's kind.
    pub kind: FactorKind,
    /// Currency of `amount`.
    pub currency: Asset,
    /// Value change per unit move, signed.
    pub amount: Decimal,
}

impl FactorExposure {
    /// Builds an exposure.
    #[must_use]
    pub fn new(factor: FactorId, kind: FactorKind, currency: Asset, amount: Decimal) -> Self {
        Self {
            factor,
            kind,
            currency,
            amount,
        }
    }
}
