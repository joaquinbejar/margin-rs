//! Tiered initial and maintenance margin for inverse futures and
//! perpetuals.
//!
//! # Formula
//!
//! An inverse position settles in the base asset. With signed contracts
//! `Qc`, contract value `CV` (quote units per contract) and `mark` (in the
//! quote currency of `CV` per unit of the base), the notional is in the
//! base:
//!
//! ```text
//! notional = |Qc| * CV / mark
//! im = round_up( sum over tiers k of im_rate_k * slice_k )
//! mm = round_up( sum over tiers k of mm_rate_k * slice_k )
//! slice_k = max(0, min(notional, floor_{k+1}) - floor_k)
//! ```
//!
//! with the floors in the base. The notional is a quotient, generally not
//! a finite decimal: each tier is compared exactly by cross products
//! (`|Qc| * CV > floor * mark`), and each margin is one exact sum over the
//! common denominator `mark`, rounded up once to the base scale. Totals are
//! the exact sums of the rounded positions. The reported notionals are
//! informational (rounded half-even to 18 decimals).
//!
//! At a fixed mark the notional is linear in `Qc`, so the margin is a
//! convex, piecewise linear function of the contracts, as for linear
//! contracts. Its convexity in the price is the inverse contract's own.

use rust_decimal::Decimal;

use crate::error::{MarginError, Result};
use crate::ids::Asset;
use crate::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement, PiecewiseLinearMargin,
    PositionRequirement, RuleId, check_size,
};
use crate::rules::{MarginTier, tiers};

/// Tiered inverse margin rule. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TieredInverseIm {
    id: RuleId,
    currency: Asset,
    scale: u32,
    tiers: Vec<MarginTier>,
}

impl TieredInverseIm {
    /// The rule `inverse-im/1`: one tier, initial 10 %, maintenance 5 % of
    /// the notional in the base. `base` is the settlement asset and
    /// `scale` its number of decimals.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if `scale` exceeds
    /// [`MAX_SCALE`](crate::decimal::MAX_SCALE).
    pub fn inverse_v1(base: Asset, scale: u32) -> Result<Self> {
        Self::new(
            RuleId::new("inverse-im", 1),
            base,
            scale,
            vec![MarginTier {
                floor: Decimal::ZERO,
                initial_rate: Decimal::new(10, 2),
                maintenance_rate: Decimal::new(5, 2),
            }],
        )
    }

    /// A rule with an explicit tier table, floors in the base asset. Use a
    /// distinct [`RuleId`] for every table.
    ///
    /// # Errors
    ///
    /// As [`TieredLinearIm::new`](crate::rules::TieredLinearIm::new).
    pub fn new(id: RuleId, base: Asset, scale: u32, tiers: Vec<MarginTier>) -> Result<Self> {
        tiers::validate(scale, &tiers)?;
        Ok(Self {
            id,
            currency: base,
            scale,
            tiers,
        })
    }

    /// Settlement currency of every amount: the base asset.
    #[must_use]
    pub fn currency(&self) -> &Asset {
        &self.currency
    }

    /// Scale requirements are rounded up to.
    #[must_use]
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// The tier table.
    #[must_use]
    pub fn tiers(&self) -> &[MarginTier] {
        &self.tiers
    }

    fn position(&self, position: &ExposurePosition) -> Result<PositionRequirement> {
        if !position.product.is_inverse() {
            return Err(MarginError::UnsupportedInstrument {
                rule: self.id.to_string(),
                instrument: position.instrument.clone(),
                reason: "only inverse futures and perpetuals are covered",
            });
        }
        if position.settlement != self.currency {
            return Err(MarginError::CurrencyMismatch {
                field: "settlement",
                expected: self.currency.clone(),
                found: position.settlement.clone(),
            });
        }
        let contract_value = check_size(position.product, position.size)?;
        if position.mark <= Decimal::ZERO {
            return Err(MarginError::invalid_input("mark", "must be positive"));
        }
        let tiered = tiers::requirement(
            &self.tiers,
            &[position.contracts.abs(), contract_value],
            &[position.mark],
            self.scale,
        )?;
        Ok(PositionRequirement {
            instrument: position.instrument.clone(),
            contracts: position.contracts,
            notional: tiered.notional,
            slices: tiered.slices,
            initial: tiered.initial,
            maintenance: tiered.maintenance,
        })
    }
}

/// The tier floors above 0, in the base. Rates never decrease from one
/// tier to the next, so each margin is convex in the notional, which at a
/// fixed mark is linear in the contracts.
impl PiecewiseLinearMargin for TieredInverseIm {
    fn cash_scale(&self) -> u32 {
        self.scale
    }

    fn notional_breakpoints(&self, _position: &ExposurePosition) -> Vec<Decimal> {
        self.tiers.iter().skip(1).map(|tier| tier.floor).collect()
    }
}

impl InitialMarginModel for TieredInverseIm {
    fn rule(&self) -> &RuleId {
        &self.id
    }

    /// See the [module docs](self).
    ///
    /// # Errors
    ///
    /// - [`MarginError::UnsupportedInstrument`] for any product other than
    ///   inverse futures and perpetuals.
    /// - [`MarginError::CurrencyMismatch`] if a position settles in another
    ///   asset than the rule's base.
    /// - [`MarginError::InvalidInput`] for a size that is not a positive
    ///   contract value, a non positive mark, or two positions in the same
    ///   instrument.
    /// - [`MarginError::Arithmetic`] if a margin cannot be represented.
    fn requirement(&self, view: &ExposureView) -> Result<MarginRequirement> {
        tiers::account_requirement(&self.id, &self.currency, view, |p| self.position(p))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use crate::initial_margin::ProductKind;

    #[test]
    fn product_kind_is_checked() {
        assert!(ProductKind::InverseFuture.is_inverse());
        assert!(!ProductKind::LinearFuture.is_inverse());
    }
}
