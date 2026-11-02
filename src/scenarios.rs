//! Spot inventory stress scenarios.
//!
//! [`stress`] revalues a [`LedgerSnapshot`] under a [`Scenario`] and
//! compares it with the unstressed valuation. A scenario is applied in this
//! order:
//!
//! 1. **Hypothetical fills** ([`Scenario::fill`]), for example hedges that
//!    are expected to execute, are applied to a copy of the snapshot with
//!    the ledger's booking rules (notional rounded once to the quote scale,
//!    fee debited once from its asset, average cost positions). Balances are
//!    not checked: a hypothetical fill may overdraw. Leaving a fill out of
//!    the scenario models a failed hedge.
//! 2. **Gaps** ([`Scenario::shock`]): the price of an asset is multiplied
//!    by `1 + shock` (for example `-0.2` for a 20 % fall). Shocks are
//!    applied exactly; the shock must be greater than -1.
//! 3. **Liquidation spread** ([`Scenario::spread`]): every non reporting
//!    asset with a positive balance is valued at `price * (1 - spread)`, the
//!    proceeds of selling it across half the bid ask spread, and every one
//!    with a negative balance (a debt) at `price * (1 + spread)`, the cost of
//!    buying it back.
//!
//! All parameters are validated first: shocks must name a registered, non
//! reporting asset and be greater than -1, and the spread must be in
//! `[0, 1)`. A shock on an asset without a mark has no effect and is listed
//! in [`StressResult::ignored_shocks`]. Hypothetical fills follow the input
//! rules of [`Ledger::book_execution`](crate::ledger::Ledger::book_execution)
//! (positive price and quantity within the instrument scales, fee within its
//! asset scale).
//!
//! The stressed snapshot is then valued with [`value`] under the same
//! policy, so invalid marks stay invalid: a held asset with an invalid mark
//! adds nothing to collateral, and a debt with one makes collateral unknown
//! (or, under the stale haircut policy, counts at `price * (1 + haircut)`).

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::decimal::{Rounding, add, check_scale, mul, mul_div_round, sub};
use crate::error::{MarginError, Result};
use crate::ids::{Asset, InstrumentId};
use crate::instrument::Instrument;
use crate::ledger::{Fee, LedgerSnapshot, Position, Side, apply_fill};
use crate::valuation::{AccountValuation, MarkSet, ValuationPolicy, value};

/// A fill assumed to happen in a scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HypotheticalFill {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Direction.
    pub side: Side,
    /// Execution price.
    pub price: Decimal,
    /// Quantity.
    pub quantity: Decimal,
    /// Fee, if any.
    pub fee: Option<Fee>,
}

/// A named stress scenario. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scenario {
    /// Name, for reports.
    pub name: String,
    /// Relative price change per asset.
    pub shocks: BTreeMap<Asset, Decimal>,
    /// Fraction of value lost when liquidating non reporting assets.
    pub liquidation_spread: Decimal,
    /// Fills applied before the shocks.
    pub fills: Vec<HypotheticalFill>,
}

impl Scenario {
    /// An empty scenario (no fills, no shocks, no spread).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Adds a relative price shock for `asset`.
    #[must_use]
    pub fn shock(mut self, asset: Asset, relative: Decimal) -> Self {
        self.shocks.insert(asset, relative);
        self
    }

    /// Sets the liquidation spread.
    #[must_use]
    pub fn spread(mut self, spread: Decimal) -> Self {
        self.liquidation_spread = spread;
        self
    }

    /// Adds a hypothetical fill.
    #[must_use]
    pub fn fill(mut self, fill: HypotheticalFill) -> Self {
        self.fills.push(fill);
        self
    }
}

/// Result of [`stress`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StressResult {
    /// Scenario name.
    pub name: String,
    /// Valuation without the scenario.
    pub base: AccountValuation,
    /// Valuation under the scenario.
    pub stressed: AccountValuation,
    /// `stressed.equity - base.equity`, when both are known.
    pub equity_change: Option<Decimal>,
    /// `stressed.collateral_equity - base.collateral_equity`, when both are
    /// known.
    pub collateral_change: Option<Decimal>,
    /// Shocked assets that have no mark, so the shock had no effect.
    pub ignored_shocks: Vec<Asset>,
}

