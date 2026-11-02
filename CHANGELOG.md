# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Linear futures and perpetuals in the ledger** (#23, part A):
  `LinearTerms`, `LinearFuture`, `LinearPerpetual` and the
  `Instrument::LinearFuture` / `Instrument::LinearPerpetual` variants. A
  position is signed contracts with a signed basis in the settlement asset;
  only realized PnL and fees move cash. Every figure is rounded half-even to
  the settlement asset scale (`LedgerConfig::rounding` applies to spot only),
  and the virtual notionals stay in wide integers, so only stored results
  must fit a `Decimal`. See `docs/ledger.md`.
- `Ledger::book_funding` (`FundingPayment`, `FundingCashflow`, positive is
  received) and `Ledger::settle_expiry` (`ExpirySettlement`,
  `ExpiryCashflow`), mandatory and idempotent in the id space shared with
  executions and settlements; `PostingBooking`, `Ledger::funding`,
  `Ledger::funding_journal`, `Ledger::expiry_journal`, `Ledger::is_settled`
  and `MarginError::Expired`.
- `ExecutionCashflow::derivative` (`DerivativeFill`: multiplier, virtual
  and closing notionals, released basis), `PositionKind`, and
  `Instrument::{kind, multiplier, linear_terms}`.
- `reconcile()` checks funding per asset and, per derivative, the contracts
  and the virtual identity `sum of signed virtual notionals = realized_pnl -
  basis` in wide integers.

### Changed

- **Breaking:** an execution of a linear future or perpetual is mandatory:
  `book_execution` and `book_execution_with_reservation` refuse it with
  `InvalidInput` and no effect, and `book_execution_allowing_overdraft`
  books it (never rejected for balance) and returns the shortfall.
- **Breaking:** `ExecutionCashflow` has the new field `derivative`. For a
  derivative, `notional` is zero (no principal is exchanged) and
  `notional_exact` and `rounding_residual` are `None`: its cash is in
  `deltas` and its virtual notional in `derivative.virtual_notional`
  (`None` only when out of a `Decimal`'s range). A consumer that adds
  notionals as volume must read the virtual notional for derivatives.
- **Breaking:** `PositionSnapshot` has the new fields `kind` and
  `multiplier` (1 for spot), and `average_price()` is `cost_basis /
  (quantity * multiplier)`: the price per unit of the underlying for a
  derivative. `LedgerSnapshot` has the new fields `funding` and
  `settled_instruments`. Code that builds these structs must set them.
- `valuation::value` and the stress scenarios refuse an open derivative
  position with `UnsupportedInstrument` (its unrealized PnL needs
  instrument marks, part B); flat ones, their realized PnL and funding are
  valued. The risk gate refuses derivatives with `UnsupportedInstrument` in
  `set_limits`, `on_market_data` and `submit` (as `RiskReason::InvalidOrder`).
- `book_settlement` is kept for derivatives that are not registered in the
  ledger (legacy; documented, not deprecated in code).

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
