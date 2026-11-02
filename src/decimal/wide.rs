//! Fixed width unsigned integers for exact decimal arithmetic.
//!
//! A [`Decimal`] is `mantissa / 10^scale` with a 96 bit mantissa and a scale
//! of at most 28. Exact sums, products and quotients of such values need
//! intermediate integers far wider than 96 bits: a product of three
//! mantissas scaled by `10^56` needs about 475 bits. [`U768`] holds 768
//! bits, enough for every operation in this crate with margin; any
//! operation that would exceed it returns `None` instead of wrapping.
//!
//! Only the operations the crate needs are implemented: addition,
//! subtraction, multiplication, comparison, division with remainder and
//! powers of ten.

use core::cmp::Ordering;

use rust_decimal::Decimal;

const LIMBS: usize = 12;

/// Unsigned 768 bit integer, little endian 64 bit limbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct U768([u64; LIMBS]);

/// The low and high 64 bit halves of `value`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the split itself: the low half is masked to 64 bits and the high half is shifted down 64 bits, so no bit is lost"
)]
const fn halves(value: u128) -> (u64, u64) {
    ((value & (u64::MAX as u128)) as u64, (value >> 64) as u64)
}

impl U768 {
    pub(crate) const ZERO: Self = Self([0; LIMBS]);

    pub(crate) fn from_u128(value: u128) -> Self {
        let (low, high) = halves(value);
        let mut limbs = [0; LIMBS];
        limbs[0] = low;
        limbs[1] = high;
        Self(limbs)
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.0.iter().all(|limb| *limb == 0)
    }

    pub(crate) fn is_odd(&self) -> bool {
        self.0[0] & 1 == 1
    }

    /// The value as `u128`, or `None` if it does not fit.
    pub(crate) fn to_u128(self) -> Option<u128> {
        let [low, high, rest @ ..] = self.0;
        if rest.iter().any(|limb| *limb != 0) {
            return None;
        }
        Some(u128::from(low) | (u128::from(high) << 64))
    }

    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        let mut out = [0; LIMBS];
        let mut carry = false;
        for ((slot, a), b) in out.iter_mut().zip(self.0).zip(other.0) {
            let (sum, c1) = a.overflowing_add(b);
            let (sum, c2) = sum.overflowing_add(u64::from(carry));
            *slot = sum;
            // At most one of the two additions carries.
            carry = c1 || c2;
        }
        (!carry).then_some(Self(out))
    }

    /// `self - other`, or `None` if `other > self`.
    pub(crate) fn checked_sub(self, other: Self) -> Option<Self> {
        let mut out = [0; LIMBS];
        let mut borrow = false;
        for ((slot, a), b) in out.iter_mut().zip(self.0).zip(other.0) {
            let (diff, b1) = a.overflowing_sub(b);
            let (diff, b2) = diff.overflowing_sub(u64::from(borrow));
            *slot = diff;
            // At most one of the two subtractions borrows.
            borrow = b1 || b2;
        }
        (!borrow).then_some(Self(out))
    }

    pub(crate) fn checked_mul(self, other: Self) -> Option<Self> {
        let mut wide = [0_u64; 2 * LIMBS];
        for (i, a) in self.0.into_iter().enumerate() {
            if a == 0 {
                continue;
            }
            let row = wide.get_mut(i..)?;
            let mut carry = 0_u128;
            for (slot, b) in row.iter_mut().zip(other.0) {
                // (2^64 - 1) + (2^64 - 1)^2 + (2^64 - 1) = 2^128 - 1: these
                // never overflow, and a failure would return `None`.
                let current = u128::from(a)
                    .checked_mul(u128::from(b))?
                    .checked_add(u128::from(*slot))?
                    .checked_add(carry)?;
                // Low 64 bits stay in the limb, the rest carries.
                let (low, high) = halves(current);
                *slot = low;
                carry = u128::from(high);
            }
            for slot in row.iter_mut().skip(LIMBS) {
                if carry == 0 {
                    break;
                }
                let (low, high) = halves(u128::from(*slot).checked_add(carry)?);
                *slot = low;
                carry = u128::from(high);
            }
            if carry != 0 {
                return None;
            }
        }
        let (low, high) = wide.split_first_chunk::<LIMBS>()?;
        if high.iter().any(|limb| *limb != 0) {
            return None;
        }
        Some(Self(*low))
    }

    /// `10^exponent`, or `None` if it does not fit.
    pub(crate) fn pow10(exponent: u32) -> Option<Self> {
        let ten = Self::from_u128(10);
        let mut out = Self::from_u128(1);
        for _ in 0..exponent {
            out = out.checked_mul(ten)?;
        }
        Some(out)
    }

    /// Number of significant bits, or `None` if it does not fit `usize`.
    fn bit_length(&self) -> Option<usize> {
        for (i, limb) in self.0.iter().enumerate().rev() {
            if *limb != 0 {
                let significant =
                    usize::try_from(u64::BITS.checked_sub(limb.leading_zeros())?).ok()?;
                return i.checked_mul(64)?.checked_add(significant);
            }
        }
        Some(0)
    }

    fn bit(&self, index: usize) -> bool {
        self.0
            .get(index / 64)
            .is_some_and(|limb| (limb >> (index % 64)) & 1 == 1)
    }

    /// Sets bit `index`; `None` if it is past the 768 bits.
    fn set_bit(&mut self, index: usize) -> Option<()> {
        let limb = self.0.get_mut(index / 64)?;
        *limb |= 1 << (index % 64);
        Some(())
    }

    fn shl1(self) -> Self {
        let mut out = [0; LIMBS];
        let mut carry = 0;
        for (slot, limb) in out.iter_mut().zip(self.0) {
            *slot = (limb << 1) | carry;
            carry = limb >> 63;
        }
        Self(out)
    }

    /// Quotient and remainder of `self / divisor`, or `None` when the
    /// divisor is zero. Binary long division.
    pub(crate) fn div_rem(self, divisor: Self) -> Option<(Self, Self)> {
        if divisor.is_zero() {
            return None;
        }
        let mut quotient = Self::ZERO;
        let mut remainder = Self::ZERO;
        for index in (0..self.bit_length()?).rev() {
            remainder = remainder.shl1();
            if self.bit(index) {
                remainder.0[0] |= 1;
            }
            if remainder >= divisor {
                // remainder >= divisor, so this cannot fail.
                remainder = remainder.checked_sub(divisor)?;
                quotient.set_bit(index)?;
            }
        }
        Some((quotient, remainder))
    }
}

