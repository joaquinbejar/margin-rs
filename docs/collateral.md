# Collateral with haircuts and a stale price policy (M3-03A)

`collateral::value_collateral(&LedgerSnapshot, &MarkSet, &CollateralPolicy) -> CollateralValue`

This computes the available collateral of one account from exact ledger balances and supplied marks. It never changes the ledger and never touches realized cash. The marks and the currency conversion are explicit: every price comes from the `MarkSet`, in the reporting asset, with its source and timestamp (`valuation::Mark`, M2-08).

## Policy

| Field | Meaning |
|---|---|
| `reporting` | Asset of every amount. Its price is exactly 1 and it is always valid |
| `max_age_ms` | A mark older than this is stale |
| `haircuts` | Haircut per eligible asset, in `[0, 1]`. An asset that is not listed is **not eligible**, so a held balance counts 0. The reporting asset defaults to 0 % |
| `stale` | `Exclude` (default), `ExtraHaircut { extra }` or `Reject` |
| `basis` | `Available` (default; reserved cash is already committed) or `Total` |

## Rules

| Balance | Mark | Contribution |
|---|---|---|
| held, eligible, haircut `h` | valid | `balance * price * (1 - h)` |
| held, eligible | stale | `Exclude`: 0. `ExtraHaircut`: `balance * price * (1 - h - extra)`. `Reject`: error |
| held, eligible | missing, non-positive, future | 0. `Reject`: error |
| held, not eligible | any | 0 |
| debt | valid | `balance * price`, in full: a haircut never shrinks a debt |
| debt | stale | `Exclude`: unknown. `ExtraHaircut`: `balance * price * (1 + extra)`. `Reject`: error |
| debt | missing, non-positive, future | unknown. `Reject`: error |

Each line is rounded down once, toward negative infinity. The total is the exact sum of the lines, or `None` if any line is unknown. Assets whose mark is not valid are listed in `flags`.

**Stale marks cannot silently increase spendable collateral.** Under the default policy, a stale or otherwise invalid mark only lowers collateral or makes it unknown. Using a stale price at all requires choosing `ExtraHaircut` explicitly.

## Relation to M2-08

`valuation::AccountValuation::collateral_equity` (M2-08) is a valuation-side view with one haircut policy for stale marks. `value_collateral` is the margin-side collateral. It adds per-asset eligibility and haircuts, a choice of balance basis, and an explicit reject option. Both treat debts the same way.

## Collateral of a derivatives account

A derivatives account typically has `collateral = cash + sum(upnl of futures and perps) - sum(option marks of short positions)`, where long options and non-cash assets may count as zero collateral.

This function covers the cash and the futures and perpetuals:

- The cash asset (for example USDC) is the reporting asset, with a 0 % haircut.
- An asset that must not count is left out of `haircuts`, or given a 100 % haircut.
- The unrealized PnL of the open linear futures and perpetuals held in the ledger is added to the balance of their settlement asset, as one net amount, at their instrument marks (`MarkSet::instruments`, see `docs/valuation.md`). The asset's haircut and stale policy apply to that net; `CollateralLine::unrealized_pnl` shows what was counted. A gain at a stale or future instrument mark is excluded, a loss at one makes the line unknown, and `StalePolicy::Reject` refuses either with `InvalidInstrumentMark`.

Short option marks are not covered: options are not ledger positions.
