//! Stress scenarios for spot inventory and open linear futures and
//! perpetuals.
//!
//! [`stress`] revalues a [`LedgerSnapshot`] under a [`Scenario`] and
//! compares it with the unstressed valuation. It never changes the
//! snapshot, the marks or the ledger: it values a stressed copy of each. A
//! scenario is applied in this order:
//!
//! 1. **Hypothetical fills** ([`Scenario::fill`]), for example hedges that
//!    are expected to execute, are applied to a copy of the snapshot with
//!    the ledger's booking rules. A spot fill exchanges its notional
//!    (rounded once to the quote scale) for the base asset, with average
//!    cost positions. A linear future or perpetual fill follows the
//!    derivative booking of [`ledger`](crate::ledger) (half-even, only the
//!    realized PnL moves cash), at the valuation time `marks.as_of_ms`: a
//!    future at or past its expiry, or already settled, cannot take one. A
//!    fee is debited once from its asset. Balances are not checked: a
//!    hypothetical fill may overdraw, and it reserves nothing. Leaving a
//!    fill out of the scenario models a failed hedge.
//! 2. **Instrument shocks** ([`Scenario::instrument_shock`]): the mark of a
//!    derivative is multiplied by `1 + shock`. Instrument shocks are not
//!    linked to asset shocks (the instrument terms do not name an
//!    underlying): a scenario that moves an asset and the derivatives on it
//!    shocks both. Open derivatives without a shock are listed in
//!    [`StressResult::unshocked_instruments`].
//! 3. **Asset shocks** ([`Scenario::shock`]): the price of an asset is
//!    multiplied by `1 + shock` (for example `-0.2` for a 20 % fall).
//! 4. **Liquidation spread** ([`Scenario::spread`]), the cost of closing
//!    across half the bid ask spread:
//!    - an open derivative is valued at `mark * (1 - spread)` when long and
//!      `mark * (1 + spread)` when short, by its contracts after the fills;
//!    - a non reporting asset is valued at `price * (1 - spread)` when the
//!      amount collateral counts for it is positive, and at
//!      `price * (1 + spread)` when it is negative (a debt). That amount is
//!      its balance plus the unrealized PnL of the derivatives settled in
//!      it at valid instrument marks, the netting of
//!      [`AccountValuation::collateral_equity`]. When a loss is at an
//!      invalid instrument mark, collateral is unknown and the sign of the
//!      balance plus all of that PnL is used. When that full net and the
//!      counted one have different signs, collateral is valued
//!      conservatively and reporting equity is not: the asset is listed in
//!      [`StressResult::spread_sign_conflicts`].
//!
//! Inverse futures and perpetuals are valued but not stressed yet: an open
//! inverse position, a shock on one or a hypothetical fill on one is
//! [`MarginError::UnsupportedInstrument`].
//!
//! Every stressed price, `price * (1 + shock) * (1 +- spread)`, is one exact
//! value, `price * side + price * shock * side`, converted once: only the
//! stressed price must be representable, not `1 + shock` nor a partial
//! product.
//! Shocks are applied exactly and must be greater than -1. The stressed
//! instrument marks are final before the asset factors are chosen, so the
//! order has no cycle: the derivative values that choose them are the ones
//! the stressed valuation reports.
//!
//! All parameters are validated first: asset shocks must name a registered,
//! non reporting asset, instrument shocks a registered linear future or
//! perpetual, both must be greater than -1, and the spread must be in
//! `[0, 1)`. A shock on an asset without a mark has no effect and is listed
//! in [`StressResult::ignored_shocks`]; a shock on a derivative that is flat
//! after the fills, in [`StressResult::ignored_instrument_shocks`].
//! Hypothetical fills follow the input rules of
//! [`Ledger::book_execution`](crate::ledger::Ledger::book_execution)
//! (positive price and quantity within the instrument scales, fee within its
//! asset scale).
//!
//! The stressed snapshot is then valued with [`value`] under the same
//! policy, so invalid marks stay invalid: a stressed mark keeps its time and
//! source. A held asset with an invalid mark adds nothing to collateral, and
//! a debt with one makes collateral unknown (or, under the stale haircut
//! policy, counts at `price * (1 + haircut)`); a derivative gain at an
//! invalid instrument mark is not counted, and a loss there makes collateral
//! unknown.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use core::cmp::Ordering;

