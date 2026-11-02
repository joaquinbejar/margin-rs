# margin-rs

Risk and margin engine for the Xynthex market making lab; usable standalone.

## Status

Bootstrap. The crate builds and exports only its typed error boundary:

| Export | Purpose |
|---|---|
| `MarginError` | Error enum (`#[non_exhaustive]`) with typed variants: `InvalidInput { field, reason }` and `Arithmetic { operation }`. |
| `Result<T>` | Alias for `core::result::Result<T, MarginError>`. |

There is no ledger, pretrade risk check or margin model yet. They are tracked
in the GitHub issues (M1-04, M1-05 and later milestones).

## Usage

The crate is not published. Depend on it by git:

```toml
[dependencies]
margin-rs = { git = "https://github.com/joaquinbejar/margin-rs" }
```

```rust
use margin_rs::{MarginError, Result};

fn positive(field: &'static str, value: i64) -> Result<i64> {
    if value > 0 {
        Ok(value)
    } else {
        Err(MarginError::invalid_input(field, "must be positive"))
    }
}
```

## Policies

- Rust edition 2024, MSRV 1.85. CI runs the latest stable and the MSRV.
- Small default dependency set (`thiserror` only). margin-rs never depends on
  market-maker-rs or tessera-rl.
- No binary floating point arithmetic: `clippy::float_arithmetic` is denied
  crate wide. Money, prices and quantities use exact decimals with checked
  operations.
- No `unsafe` code.

## Development

```sh
make pre-push   # fmt-check, clippy -D warnings, tests, docs -D warnings, MSRV check
make fmt        # format the code
```

## License

MIT
