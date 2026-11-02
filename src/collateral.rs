//! Collateral value of one account with explicit haircuts and a declared
//! stale price policy.
//!
//! [`value_collateral`] reads an immutable [`LedgerSnapshot`] and a
//! [`MarkSet`] (prices in the reporting asset, with source and timestamp)
//! and returns a [`CollateralValue`]. It never changes the ledger and never
//! touches realized cash: valuation stays separate from posting.
//!
//! # Rules
//!
//! For each asset, the policy's [`BalanceBasis`] selects the balance
//! (available by default, so cash already reserved for orders is not
//! counted twice).
//!
//! | Balance | Mark | Contribution |
//! |---|---|---|
//! | held (> 0), asset with a haircut `h` | valid | `balance * price * (1 - h)` |
//! | held, asset with a haircut | stale | per [`StalePolicy`]: nothing, `balance * price * (1 - h - extra)`, or an error |
//! | held, asset with a haircut | missing, non positive, future | nothing ([`StalePolicy::Reject`]: an error) |
//! | held, asset without a haircut | any | nothing: not eligible collateral |
//! | debt (< 0) | valid | `balance * price` in full (no haircut ever shrinks a debt) |
//! | debt | stale | per [`StalePolicy`]: unknown, `balance * price * (1 + extra)`, or an error |
//! | debt | missing, non positive, future | unknown ([`StalePolicy::Reject`]: an error) |
//!
//! The reporting asset is worth exactly 1, always valid; it is eligible
//! with its configured haircut, 0 if none.
//!
//! Each line is rounded down (toward negative infinity) once; the total is
//! the exact sum of the lines, `None` when any line is unknown. An invalid
//! mark therefore only ever lowers collateral or makes it unknown; it
//! cannot increase spendable collateral.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::decimal::{Rounding, add, neg, sub, sum, sum_terms_round};
use crate::error::{MarginError, Result};
use crate::ids::Asset;
use crate::ledger::LedgerSnapshot;
use crate::valuation::{Mark, MarkSet, MarkStatus, ValuationPolicy, pricing, validate};

/// Which balance counts as collateral.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BalanceBasis {
    /// Total minus reservations. The default: reserved cash is committed.
    #[default]
    Available,
    /// Total balance.
    Total,
}

/// What to do with a stale mark (older than the policy allows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StalePolicy {
    /// Held assets count nothing; a debt makes collateral unknown. The
    /// default.
    #[default]
    Exclude,
    /// Use the stale price with an extra haircut: held assets at
    /// `price * (1 - h - extra)`, debts at `price * (1 + extra)`.
    ExtraHaircut {
        /// Extra fraction, with `h + extra <= 1` for every eligible asset.
        extra: Decimal,
    },
    /// Fail with [`MarginError::InvalidCollateralMark`] on any invalid
    /// mark (stale, missing, non positive or future) that matters.
    Reject,
}

/// How to value collateral.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollateralPolicy {
    /// Asset every amount is expressed in.
    pub reporting: Asset,
    /// Maximum age of a valid mark, in milliseconds.
    pub max_age_ms: u64,
    /// Haircut per eligible asset, in `[0, 1]`. Assets absent from the
    /// map are not eligible (held balances count nothing); the reporting
    /// asset defaults to 0.
    pub haircuts: BTreeMap<Asset, Decimal>,
    /// Treatment of stale marks.
    pub stale: StalePolicy,
    /// Which balance counts.
    pub basis: BalanceBasis,
}

impl CollateralPolicy {
    /// A policy with no eligible asset other than the reporting asset,
    /// [`StalePolicy::Exclude`] and [`BalanceBasis::Available`].
    #[must_use]
    pub fn new(reporting: Asset, max_age_ms: u64) -> Self {
        Self {
            reporting,
            max_age_ms,
            haircuts: BTreeMap::new(),
            stale: StalePolicy::Exclude,
            basis: BalanceBasis::Available,
        }
    }