use crate::decimal::{
    Rounding, add, check_scale, mul_div_round, neg, product, sub, sum, sum_products, sum_sign,
};
use crate::direction::Dir;
use crate::error::{MarginError, Result};
use crate::ids::{Asset, InstrumentId};
use crate::instrument::{ContractSize, Instrument, LinearTerms, SpotInstrument};
use crate::ledger::derivative::apply_derivative_fill;
use crate::ledger::{Action, Fee, LedgerSnapshot, Position, apply_fill};
use crate::valuation::{
    AccountValuation, MarkSet, ValuationPolicy, collateral_upnl, derivative_values, value,
};

/// A fill assumed to happen in a scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HypotheticalFill {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Direction.
    pub side: Action,
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
    /// Relative mark change per linear future or perpetual.
    pub instrument_shocks: BTreeMap<InstrumentId, Decimal>,
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

    /// Adds a relative mark shock for the linear future or perpetual
    /// `instrument`.
    #[must_use]
    pub fn instrument_shock(mut self, instrument: InstrumentId, relative: Decimal) -> Self {
        self.instrument_shocks.insert(instrument, relative);
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
    /// Shocked derivatives that are flat after the hypothetical fills, so
    /// the shock had no effect.
    pub ignored_instrument_shocks: Vec<InstrumentId>,
    /// Derivatives open after the hypothetical fills without an instrument
    /// shock (an explicit shock of zero is a shock): their marks are not
    /// moved by the asset shocks.
    pub unshocked_instruments: Vec<InstrumentId>,
    /// Assets whose liquidation spread was chosen for collateral against
    /// the sign reporting equity would need: the amount collateral counts
    /// and the full net with every unrealized PnL have different signs, so
    /// the stressed reporting equity is not conservative for them.
    pub spread_sign_conflicts: Vec<Asset>,
}

/// Values `snapshot` with and without `scenario`.
///
/// # Errors
///
/// - Every error of [`value`], for the snapshot and for the stressed copy
///   (for example [`MarginError::MissingMark`] for a derivative opened by a
///   hypothetical fill without an instrument mark).
/// - [`MarginError::UnsupportedAction`] for a fill side that is not `Buy`
///   or `Sell`.
/// - [`MarginError::UnknownInstrument`] or [`MarginError::UnknownAsset`] for
///   a fill or an instrument shock on an unknown instrument, or a fee in an
///   unknown asset.
/// - [`MarginError::InvalidInput`] for a non positive fill price or
///   quantity, a fill on a settled future, an instrument shock on a spot
///   instrument, a shock not greater than -1 or a spread outside `[0, 1)`.
/// - [`MarginError::ExecutedAfterExpiry`] for a fill on a future at or past
///   its expiry at `marks.as_of_ms`.
/// - [`MarginError::Arithmetic`] if a result cannot be represented.
pub fn stress(
    snapshot: &LedgerSnapshot,
    marks: &MarkSet,
    policy: &ValuationPolicy,
    scenario: &Scenario,
) -> Result<StressResult> {
    validate_scenario(snapshot, policy, scenario)?;

    let base = value(snapshot, marks, policy)?;

    let mut stressed_snapshot = snapshot.clone();
    for fill in &scenario.fills {
        apply_hypothetical(
            &mut stressed_snapshot,
            fill,
            policy.rounding,
            marks.as_of_ms,
        )?;
    }

    // Derivatives open after the fills, with their contracts.
    let mut open: BTreeMap<&InstrumentId, Decimal> = BTreeMap::new();
    for (id, position) in &stressed_snapshot.positions {
        let derivative = match stressed_snapshot.instruments.get(id) {
            Some(Instrument::Spot(_)) | None => false,
            Some(Instrument::LinearFuture(_) | Instrument::LinearPerpetual(_)) => true,
            Some(Instrument::InverseFuture(_) | Instrument::InversePerpetual(_)) => {
                if position.quantity.is_zero() {
                    continue;
                }
                return Err(inverse_unsupported(id));
            }
        };
        if derivative && !position.quantity.is_zero() {
            open.insert(id, position.quantity);
        }
    }
    let ignored_shocks: Vec<Asset> = scenario
        .shocks
        .keys()
        .filter(|asset| !marks.prices.contains_key(*asset))
        .cloned()
        .collect();
    let ignored_instrument_shocks: Vec<InstrumentId> = scenario
        .instrument_shocks
        .keys()
        .filter(|id| !open.contains_key(id))
        .cloned()
        .collect();
    let unshocked_instruments: Vec<InstrumentId> = open
        .keys()
        .filter(|id| !scenario.instrument_shocks.contains_key(**id))
        .map(|id| (*id).clone())
        .collect();

    let spread = scenario.liquidation_spread;
    let sell = sub(Decimal::ONE, spread, "1 - spread")?;
    let buy_back = add(Decimal::ONE, spread, "1 + spread")?;
    let mut stressed_marks = marks.clone();

    // Final instrument marks: mark * (1 + shock) * spread factor as one
    // exact product, a long closed at the bid side and a short at the ask.
    for (id, mark) in &mut stressed_marks.instruments {
        let Some(contracts) = open.get(id) else {
            continue;
        };
        let side = (!spread.is_zero()).then_some(if contracts.is_sign_positive() {
            sell
        } else {
            buy_back
        });
        mark.price = stressed_price(
            mark.price,
            scenario.instrument_shocks.get(id).copied(),
            side,
            "stressed instrument mark",
        )?;
    }

    let spread_sign_conflicts = stress_asset_prices(
        &stressed_snapshot,
        &mut stressed_marks,
        policy,
        scenario,
        (sell, buy_back),
    )?;

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
        ignored_instrument_shocks,
        unshocked_instruments,
        spread_sign_conflicts,
    })
}

