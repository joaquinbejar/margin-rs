[![License](https://img.shields.io/badge/license-MIT-blue)](./LICENSE)
[![Crates.io](https://img.shields.io/crates/v/margin-rs.svg)](https://crates.io/crates/margin-rs)
[![Downloads](https://img.shields.io/crates/d/margin-rs.svg)](https://crates.io/crates/margin-rs)
[![CI](https://github.com/joaquinbejar/margin-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/joaquinbejar/margin-rs/actions/workflows/ci.yml)
[![Stars](https://img.shields.io/github/stars/joaquinbejar/margin-rs.svg)](https://github.com/joaquinbejar/margin-rs/stargazers)
[![Issues](https://img.shields.io/github/issues/joaquinbejar/margin-rs.svg)](https://github.com/joaquinbejar/margin-rs/issues)
[![PRs](https://img.shields.io/github/issues-pr/joaquinbejar/margin-rs.svg)](https://github.com/joaquinbejar/margin-rs/pulls)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-orange)](./rust-toolchain.toml)
[![Documentation](https://img.shields.io/badge/docs-latest-blue.svg)](https://docs.rs/margin-rs)

# margin-rs

A risk and margin engine for market making and trading systems, written in
Rust with exact decimal arithmetic throughout. It provides a single
accounting ledger, a pretrade risk gate, mark to market valuation and stress,
versioned margin rules, collateral with haircuts, margin reserved for pending
orders, maintenance states, and staged closeout plans. It can be used on its
own or as the accounting and risk layer of a market making stack:
[market-maker-rs](https://github.com/joaquinbejar/market-maker-rs) uses it
optionally through its `margin` feature, pinned by git to an earlier
revision; updating it to this release is pending.

## Overview

A trading system needs one place that knows how much cash and inventory an
account has, what is committed to live orders, and how far the account is
from its margin limits. margin-rs is that place:

- every fill is booked once, by execution id, into one ledger; nothing else
  keeps a second balance that could drift;
- every order passes a pretrade gate that reserves what it could spend,
  before it reaches a venue;
- margin requirements come from versioned rules, and every figure carries
  the rule that produced it and a per position breakdown;
- when an account falls below maintenance, the crate reports its state and
  computes a staged closeout plan, leaving any deficit explicit.

Every amount is an exact `Decimal`: no binary floating point, no silent
rounding, and no partial sum that can overflow while the result does not.

## Features

### Accounting
- **Single ledger** per account: balances per asset split into reserved and
  available, positions with signed average cost and realized PnL, fees per
  asset (rebates as negative fees).
- **Futures and perpetuals in the ledger, linear and inverse**: signed
  contracts with a basis, realized PnL in cash on reductions, mark to
  market, funding and expiry settlement, always with half-even rounding.
  An inverse contract settles in the base asset: a fill of `n` contracts at
  `p` is worth `rhe(n * CV / p)` there, and the exact rest of each value is
  recorded.
- **Idempotent booking** by execution and transfer id; reusing an id for
  different content is an error. Every operation is atomic.
- **Cashflow per execution** and a full **reconciliation** of the state from
  the journal.
- **Mandatory postings** that are never lost: a real execution, a funding
  payment, an expiry or a derivative settlement that overdraws the account
  is booked and the shortfall is reported. A real fill delivered late is
  booked and flagged; only a causally impossible report is an error.

### Pretrade risk
- **Risk gate** that owns the ledger: reservations for every spot order,
  sized so that no fragmentation of fills can exceed them. Derivative
  orders reserve margin instead (see Margin).
- **Fees in the quote or the base asset** (`InstrumentLimits::fee_asset`):
  a fee in the asset an order pays is reserved with it; a fee in the asset
  it receives is netted from that credit in the same booking.
- **Declared fee rounding** for fees in base (`InstrumentLimits::fee_rounding`):
  per fill (a mode and a minimum fill) or cumulative at an exact rate, so a
  sell reserves the declared fee bound instead of the worst case. A fill
  outside the declaration is booked, reported, and stops new orders under
  that declaration until it is reset.
- **Hard limits** on the joint exposure of positions and live orders: order
  size, position, notional, open orders, price band and market data age.
- **Typed decisions** (accepted, reduced, rejected) with every reason.
- **Circuit breaker** with quote withdrawal, also when the gate halts itself
  on an accounting breach or an execution anomaly.

### Valuation and stress
- **Mark to market** in one reporting asset with price source and freshness
  flags; collateral equity that invalid marks can never raise. Open futures
  and perpetuals, linear and inverse, are valued at instrument marks, and
  collateral counts `cash + sum(upnl)` per settlement asset as one net
  amount.
- **PnL attribution** into realized, unrealized and fees, counted once.
- **Stress** of spot inventory and open futures and perpetuals:
  hypothetical fills (hedges, spot or derivative), asset and instrument
  shocks and a liquidation spread, from the baseline marks, which are never
  changed. Inverse positions are valued but not stressed yet: an open one
  is refused with `UnsupportedInstrument`.

### Margin
- **Versioned margin rules** with progressive tiers: `TieredLinearIm`
  (`linear-im/1`: initial 10 %, maintenance 5 % of `|Q| * M * mark`) for
  linear futures and perpetuals, and `TieredInverseIm` (`inverse-im/1`: the
  same rates on `|Q| * CV / mark`, in the base) for inverse ones. Each
  margin is one exact value rounded up once.
- **Factor risk**: exposures with explicit units and portfolio variance
  under a validated, exactly positive semidefinite covariance matrix.
- **Collateral** with per asset haircuts, eligibility and a declared stale
  price policy; debts always count.
- **Margin reservations** for pending derivative orders, linear or
  inverse, on positions kept by the service or read from the gate's ledger,
  with one cash pool shared with spot orders, so no unit of cash is
  reserved twice.

### Maintenance and liquidation
- **Account state** (healthy, reduce only, default) from the maintenance
  threshold; equality is healthy, unknown figures never are.
- **Staged closeout plan**: cancel resting orders, reduce positions,
  linear or inverse, against scenario liquidity with the smallest closeout
  that restores the target, then report what is left (unfilled size,
  deficit and debt).
- **Certified search**: the smallest count of each level is found as the
  ledger would book it, within a budget of `SEARCH_BUDGET` evaluations per
  plan. Beyond it the plan takes the first count certified to meet the
  target, and `LiquidationPlan::minimal` is `false` when that count is not
  proven the smallest; with no certified count the plan fails with
  `CloseoutSearchLimit`.
  A `contract_step` whose counts are not exact at its scale is refused.
- **From and back to the ledger**: `LiquidationAccount::from_ledger` reads
  the account, and `ForcedReduction::execution` books each forced fill, so
  the booked plan leaves exactly `LiquidationPlan::after`. Positions kept
  outside the ledger post their cash as derivative settlements.

## Modules

| Module | Purpose |
|---|---|
| `ledger` | `Ledger`: balances, spot positions and linear and inverse derivative positions, fees, idempotent booking, funding, expiry, cashflows, snapshots, reconciliation, valuation by asset, legacy derivative settlements. |
| `risk` | `RiskGate`: pretrade gate for spot orders, reservations with fees in quote or base, hard limits, typed decisions, circuit breaker and quote withdrawal; mandatory derivative postings. |
| `valuation` | Account mark to market in one reporting asset over a `LedgerSnapshot`, PnL attribution, collateral equity. |
| `scenarios` | Stress of spot inventory and open linear futures and perpetuals. |
| `initial_margin`, `rules` | `InitialMarginModel`, `AccountMargin`, `PiecewiseLinearMargin`, `TieredLinearIm` (`linear-im/1`), `TieredInverseIm` (`inverse-im/1`), `ShortOptionIm` (`short-option-im/1`) and `ScenarioPm` (`scenario-pm/1`, scenario portfolio margin, `docs/scenario-margin.md`). |
| `factors`, `covariance` | Factor exposures with units and portfolio variance. |
| `collateral` | Collateral value with haircuts and a stale price policy. |
| `margin_reservations` | Initial margin reserved for pending derivative orders, own positions or on the ledger. |
| `maintenance` | Account state from the maintenance threshold. |
| `liquidation` | Staged closeout plan with a certified search, from and back to the ledger, and adapter intents. |
| `decimal` | Exact wide integer arithmetic and the `Rounding` modes. |
| `instrument` | Ledger instruments: spot, linear and inverse futures and perpetuals; with the `instruments` feature, `Instrument::from_spec` converts a `financial_types` instrument specification. |
| `error` | `MarginError` and `Result`: the typed error boundary. |

The detailed contracts are in [`docs/`](docs):
[ledger](docs/ledger.md), [risk gate](docs/risk-gate.md),
[valuation and stress](docs/valuation.md),
[initial margin](docs/initial-margin.md), [covariance](docs/covariance.md),
[collateral](docs/collateral.md),
[margin reservations](docs/margin-reservations.md),
[maintenance and liquidation](docs/liquidation.md),
[instrument specifications](docs/instrument-specs.md) and the
[exact arithmetic rule](docs/exact-arithmetic.md).

## Installation

Add the crate from crates.io:

```toml
[dependencies]
margin-rs = "0.1"
```

### Feature flags

| Feature | Default | Effect |
|---|---|---|
| `instruments` | off | `Instrument::from_spec` and `InstrumentScales`: an exact conversion of a `financial_types` instrument specification (spot, linear and inverse futures and perpetuals, options) into a ledger instrument, with explicit scales and typed errors (`ScaleBelowGrid`, `UnsupportedSpec` for what is not mapped). Enables `financial_types/instruments`. |

## Examples

### Ledger

```rust
use margin_rs::ledger::{Execution, Ledger, Action};
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

    let buy = Execution::new(ExecutionId::new("e1")?, pair, Action::Buy, Decimal::from(100), Decimal::from(2))
        .with_fee(usdc.clone(), Decimal::new(2, 1)); // 0.2 USDC
    ledger.book_execution(&buy)?;
    ledger.book_execution(&buy)?; // duplicate: ignored

    assert_eq!(ledger.balance(&usdc).map(|b| b.total), Some(Decimal::new(7998, 1)));
    assert!(ledger.reconcile()?.is_clean());
    Ok(())
}
```

### Pretrade risk gate

```rust
use margin_rs::ledger::{Execution, Ledger, Action};
use margin_rs::risk::{FillReport, InstrumentLimits, OrderRequest, RiskGate};
use margin_rs::{Asset, Decimal, ExecutionId, InstrumentId, OrderId, SpotInstrument, TransferId};

fn main() -> margin_rs::Result<()> {
    let usdc = Asset::new("USDC")?;
    let btc = Asset::new("BTC")?;
    let pair = InstrumentId::new("BTC-USDC")?;
    let mut ledger = Ledger::default();
    ledger.register_asset(usdc.clone(), 6)?;
    ledger.register_asset(btc.clone(), 8)?;
    ledger.register_instrument(SpotInstrument::new(pair.clone(), btc, usdc.clone(), 2, 8)?)?;

    let mut gate = RiskGate::new(ledger);
    gate.deposit(TransferId::new("d1")?, usdc.clone(), Decimal::from(1000))?;
    gate.set_limits(&pair, InstrumentLimits::default())?;

    // 1000 USDC: a buy of 6 at 100 reserves 600; a second one does not fit.
    let first = OrderRequest::new(OrderId::new("o1")?, pair.clone(), Action::Buy, Decimal::from(100), Decimal::from(6));
    let second = OrderRequest::new(OrderId::new("o2")?, pair.clone(), Action::Buy, Decimal::from(100), Decimal::from(6));
    assert!(!gate.submit(&first, 0).is_rejected());
    assert!(gate.submit(&second, 0).is_rejected());

    // The fill consumes the reservation exactly once.
    let fill = Execution::new(ExecutionId::new("e1")?, pair, Action::Buy, Decimal::from(100), Decimal::from(6));
    assert!(gate.on_fill(&OrderId::new("o1")?, &FillReport::new(fill, Decimal::from(6)))?.is_clean());
    assert_eq!(gate.ledger().balance(&usdc).map(|b| b.available), Some(Decimal::from(400)));
    Ok(())
}
```

### Margin, maintenance and closeout

```rust
use std::collections::BTreeMap;

use margin_rs::initial_margin::{ExposurePosition, ExposureView, InitialMarginModel, ProductKind};
use margin_rs::liquidation::{
    CloseoutOutcome, CloseoutTarget, LiquidationAccount, LiquidationPolicy, LiquidityLevel,
    plan_liquidation,
};
use margin_rs::maintenance::{AccountState, CollateralView, assess};
use margin_rs::rules::TieredLinearIm;
use margin_rs::{AccountId, Asset, ContractSize, Decimal, InstrumentId, VenueId};

fn main() -> margin_rs::Result<()> {
    let usdc = Asset::new("USDC")?;
    let perp = InstrumentId::new("BTC-PERP")?;
    let rule = TieredLinearIm::linear_v1(usdc.clone(), 6)?;

    // Long 10 contracts entered at 100 (basis 1000), marked at 95.
    let view = ExposureView {
        account: AccountId::new("acct")?,
        venue: VenueId::new("venue")?,
        positions: vec![ExposurePosition {
            instrument: perp.clone(),
            product: ProductKind::LinearPerpetual,
            settlement: usdc.clone(),
            contracts: Decimal::from(10),
            size: ContractSize::Multiplier(Decimal::ONE),
            mark: Decimal::from(95),
        }],
    };
    let requirement = rule.requirement(&view)?;
    assert_eq!(requirement.maintenance, Decimal::new(475, 1)); // 5 % of 950

    // Cash 90 and upnl -50: collateral 40 is below maintenance.
    let collateral = Decimal::from(40);
    let state = assess(
        AccountState::Healthy,
        &CollateralView { currency: usdc.clone(), cash: Decimal::from(90), collateral: Some(collateral), equity: Some(collateral) },
        &requirement,
    )?;
    assert_eq!(state.state, AccountState::ReduceOnly);

    // Close just enough at the bid to restore maintenance.
    let account = LiquidationAccount {
        view,
        basis: BTreeMap::from([(perp.clone(), Decimal::from(1000))]),
        cash: Decimal::from(90),
        resting_orders: vec![],
    };
    let liquidity = BTreeMap::from([(perp, vec![LiquidityLevel { price: Decimal::from(94), contracts: Decimal::from(100) }])]);
    let policy = LiquidationPolicy {
        target: CloseoutTarget::Maintenance,
        fee_rate: Decimal::new(5, 4), // 0.0005
        fee_rates: BTreeMap::new(),
        loss_budget: None,
        contract_step: Decimal::ONE,
    };
    let plan = plan_liquidation(&rule, &account, &liquidity, &policy)?;
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    assert_eq!(plan.reductions[0].contracts, Decimal::from(3));
    Ok(())
}
```

### Options

The ledger holds option positions and takes their value as input: premium
marks, settlement outcomes and the underlying price. The premium moves cash at
the fill. A short requires margin under `short-option-im/1`; its mark is
deducted from collateral, not added to the requirement. Stress and closeout
plans refuse an open option, and margin reservations block in its currency.
See [docs/options.md](docs/options.md).

```rust
use margin_rs::instrument::{
    ExerciseStyle, LinearTerms, OptionContract, OptionDelivery, OptionInstrument, OptionMargining,
    OptionRight,
};
use margin_rs::ledger::{Action, Execution, Ledger, OptionOutcome, OptionSettlement};
use margin_rs::{Asset, ExecutionId, InstrumentId, TransferId};
use rust_decimal_macros::dec;

fn main() -> margin_rs::Result<()> {
    let usdc = Asset::new("USDC")?;
    let id = InstrumentId::new("BTC-CALL-100")?;
    let mut ledger = Ledger::default();
    ledger.register_asset(usdc.clone(), 6)?;
    ledger.register_instrument(OptionInstrument::new(
        LinearTerms::new(id.clone(), usdc.clone(), dec!(1), 4, 2)?,
        OptionContract {
            underlying: Asset::new("BTC")?,
            strike: dec!(100),
            expiry_ms: 1_000,
            right: OptionRight::Call,
            exercise: ExerciseStyle::European,
            delivery: OptionDelivery::Cash,
            margining: OptionMargining::Linear,
        },
    )?)?;
    ledger.deposit(TransferId::new("d1")?, usdc.clone(), dec!(1000))?;

    // Long 2 at a premium of 10: 20 paid.
    let fill = Execution::new(ExecutionId::new("e1")?, id.clone(), Action::Buy, dec!(10), dec!(2))
        .with_executed_at(1);
    ledger.book_execution_allowing_overdraft(&fill, None)?;

    // Settled at 130: 2 * 30 = 60 received, realized 40.
    let settled = ledger.settle_option(&OptionSettlement {
        id: ExecutionId::new("s1")?,
        instrument: id,
        contracts: dec!(2),
        outcome: OptionOutcome::Cash { price: dec!(130) },
        as_of_ms: 1_000,
    })?;
    assert_eq!(settled.cashflow.realized_pnl, dec!(40));
    assert_eq!(ledger.balance(&usdc).map(|b| b.total), Some(dec!(1040)));
    ledger.reconcile()?;
    Ok(())
}
```

### Inverse margin

```rust
use margin_rs::initial_margin::{ExposurePosition, ExposureView, InitialMarginModel, ProductKind};
use margin_rs::rules::TieredInverseIm;
use margin_rs::{AccountId, Asset, ContractSize, Decimal, InstrumentId, VenueId};

fn main() -> margin_rs::Result<()> {
    let btc = Asset::new("BTC")?;
    let rule = TieredInverseIm::inverse_v1(btc.clone(), 8)?;

    // Short 10 contracts of 100 USD each, marked at 50000 USD per BTC:
    // notional 10 * 100 / 50000 = 0.02 BTC.
    let view = ExposureView {
        account: AccountId::new("acct")?,
        venue: VenueId::new("venue")?,
        positions: vec![ExposurePosition {
            instrument: InstrumentId::new("BTC-USD-INV")?,
            product: ProductKind::InversePerpetual,
            settlement: btc,
            contracts: Decimal::from(-10),
            size: ContractSize::ContractValue(Decimal::from(100)),
            mark: Decimal::from(50_000),
        }],
    };
    let requirement = rule.requirement(&view)?;
    assert_eq!(requirement.initial, Decimal::new(2, 3)); // 10 % of 0.02 BTC
    assert_eq!(requirement.maintenance, Decimal::new(1, 3)); // 5 %
    Ok(())
}
```

## Exact arithmetic

- Money, prices and quantities are `rust_decimal::Decimal`. Every operation
  is checked: it returns the exact result or a typed `Arithmetic` error,
  never a saturated or silently rounded value.
- Sums and products of more than two terms are computed in 768-bit wide
  integers with one final conversion or one final rounding, so an
  intermediate that does not fit can never change a result that does.
- The only rounding steps are explicit and documented (notional, released
  cost, margin per position, fees, valuation), each from the exact value
  with a declared `Rounding` mode.
- No panics and no unchecked arithmetic in library code. Clippy runs
  pedantic, with `unwrap_used`, `expect_used`, `panic`, `unreachable`,
  `todo`, `unimplemented`, `indexing_slicing` and `float_arithmetic` (no
  binary floating point) denied, and `arithmetic_side_effects` as a
  warning, which `make lint` turns into an error. `unsafe` code is
  forbidden. The few documented exceptions are `#[expect]`s with their
  reason.

See [docs/exact-arithmetic.md](docs/exact-arithmetic.md).

## Status and policies

- Version 0.1.1, published on crates.io. Before 1.0 the API may change
  in a minor version.
- Rust edition 2024, MSRV 1.88. CI runs the latest stable and the MSRV.
- Small dependency set: `financial_types`, `thiserror` and `rust_decimal`
  (without default features). margin-rs never depends on market-maker-rs.
  market-maker-rs uses margin-rs optionally through its `margin` feature,
  pinned by git to an earlier revision; updating it to this release is
  pending.
  `Cargo.lock` is committed so CI, including the MSRV job, is
  reproducible.

## 🛠 Makefile Commands

```sh
make build       # Compile every target
make test        # Unit, integration and doc tests (README examples included)
make fmt         # Format the code
make fmt-check   # Check formatting
make lint        # Clippy with warnings as errors
make doc         # Documentation with warnings as errors
make msrv        # Check every target on the MSRV (1.88)
make pre-push    # fmt-check, lint, test, doc and msrv
```

## Contribution and Contact

We welcome contributions to this project! If you would like to contribute, please follow these steps:

1. Fork the repository.
2. Create a new branch for your feature or bug fix.
3. Make your changes and ensure that the project still builds and all tests pass.
4. Commit your changes and push your branch to your forked repository.
5. Submit a pull request to the main repository.

If you have any questions, issues, or would like to provide feedback, please feel free to contact the project
maintainer:

### **Contact Information**
- **Author**: Joaquín Béjar García
- **Email**: jb@taunais.com
- **Telegram**: [@joaquin_bejar](https://t.me/joaquin_bejar)
- **Repository**: <https://github.com/joaquinbejar/margin-rs>
- **Documentation**: <https://docs.rs/margin-rs>


We appreciate your interest and look forward to your contributions!

**License**: MIT

<!-- related-projects:start -->
## Related projects

Repositories by the same author that this project depends on, and repositories that depend on it.

### Depends on

| Repository | Description |
|------------|-------------|
| [financial_types](https://github.com/joaquinbejar/financial_types) · [crates.io](https://crates.io/crates/financial_types) | Core financial type definitions for trading systems; margin-rs uses its `Action` for the order and execution direction and, with the `instruments` feature, its instrument specifications. |

### Used by

| Repository | Description |
|------------|-------------|
| [market-maker-rs](https://github.com/joaquinbejar/market-maker-rs) · [crates.io](https://crates.io/crates/market-maker-rs) | Quantitative market making strategies, starting with the Avellaneda-Stoikov model. It uses margin-rs optionally through its `margin` feature, pinned by git to an earlier revision; updating it to this release is pending. |

<!-- related-projects:end -->
