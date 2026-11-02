# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

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
