//! Account mark to market, kept separate from cash posting.
//!
//! The ledger posts cash, positions and fees; it never looks at prices.
//! [`value`] reads an immutable [`LedgerSnapshot`] and a [`MarkSet`] and
//! returns an [`AccountValuation`] in one reporting asset. Nothing here
//! changes the ledger.
//!
//! # Prices, freshness and reporting asset
//!
//! - Every price is the value of one unit of an asset in the reporting
//!   asset ([`ValuationPolicy::reporting`]), with its timestamp and
//!   [`PriceSource`]. The reporting asset itself is worth exactly 1 and must
//!   not appear in the mark set.
//! - A mark is [`MarkStatus::Valid`] when its price is positive and its age
//!   (`as_of_ms - timestamp_ms`) is at most [`ValuationPolicy::max_age_ms`].
//!   Otherwise it is flagged: [`MarkStatus::Stale`], [`MarkStatus::Future`]
//!   (timestamp after `as_of_ms`), [`MarkStatus::NonPositive`] or
//!   [`MarkStatus::Missing`].
//!
//! # Reporting equity and collateral equity
//!
//! - **Reporting equity** `= sum(total balance * price)`. Stale and future
//!   marks are used but flagged; if an asset with a non zero balance has no
//!   positive price, equity is `None` rather than a guess.
//! - **Collateral equity** counts only what a mark can be trusted for, and
//!   is rounded down (toward negative infinity):
//!   - an asset held counts at a valid mark, at `price * (1 - haircut)` for
//!     a stale mark under the explicit [`InvalidMarkPolicy::HaircutStale`]
//!     policy, and otherwise not at all;
//!   - a debt (a negative balance, for example after a hypothetical fill in
//!     a stress scenario) always counts: in full at a valid mark, at
//!     `price * (1 + haircut)` for a stale mark under the haircut policy,
//!     and otherwise collateral equity is `None`, never a guess.
//!
//!   So an invalid mark can only lower collateral or make it unknown; it
//!   can never create spendable equity.
//!
//! # PnL with fees counted once
//!
//! Fees are already debited from the balances they were charged in, so
//! equity includes them exactly once. `net_pnl = equity - net_transfers`
//! (deposits minus withdrawals, valued at the same marks) is the
//! authoritative PnL. [`PnlAttribution`] splits it into realized,
//! unrealized and fees, all in the reporting asset:
//!
//! - `realized = sum(realized_pnl * price(quote))`;
//! - `unrealized = sum(quantity * price(base) - cost_basis * price(quote))`;
//! - `fees = sum(fees_paid * price(asset))`.
//!
//! For spot, `equity - net_transfers = realized + unrealized - fees`
//! exactly; each figure is rounded once, so the attribution can differ from
//! `net_pnl` by rounding only, reported as
//! [`PnlAttribution::rounding_difference`].

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;

use crate::decimal::{Rounding, add, sub, sum_terms_round};
use crate::error::{MarginError, Result};
use crate::ids::Asset;
use crate::instrument::Instrument;
use crate::ledger::LedgerSnapshot;

/// Where a price comes from. Reported, not interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PriceSource {
    /// Mid of the best bid and ask.
    Mid,
    /// Last trade.
    Last,
    /// External index.
    Index,
    /// Official settlement price.
    Settlement,
    /// Model price.
    Model,
}

/// Price of one unit of an asset in the reporting asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// Price in the reporting asset.
    pub price: Decimal,
    /// When the price was observed, in milliseconds.
    pub timestamp_ms: u64,
    /// Where it comes from.
    pub source: PriceSource,
}

impl Mark {
    /// Builds a mark.
    #[must_use]
    pub fn new(price: Decimal, timestamp_ms: u64, source: PriceSource) -> Self {
        Self {
            price,
            timestamp_ms,
            source,
        }
    }
}

/// Marks observed for a valuation at `as_of_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkSet {
    /// Valuation time, in milliseconds.
    pub as_of_ms: u64,
    /// Price per asset, in the reporting asset.
    pub prices: BTreeMap<Asset, Mark>,
}