    /// Makes `asset` eligible with `haircut`.
    #[must_use]
    pub fn with_haircut(mut self, asset: Asset, haircut: Decimal) -> Self {
        self.haircuts.insert(asset, haircut);
        self
    }
}

/// Collateral contribution of one asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollateralLine {
    /// Balance on the policy's basis.
    pub balance: Decimal,
    /// Mark used, if any.
    pub mark: Option<Mark>,
    /// Status of the mark.
    pub status: MarkStatus,
    /// Haircut applied (`None` for a debt or an ineligible asset).
    pub haircut: Option<Decimal>,
    /// Contribution, rounded down; `None` when unknown.
    pub value: Option<Decimal>,
}

/// Result of [`value_collateral`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollateralValue {
    /// Reporting asset.
    pub reporting: Asset,
    /// Valuation time.
    pub as_of_ms: u64,
    /// One line per registered asset with a non zero balance.
    pub lines: BTreeMap<Asset, CollateralLine>,
    /// Assets with a non zero balance whose mark is not valid.
    pub flags: BTreeMap<Asset, MarkStatus>,
    /// Exact sum of the lines; `None` when a line is unknown.
    pub total: Option<Decimal>,
}

/// Values the collateral of the account in `snapshot`. See the
/// [module docs](self).
///
/// # Errors
///
/// - [`MarginError::UnknownAsset`] if the reporting asset or a priced or
///   haircut asset is not registered.
/// - [`MarginError::InvalidInput`] if the mark set prices the reporting
///   asset, a haircut is outside `[0, 1]`, or `haircut + extra` exceeds 1.
/// - [`MarginError::InvalidCollateralMark`] under [`StalePolicy::Reject`].
/// - [`MarginError::Arithmetic`] if a value cannot be represented.
#[expect(
    clippy::too_many_lines,
    reason = "one pass that validates every mark, haircut and conversion before it values anything; splitting it would separate the checks from the values they guard"
)]
pub fn value_collateral(
    snapshot: &LedgerSnapshot,
    marks: &MarkSet,
    policy: &CollateralPolicy,
) -> Result<CollateralValue> {
    let valuation_policy = ValuationPolicy::new(policy.reporting.clone(), policy.max_age_ms);
    let scale = validate(snapshot, marks, &valuation_policy)?;
    let extra = match policy.stale {
        StalePolicy::ExtraHaircut { extra } => {
            if extra < Decimal::ZERO || extra > Decimal::ONE {
                return Err(MarginError::invalid_input(
                    "extra",
                    "must be between 0 and 1",
                ));
            }
            Some(extra)
        }
        StalePolicy::Exclude | StalePolicy::Reject => None,
    };
    for (asset, haircut) in &policy.haircuts {
        if !snapshot.assets.contains_key(asset) {
            return Err(MarginError::UnknownAsset {
                asset: asset.clone(),
            });
        }
        if *haircut < Decimal::ZERO || *haircut > Decimal::ONE {
            return Err(MarginError::invalid_input(
                "haircut",
                "must be between 0 and 1",
            ));
        }
        if let Some(extra) = extra {
            if add(*haircut, extra, "haircut + extra")? > Decimal::ONE {
                return Err(MarginError::invalid_input(
                    "extra",
                    "haircut + extra must not exceed 1",
                ));
            }
        }
    }
    let pricing = pricing(snapshot, marks, &valuation_policy);

    let mut lines = BTreeMap::new();
    let mut flags = BTreeMap::new();
    let mut line_values = Vec::new();
    for (asset, balance) in &snapshot.balances {
        let amount = match policy.basis {
            BalanceBasis::Available => balance.available,
            BalanceBasis::Total => balance.total,
        };
        if amount.is_zero() {
            continue;
        }
        let status = pricing
            .status
            .get(asset)
            .copied()
            .unwrap_or(MarkStatus::Missing);
        if !status.is_valid() {
            flags.insert(asset.clone(), status);
        }
        let price = pricing.prices.get(asset).copied();
        let haircut = if *asset == policy.reporting {
            Some(policy.haircuts.get(asset).copied().unwrap_or_default())
        } else {
            policy.haircuts.get(asset).copied()
        };
        let debt = amount < Decimal::ZERO;
        let reject = || MarginError::InvalidCollateralMark {
            asset: asset.clone(),
        };

        // Factors of the contribution, or None for unknown, or an empty
        // vector for nothing.
        let factors: Option<Vec<Decimal>> = match (status, price, debt) {
            (MarkStatus::Valid, Some(p), true) => Some(vec![amount, p]),
            (MarkStatus::Valid, Some(p), false) => match haircut {
                Some(h) => Some(vec![amount, p, sub(Decimal::ONE, h, "1 - haircut")?]),
                None => Some(Vec::new()),
            },
            (MarkStatus::Stale { .. }, Some(p), _) => match policy.stale {
                StalePolicy::Reject if debt || haircut.is_some() => return Err(reject()),
                StalePolicy::ExtraHaircut { extra } if debt => {
                    Some(vec![amount, p, add(Decimal::ONE, extra, "1 + extra")?])
                }
                StalePolicy::ExtraHaircut { extra } => match haircut {
                    Some(h) => Some(vec![
                        amount,
                        p,
                        sum(&[Decimal::ONE, neg(h), neg(extra)], "1 - h - extra")?,
                    ]),
                    None => Some(Vec::new()),
                },
                _ if debt => None,
                _ => Some(Vec::new()),
            },
            // Missing, non positive or future marks.
            _ => {
                if matches!(policy.stale, StalePolicy::Reject) && (debt || haircut.is_some()) {
                    return Err(reject());
                }
                if debt { None } else { Some(Vec::new()) }
            }
        };
        let value = match &factors {
            None => None,
            Some(f) if f.is_empty() => Some(Decimal::ZERO),
            Some(f) => Some(floor(&[f.as_slice()], scale)?),
        };
        line_values.push(value);
        lines.insert(
            asset.clone(),
            CollateralLine {
                balance: amount,
                mark: marks.prices.get(asset).copied(),
                status,
                haircut: if debt { None } else { haircut },
                value,
            },
        );
    }

    Ok(CollateralValue {
        reporting: policy.reporting.clone(),
        as_of_ms: marks.as_of_ms,
        lines,
        flags,
        total: total_of(&line_values)?,
    })
}