impl PartialOrd for U768 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for U768 {
    fn cmp(&self, other: &Self) -> Ordering {
        for (a, b) in self.0.iter().zip(other.0.iter()).rev() {
            let order = a.cmp(b);
            if order != Ordering::Equal {
                return order;
            }
        }
        Ordering::Equal
    }
}

/// Signed wide integer: a sign and a [`U768`] magnitude. Zero is never
/// negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wide {
    pub(crate) negative: bool,
    pub(crate) magnitude: U768,
}

impl Wide {
    pub(crate) const ZERO: Self = Self {
        negative: false,
        magnitude: U768::ZERO,
    };

    pub(crate) fn new(negative: bool, magnitude: U768) -> Self {
        Self {
            negative: negative && !magnitude.is_zero(),
            magnitude,
        }
    }

    /// The mantissa of `value`, with its scale.
    pub(crate) fn from_decimal(value: Decimal) -> (Self, u32) {
        let mantissa = value.mantissa();
        (
            Self::new(mantissa < 0, U768::from_u128(mantissa.unsigned_abs())),
            value.scale(),
        )
    }

    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        if self.negative == other.negative {
            return Some(Self::new(
                self.negative,
                self.magnitude.checked_add(other.magnitude)?,
            ));
        }
        if self.magnitude >= other.magnitude {
            Some(Self::new(
                self.negative,
                self.magnitude.checked_sub(other.magnitude)?,
            ))
        } else {
            Some(Self::new(
                other.negative,
                other.magnitude.checked_sub(self.magnitude)?,
            ))
        }
    }

    pub(crate) fn checked_mul(self, other: Self) -> Option<Self> {
        Some(Self::new(
            self.negative != other.negative,
            self.magnitude.checked_mul(other.magnitude)?,
        ))
    }

    /// `self`, given at scale `from`, rescaled to the scale `to >= from`:
    /// `self * 10^(to - from)`. `None` if `to < from` or on overflow.
    pub(crate) fn align(self, from: u32, to: u32) -> Option<Self> {
        self.scale_up(to.checked_sub(from)?)
    }

    /// `self` with its sign flipped. Exact.
    pub(crate) fn negated(self) -> Self {
        Self::new(!self.negative, self.magnitude)
    }

    /// `self * 10^exponent`.
    pub(crate) fn scale_up(self, exponent: u32) -> Option<Self> {
        Some(Self::new(
            self.negative,
            self.magnitude.checked_mul(U768::pow10(exponent)?)?,
        ))
    }

    /// Signed comparison.
    pub(crate) fn cmp_signed(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => self.magnitude.cmp(&other.magnitude),
            (true, true) => other.magnitude.cmp(&self.magnitude),
        }
    }

    /// The decimal `self / 10^scale`, if it can be represented exactly.
    ///
    /// Trailing zeros are removed only as far as needed to fit the 96 bit
    /// mantissa and the 28 digit scale; otherwise the scale is kept.
    /// Zero is always returned as `Decimal::ZERO` (scale 0).
    pub(crate) fn to_decimal(self, scale: u32) -> Option<Decimal> {
        const MAX_MANTISSA: u128 = (1 << 96) - 1;
        if self.magnitude.is_zero() {
            return Some(Decimal::ZERO);
        }
        let ten = U768::from_u128(10);
        let mut magnitude = self.magnitude;
        let mut scale = scale;
        loop {
            let fits = scale <= 28 && magnitude.to_u128().is_some_and(|m| m <= MAX_MANTISSA);
            if fits {
                break;
            }
            if scale == 0 {
                return None;
            }
            let (quotient, remainder) = magnitude.div_rem(ten)?;
            if !remainder.is_zero() {
                return None;
            }
            magnitude = quotient;
            scale = scale.checked_sub(1)?;
        }
        let mantissa = magnitude.to_u128()?;
        let (lo, mid, hi) = parts(mantissa);
        Some(Decimal::from_parts(lo, mid, hi, self.negative, scale))
    }
}

