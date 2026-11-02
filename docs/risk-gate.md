# Pretrade risk gate (M1-05)

`margin_rs::risk::RiskGate` stands between every order source (quotes, hedges, policy proposals) and the venue. It owns the `Ledger` from M1-04, so reservations live in the same accounting authority as cash and positions.

## Mandatory path

- `RiskGate::new(ledger)` takes the ledger by value. Afterwards the gate only lends `&Ledger`, so code outside the gate cannot book, reserve or release around it. A `compile_fail` doctest checks this.
- The only ways to change state are the gate's own methods:
  - `submit`;
  - `on_venue_accepted`, `on_venue_rejected`;
  - `request_cancel`, `on_cancel_confirmed`;
  - `on_fill`;
  - `trip`, `resume`;
  - `deposit`, `withdraw`.
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

Every approved order gets `ReservationTerms`, fixed at admission and stored in its `OrderRecord`. Later `set_limits` calls never change the reservation of a live order, so lowering `max_fee_rate` cannot release budget a live order may still need (M105-3).

The reservation for `remaining` is `round_up(remaining / lot * per_lot)` on the asset grid.

- **Sell.** `per_lot = lot` of the base asset, so the reservation is the remaining quantity itself. Spot sell fees are assumed to be charged in the quote asset and paid from the proceeds.
- **Buy.** `per_lot` bounds the quote that one lot can cost in **any** fill at the limit price or better, however the order is fragmented (M105-1). With `x = price * lot` and `r = max_fee_rate`, it is the smaller of two valid bounds:
  - `round_up(x) + round_up(r * round_up(x))`, on the grid, valid because rounding up is subadditive;
  - `(1 + r) * (x + h) + (one ulp if r > 0)`, exact, where `h` is the largest error of the ledger's notional rounding: half an ulp for the half modes, 0 toward zero, one ulp away from zero.

  The fee assumption is that a fill's fee is at most `r` times its booked notional, rounded up to the quote grid, and charged in the quote asset.
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
| `on_fill` on a canceled or filled order (late report) | none left: booked against the free balance (M105-2) |

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

The market-maker-rs integration is a separate PR in market-maker-rs (M1-05-INT). The table below is the contract that PR implements.

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

## Out of scope

- Regulatory or venue margin. This gate enforces balances and configured inventory limits only, and does not replace margin rules.
- Fees charged in the base or a third asset on sells. The gate assumes the quote asset. A sell fee in another asset is still booked correctly by the ledger, but it is not reserved for.
- Market orders. Requests are limit orders; a market order should be submitted with a protective limit (collar).
