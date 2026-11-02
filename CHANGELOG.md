# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- **Breaking:** the order and execution direction is `financial_types::Action`
  (0.3), shared with the other crates of the stack, re-exported as
  `margin_rs::Action` and `margin_rs::ledger::Action`. It replaces
  `ledger::Side` in `Execution`, `ExecutionCashflow`, `OrderRequest`,
  `OrderRecord`, `MarginOrder`, `HypotheticalFill`, `ForcedReduction` and
  `CloseoutIntent::Reduce`. `Action::Buy` and `Action::Sell` behave as
  `Side::Buy` and `Side::Sell` did; the old `Side` had no `Display`, so no
  text changes.
- `Action` is `#[non_exhaustive]` and has `Other`. Every public entry turns
  it into a private `{Buy, Sell}` direction first, with the crate's only
  wildcard arm; anything else is the new
  `MarginError::UnsupportedAction { field, action }`, before any change:
  `Ledger::book_execution*` and `RiskGate::on_fill` (before the duplicate
  check), `MarginReservationService::reserve` (before the order id is used)
  `scenarios::stress` and `LiquidationPlan::intents` (every reduction, since
  plan fields are public). `RiskGate::submit` checks it first and rejects it
  as `RiskReason::InvalidOrder` without recording anything: the id is not
  consumed and an existing record under it is untouched. Every other
  rejection is still recorded. Matches inside the crate stay exhaustive.
- **MSRV 1.88** (required by `financial_types` 0.3).
- `financial_types::Side` (long or short), `OptionStyle` and
  `UnderlyingAssetType` are not adopted: positions are signed, and margin-rs
  has no option style or asset type enum to replace.

- Lints: clippy pedantic is on, and `unwrap_used`, `expect_used`, `panic`,
  `unreachable`, `todo`, `unimplemented` and `indexing_slicing` are denied;
  `arithmetic_side_effects` warns, and `make lint` runs with `-D warnings`.
  `unsafe_code` stays forbidden, and `missing_docs` and
  `clippy::float_arithmetic` stay denied.
  Library code has no `unwrap`, `expect`, `panic!`, unchecked indexing or
  unchecked arithmetic left; the few exceptions are `#[expect]`s with a
  reason. Test code opts out of the panic and indexing lints.
- Ledger balances compute `available = total - reserved` exactly when the
  balance changes. A balance whose available amount cannot be represented
  is now an `Arithmetic` error at booking time, instead of a panic or a
  rounded value when it is read. `Ledger::balance` and `Ledger::overdrafts`
  keep their signatures and read the stored value.
- Negations of `Decimal` amounts go through an exact sign flip, with no
  arithmetic.

- **Breaking:** `OrderRecord::remaining` returns `Result<Decimal>`. Its
  fields are public, so a record can hold any values, and a difference
  that cannot be represented is now `MarginError::Arithmetic` instead of a
  panic. Records kept by the `RiskGate` never fail; an overfilled record
  still gives zero.

No other public signature changes.
