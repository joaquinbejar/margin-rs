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
| `reserve(order, collateral, marks)` | Accepted only if the total requirement with the order is within `collateral`. Otherwise `Rejected` with a reason, and nothing changes |
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

Spot orders reserve **cash** in the ledger through `risk::RiskGate` (M1-05). Derivative orders reserve **margin** here.

The `collateral` passed to `reserve` should come from `collateral::value_collateral` on the default `BalanceBasis::Available`, which already subtracts the cash held by spot reservations. Each unit of capital is therefore committed once: either to a spot order's cash reservation or to derivative margin, never to both. A `None` collateral, meaning a debt with no trusted mark, rejects every new order.
