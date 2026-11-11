//! A per-position rule for linear derivatives together with
//! [`ShortOptionIm`] for options, as one model.
//!
//! [`LinearWithOptionsIm`] margins every option position of an
//! [`ExposureView`] under its [`ShortOptionIm`] and every other position
//! under its linear rule, and adds the two requirements: no netting
//! between positions. An [`ExposurePosition`] carries neither a strike
//! nor an underlying price, so the model holds, for each option
//! instrument, its definition and the underlying price of the current
//! evaluation.
//!
//! It is an [`AccountMargin`], so
//! [`plan_liquidation`](crate::liquidation::plan_liquidation) closes an
//! account of linear and option positions under it: linear positions keep
//! the exact tiered search (each option's margin is constant while a
//! linear position shrinks), option positions are searched count by count.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::decimal::sum;
use crate::error::{MarginError, Result};
use crate::ids::InstrumentId;
use crate::initial_margin::{
    AccountMargin, ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement,
    PiecewiseLinearMargin, ProductKind, RuleId,
};
use crate::instrument::OptionInstrument;
use crate::rules::{MarginTier, OptionExposure, OptionExposureView, ShortOptionIm};

/// A linear rule and [`ShortOptionIm`] as one model. See the
/// [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearWithOptionsIm<L> {
    id: RuleId,
    linear: L,
    options: ShortOptionIm,
    terms: BTreeMap<InstrumentId, (OptionInstrument, Decimal)>,
}

impl<L: PiecewiseLinearMargin> LinearWithOptionsIm<L> {
    /// The model with identifier `id`, margining options under `options`
    /// with, for each option instrument, its definition and the price of
    /// its underlying.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if the two rules round to different
    /// scales or margin different currencies, or `options_terms` repeats an
    /// instrument.
    pub fn new(
        id: RuleId,
        linear: L,
        options: ShortOptionIm,
        options_terms: Vec<(OptionInstrument, Decimal)>,
    ) -> Result<Self> {
        if linear.cash_scale() != options.scale() {
            return Err(MarginError::invalid_input(
                "options",
                "both rules must round to the same scale",
            ));
        }
        let mut terms = BTreeMap::new();
        for (option, underlying_price) in options_terms {
            let id = option.terms().id().clone();
            if terms.insert(id, (option, underlying_price)).is_some() {
                return Err(MarginError::invalid_input(
                    "options_terms",
                    "must not repeat an instrument",
                ));
            }
        }
        Ok(Self {
            id,
            linear,
            options,
            terms,
        })
    }
}

impl<L: PiecewiseLinearMargin> InitialMarginModel for LinearWithOptionsIm<L> {
    fn rule(&self) -> &RuleId {
        &self.id
    }

    /// The sum of the linear requirement of the other positions and the
    /// short option requirement of the options, with one line per position
    /// in view order.
    ///
    /// # Errors
    ///
    /// - [`MarginError::MissingMark`] for an option without terms.
    /// - [`MarginError::InvalidInput`] if the two requirements are in
    ///   different currencies.
    /// - The errors of either rule.
    fn requirement(&self, view: &ExposureView) -> Result<MarginRequirement> {
        let (options, linear): (Vec<&ExposurePosition>, Vec<&ExposurePosition>) = view
            .positions
            .iter()
            .partition(|p| p.product == ProductKind::Option);
        let linear = self.linear.requirement(&ExposureView {
            account: view.account.clone(),
            venue: view.venue.clone(),
            positions: linear.into_iter().cloned().collect(),
        })?;
        let mut exposures = Vec::with_capacity(options.len());
        for position in options {
            let (option, underlying_price) =
                self.terms
                    .get(&position.instrument)
                    .ok_or_else(|| MarginError::MissingMark {
                        instrument: position.instrument.clone(),
                    })?;
            exposures.push(OptionExposure {
                option: option.clone(),
                contracts: position.contracts,
                underlying_price: *underlying_price,
            });
        }
        let options = self.options.requirement(&OptionExposureView {
            account: view.account.clone(),
            venue: view.venue.clone(),
            positions: exposures,
        })?;
        if options.currency != linear.currency {
            return Err(MarginError::invalid_input(
                "options",
                "both rules must margin the same currency",
            ));
        }
        let mut lines: BTreeMap<InstrumentId, _> = linear
            .positions
            .into_iter()
            .chain(options.positions)
            .map(|line| (line.instrument.clone(), line))
            .collect();
        let positions: Vec<_> = view
            .positions
            .iter()
            .filter_map(|p| lines.remove(&p.instrument))
            .collect();
        Ok(MarginRequirement {
            rule: self.id.clone(),
            account: view.account.clone(),
            venue: view.venue.clone(),
            currency: linear.currency,
            initial: sum(&[linear.initial, options.initial], "total initial")?,
            maintenance: sum(
                &[linear.maintenance, options.maintenance],
                "total maintenance",
            )?,
            positions,
        })
    }
}

impl<L: PiecewiseLinearMargin> AccountMargin for LinearWithOptionsIm<L> {
    fn plan_cash_scale(&self) -> u32 {
        self.linear.cash_scale()
    }

    /// The linear rule's tiers for a linear position, none for an option.
    fn plan_tiers(&self, position: &ExposurePosition) -> Option<Vec<MarginTier>> {
        (position.product != ProductKind::Option).then(|| self.linear.margin_tiers(position))
    }
}