impl MarkSet {
    /// An empty mark set at `as_of_ms`.
    #[must_use]
    pub fn new(as_of_ms: u64) -> Self {
        Self {
            as_of_ms,
            prices: BTreeMap::new(),
        }
    }

    /// Adds or replaces the mark of `asset`.
    #[must_use]
    pub fn with(mut self, asset: Asset, mark: Mark) -> Self {
        self.prices.insert(asset, mark);
        self
    }
}

/// What collateral equity may do with a mark that is not valid. See the
/// [module docs](self) for how debts are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InvalidMarkPolicy {
    /// Count nothing for an asset without a valid mark. The default.
    #[default]
    Exclude,
    /// Count a stale mark at `price * (1 - haircut)`; other invalid marks
    /// still count nothing. `haircut` is in `[0, 1]`.
    HaircutStale {
        /// Fraction removed from the stale price.
        haircut: Decimal,
    },
}

/// How to value an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValuationPolicy {
    /// Asset every amount is expressed in.
    pub reporting: Asset,
    /// Maximum age of a valid mark, in milliseconds.
    pub max_age_ms: u64,
    /// Treatment of invalid marks in collateral equity.
    pub invalid_marks: InvalidMarkPolicy,
    /// Rounding of reporting figures to the reporting asset scale.
    /// Collateral equity always rounds toward zero.
    pub rounding: Rounding,
}

impl ValuationPolicy {
    /// Policy with [`InvalidMarkPolicy::Exclude`] and half even rounding.
    #[must_use]
    pub fn new(reporting: Asset, max_age_ms: u64) -> Self {
        Self {
            reporting,
            max_age_ms,
            invalid_marks: InvalidMarkPolicy::Exclude,
            rounding: Rounding::HalfEven,
        }
    }
}

/// Status of the mark used for one asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkStatus {
    /// Positive and fresh (or the reporting asset itself).
    Valid,
    /// Older than allowed.
    Stale {
        /// Age, in milliseconds.
        age_ms: u64,
    },
    /// Timestamped after the valuation time.
    Future {
        /// How far ahead, in milliseconds.
        ahead_ms: u64,
    },
    /// Zero or negative price.
    NonPositive,
    /// No mark.
    Missing,
}

impl MarkStatus {
    /// True for [`MarkStatus::Valid`].
    #[must_use]
    pub fn is_valid(self) -> bool {
        self == Self::Valid
    }
}

/// Valuation of one asset's balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetValue {
    /// Total balance.
    pub total: Decimal,
    /// Available balance (total minus reservations).
    pub available: Decimal,
    /// Mark used, if any (`None` for the reporting asset and when missing).
    pub mark: Option<Mark>,
    /// Status of the mark.
    pub status: MarkStatus,
    /// `total * price` rounded, or `None` without a positive price.
    pub value: Option<Decimal>,
    /// Contribution to collateral equity, rounded down (toward negative
    /// infinity); `None` when it cannot be bounded (a debt without a usable
    /// mark).
    pub collateral_value: Option<Decimal>,
}

/// Split of the PnL into realized, unrealized and fees, in the reporting
/// asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PnlAttribution {
    /// Realized PnL gross of fees.
    pub realized: Decimal,
    /// Unrealized PnL.
    pub unrealized: Decimal,
    /// Fees paid (negative for net rebates).
    pub fees: Decimal,
    /// `net_pnl - (realized + unrealized - fees)`: zero unless rounding
    /// separated the figures.
    pub rounding_difference: Decimal,
}

