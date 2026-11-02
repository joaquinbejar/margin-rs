# Pretrade risk gate

`margin_rs::risk::RiskGate` stands between every order source (quotes, hedges, policy proposals) and the venue. It owns the `Ledger`, so reservations live in the same accounting authority as cash and positions.

## Mandatory path

- `RiskGate::new(ledger)` takes the ledger by value. Afterwards the gate only lends `&Ledger`, so code outside the gate cannot book, reserve or release around it. A `compile_fail` doctest checks this.
- The only ways to change state are the gate's own methods:
  - `submit`;
  - `on_venue_accepted`, `on_venue_rejected`;
  - `request_cancel`, `on_cancel_confirmed`;
  - `on_fill`;
  - `trip`, `resume`;
  - `deposit`, `withdraw`;
  - `book_derivative_execution`, `book_funding`, `settle_expiry`: postings of futures and perpetuals (linear or inverse), which are mandatory and do not go through orders (a spot execution is refused by `book_derivative_execution`).
- Every order is decided again on every submission. A policy that keeps proposing an invalid order gets it rejected every time.
- An order id is recorded together with its decision, including when the decision is a rejection. A repeated id is rejected with `DuplicateOrderId` and never reserves twice.

## Decision

`submit(request, now_ms)` returns a `RiskDecision`:

| Decision | Meaning |
|---|---|
| `Accepted { quantity }` | Approved in full; the reservation is held. |
| `Reduced { requested, quantity, reasons }` | Approved for less. Only happens when the request is `reducible()`. |
| `Rejected { reasons }` | Nothing is reserved. |

Reasons are typed (`RiskReason`, `#[non_exhaustive]`). The checks run in this order:

1. **Hard checks.** All reasons found are reported together:
   - `DuplicateOrderId`, which returns immediately;
   - `Halted { reason }`;
   - `UnknownInstrument`;
   - `InvalidOrder { error }`: a non-positive price or quantity, or a value finer than the tick or lot;
   - `NoMarketData`, `LookAheadMarketData { mark_ms, now_ms }` (a mark timestamped after the decision time) or `StaleMarketData { age_ms, max_age_ms }`, when `max_data_age_ms` or `price_band` is set;
   - `PriceOutsideBand { mark, band }`, when `price_band` is set. The check `|price - mark| > band * mark` is decided exactly;
   - `MaxOpenOrders { limit }`.
2. **Quantity caps.** Each cap that binds is reported:
   - `MaxOrderQuantity { limit }`;
   - `MaxPosition { limit, headroom }`;
   - `MaxPositionNotional { limit, headroom }`;
   - `InsufficientBalance { asset, required, available }`.
   The allowed quantity is the smallest cap, floored to the lot.
3. **Minimum quantity.** If the allowed quantity is below the minimum (by default one lot), the order is rejected with `BelowMinimumQuantity`.
4. **Reduce or reject.** If the allowed quantity is below the request and the request is not reducible, the order is rejected with the cap reasons.

### Joint exposure

The position limits apply to the worst-case position on the order's side:

- buy: `position + open buys + q <= max_position`;
- sell: `position - open sells - q >= -max_position`.

"Open" counts every live order: `PendingNew`, `Open`, and `CancelPending`, since an order with a cancel pending can still fill. `max_position_notional` uses the same rule with the bound `floor(max_position_notional / price)`.

## Reservations

Every approved order gets `ReservationTerms`, fixed at admission and stored in its `OrderRecord`. Later `set_limits` calls never change the reservation of a live order, so lowering `max_fee_rate` or changing `fee_asset` cannot release budget a live order may still need.

An order reserves the asset it pays: quote for a buy, base for a sell. `InstrumentLimits::fee_asset` (`FeeAsset::Quote` by default, or `FeeAsset::Base`) says where the venue charges fees. A fee in the asset the order pays is reserved. A fee in the asset the order receives is netted: the ledger books the credit and the fee of a fill in one exact sum per asset, so the fill pays its own fee and never draws on balance held for other orders.

The reservation for `remaining` is `round_up(remaining / lot * per_lot)` on the asset grid.

- **Sell.** With fees in quote, `per_lot = lot` of the base asset, so the reservation is the remaining quantity itself; the fee is paid from the proceeds. With fees in base, `per_lot = lot + round_up(r * lot)` on the base grid, `r = max_fee_rate`, assuming that a fill's fee is at most `r` times its quantity, rounded up to the base grid. A fill of `k` lots is charged at most `round_up(r * k * lot) <= k * round_up(r * lot)`, because rounding up is subadditive. The lot is on the base grid (quantity scale at most the base scale), so this single bound is never above the exact one, `lot * (1 + r) + ulp`.
  - **Cost on a fine lot.** When the lot is the base grid itself (1e-8 BTC on an 8 digit grid), any positive rate rounds up to one unit per lot, so `per_lot = 2 * lot`: a sell of 2 BTC reserves 4 BTC. That is the worst case of the round up fee model (every one lot fill charged one unit), the default. A venue can declare how it rounds instead (below).

