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
//! - **Collateral equity** counts only what a mark can be trusted for:
//!   valid marks at full value, stale marks only under the explicit
//!   [`InvalidMarkPolicy::HaircutStale`] policy, and nothing else. Spot
//!   balances are never negative, so leaving an asset out never raises
//!   collateral: an invalid mark cannot create spendable equity. It is
//!   rounded toward zero.
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

/// What collateral equity may do with a mark that is not valid.
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
    /// Contribution to collateral equity, rounded toward zero.
    pub collateral_value: Decimal,
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
    /// Equity that marks can be trusted for. See the [module docs](self).
    pub collateral_equity: Decimal,
}

impl AccountValuation {
    /// True when every mark the valuation needs is valid.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.flags.is_empty()
    }
}

struct Pricing {
    /// Positive price per asset (stale and future included), reporting = 1.
    prices: BTreeMap<Asset, Decimal>,
    /// Status per registered asset.
    status: BTreeMap<Asset, MarkStatus>,
}

fn pricing(snapshot: &LedgerSnapshot, marks: &MarkSet, policy: &ValuationPolicy) -> Pricing {
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

fn validate(snapshot: &LedgerSnapshot, marks: &MarkSet, policy: &ValuationPolicy) -> Result<u32> {
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
        let collateral_factors: Option<Vec<Decimal>> = match (state, asset_price) {
            (MarkStatus::Valid, Some(p)) => Some(vec![balance.total, p]),
            (MarkStatus::Stale { .. }, Some(p)) => match policy.invalid_marks {
                InvalidMarkPolicy::HaircutStale { haircut } => Some(vec![
                    balance.total,
                    p,
                    sub(Decimal::ONE, haircut, "1 - haircut")?,
                ]),
                InvalidMarkPolicy::Exclude => None,
            },
            _ => None,
        };
        let collateral_value = match &collateral_factors {
            Some(factors) => {
                sum_terms_round(&[factors], scale, Rounding::TowardZero, "collateral")?
            }
            None => Decimal::ZERO,
        };
        if let Some(factors) = collateral_factors {
            collateral_terms.push(factors);
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
    let collateral_equity =
        round_terms(&collateral_terms, Rounding::TowardZero, "collateral equity")?;

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
        if position.quantity.is_zero()
            && position.cost_basis.is_zero()
            && position.realized_pnl.is_zero()
        {
            continue;
        }
        needed.insert(spot.base());
        needed.insert(spot.quote());
        if let (Some(base), Some(quote)) = (price(spot.base()), price(spot.quote())) {
            realized_terms.push(vec![position.realized_pnl, quote]);
            unrealized_terms.push(vec![position.quantity, base]);
            unrealized_terms.push(vec![-position.cost_basis, quote]);
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
