//! Exact decimal arithmetic and explicit rounding.
//!
//! `rust_decimal` operations can lose digits without reporting it: a product
//! or a sum whose mantissa does not fit in 96 bits is rescaled and rounded,
//! and the `checked_*` variants only return `None` when no rescaling helps.
//! The helpers here reject any result that is not exact, so the only places
//! where a value is rounded are two explicit rounding helpers, each called
//! with a [`Rounding`] mode and a target scale.

use rust_decimal::{Decimal, RoundingStrategy};

use crate::error::{MarginError, Result};

/// Largest scale (digits after the decimal point) accepted for an asset,
/// a price or a quantity.
///
/// `Decimal` carries at most 28 fractional digits; capping configured scales
/// at 18 keeps the product of a price and a quantity exactly representable
/// in common cases and leaves headroom for intermediate results.
pub const MAX_SCALE: u32 = 18;

/// Rounding mode applied when a value must be brought to a fixed scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Rounding {
    /// Round half to even (banker's rounding). The default: it has no
    /// systematic bias over many operations.
    #[default]
    HalfEven,
    /// Round half away from zero.
    HalfAwayFromZero,
    /// Truncate toward zero.
    TowardZero,
    /// Round away from zero.
    AwayFromZero,
}

impl Rounding {
    const fn strategy(self) -> RoundingStrategy {
        match self {
            Self::HalfEven => RoundingStrategy::MidpointNearestEven,
            Self::HalfAwayFromZero => RoundingStrategy::MidpointAwayFromZero,
            Self::TowardZero => RoundingStrategy::ToZero,
            Self::AwayFromZero => RoundingStrategy::AwayFromZero,
        }
    }
}

/// Exact sum. Fails if the result overflows or would be rounded.
pub(crate) fn add(a: Decimal, b: Decimal, operation: &'static str) -> Result<Decimal> {
    let sum = a.checked_add(b).ok_or(MarginError::arithmetic(operation))?;
    if is_exact_sum(a, b, sum) {
        Ok(sum)
    } else {
        Err(MarginError::arithmetic(operation))
    }
}

/// Exact difference. Fails if the result overflows or would be rounded.
pub(crate) fn sub(a: Decimal, b: Decimal, operation: &'static str) -> Result<Decimal> {
    add(a, -b, operation)
}

/// True if `sum` is exactly `a + b`.
///
/// An exact sum needs at most `max(scale(a), scale(b))` fractional digits,
/// so a result at that scale or finer is exact. `rust_decimal` may return a
/// coarser scale either because it rounded or because the value is zero or
/// an operand was zero; the inverse subtraction tells the cases apart, since
/// it can only give back `b` when nothing was lost.
fn is_exact_sum(a: Decimal, b: Decimal, sum: Decimal) -> bool {
    sum.scale() >= a.scale().max(b.scale()) || sum.checked_sub(a) == Some(b)
}

/// Exact product. Fails if the result overflows or would be rounded.
///
/// Trailing zeros of the factors are dropped first, so they do not count
/// against the 28 digit fractional limit.
pub(crate) fn mul(a: Decimal, b: Decimal, operation: &'static str) -> Result<Decimal> {
    if a.is_zero() || b.is_zero() {
        return Ok(Decimal::ZERO);
    }
    let a = a.normalize();
    let b = b.normalize();
    let product = a.checked_mul(b).ok_or(MarginError::arithmetic(operation))?;
    if product.scale() != a.scale() + b.scale() {
        return Err(MarginError::arithmetic(operation));
    }
    Ok(product)
}

/// Rounds `value` to `scale` digits with `rounding`.
pub(crate) fn round(value: Decimal, scale: u32, rounding: Rounding) -> Decimal {
    value.round_dp_with_strategy(scale, rounding.strategy())
}

/// Computes `numerator / denominator` rounded to `scale` digits.
///
/// The quotient is first computed by `rust_decimal` to 28 significant digits
/// and then rounded to `scale`. Fails on division by zero or overflow.
pub(crate) fn div_round(
    numerator: Decimal,
    denominator: Decimal,
    scale: u32,
    rounding: Rounding,
    operation: &'static str,
) -> Result<Decimal> {
    let quotient = numerator
        .checked_div(denominator)
        .ok_or(MarginError::arithmetic(operation))?;
    Ok(round(quotient, scale, rounding))
}

/// Number of significant fractional digits of `value` (trailing zeros do
/// not count).
pub(crate) fn significant_scale(value: Decimal) -> u32 {
    value.normalize().scale()
}