/// Validates every parameter before anything is computed.
fn validate_scenario(
    snapshot: &LedgerSnapshot,
    policy: &ValuationPolicy,
    scenario: &Scenario,
) -> Result<()> {
    // Inverse positions are valued, but not stressed yet: refuse an open
    // one rather than report it unstressed.
    for (id, position) in &snapshot.positions {
        if let Some(Instrument::InverseFuture(_) | Instrument::InversePerpetual(_)) =
            snapshot.instruments.get(id)
            && !position.quantity.is_zero()
        {
            return Err(inverse_unsupported(id));
        }
    }
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
        check_shock(*shock)?;
    }
    for (instrument, shock) in &scenario.instrument_shocks {
        match snapshot.instruments.get(instrument) {
            Some(Instrument::LinearFuture(_) | Instrument::LinearPerpetual(_)) => {}
            Some(Instrument::InverseFuture(_) | Instrument::InversePerpetual(_)) => {
                return Err(inverse_unsupported(instrument));
            }
            Some(Instrument::Spot(_)) => {
                return Err(MarginError::invalid_input(
                    "instrument shock",
                    "a spot instrument is shocked through its base asset",
                ));
            }
            None => {
                return Err(MarginError::UnknownInstrument {
                    instrument: instrument.clone(),
                });
            }
        }
        check_shock(*shock)?;
    }
    for fill in &scenario.fills {
        Dir::from_action(fill.side, "fill side")?;
    }
    Ok(())
}

/// Final asset prices: `price * (1 + shock) * spread factor` as one exact
/// product. The stressed derivative values, the ones the stressed
/// valuation reports, choose the side of the spread for their settlement
/// assets. Returns the assets whose collateral and full nets have
/// different signs.
fn stress_asset_prices(
    stressed_snapshot: &LedgerSnapshot,
    stressed_marks: &mut MarkSet,
    policy: &ValuationPolicy,
    scenario: &Scenario,
    (sell, buy_back): (Decimal, Decimal),
) -> Result<Vec<Asset>> {
    let spread = scenario.liquidation_spread;
    let (counted, full) = if spread.is_zero() {
        (BTreeMap::new(), BTreeMap::new())
    } else {
        let derivatives = derivative_values(stressed_snapshot, stressed_marks, policy.max_age_ms)?;
        let mut full: BTreeMap<Asset, Vec<Decimal>> = BTreeMap::new();
        for value in derivatives.values() {
            full.entry(value.settlement.clone())
                .or_default()
                .push(value.unrealized_pnl);
        }
        (collateral_upnl(&derivatives), full)
    };
    let mut spread_sign_conflicts = Vec::new();
    for (asset, mark) in &mut stressed_marks.prices {
        let mut side = None;
        if !spread.is_zero() {
            let cash = stressed_snapshot
                .balances
                .get(asset)
                .map(|b| b.total)
                .unwrap_or_default();
            let net_sign = |pnl: &[Decimal]| {
                let mut values = Vec::with_capacity(pnl.len().saturating_add(1));
                values.push(cash);
                values.extend_from_slice(pnl);
                sum_sign(&values, "balance + unrealized pnl")
            };
            let full_sign = net_sign(full.get(asset).map_or(&[], Vec::as_slice))?;
            // `None` when a loss at an invalid mark makes collateral unknown.
            let counted_sign = match counted.get(asset) {
                None => Some(full_sign),
                Some(Some(terms)) => Some(net_sign(terms)?),
                Some(None) => None,
            };
            if counted_sign.is_some_and(|sign| sign != full_sign) {
                spread_sign_conflicts.push(asset.clone());
            }
            side = match counted_sign.unwrap_or(full_sign) {
                Ordering::Greater => Some(sell),
                Ordering::Less => Some(buy_back),
                Ordering::Equal => None,
            };
        }
        mark.price = stressed_price(
            mark.price,
            scenario.shocks.get(asset).copied(),
            side,
            "stressed price",
        )?;
    }
    Ok(spread_sign_conflicts)
}

