//! Exact decimal arithmetic and explicit rounding.
//!
//! `rust_decimal` operations can lose digits without reporting it: a sum or
//! a product whose exact value needs a mantissa wider than 96 bits or more
//! than 28 fractional digits is rescaled and rounded, and the `checked_*`
//! variants only return `None` when no rescaling helps.
//!
//! This module never relies on those operations. It works on the integer
//! mantissas in 768 bit integers, so every intermediate is exact:
//!
//! - `add`, `sub` and `mul` return the exact result, or an error when that
//!   result cannot be represented as a [`Decimal`];
//! - `mul_div_round` computes a product of decimals divided by another
//!   product, and `sum_products_round` a sum of products, each rounded
//!   **once** from the exact rational value to the target scale with an
//!   explicit [`Rounding`] mode.
//!
//! So a result that can be represented at the target scale is always
//! produced, and one that cannot is an error that leaves the caller's state
//! untouched.

mod wide;

use rust_decimal::Decimal;

use crate::error::{MarginError, Result};
use wide::{U768, Wide};

/// Largest scale (digits after the decimal point) accepted for an asset,
/// a price or a quantity.
///
/// `Decimal` carries at most 28 fractional digits. Every intermediate is
/// computed exactly in wide integers, so this cap is not needed for
/// correctness; it keeps configured precisions within what real assets use
/// (18 for most ERC-20 tokens) and leaves room for integer digits.
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

/// Exact sum. Fails if the exact result cannot be represented.
pub(crate) fn add(a: Decimal, b: Decimal, operation: &'static str) -> Result<Decimal> {
    let (wa, sa) = Wide::from_decimal(a);
    let (wb, sb) = Wide::from_decimal(b);
    let scale = sa.max(sb);
    wa.scale_up(scale - sa)
        .zip(wb.scale_up(scale - sb))
        .and_then(|(x, y)| x.checked_add(y))
        .and_then(|sum| sum.to_decimal(scale))
        .ok_or(MarginError::arithmetic(operation))
}

/// Exact difference. Fails if the exact result cannot be represented.
pub(crate) fn sub(a: Decimal, b: Decimal, operation: &'static str) -> Result<Decimal> {
    // Negation is exact for every Decimal.
    add(a, -b, operation)
}

/// Exact product. Fails if the exact result cannot be represented (more
/// than 28 fractional digits after removing trailing zeros, or a mantissa
/// wider than 96 bits).
pub(crate) fn mul(a: Decimal, b: Decimal, operation: &'static str) -> Result<Decimal> {
    let (wa, sa) = Wide::from_decimal(a);
    let (wb, sb) = Wide::from_decimal(b);
    wa.checked_mul(wb)
        .and_then(|product| product.to_decimal(sa + sb))
        .ok_or(MarginError::arithmetic(operation))
}

/// `product(numerators) / product(denominators)`, computed exactly and
/// rounded once to `scale` digits. An empty slice is the product 1.
///
/// Fails on a zero denominator or if the rounded result cannot be
/// represented.
pub(crate) fn mul_div_round(
    numerators: &[Decimal],
    denominators: &[Decimal],
    scale: u32,
    rounding: Rounding,
    operation: &'static str,
) -> Result<Decimal> {
    let error = || MarginError::arithmetic(operation);
    // With n_i = m_i / 10^s_i and d_j = e_j / 10^t_j, the value is
    // (prod m_i * 10^sum t_j) / (prod e_j * 10^sum s_i).
    let (numerator, numerator_scale) = wide_product(numerators).ok_or_else(error)?;
    let (denominator, denominator_scale) = wide_product(denominators).ok_or_else(error)?;
    let numerator = numerator.scale_up(denominator_scale).ok_or_else(error)?;
    let denominator = denominator.scale_up(numerator_scale).ok_or_else(error)?;
    round_ratio(numerator, denominator, scale, rounding).ok_or_else(error)
}

/// `sum(a_i * b_i)`, computed exactly and rounded once to `scale` digits.
///
/// Fails if the rounded result cannot be represented.
pub(crate) fn sum_products_round(
    terms: &[(Decimal, Decimal)],
    scale: u32,
    rounding: Rounding,
    operation: &'static str,
) -> Result<Decimal> {
    let terms: Vec<[Decimal; 2]> = terms.iter().map(|(a, b)| [*a, *b]).collect();
    let terms: Vec<&[Decimal]> = terms.iter().map(|t| t.as_slice()).collect();
    sum_terms_round(&terms, scale, rounding, operation)
}

