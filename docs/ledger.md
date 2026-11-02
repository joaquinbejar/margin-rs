# Ledger contract (M1-04)

The `margin_rs::ledger::Ledger` is the single accounting authority for one account. Strategy, risk, backtest and environment code read cash, positions and fees from it and never keep a second balance that could drift.

The first supported product is spot with maker and taker fees. The types already carry what later derivative cashflows need: a fee asset separate from the quote, signed positions, and per-asset PnL.

## State

| Item | Key | Content |
|---|---|---|
| Balance | asset | `total`, `reserved`, `available = total - reserved` |
| Position | instrument | signed `quantity`, signed `cost_basis`, `realized_pnl` (in the quote asset) |
| Fees paid | asset | cumulative fees, where a rebate is a negative fee |
| Reservations | reservation id | `(asset, amount)` held back from available |
| Transfers | transfer id | deposits and withdrawals |
| Journal | booking order | one `ExecutionCashflow` per booked execution |
| Settlement journal | booking order | one `SettlementCashflow` per derivative settlement (M3-04) |
| Settled PnL | asset | cumulative realized PnL paid by derivative settlements, gross of fees |

`quantity` is the net quantity traded on the instrument since the ledger was opened. It is not the base asset balance. The base balance also moves with transfers and with fees charged in the base asset.

Example: a buy of 1 BTC with a fee of 0.001 BTC gives a position of 1 and a BTC balance of 0.999.

## Booking rules

1. **Idempotency.**
   - An execution is identified by its `ExecutionId`. If the identical execution is booked again, the call returns `Booking::Duplicate` with the original cashflow and nothing changes.
   - The same id with different content returns `ConflictingEntry`.
   - Transfers follow the same rule by `TransferId`.
2. **Atomicity.** Every operation either applies completely or returns an error and leaves the ledger unchanged.
3. **No overspending.** A booking, withdrawal or reservation that would leave an asset's available balance negative fails with `InsufficientBalance`.

   The one exception is `book_execution_allowing_overdraft`, used for executions that already happened at a venue (M1-05). It books the execution anyway and returns the shortfall. The overdraft stays visible in `overdrafts()` and in the `Reconciliation` returned by `reconcile()` until the account is funded.
4. **Precision.**
   - Each asset has a scale: its number of fractional digits, at most 18.
   - Each spot instrument has a price scale and a quantity scale, and the quantity scale may not exceed the base asset scale.
   - An input with more significant fractional digits than its scale allows fails with `PrecisionExceeded`. Inputs are never truncated. Trailing zeros do not count.
5. **Exact arithmetic.**
   - `rust_decimal` operations can silently drop digits when an exact result needs a mantissa wider than 96 bits or more than 28 fractional digits. The crate never relies on them for arithmetic.
   - Instead it works on the integer mantissas in 768-bit integers (`src/decimal/wide.rs`), so every intermediate is exact.
   - A sum, difference or product is either the exact result or `Arithmetic`, when that result cannot be represented. The ledger is unchanged in that case.
   - Products and ratios that are rounded (below) are computed exactly and rounded **once**, from the exact quotient and remainder. A result that can be represented at the target scale is therefore always produced, including with 18-digit price, quantity and asset scales.
   - Binary floating point is banned crate wide (`clippy::float_arithmetic = deny`).

## Rounding

A value is rounded only in the places below. Each rounding is a single step from the exact value, using the ledger's `Rounding` mode (half even by default; also half away from zero, toward zero, away from zero).

1. **Notional.** `price * quantity` is computed exactly and then rounded to the quote asset scale. The rounded value is the cash that changes hands. The cashflow records `notional`, and also `notional_exact` and `rounding_residual = notional - notional_exact` when a `Decimal` can hold the exact product. It cannot when price and quantity scales of 18 each give 36 fractional digits; then both fields are `None`.

   Fragmented fills therefore round once per fill. For example, three fills at 100.01 for 0.33333333, 0.33333333 and 0.33333334 cost 100.009999 in total, while a single fill would cost 100.01. The residuals sum to the difference, here -0.000001.

2. **Released cost.** When a fill reduces a position by `r` out of `|q|`, the cost basis released is `round(cost_basis * r / |q|)`, rounded to the quote asset scale. A fill that closes the position releases the whole remaining cost basis.

   The cost released over a full close therefore always sums exactly to the cost paid. Realized PnL is `-(fill value) - released`, so total realized PnL equals total quote cash exchanged by trades.

3. **Valuation.** `round(quantity * mark)`, described under the valuation policy.

When a fill crosses zero, it closes the old position at `round(price * |q|)`. The rest of the fill's rounded notional opens the new position, so the fill's cash is split without loss.

## Cashflow reconciliation

Each booked execution returns an `ExecutionCashflow` with:

