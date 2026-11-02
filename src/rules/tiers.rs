//! The tier arithmetic shared by the tiered rules.
//!
//! A position's notional is the exact ratio `N = product(numerators) /
//! product(denominators)`: `|Qc| * M * mark / 1` for a linear position,
//! `|Qc| * CV / mark` for an inverse one. With `k` the highest tier whose
//! floor is below `N` (compared exactly by cross products), each margin is
//!
//! ```text
//! sum over i < k of rate_i * (floor_{i+1} - floor_i) + rate_k * (N - floor_k)
//! ```
//!
//! computed as one exact sum over the common denominator and rounded up
//! once to the rule's scale. The reported notionals are informational.

use core::cmp::Ordering;

use rust_decimal::Decimal;

use crate::decimal::{
    MAX_SCALE, Rounding, check_configured_scale, compare_products, mul_div_round, neg, product,
    sub, sum_terms_div_round,
};
use std::collections::BTreeSet;

use crate::decimal::sum;
use crate::error::{MarginError, Result};
use crate::ids::Asset;
use crate::initial_margin::{
    ExposurePosition, ExposureView, MarginRequirement, PositionRequirement, RuleId, TierSlice,
};
use crate::rules::MarginTier;

/// Validates a tier table and a scale. See
/// [`TieredLinearIm::new`](crate::rules::TieredLinearIm::new).
pub(crate) fn validate(scale: u32, tiers: &[MarginTier]) -> Result<()> {
    check_configured_scale(scale, "scale")?;
    let invalid = |reason| Err(MarginError::invalid_input("tiers", reason));
    let Some(first) = tiers.first() else {
        return invalid("must not be empty");
    };
    if !first.floor.is_zero() {
        return invalid("the first floor must be 0");
    }
    for tier in tiers {
        if tier.maintenance_rate <= Decimal::ZERO
            || tier.maintenance_rate > tier.initial_rate
            || tier.initial_rate > Decimal::ONE
        {
            return invalid("rates must satisfy 0 < maintenance <= initial <= 1");
        }
    }
    for pair in tiers.windows(2) {
        let [lower, upper] = pair else {
            continue;
        };
        if upper.floor <= lower.floor {
            return invalid("floors must be strictly increasing");
        }
        if upper.initial_rate < lower.initial_rate
            || upper.maintenance_rate < lower.maintenance_rate
        {
            return invalid("rates must not decrease from one tier to the next");
        }
    }
    Ok(())
}

/// The margins of a position with notional `numerators / denominators`.
pub(crate) struct Tiered {
    /// The informational notional.
    pub(crate) notional: Option<Decimal>,
    /// The slices, one per tier the notional reaches.
    pub(crate) slices: Vec<TierSlice>,
    /// Initial margin, rounded up once.
    pub(crate) initial: Decimal,
    /// Maintenance margin, rounded up once.
    pub(crate) maintenance: Decimal,
}

/// The value of `numerators / denominators` minus `floor`: exact when there
/// is no denominator, else rounded half-even to [`MAX_SCALE`]; `None` when
/// it does not fit.
fn informational(
    numerators: &[Decimal],
    denominators: &[Decimal],
    floor: Decimal,
) -> Option<Decimal> {
    if denominators.is_empty() {
        let value = product(numerators, "notional").ok()?;
        return sub(value, floor, "notional - floor").ok();
    }
    if floor.is_zero() {
        return mul_div_round(
            numerators,
            denominators,
            MAX_SCALE,
            Rounding::HalfEven,
            "notional",
        )
        .ok();
    }
    let mut floor_term = vec![neg(floor)];
    floor_term.extend_from_slice(denominators);
    sum_terms_div_round(
        &[numerators, &floor_term],
        denominators,
        MAX_SCALE,
        Rounding::HalfEven,
        "notional - floor",
    )
    .ok()
}

