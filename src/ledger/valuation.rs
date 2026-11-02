//! Mark to market valuation.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::decimal::{Rounding, sum_products_round};
use crate::error::{MarginError, Result};
use crate::ids::{Asset, InstrumentId};

/// Valuation of one position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionValuation {
    /// Asset the amounts are expressed in.
    pub quote: Asset,
    /// Net traded quantity.
    pub quantity: Decimal,
    /// Mark price used, or `None` for a flat position (no mark needed).
    pub mark: Option<Decimal>,
    /// `round(quantity * mark)`; zero when flat.
    pub market_value: Decimal,
    /// Signed cost attributed to the open quantity.
    pub cost_basis: Decimal,
    /// `market_value - cost_basis`.
    pub unrealized_pnl: Decimal,
    /// Realized PnL gross of fees.
    pub realized_pnl: Decimal,
}

/// PnL of one asset. Amounts are all in that asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AssetPnl {
    /// Realized PnL of the positions quoted in this asset, gross of fees.
    pub realized_pnl: Decimal,
    /// Unrealized PnL of the positions quoted in this asset.
    pub unrealized_pnl: Decimal,
    /// Fees paid in this asset (negative for net rebates).
    pub fees_paid: Decimal,
    /// `realized_pnl + unrealized_pnl - fees_paid`.
    pub net_pnl: Decimal,
}

/// Result of [`Ledger::valuation`](crate::ledger::Ledger::valuation).
///
/// # Valuation policy
///
/// # Valuation policy
///
/// - An open position is valued at a caller supplied mark price:
///   `market_value = round(quantity * mark)` to the quote asset scale, and
///   `unrealized_pnl = market_value - cost_basis`. For a short both terms are
///   negative, so a falling mark yields a positive unrealized PnL.
/// - Realized PnL is gross of fees. Fees are reported separately, per asset.
/// - Results are grouped by asset. `net_pnl` of an asset is
///   `realized_pnl + unrealized_pnl - fees_paid` for that asset only: a fee
///   paid in a third asset appears in that asset's line and is never folded
///   into another asset.
/// - Amounts in different assets are only combined by [`Valuation::net_in`],
///   with caller supplied conversion rates.
///
/// Note that `quantity * mark` alone is a notional, not a PnL; market-maker-rs
/// currently stores that notional as `pnl.unrealized`, which this policy
/// replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Valuation {
    /// Valuation per instrument.
    pub positions: BTreeMap<InstrumentId, PositionValuation>,
    /// PnL per asset; amounts of different assets are never summed here.
    pub by_asset: BTreeMap<Asset, AssetPnl>,
    pub(crate) scales: BTreeMap<Asset, u32>,
    pub(crate) rounding: Rounding,
}

impl Valuation {
    /// Converts every asset's `net_pnl` into `target` and sums them.
    ///
    /// `rates` maps an asset to the price of one unit of it in `target`
    /// (the rate of `target` itself is 1 and need not be given). This is the
    /// only operation in the crate that combines different assets.
    ///
    /// # Precision
    ///
    /// The products and their sum are computed exactly, then rounded once
    /// to the scale of `target` with the ledger's rounding mode. The result
    /// is the exact converted sum rounded to `target`'s precision: within
    /// half a unit of its last digit under half even rounding (for example
    /// 0.0000005 USDC at scale 6). The rates themselves are taken as exact.
    ///
    /// # Errors
    ///
    /// - [`MarginError::UnknownAsset`] if `target` is not a registered asset.
    /// - [`MarginError::MissingConversionRate`] if an asset with a non zero
    ///   `net_pnl` has no rate.
    /// - [`MarginError::InvalidInput`] if a used rate is not positive.
    /// - [`MarginError::Arithmetic`] on overflow.
    pub fn net_in(&self, target: &Asset, rates: &BTreeMap<Asset, Decimal>) -> Result<Decimal> {
        let scale = *self
            .scales
            .get(target)
            .ok_or_else(|| MarginError::UnknownAsset {
                asset: target.clone(),
            })?;
        let mut terms = Vec::with_capacity(self.by_asset.len());
        for (asset, pnl) in &self.by_asset {
            if pnl.net_pnl.is_zero() {
                continue;
            }
            let rate = if asset == target {
                Decimal::ONE
            } else {
                let rate = *rates
                    .get(asset)
                    .ok_or_else(|| MarginError::MissingConversionRate {
                        from: asset.clone(),
                        to: target.clone(),
                    })?;
                if rate <= Decimal::ZERO {
                    return Err(MarginError::invalid_input("rate", "must be positive"));
                }
                rate
            };
            terms.push((pnl.net_pnl, rate));
        }
        sum_products_round(&terms, scale, self.rounding, "sum of net_pnl * rate")
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn asset(code: &str) -> Asset {
        Asset::new(code).expect("valid asset")
    }

    fn line(net_pnl: Decimal) -> AssetPnl {
        AssetPnl {
            net_pnl,
            ..AssetPnl::default()
        }
    }

    /// M104-3: the conversion is exact before its single rounding.
    /// A = MAX, B = 0.1, USD = -MAX at rate 1 sum to 0.1, not 0.
    #[test]
    fn net_in_is_exact_before_rounding() {
        let (a, b, usd) = (asset("A"), asset("B"), asset("USD"));
        let valuation = Valuation {
            positions: BTreeMap::new(),
            by_asset: BTreeMap::from([
                (a.clone(), line(Decimal::MAX)),
                (b.clone(), line(dec!(0.1))),
                (usd.clone(), line(Decimal::MIN)),
            ]),
            scales: BTreeMap::from([(a.clone(), 6), (b.clone(), 6), (usd.clone(), 6)]),
            rounding: Rounding::HalfEven,
        };
        let rates = BTreeMap::from([(a, dec!(1)), (b, dec!(1))]);
        assert_eq!(valuation.net_in(&usd, &rates), Ok(dec!(0.1)));
    }
}
