//! Identifiers: assets, instruments, executions, transfers and reservations.
//!
//! Every identifier is an owned, validated string. Validation rejects empty
//! values, whitespace and control characters, so identifiers print and
//! compare without surprises. Ordering is lexicographic, which makes every
//! map keyed by an identifier iterate deterministically.

use core::fmt;

use crate::error::{MarginError, Result};

/// Maximum length, in bytes, of any identifier.
pub const MAX_ID_LEN: usize = 64;

fn validate(value: &str, field: &'static str) -> Result<()> {
    if value.is_empty() {
        return Err(MarginError::invalid_input(field, "must not be empty"));
    }
    if value.len() > MAX_ID_LEN {
        return Err(MarginError::invalid_input(
            field,
            "must be at most 64 bytes",
        ));
    }
    if !value.chars().all(|c| c.is_ascii_graphic()) {
        return Err(MarginError::invalid_input(
            field,
            "must contain only printable ASCII without whitespace",
        ));
    }
    Ok(())
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Box<str>);

        impl $name {
            /// Builds the identifier after validating it.
            ///
            /// # Errors
            ///
            /// [`MarginError::InvalidInput`] if the value is empty, longer
            /// than [`MAX_ID_LEN`] bytes, or contains whitespace, control or
            /// non ASCII characters.
            pub fn new(value: impl AsRef<str>) -> Result<Self> {
                let value = value.as_ref();
                validate(value, $field)?;
                Ok(Self(value.into()))
            }

            /// Returns the identifier as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    /// Identifier of an execution (a fill), unique within one ledger.
    ///
    /// Booking is idempotent by this identifier: see
    /// [`Ledger::book_execution`](crate::ledger::Ledger::book_execution).
    ExecutionId,
    "execution_id"
);

string_id!(
    /// Identifier of a deposit or withdrawal, unique within one ledger.
    TransferId,
    "transfer_id"
);

string_id!(
    /// Identifier of a balance reservation, unique within one ledger.
    ReservationId,
    "reservation_id"
);

string_id!(
    /// Identifier of an order submitted through the
    /// [`RiskGate`](crate::risk::RiskGate), unique within one gate.
    OrderId,
    "order_id"
);

string_id!(
    /// Identifier of an account. Margin is computed per account and never
    /// netted across accounts.
    AccountId,
    "account_id"
);

string_id!(
    /// Identifier of a venue. Margin is computed per venue and never netted
    /// across venues.
    VenueId,
    "venue_id"
);

string_id!(
    /// Identifier of a risk factor, for example `BTC-USD-RETURN`.
    FactorId,
    "factor_id"
);

string_id!(
    /// Identifier of a tradable instrument, for example `BTC-USDC`.
    InstrumentId,
    "instrument_id"
);

impl From<u64> for ExecutionId {
    fn from(value: u64) -> Self {
        Self(value.to_string().into())
    }
}

/// Code of an asset (a currency or a token), for example `USDC` or `BTC`.
///
/// Codes are 1 to 16 characters from `A-Z` and `0-9`. Amounts in different
/// assets are never added together by this crate; see
/// [`Valuation::net_in`](crate::ledger::Valuation::net_in) for the explicit
/// conversion.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Asset(Box<str>);

impl Asset {
    /// Maximum length of an asset code.
    pub const MAX_LEN: usize = 16;

    /// Builds an asset code after validating it.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] unless the code has 1 to 16 characters,
    /// all of them uppercase ASCII letters or digits.
    pub fn new(code: impl AsRef<str>) -> Result<Self> {
        let code = code.as_ref();
        if code.is_empty() || code.len() > Self::MAX_LEN {
            return Err(MarginError::invalid_input(
                "asset",
                "must have 1 to 16 characters",
            ));
        }
        if !code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return Err(MarginError::invalid_input(
                "asset",
                "must contain only A-Z and 0-9",
            ));
        }
        Ok(Self(code.into()))
    }

    /// Returns the code as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Asset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Asset {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_codes_are_validated() {
        assert_eq!(Asset::new("USDC").map(|a| a.to_string()), Ok("USDC".into()));
        assert!(Asset::new("BTC2").is_ok());
        assert!(Asset::new("").is_err());
        assert!(Asset::new("usdc").is_err());
        assert!(Asset::new("US DC").is_err());
        assert!(Asset::new("ABCDEFGHIJKLMNOPQ").is_err());
    }

    #[test]
    fn string_ids_are_validated() {
        assert!(ExecutionId::new("run1:42").is_ok());
        assert!(ExecutionId::new("").is_err());
        assert!(ExecutionId::new("a b").is_err());
        assert!(ExecutionId::new("x".repeat(MAX_ID_LEN + 1)).is_err());
        assert!(InstrumentId::new("BTC-USDC").is_ok());
        assert!(TransferId::new("dep\n1").is_err());
    }

    #[test]
    fn numeric_execution_ids() {
        assert_eq!(ExecutionId::from(42_u64).as_str(), "42");
    }
}
