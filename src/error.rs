//! Typed error boundary of the crate.
//!
//! Every fallible public function returns [`Result`]. Variants carry typed
//! fields instead of preformatted strings, so callers can match on the cause
//! without parsing messages. The enum is `#[non_exhaustive]`: later modules
//! add variants without a breaking change, and callers must keep a wildcard
//! arm.

use thiserror::Error;

/// Result alias used by every fallible function in this crate.
pub type Result<T> = core::result::Result<T, MarginError>;

/// Error returned by margin-rs operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum MarginError {
    /// An input value is outside its valid domain.
    ///
    /// `field` names the offending input and `reason` states the violated
    /// rule, for example a negative quantity where only positive ones are
    /// valid.
    #[error("invalid input `{field}`: {reason}")]
    InvalidInput {
        /// Name of the offending input.
        field: &'static str,
        /// Rule the input violates.
        reason: &'static str,
    },

    /// A checked arithmetic operation returned no value.
    ///
    /// Raised when a checked decimal operation overflows, underflows or
    /// divides by zero. Results are never saturated or rounded silently.
    #[error("arithmetic failure in {operation}")]
    Arithmetic {
        /// Operation that failed, for example `"cash + fee"`.
        operation: &'static str,
    },
}

impl MarginError {
    /// Builds [`MarginError::InvalidInput`].
    #[must_use]
    pub const fn invalid_input(field: &'static str, reason: &'static str) -> Self {
        Self::InvalidInput { field, reason }
    }

    /// Builds [`MarginError::Arithmetic`].
    #[must_use]
    pub const fn arithmetic(operation: &'static str) -> Self {
        Self::Arithmetic { operation }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_build_the_matching_variant() {
        assert_eq!(
            MarginError::invalid_input("quantity", "must be positive"),
            MarginError::InvalidInput {
                field: "quantity",
                reason: "must be positive",
            }
        );
        assert_eq!(
            MarginError::arithmetic("cash + fee"),
            MarginError::Arithmetic {
                operation: "cash + fee",
            }
        );
    }

    #[test]
    fn display_names_the_cause() {
        assert_eq!(
            MarginError::invalid_input("quantity", "must be positive").to_string(),
            "invalid input `quantity`: must be positive"
        );
        assert_eq!(
            MarginError::arithmetic("cash + fee").to_string(),
            "arithmetic failure in cash + fee"
        );
    }
}
