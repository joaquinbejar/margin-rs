# Margin reservations for pending orders (M3-03C)

`margin_reservations::MarginReservationService<M: InitialMarginModel>` serves one account at one venue under one margin rule, for example `linear-im/1`.

## Requirement

The requirement is recomputed from state on every call, never accumulated, so it is always exact.

- **Worst case per instrument.** Take the larger absolute value of `position + open buys` and `position - open sells`. That is the position if every live order on one side fills.
- **Rule input.** `MarginState::total` is the rule's requirement on those worst-case positions. `positions` is the requirement of the current positions alone, and `pending = total - positions`.
- **No offsets.** There are no portfolio offsets: each instrument is margined on its own worst case, as the rule does.

## Lifecycle

| Call | Effect |
|---|---|
| `reserve(order, collateral, marks)` | Accepted if the total requirement with the order is within `collateral`, or if it is risk-reducing. That means the opposite side to the position, with a size of at most `|position|` minus the reducing orders already resting, so it can neither flip nor extend the position. A risk-reducing order is accepted even when the account is under-margined. Otherwise the order is `Rejected` with a reason and nothing changes. The `reserved` amount is accounting against the collateral passed in, not a ledger reservation |
| `apply_fill(order, execution_id, contracts)` | Idempotent by execution id. Moves the filled contracts from the order to the position |
| `release_on_effective_cancel(order)` | Closes the order's remaining quantity and returns it. Repeating it returns 0 |

`reserve` can reject for these reasons:

- `DuplicateOrderId`: the id was recorded before, including when that earlier order was rejected;
- `UnknownInstrument`;
- `InvalidQuantity`;
- `MissingMark`;
- `CollateralUnknown`;
- `InsufficientMargin { required, collateral, shortfall }`.

Orders are decided one at a time against everything already accepted, so **two plans can never reserve the same capital**. A rejected order changes nothing. After partial fills and effective cancels, the requirement is exactly that of the remaining positions and orders.

A real execution is never lost, so `apply_fill` handles these cases instead of rejecting them:

- **Overfill.** The whole fill goes to the position, and the order is closed.
- **Fill after the order was filled or cancelled.** The fill is still applied to the position.

## Relationship with cash reservations (no double reservation)

`reserve` on its own is a pure decision against the `collateral` it is given. It does not stop a spot order from spending the same cash. An account with both spot and derivative orders must use the gate-integrated calls:

| Call | Effect |
|---|---|
| `reserve_with_gate(order, &mut gate, &hold, marks)` | Collateral is the settlement asset's available cash in the `RiskGate` ledger plus the current hold. On acceptance, the whole requirement becomes the external hold, set with `RiskGate::set_hold` |
| `sync_hold(&mut gate, &hold, marks)` | After fills, effective cancels or new marks, the hold is set to the current requirement |

Spot reservations therefore reduce what margin can use, and the margin hold reduces what spot orders can use. **Either order of admission is safe**, and no unit of cash is reserved twice. The tests `margin_first_blocks_the_same_cash_for_spot` and `spot_first_blocks_the_same_cash_for_margin` cover both orders.

`RiskGate::set_hold` is atomic: if the new amount does not fit, the previous hold stays. A hold id cannot be an order id. Collateral here is cash in the settlement asset; other assets and long options count as zero collateral.

Fills are idempotent by execution id. Reusing an id with another order or quantity returns `ConflictingEntry` and changes nothing.

## Positions read from the ledger

`MarginReservationService::on_ledger(model, account, venue, hold)` builds a service that keeps no positions. Every call reads them from the ledger of the `RiskGate` it is given, where the futures and perpetuals are registered and their fills booked. The service's hold is the `ReservationId` fixed at construction.

| Call | Effect |
|---|---|
| `reserve_with_gate(order, &mut gate, &hold, marks)` | As above, with the positions of the ledger and `collateral = available + own hold + sum(upnl)` in the settlement asset, one exact sum. `hold` must be the service's own |
| `apply_booked_fill(&gate, order, execution_id)` | The fill must already be booked in the gate's ledger; its contracts are the booking's. Only the order's remaining quantity moves |
| `sync_hold(&mut gate, &hold, marks)` | The hold follows the ledger's positions |
| `requirement_with_gate(&gate, marks)` | The requirement, positions read from the ledger |

**Collateral counts every open derivative.** The unrealized PnL `rhe(Q * M * mark) - basis` covers every open linear future and perpetual in the ledger settled in the service's asset, including the ones this service does not trade. A loss on another strategy's position is never left out. Each needs a mark: a missing one rejects the order with `MissingMark`, never a zero, and a mark that is not positive is `InvalidInput`. Derivatives settled in other assets do not count.

**Own hold once, foreign holds never.** `available` already excludes every reservation. The service adds back its own hold, read by its own id and checked to be in the settlement asset (`CurrencyMismatch` otherwise), and nothing else. On acceptance the hold becomes `min(required, max(available + own hold, 0))`: a hold takes cash only, never unrealized gains.

**Booking protocol.** The caller books each fill first, with `RiskGate::book_derivative_execution` (mandatory: booked even into debt), then reports it:

- not booked yet: `FillNotBooked`, nothing changes;
- reported again for the same order: `Ok(false)`, nothing changes, so the remaining quantity never moves twice;
- reported for another order, or booked with another instrument or side than the order: `ConflictingEntry`;
- otherwise the remaining quantity falls by the booked contracts (an overfill closes the order).

The hold is then synced with `sync_hold`, which reads the position the ledger already holds, so it never follows a stale position.

**Registry.** Every registered instrument must be a linear future or perpetual of the ledger with the same product, settlement asset and multiplier (`UnknownInstrument`, `InvalidInput` or `CurrencyMismatch` otherwise).

**Explicit modes.** On the ledger, `reserve`, `requirement` and `apply_fill` are `WrongReservationMode`, and so is `apply_booked_fill` on a service with its own positions.

## Account in debt

Cash can go negative after a mandatory debit (a fee, a realized loss or a closeout settlement; see `docs/liquidation.md`). Then collateral, which is the available cash plus the current hold, is negative too:

- **`reserve_with_gate`.** Only risk-reducing orders can be accepted, because every other order fails the `collateral >= required` test. When one is accepted, the hold is set to `min(required, max(collateral, 0))`. It covers the requirement as far as the cash allows and is never negative, because a negative hold would create available cash. Under water, the hold is therefore 0. Once cash is positive again, the hold takes whatever cash there is, up to the requirement.
- **`sync_hold`.** This call sets the hold to the full requirement. When the cash cannot cover it, as with any debt, `RiskGate::set_hold` fails with `InsufficientBalance`. The previous hold stays in place and the error is returned to the caller. This is by design: `sync_hold` reports that the requirement is not covered, and never quietly lowers the hold. The account state itself (`ReduceOnly` or `Default`) is computed by `maintenance::assess`.
