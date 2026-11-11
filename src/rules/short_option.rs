//! Margin of short options, position by position.
//!
//! # Formula
//!
//! With signed contracts `Qc`, multiplier `M`, underlying price `U` and strike
//! `K`, both in the price asset, the amount out of the money is
//! `OTM = max(0, K - U)` for a call and `max(0, U - K)` for a put. A short
//! position requires, rounded up once to the rule's scale:
//!
//! ```text
//! linear option, in the settlement asset:
//!     im = |Qc| * M * max(a * U - OTM, b * U)
//!     mm = |Qc| * M * m * U
//! inverse option, in the underlying:
//!     im = |Qc| * M * max(a - OTM / U, b)
//!     mm = |Qc| * M * m
//! ```
//!
//! with `0 < b <= a <= 1` and `0 < m <= b` ([`ShortOptionParameters`]). An
//! inverse figure is the linear one divided by `U`. A long position requires
//! nothing: its premium was paid in full.
//!
//! # The mark is not in the requirement
//!
//! A short's premium mark is its cost to close. Collateral already deducts it
//! ([`crate::valuation`]), so this rule requires only the risk beyond it, and
//! adding the mark here would count it twice. A venue that states its short
//! option margin as "mark plus a percentage" states the same total when its
//! margin balance does not also deduct the mark.
//!
//! # Scope
//!
//! Each position stands alone: no netting between options or against the
//! underlying, which is left to portfolio rules. The rule prices nothing: the
//! underlying price comes from the caller. It takes its own view because an
//! [`ExposurePosition`](crate::initial_margin::ExposurePosition) carries neither
//! a strike nor an underlying price, and it returns the usual
//! [`MarginRequirement`], so [`crate::maintenance`] reads it like any other.
//!
//! The formula and the parameters of [`ShortOptionIm::short_option_v1`] are this
//! crate's own specification (`docs/options.md`), not a venue's rule. A venue's
//! rule gets its own identifier and cites its venue's text.

use std::collections::{BTreeMap, BTreeSet};

use core::cmp::Ordering;

use rust_decimal::Decimal;

use crate::decimal::{
    MAX_SCALE, Rounding, check_configured_scale, compare_products, mul_div_round, neg, sub, sum,
    sum_terms_div_round,
};
use crate::error::{MarginError, Result};
use crate::ids::{AccountId, Asset, InstrumentId, VenueId};
use crate::initial_margin::{MarginRequirement, PositionRequirement, RuleId};
use crate::instrument::{Instrument, OptionInstrument, OptionMargining};
use crate::ledger::LedgerSnapshot;

/// Rates of the short option formula. See the [module docs](self#formula).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortOptionParameters {
    /// `a`: the initial rate on the underlying, before the amount out of the
    /// money is deducted.
    pub initial_rate: Decimal,
    /// `b`: the lowest initial rate on the underlying, however far out of the
    /// money.
    pub floor_rate: Decimal,
    /// `m`: the maintenance rate on the underlying.
    pub maintenance_rate: Decimal,
}

/// One option position as the rule sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionExposure {
    /// The option.
    pub option: OptionInstrument,
    /// Signed contracts: positive long, negative short.
    pub contracts: Decimal,
    /// Price of the underlying in the price asset: the same unit as the
    /// strike.
    pub underlying_price: Decimal,
}

/// The option positions of one account at one venue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionExposureView {
    /// Account.
    pub account: AccountId,
    /// Venue.
    pub venue: VenueId,
    /// Positions, at most one per instrument.
    pub positions: Vec<OptionExposure>,
}

