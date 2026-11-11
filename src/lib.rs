//! # margin-rs
//!
//! Risk and margin engine for market making and trading systems, with
//! exact decimal arithmetic throughout. Usable standalone or as the
//! accounting and risk layer of a market making stack.
//!
//! ## Status
//!
//! - [`ledger`]: the single accounting authority for one account. Spot
//!   instruments, linear and inverse futures and perpetuals and options, available
//!   and reserved balances per asset, spot positions with signed average
//!   cost, derivative positions as signed contracts with a basis, funding
//!   and expiry settlement, fees per asset, idempotent booking by execution
//!   identifier, per execution cashflow reconciliation, snapshots and an
//!   explicit mark to market policy. An inverse contract settles in the
//!   base asset, and the exact rest of each of its values is recorded.
//! - [`risk`]: the pretrade risk gate for spot orders. Every order passes
//!   through [`risk::RiskGate::submit`], which reserves balance or
//!   inventory in the ledger, fees included whether the venue charges them
//!   in the quote or the base asset (with the rounding the venue declares
//!   for fees in base), and enforces hard limits on the joint
//!   exposure of positions and live orders, with typed accepted, reduced
//!   and rejected decisions, quote withdrawal on a circuit breaker, and
//!   exactly once reservation updates on fills, rejects and confirmed
//!   cancels. Derivative executions, funding and expiries are posted
//!   through the gate as mandatory postings.
//! - [`valuation`]: account mark to market in one reporting asset over an
//!   immutable ledger snapshot, with price source and freshness flags,
//!   collateral equity that invalid marks cannot raise, open linear and
//!   inverse derivatives at instrument marks, and PnL with fees counted
//!   once.
//! - [`scenarios`]: stress of spot inventory and open linear futures and
//!   perpetuals (hypothetical fills such as hedges, asset and instrument
//!   shocks, liquidation spread). Inverse positions are valued but not
//!   stressed yet.
//! - [`initial_margin`] and [`rules`]: versioned initial and maintenance
//!   margin rules with progressive tiers: `linear-im/1`
//!   ([`rules::TieredLinearIm`]) for linear futures and perpetuals and
//!   `inverse-im/1` ([`rules::TieredInverseIm`]) for inverse ones, in the
//!   base, and `short-option-im/1` ([`rules::ShortOptionIm`]) for short
//!   options. Every requirement carries its rule id and a per position
//!   breakdown.
//! - [`factors`] and [`covariance`]: factor exposures with explicit units
//!   and portfolio variance under a validated (symmetric, positive
//!   semidefinite) covariance matrix.
//! - [`collateral`]: available collateral of one account with explicit
//!   per asset haircuts and a declared stale price policy.
//! - [`margin_reservations`]: initial margin reserved for pending
//!   derivative orders of one account, recomputed exactly from positions
//!   and live orders, kept by the service or read from the gate's ledger.
//! - [`maintenance`] and [`liquidation`]: the maintenance threshold and
//!   account state (healthy, reduce only, default), and a staged closeout
//!   plan (cancel, reduce, residual) against scenario liquidity, with the
//!   deficit left explicit. The smallest closeout of each level is found by
//!   a certified search within [`liquidation::SEARCH_BUDGET`] evaluations
//!   per plan. Beyond it the plan takes the first count certified to meet
//!   the target, and [`liquidation::LiquidationPlan::minimal`] is `false`
//!   when that count is not proven the smallest; with no certified count
//!   the plan fails with [`MarginError::CloseoutSearchLimit`]. The account
//!   is read with
//!   [`liquidation::LiquidationAccount::from_ledger`], and each forced fill
//!   is booked back with [`liquidation::ForcedReduction::execution`], so the
//!   booked plan leaves exactly the planned state.
//! - Options: [`instrument::OptionInstrument`], with the premium paid in cash
//!   at the fill, settlement by a cash price, a delivery or a lapse
//!   ([`ledger::Ledger::settle_option`]), valuation at premium marks, and
//!   `short-option-im/1` ([`rules::ShortOptionIm`]) for the short side. The
//!   crate prices nothing: marks and outcomes are inputs. Stress and closeout
//!   plans refuse an open option, and margin reservations block in its
//!   currency. See `docs/options.md`.
//! - [`MarginError`] and [`Result`]: the typed error boundary.
//!
//! ## Feature flags
//!
//! | Feature | Default | Effect |
//! |---|---|---|
//! | `instruments` | off | `Instrument::from_spec` and `instrument::InstrumentScales`: an exact conversion of a `financial_types` instrument specification (spot, linear and inverse futures and perpetuals, options) into an [`Instrument`], with explicit scales and typed errors (`MarginError::ScaleBelowGrid`, `MarginError::UnsupportedSpec` for what is not mapped). Enables `financial_types/instruments`. |
//!
//! ## Policies
//!
//! - Edition 2024, MSRV 1.88; CI checks both the latest stable and the MSRV.
//! - Small default dependency set; the crate never depends on
//!   market-maker-rs. market-maker-rs uses margin-rs optionally through its
//!   `margin` feature, pinned by git to an earlier revision; updating it to
//!   this release is pending.
//! - No binary floating point arithmetic (`clippy::float_arithmetic`).
//!   Money, prices and quantities are [`Decimal`]s; every operation is
//!   checked and exact, and the few rounding steps are explicit (see
//!   [`decimal`]).
//! - No panics and no unchecked arithmetic in library code: `unwrap`,
//!   `expect`, `panic!`, unchecked indexing and arithmetic with side effects
//!   are lint errors, with documented `#[expect]` exceptions only.
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

pub mod account_view;
pub mod collateral;
pub mod covariance;
pub mod decimal;
mod direction;
pub mod error;
pub mod factors;
pub mod ids;
pub mod initial_margin;
pub mod instrument;
pub mod ledger;
pub mod liquidation;
pub mod maintenance;
pub mod margin_reservations;
pub mod risk;
pub mod rules;
pub mod scenarios;
pub mod valuation;

pub use decimal::Rounding;
pub use error::{MarginError, Result};
pub use financial_types::Action;
pub use ids::{
    AccountId, Asset, ExecutionId, FactorId, InstrumentId, OrderId, ReservationId, TransferId,
    VenueId,
};
pub use instrument::{
    ContractSize, Instrument, InverseFuture, InversePerpetual, InverseTerms, LinearFuture,
    LinearPerpetual, LinearTerms, PositionKind, SpotInstrument,
};
pub use rust_decimal::Decimal;

// Compiles and runs the Rust examples in README.md as doctests, so the README
// cannot drift from the exported API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