/// The tiered margins of a position with notional `numerators /
/// denominators` (non negative), rounded up once to `scale`. See the
/// [module docs](self).
pub(crate) fn requirement(
    tiers: &[MarginTier],
    numerators: &[Decimal],
    denominators: &[Decimal],
    scale: u32,
) -> Result<Tiered> {
    // The tiers the notional reaches: floor < N, i.e. floor * den < num.
    let mut reached = 0_usize;
    for tier in tiers {
        let mut floor_side = vec![tier.floor];
        floor_side.extend_from_slice(denominators);
        if compare_products(numerators, &floor_side, "notional against a floor")?
            == Ordering::Greater
        {
            reached = reached.saturating_add(1);
        } else {
            break;
        }
    }
    let mut slices = Vec::with_capacity(reached);
    // Terms of margin * den, per rate: for i < k, rate_i * floor_{i+1} *
    // den - rate_i * floor_i * den; for k, rate_k * num - rate_k * floor_k
    // * den.
    let mut initial_terms: Vec<Vec<Decimal>> = Vec::new();
    let mut maintenance_terms: Vec<Vec<Decimal>> = Vec::new();
    let mut push = |rate: Decimal, maintenance: Decimal, factors: &[Decimal]| {
        let mut term = vec![rate];
        term.extend_from_slice(factors);
        initial_terms.push(term);
        let mut term = vec![maintenance];
        term.extend_from_slice(factors);
        maintenance_terms.push(term);
    };
    for (index, tier) in tiers.iter().enumerate().take(reached) {
        let lower = {
            let mut factors = vec![neg(tier.floor)];
            factors.extend_from_slice(denominators);
            factors
        };
        push(tier.initial_rate, tier.maintenance_rate, &lower);
        // A tier below the top one is full: floor_{i+1} - floor_i.
        let full = tiers
            .get(index.saturating_add(1))
            .filter(|_| index.saturating_add(1) < reached);
        let notional = if let Some(next) = full {
            let mut factors = vec![next.floor];
            factors.extend_from_slice(denominators);
            push(tier.initial_rate, tier.maintenance_rate, &factors);
            sub(next.floor, tier.floor, "slice").ok()
        } else {
            push(tier.initial_rate, tier.maintenance_rate, numerators);
            informational(numerators, denominators, tier.floor)
        };
        slices.push(TierSlice {
            tier: index,
            notional,
            initial_rate: tier.initial_rate,
            maintenance_rate: tier.maintenance_rate,
        });
    }
    let round_up = |terms: &[Vec<Decimal>], operation| {
        let terms: Vec<&[Decimal]> = terms.iter().map(Vec::as_slice).collect();
        sum_terms_div_round(
            &terms,
            denominators,
            scale,
            Rounding::AwayFromZero,
            operation,
        )
    };
    Ok(Tiered {
        notional: informational(numerators, denominators, Decimal::ZERO),
        slices,
        initial: round_up(&initial_terms, "initial margin")?,
        maintenance: round_up(&maintenance_terms, "maintenance margin")?,
    })
}

/// The requirement of `view`: one position at a time (at most one per
/// instrument), totals as exact sums of the rounded positions.
pub(crate) fn account_requirement(
    rule: &RuleId,
    currency: &Asset,
    view: &ExposureView,
    mut position: impl FnMut(&ExposurePosition) -> Result<PositionRequirement>,
) -> Result<MarginRequirement> {
    let mut seen = BTreeSet::new();
    let mut positions = Vec::with_capacity(view.positions.len());
    for exposure in &view.positions {
        if !seen.insert(&exposure.instrument) {
            return Err(MarginError::invalid_input(
                "positions",
                "at most one position per instrument",
            ));
        }
        positions.push(position(exposure)?);
    }
    let initials: Vec<Decimal> = positions.iter().map(|p| p.initial).collect();
    let maintenances: Vec<Decimal> = positions.iter().map(|p| p.maintenance).collect();
    Ok(MarginRequirement {
        rule: rule.clone(),
        account: view.account.clone(),
        venue: view.venue.clone(),
        currency: currency.clone(),
        initial: sum(&initials, "total initial")?,
        maintenance: sum(&maintenances, "total maintenance")?,
        positions,
    })
}
