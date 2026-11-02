//! Instrument definitions known to the ledger.
//!
//! - A spot trade exchanges the base asset against the quote asset.
//! - A linear future or perpetual is held as signed contracts with a
//!   multiplier: its position has a signed basis in the settlement asset,
//!   and only realized PnL and fees move cash (see
//!   [`Ledger`](crate::ledger::Ledger)).
//!
//! Options are not ledger instruments yet.

use rust_decimal::Decimal;

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

/// Terms shared by linear futures and perpetuals: contracts settled in one
/// asset, each worth `multiplier` units of the underlying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearTerms {
    id: InstrumentId,
    settlement: Asset,
    multiplier: Decimal,
    price_scale: u32,
    contract_scale: u32,
}

impl LinearTerms {
    /// Defines the terms of a linear derivative.
    ///
    /// `price_scale` and `contract_scale` are the largest number of
    /// fractional digits accepted for execution prices and contract
    /// quantities.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if `multiplier` is not positive or a
    /// scale exceeds [`MAX_SCALE`](crate::decimal::MAX_SCALE).
    pub fn new(
        id: InstrumentId,
        settlement: Asset,
        multiplier: Decimal,
        price_scale: u32,
        contract_scale: u32,
    ) -> Result<Self> {
        if multiplier <= Decimal::ZERO {
            return Err(MarginError::invalid_input("multiplier", "must be positive"));
        }
        check_configured_scale(price_scale, "price_scale")?;
        check_configured_scale(contract_scale, "contract_scale")?;
        Ok(Self {
            id,
            settlement,
            multiplier,
            price_scale,
            contract_scale,
        })
    }

    /// Instrument identifier.
    #[must_use]
    pub fn id(&self) -> &InstrumentId {
        &self.id
    }

    /// Asset the contracts settle in: basis, PnL, funding and fees.
    #[must_use]
    pub fn settlement(&self) -> &Asset {
        &self.settlement
    }

    /// Units of the underlying per contract.
    #[must_use]
    pub fn multiplier(&self) -> Decimal {
        self.multiplier
    }

    /// Maximum fractional digits of an execution price.
    #[must_use]
    pub fn price_scale(&self) -> u32 {
        self.price_scale
    }

    /// Maximum fractional digits of a contract quantity.
    #[must_use]
    pub fn contract_scale(&self) -> u32 {
        self.contract_scale
    }
}

/// A linear future: settled in cash at its expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearFuture {
    terms: LinearTerms,
    expiry_ms: u64,
}

impl LinearFuture {
    /// A linear future expiring at `expiry_ms` (milliseconds since the
    /// epoch). Settlement at expiry is booked with
    /// [`Ledger::settle_expiry`](crate::ledger::Ledger::settle_expiry).
    #[must_use]
    pub fn new(terms: LinearTerms, expiry_ms: u64) -> Self {
        Self { terms, expiry_ms }
    }

    /// The contract terms.
    #[must_use]
    pub fn terms(&self) -> &LinearTerms {
        &self.terms
    }

    /// Expiry, in milliseconds since the epoch.
    #[must_use]
    pub fn expiry_ms(&self) -> u64 {
        self.expiry_ms
    }
}

/// A linear perpetual: no expiry; funding is exchanged periodically
/// ([`Ledger::book_funding`](crate::ledger::Ledger::book_funding)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearPerpetual {
    terms: LinearTerms,
}

impl LinearPerpetual {
    /// A linear perpetual with `terms`.
    #[must_use]
    pub fn new(terms: LinearTerms) -> Self {
        Self { terms }
    }

    /// The contract terms.
    #[must_use]
    pub fn terms(&self) -> &LinearTerms {
        &self.terms
    }
}

/// What kind of position an instrument holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PositionKind {
    /// Spot: the base asset itself.
    Spot,
    /// Contracts of a linear future.
    LinearFuture,
    /// Contracts of a linear perpetual.
    LinearPerpetual,
}

/// An instrument the ledger can book executions for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Instrument {
    /// Spot exchange of a base asset against a quote asset.
    Spot(SpotInstrument),
    /// Linear future, settled in cash at expiry.
    LinearFuture(LinearFuture),
    /// Linear perpetual, with funding.
    LinearPerpetual(LinearPerpetual),
}

impl Instrument {
    /// Instrument identifier.
    #[must_use]
    pub fn id(&self) -> &InstrumentId {
        match self {
            Self::Spot(spot) => spot.id(),
            Self::LinearFuture(future) => future.terms().id(),
            Self::LinearPerpetual(perpetual) => perpetual.terms().id(),
        }
    }

    /// Asset in which positions, cost basis and PnL are measured: the quote
    /// asset of a spot instrument, the settlement asset of a derivative.
    #[must_use]
    pub fn quote(&self) -> &Asset {
        match self {
            Self::Spot(spot) => spot.quote(),
            Self::LinearFuture(future) => future.terms().settlement(),
            Self::LinearPerpetual(perpetual) => perpetual.terms().settlement(),
        }
    }

    /// The kind of position the instrument holds.
    #[must_use]
    pub fn kind(&self) -> PositionKind {
        match self {
            Self::Spot(_) => PositionKind::Spot,
            Self::LinearFuture(_) => PositionKind::LinearFuture,
            Self::LinearPerpetual(_) => PositionKind::LinearPerpetual,
        }
    }

    /// Units of the underlying per unit of position: 1 for spot, the
    /// contract multiplier for a derivative.
    #[must_use]
    pub fn multiplier(&self) -> Decimal {
        self.linear_terms()
            .map_or(Decimal::ONE, LinearTerms::multiplier)
    }

    /// The contract terms of a derivative, `None` for spot.
    #[must_use]
    pub fn linear_terms(&self) -> Option<&LinearTerms> {
        match self {
            Self::Spot(_) => None,
            Self::LinearFuture(future) => Some(future.terms()),
            Self::LinearPerpetual(perpetual) => Some(perpetual.terms()),
        }
    }
}

impl From<SpotInstrument> for Instrument {
    fn from(spot: SpotInstrument) -> Self {
        Self::Spot(spot)
    }
}

impl From<LinearFuture> for Instrument {
    fn from(future: LinearFuture) -> Self {
        Self::LinearFuture(future)
    }
}

impl From<LinearPerpetual> for Instrument {
    fn from(perpetual: LinearPerpetual) -> Self {
        Self::LinearPerpetual(perpetual)
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
