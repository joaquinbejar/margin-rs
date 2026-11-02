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
//! - [`MarginError`] and [`Result`]: the typed error boundary.
//!
//! Pretrade risk checks and margin models are not implemented yet.
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

pub mod decimal;
pub mod error;
pub mod ids;
pub mod instrument;
pub mod ledger;

pub use decimal::Rounding;
pub use error::{MarginError, Result};
pub use ids::{Asset, ExecutionId, InstrumentId, ReservationId, TransferId};
pub use instrument::{Instrument, SpotInstrument};
pub use rust_decimal::Decimal;

// Compiles and runs the Rust examples in README.md as doctests, so the README
// cannot drift from the exported API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