impl OptionExposureView {
    /// The open options of a ledger settled in `currency`, with the underlying
    /// prices by option instrument.
    ///
    /// # Errors
    ///
    /// [`MarginError::MissingMark`] if an open option has no underlying price;
    /// it is never margined as if it had none.
    pub fn from_snapshot(
        snapshot: &LedgerSnapshot,
        account: AccountId,
        venue: VenueId,
        currency: &Asset,
        underlying_prices: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<Self> {
        let mut positions = Vec::new();
        for (id, position) in &snapshot.positions {
            let Some(Instrument::Option(option)) = snapshot.instruments.get(id) else {
                continue;
            };
            if position.quantity.is_zero() || option.terms().settlement() != currency {
                continue;
            }
            let underlying_price =
                *underlying_prices
                    .get(id)
                    .ok_or_else(|| MarginError::MissingMark {
                        instrument: id.clone(),
                    })?;
            positions.push(OptionExposure {
                option: option.clone(),
                contracts: position.quantity,
                underlying_price,
            });
        }
        Ok(Self {
            account,
            venue,
            positions,
        })
    }
}

/// Short option margin rule. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortOptionIm {
    id: RuleId,
    currency: Asset,
    scale: u32,
    parameters: ShortOptionParameters,
}

impl ShortOptionIm {
    /// The rule `short-option-im/1`: `a` 15 %, `b` 10 %, `m` 7.5 %. The rates
    /// are this crate's own, not a venue's. `currency` is the settlement asset
    /// and `scale` its scale.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if `scale` exceeds
    /// [`MAX_SCALE`].
    ///
    /// # Examples
    ///
    /// ```
    /// use margin_rs::Asset;
    /// use margin_rs::rules::ShortOptionIm;
    ///
    /// let rule = ShortOptionIm::short_option_v1(Asset::new("USDC")?, 6)?;
    /// assert_eq!(rule.rule().to_string(), "short-option-im/1");
    /// # Ok::<(), margin_rs::MarginError>(())
    /// ```
    pub fn short_option_v1(currency: Asset, scale: u32) -> Result<Self> {
        Self::new(
            RuleId::new("short-option-im", 1),
            currency,
            scale,
            ShortOptionParameters {
                initial_rate: Decimal::new(15, 2),
                floor_rate: Decimal::new(10, 2),
                maintenance_rate: Decimal::new(75, 3),
            },
        )
    }

    /// A rule with explicit rates. Use a distinct [`RuleId`] for every set:
    /// the identifier is what audits key on.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] unless `0 < b <= a <= 1` and
    /// `0 < m <= b`, or if `scale` exceeds
    /// [`MAX_SCALE`].
    pub fn new(
        id: RuleId,
        currency: Asset,
        scale: u32,
        parameters: ShortOptionParameters,
    ) -> Result<Self> {
        check_configured_scale(scale, "scale")?;
        let ShortOptionParameters {
            initial_rate,
            floor_rate,
            maintenance_rate,
        } = parameters;
        if floor_rate <= Decimal::ZERO || floor_rate > initial_rate || initial_rate > Decimal::ONE {
            return Err(MarginError::invalid_input(
                "parameters",
                "needs 0 < floor_rate <= initial_rate <= 1",
            ));
        }
        if maintenance_rate <= Decimal::ZERO || maintenance_rate > floor_rate {
            return Err(MarginError::invalid_input(
                "parameters",
                "needs 0 < maintenance_rate <= floor_rate",
            ));
        }
        Ok(Self {
            id,
            currency,
            scale,
            parameters,
        })
    }

    /// Name and version of the rule.
    #[must_use]
    pub fn rule(&self) -> &RuleId {
        &self.id
    }

    /// Settlement currency of every amount.
    #[must_use]
    pub fn currency(&self) -> &Asset {
        &self.currency
    }

    /// Scale every amount is rounded up to.
    #[must_use]
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// The rates.
    #[must_use]
    pub fn parameters(&self) -> ShortOptionParameters {
        self.parameters
    }

