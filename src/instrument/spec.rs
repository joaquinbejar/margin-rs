//! Conversion of a `financial_types` instrument specification into an
//! [`Instrument`] (feature `instruments`).
//!
//! [`Instrument::from_spec`] is a conversion only: it maps the fields
//! exactly, with no rounding, rescaling or currency substitution, and
//! returns a typed [`MarginError`] for anything the ledger cannot represent.
//!
//! | specification | instrument | contract size |
//! |---|---|---|
//! | linear spot | [`SpotInstrument`]: base the underlying, quote the price asset | |
//! | linear future | [`LinearFuture`], settled in the settlement asset | multiplier: the size amount |
//! | linear perpetual | [`LinearPerpetual`] | multiplier: the size amount |
//! | inverse future | [`InverseFuture`], settled in the settlement asset (the underlying) | contract value: the size amount |
//! | inverse perpetual | [`InversePerpetual`] | contract value: the size amount |
//! | option | none: [`MarginError::UnsupportedSpec`] | |
//!
//! The identifier and the asset codes go through [`InstrumentId::new`] and
//! [`Asset::new`], and the expiry (milliseconds since the epoch) is passed
//! unchanged.
//!
//! # Scales
//!
//! The ledger checks the fractional digits of execution prices and expiry
//! settlement prices against `price_scale`, and of execution quantities
//! against `contract_scale` (`quantity_scale` for spot). Venues often
//! publish marks, index and delivery prices with more digits than the order
//! tick, so the scales are explicit ([`InstrumentScales`]), never inferred.
//! Each must be at least the number of digits of the finest price tick and
//! of the lot, trailing zeros ignored ([`MarginError::ScaleBelowGrid`]).
//!
//! The scales are precision ceilings only: they do not enforce the grid. A
//! price of `0.3` has one digit but is off a `0.5` tick, and the ledger
//! books it; check orders with `InstrumentSpec::check_price` and
//! `InstrumentSpec::check_lot`. Nor does a conversion guarantee that the
//! instrument registers (see below).
//!
//! # What the conversion does not carry
//!
//! The conversion is not reversible. An [`Instrument`] has no field for:
//!
//! - the tick schedule and the lot (see above);
//! - the underlying of a linear derivative: its multiplier is in units of
//!   the underlying, and its prices and settlement are in the price asset;
//! - the price asset of an inverse derivative: its contract value and
//!   prices are in that currency, which is not a ledger asset;
//! - option terms (options are not converted).
//!
//! The lot is not a risk minimum either. When the risk limits of an
//! instrument leave `min_quantity` unset, the risk gate uses one unit at the
//! quantity scale (`10^-quantity_scale`) as the minimum order size: with a
//! lot of `0.25` and a scale of `2`, that is `0.01`, not `0.25`. Configure
//! `min_quantity` as the lot to make the gate agree with the venue.
//!
//! # Checks made at registration
//!
//! Some checks need the scale of an asset, which a specification does not
//! hold; [`Ledger::register_instrument`](crate::ledger::Ledger::register_instrument)
//! makes them:
//!
//! - every asset of the instrument must be registered;
//! - the quantity scale of a spot instrument must not exceed the scale of
//!   its base asset;
//! - the scale of the settlement asset of an inverse instrument plus its
//!   price scale must not exceed 28.
//!
//! # Rounding of inverse PnL
//!
//! `financial_types` gives the realized PnL of an inverse contract as one
//! exact amount, `±n·CV·(1/entry - 1/exit)`, and settles it with one
//! rounding. The ledger rounds each leg instead: the value of each fill,
//! `V = n·CV/p`, is rounded once (half even, at the settlement asset scale)
//! and the realized PnL is the difference of rounded values (see
//! `docs/ledger.md`). Both are exact policies, and they can differ by one
//! unit of the settlement asset: long 100 contracts of 10 USD from 60000 to
//! 61000 is exactly `1/3660` BTC, `0.00027322` settled once, while the
//! ledger books `0.01666667 - 0.01639344 = 0.00027323`. The exact rest of
//! each rounded value is recorded in the fill's `InverseResidual`, so the
//! exact PnL is always recoverable from the bookings.

use core::convert::TryFrom;

use financial_types::instrument::{ContractKind, InstrumentSpec, Margining};
use rust_decimal::Decimal;

use super::{
    Instrument, InverseFuture, InversePerpetual, InverseTerms, LinearFuture, LinearPerpetual,
    LinearTerms, SpotInstrument,
};
use crate::error::{MarginError, Result};
use crate::ids::{Asset, InstrumentId};

/// The precision of an instrument converted from a specification: the
/// largest number of fractional digits the ledger accepts.
///
/// # Examples
///
/// ```
/// use margin_rs::instrument::InstrumentScales;
///
/// // Deribit BTC-PERPETUAL: tick 0.5 (1 digit), whole contracts; marks
/// // and settlement prices with 2 decimals.
/// let scales = InstrumentScales {
///     price_scale: 2,
///     contract_scale: 0,
/// };
/// assert_eq!(scales.price_scale, 2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstrumentScales {
    /// Digits of execution prices and expiry settlement prices.
    pub price_scale: u32,
    /// Digits of contract quantities (the base quantity for spot).
    pub contract_scale: u32,
}

/// The number of fractional digits of `value`, trailing zeros ignored.
fn digits(value: Decimal) -> u32 {
    value.normalize().scale()
}

