//! Validated factor covariance and portfolio variance.
//!
//! # Covariance
//!
//! A [`ValidatedCovariance`] holds the covariance of factor moves over one
//! horizon, in each factor's unit (for example the variance of daily price
//! returns). It can only be built from a matrix that passes validation:
//!
//! - one row and column per factor, factor identifiers unique;
//! - exactly symmetric;
//! - positive semidefinite, checked exactly: every diagonal entry is
//!   non negative and, for two factors, `c11 * c22 - c12^2 >= 0`.
//!
//! **Regularization decision:** a matrix that fails is rejected with
//! [`MarginError::InvalidCovariance`]; it is never repaired (no eigenvalue
//! clipping, no shrinkage). Estimation and any repair belong to the caller,
//! who then passes a valid matrix.
//!
//! This version supports one and two factors, where positive
//! semidefiniteness has the exact closed form above. Larger matrices are
//! rejected until an exact check for them is added.
//!
//! # Variance
//!
//! [`portfolio_variance`] computes `e' C e` for the net exposure vector `e`
//! (exposures to the same factor are summed: offsetting positions offset).
//! The sum is exact and rounded up once; since `C` is positive
//! semidefinite the exact value is never negative, so the result is never
//! negative either. Units: currency squared per horizon.
//!
//! This is not the market-maker-rs vega squared proxy: a vega exposure is
//! only accepted against a [`FactorKind::VolatilityPoint`] factor with a
//! covariance in volatility points, never converted into price variance.
//! Scenario revaluation is a separate interface ([`crate::scenarios`]).

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;

use crate::decimal::{Rounding, compare_products, product, sum, sum_terms_round};
use crate::error::{MarginError, Result};
use crate::factors::{Factor, FactorExposure, FactorKind};
use crate::ids::{Asset, FactorId};

/// A covariance matrix that passed validation. See the
/// [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCovariance {
    factors: Vec<Factor>,
    matrix: Vec<Vec<Decimal>>,
}

impl ValidatedCovariance {
    /// Validates `matrix` (row `i`, column `j` is the covariance of factors
    /// `i` and `j`).
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidCovariance`] if there are no factors or more
    /// than two, factor identifiers repeat, the matrix is not square with
    /// one row per factor, it is not exactly symmetric, or it is not
    /// positive semidefinite, or if its determinant cannot be evaluated.
    /// Validation never fails with another error.
    pub fn new(factors: Vec<Factor>, matrix: Vec<Vec<Decimal>>) -> Result<Self> {
        let invalid = |reason| Err(MarginError::InvalidCovariance { reason });
        let n = factors.len();
        if n == 0 {
            return invalid("at least one factor is required");
        }
        if n > 2 {
            return invalid("only one and two factor matrices are supported");
        }
        let ids: BTreeSet<&FactorId> = factors.iter().map(|f| &f.id).collect();
        if ids.len() != n {
            return invalid("factor identifiers must be unique");
        }
        if matrix.len() != n || matrix.iter().any(|row| row.len() != n) {
            return invalid("the matrix must have one row and one column per factor");
        }
        for (i, row) in matrix.iter().enumerate() {
            for (j, value) in row.iter().enumerate() {
                if *value != matrix[j][i] {
                    return invalid("the matrix must be exactly symmetric");
                }
            }
        }
        for (i, row) in matrix.iter().enumerate() {
            if row[i] < Decimal::ZERO {
                return invalid("variances must not be negative");
            }
        }
        if n == 2 {
            // det >= 0 <=> c11 * c22 >= c12 * c21, compared exactly: the
            // products need not be representable as Decimals.
            let order = compare_products(
                &[matrix[0][0], matrix[1][1]],
                &[matrix[0][1], matrix[1][0]],
                "determinant",
            )
            .map_err(|_| MarginError::InvalidCovariance {
                reason: "the determinant cannot be evaluated",
            })?;
            if order == core::cmp::Ordering::Less {
                return invalid("the matrix must be positive semidefinite");
            }
        }
        Ok(Self { factors, matrix })
    }

    /// Builds the matrix from standard deviations and a correlation
    /// matrix: `c_ij = rho_ij * sigma_i * sigma_j`.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidCovariance`] if a deviation is negative, a
    /// correlation is outside `[-1, 1]`, the diagonal of the correlation
    /// matrix is not 1, a covariance cannot be represented, or the result
    /// fails [`ValidatedCovariance::new`].
    pub fn from_correlation(
        factors: Vec<Factor>,
        deviations: Vec<Decimal>,
        correlation: Vec<Vec<Decimal>>,
    ) -> Result<Self> {
        let invalid = |reason| Err(MarginError::InvalidCovariance { reason });
        let n = factors.len();
        if deviations.len() != n
            || correlation.len() != n
            || correlation.iter().any(|row| row.len() != n)
        {
            return invalid("deviations and correlations must match the factors");
        }
        if deviations.iter().any(|d| *d < Decimal::ZERO) {
            return invalid("standard deviations must not be negative");
        }
        for (i, row) in correlation.iter().enumerate() {
            if row[i] != Decimal::ONE {
                return invalid("correlations of a factor with itself must be 1");
            }
            if row.iter().any(|r| r.abs() > Decimal::ONE) {
                return invalid("correlations must be in [-1, 1]");
            }
        }
        let mut matrix = vec![vec![Decimal::ZERO; n]; n];
        for i in 0..n {
            for j in 0..n {
                // One exact product: only the covariance itself must be
                // representable.
                matrix[i][j] = product(
                    &[correlation[i][j], deviations[i], deviations[j]],
                    "rho * sigma_i * sigma_j",
                )
                .map_err(|_| MarginError::InvalidCovariance {
                    reason: "a covariance rho * sigma_i * sigma_j cannot be represented",
                })?;
            }
        }
        Self::new(factors, matrix)
    }

    /// Factors, in matrix order.
    #[must_use]
    pub fn factors(&self) -> &[Factor] {
        &self.factors
    }

    /// Covariance of factors `i` and `j`.
    #[must_use]
    pub fn get(&self, i: usize, j: usize) -> Option<Decimal> {
        self.matrix.get(i).and_then(|row| row.get(j)).copied()
    }
}

