# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Margin for inverse futures and perpetuals** (#26, part B):
  `ProductKind::InverseFuture` / `ProductKind::InversePerpetual` and the
  rule `TieredInverseIm` (`inverse-im/1`: 10 % / 5 % of the notional
  `|Qc| * CV / mark` in the base; tier tables with floors in the base).
  Tiers are compared exactly by cross products and each margin is one
  exact sum over the denominator `mark`, rounded up once; the linear rule
  computes its margins the same way, so a notional out of range no longer
  blocks a representable margin. Margin reservations margin and value
  inverse positions (own or on the ledger).

- **Inverse futures and perpetuals in the ledger** (#26, part A):
  `InverseTerms` (settlement in the base asset, contract value `CV` in
  quote units), `InverseFuture`, `InversePerpetual`, the
  `Instrument::InverseFuture` / `Instrument::InversePerpetual` and
  `PositionKind` variants, `ContractSize` and
  `Instrument::{contract_size, inverse_terms, expiry_ms}`. A fill is worth
  `V = rhe(n * CV / p)` in the base; the basis is the signed entry value
  and the realized PnL `Q * CV * (1/entry - 1/exit)`. Funding, expiry and
  late fills follow the linear rules. Each value's exact rest is recorded
  as an `InverseResidual` (`DerivativeFill::residual`,
  `DerivativeFill::closing_residual`). `valuation::value`,
  `collateral::value_collateral` and `Ledger::valuation` value inverse
  positions (`upnl = basis - rhe(Q * CV / mark)`). Margin reservations on
  the ledger, `LiquidationAccount::from_ledger`, the stress scenarios and
  the risk gate refuse an open inverse position with
  `UnsupportedInstrument` until parts B and C. See `docs/ledger.md`.

- **Stress of open futures and perpetuals** (#31): `scenarios::stress`
  values open linear futures and perpetuals at stressed instrument marks
  instead of refusing them. `Scenario::instrument_shock` moves an
  instrument mark by `1 + shock`; the liquidation spread closes a long at
  `mark * (1 - spread)` and a short at `mark * (1 + spread)`, and takes the
  side of a settlement asset from the net collateral counts for it (cash
  plus the unrealized PnL at valid marks). Each stressed price is one exact
  product. Hypothetical fills on futures and perpetuals use the ledger's
  derivative booking at the valuation time (a future at or past expiry is
  `ExecutedAfterExpiry`, a settled one `InvalidInput`). `StressResult`
  reports `ignored_instrument_shocks`, `unshocked_instruments` (open
  derivatives without a shock) and `spread_sign_conflicts` (assets where
  the spread side chosen for collateral is not conservative for reporting
  equity). The baseline marks, snapshot and ledger are never changed.

- **Liquidation and margin reservations read the ledger** (#23, part C):
  `LiquidationAccount::from_ledger` builds the closeout account from a
  `LedgerSnapshot` (every open future and perpetual in the plan's currency,
  the total cash, the ledger's basis; spot left out; derivatives in another
  asset, missing or non positive marks and expired unsettled futures are
  errors), and
  `ForcedReduction::execution` books each forced fill back as a mandatory
  `Execution`, so the booked plan leaves exactly `plan.after`.
  `MarginReservationService::on_ledger` reads positions from the gate's
  ledger, margins and values every open derivative in the settlement asset
  (registered or not; its hold covers the whole account in that asset),
  uses its own fixed hold, and takes fills
  already booked (`apply_booked_fill`, which flags overfills and sets the
  hold atomically, returning `BookedFill`; `requirement_with_gate`).
  `RiskGate::book_derivative_execution`, `RiskGate::book_funding` and
  `RiskGate::settle_expiry` post derivative executions, funding and expiries
  through the gate. New errors: `MarginError::FillNotBooked`,
  `MarginError::WrongReservationMode` and `MarginError::ExpiredUnsettled`.

- **Valuation and collateral of open futures and perpetuals** (#23, part
  B): instrument marks (`MarkSet::instruments`, `MarkSet::with_instrument`;
  per unit of the underlying in the settlement asset). `valuation::value`
  values each open derivative at `upnl = rhe(Q * M * mark) - basis`
  (`AccountValuation::derivatives`, `DerivativeValue`), adds it to equity
  and to the attribution's unrealized PnL, and counts `cash + sum(upnl)`
  per settlement asset as one net amount in collateral equity; stale or
  future instrument marks are flagged (`AccountValuation::instrument_flags`)
  and never add collateral (a gain is excluded, a loss makes it unknown).
  `collateral::value_collateral` applies the same net per asset
  (`CollateralLine::unrealized_pnl`, `CollateralValue::instrument_flags`,
  `MarginError::InvalidInstrumentMark` under `StalePolicy::Reject`).
  `maintenance::CollateralView::from_valuation` builds the maintenance view
  from a valuation.

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
  `Ledger::funding_journal`, `Ledger::expiry_journal` and
  `Ledger::is_settled`. A real fill executed before a future's expiry but
  delivered after its settlement is still booked, never rejected: it is
  closed at once at the settlement price and flagged in
  `DerivativeFill::closed_at_settlement`. A fill executed at or after the
  expiry is causally impossible: `MarginError::ExecutedAfterExpiry`.
- `ExecutionCashflow::derivative` (`DerivativeFill`: multiplier, virtual
  and closing notionals, released basis, settlement price of a late fill),
  `PositionKind`, and
  `Instrument::{kind, multiplier, linear_terms}`.
- `reconcile()` checks funding per asset and, per derivative, the contracts
  and the virtual identity `sum of signed virtual notionals = realized_pnl -
  basis` in wide integers.

### Changed

- **Breaking:** `ExposurePosition::multiplier` and
  `MarginInstrument::multiplier` are replaced by `size: ContractSize`,
  validated against the product. `PositionRequirement::notional` and
  `TierSlice::notional` are `Option<Decimal>` (informational, `None` when
  they do not fit). An unrepresentable margin is reported as
  `Arithmetic { operation: "initial margin" }` (or maintenance) instead of
  failing on the notional.
- **Breaking:** `PositionSnapshot::multiplier` and `DerivativeFill::multiplier`
  are replaced by `size: ContractSize`, and `Instrument::multiplier` by
  `Instrument::contract_size`; `DerivativeFill` has the new fields
  `residual` and `closing_residual`. `Instrument` and `PositionKind` have
  new variants (both are `non_exhaustive`). Registering an inverse
  instrument requires the base scale plus its price scale to be at most 28.
- **Breaking:** `Scenario` has a new field, `instrument_shocks`, and
  `StressResult` three (`ignored_instrument_shocks`,
  `unshocked_instruments`, `spread_sign_conflicts`): struct literals must
  name them (#31).
- **Breaking:** `Execution` has the new field `executed_at_ms` (and
  `Execution::with_executed_at`); it is part of the execution, so a repeat
  must carry the same time. It is optional for spot and required for a
  linear future or perpetual; a future's fills must be executed strictly
  before its expiry.
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
- **Breaking:** `MarkSet` has the new field `instruments`,
  `AccountValuation` the new fields `derivatives` and `instrument_flags`,
  `CollateralLine` the new field `unrealized_pnl` and `CollateralValue` the
  new field `instrument_flags`. `valuation::value` and
  `collateral::value_collateral` need an instrument mark for every open
  derivative (`MissingMark` otherwise, never zero); `AssetValue::collateral_value`
  and `CollateralLine::balance` include the unrealized PnL counted for the
  asset. The stress scenarios refuse an open derivative with
  `UnsupportedInstrument` (they shock asset marks only).
- The risk gate refuses derivatives with `UnsupportedInstrument` in
  `set_limits`, `on_market_data` and `submit` (as `RiskReason::InvalidOrder`).
- `book_settlement` is kept for derivatives that are not registered in the
  ledger (legacy; documented, not deprecated in code).
- `plan_liquidation` accepts a basis of zero (an entry notional rounded to
  zero); a non zero basis must still have the sign of the position.
- `MarginReservationService::reserve_with_gate` refuses an order side other
  than `Buy` or `Sell` before reading the gate, and a hold in another asset
  than the settlement asset is `CurrencyMismatch`.

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