/// Result of [`value`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountValuation {
    /// Reporting asset.
    pub reporting: Asset,
    /// Valuation time.
    pub as_of_ms: u64,
    /// Per asset valuation, for every registered asset.
    pub assets: BTreeMap<Asset, AssetValue>,
    /// Assets the valuation needs whose mark is not valid.
    pub flags: BTreeMap<Asset, MarkStatus>,
    /// Reporting equity; `None` if a needed price is missing or not
    /// positive.
    pub equity: Option<Decimal>,
    /// Deposits minus withdrawals at the same marks.
    pub net_transfers: Option<Decimal>,
    /// `equity - net_transfers`.
    pub net_pnl: Option<Decimal>,
    /// Attribution of `net_pnl`.
    pub attribution: Option<PnlAttribution>,
    /// Equity that marks can be trusted for, rounded down; `None` when a
    /// debt cannot be valued conservatively. See the [module docs](self).
    pub collateral_equity: Option<Decimal>,
}

impl AccountValuation {
    /// True when every mark the valuation needs is valid.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.flags.is_empty()
    }
}

pub(crate) struct Pricing {
    /// Positive price per asset (stale and future included), reporting = 1.
    pub(crate) prices: BTreeMap<Asset, Decimal>,
    /// Status per registered asset.
    pub(crate) status: BTreeMap<Asset, MarkStatus>,
}

pub(crate) fn pricing(
    snapshot: &LedgerSnapshot,
    marks: &MarkSet,
    policy: &ValuationPolicy,
) -> Pricing {
    let mut prices = BTreeMap::new();
    let mut status = BTreeMap::new();
    for asset in snapshot.assets.keys() {
        if *asset == policy.reporting {
            prices.insert(asset.clone(), Decimal::ONE);
            status.insert(asset.clone(), MarkStatus::Valid);
            continue;
        }
        let state = match marks.prices.get(asset) {
            None => MarkStatus::Missing,
            Some(mark) if mark.price <= Decimal::ZERO => MarkStatus::NonPositive,
            Some(mark) => {
                prices.insert(asset.clone(), mark.price);
                if mark.timestamp_ms > marks.as_of_ms {
                    MarkStatus::Future {
                        ahead_ms: mark.timestamp_ms - marks.as_of_ms,
                    }
                } else {
                    let age_ms = marks.as_of_ms - mark.timestamp_ms;
                    if age_ms > policy.max_age_ms {
                        MarkStatus::Stale { age_ms }
                    } else {
                        MarkStatus::Valid
                    }
                }
            }
        };
        status.insert(asset.clone(), state);
    }
    Pricing { prices, status }
}

pub(crate) fn validate(
    snapshot: &LedgerSnapshot,
    marks: &MarkSet,
    policy: &ValuationPolicy,
) -> Result<u32> {
    let scale =
        *snapshot
            .assets
            .get(&policy.reporting)
            .ok_or_else(|| MarginError::UnknownAsset {
                asset: policy.reporting.clone(),
            })?;
    if marks.prices.contains_key(&policy.reporting) {
        return Err(MarginError::invalid_input(
            "marks",
            "must not price the reporting asset",
        ));
    }
    for asset in marks.prices.keys() {
        if !snapshot.assets.contains_key(asset) {
            return Err(MarginError::UnknownAsset {
                asset: asset.clone(),
            });
        }
    }
    if let InvalidMarkPolicy::HaircutStale { haircut } = policy.invalid_marks {
        if haircut < Decimal::ZERO || haircut > Decimal::ONE {
            return Err(MarginError::invalid_input(
                "haircut",
                "must be between 0 and 1",
            ));
        }
    }
    Ok(scale)
}

/// How one balance counts toward collateral equity.
enum Collateral {
    /// `product(factors)`.
    Counted(Vec<Decimal>),
    /// Contributes nothing (an asset held, without a trusted mark).
    Nothing,
    /// Cannot be bounded: a debt without a usable mark.
    Unknown,
}

