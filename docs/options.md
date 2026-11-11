# Options

The ledger holds option positions, books their premium and their settlement, and takes everything about their value as input: premium marks, settlement prices, exercise and assignment, and the price of the underlying. Pricing an option stays outside this crate.

## Instrument

`instrument::OptionInstrument::new(LinearTerms, OptionContract)`:

| Term | Meaning |
|---|---|
| `LinearTerms` | Identifier, settlement asset, multiplier `M` (units of the underlying per contract), scale of the premium, scale of the contracts |
| `underlying` | Underlying asset. It need not be registered unless the option is physically delivered |
| `strike` | `K`, in the price asset per unit of the underlying |
| `expiry_ms` | Expiry, in milliseconds since the epoch |
| `right` | `Call` or `Put` |
| `exercise` | `European` or `American` |
| `delivery` | `Cash` or `Physical` |
| `margining` | `Linear`: settled in the price asset. `Inverse`: settled in the underlying, cash only |

Premiums and marks are always in the settlement asset per unit of the underlying. The strike and the settlement price are in the price asset. For an inverse option, the price asset is not an asset of the ledger.

`Instrument::from_spec` converts a `financial_types` option specification, linear or inverse. See `instrument-specs.md`.

## Premium

A position is signed contracts `Q` with a signed basis, as for a linear contract. A fill of `n` contracts at premium `p` has the value `N = rhe(p * n * M)`, rounded half-even to the settlement asset scale.

Unlike a future, the value **is** exchanged:

| | Cash | Basis |
|---|---|---|
| buy | `-N` | `+N` on opening: the premium paid |
| sell | `+N` | `-N` on opening: the premium received |

Reducing and crossing zero follow the same rules as a linear contract (`ledger.md`). The realized PnL moves no further cash, because the premium already did. `ExecutionCashflow::notional` is `N`. The identity `sum of signed N = realized - basis` holds and `Ledger::reconcile` checks it.

Fills are mandatory (`book_execution_allowing_overdraft`), need `executed_at_ms`, and must be executed before the expiry. A premium of zero is valid: a worthless option can trade at it. A negative one is refused.

## Settlement

`Ledger::settle_option(&OptionSettlement { id, instrument, contracts, outcome, as_of_ms })` closes `contracts` of the position, all or part of it. The caller gives the outcome; the ledger decides no exercise.

With `d = max(0, P - K)` for a call and `max(0, K - P)` for a put:

| Outcome | For | Value `V` | Cash in the settlement asset |
|---|---|---|---|
| `Cash { price: P }`, linear | Cash options | `rhe(n * M * d)` | `+V` long, `-V` short |
| `Cash { price: P }`, inverse | Cash options | `rhe(n * M * d / P)`, with its `InverseResidual` | `+V` long, `-V` short |
| `Delivery { spot, delivery_id }` | Physical options | `0` | None here: a spot execution at the strike |
| `Lapsed` | Physical options | `0` | None |

With `released = rhe(basis * n / abs(Q))`, all of it on a full close:

```text
realized = +V - released   for a long
realized = -V - released   for a short
```

Rules:

- **Delivery.** A spot execution of `n * M` at `K`, on a registered spot with the underlying as base and the settlement asset as quote. A long call and a short put buy; a long put and a short call sell. It is booked with the close of the option in one atomic step, overdraft allowed. The premium stays in the option's realized PnL and never enters the cost of the underlying.
- **A delivery is never rounded.** The strike and the quantity must fit the spot's grids and `K * n * M` must fit the settlement asset's scale, or the settlement is refused and nothing changes.
- **A cash option cannot lapse,** so a payoff is never dropped by leaving out its price. It settles at a price, which pays nothing at or out of the money.
- **Timing.** A European option settles at or after its expiry. An American option may settle before it, by price or by delivery, but lapses only at or after it.
- **Identifiers.** The settlement id and the delivery id share the namespace of execution identifiers. An identical settlement is a duplicate before any other check, and a different one under the same id is a conflict, also when it paid nothing. A delivery id must be unused.
- **Mandatory.** A debit beyond the available balance is booked and reported in `OptionSettlementBooking::shortfalls`, per asset.
- **No terminal state.** A fill executed before the expiry and delivered after a settlement reopens a position, which is settled again under another id. A fill executed at or after the expiry is refused. `settle_expiry` refuses options.

`book_settlement` remains for a derivative held outside the ledger, and refuses a registered option.

## Valuation

An open option needs a premium mark in `MarkSet::instruments`, or the valuation fails with `MissingMark`. A mark of zero is valid; a negative one is an error.

`market value = rhe(Q * M * mark)` and `upnl = market value - basis`. Because the premium already moved cash:

| | Future or perpetual | Option |
|---|---|---|
| Reporting equity adds | `upnl` | market value |
| Collateral counts | `upnl` | `min(market value, 0)` |
| An invalid mark leaves collateral unknown | on a loss | on a short, whatever the mark says |

So a short deducts its mark from both, and a long adds to equity but gives no collateral credit. `equity - net_transfers = realized + unrealized - fees` still holds.

## Margin: `short-option-im/1`

| | |
|---|---|
| Rule | `rules::ShortOptionIm::short_option_v1(currency, scale)` |
| Products | Options settled in the rule's asset. Only shorts require margin: a long's premium was paid in full |
| Specification | This document: the crate's own rule, not a venue's. A venue's rule gets its own identifier and cites its venue's text |
| Rates | `a` = 15 %, `b` = 10 %, `m` = 7.5 %. `ShortOptionIm::new` takes others under another `RuleId`, with `0 < b <= a <= 1` and `0 < m <= b` |
| Rounding | Each position rounded up once to the rule's scale; totals are exact sums |

With `U` the price of the underlying, given by the caller in the price asset, and `OTM = max(0, K - U)` for a call and `max(0, U - K)` for a put:

```text
linear, in the settlement asset:
    im = |Qc| * M * max(a * U - OTM, b * U)
    mm = |Qc| * M * m * U

inverse, in the underlying:
    im = |Qc| * M * max(a - OTM / U, b)
    mm = |Qc| * M * m
```

**The mark is not in the requirement.** A short's mark is its cost to close, and collateral already deducts it. Adding it here would count it twice.

**Its own view.** `ExposurePosition` carries neither a strike nor an underlying price, so the rule takes an `OptionExposureView`, which `from_snapshot` builds from a ledger, and is not an `InitialMarginModel`. It returns the usual `MarginRequirement`, which `maintenance::assess` reads like any other. An open option without an underlying price is `MissingMark`.

**No netting.** Each position stands alone. Netting between options and against the underlying belongs to portfolio rules.

## What does not cover options yet

These refuse an open option with `UnsupportedInstrument` instead of leaving it out, since leaving it out would overstate the account (the closeout plan supports European linear options under a rule that margins them, see `liquidation.md`):

- **Stress** (`scenarios::stress`): options are not stressed, shocked or filled hypothetically. Revaluing them needs a pricing model.
- **Closeout**: American and inverse options get no plan (`LiquidationAccount::from_ledger`), and a rule that does not margin options refuses the plan (`plan_liquidation`).
- **Margin reservations** (`MarginReservationService` on a ledger): an open option settled in the service's currency makes its requirement fail, because its rule does not cover options, so the service blocks orders in that currency. An option settled in another asset does not affect it, and neither does a flat one. An external hold does not lift this.
- **Pretrade risk gate** (`RiskGate::submit`): orders are for spot. Its option fills and settlements are mandatory postings (`book_derivative_execution`, `settle_option`).