/// `sum(product(term_i))` over terms of any length, computed exactly and
/// rounded once to `scale` digits. An empty term is the product 1.
///
/// Fails if the rounded result cannot be represented.
pub(crate) fn sum_terms_round(
    terms: &[&[Decimal]],
    scale: u32,
    rounding: Rounding,
    operation: &'static str,
) -> Result<Decimal> {
    let error = || MarginError::arithmetic(operation);
    let mut products = Vec::with_capacity(terms.len());
    for term in terms {
        products.push(wide_product(term).ok_or_else(error)?);
    }
    let common = products.iter().map(|(_, s)| *s).max().unwrap_or(0);
    let mut sum = Wide::ZERO;
    for (product, product_scale) in products {
        let aligned = product.scale_up(common - product_scale).ok_or_else(error)?;
        sum = sum.checked_add(aligned).ok_or_else(error)?;
    }
    let denominator = Wide::new(false, U768::pow10(common).ok_or_else(error)?);
    round_ratio(sum, denominator, scale, rounding).ok_or_else(error)
}

/// Product of the mantissas and sum of the scales.
fn wide_product(values: &[Decimal]) -> Option<(Wide, u32)> {
    let mut product = Wide::new(false, U768::from_u128(1));
    let mut scale = 0_u32;
    for value in values {
        let (mantissa, value_scale) = Wide::from_decimal(*value);
        product = product.checked_mul(mantissa)?;
        scale += value_scale;
    }
    Some((product, scale))
}