/// `price * (1 + shock) * side` as one exact value converted once:
/// `price * side + price * shock * side`, so neither `1 + shock` nor a
/// partial product needs to be representable.
fn stressed_price(
    price: Decimal,
    shock: Option<Decimal>,
    side: Option<Decimal>,
    operation: &'static str,
) -> Result<Decimal> {
    let mut unshocked = vec![price];
    unshocked.extend(side);
    match shock {
        None => product(&unshocked, operation),
        Some(shock) => {
            let mut shocked = unshocked.clone();
            shocked.push(shock);
            sum_products(&[&unshocked, &shocked], operation)
        }
    }
}

/// Inverse futures and perpetuals are not stressed yet.
#[cold]
fn inverse_unsupported(instrument: &InstrumentId) -> MarginError {
    MarginError::UnsupportedInstrument {
        rule: "stress".to_string(),
        instrument: instrument.clone(),
        reason: "inverse futures and perpetuals are not stressed yet",
    }
}

/// A relative shock must be greater than -1.
fn check_shock(shock: Decimal) -> Result<()> {
    if shock <= -Decimal::ONE {
        return Err(MarginError::invalid_input(
            "shock",
            "must be greater than -1",
        ));
    }
    Ok(())
}

fn apply_hypothetical(
    snapshot: &mut LedgerSnapshot,
    fill: &HypotheticalFill,
    rounding: Rounding,
    as_of_ms: u64,
) -> Result<()> {
    match snapshot.instruments.get(&fill.instrument).cloned() {
        Some(Instrument::Spot(spot)) => apply_spot(snapshot, &spot, fill, rounding),
        Some(Instrument::LinearFuture(future)) => {
            if snapshot.settled_instruments.contains(&fill.instrument) {
                return Err(MarginError::invalid_input(
                    "fill",
                    "a settled future cannot take a hypothetical fill",
                ));
            }
            if as_of_ms >= future.expiry_ms() {
                return Err(MarginError::ExecutedAfterExpiry {
                    instrument: fill.instrument.clone(),
                    executed_at_ms: as_of_ms,
                    expiry_ms: future.expiry_ms(),
                });
            }
            apply_derivative(snapshot, future.terms(), fill)
        }
        Some(Instrument::LinearPerpetual(perpetual)) => {
            apply_derivative(snapshot, perpetual.terms(), fill)
        }
        Some(Instrument::InverseFuture(_) | Instrument::InversePerpetual(_)) => {
            Err(inverse_unsupported(&fill.instrument))
        }
        None => Err(MarginError::UnknownInstrument {
            instrument: fill.instrument.clone(),
        }),
    }
}

/// Positive price and quantity, within the instrument scales: the input
/// rules of `Ledger::book_execution`.
fn check_fill(fill: &HypotheticalFill, price_scale: u32, quantity_scale: u32) -> Result<()> {
    if fill.price <= Decimal::ZERO {
        return Err(MarginError::invalid_input("price", "must be positive"));
    }
    if fill.quantity <= Decimal::ZERO {
        return Err(MarginError::invalid_input("quantity", "must be positive"));
    }
    check_scale(fill.price, price_scale, "price")?;
    check_scale(fill.quantity, quantity_scale, "quantity")
}

fn scale_of(snapshot: &LedgerSnapshot, asset: &Asset) -> Result<u32> {
    snapshot
        .assets
        .get(asset)
        .copied()
        .ok_or_else(|| MarginError::UnknownAsset {
            asset: asset.clone(),
        })
}