    /// Margin required by `view`: the exact sums of its positions.
    ///
    /// # Errors
    ///
    /// - [`MarginError::CurrencyMismatch`] if an option settles in another
    ///   asset than the rule's currency.
    /// - [`MarginError::InvalidInput`] for an underlying price that is not
    ///   positive or finer than [`MAX_SCALE`], or
    ///   two positions in the same instrument.
    /// - [`MarginError::Arithmetic`] if an amount cannot be represented; it is
    ///   never reported as a smaller requirement.
    pub fn requirement(&self, view: &OptionExposureView) -> Result<MarginRequirement> {
        let mut seen = BTreeSet::new();
        let mut positions = Vec::with_capacity(view.positions.len());
        for exposure in &view.positions {
            if !seen.insert(exposure.option.terms().id()) {
                return Err(MarginError::invalid_input(
                    "positions",
                    "at most one position per instrument",
                ));
            }
            positions.push(self.position(exposure)?);
        }
        let initials: Vec<Decimal> = positions.iter().map(|p| p.initial).collect();
        let maintenances: Vec<Decimal> = positions.iter().map(|p| p.maintenance).collect();
        Ok(MarginRequirement {
            rule: self.id.clone(),
            account: view.account.clone(),
            venue: view.venue.clone(),
            currency: self.currency.clone(),
            initial: sum(&initials, "total initial")?,
            maintenance: sum(&maintenances, "total maintenance")?,
            positions,
        })
    }

    fn position(&self, exposure: &OptionExposure) -> Result<PositionRequirement> {
        let terms = exposure.option.terms();
        if *terms.settlement() != self.currency {
            return Err(MarginError::CurrencyMismatch {
                field: "settlement",
                expected: self.currency.clone(),
                found: terms.settlement().clone(),
            });
        }
        let underlying = exposure.underlying_price;
        if underlying <= Decimal::ZERO {
            return Err(MarginError::invalid_input(
                "underlying_price",
                "must be positive",
            ));
        }
        crate::decimal::check_scale(underlying, MAX_SCALE, "underlying_price")?;
        let contracts = exposure.contracts.abs();
        let multiplier = terms.multiplier();
        let inverse = exposure.option.contract().margining == OptionMargining::Inverse;
        let notional = if inverse {
            mul_div_round(
                &[contracts, multiplier],
                &[],
                18,
                Rounding::HalfEven,
                "contracts * multiplier",
            )
            .ok()
        } else {
            mul_div_round(
                &[contracts, multiplier, underlying],
                &[],
                18,
                Rounding::HalfEven,
                "contracts * multiplier * underlying",
            )
            .ok()
        };
        let (initial, maintenance) = if exposure.contracts < Decimal::ZERO {
            self.short_margins(exposure, contracts, multiplier, inverse)?
        } else {
            (Decimal::ZERO, Decimal::ZERO)
        };
        Ok(PositionRequirement {
            instrument: terms.id().clone(),
            contracts: exposure.contracts,
            notional,
            slices: Vec::new(),
            initial,
            maintenance,
        })
    }

    /// Both margins of a short, each one exact sum over the common denominator
    /// `U` (inverse) rounded up once.
    fn short_margins(
        &self,
        exposure: &OptionExposure,
        contracts: Decimal,
        multiplier: Decimal,
        inverse: bool,
    ) -> Result<(Decimal, Decimal)> {
        let underlying = exposure.underlying_price;
        let ShortOptionParameters {
            initial_rate,
            floor_rate,
            maintenance_rate,
        } = self.parameters;
        let out_of_the_money = exposure.option.out_of_the_money(underlying)?;
        // Which branch of the max: `a * U - OTM >= b * U`, exactly.
        let spread = sub(initial_rate, floor_rate, "initial rate - floor rate")?;
        let above_floor = compare_products(
            &[spread, underlying],
            &[out_of_the_money],
            "short option floor",
        )? != Ordering::Less;
        let denominators: &[Decimal] = if inverse { &[underlying] } else { &[] };
        let negative_otm = neg(out_of_the_money);
        let rated = [contracts, multiplier, initial_rate, underlying];
        let deducted = [contracts, multiplier, negative_otm];
        let floor = [contracts, multiplier, floor_rate, underlying];
        let initial_terms: Vec<&[Decimal]> = if above_floor {
            vec![&rated, &deducted]
        } else {
            vec![&floor]
        };
        let initial = sum_terms_div_round(
            &initial_terms,
            denominators,
            self.scale,
            Rounding::AwayFromZero,
            "short option initial margin",
        )?;
        let maintenance = [contracts, multiplier, maintenance_rate, underlying];
        let maintenance = sum_terms_div_round(
            &[&maintenance],
            denominators,
            self.scale,
            Rounding::AwayFromZero,
            "short option maintenance margin",
        )?;
        Ok((initial, maintenance))
    }
}