/// The decimal nearest to `numerator / denominator` at `scale` digits under
/// `rounding`, from the exact quotient and remainder.
fn round_ratio(
    numerator: Wide,
    denominator: Wide,
    scale: u32,
    rounding: Rounding,
) -> Option<Decimal> {
    let scaled = numerator.magnitude.checked_mul(U768::pow10(scale)?)?;
    let (quotient, remainder) = scaled.div_rem(denominator.magnitude)?;
    let quotient = if remainder.is_zero() {
        quotient
    } else {
        let twice = remainder.checked_add(remainder)?;
        let up = match rounding {
            Rounding::TowardZero => false,
            Rounding::AwayFromZero => true,
            Rounding::HalfAwayFromZero => twice >= denominator.magnitude,
            Rounding::HalfEven => match twice.cmp(&denominator.magnitude) {
                core::cmp::Ordering::Greater => true,
                core::cmp::Ordering::Less => false,
                core::cmp::Ordering::Equal => quotient.is_odd(),
            },
        };
        if up {
            quotient.checked_add(U768::from_u128(1))?
        } else {
            quotient
        }
    };
    Wide::new(numerator.negative != denominator.negative, quotient).to_decimal(scale)
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
        let r = |v: Decimal, mode| mul_div_round(&[v], &[], 0, mode, "t");
        assert_eq!(r(dec!(2.5), Rounding::HalfEven), Ok(dec!(2)));
        assert_eq!(r(dec!(3.5), Rounding::HalfEven), Ok(dec!(4)));
        assert_eq!(r(dec!(2.5), Rounding::HalfAwayFromZero), Ok(dec!(3)));
        assert_eq!(r(dec!(-2.5), Rounding::HalfAwayFromZero), Ok(dec!(-3)));
        assert_eq!(r(dec!(2.9), Rounding::TowardZero), Ok(dec!(2)));
        assert_eq!(r(dec!(-2.9), Rounding::TowardZero), Ok(dec!(-2)));
        assert_eq!(r(dec!(2.1), Rounding::AwayFromZero), Ok(dec!(3)));
        assert_eq!(r(dec!(-2.1), Rounding::AwayFromZero), Ok(dec!(-3)));
    }

    /// M104-1: a sum that rust_decimal rounds (and whose inverse
    /// subtraction rounds back) is rejected, in both orders and signs.
    #[test]
    fn sums_rounded_by_rust_decimal_are_rejected_in_both_orders_and_signs() {
        for (a, b) in [
            (dec!(0.1), Decimal::MAX),
            (Decimal::MAX, dec!(0.1)),
            (dec!(-0.1), Decimal::MIN),
            (Decimal::MIN, dec!(-0.1)),
            (dec!(7922816251426433759354395033.5), dec!(0.01)),
        ] {
            assert_eq!(
                add(a, b, "t"),
                Err(MarginError::arithmetic("t")),
                "{a} + {b}"
            );
        }
        assert_eq!(
            sub(Decimal::MAX, dec!(-0.1), "t"),
            Err(MarginError::arithmetic("t"))
        );
        assert_eq!(
            sub(Decimal::MIN, dec!(0.1), "t"),
            Err(MarginError::arithmetic("t"))
        );
        // Representable results near the limits are still exact.
        assert_eq!(add(Decimal::MAX, dec!(-1), "t"), Ok(Decimal::MAX - dec!(1)));
        assert!(add(dec!(0.5), Decimal::MAX - dec!(1), "t").is_err());
        assert_eq!(
            add(dec!(7922816251426433759354395033.5), dec!(0.5), "t"),
            Ok(dec!(7922816251426433759354395034))
        );
    }

    /// Products that only fit after dropping trailing zeros are exact.
    #[test]
    fn products_with_wide_scales_are_exact_when_representable() {
        // 18 + 18 fractional digits, but the exact product has 20.
        assert_eq!(
            mul(dec!(0.100000000000000000), dec!(0.000000000000000002), "t"),
            Ok(dec!(0.0000000000000000002))
        );
        // A 96 bit mantissa times 10 at scale 1 stays exact.
        assert_eq!(
            mul(Decimal::MAX, dec!(0.1), "t"),
            Ok(dec!(7922816251426433759354395033.5))
        );
    }

    /// One rounding from the exact rational value, every mode and sign.
    #[test]
    fn mul_div_round_rounds_once_in_every_mode() {
        let cases = [
            // (numerator, denominator, scale, half_even, half_away, toward, away)
            (dec!(3), dec!(2), 0, dec!(2), dec!(2), dec!(1), dec!(2)),
            (dec!(5), dec!(2), 0, dec!(2), dec!(3), dec!(2), dec!(3)),
            (
                dec!(100),
                dec!(3),
                6,
                dec!(33.333333),
                dec!(33.333333),
                dec!(33.333333),
                dec!(33.333334),
            ),
            (
                dec!(200),
                dec!(3),
                6,
                dec!(66.666667),
                dec!(66.666667),
                dec!(66.666666),
                dec!(66.666667),
            ),
            (
                dec!(66.666667),
                dec!(2),
                6,
                dec!(33.333334),
                dec!(33.333334),
                dec!(33.333333),
                dec!(33.333334),
            ),
        ];
        for (n, d, scale, half_even, half_away, toward, away) in cases {
            for (mode, expected) in [
                (Rounding::HalfEven, half_even),
                (Rounding::HalfAwayFromZero, half_away),
                (Rounding::TowardZero, toward),
                (Rounding::AwayFromZero, away),
            ] {
                assert_eq!(
                    mul_div_round(&[n], &[d], scale, mode, "t"),
                    Ok(expected),
                    "{n}/{d} {mode:?}"
                );
                assert_eq!(
                    mul_div_round(&[-n], &[d], scale, mode, "t"),
                    Ok(-expected),
                    "-{n}/{d} {mode:?}"
                );
                assert_eq!(
                    mul_div_round(&[n], &[-d], scale, mode, "t"),
                    Ok(-expected),
                    "{n}/-{d} {mode:?}"
                );
            }
        }
        assert_eq!(
            mul_div_round(&[dec!(1)], &[Decimal::ZERO], 6, Rounding::HalfEven, "1 / 0"),
            Err(MarginError::arithmetic("1 / 0"))
        );
    }

    /// M104-2: 3 * 1 / 2000000000000000001 at 18 digits is strictly below
    /// 1.5e-18, so half even gives 1e-18 (rounding the 28 digit quotient
    /// first would give 2e-18). Neighbours of the exact half are checked
    /// too.
    #[test]
    fn released_cost_is_not_double_rounded() {
        let held = dec!(2000000000000000001);
        assert_eq!(
            mul_div_round(&[dec!(3), dec!(1)], &[held], 18, Rounding::HalfEven, "t"),
            Ok(dec!(0.000000000000000001))
        );
        // Exactly 1.5e-18: 3 / 2e18.
        let half = dec!(2000000000000000000);
        assert_eq!(
            mul_div_round(&[dec!(3)], &[half], 18, Rounding::HalfEven, "t"),
            Ok(dec!(0.000000000000000002))
        );
        assert_eq!(
            mul_div_round(&[dec!(3)], &[half], 18, Rounding::TowardZero, "t"),
            Ok(dec!(0.000000000000000001))
        );
        // Just above the half: 3 / (2e18 - 1).
        assert_eq!(
            mul_div_round(
                &[dec!(3)],
                &[dec!(1999999999999999999)],
                18,
                Rounding::HalfEven,
                "t"
            ),
            Ok(dec!(0.000000000000000002))
        );
        assert_eq!(
            mul_div_round(&[dec!(3)], &[held], 18, Rounding::HalfAwayFromZero, "t"),
            Ok(dec!(0.000000000000000001))
        );
    }

    /// Products of 18 digit scales round once to the target scale.
    #[test]
    fn products_round_once_to_the_target_scale() {
        // 3000.12 * 0.123456789012345678 = 370.38518185171851548136, DAI at 18.
        assert_eq!(
            mul_div_round(
                &[dec!(3000.12), dec!(0.123456789012345678)],
                &[],
                18,
                Rounding::HalfEven,
                "t"
            ),
            Ok(dec!(370.385181851718515481))
        );
        // 1.123456789012345678 * 3000.123456789012345678 has 36 fractional
        // digits; the rounded value at 18 is representable.
        assert_eq!(
            mul_div_round(
                &[dec!(1.123456789012345678), dec!(3000.123456789012345678)],
                &[],
                18,
                Rounding::HalfEven,
                "t"
            ),
            Ok(dec!(3370.509065404802618515))
        );
    }

    #[test]
    fn exact_sum_of_products() {
        // M104-3: MAX + 0.1 - MAX at rate 1 is 0.1, not 0.
        assert_eq!(
            sum_products_round(
                &[
                    (Decimal::MAX, dec!(1)),
                    (dec!(0.1), dec!(1)),
                    (Decimal::MIN, dec!(1))
                ],
                6,
                Rounding::HalfEven,
                "t"
            ),
            Ok(dec!(0.1))
        );
        assert_eq!(
            sum_products_round(
                &[(dec!(-0.01), dec!(300)), (dec!(-0.001), dec!(100))],
                6,
                Rounding::HalfEven,
                "t"
            ),
            Ok(dec!(-3.1))
        );
        assert_eq!(
            sum_products_round(&[], 6, Rounding::HalfEven, "t"),
            Ok(Decimal::ZERO)
        );
        assert_eq!(
            sum_products_round(&[(Decimal::MAX, dec!(2))], 0, Rounding::HalfEven, "t"),
            Err(MarginError::arithmetic("t"))
        );
    }

    /// Wide results agree with i128 arithmetic where i128 suffices.
    #[test]
    fn mul_div_round_matches_i128_reference() {
        let mut state = 0x0123_4567_89AB_CDEF_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            // Integers below 2^40 with scale 0..=6, result at scale 0..=6.
            let a = i128::from(next() >> 24) * if next() % 2 == 0 { 1 } else { -1 };
            let b = i128::from(next() >> 24);
            let c = i128::from(next() >> 24).max(1);
            let (sa, sb, sc, scale) = (
                (next() % 7) as u32,
                (next() % 7) as u32,
                (next() % 7) as u32,
                (next() % 7) as u32,
            );
            let da = Decimal::from_i128_with_scale(a, sa);
            let db = Decimal::from_i128_with_scale(b, sb);
            let dc = Decimal::from_i128_with_scale(c, sc);
            // Reference: q = round_half_even(a * b * 10^(sc + scale) / (c * 10^(sa + sb))).
            let num = a * b * 10_i128.pow(sc + scale);
            let den = c * 10_i128.pow(sa + sb);
            let (q, r) = (num / den, num % den);
            let twice = 2 * r.abs();
            let q = if twice > den || (twice == den && q % 2 != 0) {
                q + num.signum()
            } else {
                q
            };
            assert_eq!(
                mul_div_round(&[da, db], &[dc], scale, Rounding::HalfEven, "t"),
                Ok(Decimal::from_i128_with_scale(q, scale)),
                "{da} * {db} / {dc} at {scale}"
            );
            assert_eq!(add(da, db, "t"), Ok(da + db));
        }
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