- the booking sequence;
- the exact and rounded notional, and the rounding residual;
- the fee;
- the signed balance change of every asset touched;
- the realized PnL;
- the position after the fill.

`Ledger::reconcile()` recomputes the state from the transfers and the journal and checks:

- per asset: `total = deposits - withdrawals + sum(deltas)`;
- per asset: `reserved` equals the sum of open reservations, and `reserved <= total`;
- per asset: fees paid equal the sum of fees in the journal;
- per instrument: `quantity` equals the net traded quantity;
- per instrument: quote cash exchanged by trades equals `realized_pnl - cost_basis`. This checks that cost is conserved, and it is exact.

## Valuation policy

`Ledger::valuation(marks)` values open positions at caller-supplied marks:

- `market_value = round(quantity * mark)` to the quote asset scale;
- `unrealized_pnl = market_value - cost_basis`, which is positive for a short when the mark falls;
- realized PnL is gross of fees, and fees are reported per asset;
- `by_asset[a].net_pnl = realized + unrealized - fees`, counting only amounts in asset `a`. A fee paid in BNB appears in the BNB line, never in the USDC line;
- flat positions need no mark; an open position without one fails with `MissingMark`.

`quantity * mark` alone is a notional, not a PnL. market-maker-rs `BacktestEngine::run_with_progress` currently stores that notional in `pnl.unrealized` and the trade cash flow in `pnl.realized`. Their sum equals the equity change, but each field is mislabelled. This policy replaces both.

### Conversion: the only cross-asset operation

`Valuation::net_in(target, rates)` converts each asset's `net_pnl` with an explicit rate, defined as the price of one unit in `target`. It fails with `MissingConversionRate` when a rate is missing.

Precision: the products and their sum are computed exactly and rounded once to the target asset scale. The result is the exact converted sum rounded to that scale, so under half even it is within half a unit in the last place, for example ±0.0000005 USDC at scale 6. The rates are taken as exact. Apart from the explicit roundings above, nothing is approximate.

## Mapping from market-maker-rs (contract for the adapter)

The market-maker-rs side of this issue covers wiring `BacktestEngine` and `InventoryPosition::update_fill` to read from the ledger, and the reconciliation tests in market-maker-rs. It is tracked as the market-maker-rs integration tasks of M1-04 and is not part of this PR.

The adapter maps types as follows:

| market-maker-rs (`origin/main` a869711) | margin-rs |
|---|---|
| `ExecutionReport` with `is_fill()` | `Execution` |
| `stamp.sequence` (unique within a run) | `ExecutionId`: `"{run}:{sequence}"`, or the bare `u64` via `From<u64>` when one ledger serves one run |
| `side: execution::Side` | `ledger::Action`, from `financial_types`: `Buy` and `Sell`; `Other` is refused with `UnsupportedAction` |
| `price` | `price`, which must respect the instrument's price scale |
| `last_quantity` | `quantity`, which must respect the quantity scale |
| `fee` (`price * quantity * fee_rate`, unrounded) | `Fee { asset: quote, amount }`. The adapter rounds the amount to the fee asset scale with an explicit mode, because the ledger rejects finer amounts |
| `BacktestConfig::initial_capital` | `deposit(TransferId, quote, amount)` |
| `InventoryPosition { quantity, avg_entry_price }` | `PositionSnapshot { quantity, average_price() }`, with `cost_basis` as the authoritative value |
| `PnL { realized, unrealized }`, `total_fees` | `PositionSnapshot::realized_pnl`, `Valuation`, `fees_paid` |
| non-fill reports (`Accepted`, `Rejected`, `Canceled`, `CancelRejected`) | not booked; M1-05 maps them to reservation changes |

The currency, unit and cashflow contracts of market-maker-rs#78 (M0-05) are still open. Execution reports carry no currency yet, so the adapter supplies the instrument and fee asset from its configuration until #78 lands.

## Derivative positions

A linear future (`LinearFuture`) or perpetual (`LinearPerpetual`) registered in the ledger is held as signed contracts `Q` with a signed **basis**: the cash value of the open contracts at their entry prices, in the settlement asset (a long has a positive basis). With multiplier `M`, settlement asset scale `c` and `rhe` the half-even rounding to `c` digits:

| Figure | Rule |
|---|---|
| rounding | always half-even; `LedgerConfig::rounding` applies to spot only, so the ledger and `plan_liquidation` agree to the unit |
| virtual notional of `n` at `p` | `N = rhe(p * n * M)`; never exchanged in cash |
| open or increase | `basis += N` for a buy, `-N` for a sell; only the fee moves cash |
| reduce `n <= abs(Q)` | `released = rhe(basis * n / abs(Q))`, the whole basis on a full close; realized `= +N - released` (sell) or `-N - released` (buy) |
| cross zero `n > abs(Q)` | `C = rhe(p * abs(Q) * M)`, `O = N - C` exactly (no second rounding); realized `= +C - basis` (sell) or `-C - basis` (buy); new basis `-O` (sell) or `+O` (buy) |
| cash | `+ realized - fee`, in the settlement asset |
| market value at `mark` | `rhe(Q * M * mark)`; unrealized PnL `= market value - basis` |

