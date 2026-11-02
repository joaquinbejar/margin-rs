# Initial margin rules (M3-03B)

`initial_margin::InitialMarginModel::requirement(&ExposureView) -> MarginRequirement`

## Contract

- **Input.** An `ExposureView` covers **one account at one venue**: `account`, `venue`, and at most one `ExposurePosition` per instrument. Each position carries `product`, `settlement` asset, signed `contracts` (`Qc`), `multiplier` (`M`) and `mark`.
- **No netting.** Exposure is never netted across accounts or venues. Two views give two requirements, and the crate never combines them. Within a view, positions are margined gross, with no netting between instruments.
- **Output.** A `MarginRequirement` carries:
  - the `RuleId` (name and version) that produced it, for audit;
  - the account, venue and currency;
  - the totals `initial` and `maintenance`;
  - a `PositionRequirement` per position, with the exact notional, the `TierSlice`s, and the rounded `initial` and `maintenance`. The totals are the exact sums of the positions, so the breakdown always adds up.
- **Errors.** All typed:
  - `UnsupportedInstrument { rule, instrument, reason }` for products the rule does not cover;
  - `CurrencyMismatch` for a settlement asset other than the rule's;
  - `InvalidInput` for a non-positive multiplier or mark, or a duplicate instrument;
  - `Arithmetic` for an amount that cannot be represented.

## `xynthex-linear-im/1`

| | |
|---|---|
| Rule | `rules::TieredLinearIm::xynthex_v1(usdc, scale)` |
| Products | Linear USDC futures and perpetuals (`ProductKind::LinearFuture`, `LinearPerpetual`) |
| Primary specification | Xynthex `docs/design/economics.md` section 11.2, at commit `2b6a97330ef056673c97f3b1a48da4e62cdb67f3` (gitlab.taunais.com/capitaldelta/xynthex; the file last changed in `84727a8014172c3bea6c6de2a47d5b6715122ace`) |
| Formula | `im = 10 % * |Qc| * M * mark`, `mm = 5 % * |Qc| * M * mark` |
| Units | `Qc` in contracts, `M` in underlying units per contract, `mark` in USDC per underlying unit; amounts in USDC at the given scale |
| Rounding | Each position rounded up once to the USDC scale; totals are exact sums |
| Not covered | Spot (cash margined) and options (their own formula in the same section): `UnsupportedInstrument` |

The rule is the tier table `[0, inf)` with rates 0.10 and 0.05.

## Tier tables

`TieredLinearIm::new(rule_id, currency, scale, tiers)` builds the same formula over any tier table. The bands apply **progressively**: each rate covers the part of the notional inside its band, so a requirement is continuous at every boundary. Formally:

```text
slice_k = max(0, min(notional, floor_{k+1}) - floor_k)
im = round_up(sum(slice_k * im_rate_k))
mm = round_up(sum(slice_k * mm_rate_k))
```

A table is valid when:

- it is non-empty;
- the first floor is 0 and floors strictly increase;
- every tier has `0 < mm_rate <= im_rate <= 1`;
- rates never decrease from one tier to the next.

Each table must have its own `RuleId`. The two-tier fixture in `tests/initial_margin.rs` is labelled `synthetic-two-tier/1`. It exists to test boundaries and is not any venue's rule.

## Future rules

External venue rules are future rules, each with its own identifier and a specification pinned by date. One example is Binance USDⓈ-M leverage brackets, which apply the bracket of the whole notional and a maintenance amount rather than progressive bands. Their tables change without notice, so the date is part of the rule version.