/// The three 32 bit parts of a 96 bit mantissa, low first.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the split itself: each part is masked to 32 bits after its shift, and the caller passes at most 96 bits"
)]
const fn parts(mantissa: u128) -> (u32, u32, u32) {
    let mask = u32::MAX as u128;
    (
        (mantissa & mask) as u32,
        ((mantissa >> 32) & mask) as u32,
        ((mantissa >> 64) & mask) as u32,
    )
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

    fn u(value: u128) -> U768 {
        U768::from_u128(value)
    }

    #[test]
    fn arithmetic_matches_u128_where_it_fits() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            // xorshift64*, deterministic.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for _ in 0..2000 {
            let a = u128::from(next());
            let b = u128::from(next() >> (next() % 64));
            assert_eq!(u(a).checked_mul(u(b)).and_then(U768::to_u128), Some(a * b));
            assert_eq!(u(a).checked_add(u(b)).and_then(U768::to_u128), Some(a + b));
            if b != 0 {
                let (q, r) = u(a * 7 + 3).div_rem(u(b)).expect("non zero");
                assert_eq!(q.to_u128(), Some((a * 7 + 3) / b));
                assert_eq!(r.to_u128(), Some((a * 7 + 3) % b));
            }
            assert_eq!(u(a).checked_sub(u(a + 1)), None);
        }
    }

    #[test]
    fn overflow_is_reported() {
        let big = U768::pow10(231).expect("10^231 fits in 768 bits");
        assert!(U768::pow10(232).is_none());
        assert!(big.checked_mul(big).is_none());
        assert!(u(1).div_rem(U768::ZERO).is_none());
    }

    #[test]
    fn decimal_round_trip_and_trailing_zeros() {
        let (w, scale) = Wide::from_decimal(Decimal::new(-12_345, 3));
        assert_eq!(w.to_decimal(scale), Some(Decimal::new(-12_345, 3)));
        // 1.20 * 10^30 at scale 30 strips zeros down to scale 28.
        let w = Wide::new(false, u(120)).scale_up(28).expect("fits");
        assert_eq!(w.to_decimal(30), Some(Decimal::new(12, 1)));
        // A value needing 29 digits cannot be represented.
        assert_eq!(Wide::new(false, u(1)).to_decimal(29), None);
        assert_eq!(Wide::new(true, U768::ZERO), Wide::ZERO);
        // Zero comes back at scale 0, whatever the requested scale.
        assert_eq!(Wide::ZERO.to_decimal(56).map(|d| d.scale()), Some(0));
    }
}
