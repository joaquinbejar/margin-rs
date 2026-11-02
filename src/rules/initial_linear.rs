//! Tiered initial and maintenance margin for linear futures and perpetuals.
//!
//! # Formula
//!
//! For a position with signed contracts `Qc`, multiplier `M` and mark
//! `mark`, all in the settlement currency:
//!
//! ```text
//! notional = |Qc| * M * mark
//! im = round_up( sum over tiers k of im_rate_k * slice_k )
//! mm = round_up( sum over tiers k of mm_rate_k * slice_k )
//! slice_k = max(0, min(notional, floor_{k+1}) - floor_k)
//! ```
//!
//! Tiers apply progressively (each rate to the part of the notional inside
//! its band), so the requirement is continuous at every tier boundary. The
//! sum of the slice products is exact; each position is rounded up once to
//! the currency scale, and totals are the exact sum of the rounded
//! positions, so the breakdown always adds up to the total.
//!
//! Positions are margined gross, one per instrument: no netting between
//! instruments, accounts or venues.
//!
//! # `linear-im/1`
//!
//! [`TieredLinearIm::linear_v1`] is the crate's rule for linear USDC
//! futures and perpetuals:
//!
//! ```text
//! im = 0.10 * |Qc| * M * mark
//! mm = 0.05 * |Qc| * M * mark
//! ```
//!
//! It is the table with a single tier `[0, inf)` at `im_rate = 0.10` and
//! `mm_rate = 0.05`. Spot and options are outside this rule: spot is
//! margined by cash and options by a different formula.

use rust_decimal::Decimal;

use crate::error::{MarginError, Result};
use crate::ids::Asset;
use crate::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement, PiecewiseLinearMargin,
    PositionRequirement, ProductKind, RuleId, check_size,
};
use crate::rules::tiers;

/// One band of a tier table: the rates apply to the part of the notional
/// at or above `floor` and below the next tier's floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginTier {
    /// Lower bound of the band, in the settlement currency.
    pub floor: Decimal,
    /// Initial margin rate, in `(0, 1]`.
    pub initial_rate: Decimal,
    /// Maintenance margin rate, in `(0, initial_rate]`.
    pub maintenance_rate: Decimal,
}

/// Tiered linear margin rule. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TieredLinearIm {
    id: RuleId,
    currency: Asset,
    scale: u32,
    tiers: Vec<MarginTier>,
}

impl TieredLinearIm {
    /// The rule `linear-im/1`: one tier, initial 10 %, maintenance 5 %.
    /// `usdc` is the settlement asset and `scale` its number of decimals.
    ///
    /// The identifier names exactly these rates. Other `im_rate` or
    /// `mm_rate` values are a different rule: build it with
    /// [`TieredLinearIm::new`] and its own [`RuleId`] or version, never
    /// with this one.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if `scale` exceeds
    /// [`MAX_SCALE`](crate::decimal::MAX_SCALE).
    pub fn linear_v1(usdc: Asset, scale: u32) -> Result<Self> {
        Self::new(
            RuleId::new("linear-im", 1),
            usdc,
            scale,
            vec![MarginTier {
                floor: Decimal::ZERO,
                initial_rate: Decimal::new(10, 2),
                maintenance_rate: Decimal::new(5, 2),
            }],
        )
    }

    /// A rule with an explicit tier table. Use a distinct [`RuleId`] for
    /// every table: the identifier is what audits key on.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] unless the table is non empty, starts
    /// at floor 0 with strictly increasing floors, has
    /// `0 < maintenance_rate <= initial_rate <= 1` in every tier, and rates
    /// that never decrease from one tier to the next; or if `scale`
    /// exceeds [`MAX_SCALE`](crate::decimal::MAX_SCALE).
    pub fn new(id: RuleId, currency: Asset, scale: u32, tiers: Vec<MarginTier>) -> Result<Self> {
        tiers::validate(scale, &tiers)?;
        Ok(Self {
            id,
            currency,
            scale,
            tiers,
        })
    }

    /// Settlement currency of every amount.
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
        match position.product {
            ProductKind::LinearFuture | ProductKind::LinearPerpetual => {}
            _ => {
                return Err(MarginError::UnsupportedInstrument {
                    rule: self.id.to_string(),
                    instrument: position.instrument.clone(),
                    reason: "only linear futures and perpetuals are covered",
                });
            }
        }
        if position.settlement != self.currency {
            return Err(MarginError::CurrencyMismatch {
                field: "settlement",
                expected: self.currency.clone(),
                found: position.settlement.clone(),
            });
        }
        let multiplier = check_size(position.product, position.size)?;
        if position.mark <= Decimal::ZERO {
            return Err(MarginError::invalid_input("mark", "must be positive"));
        }
        // The notional |Qc| * M * mark is one exact product: intermediate
        // |Qc| * M may not be representable even when the notional is, and
        // the notional itself need not be (it is then reported as None).
        let tiered = tiers::requirement(
            &self.tiers,
            &[position.contracts.abs(), multiplier, position.mark],
            &[],
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

/// The rule's tier table. Rates never decrease from one tier to the next
/// ([`TieredLinearIm::new`] rejects such a table), so each margin is convex
/// in the notional.
impl PiecewiseLinearMargin for TieredLinearIm {
    fn cash_scale(&self) -> u32 {
        self.scale
    }

    fn margin_tiers(&self, _position: &ExposurePosition) -> Vec<MarginTier> {
        self.tiers.clone()
    }
}

impl InitialMarginModel for TieredLinearIm {
    fn rule(&self) -> &RuleId {
        &self.id
    }

    /// See the [module docs](self).
    ///
    /// # Errors
    ///
    /// - [`MarginError::UnsupportedInstrument`] for spot, options or any
    ///   product other than linear futures and perpetuals.
    /// - [`MarginError::CurrencyMismatch`] if a position settles in another
    ///   asset than the rule's currency.
    /// - [`MarginError::InvalidInput`] for a size that is not a positive
    ///   multiplier, a non positive mark, or two positions in the same
    ///   instrument.
    /// - [`MarginError::Arithmetic`] if an amount cannot be represented.
    fn requirement(&self, view: &ExposureView) -> Result<MarginRequirement> {
        tiers::account_requirement(&self.id, &self.currency, view, |p| self.position(p))
    }
}
