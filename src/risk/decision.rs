//! Order requests and the gate's decisions on them.

use rust_decimal::Decimal;

use crate::error::MarginError;
use crate::ids::{Asset, InstrumentId, OrderId};
use crate::ledger::Side;

/// Why an order is sent. Informational: every purpose goes through the same
/// checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OrderPurpose {
    /// A market making quote.
    Quote,
    /// A hedge of existing exposure.
    Hedge,
    /// Anything else, for example an order proposed by a learned policy.
    #[default]
    Other,
}

/// A limit order proposed to the [`RiskGate`](super::RiskGate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRequest {
    /// Identifier, unique within the gate. A repeated id is rejected.
    pub id: OrderId,
    /// Instrument to trade.
    pub instrument: InstrumentId,
    /// Direction.
    pub side: Side,
    /// Limit price. Fills may only improve on it.
    pub price: Decimal,
    /// Requested quantity.
    pub quantity: Decimal,
    /// Whether the gate may approve a smaller quantity instead of rejecting.
    pub allow_reduce: bool,
    /// Why the order is sent.
    pub purpose: OrderPurpose,
}

impl OrderRequest {
    /// Builds a request that must be approved in full or rejected.
    #[must_use]
    pub fn new(
        id: OrderId,
        instrument: InstrumentId,
        side: Side,
        price: Decimal,
        quantity: Decimal,
    ) -> Self {
        Self {
            id,
            instrument,
            side,
            price,
            quantity,
            allow_reduce: false,
            purpose: OrderPurpose::Other,
        }
    }

    /// Allows the gate to approve a reduced quantity.
    #[must_use]
    pub fn reducible(mut self) -> Self {
        self.allow_reduce = true;
        self
    }

    /// Sets the purpose.
    #[must_use]
    pub fn with_purpose(mut self, purpose: OrderPurpose) -> Self {
        self.purpose = purpose;
        self
    }
}

/// Why trading is halted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HaltReason {
    /// Halted by an operator.
    Manual,
    /// An external circuit breaker (for example market-maker-rs
    /// `CircuitBreaker`) stopped allowing trading.
    CircuitBreaker,
    /// Market data was lost.
    DataDisconnect,
    /// A fill could not be booked: the account would be overdrawn. Set by
    /// the gate itself.
    AccountingBreach,
}

/// One reason behind a reduction or a rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RiskReason {
    /// The order id was already used.
    DuplicateOrderId,
    /// Trading is halted.
    Halted {
        /// Why.
        reason: HaltReason,
    },
    /// The instrument is not registered in the ledger.
    UnknownInstrument,
    /// The order itself is malformed (non positive price or quantity, or
    /// finer than the instrument precision).
    InvalidOrder {
        /// The validation error.
        error: MarginError,
    },
    /// A market data check is configured and no mark was received.
    NoMarketData,
    /// The last mark is older than allowed.
    StaleMarketData {
        /// Age of the last mark, in milliseconds.
        age_ms: u64,
        /// Maximum age allowed.
        max_age_ms: u64,
    },
    /// The limit price is too far from the mark.
    PriceOutsideBand {
        /// Last mark.
        mark: Decimal,
        /// Maximum relative distance allowed.
        band: Decimal,
    },
    /// The instrument already has the maximum number of live orders.
    MaxOpenOrders {
        /// Limit.
        limit: usize,
    },
    /// The quantity exceeds the per order maximum.
    MaxOrderQuantity {
        /// Limit.
        limit: Decimal,
    },
    /// The worst case position (position plus every live order on the same
    /// side plus this order) would exceed the limit.
    MaxPosition {
        /// Limit on the absolute worst case position.
        limit: Decimal,
        /// Largest quantity this order may still add.
        headroom: Decimal,
    },
    /// The worst case position valued at the order price would exceed the
    /// notional limit.
    MaxPositionNotional {
        /// Limit, in the quote asset.
        limit: Decimal,
        /// Largest quantity this order may still add.
        headroom: Decimal,
    },
    /// The reservation for the requested quantity exceeds the available
    /// balance.
    InsufficientBalance {
        /// Asset to reserve.
        asset: Asset,
        /// Reservation the requested quantity needs.
        required: Decimal,
        /// Available balance.
        available: Decimal,
    },
    /// After every cap the quantity left is below the minimum.
    BelowMinimumQuantity {
        /// Minimum order quantity.
        minimum: Decimal,
        /// Quantity left after the caps.
        allowed: Decimal,
    },
}

/// Decision of the gate on an [`OrderRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskDecision {
    /// Approved in full; the reservation is held.
    Accepted {
        /// Approved quantity (the requested one).
        quantity: Decimal,
    },
    /// Approved for a smaller quantity; the reservation is held for it.
    Reduced {
        /// Requested quantity.
        requested: Decimal,
        /// Approved quantity.
        quantity: Decimal,
        /// Every cap that reduced the order.
        reasons: Vec<RiskReason>,
    },
    /// Not approved; nothing is reserved.
    Rejected {
        /// Every reason found.
        reasons: Vec<RiskReason>,
    },
}

impl RiskDecision {
    /// Approved quantity, or `None` when rejected.
    #[must_use]
    pub fn approved_quantity(&self) -> Option<Decimal> {
        match self {
            Self::Accepted { quantity } | Self::Reduced { quantity, .. } => Some(*quantity),
            Self::Rejected { .. } => None,
        }
    }

    /// True when rejected.
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }

    /// Reasons for a reduction or rejection; empty when accepted.
    #[must_use]
    pub fn reasons(&self) -> &[RiskReason] {
        match self {
            Self::Accepted { .. } => &[],
            Self::Reduced { reasons, .. } | Self::Rejected { reasons } => reasons,
        }
    }
}
