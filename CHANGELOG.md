# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `LiquidationPolicy::fee_rates`, a fee rate per instrument in place of
  `fee_rate`, and `LiquidationPolicy::fee_rate_for` (#55).
- `LiquidationPolicy::loss_budget` and `CloseoutOutcome::BudgetExhausted`: the
  plan never brings its projected equity below `-B` (#54).
- `initial_margin::AccountMargin`: `plan_liquidation` and
  `LiquidationAccount::from_ledger` take any rule evaluated on the whole
  projected account, including one that nets across positions. Every
  `PiecewiseLinearMargin` implements it, with unchanged plans (#52).

### Changed

- `LiquidationPolicy` is no longer `Copy`.

## [0.2.0] - 2026-11-06

Options in the ledger: positions, premium, settlement, valuation and the short
side's margin. The ledger takes marks and outcomes as inputs and prices nothing
(#49). See `docs/options.md`.

### Added

- `instrument::OptionInstrument`, `OptionContract`, `OptionRight`,
  `ExerciseStyle`, `OptionDelivery` and `OptionMargining`;
  `Instrument::Option`, `Instrument::option_terms` and `PositionKind::Option`.
- Option fills through `Ledger::book_execution_allowing_overdraft`: the premium
  moves cash at the fill. A premium of zero is valid.
- `Ledger::settle_option` and `RiskGate::settle_option`: closes contracts by a
  cash price, a delivery booked atomically as a spot execution at the strike, or
  a lapse. `Ledger::option_settlement_journal`.
- Rule `short-option-im/1`: `rules::ShortOptionIm`, `ShortOptionParameters`,
  `OptionExposure` and `OptionExposureView`. It is this crate's own
  specification, not a venue's.
- `DerivativeValue::is_option`.

### Changed

- **Breaking:** `Instrument::from_spec` now converts option specifications, where
  it returned `MarginError::UnsupportedSpec`. Its checks now start with the
  scales for options too.
- **Breaking:** `DerivativeValue` has the new public field `is_option`.
- `Ledger::reconcile` checks option settlements. `ExecutionCashflow::notional` is
  the premium for an option.
- Valuation: an option's mark may be zero. Reporting equity adds its market value
  and collateral counts `min(market value, 0)`; a short at an invalid mark makes
  collateral unknown.

### Not covered

These refuse an open option with `UnsupportedInstrument` instead of leaving it
out:

- `scenarios::stress`;
- `LiquidationAccount::from_ledger`;
- `MarginReservationService` on a ledger, for an option settled in its currency;
- the risk gate's orders, which remain for spot.

## [0.1.1] - 2026-11-04

### Added

- Feature `instruments` (off by default; enables
  `financial_types/instruments`): `Instrument::from_spec` and
  `TryFrom<(&InstrumentSpec, InstrumentScales)>` convert a
  `financial_types` instrument specification (spot, linear and inverse
  futures and perpetuals) into an `Instrument`, with explicit
  `instrument::InstrumentScales` and an exact field mapping. New
  `MarginError` variants under the feature: `UnsupportedSpec` (options)
  and `ScaleBelowGrid` (a scale below the digits of the price ticks or the
  lot). See `docs/instrument-specs.md` (#45).

### Changed

- `financial_types` floor raised to 0.3.1.

## [0.1.0] - 2026-11-03

First release. It describes the crate as it is; the changes made before
the first release are not listed one by one. The earlier 0.0.1 on
crates.io only reserved the name.

### Added

#### Accounting (`ledger`)

- `Ledger`: the single accounting authority for one account. Balances per
  asset split into total, reserved and available; spot positions with
  signed average cost and realized PnL; fees per asset (rebates as
  negative fees); per execution cashflows; snapshots (`LedgerSnapshot`,
  self contained with assets, instruments and net transfers); valuation by
  asset; and a full `reconcile()` of the state from the journal.
- Idempotent booking by execution and transfer id: the same id with the
  same content is a duplicate that changes nothing, and with other content
  it is `ConflictingEntry`. Every operation is atomic.
- Linear futures and perpetuals (`LinearTerms`, `LinearFuture`,
  `LinearPerpetual`) and inverse futures and perpetuals (`InverseTerms`,
  `InverseFuture`, `InversePerpetual`), with `ContractSize` (a multiplier
  for a linear contract, a contract value `CV` in quote units for an
  inverse one). A position is signed contracts with a signed basis in the
  settlement asset; only realized PnL and fees move cash. Every figure is
  rounded half-even once to the settlement asset scale, and intermediate
  notionals stay in wide integers.
- Inverse contracts settle in the base asset: a fill of `n` contracts at
  `p` is worth `V = rhe(n * CV / p)`, the basis is the signed entry value,
  and the realized PnL is `Q * CV * (1/entry - 1/exit)`. The exact rest of
  each value is recorded as an `InverseResidual`.
- `Ledger::book_funding` (`FundingPayment`) and `Ledger::settle_expiry`
  (`ExpirySettlement`), in the id space shared with executions.
- Mandatory postings: a derivative execution, a funding payment, an expiry
  and a derivative settlement are booked even when they overdraw the
  account (`book_execution_allowing_overdraft`), and the shortfall is
  reported and stays visible in `overdrafts()` and `reconcile()`. A real
  fill executed before a future's expiry but delivered after its
  settlement is booked, closed at the settlement price and flagged
  (`DerivativeFill::closed_at_settlement`); a fill executed at or after the
  expiry is `ExecutedAfterExpiry`.
- `book_settlement` (`DerivativeSettlement`) posts the cash of derivatives
  that are not registered in the ledger (legacy path).
- `reconcile()` checks balances, fees, transfers and funding per asset,
  and per derivative the contracts and, in wide integers, the identity
  `sum of signed virtual notionals (-N buy, +N sell) = realized_pnl -
  basis` for a linear contract and `sum of signed virtual values (-V buy,
  +V sell) = -realized_pnl - basis` for an inverse one.

#### Pretrade risk (`risk`)

- `RiskGate`, which owns the ledger: every spot order passes through
  `submit` and reserves what it could spend, sized so that no
  fragmentation of fills can exceed it. Reservation terms are fixed per
  order at admission.
- Hard limits on the joint exposure of positions and live orders
  (`InstrumentLimits`): order size, position, notional, open orders, price
  band and market data age. Typed decisions (accepted, reduced, rejected)
  with every reason.
- Fees in the quote or the base asset (`InstrumentLimits::fee_asset`,
  `FeeAsset`): a fee in the asset an order pays is reserved with it, up to
  `max_fee_rate`; a fee in the asset it receives is netted from that credit
  in the same booking. A real fee above the bound, or in another asset, is
  booked from the free balance.
- Declared fee rounding for fees in base (`InstrumentLimits::fee_rounding`,
  `FeeRounding::PerFill { mode, min_fill }`, the default rounding every
  fill up, or `FeeRounding::Cumulative { mode, rate }` at an exact rate):
  a sell with fees in base reserves its quantity plus the declared fee
  bound instead of the worst case (for 2 BTC at a 1e-8 lot and 0.001: 4
  by default, 3.002 with a half mode, 2.002 toward zero or cumulative).
  A fill outside the declaration is booked and reported in
  `FillOutcome::fee_breaches`; the first breach of each declaration
  (`RiskGate::fee_declaration_breaches`) rejects new orders under it with
  `RiskReason::FeeDeclarationBreached` until
  `RiskGate::reset_fee_declaration`.
- Every reservation is the hold, the smallest Decimal at least the exact
  requirement on the asset grid (equal to it whenever it is a Decimal),
  with counts and bounds in wide integers; affordability is exact, by an
  integer bisection over lot counts.
- Exactly once reservation updates on fills, venue rejects and confirmed
  cancels. A real fill that does not match its order (overfill, fill after
  filled, worse price) is booked and halts the gate as an
  `ExecutionAnomaly`; one that overdraws the account halts it as an
  `AccountingBreach`.
- Circuit breaker (`trip`, `resume`) with quote withdrawal, also when the
  gate halts itself.
- `book_derivative_execution`, `book_funding` and `settle_expiry` post
  derivative postings through the gate.

#### Valuation and stress (`valuation`, `scenarios`)

- `valuation::value`: mark to market of a `LedgerSnapshot` in one
  reporting asset, with price source and freshness flags, and a PnL
  attribution into realized, unrealized and fees, each counted once.
- Collateral equity that an invalid mark can only lower or make unknown,
  never raise; debts always count.
- Open futures and perpetuals are valued at instrument marks
  (`MarkSet::instruments`): `upnl = rhe(Q * M * mark) - basis` for a linear
  contract and `basis - rhe(Q * CV / mark)` in the base for an inverse one.
  Collateral counts `cash + sum(upnl)` per settlement asset as one net
  amount.
- `scenarios::stress`: stress of spot inventory and open linear futures and
  perpetuals with hypothetical fills (hedges, spot or derivative), asset
  and instrument shocks and a liquidation spread, from the baseline marks,
  which are never changed. Each stressed price is one exact value.
  `StressResult` reports ignored shocks, unshocked instruments and assets
  where the spread side is not conservative for reporting equity.

#### Margin (`initial_margin`, `rules`, `factors`, `covariance`, `collateral`, `margin_reservations`)

- `InitialMarginModel` and versioned rules with progressive tier tables,
  every requirement carrying its `RuleId` and a per position breakdown:
  `TieredLinearIm` (`linear-im/1`: initial 10 %, maintenance 5 % of
  `|Q| * M * mark`) and `TieredInverseIm` (`inverse-im/1`: the same rates
  on `|Q| * CV / mark`, in the base). Each margin is one exact sum rounded
  up once; notionals are informational and never block a representable
  margin. `PiecewiseLinearMargin::margin_tiers` exposes the tiers that give
  the unrounded margin exactly.
- Factor exposures with explicit units (`factors`) and portfolio variance
  under a validated, exactly positive semidefinite covariance matrix
  (`covariance`).
- `collateral::value_collateral`: collateral with per asset haircuts,
  eligibility, a choice of balance basis and a declared stale price policy,
  including the unrealized PnL of open derivatives per settlement asset.
- `MarginReservationService`: initial margin reserved for pending
  derivative orders, linear or inverse, with its own positions (`new`) or
  reading them from the gate's ledger (`on_ledger`, where the hold covers
  every open derivative of the account in the settlement asset). Spot and
  margin reservations share one cash pool, so no unit of cash is reserved
  twice. Risk reducing orders are accepted when the account is under
  margined, and a hold never goes negative.

#### Maintenance and liquidation (`maintenance`, `liquidation`)

- `maintenance::assess`: account state (healthy, reduce only, default)
  from the maintenance threshold; equality is healthy, unknown figures
  never are. `CollateralView::from_valuation` builds its input.
- `plan_liquidation`: a staged closeout plan that cancels resting orders,
  reduces positions (linear or inverse) against scenario liquidity with
  the smallest closeout that restores the target, and reports what is left
  (unfilled size, deficit and debt). `LiquidationPlan::intents` gives the
  adapter intents.
- Certified search: the smallest count of each level is found as the
  ledger would book it, even where rounding to a coarse cash quantum opens
  windows the unrounded figures do not have. Exact bounds in wide integers
  limit an exact walk, which shares a budget of `SEARCH_BUDGET` (4096)
  evaluations per plan. Beyond it the plan takes the first count certified
  to meet the target, and `LiquidationPlan::minimal` is `false` when that
  count is not proven the smallest; with no certified count the plan
  fails with `MarginError::CloseoutSearchLimit`. Counts that would leave a position
  no `Decimal` can hold are skipped arithmetically.
- A `contract_step` whose multiples below a level's size (or the
  position's) are not exact at the step's scale is refused with
  `InvalidInput`.
- `LiquidationAccount::from_ledger` reads the account from a ledger
  snapshot, and `ForcedReduction::execution` books each forced fill back as
  a mandatory execution, so the booked plan leaves exactly
  `LiquidationPlan::after`. `ForcedReduction::settlement` serves positions
  kept outside the ledger.

#### Exact arithmetic and error boundary

- Money, prices and quantities are `rust_decimal::Decimal`. Every
  operation is checked and exact or a typed `MarginError::Arithmetic`;
  sums and products of more than two terms are computed in 768 bit wide
  integers with one final conversion or rounding. The only roundings are
  explicit, with a declared `Rounding` mode.
- `MarginError` and `Result`: the typed error boundary.
- The order and execution direction is `financial_types::Action` (0.3),
  re-exported as `margin_rs::Action`; a direction other than `Buy` or
  `Sell` is `MarginError::UnsupportedAction`, before any change.
- Lints: clippy pedantic, with `unwrap_used`, `expect_used`, `panic`,
  `unreachable`, `todo`, `unimplemented`, `indexing_slicing` and
  `float_arithmetic` denied, `arithmetic_side_effects` as an error under
  `make lint`, and `unsafe_code` forbidden. The few exceptions are
  `#[expect]`s with a reason.
- Edition 2024, MSRV 1.88; CI runs the latest stable and the MSRV.

### Known limitations

- Inverse futures and perpetuals are valued, margined and closed out, but
  not stressed: `scenarios::stress` refuses an open inverse position, a
  shock on one or a hypothetical fill on one with `UnsupportedInstrument`.
- The risk gate handles spot orders only; derivative orders reserve margin
  through `margin_reservations`.
- A booking whose resulting ledger balance or position has no Decimal form
  (more significant digits than a Decimal holds) fails with an arithmetic
  error and changes nothing; reservations then cover every state the
  ledger can book.
- Options are not covered by the ledger, the margin rules or the closeout.
- The journal and the set of seen ids grow for the life of a ledger;
  pruning and checkpoints are future work.
- market-maker-rs uses margin-rs optionally through its `margin` feature,
  pinned by git to an earlier revision; updating it to this release is
  pending.

[Unreleased]: https://github.com/joaquinbejar/margin-rs/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/joaquinbejar/margin-rs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/joaquinbejar/margin-rs/releases/tag/v0.1.0
