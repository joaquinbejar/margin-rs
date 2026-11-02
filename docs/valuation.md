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
| `collateral_equity` | Value that marks can be trusted for, rounded toward zero | Only valid marks count, plus stale marks under the explicit `InvalidMarkPolicy::HaircutStale { haircut }`, which counts `price * (1 - haircut)`. Everything else counts 0 |

Spot balances are never negative. Leaving an asset out therefore never raises collateral: **an invalid mark cannot create spendable equity without an explicit policy**. A stale mark above the true price cannot inflate collateral either.

## PnL with fees counted once

- Fees are debited from the balance of the asset they were charged in, so equity includes them once.
- `net_pnl = equity - net_transfers`, where deposits minus withdrawals are valued at the same marks. This identity is exact.
- `PnlAttribution` splits `net_pnl` in the reporting asset:
  - `realized = sum(realized_pnl * price(quote))`, gross of fees;
  - `unrealized = sum(quantity * price(base) - cost_basis * price(quote))`;
  - `fees = sum(fees_paid * price(asset))`.
- For spot, `equity - net_transfers = realized + unrealized - fees` exactly. Each figure is rounded once, so `rounding_difference` shows the only possible gap. It is zero on grid values.

## Stress scenarios

`stress` values the snapshot with and without a `Scenario`, applying in this order:

1. **Hypothetical fills.** Fills such as a hedge that is expected to execute are applied to a copy of the snapshot with the ledger's booking rules: the notional is rounded once, the fee is debited once, and positions use average cost. Balances are not checked. Leaving the hedge out models a failed hedge.
2. **Gaps.** `price * (1 + shock)` per asset, applied exactly. The shock must be greater than -1.
3. **Liquidation spread.** Each non-reporting asset with a positive balance is valued at `price * (1 - spread)`, with `spread` in `[0, 1)`.

`StressResult` reports both valuations, `equity_change` and `collateral_change`. Invalid marks stay invalid under stress.

## Snapshot additions

`LedgerSnapshot` now carries `assets` (scales), `instruments` and `net_transfers`, so it is self-contained. The ledger tracks net transfers per asset as transfers are applied, and `reconcile()` checks them against the transfer list.

## Out of scope

- Margin requirements (M3-03).
- Portfolio risk (M3-02).
- Derivatives and negative balances.
- Mark sourcing itself: the caller supplies marks.
