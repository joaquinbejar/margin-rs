# Portfolio covariance risk (M3-02)

`covariance::portfolio_variance(&[FactorExposure], &ValidatedCovariance, scale) -> PortfolioVariance`

## Units

A `Factor` has a `FactorKind`, which fixes what one unit move of that factor is:

| Kind | One unit | Exposure |
|---|---|---|
| `PriceReturn` | relative price change of 1 (100 %) | cash delta: currency per unit return |
| `VolatilityPoint` | +1 implied volatility point (0.01) | vega: currency per vol point |
| `RatePoint` | +1 basis point | DV01: currency per basis point |

A `FactorExposure` states the factor, the kind it was measured in, its currency and its amount. An exposure whose kind differs from its factor's kind is rejected with `FactorUnitMismatch`. In particular, **a vega exposure is never read as price variance.** This crate does not carry over the market-maker-rs vega-squared proxy.

The covariance is per horizon, in each factor's units. Cross-covariances, for example between price and volatility, are allowed. The variance is in currency squared per horizon.

## Validation and regularization decision

`ValidatedCovariance::new` checks the matrix:

- one row and one column per factor, with unique factor ids;
- exact symmetry;
- exact positive semidefiniteness: non-negative diagonal and, for two factors, `c11 * c22 - c12^2 >= 0`, computed exactly.

`from_correlation` builds `c_ij = rho_ij * sigma_i * sigma_j` as one exact product, so only the covariance itself must be representable. It checks that each `sigma >= 0`, that every `rho` is in `[-1, 1]`, and that the diagonal of `rho` is 1.

**Decision.** An invalid matrix is **rejected** with `InvalidCovariance`. It is never regularized: no eigenvalue clipping and no shrinkage. Estimation and repair are the caller's job.

**Scope.** This version supports one and two factors, where the exact positive-semidefinite test has a closed form. Larger matrices are rejected until an exact check for them is implemented.

## Variance

Exposures to the same factor net, so offsetting positions offset. The net per factor is an exact sum, kept in wide integers until the final rounding. It is independent of the order of the exposures, and neither partial sums nor the nets themselves need to be representable. `PortfolioVariance::net_exposures` reports a net as `None` when it is not a `Decimal`. The function computes `e' C e` over at most `n * n` terms, exactly, and rounds it **up** once to `scale`. Time is O(exposures) and memory is bounded by the number of factors. Because `C` is positive semidefinite, the exact value is never negative, so the result is never negative either. A negative value would be reported as an error, never returned.

Scenario revaluation is a separate interface (`scenarios`, M2-08).

## Reference

Closed-form approximations in multi-asset market making (arXiv:1810.04383v5). It uses `e' C e` as the inventory risk term.
