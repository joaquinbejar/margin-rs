//! Exact rational arithmetic for the instrument tests: `i128` numerator and
//! denominator, every operation checked. An overflow fails the test loudly;
//! nothing is ever rounded.

#![allow(dead_code, clippy::expect_used)]

use margin_rs::Decimal;

/// An exact fraction `n / d`, `d > 0`, reduced.
#[derive(Debug, Clone, Copy)]
pub struct Q {
    n: i128,
    d: i128,
}

fn gcd(mut a: i128, mut b: i128) -> i128 {
    a = a.checked_abs().expect("gcd abs");
    b = b.checked_abs().expect("gcd abs");
    while b != 0 {
        let r = a.checked_rem(b).expect("gcd rem");
        a = b;
        b = r;
    }
    a
}

impl Q {
    /// `n / d` reduced, with a positive denominator.
    pub fn new(n: i128, d: i128) -> Self {
        assert!(d != 0, "zero denominator");
        let g = gcd(n, d).max(1);
        let (mut n, mut d) = (
            n.checked_div(g).expect("div"),
            d.checked_div(g).expect("div"),
        );
        if d < 0 {
            n = n.checked_neg().expect("neg");
            d = d.checked_neg().expect("neg");
        }
        Self { n, d }
    }

    /// The exact value of a decimal: its mantissa over `10^scale`.
    pub fn dec(value: Decimal) -> Self {
        let d = 10_i128.checked_pow(value.scale()).expect("pow10");
        Self::new(value.mantissa(), d)
    }

    /// `num / den` of two decimals.
    pub fn ratio(num: Decimal, den: Decimal) -> Self {
        Self::dec(num).div(Self::dec(den))
    }

    pub fn add(self, o: Self) -> Self {
        let n = self
            .n
            .checked_mul(o.d)
            .and_then(|a| o.n.checked_mul(self.d).and_then(|b| a.checked_add(b)))
            .expect("exact add");
        Self::new(n, self.d.checked_mul(o.d).expect("exact add"))
    }

    pub fn neg(self) -> Self {
        Self::new(self.n.checked_neg().expect("neg"), self.d)
    }

    pub fn sub(self, o: Self) -> Self {
        self.add(o.neg())
    }

    pub fn mul(self, o: Self) -> Self {
        Self::new(
            self.n.checked_mul(o.n).expect("exact mul"),
            self.d.checked_mul(o.d).expect("exact mul"),
        )
    }

    pub fn div(self, o: Self) -> Self {
        Self::new(
            self.n.checked_mul(o.d).expect("exact div"),
            self.d.checked_mul(o.n).expect("exact div"),
        )
    }
}

impl PartialEq for Q {
    fn eq(&self, o: &Self) -> bool {
        // Both are reduced with a positive denominator.
        self.n == o.n && self.d == o.d
    }
}

impl Eq for Q {}
