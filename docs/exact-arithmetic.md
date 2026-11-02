# Exact arithmetic rule

Every amount in margin-rs is an exact `Decimal`. The rule, from the coordinator, applies to all code and to every new PR:

> Any aggregation or product of more than two terms goes through the wide-integer helpers in `src/decimal/mod.rs`, with **one** final conversion, or **one** final rounding. Chaining `add`, `sub` or `mul` over `Decimal` values is not allowed when an intermediate could be unrepresentable while the result is not.

| Helper | Use |
|---|---|
| `sum(values)` | exact sum of any number of terms, independent of order |
| `product(values)` | exact product of any number of factors |
| `sum_products(terms)` | exact sum of products, not rounded |
| `mul_div_round(nums, dens, scale, mode)` | product over product, rounded once |
| `sum_terms_round(terms, scale, mode)` | sum of products, rounded once |
| `quadratic_form_round(groups, C, scale, mode)` | `e' C e` with exact group nets, rounded once |
| `compare_products(left, right)` | exact comparison of two products |

A single `add`, `sub` or `mul` of two values is fine: it is exact or it fails. The same holds for an update of stored state by one value, such as a balance plus a delta, because the stored state has to be representable anyway.

## Audit (exact aggregation PR)

Each row is a site that chained operations before this PR.

| Site | Before | After | Test |
|---|---|---|---|
| `scenarios::stress`, stressed price | `price * (1 + shock)`, then `* (1 ± spread)` | one `product` | `stressed_price_is_one_exact_product` (7e28 × 1.5 × 0.5) |
| `scenarios`, hypothetical fill deltas | balance updated once per delta, so a base fee was applied after the base quantity | all changes per asset in one `sum` | `hypothetical_fill_changes_apply_in_one_step` (MAX − 1 + 2 − 2) |
| `ledger::book`, deltas per asset | accumulated with `add` | one `sum` per asset | covered by the ledger fixtures. The terms are at most one quantity, one notional and one fee |
| `Ledger::valuation`, per-asset realized, unrealized, net | running `add` across positions, then `add` and `sub` | one `sum` per figure | ledger fixtures. Signs can be mixed, so order independence matters |
| `Ledger::reconcile` | running `add` over the transfers and the journal | one `sum` per checked figure | every `reconcile()` call in the test suite |
| `valuation::value`, `rounding_difference` | `net - (realized + unrealized - fees)` | one `sum` | valuation fixtures |
| `collateral`, `1 - h - extra` | `sub(1, add(h, extra))` | one `sum` | collateral fixtures |
| `risk`, position headroom | `bound - (position + open)` | one `sum` | gate fixtures |
| `risk`, exact per-lot bound | `(1 + r) * (x + h) + ulp` as chained calls | one `sum_products` | gate fixtures and the fragmentation property test |
| `risk`, lot fee | `round_up(mul(r, c))` | `mul_div_round` | gate fixtures |
| `risk`, open buy and sell quantities | running `add` | `sum` | gate fixtures. These sums only grow, so the fix keeps the rule uniform |
| `rules::TieredLinearIm`, totals | running `add` | `sum` | initial margin fixtures. These sums only grow |

Two-term operations on stored state are kept as they are, because the state must be representable in any case:

- `apply_fill`: quantity, cost basis, released cost and realized PnL;
- reservations and transfers in the ledger;
- the order filled quantity in the gate;
- per-slice notionals in the margin rule.