### Declared fee rounding (fees in base)

`InstrumentLimits::fee_rounding` declares how the venue rounds fees charged in the base asset; only `FeeAsset::Base` accepts a value other than the default, and it is fixed per order at admission (`ReservationTerms::base_fee`).

- `FeeRounding::PerFill { mode, min_fill }`: each fill of `n` is charged at most `round(r * n)` on the base grid in `mode`, `r = max_fee_rate`; with `min_fill` (positive, a multiple of the lot) every fill but the one completing the order is at least `min_fill`. The default is `PerFill { AwayFromZero, None }`, the bound above. A sell reserves `round_up(R + min(A, B))` for a remaining `R`, with `A = (R / lot) * round_up(r * lot)` (every fill rounded up, valid for any mode) and `B = r * R + h * ceil(R / m)`, where `h` is one grid unit away from zero, half of it in a half mode and 0 toward zero, and `m` is the minimum fill (the lot without one). A fill of at least `m`, or the last, frees at least its fee under each bound, so under their minimum too: when the minimum switches from `A` to `B`, the fall is `A(R) - B(R - n) >= A(R) - A(R - n)`, and the other way round. With the default `A <= B`.
- `FeeRounding::Cumulative { mode, rate }`: the order is charged on its cumulative quantity at an **exact** rate, fixed per order (`max_fee_rate` is not used): after a cumulative `c` the venue has charged exactly `round(rate * c)`. A sell of `Q` with `filled` so far (the real cumulative, late fills and overfills included) reserves `R + round(rate * Q) - round(rate * filled)`, exactly the fees still to charge, and nothing once `R` is 0. Rounded increments are not monotone in the rate, so a rate that is only a maximum must be declared per fill.

Reservations of a sell of 2 BTC, lot 1e-8 on an 8 digit grid, rate 0.001:

| Declared rounding | Reserved |
|---|---:|
| default (per fill, away from zero) | 4 |
| per fill, half even or half away from zero | 3.002 |
| per fill, toward zero | 2.002 |
| cumulative, exact rate 0.001 | 2.002 |

The half modes are conservative: each fill may round up by half a unit, and 2e8 one lot fills could add 1 BTC.

