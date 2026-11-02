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

pub(crate) mod grid;
mod wide;

use rust_decimal::Decimal;

use crate::error::{MarginError, Result};
use wide::U768;
pub(crate) use wide::Wide;

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

/// `-x`, exactly: the sign flipped, as `Decimal`'s own negation does. It
/// never overflows and does no arithmetic.
#[must_use]
pub(crate) fn neg(x: Decimal) -> Decimal {
    let mut negated = x;
    negated.set_sign_negative(x.is_sign_positive());
    negated
}

/// Exact sum. Fails if the exact result cannot be represented.
pub(crate) fn add(a: Decimal, b: Decimal, operation: &'static str) -> Result<Decimal> {
    combine(a, b, false, operation)
}

/// Exact `a + b`, or `a - b` with `negate_b`.
fn combine(a: Decimal, b: Decimal, negate_b: bool, operation: &'static str) -> Result<Decimal> {
    let (wa, sa) = Wide::from_decimal(a);
    let (wb, sb) = Wide::from_decimal(b);
    let wb = if negate_b { wb.negated() } else { wb };
    let scale = sa.max(sb);
    wa.align(sa, scale)
        .zip(wb.align(sb, scale))
        .and_then(|(x, y)| x.checked_add(y))
        .and_then(|sum| sum.to_decimal(scale))
        .ok_or(MarginError::arithmetic(operation))
}

/// Exact difference. Fails if the exact result cannot be represented.
pub(crate) fn sub(a: Decimal, b: Decimal, operation: &'static str) -> Result<Decimal> {
    // Negation is exact in wide integers.
    combine(a, b, true, operation)
}

/// Exact product. Fails if the exact result cannot be represented (more
/// than 28 fractional digits after removing trailing zeros, or a mantissa
/// wider than 96 bits).
pub(crate) fn mul(a: Decimal, b: Decimal, operation: &'static str) -> Result<Decimal> {
    product(&[a, b], operation)
}

/// Exact product of any number of factors. Only the complete product must
/// be representable: intermediate products are kept in wide integers, so a
/// partial product that overflows or needs more than 28 fractional digits
/// does not fail on its own.
pub(crate) fn product(values: &[Decimal], operation: &'static str) -> Result<Decimal> {
    wide_product(values)
        .and_then(|(product, scale)| product.to_decimal(scale))
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
    let terms: Vec<&[Decimal]> = terms
        .iter()
        .map(<[rust_decimal::Decimal; 2]>::as_slice)
        .collect();
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
        let aligned = product.align(product_scale, common).ok_or_else(error)?;
        sum = sum.checked_add(aligned).ok_or_else(error)?;
    }
    let denominator = Wide::new(false, U768::pow10(common).ok_or_else(error)?);
    round_ratio(sum, denominator, scale, rounding).ok_or_else(error)
}

/// Exact sum of any number of values. Partial sums are kept in wide
/// integers, so only the final sum must be representable, whatever the
/// order of the values.
pub(crate) fn sum(values: &[Decimal], operation: &'static str) -> Result<Decimal> {
    let error = || MarginError::arithmetic(operation);
    let common = values.iter().map(Decimal::scale).max().unwrap_or(0);
    let mut total = Wide::ZERO;
    for value in values {
        let (wide, scale) = Wide::from_decimal(*value);
        total = total
            .checked_add(wide.align(scale, common).ok_or_else(error)?)
            .ok_or_else(error)?;
    }
    total.to_decimal(common).ok_or_else(error)
}

/// The sign of the exact sum of `values`, without converting the sum to a
/// `Decimal`: it need not be representable.
pub(crate) fn sum_sign(values: &[Decimal], operation: &'static str) -> Result<core::cmp::Ordering> {
    let error = || MarginError::arithmetic(operation);
    let common = values.iter().map(Decimal::scale).max().unwrap_or(0);
    let mut total = Wide::ZERO;
    for value in values {
        let (wide, scale) = Wide::from_decimal(*value);
        total = total
            .checked_add(wide.align(scale, common).ok_or_else(error)?)
            .ok_or_else(error)?;
    }
    Ok(total.cmp_signed(&Wide::ZERO))
}

/// Exact `sum(product(term))`, without rounding: intermediate products and
/// partial sums stay in wide integers and only the result must be
/// representable.
pub(crate) fn sum_products(terms: &[&[Decimal]], operation: &'static str) -> Result<Decimal> {
    let error = || MarginError::arithmetic(operation);
    let mut products = Vec::with_capacity(terms.len());
    for term in terms {
        products.push(wide_product(term).ok_or_else(error)?);
    }
    let common = products.iter().map(|(_, s)| *s).max().unwrap_or(0);
    let mut total = Wide::ZERO;
    for (value, scale) in products {
        total = total
            .checked_add(value.align(scale, common).ok_or_else(error)?)
            .ok_or_else(error)?;
    }
    total.to_decimal(common).ok_or_else(error)
}

