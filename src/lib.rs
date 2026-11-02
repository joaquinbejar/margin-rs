//! # margin-rs
//!
//! Risk and margin engine for the Xynthex market making lab, usable
//! standalone.
//!
//! ## Status
//!
//! Bootstrap only. The crate currently exports the typed error boundary,
//! [`MarginError`] and the [`Result`] alias. It does not yet implement a
//! ledger, pretrade risk checks or any margin model.
//!
//! ## Policies
//!
//! - Edition 2024, MSRV 1.85; CI checks both the latest stable and the MSRV.
//! - Small default dependency set; the crate never depends on
//!   market-maker-rs or tessera-rl (they depend on it).
//! - No binary floating point arithmetic: the `clippy::float_arithmetic`
//!   lint is denied crate wide, and money, prices and quantities will use
//!   exact decimals with checked operations.
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

pub mod error;

pub use error::{MarginError, Result};

// Compiles and runs the Rust examples in README.md as doctests, so the README
// cannot drift from the exported API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