**The hold.** Every count and bound is computed in wide integers; the reservation is the hold `H`, the smallest Decimal at least the exact requirement on the asset grid. It equals the requirement whenever that is a Decimal, and otherwise rounds it up to the fewest digits that make it one, by less than one representable quantum at that magnitude. `H` is monotone, and a fill stays covered: if an order's hold `B` covers its requirement `A(R)` and a fill within the declaration debits `d` with `A(R') <= A(R) - d`, then `A(R') <= B - d`; whenever `B - d` is a Decimal (a state the ledger can book at all), `H(A(R')) <= B - d`: the new hold stays within what is left of the old one after the debit, so the fill causes no shortfall. (Holds of two states need not differ by the debit, since `H` rounds each state up on its own.) A booking whose resulting balance has no Decimal form fails as it always did; that is a limit of the ledger, not of the hold.

Affordability is exact for every declaration: the largest multiple of the lot, at most the request, whose hold as a whole order fits the available balance, found by an integer bisection over lot counts (the hold is monotone; `InsufficientBalance::required` is the hold of the request).

A buy with fees in base still reserves the notional only: in units of the base grid the fill `n` is an integer and `b = r * n < n`, so `ceil(a + b) - ceil(a) <= ceil(b) <= n`, a half mode gives at most `floor(b + 1) <= n` and toward zero `ceil(b) <= n`; a per fill fee and a cumulative increment both stay within what the fill credits.

**Breaches.** A fill outside the declaration is booked anyway, as a mandatory debit, and reported in `FillOutcome::fee_breaches`: per fill, a base fee above `round(r * n)` (`FeeBreach::AboveBound`) or a fill below `min_fill` that does not complete the order (`BelowMinimumFill`, even with no fee); cumulative, a base fee other than the declared increment `round(rate * (c + n)) - round(rate * c)`, above or below it, with `c` the real cumulative before the fill (`DeclaredFeeMismatch`; a missing fee conforms only when the increment is 0); and in any mode a fee in another asset (`UnexpectedAsset`), which does not change the reservation. The first breach of an instrument is recorded (`RiskGate::fee_declaration_breaches`): from then on every new order on it under that declaration (the same base fee rate, rounding and asset) is rejected with `RiskReason::FeeDeclarationBreached`, reducing orders included; cancels, confirmations and fill reports go on, and an order under another declaration (for example the default) is admitted. Live orders keep their terms; a shortfall from a breach still halts the gate (`AccountingBreach`). `RiskGate::reset_fee_declaration` clears the record once the declaration is checked; it is idempotent and keeps executions, reservations and terms. One gate is one account at one venue, so the record is kept per instrument. Repeated breaches mean the declaration is wrong.
- **Buy.** `per_lot` bounds the quote that one lot can cost in **any** fill at the limit price or better, however the order is fragmented. With `x = price * lot` and `r = max_fee_rate`, it is the smaller of two valid bounds:
  - `round_up(x) + round_up(r * round_up(x))`, on the grid, valid because rounding up is subadditive;
  - `(1 + r) * (x + h) + (one ulp if r > 0)`, exact, where `h` is the largest error of the ledger's notional rounding: half an ulp for the half modes, 0 toward zero, one ulp away from zero.

  With fees in quote, the assumption is that a fill's fee is at most `r` times its booked notional, rounded up to the quote grid. With fees in base, the fee is paid from the base the fill credits, so `r` is 0 here and only the notional is reserved: a fill of `q` with a fee `f <= round_up(r * q) <= q` (because `r < 1` and `q` is on the base grid) changes base by `q - f >= 0` in one booking. `set_limits` therefore refuses fees in base with `max_fee_rate >= 1`.
- **Why it covers any fragmentation.** A fill of `k` lots debits a grid amount of at most `k * per_lot`. The reservation falls by at least the grid floor of `k * per_lot`, so every fill frees at least what it debits, and no sequence of fills exceeds the reservation. `no_fragmentation_exceeds_the_reservation` checks this on 400 pseudo-random cases with no free balance.
- **Cost of the guarantee.** When `x` is on the quote grid, for example 100 USDC at a 1e-8 BTC lot, nothing is over-reserved. When `x` is off the grid, a fill of one lot really can cost more than `x`. Example: 150.01 at a 1e-8 lot is 1.5001e-6, booked as 2e-6. The bound reserves for that worst case. Choose the instrument's quantity scale to match the venue's real lot to keep it small.
- **Affordability.** The affordable quantity is `floor(available / per_lot)` lots. It is exact: for a grid value `available`, the rounded-up reservation fits exactly when the unrounded one does.

The lifecycle changes the reservation as follows:

| Event | Reservation |
|---|---|
| `submit` (approved) | created for the approved quantity |
| `on_fill` on a live order | shrunk to the amount the remaining quantity needs (zero after an overfill), **in the same atomic step** that books the fill; removed when the order is filled. A fee above the bound is taken from the free balance |
| repeated fill (same `ExecutionId` and content) | unchanged; the ledger returns `Duplicate`, so the adjustment happens exactly once |
| `request_cancel` | **unchanged**; the order may still fill until the venue confirms |
| `on_cancel_confirmed` | the rest is released; repeated or late confirmations release 0 |
| `on_venue_rejected` | the rest is released (only before any fill) |
| `on_fill` on a canceled or filled order (late report) | none left: booked against the free balance |

### Fill reports: a real execution is never lost

`on_fill(order_id, &FillReport)` takes the execution and the order's cumulative filled quantity after it (FIX `CumQty`). It returns a `FillOutcome { booking, anomalies, shortfalls, withdraw }`.

**Rejected, with nothing changed.** Only reports that cannot be causally true are rejected:

- an unknown order;
- an order the gate rejected (it was never sent);
- another instrument or side;
- `cumulative_quantity != filled so far + quantity`, which is a broken sequence for that order;
- a reused execution id with different content;
- inputs the ledger cannot represent: non-positive values, or values finer than the instrument scale.

**Booked, always.** Everything else happened at the venue, so it is booked:

| Case | What happens |
|---|---|
| Normal fill on a live order | Booked; the reservation shrinks to what the rest needs |
| Fill after a confirmed cancel, or on a filled order | Booked against the free balance |
| Overfill (more than the open quantity) | Booked; only the reserved part is released; `ExecutionAnomaly::Overfill`, and the gate halts with `HaltReason::ExecutionAnomaly { order, anomaly }` |
| Fill on a filled order | Booked; `ExecutionAnomaly::FillAfterFilled`, and the gate halts |
| Price worse than the limit | Booked; `ExecutionAnomaly::WorsePrice`, and the gate halts |
| Debit larger than reservation plus free balance | Booked anyway with `Ledger::book_execution_allowing_overdraft`. The balance goes negative (a debt) and the shortfall is in `FillOutcome::shortfalls`, `Ledger::overdrafts()` and `Ledger::reconcile().overdrafts`. The gate halts with `HaltReason::AccountingBreach` |

A repeated report (same execution id and content) returns a `Duplicate` booking and changes nothing.

## Circuit breaker and quote withdrawal

- **`trip(reason)`.** Activates the halt. Every later `submit` is rejected with `Halted { reason }`. Every live order without a cancel pending moves to `CancelPending`, and their ids are returned. The caller sends exactly those cancels; that is the quote withdrawal. Reservations stay until each cancel is confirmed, and fills that race the cancels are booked normally. Tripping again while halted returns no orders and keeps the first reason.
- **Internal halts withdraw quotes too (#16).** When `on_fill` halts the gate (an execution anomaly or an accounting breach), it withdraws quotes exactly as `trip()` does. Every live order without a cancel pending moves to `CancelPending`, and `FillOutcome::withdraw` lists their ids. The caller sends exactly those cancels, so no live order is left without a pending cancel.
  - The order that halted the gate is updated first. If the fill completed it, it is `Filled` and needs no cancel. If it was a partial fill, for example a worse price on part of the order, the order is withdrawn with the rest.
  - Reservations stay until each cancel is confirmed, and fills that race the cancels are booked normally.
  - `withdraw` is empty for a clean fill, for a duplicate, and for a second internal halt while every live order already has a cancel pending.
- **`resume()`.** Lifts the halt and clears every recorded reason.
- **Several reasons.** Every reason is recorded in `halt_reasons()`, in order. `halted()` reports the most severe by `HaltReason::severity`: `AccountingBreach` (3), then `ExecutionAnomaly` (2), then the external halts (1). Among equal severities it reports the first. A fill that is both an anomaly and an overdraft therefore records both and reports the breach.
- **Sources of `HaltReason`:**
  - `Manual`: an operator.
  - `CircuitBreaker`: an external breaker. The market-maker-rs adapter calls `trip(CircuitBreaker)` when `CircuitBreaker::is_trading_allowed()` turns false.
  - `DataDisconnect`: the adapter calls it when its feed drops. Independently of that, stale marks reject orders through `max_data_age_ms`.
  - `AccountingBreach`: set by the gate itself when a real fill overdraws the account. Example: the venue fee exceeded `max_fee_rate` and there is no free balance. The fill is booked as a debt and trading halts until an operator funds and reconciles the account.
  - `ExecutionAnomaly { order, anomaly }`: set by the gate itself when a real fill does not match its order (overfill, fill after filled, worse price). The fill is booked.

## Mapping from market-maker-rs (contract for the adapter)

The market-maker-rs integration belongs to market-maker-rs, which uses margin-rs optionally through its `margin` feature. The table below is the contract it implements.

| market-maker-rs (`origin/main` a869711) | margin-rs |
|---|---|
| Quote orders from `BacktestEngine` / `OrderManager` and hedge orders | `OrderRequest` (`OrderPurpose::Quote` / `Hedge`), sent through `RiskGate::submit`. There is no other path to the venue or the simulator |
| `RiskLimits { max_position, max_notional }` | `InstrumentLimits { max_position, max_position_notional }`. The gate checks them on the joint exposure (positions plus live orders); `RiskLimits::check_order` only looks at position plus the new order |
| `RiskLimits::scale_order_size` (soft scaling) | Stays strategy-side, applied before `submit`. The gate enforces hard limits and does not duplicate the scaling |
| `CircuitBreaker::is_trading_allowed() == false` | `trip(HaltReason::CircuitBreaker)`; then send cancels for the returned orders |
| `CircuitBreaker` back to `Active` | `resume()` |
| `ExecutionReport` `Accepted` | `on_venue_accepted` |
| `ExecutionReport` `Rejected` | `on_venue_rejected` |
| `ExecutionReport` `PartialFill` / `Fill` | `on_fill(order_id, &FillReport { execution, cumulative_quantity })`. The execution is mapped as in `docs/ledger.md`; `cumulative_quantity` comes from `ExecutionReport::cumulative_quantity`. Then send cancels for every order in `FillOutcome::withdraw` |
| `ExecutionReport` `Canceled` | `on_cancel_confirmed` |
| `ExecutionReport` `CancelRejected` (already filled) | nothing to do: the fill already ended the order and released the reservation |
| A fill report arriving after `Canceled` | `on_fill` as usual; it is booked as a late fill |
| `BacktestConfig::fee_rate` | `InstrumentLimits::max_fee_rate` (at least the rate actually charged) |
| The asset the venue charges fees in (quote, or base) | `InstrumentLimits::fee_asset` |

## Out of scope

- Regulatory or venue margin. This gate enforces balances and configured inventory limits only, and does not replace margin rules.
- Fees charged in a third asset (neither base nor quote), and fees charged in an asset other than `fee_asset`. They are booked by the ledger as mandatory debits from the free balance, and an overdraft halts the gate (`AccountingBreach`), but they are not reserved for.
- Margin reservations (`margin_reservations`) margin derivative orders, whose fees are in the settlement asset; they have no spot fee path, and `fee_asset` does not apply to them.
- Market orders. Requests are limit orders; a market order should be submitted with a protective limit (collar).