impl Instrument {
    /// Converts `spec` into an instrument with the precision `scales`; see
    /// the [module documentation](crate::instrument::spec) for the mapping.
    ///
    /// # Errors
    ///
    /// Checked in this order:
    ///
    /// - [`MarginError::UnsupportedSpec`] for an option;
    /// - [`MarginError::ScaleBelowGrid`] if `scales.price_scale` is below
    ///   the digits of a price tick, or `scales.contract_scale` below the
    ///   digits of the lot;
    /// - [`MarginError::InvalidInput`] if a constructor refuses a field: a
    ///   scale above [`MAX_SCALE`](crate::decimal::MAX_SCALE), or a
    ///   contract value whose digits plus `contract_scale` exceed 28.
    ///
    /// # Examples
    ///
    /// ```
    /// use financial_types::instrument::reference::deribit;
    /// use margin_rs::instrument::InstrumentScales;
    /// use margin_rs::{Decimal, Instrument, PositionKind};
    ///
    /// let spec = deribit::btc_perpetual()?;
    /// let scales = InstrumentScales {
    ///     price_scale: 2,
    ///     contract_scale: 0,
    /// };
    /// let instrument = Instrument::from_spec(&spec, scales)?;
    /// assert_eq!(instrument.kind(), PositionKind::InversePerpetual);
    /// assert_eq!(instrument.quote().as_str(), "BTC");
    /// let terms = instrument.inverse_terms().ok_or("inverse")?;
    /// assert_eq!(terms.contract_value(), Decimal::TEN);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn from_spec(spec: &InstrumentSpec, scales: InstrumentScales) -> Result<Self> {
        let id = InstrumentId::new(spec.id().as_str())?;
        let (kind, margining) = (spec.kind(), spec.margining());
        if kind == ContractKind::Option {
            return Err(MarginError::unsupported_spec(id, kind, margining));
        }
        let tick_digits = spec
            .price_ticks()
            .bands()
            .iter()
            .map(|band| digits(band.tick))
            .max()
            .unwrap_or(0);
        if scales.price_scale < tick_digits {
            return Err(MarginError::scale_below_grid(
                id,
                "price_scale",
                "price ticks",
                scales.price_scale,
                tick_digits,
            ));
        }
        let lot_digits = digits(spec.lot());
        if scales.contract_scale < lot_digits {
            return Err(MarginError::scale_below_grid(
                id,
                "contract_scale",
                "lot",
                scales.contract_scale,
                lot_digits,
            ));
        }

        let InstrumentScales {
            price_scale,
            contract_scale,
        } = scales;
        let settlement = Asset::new(spec.settlement_asset().as_str())?;
        let size = spec.size().amount();
        // `ContractKind` and `Margining` are `#[non_exhaustive]` in
        // `financial_types`, so the last arm is required: a kind or a
        // margining added there has no ledger instrument until it is mapped
        // here.
        match (kind, margining, spec.expiry_ms()) {
            (ContractKind::Spot, Margining::Linear, None) => Ok(SpotInstrument::new(
                id,
                Asset::new(spec.underlying().as_str())?,
                Asset::new(spec.price_asset().as_str())?,
                price_scale,
                contract_scale,
            )?
            .into()),
            (ContractKind::Future, Margining::Linear, Some(expiry_ms)) => {
                let terms = LinearTerms::new(id, settlement, size, price_scale, contract_scale)?;
                Ok(LinearFuture::new(terms, expiry_ms).into())
            }
            (ContractKind::Perpetual, Margining::Linear, None) => {
                let terms = LinearTerms::new(id, settlement, size, price_scale, contract_scale)?;
                Ok(LinearPerpetual::new(terms).into())
            }
            (ContractKind::Future, Margining::Inverse, Some(expiry_ms)) => {
                let terms = InverseTerms::new(id, settlement, size, price_scale, contract_scale)?;
                Ok(InverseFuture::new(terms, expiry_ms).into())
            }
            (ContractKind::Perpetual, Margining::Inverse, None) => {
                let terms = InverseTerms::new(id, settlement, size, price_scale, contract_scale)?;
                Ok(InversePerpetual::new(terms).into())
            }
            _ => Err(MarginError::unsupported_spec(id, kind, margining)),
        }
    }
}

/// The same conversion as [`Instrument::from_spec`].
///
/// # Errors
///
/// As [`Instrument::from_spec`].
///
/// # Examples
///
/// ```
/// use financial_types::instrument::reference::binance_usdm;
/// use margin_rs::instrument::InstrumentScales;
/// use margin_rs::{Decimal, Instrument};
///
/// let spec = binance_usdm::btcusdt_perpetual()?;
/// let scales = InstrumentScales {
///     price_scale: 1,
///     contract_scale: 3,
/// };
/// let instrument = Instrument::try_from((&spec, scales))?;
/// assert_eq!(instrument.quote().as_str(), "USDT");
/// let terms = instrument.linear_terms().ok_or("linear")?;
/// assert_eq!(terms.multiplier(), Decimal::ONE);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
impl TryFrom<(&InstrumentSpec, InstrumentScales)> for Instrument {
    type Error = MarginError;

    fn try_from((spec, scales): (&InstrumentSpec, InstrumentScales)) -> Result<Self> {
        Self::from_spec(spec, scales)
    }
}
