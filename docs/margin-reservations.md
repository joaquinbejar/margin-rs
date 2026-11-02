# Margin reservations for pending orders (M3-03C)

`margin_reservations::MarginReservationService<M: InitialMarginModel>` serves one account at one venue under one margin rule, for example `xynthex-linear-im/1`.

## Requirement

The requirement is recomputed from state on every call, never accumulated, so it is always exact.

- **Worst case per instrument.** Take the larger absolute value of `position + open buys` and `position - open sells`. That is the position if every live order on one side fills.
- **Rule input.** `MarginState::total` is the rule's requirement on those worst-case positions. `positions` is the requirement of the current positions alone, and `pending = total - positions`.
- **No offsets.** There are no portfolio offsets: each instrument is margined on its own worst case, as the rule does.

## Lifecycle

| Call | Effect |
|---|---|
| `reserve(order, collateral, marks)` | Accepted if the total requirement with the order is within `collateral`, or if the order does not raise the requirement at all. The second case is a risk-reducing order, which is accepted even when the account is under-margined. Otherwise the order is `Rejected` with a reason and nothing changes. The `reserved` amount is accounting against the collateral passed in, not a ledger reservation |
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

`RiskGate::set_hold` is atomic: if the new amount does not fit, the previous hold stays. A hold id cannot be an order id. Collateral here is cash in the settlement asset, as in Xynthex v1, where XYZ and long options count as zero collateral.

Fills are idempotent by execution id. Reusing an id with another order or quantity returns `ConflictingEntry` and changes nothing.
