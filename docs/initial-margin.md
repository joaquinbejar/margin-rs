# Initial margin rules

`initial_margin::InitialMarginModel::requirement(&ExposureView) -> MarginRequirement`

## Contract

- **Input.** An `ExposureView` covers **one account at one venue**: `account`, `venue`, and at most one `ExposurePosition` per instrument. Each position carries `product`, `settlement` asset, signed `contracts` (`Qc`), its `size` and `mark`. The size is a `ContractSize`: `Multiplier(M)` (underlying units per contract) for a linear product, `ContractValue(CV)` (quote units per contract) for an inverse one; any other pairing is `InvalidInput`.
- **Notional.** In the settlement asset: `|Qc| * M * mark` for a linear position, `|Qc| * CV / mark` for an inverse one, whose mark is in the quote currency of `CV` per unit of the base (the settlement asset). At a fixed mark both are linear in `Qc`.
- **No netting.** Exposure is never netted across accounts or venues. Two views give two requirements, and the crate never combines them. Within a view, positions are margined gross, with no netting between instruments.
- **Output.** A `MarginRequirement` carries:
  - the `RuleId` (name and version) that produced it, for audit;
  - the account, venue and currency;
  - the totals `initial` and `maintenance`;
  - a `PositionRequirement` per position, with the notional, the `TierSlice`s, and the rounded `initial` and `maintenance`. The totals are the exact sums of the positions, so the breakdown always adds up. The notionals are informational (`Option`): exact for a linear position, rounded half-even to 18 decimals for an inverse one, and `None` when they do not fit a `Decimal`. The margins never depend on them: a notional out of range does not block a representable margin.
- **Errors.** All typed:
  - `UnsupportedInstrument { rule, instrument, reason }` for products the rule does not cover;
  - `CurrencyMismatch` for a settlement asset other than the rule's;
  - `InvalidInput` for a size that is not positive or not of the product's kind, a non-positive mark, or a duplicate instrument;
  - `Arithmetic` for an amount that cannot be represented.

## `linear-im/1`

| | |
|---|---|
| Rule | `rules::TieredLinearIm::linear_v1(usdc, scale)` |
| Products | Linear futures and perpetuals settled in the rule's asset, USDC in the examples (`ProductKind::LinearFuture`, `LinearPerpetual`) |
| Specification | This document: the crate's own rule for linear futures and perpetuals |
| Formula | `im = 10 % * |Qc| * M * mark`, `mm = 5 % * |Qc| * M * mark` |
| Units | `Qc` in contracts, `M` in underlying units per contract, `mark` in the settlement asset per underlying unit; amounts in the settlement asset at the given scale |
| Rounding | Each position rounded up once to the settlement asset scale; totals are exact sums |
| Not covered | Spot (cash margined) and options (a different formula): `UnsupportedInstrument` |

The rule is the tier table `[0, inf)` with rates 0.10 and 0.05.

The identifier names exactly these rates. A venue configured with other rates is a different rule. It must be built with `TieredLinearIm::new` and given its own `RuleId` or version, never `linear-im/1`.

Rounding each position up once to the settlement asset scale, with totals as exact sums, is part of the rule. A different rounding is a new version of the rule.

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

## `inverse-im/1`

| | |
|---|---|
| Rule | `rules::TieredInverseIm::inverse_v1(base, scale)` |
| Products | Inverse futures and perpetuals (`ProductKind::InverseFuture`, `InversePerpetual`), settled in the base |
| Formula | `im = 10 % * |Qc| * CV / mark`, `mm = 5 % * |Qc| * CV / mark`, in the base |
| Units | `Qc` in contracts, `CV` in quote units per contract, `mark` in quote units per unit of the base; amounts in the base at the given scale |
| Rounding | Each position rounded up once to the base scale; totals are exact sums |
| Not covered | Spot, options and linear products: `UnsupportedInstrument` |

`TieredInverseIm::new(rule_id, base, scale, tiers)` takes the same tier tables as the linear rule, with the floors in the base. The notional `N = |Qc| * CV / mark` is a quotient and generally not a finite decimal, so:

- each tier is compared exactly by cross products: `N > floor` is `|Qc| * CV > floor * mark`;
- each margin `sum over i < k of rate_i * (floor_{i+1} - floor_i) + rate_k * (N - floor_k)` is one exact sum over the common denominator `mark`, rounded up once.

The linear rule computes its margins the same way (with denominator 1), so neither ever needs the notional itself to fit a `Decimal`.

## Future rules

External venue rules are future rules, each with its own identifier and a specification pinned by date. One example is Binance USDⓈ-M leverage brackets, which apply the bracket of the whole notional and a maintenance amount rather than progressive bands. Their tables change without notice, so the date is part of the rule version.
