# margin-rs

Risk and margin engine for the Xynthex market making lab; usable standalone.

## Status

M1-04: the single accounting ledger for one account. Pretrade risk checks
and margin models are not implemented yet.

| Export | Purpose |
|---|---|
| `ledger::Ledger` | Accounting authority: balances per asset (available and reserved), positions per instrument with signed average cost, fees per asset, idempotent booking by execution id, cashflow per execution, snapshots, reconciliation. |
| `ledger::Valuation` | Mark to market by asset under an explicit policy; `net_in` is the only conversion between assets. |
| `SpotInstrument`, `Instrument` | Instrument definitions (spot only). |
| `Asset`, `ExecutionId`, `InstrumentId`, `TransferId`, `ReservationId` | Validated identifiers. |
| `Rounding` | Rounding mode for the notional and the released cost. |
| `MarginError`, `Result<T>` | Typed error boundary (`#[non_exhaustive]`). |
| `Decimal` | Re-export of `rust_decimal::Decimal`. |

The full contract, the rounding and valuation policies and the mapping from
market-maker-rs are in [docs/ledger.md](docs/ledger.md).

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
    ledger.reconcile()
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
