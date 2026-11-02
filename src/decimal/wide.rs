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

impl U768 {
    pub(crate) const ZERO: Self = Self([0; LIMBS]);

    pub(crate) fn from_u128(value: u128) -> Self {
        let mut limbs = [0; LIMBS];
        // Truncations are intended: low and high halves of the u128.
        limbs[0] = value as u64;
        limbs[1] = (value >> 64) as u64;
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
        if self.0[2..].iter().any(|limb| *limb != 0) {
            return None;
        }
        Some(u128::from(self.0[0]) | (u128::from(self.0[1]) << 64))
    }

    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        let mut out = [0; LIMBS];
        let mut carry = 0_u64;
        for (i, slot) in out.iter_mut().enumerate() {
            let (sum, c1) = self.0[i].overflowing_add(other.0[i]);
            let (sum, c2) = sum.overflowing_add(carry);
            *slot = sum;
            carry = u64::from(c1) + u64::from(c2);
        }
        (carry == 0).then_some(Self(out))
    }

    /// `self - other`, or `None` if `other > self`.
    pub(crate) fn checked_sub(self, other: Self) -> Option<Self> {
        let mut out = [0; LIMBS];
        let mut borrow = 0_u64;
        for (i, slot) in out.iter_mut().enumerate() {
            let (diff, b1) = self.0[i].overflowing_sub(other.0[i]);
            let (diff, b2) = diff.overflowing_sub(borrow);
            *slot = diff;
            borrow = u64::from(b1) + u64::from(b2);
        }
        (borrow == 0).then_some(Self(out))
    }

    pub(crate) fn checked_mul(self, other: Self) -> Option<Self> {
        let mut wide = [0_u64; 2 * LIMBS];
        for i in 0..LIMBS {
            if self.0[i] == 0 {
                continue;
            }
            let mut carry = 0_u128;
            for j in 0..LIMBS {
                let current = u128::from(wide[i + j])
                    + u128::from(self.0[i]) * u128::from(other.0[j])
                    + carry;
                // Low 64 bits stay in the limb, the rest carries.
                wide[i + j] = current as u64;
                carry = current >> 64;
            }
            let mut k = i + LIMBS;
            while carry != 0 {
                let current = u128::from(wide[k]) + carry;
                wide[k] = current as u64;
                carry = current >> 64;
                k += 1;
            }
        }
        if wide[LIMBS..].iter().any(|limb| *limb != 0) {
            return None;
        }
        let mut out = [0; LIMBS];
        out.copy_from_slice(&wide[..LIMBS]);
        Some(Self(out))
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

    fn bit_length(&self) -> usize {
        for i in (0..LIMBS).rev() {
            if self.0[i] != 0 {
                return i * 64 + (64 - self.0[i].leading_zeros() as usize);
            }
        }
        0
    }

    fn bit(&self, index: usize) -> bool {
        (self.0[index / 64] >> (index % 64)) & 1 == 1
    }

    fn set_bit(&mut self, index: usize) {
        self.0[index / 64] |= 1 << (index % 64);
    }

    fn shl1(self) -> Self {
        let mut out = [0; LIMBS];
        let mut carry = 0;
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = (self.0[i] << 1) | carry;
            carry = self.0[i] >> 63;
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
        for index in (0..self.bit_length()).rev() {
            remainder = remainder.shl1();
            if self.bit(index) {
                remainder.0[0] |= 1;
            }
            if remainder >= divisor {
                // remainder >= divisor, so this cannot fail.
                remainder = remainder.checked_sub(divisor)?;
                quotient.set_bit(index);
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
        for i in (0..LIMBS).rev() {
            match self.0[i].cmp(&other.0[i]) {
                Ordering::Equal => continue,
                unequal => return unequal,
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
            scale -= 1;
        }
        let mantissa = magnitude.to_u128()?;
        // Split the 96 bit mantissa into the three 32 bit parts.
        let lo = mantissa as u32;
        let mid = (mantissa >> 32) as u32;
        let hi = (mantissa >> 64) as u32;
        Some(Decimal::from_parts(lo, mid, hi, self.negative, scale))
    }
}

#[cfg(test)]
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