/// Fails with [`MarginError::PrecisionExceeded`] if `value` has more
/// significant fractional digits than `max_scale`.
pub(crate) fn check_scale(value: Decimal, max_scale: u32, field: &'static str) -> Result<()> {
    let scale = significant_scale(value);
    if scale > max_scale {
        return Err(MarginError::PrecisionExceeded {
            field,
            scale,
            max_scale,
        });
    }
    Ok(())
}

/// Fails with [`MarginError::InvalidInput`] unless `scale <= MAX_SCALE`.
pub(crate) fn check_configured_scale(scale: u32, field: &'static str) -> Result<()> {
    if scale > MAX_SCALE {
        return Err(MarginError::invalid_input(
            field,
            "scale must be at most 18",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn exact_operations_keep_every_digit() {
        assert_eq!(add(dec!(1.5), dec!(0.25), "t"), Ok(dec!(1.75)));
        assert_eq!(sub(dec!(1.5), dec!(0.25), "t"), Ok(dec!(1.25)));
        assert_eq!(mul(dec!(1.50), dec!(2.0), "t"), Ok(dec!(3.000)));
    }

    #[test]
    fn product_that_rust_decimal_would_round_is_rejected() {
        // rust_decimal returns 118842243771396506390315925.50 here, silently
        // dropping the last digits of the exact 118842243771396506390315925.5025.
        let a = dec!(79228162514264337593543950.335);
        assert_eq!(
            mul(a, dec!(1.5), "big"),
            Err(MarginError::arithmetic("big"))
        );
        // More than 28 fractional digits cannot be represented.
        assert_eq!(
            mul(dec!(0.1234567890123456), dec!(0.1234567890123456), "tiny"),
            Err(MarginError::arithmetic("tiny"))
        );
    }

    #[test]
    fn overflow_is_an_error_not_a_saturation() {
        assert_eq!(
            add(Decimal::MAX, dec!(1), "max + 1"),
            Err(MarginError::arithmetic("max + 1"))
        );
        assert_eq!(
            sub(Decimal::MIN, dec!(1), "min - 1"),
            Err(MarginError::arithmetic("min - 1"))
        );
        assert_eq!(
            mul(Decimal::MAX, dec!(2), "max * 2"),
            Err(MarginError::arithmetic("max * 2"))
        );
    }

    #[test]
    fn sum_that_loses_fractional_digits_is_rejected() {
        assert_eq!(
            add(Decimal::MAX, dec!(0.1), "max + 0.1"),
            Err(MarginError::arithmetic("max + 0.1"))
        );
    }

    #[test]
    fn zero_results_and_operands_are_exact() {
        assert_eq!(sub(dec!(0.00000000), Decimal::ZERO, "t"), Ok(Decimal::ZERO));
        assert_eq!(
            add(dec!(1.23456789), dec!(-1.23456789), "t"),
            Ok(Decimal::ZERO)
        );
        assert_eq!(add(Decimal::ZERO, dec!(0.000001), "t"), Ok(dec!(0.000001)));
        assert_eq!(mul(Decimal::ZERO, dec!(1.5), "t"), Ok(Decimal::ZERO));
    }

    #[test]
    fn rounding_modes() {
        assert_eq!(round(dec!(2.5), 0, Rounding::HalfEven), dec!(2));
        assert_eq!(round(dec!(3.5), 0, Rounding::HalfEven), dec!(4));
        assert_eq!(round(dec!(2.5), 0, Rounding::HalfAwayFromZero), dec!(3));
        assert_eq!(round(dec!(-2.5), 0, Rounding::HalfAwayFromZero), dec!(-3));
        assert_eq!(round(dec!(2.9), 0, Rounding::TowardZero), dec!(2));
        assert_eq!(round(dec!(2.1), 0, Rounding::AwayFromZero), dec!(3));
    }

    #[test]
    fn rounded_division() {
        assert_eq!(
            div_round(dec!(100), dec!(3), 6, Rounding::HalfEven, "t"),
            Ok(dec!(33.333333))
        );
        assert_eq!(
            div_round(dec!(1), Decimal::ZERO, 6, Rounding::HalfEven, "1 / 0"),
            Err(MarginError::arithmetic("1 / 0"))
        );
    }

    #[test]
    fn scale_checks_ignore_trailing_zeros() {
        assert_eq!(check_scale(dec!(1.50), 1, "x"), Ok(()));
        assert_eq!(
            check_scale(dec!(1.25), 1, "x"),
            Err(MarginError::PrecisionExceeded {
                field: "x",
                scale: 2,
                max_scale: 1,
            })
        );
        assert!(check_configured_scale(18, "s").is_ok());
        assert!(check_configured_scale(19, "s").is_err());
    }
}
