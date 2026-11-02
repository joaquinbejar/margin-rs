//! Contract counts on a step grid, indexed in wide integers.
//!
//! A [`ContractGrid`] is the set `{0, step, 2 step, ...}` capped at `cap`:
//! index `k` is `min(k * step, cap)` contracts, and the last index is
//! `ceil(cap / step)`, so the last count may be less than a full step from
//! the previous one. Indices are [`U768`]s, so a tiny step on a huge
//! position (more indices than a `Decimal` can count) and the products and
//! midpoints of a bisection never overflow; only counts, which are at most
//! `cap`, become `Decimal`s.
//!
//! Near the limits of `Decimal` a fractional count may have too many
//! digits to be represented (for example `k * 0.5` close to
//! `Decimal::MAX`). Such a count is rounded down to the scale of the cap,
//! which keeps counts non decreasing in `k` and the remainder of an integer
//! position representable.

use rust_decimal::Decimal;

use super::wide::{U768, Wide};
use super::wide_product;
use crate::error::{MarginError, Result};

/// Index into a [`ContractGrid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GridIndex(U768);

impl GridIndex {
    /// Index 0: no contracts.
    pub(crate) const ZERO: Self = Self(U768::ZERO);
}

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub(crate) struct ContractGrid {
    step: U768,
    step_scale: u32,
    cap: Decimal,
    last: U768,
}

fn magnitude(value: Decimal) -> U768 {
    U768::from_u128(value.mantissa().unsigned_abs())
}

impl ContractGrid {
    /// The grid of `step` capped at `cap`.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] unless `step > 0` and `cap >= 0`.
    pub(crate) fn new(step: Decimal, cap: Decimal) -> Result<Self> {
        if step <= Decimal::ZERO || cap < Decimal::ZERO {
            return Err(MarginError::invalid_input(
                "contract grid",
                "step must be positive and the cap non negative",
            ));
        }
        let error = || MarginError::arithmetic("contract grid");
        // cap / step = (mc / 10^sc) / (ms / 10^ss) = mc * 10^ss / (ms * 10^sc).
        let numerator = magnitude(cap)
            .checked_mul(U768::pow10(step.scale()).ok_or_else(error)?)
            .ok_or_else(error)?;
        let denominator = magnitude(step)
            .checked_mul(U768::pow10(cap.scale()).ok_or_else(error)?)
            .ok_or_else(error)?;
        let (quotient, remainder) = numerator.div_rem(denominator).ok_or_else(error)?;
        let last = if remainder.is_zero() {
            quotient
        } else {
            quotient.checked_add(U768::from_u128(1)).ok_or_else(error)?
        };
        Ok(Self {
            step: magnitude(step),
            step_scale: step.scale(),
            cap,
            last,
        })
    }

    /// The last index: the whole cap.
    pub(crate) fn last(&self) -> GridIndex {
        GridIndex(self.last)
    }

    /// `min(k * step, cap)` contracts, rounded down to the scale of the
    /// cap when the exact count cannot be represented (see the
    /// [module docs](self)).
    ///
    /// # Errors
    ///
    /// [`MarginError::Arithmetic`] only if even the rounded count cannot be
    /// represented.
    pub(crate) fn contracts(&self, index: GridIndex) -> Result<Decimal> {
        if index.0 >= self.last {
            return Ok(self.cap);
        }
        let error = || MarginError::arithmetic("k * step");
        let product = index.0.checked_mul(self.step).ok_or_else(error)?;
        if let Some(exact) = Wide::new(false, product).to_decimal(self.step_scale) {
            return Ok(exact);
        }
        let scale = self.cap.scale();
        if scale >= self.step_scale {
            return Err(error());
        }
        let (truncated, _) = product
            .div_rem(
                U768::pow10(self.step_scale.checked_sub(scale).ok_or_else(error)?)
                    .ok_or_else(error)?,
            )
            .ok_or_else(error)?;
        Wide::new(false, truncated)
            .to_decimal(scale)
            .ok_or_else(error)
    }

    /// `floor(numerator / (product(denominators) * step))`, clamped to the
    /// grid, where `numerator` is the exact sum of the products of
    /// `numerator_terms`; index 0 for a non positive numerator. The
    /// numerator stays in wide integers: it need not fit a `Decimal`, in
    /// magnitude or in scale, as long as the index does.
    ///
    /// # Errors
    ///
    /// [`MarginError::Arithmetic`] on a zero denominator or an overflow of
    /// the wide integers.
    pub(crate) fn floor_index_of(
        &self,
        numerator_terms: &[&[Decimal]],
        denominators: &[Decimal],
    ) -> Result<GridIndex> {
        let error = || MarginError::arithmetic("grid index");
        let mut products = Vec::with_capacity(numerator_terms.len());
        for term in numerator_terms {
            products.push(wide_product(term).ok_or_else(error)?);
        }
        let numerator_scale = products.iter().map(|(_, s)| *s).max().unwrap_or(0);
        let mut numerator = Wide::ZERO;
        for (product, scale) in products {
            let aligned = product.align(scale, numerator_scale).ok_or_else(error)?;
            numerator = numerator.checked_add(aligned).ok_or_else(error)?;
        }
        if numerator.negative || numerator.magnitude.is_zero() {
            return Ok(GridIndex::ZERO);
        }
        let (denominator, denominator_scale) = wide_product(denominators).ok_or_else(error)?;
        // value / step = (mn / 10^sn) / ((md / 10^sd) * (ms / 10^ss))
        //              = mn * 10^(sd + ss) / (md * ms * 10^sn).
        let top = numerator
            .magnitude
            .checked_mul(
                U768::pow10(
                    denominator_scale
                        .checked_add(self.step_scale)
                        .ok_or_else(error)?,
                )
                .ok_or_else(error)?,
            )
            .ok_or_else(error)?;
        let bottom = denominator
            .magnitude
            .checked_mul(self.step)
            .and_then(|d| d.checked_mul(U768::pow10(numerator_scale)?))
            .ok_or_else(error)?;
        let (quotient, _) = top.div_rem(bottom).ok_or_else(error)?;
        Ok(GridIndex(quotient.min(self.last)))
    }