/// Adds the fee to `deltas` and to the fees paid, after checking its scale.
fn book_fee(
    snapshot: &mut LedgerSnapshot,
    fee: Option<&Fee>,
    deltas: &mut BTreeMap<Asset, Vec<Decimal>>,
) -> Result<()> {
    if let Some(fee) = fee {
        check_scale(fee.amount, scale_of(snapshot, &fee.asset)?, "fee")?;
        deltas
            .entry(fee.asset.clone())
            .or_default()
            .push(neg(fee.amount));
        let paid = snapshot.fees_paid.entry(fee.asset.clone()).or_default();
        *paid = add(*paid, fee.amount, "fees + fee")?;
    }
    Ok(())
}

/// Applies every change to one asset in one exact step, to its total and
/// available balance (a hypothetical fill reserves nothing), so a fee in
/// the same asset cannot make an intermediate overflow.
fn apply_deltas(
    snapshot: &mut LedgerSnapshot,
    deltas: BTreeMap<Asset, Vec<Decimal>>,
) -> Result<()> {
    for (asset, changes) in deltas {
        let balance = snapshot.balances.entry(asset).or_default();
        let mut total = vec![balance.total];
        total.extend(&changes);
        let mut available = vec![balance.available];
        available.extend(&changes);
        balance.total = sum(&total, "balance + deltas")?;
        balance.available = sum(&available, "available + deltas")?;
    }
    Ok(())
}

fn apply_spot(
    snapshot: &mut LedgerSnapshot,
    spot: &SpotInstrument,
    fill: &HypotheticalFill,
    rounding: Rounding,
) -> Result<()> {
    check_fill(fill, spot.price_scale(), spot.quantity_scale())?;
    let quote_scale = scale_of(snapshot, spot.quote())?;
    let notional = mul_div_round(
        &[fill.price, fill.quantity],
        &[],
        quote_scale,
        rounding,
        "price * quantity",
    )?;
    let (signed_quantity, signed_value) = match Dir::from_action(fill.side, "fill side")? {
        Dir::Buy => (fill.quantity, notional),
        Dir::Sell => (neg(fill.quantity), neg(notional)),
    };
    let mut deltas: BTreeMap<Asset, Vec<Decimal>> = BTreeMap::new();
    deltas
        .entry(spot.base().clone())
        .or_default()
        .push(signed_quantity);
    deltas
        .entry(spot.quote().clone())
        .or_default()
        .push(neg(signed_value));
    book_fee(snapshot, fill.fee.as_ref(), &mut deltas)?;
    apply_deltas(snapshot, deltas)?;
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

/// A linear future or perpetual fill, booked like
/// [`Ledger::book_execution`](crate::ledger::Ledger::book_execution): no
/// asset is exchanged, the basis and contracts move, and only the realized
/// PnL and the fee move cash.
fn apply_derivative(
    snapshot: &mut LedgerSnapshot,
    terms: &LinearTerms,
    fill: &HypotheticalFill,
) -> Result<()> {
    check_fill(fill, terms.price_scale(), terms.contract_scale())?;
    let settlement = terms.settlement();
    let scale = scale_of(snapshot, settlement)?;
    let dir = Dir::from_action(fill.side, "fill side")?;
    let position =
        snapshot
            .positions
            .get(&fill.instrument)
            .ok_or_else(|| MarginError::UnknownInstrument {
                instrument: fill.instrument.clone(),
            })?;
    let effect = apply_derivative_fill(
        Position {
            quantity: position.quantity,
            cost_basis: position.cost_basis,
            realized_pnl: position.realized_pnl,
        },
        dir,
        fill.price,
        fill.quantity,
        ContractSize::Multiplier(terms.multiplier()),
        scale,
    )?;
    let mut deltas: BTreeMap<Asset, Vec<Decimal>> = BTreeMap::new();
    deltas
        .entry(settlement.clone())
        .or_default()
        .push(effect.realized);
    book_fee(snapshot, fill.fee.as_ref(), &mut deltas)?;
    apply_deltas(snapshot, deltas)?;
    let position = snapshot
        .positions
        .get_mut(&fill.instrument)
        .ok_or_else(|| MarginError::UnknownInstrument {
            instrument: fill.instrument.clone(),
        })?;
    position.quantity = effect.position.quantity;
    position.cost_basis = effect.position.cost_basis;
    position.realized_pnl = effect.position.realized_pnl;
    Ok(())
}