/// Values `snapshot` with and without `scenario`.
///
/// # Errors
///
/// - Every error of [`value`].
/// - [`MarginError::UnknownInstrument`] or [`MarginError::UnknownAsset`] for
///   a fill on an unknown instrument or with a fee in an unknown asset.
/// - [`MarginError::InvalidInput`] for a non positive fill price or
///   quantity, a shock not greater than -1 or a spread outside `[0, 1)`.
/// - [`MarginError::Arithmetic`] if a result cannot be represented.
pub fn stress(
    snapshot: &LedgerSnapshot,
    marks: &MarkSet,
    policy: &ValuationPolicy,
    scenario: &Scenario,
) -> Result<StressResult> {
    // Validate every parameter before doing anything.
    if scenario.liquidation_spread < Decimal::ZERO || scenario.liquidation_spread >= Decimal::ONE {
        return Err(MarginError::invalid_input(
            "liquidation_spread",
            "must be in [0, 1)",
        ));
    }
    for (asset, shock) in &scenario.shocks {
        if !snapshot.assets.contains_key(asset) {
            return Err(MarginError::UnknownAsset {
                asset: asset.clone(),
            });
        }
        if *asset == policy.reporting {
            return Err(MarginError::invalid_input(
                "shock",
                "the reporting asset cannot be shocked",
            ));
        }
        if *shock <= -Decimal::ONE {
            return Err(MarginError::invalid_input(
                "shock",
                "must be greater than -1",
            ));
        }
    }

    let base = value(snapshot, marks, policy)?;

    let mut stressed_snapshot = snapshot.clone();
    for fill in &scenario.fills {
        apply_hypothetical(&mut stressed_snapshot, fill, policy.rounding)?;
    }

    let ignored_shocks: Vec<Asset> = scenario
        .shocks
        .keys()
        .filter(|asset| !marks.prices.contains_key(*asset))
        .cloned()
        .collect();
    let sell = sub(Decimal::ONE, scenario.liquidation_spread, "1 - spread")?;
    let buy_back = add(Decimal::ONE, scenario.liquidation_spread, "1 + spread")?;
    let mut stressed_marks = marks.clone();
    for (asset, mark) in &mut stressed_marks.prices {
        if let Some(shock) = scenario.shocks.get(asset) {
            mark.price = mul(
                mark.price,
                add(Decimal::ONE, *shock, "1 + shock")?,
                "price * (1 + shock)",
            )?;
        }
        if scenario.liquidation_spread.is_zero() {
            continue;
        }
        // Holdings are sold across the spread; debts are bought back across
        // it.
        let total = stressed_snapshot
            .balances
            .get(asset)
            .map(|b| b.total)
            .unwrap_or_default();
        if total > Decimal::ZERO {
            mark.price = mul(mark.price, sell, "price * (1 - spread)")?;
        } else if total < Decimal::ZERO {
            mark.price = mul(mark.price, buy_back, "price * (1 + spread)")?;
        }
    }

    let stressed = value(&stressed_snapshot, &stressed_marks, policy)?;
    let equity_change = match (stressed.equity, base.equity) {
        (Some(s), Some(b)) => Some(sub(s, b, "stressed - base equity")?),
        _ => None,
    };
    let collateral_change = match (stressed.collateral_equity, base.collateral_equity) {
        (Some(s), Some(b)) => Some(sub(s, b, "stressed - base collateral")?),
        _ => None,
    };
    Ok(StressResult {
        name: scenario.name.clone(),
        base,
        stressed,
        equity_change,
        collateral_change,
        ignored_shocks,
    })
}

fn apply_hypothetical(
    snapshot: &mut LedgerSnapshot,
    fill: &HypotheticalFill,
    rounding: Rounding,
) -> Result<()> {
    let Some(Instrument::Spot(spot)) = snapshot.instruments.get(&fill.instrument).cloned() else {
        return Err(MarginError::UnknownInstrument {
            instrument: fill.instrument.clone(),
        });
    };
    // The same input rules as Ledger::book_execution.
    if fill.price <= Decimal::ZERO {
        return Err(MarginError::invalid_input("price", "must be positive"));
    }
    if fill.quantity <= Decimal::ZERO {
        return Err(MarginError::invalid_input("quantity", "must be positive"));
    }
    check_scale(fill.price, spot.price_scale(), "price")?;
    check_scale(fill.quantity, spot.quantity_scale(), "quantity")?;
    let scale_of = |asset: &Asset| {
        snapshot
            .assets
            .get(asset)
            .copied()
            .ok_or_else(|| MarginError::UnknownAsset {
                asset: asset.clone(),
            })
    };
    let quote_scale = scale_of(spot.quote())?;
    let notional = mul_div_round(
        &[fill.price, fill.quantity],
        &[],
        quote_scale,
        rounding,
        "price * quantity",
    )?;
    let (signed_quantity, signed_value) = match fill.side {
        Side::Buy => (fill.quantity, notional),
        Side::Sell => (-fill.quantity, -notional),
    };
    let mut deltas: Vec<(Asset, Decimal)> = vec![
        (spot.base().clone(), signed_quantity),
        (spot.quote().clone(), -signed_value),
    ];
    if let Some(fee) = &fill.fee {
        check_scale(fee.amount, scale_of(&fee.asset)?, "fee")?;
        deltas.push((fee.asset.clone(), -fee.amount));
        let paid = snapshot.fees_paid.entry(fee.asset.clone()).or_default();
        *paid = add(*paid, fee.amount, "fees + fee")?;
    }
    for (asset, delta) in deltas {
        let balance = snapshot.balances.entry(asset).or_default();
        balance.total = add(balance.total, delta, "balance + delta")?;
        balance.available = add(balance.available, delta, "available + delta")?;
    }
    let position = snapshot
        .positions
        .get_mut(&fill.instrument)
        .ok_or_else(|| MarginError::UnknownInstrument {
            instrument: fill.instrument.clone(),
        })?;
    let (next, _) = apply_fill(
        Position {
            quantity: position.quantity,
            cost_basis: position.cost_basis,
            realized_pnl: position.realized_pnl,
        },
        signed_quantity,
        signed_value,
        fill.price,
        quote_scale,
        rounding,
    )?;
    position.quantity = next.quantity;
    position.cost_basis = next.cost_basis;
    position.realized_pnl = next.realized_pnl;
    Ok(())
}