    /// `index + 1`, if it is still on the grid.
    pub(crate) fn next(&self, index: GridIndex) -> Option<GridIndex> {
        let next = index.0.checked_add(U768::from_u128(1))?;
        (next <= self.last).then_some(GridIndex(next))
    }

    /// True when no index lies strictly between `low` and `high`.
    pub(crate) fn adjacent(low: GridIndex, high: GridIndex) -> bool {
        match high.0.checked_sub(low.0) {
            Some(gap) => gap <= U768::from_u128(1),
            None => true,
        }
    }

    /// `low + (high - low) / 2`, for `low <= high`.
    pub(crate) fn midpoint(low: GridIndex, high: GridIndex) -> GridIndex {
        let gap = high.0.checked_sub(low.0).unwrap_or(U768::ZERO);
        let half = gap
            .div_rem(U768::from_u128(2))
            .map_or(U768::ZERO, |(q, _)| q);
        // low + half <= high, which is on the grid, so this cannot overflow.
        GridIndex(low.0.checked_add(half).unwrap_or(high.0))
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

    #[test]
    fn counts_and_last_step() {
        let grid = ContractGrid::new(dec!(2), dec!(7)).expect("grid");
        let counts: Vec<Decimal> = (0..=5)
            .map(|k| {
                grid.contracts(GridIndex(U768::from_u128(k)))
                    .expect("count")
            })
            .collect();
        assert_eq!(
            counts,
            vec![dec!(0), dec!(2), dec!(4), dec!(6), dec!(7), dec!(7)]
        );
        assert_eq!(grid.last(), GridIndex(U768::from_u128(4)));
    }

    /// `Decimal::MAX` with step 2: ceil(MAX / 2) * 2 exceeds MAX, but the last
    /// count is the cap.
    #[test]
    fn huge_cap_and_tiny_step_do_not_overflow() {
        let grid = ContractGrid::new(dec!(2), Decimal::MAX).expect("grid");
        assert_eq!(grid.contracts(grid.last()).expect("count"), Decimal::MAX);
        // MAX / 0.5 has more indices than a Decimal can count.
        let grid = ContractGrid::new(dec!(0.5), Decimal::MAX).expect("grid");
        let middle = ContractGrid::midpoint(GridIndex::ZERO, grid.last());
        let count = grid.contracts(middle).expect("count");
        assert_eq!(count.scale(), 0);
        assert!(count >= Decimal::MAX / dec!(2) - Decimal::ONE);
        assert!(count <= Decimal::MAX / dec!(2) + Decimal::ONE);
        assert_eq!(grid.next(grid.last()), None);
        assert!(ContractGrid::adjacent(grid.last(), grid.last()));
    }

    #[test]
    fn floor_index_of_a_ratio() {
        let grid = ContractGrid::new(dec!(0.5), dec!(10)).expect("grid");
        // 7.3 / (1 * 0.5) = 14.6 -> 14.
        let index = grid
            .floor_index_of(&[&[dec!(7.3)]], &[dec!(1)])
            .expect("index");
        assert_eq!(grid.contracts(index).expect("count"), dec!(7));
        // Beyond the cap: clamped to the last index.
        let index = grid
            .floor_index_of(&[&[dec!(100)]], &[dec!(1)])
            .expect("index");
        assert_eq!(index, grid.last());
        assert_eq!(
            grid.floor_index_of(&[&[dec!(-1)]], &[dec!(1)])
                .expect("index"),
            GridIndex::ZERO
        );
    }

    /// The numerator is a sum of products that need not fit a Decimal:
    /// `MAX * MAX - MAX` over `MAX` is `MAX - 1`, and a scale of 32
    /// (`1e-16 * 1e-16`) over a scale of 32 is fine.
    #[test]
    fn floor_index_of_a_wide_numerator() {
        let grid = ContractGrid::new(dec!(1), Decimal::MAX).expect("grid");
        let index = grid
            .floor_index_of(
                &[&[Decimal::MAX, Decimal::MAX], &[-Decimal::MAX]],
                &[Decimal::MAX],
            )
            .expect("index");
        assert_eq!(
            grid.contracts(index).expect("count"),
            Decimal::MAX - Decimal::ONE
        );
        let tiny = Decimal::new(1, 16);
        let grid = ContractGrid::new(dec!(1), dec!(10)).expect("grid");
        // (3 * 1e-16 * 1e-16) / 1e-32 = 3.
        let index = grid
            .floor_index_of(&[&[dec!(3), tiny, tiny]], &[tiny, tiny])
            .expect("index");
        assert_eq!(grid.contracts(index).expect("count"), dec!(3));
    }
}
