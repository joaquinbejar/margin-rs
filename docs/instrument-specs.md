# Instrument specifications

With the `instruments` feature, `Instrument::from_spec(&InstrumentSpec, InstrumentScales)` converts a `financial_types` instrument specification into a ledger instrument. `TryFrom<(&InstrumentSpec, InstrumentScales)>` is the same conversion.

## Mapping

The conversion is exact: nothing is rounded, rescaled or substituted.

| Specification | Instrument | Contract size |
|---|---|---|
| linear spot | `SpotInstrument`: base the underlying, quote the price asset | |
| linear future | `LinearFuture`, settled in the settlement asset | multiplier: the size amount |
| linear perpetual | `LinearPerpetual` | multiplier: the size amount |
| inverse future | `InverseFuture`, settled in the settlement asset (the underlying) | contract value: the size amount |
| inverse perpetual | `InversePerpetual` | contract value: the size amount |
| option | none: `MarginError::UnsupportedSpec` | |

The identifier and the assets go through `InstrumentId::new` and `Asset::new`. The expiry, in milliseconds since the epoch, is passed unchanged.

The conversion is not reversible. An instrument does not carry:

- the tick schedule or the lot;
- the underlying of a linear derivative;
- the price asset of an inverse derivative, which is not a ledger asset;
- option terms.

## Scales

`price_scale` caps the digits of execution prices and expiry settlement prices. `contract_scale` caps the digits of contract quantities (the quantity scale for spot).

The scales are explicit. Venues publish marks and delivery prices with more digits than the tick: a tick of 0.5 with a delivery price of 61234.57 needs a price scale of 2.

Each scale must be at least the digits of the finest price tick, or of the lot, with trailing zeros ignored. Otherwise the conversion returns `MarginError::ScaleBelowGrid`.

The scales do not enforce the grid. The ledger books a price of 60000.3 on a 0.5 tick, and a size of 0.01 against a lot of 0.25, because both have digits within scale. Use `InstrumentSpec::check_price` and `check_lot` for the grid.

The lot is not the risk minimum either. With `min_quantity` unset, the risk gate's minimum is `10^-quantity_scale`, so set `min_quantity` to the lot.

## Errors

The checks run in this order, after the identifier goes through `InstrumentId::new`. A validated specification's identifier always passes, because both crates apply the same rule (1 to 64 bytes, printable ASCII without whitespace).

1. `UnsupportedSpec` for an option.
2. `ScaleBelowGrid`.
3. `InvalidInput` from the instrument constructors: a scale above 18, or a contract value whose digits plus `contract_scale` exceed 28.

`Ledger::register_instrument` then checks what needs asset scales:

- every asset of the instrument is registered;
- the spot quantity scale does not exceed the base asset scale;
- for an inverse instrument, the settlement asset scale plus `price_scale` does not exceed 28.

## Inverse PnL: per leg versus net

`financial_types` computes the realized PnL of an inverse contract as one exact amount, `±n·CV·(1/entry - 1/exit)`, and settles it with one rounding.

The ledger rounds each fill's value instead: `V = rhe(n·CV/p)`, half even at the settlement asset scale. The realized PnL is the difference of the rounded values (see [ledger](ledger.md)).

Both policies are exact, and they can differ by one unit of the settlement asset, in either direction. With CV 10 USD and BTC at 8 decimals:

| Case | Exact | Settled once | Ledger |
|---|---|---|---|
| long 1000, 60000 to 61000 | 1/366 | 0.00273224 | 0.16666667 - 0.16393443 = 0.00273224 |
| long 100, 60000 to 61000 | 1/3660 | 0.00027322 | 0.01666667 - 0.01639344 = 0.00027323 |
| short 100, 60000 to 61000 | -1/3660 | -0.00027322 | -0.00027323 |
| long 100, 60000 to 59000 | -1/3540 | -0.00028249 | 0.01666667 - 0.01694915 = -0.00028248 |

Nothing is lost in the bookings. Each fill records the exact rest of its value in `DerivativeFill::residual`, as an `InverseResidual`:

- `n·CV/p = V + numerator/denominator`;
- `numerator = n·CV - V·p`, in the quote currency of CV;
- `denominator = p`.

The open fill holds the open leg's rest and the close fill holds the close leg's. For `s = +1` (long) or `-1` (short):

`s·n·CV·(1/entry - 1/exit) = realized + s·(r_open - r_close)`

`tests/instrument_spec.rs` checks this identity exactly, with hand literals, and `tests/instrument_fixtures.rs` checks it on the shared fixtures. This is the ledger's existing policy; the feature does not change it.
