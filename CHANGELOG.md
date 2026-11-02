# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- Lints: clippy pedantic is on, and `unwrap_used`, `expect_used`, `panic`,
  `unreachable`, `todo`, `unimplemented` and `indexing_slicing` are denied;
  `arithmetic_side_effects` and `float_arithmetic` warn, and `make lint`
  runs with `-D warnings`. `unsafe_code` is denied and `missing_docs` warns.
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

No public signature changes.