/// Collateral treatment of a balance of `total` with a mark in `state`.
///
/// Assets held (positive balances) count at a valid mark, at
/// `price * (1 - haircut)` for a stale mark under
/// [`InvalidMarkPolicy::HaircutStale`], and otherwise not at all. Debts
/// (negative balances) always count: at a valid mark in full, at
/// `price * (1 + haircut)` for a stale mark under the haircut policy (a
/// haircut never shrinks a debt), and otherwise the collateral is unknown.
fn collateral_factors(
    total: Decimal,
    state: MarkStatus,
    price: Option<Decimal>,
    policy: &ValuationPolicy,
) -> Result<Collateral> {
    if total.is_zero() {
        return Ok(Collateral::Nothing);
    }
    let debt = total < Decimal::ZERO;
    Ok(match (state, price) {
        (MarkStatus::Valid, Some(p)) => Collateral::Counted(vec![total, p]),
        (MarkStatus::Stale { .. }, Some(p)) => match policy.invalid_marks {
            InvalidMarkPolicy::HaircutStale { haircut } => {
                let factor = if debt {
                    add(Decimal::ONE, haircut, "1 + haircut")?
                } else {
                    sub(Decimal::ONE, haircut, "1 - haircut")?
                };
                Collateral::Counted(vec![total, p, factor])
            }
            InvalidMarkPolicy::Exclude if debt => Collateral::Unknown,
            InvalidMarkPolicy::Exclude => Collateral::Nothing,
        },
        _ if debt => Collateral::Unknown,
        _ => Collateral::Nothing,
    })
}

/// `sum(product(term))` rounded once toward negative infinity.
fn floor_terms(terms: &[&[Decimal]], scale: u32, operation: &'static str) -> Result<Decimal> {
    let away = sum_terms_round(terms, scale, Rounding::AwayFromZero, operation)?;
    if away < Decimal::ZERO {
        Ok(away)
    } else {
        sum_terms_round(terms, scale, Rounding::TowardZero, operation)
    }
}

