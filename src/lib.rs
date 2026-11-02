//! # margin-rs
//!
//! Risk and margin engine for the Xynthex market making lab, usable
//! standalone.
//!
//! ## Status
//!
//! - [`ledger`]: the single accounting authority for one account. Spot
//!   instruments, available and reserved balances per asset, positions with
//!   signed average cost, fees per asset, idempotent booking by execution
//!   identifier, per execution cashflow reconciliation, snapshots and an
//!   explicit mark to market policy.
//! - [`risk`]: the pretrade risk gate. Every order passes through
//!   [`risk::RiskGate::submit`], which reserves balance or inventory in the
//!   ledger and enforces hard limits on the joint exposure of positions and
//!   live orders, with typed accepted, reduced and rejected decisions, quote
//!   withdrawal on a circuit breaker, and exactly once reservation updates
//!   on fills, rejects and confirmed cancels.
//! - [`valuation`]: account mark to market in one reporting asset over an
//!   immutable ledger snapshot, with price source and freshness flags,
//!   collateral equity that invalid marks cannot raise, and PnL with fees
//!   counted once.
//! - [`scenarios`]: spot inventory stress (hypothetical fills such as
//!   hedges, price gaps and liquidation spread).
//! - [`initial_margin`] and [`rules`]: versioned initial and maintenance
//!   margin rules. The first is `xynthex-linear-im/1` for linear USDC
//!   futures and perpetuals; every requirement carries its rule id and a per
//!   position breakdown.
//! - [`factors`] and [`covariance`]: factor exposures with explicit units
//!   and portfolio variance under a validated (symmetric, positive
//!   semidefinite) covariance matrix.
//! - [`collateral`]: available collateral of one account with explicit
//!   per asset haircuts and a declared stale price policy.
//! - [`margin_reservations`]: initial margin reserved for pending
//!   derivative orders of one account, recomputed exactly from positions
//!   and live orders.
//! - [`MarginError`] and [`Result`]: the typed error boundary.
//!
//! ## Policies
//!
//! - Edition 2024, MSRV 1.85; CI checks both the latest stable and the MSRV.
//! - Small default dependency set; the crate never depends on
//!   market-maker-rs or tessera-rl (they depend on it).
//! - No binary floating point arithmetic: the `clippy::float_arithmetic`
//!   lint is denied crate wide. Money, prices and quantities are
//!   [`Decimal`]s; every operation is checked and exact, and the few
//!   rounding steps are explicit (see [`decimal`]).
//! - No `unsafe` code.
//!
//! ## Example
//!
//! ```
//! use margin_rs::{MarginError, Result};
//!
//! fn positive(field: &'static str, value: i64) -> Result<i64> {
//!     if value > 0 {
//!         Ok(value)
//!     } else {
//!         Err(MarginError::invalid_input(field, "must be positive"))
//!     }
//! }
//!
//! assert_eq!(positive("quantity", 3), Ok(3));
//! assert_eq!(
//!     positive("quantity", 0),
//!     Err(MarginError::InvalidInput {
//!         field: "quantity",
//!         reason: "must be positive",
//!     })
//! );
//! ```

pub mod collateral;
pub mod covariance;
pub mod decimal;
pub mod error;
pub mod factors;
pub mod ids;
pub mod initial_margin;
pub mod instrument;
pub mod ledger;
pub mod margin_reservations;
pub mod risk;
pub mod rules;
pub mod scenarios;
pub mod valuation;

pub use decimal::Rounding;
pub use error::{MarginError, Result};
pub use ids::{
    AccountId, Asset, ExecutionId, FactorId, InstrumentId, OrderId, ReservationId, TransferId,
    VenueId,
};
pub use instrument::{Instrument, SpotInstrument};
pub use rust_decimal::Decimal;

// Compiles and runs the Rust examples in README.md as doctests, so the README
// cannot drift from the exported API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
