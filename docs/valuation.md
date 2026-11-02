# Account valuation and stress (M2-08)

Cash posting and valuation are separate.

- The `Ledger` posts executions, transfers and fees, and never reads a price.
- `valuation::value(&LedgerSnapshot, &MarkSet, &ValuationPolicy)` and `scenarios::stress(.., &Scenario)` are pure functions over an immutable snapshot. They never change the ledger.

## Price source, freshness, reporting asset

- `ValuationPolicy::reporting` is the asset every figure is expressed in. It is worth exactly 1 and must not appear in the mark set.
- A `Mark` is the price of one unit of an asset in the reporting asset, with `timestamp_ms` and a `PriceSource`: `Mid`, `Last`, `Index`, `Settlement` or `Model`. The source is reported, not interpreted.
- At `MarkSet::as_of_ms`, a mark's status is one of:
  - `Valid`: price > 0 and age at most `max_age_ms`;
  - `Stale { age_ms }`;
  - `Future { ahead_ms }`: timestamped after `as_of_ms`;
  - `NonPositive`;
  - `Missing`.
- Every asset the valuation needs whose mark is not valid appears in `AccountValuation::flags`. These are assets with a balance, net transfers, fees or a non-trivial position.

## Reporting equity and collateral equity

| Figure | Definition | Invalid marks |
|---|---|---|
| `equity` | `sum(total balance * price)`, rounded once (policy rounding) | Stale and future marks are used and flagged. A held asset with a missing or non-positive price gives `None`, not a guess |
| `collateral_equity` | Value that marks can be trusted for, rounded down (toward negative infinity) | See the rules below |

Collateral rules, per balance:

| Balance | Valid mark | Stale, `HaircutStale { haircut }` | Stale with `Exclude`, future, non-positive, missing |
|---|---|---|---|
| Held (positive) | `total * price` | `total * price * (1 - haircut)` | 0 |
| Debt (negative) | `total * price` | `total * price * (1 + haircut)`: a haircut never shrinks a debt | unknown, so `collateral_equity` is `None` |

A debt can appear in a stress scenario after a hypothetical fill, or from a fee charged in an asset that is not held. A debt always counts, and an asset without a trusted mark never adds value. **An invalid mark can only lower collateral or make it unknown; it cannot create spendable equity.**

With `Exclude`, a stale mark above the true price cannot inflate collateral. With `HaircutStale`, a stale mark counts at its price less the haircut, so the policy must be chosen with that in mind.

## PnL with fees counted once

- Fees are debited from the balance of the asset they were charged in, so equity includes them once.
- `net_pnl = equity - net_transfers`, where deposits minus withdrawals are valued at the same marks. This identity is exact.
- `PnlAttribution` splits `net_pnl` in the reporting asset. Each term needs only the price it multiplies, and zero terms need none: a closed position with realized PnL needs the quote mark only, not the base mark.
  - `realized = sum(realized_pnl * price(quote))`, gross of fees;
  - `unrealized = sum(quantity * price(base) - cost_basis * price(quote))`;
  - `fees = sum(fees_paid * price(asset))`.
- For spot, `equity - net_transfers = realized + unrealized - fees` exactly. Each figure is rounded once, so `rounding_difference` shows the only possible gap. It is zero on grid values.

## Futures and perpetuals

An open derivative position is valued at its **instrument mark**, `MarkSet::instruments` (`MarkSet::with_instrument`): the price per unit of the underlying, with the same freshness rules as asset marks. For a linear contract it is in the settlement asset; for an inverse contract it is in the quote currency of its contract value `CV`, per unit of the base (the settlement asset).

| Figure | Rule |
|---|---|
| unrealized PnL | linear: `upnl = rhe(Q * M * mark) - basis`; inverse: `upnl = basis - rhe(Q * CV / mark)`, in the base. Rounded once (half-even) to the settlement asset scale, per instrument, listed in `AccountValuation::derivatives` |
| missing mark | `MissingMark` for an open position (never valued at zero); flat and settled positions need none, and a mark for them is ignored |
| `equity` | adds `upnl * price(settlement asset)`; a stale or future instrument mark is used and flagged in `AccountValuation::instrument_flags` |
| `collateral_equity` | `cash + sum(upnl)` per settlement asset, **one net amount**, with the balance rules above applied to the net: a loss nets against the cash, a net debt counts in full |
| invalid instrument mark | a gain is excluded before netting; a loss makes `collateral_equity` `None` (never offset by cash). Instrument marks are never haircut under `HaircutStale` |
| attribution | `unrealized` adds `upnl * price(settlement asset)`; realized PnL and funding are already in cash and counted once |

`maintenance::CollateralView::from_valuation` builds the maintenance view from a valuation without recomputing anything: equity and collateral equity on total balances (reservations not deducted), `None` preserved.

## Stress scenarios

Scenarios shock asset marks, not instrument marks: `stress` refuses an open linear future or perpetual with `UnsupportedInstrument` rather than report it unstressed.


`stress` values the snapshot with and without a `Scenario`, applying in this order:

1. **Hypothetical fills.** Fills such as a hedge that is expected to execute are applied to a copy of the snapshot with the ledger's booking rules: the notional is rounded once, the fee is debited once, and positions use average cost. Balances are not checked. Leaving the hedge out models a failed hedge.
2. **Gaps.** `price * (1 + shock)` per asset, applied exactly. The shock must be greater than -1.
3. **Liquidation spread.** Each non-reporting asset with a positive balance is valued at `price * (1 - spread)`, which is the sale proceeds. Each one with a negative balance is valued at `price * (1 + spread)`, which is the buy-back cost. `spread` is in `[0, 1)`.

All parameters are validated before anything is computed:

- a shock must name a registered asset other than the reporting asset, and be greater than -1;
- the spread must be in `[0, 1)`;
- hypothetical fills follow the ledger's input rules: positive price and quantity within the instrument scales, and a fee within its asset scale.

A shock on an asset without a mark has no effect and is listed in `StressResult::ignored_shocks`.

`StressResult` reports both valuations, `equity_change` and `collateral_change` (each `None` when a side is unknown), and `ignored_shocks`. Invalid marks stay invalid under stress.

## Snapshot additions

`LedgerSnapshot` now carries `assets` (scales), `instruments` and `net_transfers`, so it is self-contained. The ledger tracks net transfers per asset as transfers are applied, and `reconcile()` checks them against the transfer list.

## Out of scope

- Margin requirements (M3-03).
- Portfolio risk (M3-02).
- Derivatives. Negative balances only arise in hypothetical scenarios; the ledger never books one.
- Mark sourcing itself: the caller supplies marks.
