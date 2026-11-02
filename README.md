# margin-rs

Risk and margin engine for the Xynthex market making lab; usable standalone.

## Status

M1-04: the single accounting ledger for one account. M1-05: the pretrade
risk gate that every order passes through. M3-03B: the first versioned
initial margin rule.

| Export | Purpose |
|---|---|
| `ledger::Ledger` | Accounting authority: balances per asset (available and reserved), positions per instrument with signed average cost, fees per asset, idempotent booking by execution id, cashflow per execution, snapshots, reconciliation. |
| `ledger::Valuation` | Mark to market by asset under an explicit policy; `net_in` is the only conversion between assets. |
| `risk::RiskGate` | Pretrade gate that owns the ledger: reservations for every order, hard limits on the joint exposure of positions and live orders, typed `RiskDecision` (accepted, reduced, rejected), circuit breaker with quote withdrawal. |
| `risk::InstrumentLimits`, `OrderRequest`, `RiskReason`, `HaltReason` | Limits, requests and typed reasons. |
| `valuation::value` | Account mark to market in one reporting asset over a `LedgerSnapshot`: price source and freshness flags, collateral equity that invalid marks cannot raise, PnL with fees counted once. |
| `scenarios::stress` | Spot inventory stress: hypothetical fills (hedges), price gaps, liquidation spread. |
| `initial_margin::InitialMarginModel`, `rules::TieredLinearIm` | Versioned margin rules; `xynthex-linear-im/1` for linear USDC futures and perpetuals, with the rule id and a per position breakdown in every requirement. |
| `factors`, `covariance::portfolio_variance` | Factor exposures with explicit units (price return, vol point, rate point) and portfolio variance under a validated, exactly positive semidefinite covariance matrix (one and two factors). |
| `collateral::value_collateral` | Available collateral of one account with per asset haircuts, eligibility and a declared stale price policy; debts always count. |
| `margin_reservations::MarginReservationService` | Initial margin reserved for pending derivative orders of one account: exact worst case requirement, idempotent fills and effective cancels, typed rejection reasons. |
| `maintenance::assess` | Account state from the maintenance threshold (healthy, reduce only, default; equality is healthy) with excess, shortfall and debt. |
| `liquidation::plan_liquidation` | Staged closeout plan (cancel, reduce, residual) against scenario liquidity: partial closeouts, fees, explicit deficit and debt, adapter intents. |
| `ledger::DerivativeSettlement` | Cash of a derivative fill (realized PnL and fee), booked idempotently by execution id and never rejected. |
| `SpotInstrument`, `Instrument` | Instrument definitions (spot only). |
| `Asset`, `AccountId`, `VenueId`, `ExecutionId`, `InstrumentId`, `OrderId`, `TransferId`, `ReservationId` | Validated identifiers. |
| `Rounding` | Rounding mode for the notional and the released cost. |
| `MarginError`, `Result<T>` | Typed error boundary (`#[non_exhaustive]`). |
| `Decimal` | Re-export of `rust_decimal::Decimal`. |

The full contract, the rounding and valuation policies and the mapping from
market-maker-rs are in [docs/ledger.md](docs/ledger.md); the gate's rules
and its market-maker-rs mapping are in [docs/risk-gate.md](docs/risk-gate.md);
valuation and stress are in [docs/valuation.md](docs/valuation.md); margin
rules are in [docs/initial-margin.md](docs/initial-margin.md); factor risk is
in [docs/covariance.md](docs/covariance.md); collateral is in
[docs/collateral.md](docs/collateral.md); margin reservations are in
[docs/margin-reservations.md](docs/margin-reservations.md); maintenance and
liquidation are in [docs/liquidation.md](docs/liquidation.md); the exact
arithmetic rule is in [docs/exact-arithmetic.md](docs/exact-arithmetic.md).

## Usage

The crate is not published. Depend on it by git:

```toml
[dependencies]
margin-rs = { git = "https://github.com/joaquinbejar/margin-rs" }
```

```rust
use margin_rs::ledger::{Execution, Ledger, Side};
use margin_rs::{Asset, Decimal, ExecutionId, InstrumentId, SpotInstrument, TransferId};

fn main() -> margin_rs::Result<()> {
    let usdc = Asset::new("USDC")?;
    let btc = Asset::new("BTC")?;
    let pair = InstrumentId::new("BTC-USDC")?;

    let mut ledger = Ledger::default();
    ledger.register_asset(usdc.clone(), 6)?;
    ledger.register_asset(btc.clone(), 8)?;
    ledger.register_instrument(SpotInstrument::new(pair.clone(), btc, usdc.clone(), 2, 8)?)?;
    ledger.deposit(TransferId::new("d1")?, usdc.clone(), Decimal::from(1000))?;

    let buy = Execution::new(ExecutionId::new("e1")?, pair, Side::Buy, Decimal::from(100), Decimal::from(2))
        .with_fee(usdc.clone(), Decimal::new(2, 1)); // 0.2 USDC
    ledger.book_execution(&buy)?;
    ledger.book_execution(&buy)?; // duplicate: ignored

    assert_eq!(ledger.balance(&usdc).map(|b| b.total), Some(Decimal::new(7998, 1)));
    assert!(ledger.reconcile()?.is_clean());
    Ok(())
}
```

## Policies

- Rust edition 2024, MSRV 1.85. CI runs the latest stable and the MSRV.
- Small dependency set: `thiserror` and `rust_decimal` (without default
  features). margin-rs never depends on market-maker-rs or tessera-rl.
  `Cargo.lock` is committed so CI, including the MSRV job, is reproducible.
- No binary floating point arithmetic: `clippy::float_arithmetic` is denied
  crate wide. Money, prices and quantities are exact decimals; every
  operation is checked, and the only rounding steps are explicit.
- No `unsafe` code.

## Development

```sh
make pre-push   # fmt-check, clippy -D warnings, tests, docs -D warnings, MSRV check
make fmt        # format the code
```

## License

MIT