A crossing or a partial reduction can leave an open position with a zero basis (for example a crossing whose opening part rounds to zero); that is a valid state.

`N`, `C` and `O` stay in wide integers: only the stored results (realized PnL, basis, cash, contracts) must fit a `Decimal`, so a fill whose virtual notional is out of range still books when they fit. `ExecutionCashflow::notional` is zero for a derivative (no principal is exchanged): the real cash is in `deltas`, and the virtual notional is in `ExecutionCashflow::derivative` (`None` there only when it is out of a `Decimal`'s range).

- **Every execution is mandatory**: it is booked with `book_execution_allowing_overdraft`, never rejected for balance, and a shortfall is returned. `book_execution` and `book_execution_with_reservation` refuse a derivative with `InvalidInput` and no effect; a derivative fill consumes no reservation.
- **Funding** of a perpetual: `book_funding(&FundingPayment { id, instrument, amount, as_of_ms })`. A positive amount is received, a negative one paid; the balance changes by `amount`, which joins `Ledger::funding` and counts once as realized PnL in valuation. A payment with a flat position is valid. Mandatory, with its shortfall.
- **Expiry** of a future: `settle_expiry(&ExpirySettlement { id, instrument, price, as_of_ms })`, with `as_of_ms` at or after the expiry, closes the position at `price` with the closing rules and no fee. It is terminal: the instrument cannot be settled again. The ledger reads no clock; every derivative execution carries `executed_at_ms` (required, same unit and epoch as `expiry_ms`), and a future's must be strictly before the expiry: at or after it, the fill is causally impossible and refused with `ExecutedAfterExpiry`, whether or not the future was settled. A fill executed before the expiry but delivered after the settlement (a late fill) is still booked, never rejected: it is closed at once at the settlement price, the position stays flat, its realized PnL is the fill against that price, and `DerivativeFill::closed_at_settlement` flags it for the caller to investigate. The virtual identity includes both legs. A flat future settles with no cash.
- **Ids**: executions, settlements, funding payments and expiry settlements share one id space. An identical repeat is a duplicate before any other check; the same id with other content is `ConflictingEntry`.
- **Reconciliation**: per asset, the real cash (transfers, execution deltas, settlements, funding, expiries); per derivative instrument, the contracts and the virtual identity `sum of signed virtual notionals (-N buy, +N sell) = realized_pnl - basis`, recomputed in wide integers from every fill.

`valuation::value` and `collateral::value_collateral` value open derivative positions at instrument marks (see `docs/valuation.md`); the stress scenarios do not shock instrument marks and refuse an open derivative with `UnsupportedInstrument`. The risk gate handles spot orders only and refuses a derivative with `UnsupportedInstrument` at every entry point.

## Derivative settlements outside the ledger (M3-04, legacy)

This path is kept for derivatives that are not registered in the ledger; registered futures and perpetuals use the rules above. It may be removed later. Futures and perpetual positions held outside the ledger are margined by `initial_margin`, `margin_reservations` and `liquidation`. When such a position is reduced, the cash it moves is booked with `book_settlement(&DerivativeSettlement { id, instrument, asset, realized_pnl, fee })`:

- the asset's total balance changes by `realized_pnl - fee`, one exact difference;
- the fee joins `fees_paid` and the realized PnL joins `settled_pnl`, so `Ledger::valuation`, `valuation::value` and its PnL attribution count it as realized PnL;
- it is **idempotent by execution id**, and the id space is shared with executions, funding payments and expiry settlements: the same id booked again identically is a duplicate (`is_new == false`, nothing changes); the same id with other content, or already used elsewhere, is `ConflictingEntry`;
- the instrument must not be registered in the ledger, and an id that has been settled cannot be registered later as one;
- it is a **mandatory debit**: never rejected for lack of balance, deferred or reduced. A settlement that leaves the balance below its reservations, or negative, is booked in full and returns the `shortfall`; the debt stays visible in `overdrafts()` and in `reconcile()`.

`reconcile()` adds every settlement delta to its asset's total, its fee to `fees_paid` and its realized PnL to `settled_pnl`, and checks `delta = realized_pnl - fee` for each one.

## Limits and non-goals

- Options are not ledger positions. Account valuation, stress, liquidation and margin reservations do not read derivative positions from the ledger yet.
- The journal and the set of seen execution ids grow without bound for the life of a ledger. Pruning or checkpoints are future work.
- Reservations are plain holds that a caller creates and releases. M1-05 builds order-level reservations on top: release on fill, reject and effective cancel, and exactly-once adjustment on a partial fill.