/// Quadratic form `e' C e` where `e_i` is the exact sum of `groups[i]`,
/// rounded once to `scale`. The sums stay in wide integers until the final
/// rounding, so neither a partial sum nor a net `e_i` needs to be
/// representable; only the result must be. Also returns each net as a
/// `Decimal` when it is representable.
///
/// `matrix` must be `groups.len()` square.
pub(crate) fn quadratic_form_round(
    groups: &[Vec<Decimal>],
    matrix: &[Vec<Decimal>],
    scale: u32,
    rounding: Rounding,
    operation: &'static str,
) -> Result<(Decimal, Vec<Option<Decimal>>)> {
    let error = || MarginError::arithmetic(operation);
    // Net of each group in wide integers, at the group's largest scale.
    let mut nets = Vec::with_capacity(groups.len());
    for values in groups {
        let common = values.iter().map(Decimal::scale).max().unwrap_or(0);
        let mut total = Wide::ZERO;
        for value in values {
            let (wide, s) = Wide::from_decimal(*value);
            total = total
                .checked_add(wide.align(s, common).ok_or_else(error)?)
                .ok_or_else(error)?;
        }
        nets.push((total, common));
    }
    // sum_ij net_i * net_j * c_ij, aligned to a common scale.
    let mut terms = Vec::new();
    for (i, (ni, si)) in nets.iter().enumerate() {
        for (j, (nj, sj)) in nets.iter().enumerate() {
            let entry = matrix.get(i).and_then(|row| row.get(j)).ok_or_else(error)?;
            let (c, sc) = Wide::from_decimal(*entry);
            let value = ni
                .checked_mul(*nj)
                .and_then(|v| v.checked_mul(c))
                .ok_or_else(error)?;
            let term_scale = si
                .checked_add(*sj)
                .and_then(|s| s.checked_add(sc))
                .ok_or_else(error)?;
            terms.push((value, term_scale));
        }
    }
    let common = terms.iter().map(|(_, s)| *s).max().unwrap_or(0);
    let mut total = Wide::ZERO;
    for (value, s) in terms {
        total = total
            .checked_add(value.align(s, common).ok_or_else(error)?)
            .ok_or_else(error)?;
    }
    let denominator = Wide::new(false, U768::pow10(common).ok_or_else(error)?);
    let result = round_ratio(total, denominator, scale, rounding).ok_or_else(error)?;
    let nets = nets.into_iter().map(|(n, s)| n.to_decimal(s)).collect();
    Ok((result, nets))
}

/// Exact comparison of `product(left)` with `product(right)`, without
/// converting either product to a `Decimal` (they need not be
/// representable).
pub(crate) fn compare_products(
    left: &[Decimal],
    right: &[Decimal],
    operation: &'static str,
) -> Result<core::cmp::Ordering> {
    let error = || MarginError::arithmetic(operation);
    let (l, ls) = wide_product(left).ok_or_else(error)?;
    let (r, rs) = wide_product(right).ok_or_else(error)?;
    let common = ls.max(rs);
    let l = l.align(ls, common).ok_or_else(error)?;
    let r = r.align(rs, common).ok_or_else(error)?;
    Ok(l.cmp_signed(&r))
}

/// Product of the mantissas and sum of the scales.
fn wide_product(values: &[Decimal]) -> Option<(Wide, u32)> {
    let mut product = Wide::new(false, U768::from_u128(1));
    let mut scale = 0_u32;
    for value in values {
        let (mantissa, value_scale) = Wide::from_decimal(*value);
        product = product.checked_mul(mantissa)?;
        scale = scale.checked_add(value_scale)?;
    }
    Some((product, scale))
}

/// `product(numerators) / product(denominators)` rounded once to `scale`
/// digits, kept as a wide integer count of `10^-scale` units: unlike
/// [`mul_div_round`], the rounded value need not fit a [`Decimal`], so it
/// can be combined with others before only the stored results are
/// converted ([`wide_to_decimal`]).
pub(crate) fn mul_div_round_wide(
    numerators: &[Decimal],
    denominators: &[Decimal],
    scale: u32,
    rounding: Rounding,
    operation: &'static str,
) -> Result<Wide> {
    let error = || MarginError::arithmetic(operation);
    let (numerator, numerator_scale) = wide_product(numerators).ok_or_else(error)?;
    let (denominator, denominator_scale) = wide_product(denominators).ok_or_else(error)?;
    let numerator = numerator.scale_up(denominator_scale).ok_or_else(error)?;
    let denominator = denominator.scale_up(numerator_scale).ok_or_else(error)?;
    round_ratio_wide(numerator, denominator, scale, rounding).ok_or_else(error)
}

/// `value` as a wide integer count of `10^-scale` units. Fails if `value`
/// has more than `scale` fractional digits.
pub(crate) fn decimal_to_wide(value: Decimal, scale: u32, operation: &'static str) -> Result<Wide> {
    let (wide, value_scale) = Wide::from_decimal(value);
    wide.align(value_scale, scale)
        .ok_or(MarginError::arithmetic(operation))
}