/// Values the account in `snapshot` at `marks`. See the
/// [module docs](self).
///
/// # Errors
///
/// - [`MarginError::UnknownAsset`] if the reporting asset or a priced asset
///   is not registered in the snapshot.
/// - [`MarginError::InvalidInput`] if the mark set prices the reporting
///   asset or the haircut is outside `[0, 1]`.
/// - [`MarginError::Arithmetic`] if a result cannot be represented.
pub fn value(
    snapshot: &LedgerSnapshot,
    marks: &MarkSet,
    policy: &ValuationPolicy,
) -> Result<AccountValuation> {
    let scale = validate(snapshot, marks, policy)?;
    let Pricing { prices, status } = pricing(snapshot, marks, policy);
    let rounding = policy.rounding;
    let price = |asset: &Asset| prices.get(asset).copied();
    let mut flags = BTreeMap::new();
    let mut flag = |asset: &Asset| {
        let state = status.get(asset).copied().unwrap_or(MarkStatus::Missing);
        if !state.is_valid() {
            flags.insert(asset.clone(), state);
        }
    };

    // Per asset lines, equity and collateral.
    let mut assets = BTreeMap::new();
    let mut equity_terms: Vec<Vec<Decimal>> = Vec::new();
    let mut equity_known = true;
    let mut collateral_terms: Vec<Vec<Decimal>> = Vec::new();
    let mut collateral_known = true;
    for (asset, balance) in &snapshot.balances {
        let state = status.get(asset).copied().unwrap_or(MarkStatus::Missing);
        let asset_price = price(asset);
        let line_value = match asset_price {
            Some(p) => Some(sum_terms_round(
                &[&[balance.total, p]],
                scale,
                rounding,
                "balance * price",
            )?),
            None => None,
        };
        if !balance.total.is_zero() {
            flag(asset);
            match asset_price {
                Some(p) => equity_terms.push(vec![balance.total, p]),
                None => equity_known = false,
            }
        }
        let collateral = collateral_factors(balance.total, state, asset_price, policy)?;
        let collateral_value = match &collateral {
            Collateral::Counted(factors) => {
                Some(floor_terms(&[factors.as_slice()], scale, "collateral")?)
            }
            Collateral::Nothing => Some(Decimal::ZERO),
            Collateral::Unknown => None,
        };
        match collateral {
            Collateral::Counted(factors) => collateral_terms.push(factors),
            Collateral::Nothing => {}
            Collateral::Unknown => collateral_known = false,
        }
        assets.insert(
            asset.clone(),
            AssetValue {
                total: balance.total,
                available: balance.available,
                mark: marks.prices.get(asset).copied(),
                status: state,
                value: line_value,
                collateral_value,
            },
        );
    }
    let round_terms = |terms: &[Vec<Decimal>], mode: Rounding, op| {
        let slices: Vec<&[Decimal]> = terms.iter().map(Vec::as_slice).collect();
        sum_terms_round(&slices, scale, mode, op)
    };
    let equity = if equity_known {
        Some(round_terms(&equity_terms, rounding, "equity")?)
    } else {
        None
    };
    let collateral_equity = if collateral_known {
        let slices: Vec<&[Decimal]> = collateral_terms.iter().map(Vec::as_slice).collect();
        Some(floor_terms(&slices, scale, "collateral equity")?)
    } else {
        None
    };

    // Net transfers.
    let mut transfer_terms = Vec::new();
    let mut transfers_known = true;
    for (asset, amount) in &snapshot.net_transfers {
        if amount.is_zero() {
            continue;
        }
        flag(asset);
        match price(asset) {
            Some(p) => transfer_terms.push(vec![*amount, p]),
            None => transfers_known = false,
        }
    }
    let net_transfers = if transfers_known {
        Some(round_terms(&transfer_terms, rounding, "net transfers")?)
    } else {
        None
    };
    let net_pnl = match (equity, net_transfers) {
        (Some(e), Some(t)) => Some(sub(e, t, "equity - net transfers")?),
        _ => None,
    };

    // Attribution.
    let mut needed: BTreeSet<&Asset> = BTreeSet::new();
    let mut realized_terms = Vec::new();
    let mut unrealized_terms = Vec::new();
    for (id, position) in &snapshot.positions {
        let Some(Instrument::Spot(spot)) = snapshot.instruments.get(id) else {
            return Err(MarginError::UnknownInstrument {
                instrument: id.clone(),
            });
        };
        // Each term needs only the price it multiplies, and zero terms need
        // none: a closed position with realized PnL needs the quote only.
        if !position.realized_pnl.is_zero() || !position.cost_basis.is_zero() {
            needed.insert(spot.quote());
            if let Some(quote) = price(spot.quote()) {
                realized_terms.push(vec![position.realized_pnl, quote]);
                unrealized_terms.push(vec![-position.cost_basis, quote]);
            }
        }
        if !position.quantity.is_zero() {
            needed.insert(spot.base());
            if let Some(base) = price(spot.base()) {
                unrealized_terms.push(vec![position.quantity, base]);
            }
        }
    }
    let mut fee_terms = Vec::new();
    for (asset, fees) in &snapshot.fees_paid {
        if fees.is_zero() {
            continue;
        }
        needed.insert(asset);
        if let Some(p) = price(asset) {
            fee_terms.push(vec![*fees, p]);
        }
    }
    for asset in &needed {
        flag(asset);
    }
    let attribution = match net_pnl {
        Some(net) if needed.iter().all(|asset| price(asset).is_some()) => {
            let realized = round_terms(&realized_terms, rounding, "realized")?;
            let unrealized = round_terms(&unrealized_terms, rounding, "unrealized")?;
            let fees = round_terms(&fee_terms, rounding, "fees")?;
            let explained = sub(
                add(realized, unrealized, "realized + unrealized")?,
                fees,
                "gross - fees",
            )?;
            Some(PnlAttribution {
                realized,
                unrealized,
                fees,
                rounding_difference: sub(net, explained, "net - explained")?,
            })
        }
        _ => None,
    };

    Ok(AccountValuation {
        reporting: policy.reporting.clone(),
        as_of_ms: marks.as_of_ms,
        assets,
        flags,
        equity,
        net_transfers,
        net_pnl,
        attribution,
        collateral_equity,
    })
}
