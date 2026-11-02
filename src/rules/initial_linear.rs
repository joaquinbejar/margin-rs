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

use std::collections::BTreeSet;

use rust_decimal::Decimal;

use crate::decimal::{Rounding, check_configured_scale, product, sub, sum, sum_terms_round};
use crate::error::{MarginError, Result};
use crate::ids::Asset;
use crate::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement, PiecewiseLinearMargin,
    PositionRequirement, ProductKind, RuleId, TierSlice,
};

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
        check_configured_scale(scale, "scale")?;
        let invalid = |reason| Err(MarginError::invalid_input("tiers", reason));
        let Some(first) = tiers.first() else {
            return invalid("must not be empty");
        };
        if !first.floor.is_zero() {
            return invalid("the first floor must be 0");
        }
        for tier in &tiers {
            if tier.maintenance_rate <= Decimal::ZERO
                || tier.maintenance_rate > tier.initial_rate
                || tier.initial_rate > Decimal::ONE
            {
                return invalid("rates must satisfy 0 < maintenance <= initial <= 1");
            }
        }
        for pair in tiers.windows(2) {
            if pair[1].floor <= pair[0].floor {
                return invalid("floors must be strictly increasing");
            }
            if pair[1].initial_rate < pair[0].initial_rate
                || pair[1].maintenance_rate < pair[0].maintenance_rate
            {
                return invalid("rates must not decrease from one tier to the next");
            }
        }
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
        if position.multiplier <= Decimal::ZERO {
            return Err(MarginError::invalid_input("multiplier", "must be positive"));
        }
        if position.mark <= Decimal::ZERO {
            return Err(MarginError::invalid_input("mark", "must be positive"));
        }
        // One exact product: intermediate |Qc| * M may not be representable
        // even when the notional is.
        let notional = product(
            &[position.contracts.abs(), position.multiplier, position.mark],
            "|Qc| * M * mark",
        )?;

        let mut slices = Vec::new();
        for (index, tier) in self.tiers.iter().enumerate() {
            if notional <= tier.floor {
                break;
            }
            let top = match self.tiers.get(index + 1) {
                Some(next) => notional.min(next.floor),
                None => notional,
            };
            slices.push(TierSlice {
                tier: index,
                notional: sub(top, tier.floor, "slice")?,
                initial_rate: tier.initial_rate,
                maintenance_rate: tier.maintenance_rate,
            });
        }
        let round_up = |rate: fn(&TierSlice) -> Decimal, op| {
            let terms: Vec<[Decimal; 2]> = slices.iter().map(|s| [s.notional, rate(s)]).collect();
            let terms: Vec<&[Decimal]> = terms.iter().map(|t| t.as_slice()).collect();
            sum_terms_round(&terms, self.scale, Rounding::AwayFromZero, op)
        };
        let initial = round_up(|s| s.initial_rate, "initial margin")?;
        let maintenance = round_up(|s| s.maintenance_rate, "maintenance margin")?;
        Ok(PositionRequirement {
            instrument: position.instrument.clone(),
            contracts: position.contracts,
            notional,
            slices,
            initial,
            maintenance,
        })
    }
}

/// The tier floors above 0. Rates never decrease from one tier to the
/// next ([`TieredLinearIm::new`] rejects such a table), so each margin is
/// convex in the notional.
impl PiecewiseLinearMargin for TieredLinearIm {
    fn cash_scale(&self) -> u32 {
        self.scale
    }

    fn notional_breakpoints(&self, _position: &ExposurePosition) -> Vec<Decimal> {
        self.tiers.iter().skip(1).map(|tier| tier.floor).collect()
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
    /// - [`MarginError::InvalidInput`] for a non positive multiplier or
    ///   mark, or two positions in the same instrument.
    /// - [`MarginError::Arithmetic`] if an amount cannot be represented.
    fn requirement(&self, view: &ExposureView) -> Result<MarginRequirement> {
        let mut seen = BTreeSet::new();
        let mut positions = Vec::with_capacity(view.positions.len());
        for position in &view.positions {
            if !seen.insert(&position.instrument) {
                return Err(MarginError::invalid_input(
                    "positions",
                    "at most one position per instrument",
                ));
            }
            positions.push(self.position(position)?);
        }
        // Totals: exact wide sums of the rounded positions.
        let initials: Vec<Decimal> = positions.iter().map(|p| p.initial).collect();
        let maintenances: Vec<Decimal> = positions.iter().map(|p| p.maintenance).collect();
        let initial = sum(&initials, "total initial")?;
        let maintenance = sum(&maintenances, "total maintenance")?;
        Ok(MarginRequirement {
            rule: self.id.clone(),
            account: view.account.clone(),
            venue: view.venue.clone(),
            currency: self.currency.clone(),
            initial,
            maintenance,
            positions,
        })
    }
}
