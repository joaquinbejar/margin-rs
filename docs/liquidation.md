# Maintenance and liquidation (M3-04)

This document covers two modules and one ledger posting:

- `maintenance::assess` computes the account state that the maintenance margin of a rule implies.
- `liquidation::plan_liquidation` simulates one staged closeout against supplied scenario prices and liquidity. There is no live liquidator.
- `LiquidationAccount::from_ledger` reads the account from the ledger, and `ForcedReduction::execution` books each forced fill back into it (see [Ledger round trip](#ledger-round-trip)). For positions kept outside the ledger, `Ledger::book_settlement` posts the cash of each forced fill (see `docs/ledger.md`).

## Who owns what

| Concern | Owner |
|---|---|
| Liquidation trigger, account state, cancelling resting orders | the venue, when it keeps its own account state |
| What to close, in what order, size limits, outcome against book liquidity | margin-rs: `LiquidationPlan` |
| Journal of record when the venue keeps its own ledger | the venue's ledger; margin-rs reads its collateral figures (`CollateralView`) |
| Backtests and accounts without a venue ledger | margin-rs `Ledger`: `from_ledger` and `ForcedReduction::execution` for positions held in it, `book_settlement` for positions kept outside it |

Both functions are pure. Neither books, triggers or cancels anything.

## Maintenance threshold

The margins come from a `TieredLinearIm` rule, for example a single tier with IM 10 % and MM 5 % of `|Qc| * M * mark`. Each position is rounded up once, and the totals are exact sums.

`assess(previous, &CollateralView { currency, cash, collateral, equity }, &requirement)` applies these rules, in order:

| Condition | State |
|---|---|
| `cash < 0` or `equity < 0` | `Default` |
| previous state was `Default`, unless `cash >= 0` and `collateral >= mm` | `Default` |
| `collateral < mm`, or collateral or equity unknown | `ReduceOnly` |
| otherwise | `Healthy` |

- **Equality is healthy.** `collateral == mm` is `Healthy`, and `collateral = mm - 0.000001` is `ReduceOnly`. The tests cover the values just above, at and just below the threshold.
- **Unknown values never count as healthy.** An unknown collateral or equity, for example a debt without a trusted mark, leaves the account at best `ReduceOnly`.
- **The decision carries:**
  - the reasons, in a fixed order;
  - `excess = collateral - mm`, exact;
  - `maintenance_shortfall = max(0, mm - collateral)`;
  - `debt = max(0, -cash)`;
  - the rule id.
- **`may_increase_risk()`** is true only for `Healthy`.

## Closeout plan

```text
plan_liquidation(&model, &LiquidationAccount { view, basis, cash, resting_orders },
                 &liquidity, &LiquidationPolicy { target, fee_rate, contract_step })
```

`model` implements `PiecewiseLinearMargin`, which every `TieredLinearIm` does. The trait's contract has three parts:

- margins are additive across positions;
- each position's IM and MM are convex, non-decreasing, piecewise linear functions of its notional, given exactly (before rounding) by `margin_tiers`;
- every amount is rounded to `cash_scale`, which the plan uses as its cash quantum, so cash, PnL and margins always share one scale.

Each open position needs a non-zero `basis` with the sign of the position.

### Stages

1. **Cancel.** The plan cancels every resting order first, so the closeout never races the account's own orders.
2. **Reduce.** Instruments are taken in order, largest maintenance requirement first, with ties broken by instrument id.
   - For each instrument, the plan walks its liquidity from the best level: the highest bid when selling a long, the lowest ask when buying back a short.
   - On each level it takes the smallest number of `contract_step`s that meets the target (see "Search within a level" below). If no count on the level meets it, the plan takes the whole level.
   - A partial closeout is the normal case.
3. **Residual.** If the target is still not met, the plan reports what is left:
   - `unfilled`: the contracts still open, for each instrument whose liquidity ran out;
   - `deficit = max(0, target margin - collateral)`;
   - `debt = max(0, -cash)`.

   The simulator never erases a deficit.

`LiquidationPlan::intents()` gives an adapter stages 1 and 2: each `Cancel`, then one reduce-only `Reduce { instrument, side, contracts, limit_price }` per instrument. `contracts` is the exact sum of that instrument's forced fills. `limit_price` is the last (worst) level used.

### Target

| `CloseoutTarget` | Met when |
|---|---|
| `Maintenance` | `collateral >= mm` and `cash >= 0` |
| `Initial` | `collateral >= im` and `cash >= 0` |
| `Flat` | no position is open |

`cash >= 0` is the exit condition of `Default`. Equality meets the target, as it does in `assess`.

### Outcomes

| Outcome | Meaning |
|---|---|
| `AlreadyMet` | The target holds before any action. The plan is empty: no cancels and no fills |
| `TargetMet` | The forced fills meet the target |
| `FlatWithDeficit` | Every position is closed and the target is still missed, so the deficit and the debt stay explicit |
| `LiquidityExhausted` | Positions remain open, listed in `unfilled`, and the target is missed |

### Cash conventions

`c` is the rule's `cash_scale` (USDC: 6).

| Figure | Formula |
|---|---|
| basis | signed cash value of the open position at entry prices (a long has a positive basis) |
| unrealized PnL | `round_half_even(Qc * M * mark, c) - basis` |
| collateral | `cash + sum(unrealized PnL)`, as one exact sum |
| fill cash, `n` at `p` | `+round_half_even(p * n * M, c)` when selling, negative when buying |
| released basis | `round_half_even(basis * n / abs(Qc), c)`; a full close releases the whole basis |
| realized PnL | `fill cash - released` |
| fee | `trunc(fee_rate * p * n * M, c)`: one exact four-factor product, truncated toward zero |
| cash after a fill | `cash + realized - fee`, as one exact sum, booked even if it goes negative |

Marks stay those of the `ExposureView` for the whole plan. The scenario prices for the closeout are the liquidity levels.

An **inverse** position (contract value `CV` in quote units, settled in the base, which is then the plan's currency, under `TieredInverseIm`) uses its value in the base:

| Figure | Formula |
|---|---|
| basis | signed value in the base at entry prices (a long is positive) |
| unrealized PnL | `basis - round_half_even(Qc * CV / mark, c)` |
| fill value, `n` at `p` | `+round_half_even(n * CV / p, c)` when selling, negative when buying (`ForcedReduction::fill_cash`) |
| released basis | as above |
| realized PnL | `released - fill value` |
| fee | `trunc(fee_rate * n * CV / p, c)`, in the base |

### Search within a level

On one level, let `n` be the number of contracts closed.

- `cash(n)` is linear in `n`: the price, the released basis and the fee are all proportional to `n`.
- `excess(n) = collateral(n) - margin(n)` is concave. Collateral is linear in `n`, and the margin is convex in the notional, which falls linearly in `n`.

Unrounded, `min(cash, excess)` is concave, and the counts that meet the target form an interval. That interval need not reach the end of the level, because closing more can push cash below zero. The two cases below are both of this kind:

- two positions under a single-tier rule;
- one position under two increasing tiers.

In each, the plan must close 5 contracts. Closing all 10 would leave debt that could have been avoided.

The same holds for inverse positions. Their PnL is convex in the **price**, but within a level the price is fixed, so the fill value `n * CV / p`, the fee and the remaining notional `(|Qc| - n) * CV / mark` are linear in `n`.

The booked figures, however, are rounded: half-even fill value and released basis, a truncated fee, and a margin rounded up. With a coarse cash quantum, rounding can open windows of counts that meet the target where the unrounded figures do not. For example, in BTC at whole units, a long of 13 inverse contracts meets the initial margin after closing 4, but not after 6, and only again at 13. The plan finds the smallest count that meets the target **as booked**, with a certified search.

1. **Error bounds.** With `q` the cash quantum:
   - the booked cash differs from the unrounded one by less than `2q`: half a quantum for the fill value, half for the released basis, and under one for the fee;
   - the booked excess differs by less than `3q`: half a quantum for the fill value, half for the remaining value, under one for the fee and under one for the margin. The released basis cancels.

   Only the position being reduced changes; the others enter with their booked figures.
2. **Exact bounds.** The unrounded cash and excess are evaluated exactly, in wide integers, from the contract terms and the rule's tiers (`PiecewiseLinearMargin::margin_tiers`), never from the rounded margin.
   - `J = {cash >= -2q and excess >= -3q}` contains every count that meets the target.
   - `J' = {cash >= 2q and excess >= 3q}` contains only counts that meet it.

   Both are intervals of the grid: the cash is linear and the excess concave, peaking at an end of the level or next to a tier floor. Their ends are found by exact bisection; a floor is crossed at `n = |Qc| - floor / (M * mark)`, or `|Qc| - floor * mark / CV`, kept in wide integers.
3. **Walk.** The plan walks `J` upward, from its start to the start of `J'`, evaluating each count exactly as it would be booked, and takes the first that meets the target: the smallest on the level.
   - If `J` is empty, no count meets the target, and the plan takes the whole level.
   - A count that would leave a position no `Decimal` can hold cannot be booked, and is skipped.

The walk is short when the quantum is fine against what one step changes: a few counts for real cash quanta. All levels of one call share a budget of `SEARCH_BUDGET` = 4096 exact evaluations, which costs a few tens of milliseconds in an optimized build. When the budget runs out:

- **`J'` not empty.** The plan takes the first count of `J'`, which meets the target, and reports `LiquidationPlan::minimal == false`. The count before it is not certified: unrounded, its cash is below `2q` or its excess below `3q`. This bound is in money, not in contracts. With a step that changes almost nothing, the certified count can be many contracts above the smallest.
- **`J'` empty.** No count of the level is certified. The plan fails with `CloseoutSearchLimit { instrument, level, limit }`, and nothing is planned.

`tests/closeout_oracle.rs` checks the search against an exhaustive oracle that tries every count on the grid. It covers 19 200 cases: linear and inverse, with cash at 0, 1, 2 and 8 decimals, one to three levels (including levels where no count meets the target), one and two tiers, fees of 0 and 0.3 %, long and short positions, and cash swept across the target. The plan matches the oracle on every case. `tests/closeout_search.rs` names the two rounding windows (inverse and linear), the breakpoints whose numerator overflows a `Decimal`, and an exact minimum near `Decimal::MAX`.

Counts sit on the `contract_step` grid. Grid indices are wide integers, so `k * step`, `ceil(available / step)` and the bisection midpoints never overflow, even for `Decimal::MAX` contracts with a step of 0.5. Near the limits of `Decimal`, a fractional count with too many digits is rounded down to the scale of the cap.

## Ledger round trip

When the futures and perpetuals are held in the margin-rs `Ledger`:

1. `LiquidationAccount::from_ledger(model, snapshot, account, venue, currency, marks, as_of_ms)` builds the account. The caller sets `resting_orders` afterwards.
   - **Positions.** Every open future and perpetual, linear or inverse, with its contracts, the contract size of the ledger's instrument (multiplier or contract value) and its instrument mark. An inverse plan's currency is the base asset. The snapshot's instrument definitions are authoritative; marks never change them.
   - **Basis.** Each position's cost basis. A basis of zero (an entry notional rounded to zero) is valid; a non zero basis must have the sign of the position.
   - **Cash.** The **total** balance of `currency`, reservations included: a closeout is not limited to the available part.
   - **Left out.** Spot holdings, in `currency` or any other asset, are not part of the plan. Flat and settled derivatives take no part.
   - **Errors, never a skip.** An open derivative settled in another asset is `CurrencyMismatch`. One without a mark is `MissingMark`; a mark that is not positive is `InvalidInput`, and one with more than 18 decimals `PrecisionExceeded`. An open future whose expiry is at or before `as_of_ms` is `ExpiredUnsettled`: it can no longer trade (a forced fill would be `ExecutedAfterExpiry`) and must be settled with `Ledger::settle_expiry` first. The ledger scale of `currency` must equal the rule's `cash_scale`, so that the plan and the ledger round to the same quantum (`InvalidInput` otherwise).
2. `plan_liquidation` plans as usual.
3. Each `ForcedReduction::execution(execution_id, currency, executed_at_ms)` is the fill as an `Execution` (side, price, contracts, the fee in `currency`, the execution time), booked with `Ledger::book_execution_allowing_overdraft` (mandatory) or `RiskGate::book_derivative_execution`.

The ledger books a derivative fill with the cash conventions above, so the booked plan leaves exactly the cash, contracts and basis of `plan.after`. The fixtures in `tests/ledger_liquidation.rs` check this round trip, with reconcile, on:

- the fixture below (sell 3 at 94: cash 71.859, 7 contracts, basis 700);
- a closeout into debt (cash -150.35, `overdrafts` equal to `plan.debt`);
- half-quantum rounding at 2 decimals, long and short, with a partial close (released `rhe(100 / 3) = 33.33`) followed by a full close (the rest of the basis);
- a basis of zero;
- inverse closeouts in `tests/inverse_closeout.rs`, long and short (a long of 100 BTC-INV at 50000 marked at 45000 closes 30 at 44900 and 3 at 44500, leaving 67 contracts with basis 0.134 and cash 0.02240652 BTC).

## Ledger postings for positions outside the ledger

Each `ForcedReduction::settlement(execution_id, currency)` returns the `DerivativeSettlement` for that fill. `Ledger::book_settlement` books it:

- it is idempotent by execution id;
- it shares the id namespace with spot executions;
- it is never rejected for lack of balance.

The ledger fixtures check three things:

- **Partial closeout.** The ledger reconciles before and after. The balance after equals `plan.after.cash`. The PnL attribution gives realized -18 and fees 0.141, with no rounding difference.
- **Closeout into debt.** The balance goes to -150.35. The shortfall is reported, `overdrafts()` equals `plan.debt`, and the ledger still reconciles.
- **Idempotency and conflicts.** A duplicate changes nothing. A conflicting id, an id already used by a spot execution, or a spot instrument used as the settlement target are each rejected.

## Fixture (hand calculated)

The account holds 10 BTC-PERP long, `M = 1`, entry 100 (basis 1000), mark 95, with `cash = 90`. Then:

- collateral = 90 - 50 = 40;
- mm = 47.5;
- closing `n` at 94 gives collateral `40 - 1.047n` against mm `47.5 - 4.75n`.

| Case | Result |
|---|---|
| 3 at 94, rest at 93 | `n = 2` gives 37.906 < 38; `n = 3` gives 36.859 >= 33.25. The plan sells 3 at 94, and cash goes to 71.859 |
| cash 90.094 | `n = 2` gives exactly 38 = 38, which meets the target |
| only 2 at 94 | 37.906 against 38: `LiquidityExhausted`, 8 contracts unfilled, deficit 0.094 |
| step 0.5 | `n = 2.5`: 37.3825 >= 35.625 |
| target `Initial` | `n = 7`: 32.671 >= 28.5 |
| mark 80, cash 150, bids at 70 | the whole position is closed and cash goes to -150.35: `FlatWithDeficit`, debt 150.35 |

## Out of scope

- Insurance fund, loss allocation and auto-deleveraging. The debt and the deficit are left for them, explicitly.
- Options. `TieredLinearIm` covers only linear futures and perpetuals.
