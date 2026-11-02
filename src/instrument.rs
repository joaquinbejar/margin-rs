//! Instrument definitions known to the ledger.
//!
//! - A spot trade exchanges the base asset against the quote asset.
//! - A linear future or perpetual is held as signed contracts with a
//!   multiplier: its position has a signed basis in the settlement asset,
//!   and only realized PnL and fees move cash (see
//!   [`Ledger`](crate::ledger::Ledger)).
//! - An inverse future or perpetual settles in the **base** asset: each
//!   contract is worth a fixed contract value `CV` in quote units, so `Q`
//!   contracts at price `p` are worth `Q * CV / p` of the base. Prices and
//!   marks of an inverse instrument are in the quote currency of `CV` per
//!   unit of the base; that currency is not an asset of the ledger and is
//!   never inferred.
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

/// Size of one contract of a derivative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractSize {
    /// Units of the underlying per contract: a linear contract (1 for spot).
    Multiplier(Decimal),
    /// Quote units per contract: an inverse contract, settled in the base.
    ContractValue(Decimal),
}

/// Terms shared by inverse futures and perpetuals: contracts settled in the
/// base asset, each worth `contract_value` units of the quote currency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InverseTerms {
    id: InstrumentId,
    settlement: Asset,
    contract_value: Decimal,
    price_scale: u32,
    contract_scale: u32,
}

impl InverseTerms {
    /// Defines the terms of an inverse derivative settled in `settlement`
    /// (the base asset), each contract worth `contract_value` units of the
    /// quote currency in which its prices are quoted.
    ///
    /// `price_scale` and `contract_scale` are the largest number of
    /// fractional digits accepted for execution prices and contract
    /// quantities. `contract_scale` plus the decimals of `contract_value`
    /// (trailing zeros ignored) must not exceed 28, so that `n * CV` is an
    /// exact `Decimal` product for every valid contract count `n`.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if `contract_value` is not positive, a
    /// scale exceeds [`MAX_SCALE`](crate::decimal::MAX_SCALE), or
    /// `contract_scale` plus the decimals of `contract_value` exceeds 28.
    pub fn new(
        id: InstrumentId,
        settlement: Asset,
        contract_value: Decimal,
        price_scale: u32,
        contract_scale: u32,
    ) -> Result<Self> {
        if contract_value <= Decimal::ZERO {
            return Err(MarginError::invalid_input(
                "contract_value",
                "must be positive",
            ));
        }
        check_configured_scale(price_scale, "price_scale")?;
        check_configured_scale(contract_scale, "contract_scale")?;
        if contract_scale.saturating_add(contract_value.normalize().scale()) > DECIMAL_DIGITS {
            return Err(MarginError::invalid_input(
                "contract_value",
                "its decimals plus contract_scale must not exceed 28",
            ));
        }
        Ok(Self {
            id,
            settlement,
            contract_value,
            price_scale,
            contract_scale,
        })
    }

    /// Instrument identifier.
    #[must_use]
    pub fn id(&self) -> &InstrumentId {
        &self.id
    }

    /// Asset the contracts settle in, the base: basis, PnL, funding and
    /// fees.
    #[must_use]
    pub fn settlement(&self) -> &Asset {
        &self.settlement
    }

    /// Quote units per contract (`CV`).
    #[must_use]
    pub fn contract_value(&self) -> Decimal {
        self.contract_value
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

/// The largest scale of a `Decimal`.
pub(crate) const DECIMAL_DIGITS: u32 = 28;

/// An inverse future: settled in the base asset at its expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InverseFuture {
    terms: InverseTerms,
    expiry_ms: u64,
}

impl InverseFuture {
    /// An inverse future expiring at `expiry_ms` (milliseconds since the
    /// epoch), settled with
    /// [`Ledger::settle_expiry`](crate::ledger::Ledger::settle_expiry).
    #[must_use]
    pub fn new(terms: InverseTerms, expiry_ms: u64) -> Self {
        Self { terms, expiry_ms }
    }

    /// The contract terms.
    #[must_use]
    pub fn terms(&self) -> &InverseTerms {
        &self.terms
    }