/// Result of [`portfolio_variance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortfolioVariance {
    /// Currency of the exposures; the variance is in this currency squared.
    pub currency: Asset,
    /// Net exposure per factor, in matrix order.
    pub net_exposures: BTreeMap<FactorId, Decimal>,
    /// `e' C e`, rounded up once to `scale` digits. Never negative.
    pub variance: Decimal,
}

/// Portfolio variance `e' C e` of `exposures` under `covariance`, rounded
/// up to `scale` fractional digits. See the [module docs](self).
///
/// # Errors
///
/// - [`MarginError::InvalidInput`] if there are no exposures.
/// - [`MarginError::UnknownFactor`] if an exposure's factor is not in the
///   matrix.
/// - [`MarginError::FactorUnitMismatch`] if an exposure's kind differs from
///   its factor's (for example vega against a price factor).
/// - [`MarginError::CurrencyMismatch`] if exposures are in different
///   currencies.
/// - [`MarginError::Arithmetic`] if a value cannot be represented.
pub fn portfolio_variance(
    exposures: &[FactorExposure],
    covariance: &ValidatedCovariance,
    scale: u32,
) -> Result<PortfolioVariance> {
    let Some(first) = exposures.first() else {
        return Err(MarginError::invalid_input(
            "exposures",
            "at least one exposure is required",
        ));
    };
    let currency = first.currency.clone();
    let index: BTreeMap<&FactorId, (usize, FactorKind)> = covariance
        .factors
        .iter()
        .enumerate()
        .map(|(i, f)| (&f.id, (i, f.kind)))
        .collect();
    // Factor index of every exposure, after validation.
    let mut slots = Vec::with_capacity(exposures.len());
    for exposure in exposures {
        if exposure.currency != currency {
            return Err(MarginError::CurrencyMismatch {
                field: "exposure currency",
                expected: currency,
                found: exposure.currency.clone(),
            });
        }
        let (i, kind) = *index
            .get(&exposure.factor)
            .ok_or_else(|| MarginError::UnknownFactor {
                factor: exposure.factor.clone(),
            })?;
        if exposure.kind != kind {
            return Err(MarginError::FactorUnitMismatch {
                factor: exposure.factor.clone(),
                expected: kind.unit(),
                found: exposure.kind.unit(),
            });
        }
        slots.push(i);
    }

    // e' C e expanded over the exposures themselves:
    // sum over pairs (a, b) of e_a * e_b * C[f(a)][f(b)], exact and
    // independent of the order, so partial net exposures never need to be
    // representable. Rounded up once.
    let mut terms: Vec<[Decimal; 3]> = Vec::with_capacity(exposures.len() * exposures.len());
    for (a, ea) in exposures.iter().enumerate() {
        for (b, eb) in exposures.iter().enumerate() {
            terms.push([ea.amount, eb.amount, covariance.matrix[slots[a]][slots[b]]]);
        }
    }
    let slices: Vec<&[Decimal]> = terms.iter().map(|t| t.as_slice()).collect();
    let variance = sum_terms_round(&slices, scale, Rounding::AwayFromZero, "e' C e")?;

    // Net exposure per factor, summed exactly.
    let mut net = Vec::with_capacity(covariance.factors.len());
    for i in 0..covariance.factors.len() {
        let amounts: Vec<Decimal> = exposures
            .iter()
            .zip(&slots)
            .filter(|(_, slot)| **slot == i)
            .map(|(e, _)| e.amount)
            .collect();
        net.push(sum(&amounts, "net exposure")?);
    }
    if variance < Decimal::ZERO {
        // Unreachable for a validated matrix; reported rather than hidden.
        return Err(MarginError::InvalidCovariance {
            reason: "negative variance from a validated matrix",
        });
    }
    Ok(PortfolioVariance {
        currency,
        net_exposures: covariance
            .factors
            .iter()
            .zip(net)
            .map(|(f, e)| (f.id.clone(), e))
            .collect(),
        variance,
    })
}
