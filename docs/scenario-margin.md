# Scenario portfolio margin: `scenario-pm/1`

`rules::ScenarioPm` nets an account's linear futures, perpetuals and options over a grid of scenarios the caller supplies. It prices nothing: each scenario gives every leg its value as an exact decimal (the shocked price of a linear leg, the repriced premium of an option). No floats, checked arithmetic, ordered maps only.

## Inputs

- A leg (`PmLeg`): instrument, kind (`Linear`, or `Option { underlying_price }`), signed contracts `Q`, multiplier `M`, reference value `X0` (mark price or premium mark).
- Two scenario sets (`ScenarioSets`), one for the initial figure and one for the maintenance figure, each non empty. A scenario maps every leg's instrument to its value `X_s`.
- Floor rates per figure (`ScenarioPmRates`), each in `[0, 1]`: `f` on short options, `g` on linear legs.

## Formula

```text
loss_i(s) = -Q_i * M_i * (X_s,i - X0_i)
R_s       = sum over legs of loss_i(s)
F         = f * sum over short options of |Q| * M * U
G         = g * sum over linear legs of |Q| * M * X0
figure    = round_up( max(0, max over s of R_s) + F + G )
```

- `R_s` is the linear loss plus the repricing loss of short options minus the value change of long options, in one signed sum, so offsetting legs net.
- Every term is exact; each figure is rounded up once per account.
- The worst scenario is the one with the largest `R_s`; on a tie, the lowest index.
- One linear position with `g = 0` and extreme moves `X0 * (1 +- r)` gives `round_up(r * |Q| * M * X0)`, the single-tier `linear-im/1` figure.

## Admission

`ScenarioPm::admission_requirement` counts resting orders (`PmOrder`: signed contracts `q`, positive to buy, and a limit `L`) as filled at their limit, worst case over every subset that fills:

```text
c_o(s) = -q * M * (X_s - L)
R'_s   = R_s + sum over orders of max(0, c_o(s))
F'     = f * sum over options of max(0, -(Q + sum of its sell orders)) * M * U
G'     = g * sum over linear legs of (|Q| + sum of |q| of its orders) * M * X0
```

Contributions are additive per scenario, so the worst subset for `s` takes exactly the orders with `c_o(s) > 0`. `F'` assumes every option sell fills and no buy; `G'` adds every linear order's contracts to the gross. Each is at least its value after any subset of fills, so the admission figure is never below the requirement of any fill subset (checked against a brute force in `tests/scenario_pm.rs`). Every order needs a leg for its instrument, with zero contracts if none is held.

## Version 2: `scenario-pm/2`

`ScenarioPm::scenario_pm_v2` keeps everything of version 1 and changes two figures, for venues whose collateral gives long options no credit and whose linear floor should charge only spread exposure:

```text
long option:  loss_i(s) = -Q * M * X_s     (its full scenario value)
G   = g * (gross - |net|)
G'  = g * (gross + O - max(0, |net| - O_against))
```

`gross = sum |Q| * M * X0` and `net = sum Q * M * X0` over linear legs; `O` is the notional `|q| * M * X0` of every linear order at its leg's mark and `O_against` that of the orders against the sign of `net`. A single linear position pays no `G`, so with extreme moves `X0 * (1 +- r)` the figure equals `linear-im/1` at rate `r` whatever `g`; a calendar spread of equal and opposite notionals `N` pays `g * 2N`. `G'` bounds `G` after any subset of fills. An option leg is long or short by the position held, not the post-fill one: in admission an order is valued by its loss `-q * M * (X_s - L)` against the legs as held. Version 1 keeps its formulas and id.

## Breakdown

`ScenarioPmRequirement` carries the usual `MarginRequirement` and, per figure, a `PmBreakdown`: the worst scenario's index, the exact per-leg and per-order terms in it, `max(0, R_s)`, `F`, `G` and the rounded figure. In the `MarginRequirement`, each figure is attributed whole to the leg with the largest term in its worst scenario (the first in view order on a tie) and zero to the other lines, so lines add up to the totals.

## As a model

`ScenarioPm` implements `InitialMarginModel` over an `ExposureView` of linear futures, perpetuals and options: each linear position is a linear leg at its mark, each option an option leg at its premium mark with the underlying price given to `ScenarioPm::with_underlying_prices` (an `ExposurePosition` carries none; without it the option is `MissingMark`). The price only feeds the short option floor: the option's scenario values remain the caller's repriced premiums, exact decimals the crate never computes. Inverse positions are `UnsupportedInstrument`. It implements `AccountMargin` without tiers, so `plan_liquidation` can drive a closeout under it, evaluating counts one by one (see `docs/liquidation.md`). The scenario values stay fixed while the plan shrinks positions.