    /// Expiry, in milliseconds since the epoch.
    #[must_use]
    pub fn expiry_ms(&self) -> u64 {
        self.expiry_ms
    }
}

/// An inverse perpetual: no expiry; funding in the base asset
/// ([`Ledger::book_funding`](crate::ledger::Ledger::book_funding)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InversePerpetual {
    terms: InverseTerms,
}

impl InversePerpetual {
    /// An inverse perpetual with `terms`.
    #[must_use]
    pub fn new(terms: InverseTerms) -> Self {
        Self { terms }
    }

    /// The contract terms.
    #[must_use]
    pub fn terms(&self) -> &InverseTerms {
        &self.terms
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
    /// Contracts of an inverse future.
    InverseFuture,
    /// Contracts of an inverse perpetual.
    InversePerpetual,
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
    /// Inverse future, settled in the base asset at expiry.
    InverseFuture(InverseFuture),
    /// Inverse perpetual, with funding in the base asset.
    InversePerpetual(InversePerpetual),
}

impl Instrument {
    /// Instrument identifier.
    #[must_use]
    pub fn id(&self) -> &InstrumentId {
        match self {
            Self::Spot(spot) => spot.id(),
            Self::LinearFuture(future) => future.terms().id(),
            Self::LinearPerpetual(perpetual) => perpetual.terms().id(),
            Self::InverseFuture(future) => future.terms().id(),
            Self::InversePerpetual(perpetual) => perpetual.terms().id(),
        }
    }

    /// Asset in which positions, cost basis and PnL are measured: the quote
    /// asset of a spot instrument, the settlement asset of a derivative
    /// (the base asset of an inverse one).
    #[must_use]
    pub fn quote(&self) -> &Asset {
        match self {
            Self::Spot(spot) => spot.quote(),
            Self::LinearFuture(future) => future.terms().settlement(),
            Self::LinearPerpetual(perpetual) => perpetual.terms().settlement(),
            Self::InverseFuture(future) => future.terms().settlement(),
            Self::InversePerpetual(perpetual) => perpetual.terms().settlement(),
        }
    }

    /// The kind of position the instrument holds.
    #[must_use]
    pub fn kind(&self) -> PositionKind {
        match self {
            Self::Spot(_) => PositionKind::Spot,
            Self::LinearFuture(_) => PositionKind::LinearFuture,
            Self::LinearPerpetual(_) => PositionKind::LinearPerpetual,
            Self::InverseFuture(_) => PositionKind::InverseFuture,
            Self::InversePerpetual(_) => PositionKind::InversePerpetual,
        }
    }

    /// Size of one unit of position: `Multiplier(1)` for spot, the
    /// multiplier of a linear derivative, the contract value of an inverse
    /// one.
    #[must_use]
    pub fn contract_size(&self) -> ContractSize {
        match self {
            Self::Spot(_) => ContractSize::Multiplier(Decimal::ONE),
            Self::LinearFuture(future) => ContractSize::Multiplier(future.terms().multiplier()),
            Self::LinearPerpetual(perpetual) => {
                ContractSize::Multiplier(perpetual.terms().multiplier())
            }
            Self::InverseFuture(future) => {
                ContractSize::ContractValue(future.terms().contract_value())
            }
            Self::InversePerpetual(perpetual) => {
                ContractSize::ContractValue(perpetual.terms().contract_value())
            }
        }
    }

    /// The contract terms of a linear derivative; `None` for spot and
    /// inverse instruments.
    #[must_use]
    pub fn linear_terms(&self) -> Option<&LinearTerms> {
        match self {
            Self::LinearFuture(future) => Some(future.terms()),
            Self::LinearPerpetual(perpetual) => Some(perpetual.terms()),
            Self::Spot(_) | Self::InverseFuture(_) | Self::InversePerpetual(_) => None,
        }
    }

    /// The contract terms of an inverse derivative; `None` for spot and
    /// linear instruments.
    #[must_use]
    pub fn inverse_terms(&self) -> Option<&InverseTerms> {
        match self {
            Self::InverseFuture(future) => Some(future.terms()),
            Self::InversePerpetual(perpetual) => Some(perpetual.terms()),
            Self::Spot(_) | Self::LinearFuture(_) | Self::LinearPerpetual(_) => None,
        }
    }

