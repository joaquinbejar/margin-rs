# Maintenance and liquidation (M3-04)

This document covers two modules and one ledger posting:

- `maintenance::assess` computes the account state that the maintenance margin of a rule implies.
- `liquidation::plan_liquidation` simulates one staged closeout against supplied scenario prices and liquidity. There is no live liquidator.
- `Ledger::book_settlement` posts the cash of each forced fill (see `docs/ledger.md`).

## Who owns what

| Concern | Owner |
|---|---|
| Liquidation trigger, account state, cancelling resting orders | the venue, when it keeps its own account state |
| What to close, in what order, size limits, outcome against book liquidity | margin-rs: `LiquidationPlan` |
| Journal of record when the venue keeps its own ledger | the venue's ledger; margin-rs reads its collateral figures (`CollateralView`) |
| Backtests and accounts without a venue ledger | margin-rs `Ledger`, with `book_settlement` |

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
- each position's IM and MM are convex, non-decreasing, piecewise linear functions of its notional, and the slope changes only at `notional_breakpoints` (the tier floors);
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

### Search within a level

On one level, let `n` be the number of contracts closed.

- `cash(n)` is linear in `n`: the price, the released basis and the fee are all proportional to `n`.
- `excess(n) = collateral(n) - margin(n)` is concave. Collateral is linear in `n`, and the margin is convex in the notional, which falls linearly in `n`.

So `min(cash, excess)` is concave, and the counts that meet the target form an interval `[a, b]`. That interval need not reach the end of the level, because closing more can push cash below zero. The two cases below are both of this kind:

- two positions under a single-tier rule;
- one position under two increasing tiers.

In each, the plan must close 5 contracts. Closing all 10 would leave debt that could have been avoided.

The plan finds `a` exactly, up to the rounding of single cash quanta, in three steps:

1. It evaluates the target exactly at the points where a concave piecewise linear function can peak:
   - both ends of the level;
   - the grid points on either side of each margin breakpoint;
   - the grid points on either side of each sign change of `excess - cash` within a linear segment, found by bisecting on that sign.
2. If none of these points meets the target, no count on the level does, and the plan takes the whole level.
3. Otherwise it bisects between 0, which does not meet the target, and the smallest candidate that does. The result is a count that meets the target while the count one grid step below it does not.

Counts sit on the `contract_step` grid. Grid indices are wide integers, so `k * step`, `ceil(available / step)` and the bisection midpoints never overflow, even for `Decimal::MAX` contracts with a step of 0.5. Near the limits of `Decimal`, a fractional count with too many digits is rounded down to the scale of the cap.

## Ledger postings

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
- Perpetual and futures positions inside the margin-rs `Ledger` (option B). That is follow-up work for M3-INT.
- Options. `TieredLinearIm` covers only linear futures and perpetuals.
