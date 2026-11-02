//! Instrument definitions known to the ledger.
//!
//! Only spot instruments exist so far. A spot trade exchanges the base asset
//! against the quote asset. Derivative products (futures, perpetuals,
//! options) will be added as further [`Instrument`] variants.

use crate::decimal::check_configured_scale;
use crate::error::{MarginError, Result};
use crate::ids::{Asset, InstrumentId};

/// Spot instrument: buying `quantity` at `price` receives `quantity` of the
/// base asset and pays `price * quantity` of the quote asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotInstrument {
    id: InstrumentId,
    base: Asset,
    quote: Asset,
    price_scale: u32,
    quantity_scale: u32,
}

impl SpotInstrument {
    /// Defines a spot instrument.
    ///
    /// `price_scale` and `quantity_scale` are the largest number of
    /// fractional digits accepted for execution prices and quantities (the
    /// tick and lot precision). The ledger also requires
    /// `quantity_scale <= ` the base asset scale, so a fill quantity always
    /// fits the base balance precision.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if base and quote are the same asset or
    /// a scale exceeds [`MAX_SCALE`](crate::decimal::MAX_SCALE).
    pub fn new(
        id: InstrumentId,
        base: Asset,
        quote: Asset,
        price_scale: u32,
        quantity_scale: u32,
    ) -> Result<Self> {
        if base == quote {
            return Err(MarginError::invalid_input(
                "quote",
                "base and quote assets must differ",
            ));
        }
        check_configured_scale(price_scale, "price_scale")?;
        check_configured_scale(quantity_scale, "quantity_scale")?;
        Ok(Self {
            id,
            base,
            quote,
            price_scale,
            quantity_scale,
        })
    }

    /// Instrument identifier.
    #[must_use]
    pub fn id(&self) -> &InstrumentId {
        &self.id
    }

    /// Asset received when buying.
    #[must_use]
    pub fn base(&self) -> &Asset {
        &self.base
    }

    /// Asset paid when buying; positions and PnL are measured in it.
    #[must_use]
    pub fn quote(&self) -> &Asset {
        &self.quote
    }

    /// Maximum fractional digits of an execution price.
    #[must_use]
    pub fn price_scale(&self) -> u32 {
        self.price_scale
    }

    /// Maximum fractional digits of an execution quantity.
    #[must_use]
    pub fn quantity_scale(&self) -> u32 {
        self.quantity_scale
    }
}

/// An instrument the ledger can book executions for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Instrument {
    /// Spot exchange of a base asset against a quote asset.
    Spot(SpotInstrument),
}

impl Instrument {
    /// Instrument identifier.
    #[must_use]
    pub fn id(&self) -> &InstrumentId {
        match self {
            Self::Spot(spot) => spot.id(),
        }
    }

    /// Asset in which positions, cost basis and PnL are measured.
    #[must_use]
    pub fn quote(&self) -> &Asset {
        match self {
            Self::Spot(spot) => spot.quote(),
        }
    }
}

impl From<SpotInstrument> for Instrument {
    fn from(spot: SpotInstrument) -> Self {
        Self::Spot(spot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(code: &str) -> Asset {
        Asset::new(code).expect("valid asset")
    }

    #[test]
    fn spot_instrument_validation() {
        let id = InstrumentId::new("BTC-USDC").expect("valid id");
        let spot =
            SpotInstrument::new(id.clone(), asset("BTC"), asset("USDC"), 2, 8).expect("valid spot");
        assert_eq!(spot.base().as_str(), "BTC");
        assert_eq!(Instrument::from(spot).quote().as_str(), "USDC");
        assert!(SpotInstrument::new(id.clone(), asset("BTC"), asset("BTC"), 2, 8).is_err());
        assert!(SpotInstrument::new(id, asset("BTC"), asset("USDC"), 19, 8).is_err());
    }
}