/// Exact sum of the lines, independent of order (partial sums need not be
/// representable); `None` if any line is unknown.
fn total_of(lines: &[Option<Decimal>]) -> Result<Option<Decimal>> {
    let known: Option<Vec<Decimal>> = lines.iter().copied().collect();
    match known {
        Some(values) => Ok(Some(sum(&values, "collateral total")?)),
        None => Ok(None),
    }
}

/// `sum(product(term))` rounded once toward negative infinity.
fn floor(terms: &[&[Decimal]], scale: u32) -> Result<Decimal> {
    let away = sum_terms_round(terms, scale, Rounding::AwayFromZero, "collateral line")?;
    if away < Decimal::ZERO {
        Ok(away)
    } else {
        sum_terms_round(terms, scale, Rounding::TowardZero, "collateral line")
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
    use super::*;
    use rust_decimal_macros::dec;

    /// MAX + (-MAX) + x sums to x in any order; an unknown line makes the
    /// total unknown.
    #[test]
    fn total_is_an_exact_sum() {
        for lines in [
            [Some(Decimal::MAX), Some(Decimal::MIN), Some(dec!(12.5))],
            [Some(Decimal::MAX), Some(dec!(12.5)), Some(Decimal::MIN)],
            [Some(dec!(12.5)), Some(Decimal::MAX), Some(Decimal::MIN)],
        ] {
            assert_eq!(total_of(&lines), Ok(Some(dec!(12.5))));
        }
        assert_eq!(total_of(&[Some(dec!(1)), None]), Ok(None));
        assert_eq!(total_of(&[]), Ok(Some(Decimal::ZERO)));
    }
}