    /// The expiry of a future; `None` for spot and perpetuals.
    #[must_use]
    pub fn expiry_ms(&self) -> Option<u64> {
        match self {
            Self::LinearFuture(future) => Some(future.expiry_ms()),
            Self::InverseFuture(future) => Some(future.expiry_ms()),
            Self::Spot(_) | Self::LinearPerpetual(_) | Self::InversePerpetual(_) => None,
        }
    }

    /// The margin product of a derivative; `None` for spot. Exhaustive, so
    /// a new kind of instrument must be decided here.
    pub(crate) fn derivative_product(&self) -> Option<crate::initial_margin::ProductKind> {
        use crate::initial_margin::ProductKind;
        match self {
            Self::Spot(_) => None,
            Self::LinearFuture(_) => Some(ProductKind::LinearFuture),
            Self::LinearPerpetual(_) => Some(ProductKind::LinearPerpetual),
            Self::InverseFuture(_) => Some(ProductKind::InverseFuture),
            Self::InversePerpetual(_) => Some(ProductKind::InversePerpetual),
        }
    }

    /// The derivative contract: settlement asset, contract size and grids;
    /// `None` for spot.
    pub(crate) fn derivative(&self) -> Option<DerivativeContract> {
        let (settlement, size, price_scale, contract_scale) = match self {
            Self::Spot(_) => return None,
            Self::LinearFuture(_) | Self::LinearPerpetual(_) => {
                let terms = self.linear_terms()?;
                (
                    terms.settlement(),
                    ContractSize::Multiplier(terms.multiplier()),
                    terms.price_scale(),
                    terms.contract_scale(),
                )
            }
            Self::InverseFuture(_) | Self::InversePerpetual(_) => {
                let terms = self.inverse_terms()?;
                (
                    terms.settlement(),
                    ContractSize::ContractValue(terms.contract_value()),
                    terms.price_scale(),
                    terms.contract_scale(),
                )
            }
        };
        Some(DerivativeContract {
            settlement: settlement.clone(),
            size,
            price_scale,
            contract_scale,
            expiry_ms: self.expiry_ms(),
        })
    }
}

/// What the ledger needs to book a derivative of either kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DerivativeContract {
    pub(crate) settlement: Asset,
    pub(crate) size: ContractSize,
    pub(crate) price_scale: u32,
    pub(crate) contract_scale: u32,
    pub(crate) expiry_ms: Option<u64>,
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

impl From<InverseFuture> for Instrument {
    fn from(future: InverseFuture) -> Self {
        Self::InverseFuture(future)
    }
}

impl From<InversePerpetual> for Instrument {
    fn from(perpetual: InversePerpetual) -> Self {
        Self::InversePerpetual(perpetual)
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

    #[test]
    fn inverse_terms_validation() {
        let id = InstrumentId::new("BTC-USD-INV").expect("valid id");
        let terms = InverseTerms::new(id.clone(), asset("BTC"), Decimal::from(100), 1, 0)
            .expect("valid terms");
        let perpetual = Instrument::from(InversePerpetual::new(terms.clone()));
        assert_eq!(perpetual.quote().as_str(), "BTC");
        assert_eq!(perpetual.kind(), PositionKind::InversePerpetual);
        assert_eq!(
            perpetual.contract_size(),
            ContractSize::ContractValue(Decimal::from(100))
        );
        assert_eq!(perpetual.linear_terms(), None);
        assert_eq!(perpetual.expiry_ms(), None);
        let future = Instrument::from(InverseFuture::new(terms, 1000));
        assert_eq!(future.expiry_ms(), Some(1000));
        for value in [Decimal::ZERO, Decimal::NEGATIVE_ONE] {
            assert!(InverseTerms::new(id.clone(), asset("BTC"), value, 1, 0).is_err());
        }
        // 18 contract decimals + 10 CV decimals = 28: accepted; trailing
        // zeros do not count; 29 is refused.
        let ten = Decimal::new(1, 10);
        assert!(InverseTerms::new(id.clone(), asset("BTC"), ten, 1, 18).is_ok());
        let padded = Decimal::new(1_000_000, 16);
        assert_eq!(padded.scale(), 16);
        assert!(InverseTerms::new(id.clone(), asset("BTC"), padded, 1, 18).is_ok());
        let eleven = Decimal::new(1, 11);
        assert!(InverseTerms::new(id, asset("BTC"), eleven, 1, 18).is_err());
    }
}