/// The decimal `value * 10^-scale`. Fails if it cannot be represented.
pub(crate) fn wide_to_decimal(value: Wide, scale: u32, operation: &'static str) -> Result<Decimal> {
    value
        .to_decimal(scale)
        .ok_or(MarginError::arithmetic(operation))
}

/// The decimal nearest to `numerator / denominator` at `scale` digits under
/// `rounding`, from the exact quotient and remainder.
fn round_ratio(
    numerator: Wide,
    denominator: Wide,
    scale: u32,
    rounding: Rounding,
) -> Option<Decimal> {
    round_ratio_wide(numerator, denominator, scale, rounding)?.to_decimal(scale)
}

/// [`round_ratio`] as a wide integer count of `10^-scale` units.
fn round_ratio_wide(
    numerator: Wide,
    denominator: Wide,
    scale: u32,
    rounding: Rounding,
) -> Option<Wide> {
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
    Some(Wide::new(
        numerator.negative != denominator.negative,
        quotient,
    ))
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

    /// M104-1: a sum that `rust_decimal` rounds (and whose inverse
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

    /// Only the complete product has to be representable.
    #[test]
    fn product_of_three_factors_is_exact() {
        // MAX * 2 overflows, but MAX * 2 * 0.1 = MAX / 5 does not.
        assert_eq!(
            product(&[Decimal::MAX, dec!(2), dec!(0.1)], "t"),
            Ok(dec!(15845632502852867518708790067))
        );
        // 0.1 * 1e-28 needs 29 digits; times 10 it is 1e-28.
        assert_eq!(
            product(&[dec!(0.1), Decimal::new(1, 28), dec!(10)], "t"),
            Ok(Decimal::new(1, 28))
        );
        assert_eq!(
            product(&[Decimal::MAX, dec!(2), dec!(1)], "t"),
            Err(MarginError::arithmetic("t"))
        );
        assert_eq!(product(&[], "t"), Ok(dec!(1)));
    }

    #[test]
    fn exact_sum_of_products_without_rounding() {
        // MAX * 2 - MAX * 2 + 0.1 * 3: intermediates overflow, the result is 0.3.
        assert_eq!(
            sum_products(
                &[
                    &[Decimal::MAX, dec!(2)],
                    &[Decimal::MIN, dec!(2)],
                    &[dec!(0.1), dec!(3)]
                ],
                "t"
            ),
            Ok(dec!(0.3))
        );
        assert_eq!(
            sum_products(&[&[Decimal::MAX, dec!(2)]], "t"),
            Err(MarginError::arithmetic("t"))
        );
    }

    #[test]
    fn exact_sum_in_any_order() {
        for values in [
            [Decimal::MAX, Decimal::MIN, dec!(0.5)],
            [Decimal::MAX, dec!(0.5), Decimal::MIN],
            [dec!(0.5), Decimal::MAX, Decimal::MIN],
        ] {
            assert_eq!(sum(&values, "t"), Ok(dec!(0.5)));
        }
        assert_eq!(
            sum(&[Decimal::MAX, dec!(1)], "t"),
            Err(MarginError::arithmetic("t"))
        );
        assert_eq!(sum(&[], "t"), Ok(Decimal::ZERO));
    }

    #[test]
    fn exact_sums_and_product_comparisons() {
        use core::cmp::Ordering;
        // MAX + 0.1 - MAX: the partial sum is not representable, the net is.
        let (q, nets) = quadratic_form_round(
            &[vec![Decimal::MAX, dec!(0.1), Decimal::MIN]],
            &[vec![dec!(1)]],
            6,
            Rounding::HalfEven,
            "t",
        )
        .expect("exact");
        assert_eq!((q, nets), (dec!(0.01), vec![Some(dec!(0.1))]));
        // 2 * MAX is not a Decimal; with a zero covariance the form is 0.
        let (q, nets) = quadratic_form_round(
            &[vec![Decimal::MAX, Decimal::MAX]],
            &[vec![dec!(0)]],
            6,
            Rounding::HalfEven,
            "t",
        )
        .expect("exact");
        assert_eq!((q, nets), (dec!(0), vec![None]));
        // 1e-16 * 1e-16 = 1e-32 is not a Decimal, but compares exactly.
        let tiny = Decimal::new(1, 16);
        assert_eq!(
            compare_products(&[tiny, tiny], &[Decimal::ZERO], "t"),
            Ok(Ordering::Greater)
        );
        assert_eq!(
            compare_products(&[tiny, tiny], &[tiny, tiny], "t"),
            Ok(Ordering::Equal)
        );
        assert_eq!(
            compare_products(
                &[Decimal::MAX, Decimal::MAX],
                &[Decimal::MAX, Decimal::MAX, dec!(1.1)],
                "t"
            ),
            Ok(Ordering::Less)
        );
        assert_eq!(
            compare_products(&[dec!(-1), dec!(2)], &[dec!(1)], "t"),
            Ok(Ordering::Less)
        );
    }

    /// Wide results agree with i128 arithmetic where i128 suffices.
    #[allow(clippy::many_single_char_names)]
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
