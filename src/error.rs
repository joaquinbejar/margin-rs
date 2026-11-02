//! Typed error boundary of the crate.
//!
//! Every fallible public function returns [`Result`]. Variants carry typed
//! fields instead of preformatted strings, so callers can match on the cause
//! without parsing messages. The enum is `#[non_exhaustive]`: later modules
//! add variants without a breaking change, and callers must keep a wildcard
//! arm.

use rust_decimal::Decimal;
use thiserror::Error;

use crate::ids::{Asset, FactorId, InstrumentId};

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

    /// A value has more fractional digits than allowed.
    ///
    /// Inputs are never truncated: a price, quantity or amount finer than
    /// its instrument or asset precision is rejected.
    #[error("`{field}` has {scale} fractional digits, at most {max_scale} allowed")]
    PrecisionExceeded {
        /// Name of the offending input.
        field: &'static str,
        /// Significant fractional digits of the value.
        scale: u32,
        /// Maximum allowed.
        max_scale: u32,
    },

    /// The asset is not registered in the ledger.
    #[error("unknown asset {asset}")]
    UnknownAsset {
        /// Asset code.
        asset: Asset,
    },

    /// The instrument is not registered in the ledger.
    #[error("unknown instrument {instrument}")]
    UnknownInstrument {
        /// Instrument identifier.
        instrument: InstrumentId,
    },

    /// An asset or instrument is already registered with a different
    /// definition.
    #[error("{kind} {id} is already registered with a different definition")]
    AlreadyRegistered {
        /// `"asset"` or `"instrument"`.
        kind: &'static str,
        /// Identifier.
        id: String,
    },

    /// An identifier was already used for an entry with different content.
    ///
    /// Repeating an identical entry is not an error: it is ignored, which
    /// makes booking idempotent.
    #[error("{kind} {id} already exists with different content")]
    ConflictingEntry {
        /// `"execution"`, `"transfer"` or `"reservation"`.
        kind: &'static str,
        /// Identifier.
        id: String,
    },

    /// No open reservation has this identifier.
    #[error("unknown reservation {id}")]
    UnknownReservation {
        /// Identifier.
        id: String,
    },

    /// The operation would leave the available balance of an asset
    /// negative.
    #[error("insufficient {asset}: {required} required, {available} available")]
    InsufficientBalance {
        /// Asset debited.
        asset: Asset,
        /// Amount the operation debits.
        required: Decimal,
        /// Available balance before the operation.
        available: Decimal,
    },

    /// No order with this identifier is known to the risk gate.
    #[error("unknown order {id}")]
    UnknownOrder {
        /// Order identifier.
        id: String,
    },

    /// The operation is not valid in the order's current state.
    #[error("cannot {operation} order {id} in state {state}")]
    InvalidOrderState {
        /// Order identifier.
        id: String,
        /// State the order is in.
        state: &'static str,
        /// Operation attempted.
        operation: &'static str,
    },

    /// An execution does not match the order it is reported for.
    #[error("execution for order {id} rejected: {reason}")]
    ExecutionMismatch {
        /// Order identifier.
        id: String,
        /// Rule the execution violates.
        reason: &'static str,
    },

    /// The margin rule does not cover this instrument.
    #[error("rule {rule} does not support {instrument}: {reason}")]
    UnsupportedInstrument {
        /// Rule identifier and version, for example `linear-im/1`.
        rule: String,
        /// Instrument identifier.
        instrument: InstrumentId,
        /// Why it is not covered.
        reason: &'static str,
    },

    /// An amount is in a different asset than required.
    #[error("{field} is in {found}, expected {expected}")]
    CurrencyMismatch {
        /// Name of the input.
        field: &'static str,
        /// Asset required.
        expected: Asset,
        /// Asset given.
        found: Asset,
    },

    /// An exposure refers to a factor the covariance matrix does not have.
    #[error("unknown factor {factor}")]
    UnknownFactor {
        /// Factor identifier.
        factor: FactorId,
    },

    /// An exposure's unit does not match its factor's unit, for example a
    /// vega exposure (per volatility point) against a price return factor.
    #[error("exposure to {factor} is per {found}, the factor moves in {expected}")]
    FactorUnitMismatch {
        /// Factor identifier.
        factor: FactorId,
        /// Unit of the factor.
        expected: &'static str,
        /// Unit of the exposure.
        found: &'static str,
    },

    /// A covariance matrix fails validation.
    #[error("invalid covariance matrix: {reason}")]
    InvalidCovariance {
        /// Rule violated.
        reason: &'static str,
    },

    /// A collateral asset's mark is not valid and the policy rejects it.
    #[error("collateral {asset} has an invalid mark and the policy rejects it")]
    InvalidCollateralMark {
        /// Asset.
        asset: Asset,
    },

    /// An open position has no mark price.
    #[error("missing mark price for {instrument}")]
    MissingMark {
        /// Instrument identifier.
        instrument: InstrumentId,
    },

    /// A conversion between two assets has no rate.
    #[error("missing conversion rate from {from} to {to}")]
    MissingConversionRate {
        /// Source asset.
        from: Asset,
        /// Target asset.
        to: Asset,
    },

    /// An order or execution direction this crate cannot trade: anything
    /// but [`Action::Buy`](financial_types::Action::Buy) and
    /// [`Action::Sell`](financial_types::Action::Sell), for example
    /// `Action::Other`.
    #[error("unsupported action {action:?} for {field}")]
    UnsupportedAction {
        /// Field that carried the action.
        field: &'static str,
        /// The action.
        action: financial_types::Action,
    },

    /// A reconciliation check failed: stored state differs from the state
    /// recomputed from the journal.
    #[error("reconciliation failed: {check} for {key}: expected {expected}, found {actual}")]
    ReconciliationMismatch {
        /// Name of the check.
        check: &'static str,
        /// Asset or instrument checked.
        key: String,
        /// Value recomputed from the journal.
        expected: Decimal,
        /// Value stored in the ledger.
        actual: Decimal,
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
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
