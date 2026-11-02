//! Pretrade risk gate: reservations and hard limits for every order.
//!
//! The [`RiskGate`] owns the [`Ledger`]. Every order (quote, hedge or a
//! policy's proposal) is submitted to [`RiskGate::submit`], which returns a
//! [`RiskDecision`]: accepted, reduced or rejected, with typed reasons. An
//! approved order holds a reservation in the ledger for its worst case
//! debit until the order ends:
//!
//! - a fill shrinks the reservation to what the remaining quantity needs,
//!   in the same atomic step that books the fill, so a repeated fill
//!   message adjusts it exactly once;
//! - a venue reject or a confirmed cancel releases what is left;
//! - a requested cancel keeps the reservation, because the order can still
//!   fill until the venue confirms the cancel.
//!
//! The ledger is only reachable read only ([`RiskGate::ledger`]), so there
//! is no path that books or reserves around the gate:
//!
//! ```compile_fail,E0596
//! # use margin_rs::ledger::{Execution, Ledger, Side};
//! # use margin_rs::risk::RiskGate;
//! # use margin_rs::{ExecutionId, InstrumentId, Decimal};
//! # fn bypass(gate: &mut RiskGate, execution: &Execution) {
//! // Does not compile: the gate only lends `&Ledger`.
//! gate.ledger().book_execution(execution);
//! # }
//! ```
//!
//! # Example
//!
//! ```
//! use margin_rs::ledger::{Execution, Ledger, Side};
//! use margin_rs::risk::{InstrumentLimits, OrderRequest, RiskDecision, RiskGate};
//! use margin_rs::{Asset, Decimal, ExecutionId, InstrumentId, OrderId, SpotInstrument, TransferId};
//!
//! # fn main() -> margin_rs::Result<()> {
//! let (usdc, btc) = (Asset::new("USDC")?, Asset::new("BTC")?);
//! let pair = InstrumentId::new("BTC-USDC")?;
//! let mut ledger = Ledger::default();
//! ledger.register_asset(usdc.clone(), 6)?;
//! ledger.register_asset(btc.clone(), 8)?;
//! ledger.register_instrument(SpotInstrument::new(pair.clone(), btc, usdc.clone(), 2, 8)?)?;
//!
//! let mut gate = RiskGate::new(ledger);
//! gate.deposit(TransferId::new("d1")?, usdc.clone(), Decimal::from(1000))?;
//! gate.set_limits(&pair, InstrumentLimits { max_position: Some(Decimal::from(5)), ..Default::default() })?;
//!
//! let order = OrderRequest::new(OrderId::new("q1")?, pair.clone(), Side::Buy, Decimal::from(100), Decimal::from(8))
//!     .reducible();
//! let decision = gate.submit(&order, 0);
//! assert_eq!(decision.approved_quantity(), Some(Decimal::from(5)));
//! assert_eq!(gate.ledger().balance(&usdc).map(|b| b.reserved), Some(Decimal::from(500)));
//!
//! let fill = Execution::new(ExecutionId::new("e1")?, pair, Side::Buy, Decimal::from(100), Decimal::from(5));
//! gate.on_fill(&order.id, &fill)?;
//! assert_eq!(gate.ledger().balance(&usdc).map(|b| (b.total, b.reserved)), Some((Decimal::from(500), Decimal::ZERO)));
//! # Ok(())
//! # }
//! ```
//!
//! # Reservation size
//!
//! - Buy: `round_up(price * remaining * (1 + max_fee_rate))` of the quote
//!   asset, rounded away from zero to the quote scale. Fills happen at the
//!   limit price or better, so this covers the cash and the fee as long as
//!   the venue fee rate does not exceed `max_fee_rate`.
//! - Sell: `remaining` of the base asset. Spot sell fees are assumed to be
//!   charged in the quote asset and paid out of the proceeds.
//!
//! # Limits on joint exposure
//!
//! Limits apply to the worst case position: the current position plus every
//! live order on the same side (including orders with a cancel pending,
//! which may still fill) plus the new order. See [`InstrumentLimits`].
//!
//! # Circuit breaker and quote withdrawal
//!
//! [`RiskGate::trip`] halts trading: every later submit is rejected with
//! [`RiskReason::Halted`], and the live orders that have no cancel pending
//! yet are moved to cancel pending and returned, so the caller sends
//! exactly those cancels (quote withdrawal). Their reservations stay until
//! each cancel is confirmed, and fills that race the cancels are booked
//! normally. [`RiskGate::resume`] lifts the halt. The gate trips itself with
//! [`HaltReason::AccountingBreach`] if a fill cannot be booked.

mod decision;

use std::collections::BTreeMap;

use rust_decimal::Decimal;

pub use decision::{HaltReason, OrderPurpose, OrderRequest, RiskDecision, RiskReason};

use crate::decimal::{Rounding, add, check_scale, mul_div_round, sub};
use crate::error::{MarginError, Result};
use crate::ids::{Asset, InstrumentId, OrderId, ReservationId, TransferId};
use crate::instrument::{Instrument, SpotInstrument};
use crate::ledger::{Booking, Execution, Ledger, Side};

/// Hard limits for one instrument. `None` disables a limit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstrumentLimits {
    /// Maximum quantity of a single order.
    pub max_order_quantity: Option<Decimal>,
    /// Maximum absolute worst case position, in base units.
    pub max_position: Option<Decimal>,
    /// Maximum absolute worst case position valued at the order price, in
    /// the quote asset.
    pub max_position_notional: Option<Decimal>,
    /// Maximum number of live orders.
    pub max_open_orders: Option<usize>,
    /// Minimum order quantity; defaults to one lot (`10^-quantity_scale`).
    pub min_quantity: Option<Decimal>,
    /// Fee rate reserved on top of the notional of buy orders, as a
    /// fraction (0.001 is 10 bp). Zero by default.
    pub max_fee_rate: Decimal,
    /// Maximum relative distance between the limit price and the last mark
    /// (0.05 is 5 %). Requires market data.
    pub price_band: Option<Decimal>,
    /// Maximum age of the last mark, in milliseconds. Requires market data.
    pub max_data_age_ms: Option<u64>,
}

impl InstrumentLimits {
    fn validate(&self, spot: &SpotInstrument) -> Result<()> {
        let positive = |value: Option<Decimal>, field: &'static str| -> Result<()> {
            match value {
                Some(v) if v <= Decimal::ZERO => {
                    Err(MarginError::invalid_input(field, "must be positive"))
                }
                _ => Ok(()),
            }
        };
        positive(self.max_order_quantity, "max_order_quantity")?;
        positive(self.max_position, "max_position")?;
        positive(self.max_position_notional, "max_position_notional")?;
        positive(self.min_quantity, "min_quantity")?;
        positive(self.price_band, "price_band")?;
        if let Some(min) = self.min_quantity {
            check_scale(min, spot.quantity_scale(), "min_quantity")?;
        }
        if self.max_fee_rate < Decimal::ZERO {
            return Err(MarginError::invalid_input(
                "max_fee_rate",
                "must not be negative",
            ));
        }
        if self.max_open_orders == Some(0) {
            return Err(MarginError::invalid_input(
                "max_open_orders",
                "must be positive",
            ));
        }
        Ok(())
    }
}

/// Lifecycle state of an order known to the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderState {
    /// Approved and sent; not yet acknowledged by the venue.
    PendingNew,
    /// Acknowledged and resting.
    Open,
    /// A cancel was requested; the order may still fill.
    CancelPending,
    /// Completely filled.
    Filled,
    /// Cancel confirmed.
    Canceled,
    /// Rejected by the gate or by the venue.
    Rejected,
}

impl OrderState {
    /// True while the order may still fill.
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::PendingNew | Self::Open | Self::CancelPending)
    }

    /// Name used in errors.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::PendingNew => "pending_new",
            Self::Open => "open",
            Self::CancelPending => "cancel_pending",
            Self::Filled => "filled",
            Self::Canceled => "canceled",
            Self::Rejected => "rejected",
        }
    }
}

/// An order known to the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRecord {
    /// Identifier.
    pub id: OrderId,
    /// Instrument.
    pub instrument: InstrumentId,
    /// Direction.
    pub side: Side,
    /// Limit price.
    pub price: Decimal,
    /// Approved quantity (zero for an order rejected by the gate).
    pub quantity: Decimal,
    /// Quantity filled so far.
    pub filled: Decimal,
    /// Current state.
    pub state: OrderState,
    /// Decision taken at submission.
    pub decision: RiskDecision,
}

impl OrderRecord {
    /// Quantity still open.
    #[must_use]
    pub fn remaining(&self) -> Decimal {
        // filled <= quantity is enforced on every fill.
        self.quantity - self.filled
    }

    fn reservation_id(&self) -> Result<ReservationId> {
        ReservationId::new(self.id.as_str())
    }
}

/// The pretrade risk gate. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct RiskGate {
    ledger: Ledger,
    limits: BTreeMap<InstrumentId, InstrumentLimits>,
    marks: BTreeMap<InstrumentId, (Decimal, u64)>,
    orders: BTreeMap<OrderId, OrderRecord>,
    halt: Option<HaltReason>,
}

impl RiskGate {
    /// Wraps `ledger`. Every later change to the ledger goes through the
    /// gate.
    #[must_use]
    pub fn new(ledger: Ledger) -> Self {
        Self {
            ledger,
            limits: BTreeMap::new(),
            marks: BTreeMap::new(),
            orders: BTreeMap::new(),
            halt: None,
        }
    }

    /// Read only access to the ledger.
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Deposits into the ledger. See [`Ledger::deposit`].
    ///
    /// # Errors
    ///
    /// As [`Ledger::deposit`].
    pub fn deposit(&mut self, id: TransferId, asset: Asset, amount: Decimal) -> Result<bool> {
        self.ledger.deposit(id, asset, amount)
    }

    /// Withdraws from the available balance. See [`Ledger::withdraw`].
    ///
    /// # Errors
    ///
    /// As [`Ledger::withdraw`].
    pub fn withdraw(&mut self, id: TransferId, asset: Asset, amount: Decimal) -> Result<bool> {
        self.ledger.withdraw(id, asset, amount)
    }

    /// Sets the limits of a registered instrument.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownInstrument`], or [`MarginError::InvalidInput`]
    /// / [`MarginError::PrecisionExceeded`] for an invalid limit.
    pub fn set_limits(
        &mut self,
        instrument: &InstrumentId,
        limits: InstrumentLimits,
    ) -> Result<()> {
        let spot = self.spot(instrument)?;
        limits.validate(&spot)?;
        self.limits.insert(instrument.clone(), limits);
        Ok(())
    }

    /// Limits in force for `instrument` (all disabled if never set).
    #[must_use]
    pub fn limits(&self, instrument: &InstrumentId) -> InstrumentLimits {
        self.limits.get(instrument).cloned().unwrap_or_default()
    }

    /// Records a mark price received at `timestamp_ms`.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownInstrument`]; [`MarginError::InvalidInput`] if
    /// the mark is not positive or the timestamp is older than the last one.
    pub fn on_market_data(
        &mut self,
        instrument: &InstrumentId,
        mark: Decimal,
        timestamp_ms: u64,
    ) -> Result<()> {
        self.spot(instrument)?;
        if mark <= Decimal::ZERO {
            return Err(MarginError::invalid_input("mark", "must be positive"));
        }
        if let Some((_, last)) = self.marks.get(instrument) {
            if timestamp_ms < *last {
                return Err(MarginError::invalid_input(
                    "timestamp_ms",
                    "must not go backwards",
                ));
            }
        }
        self.marks.insert(instrument.clone(), (mark, timestamp_ms));
        Ok(())
    }

    /// Current halt, if any.
    #[must_use]
    pub fn halted(&self) -> Option<HaltReason> {
        self.halt
    }

    /// An order known to the gate.
    #[must_use]
    pub fn order(&self, id: &OrderId) -> Option<&OrderRecord> {
        self.orders.get(id)
    }

    /// Orders that may still fill, ordered by id.
    pub fn live_orders(&self) -> impl Iterator<Item = &OrderRecord> {
        self.orders.values().filter(|o| o.state.is_live())
    }

    // ----- submission ---------------------------------------------------

    /// Decides on an order at time `now_ms` and, if approved, reserves for
    /// it and records it as [`OrderState::PendingNew`].
    ///
    /// The decision is recorded under the order id, including rejections,
    /// so a repeated id is always rejected with
    /// [`RiskReason::DuplicateOrderId`] and never reserves twice.
    pub fn submit(&mut self, request: &OrderRequest, now_ms: u64) -> RiskDecision {
        if self.orders.contains_key(&request.id) {
            return RiskDecision::Rejected {
                reasons: vec![RiskReason::DuplicateOrderId],
            };
        }
        let decision = self.decide(request, now_ms);
        let decision = match decision.approved_quantity() {
            Some(quantity) => match self.reserve_for(request, quantity) {
                Ok(()) => decision,
                // Unreachable by construction (sizing used the same
                // numbers); reported rather than hidden.
                Err(error) => RiskDecision::Rejected {
                    reasons: vec![RiskReason::InvalidOrder { error }],
                },
            },
            None => decision,
        };
        let (quantity, state) = match decision.approved_quantity() {
            Some(quantity) => (quantity, OrderState::PendingNew),
            None => (Decimal::ZERO, OrderState::Rejected),
        };
        self.orders.insert(
            request.id.clone(),
            OrderRecord {
                id: request.id.clone(),
                instrument: request.instrument.clone(),
                side: request.side,
                price: request.price,
                quantity,
                filled: Decimal::ZERO,
                state,
                decision: decision.clone(),
            },
        );
        decision
    }

    fn decide(&self, request: &OrderRequest, now_ms: u64) -> RiskDecision {
        let mut reasons = Vec::new();
        if let Some(reason) = self.halt {
            reasons.push(RiskReason::Halted { reason });
        }
        let Ok(spot) = self.spot(&request.instrument) else {
            reasons.push(RiskReason::UnknownInstrument);
            return RiskDecision::Rejected { reasons };
        };
        if let Err(error) = validate_order(request, &spot) {
            reasons.push(RiskReason::InvalidOrder { error });
            return RiskDecision::Rejected { reasons };
        }
        let limits = self.limits(&request.instrument);
        self.check_market_data(request, &limits, now_ms, &mut reasons);
        if let Some(limit) = limits.max_open_orders {
            let live = self
                .live_orders()
                .filter(|o| o.instrument == request.instrument)
                .count();
            if live >= limit {
                reasons.push(RiskReason::MaxOpenOrders { limit });
            }
        }
        if !reasons.is_empty() {
            return RiskDecision::Rejected { reasons };
        }

        match self.size(request, &spot, &limits) {
            Ok((allowed, mut caps)) => {
                let minimum = limits
                    .min_quantity
                    .unwrap_or_else(|| Decimal::new(1, spot.quantity_scale()));
                if allowed < minimum {
                    caps.push(RiskReason::BelowMinimumQuantity { minimum, allowed });
                    return RiskDecision::Rejected { reasons: caps };
                }
                if allowed == request.quantity {
                    RiskDecision::Accepted { quantity: allowed }
                } else if request.allow_reduce {
                    RiskDecision::Reduced {
                        requested: request.quantity,
                        quantity: allowed,
                        reasons: caps,
                    }
                } else {
                    RiskDecision::Rejected { reasons: caps }
                }
            }
            Err(error) => RiskDecision::Rejected {
                reasons: vec![RiskReason::InvalidOrder { error }],
            },
        }
    }

    fn check_market_data(
        &self,
        request: &OrderRequest,
        limits: &InstrumentLimits,
        now_ms: u64,
        reasons: &mut Vec<RiskReason>,
    ) {
        if limits.max_data_age_ms.is_none() && limits.price_band.is_none() {
            return;
        }
        let Some((mark, at)) = self.marks.get(&request.instrument).copied() else {
            reasons.push(RiskReason::NoMarketData);
            return;
        };
        if let Some(max_age_ms) = limits.max_data_age_ms {
            let age_ms = now_ms.saturating_sub(at);
            if age_ms > max_age_ms {
                reasons.push(RiskReason::StaleMarketData { age_ms, max_age_ms });
            }
        }
        if let Some(band) = limits.price_band {
            // |price - mark| > band * mark, decided exactly: the distance
            // lies on the grid of its own scale, and for a value on that
            // grid `x > w` holds exactly when `x > floor(w)` at that scale.
            let outside = sub(request.price, mark, "price - mark")
                .and_then(|distance| {
                    mul_div_round(
                        &[band, mark],
                        &[],
                        distance.scale(),
                        Rounding::TowardZero,
                        "band * mark",
                    )
                    .map(|width| distance.abs() > width)
                })
                // An overflow cannot be decided: treat it as outside.
                .unwrap_or(true);
            if outside {
                reasons.push(RiskReason::PriceOutsideBand { mark, band });
            }
        }
    }

    /// Applies every quantity cap; returns the allowed quantity and the
    /// caps that bound it.
    fn size(
        &self,
        request: &OrderRequest,
        spot: &SpotInstrument,
        limits: &InstrumentLimits,
    ) -> Result<(Decimal, Vec<RiskReason>)> {
        let lot_scale = spot.quantity_scale();
        let floor = |value: Decimal| {
            mul_div_round(
                &[value.max(Decimal::ZERO)],
                &[],
                lot_scale,
                Rounding::TowardZero,
                "floor to lot",
            )
        };
        let mut allowed = request.quantity;
        let mut caps = Vec::new();

        if let Some(limit) = limits.max_order_quantity {
            if allowed > limit {
                allowed = floor(limit)?;
                caps.push(RiskReason::MaxOrderQuantity { limit });
            }
        }

        let position = self
            .ledger
            .position(&request.instrument)
            .map(|p| p.quantity)
            .unwrap_or_default();
        let (open_buys, open_sells) = self.open_quantities(&request.instrument)?;
        // Largest quantity this order can add before the worst case position
        // on its side reaches `bound` in absolute value.
        let headroom = |bound: Decimal| -> Result<Decimal> {
            let used = match request.side {
                Side::Buy => add(position, open_buys, "position + open buys")?,
                Side::Sell => sub(open_sells, position, "open sells - position")?,
            };
            floor(sub(bound, used, "limit - worst case")?)
        };
        if let Some(limit) = limits.max_position {
            let room = headroom(limit)?;
            if allowed > room {
                allowed = room;
                caps.push(RiskReason::MaxPosition {
                    limit,
                    headroom: room,
                });
            }
        }
        if let Some(limit) = limits.max_position_notional {
            let bound = mul_div_round(
                &[limit],
                &[request.price],
                lot_scale,
                Rounding::TowardZero,
                "notional / price",
            )?;
            let room = headroom(bound)?;
            if allowed > room {
                allowed = room;
                caps.push(RiskReason::MaxPositionNotional {
                    limit,
                    headroom: room,
                });
            }
        }

        let (asset, required) =
            self.reservation(spot, limits, request.side, request.price, allowed)?;
        let available = self
            .ledger
            .balance(&asset)
            .map(|b| b.available)
            .unwrap_or_default();
        if required > available {
            let affordable = self.affordable(spot, limits, request, available)?;
            caps.push(RiskReason::InsufficientBalance {
                asset,
                required,
                available,
            });
            allowed = allowed.min(affordable);
        }
        Ok((allowed, caps))
    }

    /// Largest quantity whose reservation fits in `available`.
    ///
    /// `available` is a balance, so it lies on the asset's grid. For such a
    /// value, rounding a buy reservation up to the grid stays within it
    /// exactly when the unrounded amount does, so the answer is the exact
    /// ratio floored to the lot: `floor(available / (price * (1 + fee)))`.
    fn affordable(
        &self,
        spot: &SpotInstrument,
        limits: &InstrumentLimits,
        request: &OrderRequest,
        available: Decimal,
    ) -> Result<Decimal> {
        let lot_scale = spot.quantity_scale();
        let available = available.max(Decimal::ZERO);
        match request.side {
            Side::Sell => mul_div_round(
                &[available],
                &[],
                lot_scale,
                Rounding::TowardZero,
                "floor to lot",
            ),
            Side::Buy => mul_div_round(
                &[available],
                &[
                    request.price,
                    add(Decimal::ONE, limits.max_fee_rate, "1 + fee rate")?,
                ],
                lot_scale,
                Rounding::TowardZero,
                "available / unit cost",
            ),
        }
    }

    /// Asset and amount reserved for `quantity` of an order.
    fn reservation(
        &self,
        spot: &SpotInstrument,
        limits: &InstrumentLimits,
        side: Side,
        price: Decimal,
        quantity: Decimal,
    ) -> Result<(Asset, Decimal)> {
        match side {
            Side::Sell => Ok((spot.base().clone(), quantity)),
            Side::Buy => {
                let scale = self.ledger.asset_scale(spot.quote()).ok_or_else(|| {
                    MarginError::UnknownAsset {
                        asset: spot.quote().clone(),
                    }
                })?;
                let amount = mul_div_round(
                    &[
                        price,
                        quantity,
                        add(Decimal::ONE, limits.max_fee_rate, "1 + fee rate")?,
                    ],
                    &[],
                    scale,
                    Rounding::AwayFromZero,
                    "price * quantity * (1 + fee rate)",
                )?;
                Ok((spot.quote().clone(), amount))
            }
        }
    }

    fn reserve_for(&mut self, request: &OrderRequest, quantity: Decimal) -> Result<()> {
        let spot = self.spot(&request.instrument)?;
        let limits = self.limits(&request.instrument);
        let (asset, amount) =
            self.reservation(&spot, &limits, request.side, request.price, quantity)?;
        self.ledger
            .reserve(ReservationId::new(request.id.as_str())?, asset, amount)?;
        Ok(())
    }

    fn open_quantities(&self, instrument: &InstrumentId) -> Result<(Decimal, Decimal)> {
        let mut buys = Decimal::ZERO;
        let mut sells = Decimal::ZERO;
        for order in self.live_orders().filter(|o| &o.instrument == instrument) {
            match order.side {
                Side::Buy => buys = add(buys, order.remaining(), "open buys")?,
                Side::Sell => sells = add(sells, order.remaining(), "open sells")?,
            }
        }
        Ok((buys, sells))
    }

    fn spot(&self, instrument: &InstrumentId) -> Result<SpotInstrument> {
        match self.ledger.instrument(instrument) {
            Some(Instrument::Spot(spot)) => Ok(spot.clone()),
            None => Err(MarginError::UnknownInstrument {
                instrument: instrument.clone(),
            }),
        }
    }

    fn live_order(&self, id: &OrderId, operation: &'static str) -> Result<OrderRecord> {
        let order = self
            .orders
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        if !order.state.is_live() {
            return Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation,
            });
        }
        Ok(order)
    }

    fn set_state(&mut self, id: &OrderId, state: OrderState) {
        if let Some(order) = self.orders.get_mut(id) {
            order.state = state;
        }
    }

    // ----- lifecycle ----------------------------------------------------

    /// The venue acknowledged the order. Idempotent while the order is live.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`] for
    /// an order the gate rejected.
    pub fn on_venue_accepted(&mut self, id: &OrderId) -> Result<()> {
        let order = self
            .orders
            .get(id)
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        match order.state {
            OrderState::PendingNew => {
                self.set_state(id, OrderState::Open);
                Ok(())
            }
            // A late or repeated ack changes nothing.
            OrderState::Open
            | OrderState::CancelPending
            | OrderState::Filled
            | OrderState::Canceled => Ok(()),
            OrderState::Rejected => Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation: "accept",
            }),
        }
    }

    /// The venue rejected the order: its reservation is released and
    /// returned. A repeated reject returns zero.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`] if
    /// the order already filled in part or ended otherwise.
    pub fn on_venue_rejected(&mut self, id: &OrderId) -> Result<Decimal> {
        let order = self
            .orders
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        if order.state == OrderState::Rejected {
            return Ok(Decimal::ZERO);
        }
        if !order.state.is_live() || !order.filled.is_zero() {
            return Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation: "reject",
            });
        }
        let (_, released) = self.ledger.release(&order.reservation_id()?)?;
        self.set_state(id, OrderState::Rejected);
        Ok(released)
    }

    /// Requests a cancel. The reservation stays until
    /// [`RiskGate::on_cancel_confirmed`]. Returns `false` if a cancel was
    /// already pending.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`] if
    /// the order is no longer live.
    pub fn request_cancel(&mut self, id: &OrderId) -> Result<bool> {
        let order = self.live_order(id, "cancel")?;
        if order.state == OrderState::CancelPending {
            return Ok(false);
        }
        self.set_state(id, OrderState::CancelPending);
        Ok(true)
    }

    /// The venue confirmed the cancel: what is left of the reservation is
    /// released and returned. Confirming a cancel for an order that already
    /// ended (for example filled first) returns zero and changes nothing.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`] for
    /// an order the gate rejected.
    pub fn on_cancel_confirmed(&mut self, id: &OrderId) -> Result<Decimal> {
        let order = self
            .orders
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        match order.state {
            OrderState::PendingNew | OrderState::Open | OrderState::CancelPending => {
                let (_, released) = self.ledger.release(&order.reservation_id()?)?;
                self.set_state(id, OrderState::Canceled);
                Ok(released)
            }
            OrderState::Filled | OrderState::Canceled => Ok(Decimal::ZERO),
            OrderState::Rejected => Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation: "confirm cancel",
            }),
        }
    }

    /// Books a fill of order `id` and shrinks its reservation to what the
    /// remaining quantity needs, atomically.
    ///
    /// A repeated execution (same [`ExecutionId`](crate::ExecutionId) and content) returns
    /// [`Booking::Duplicate`] and changes nothing.
    ///
    /// # Errors
    ///
    /// Nothing changes on error.
    ///
    /// - [`MarginError::UnknownOrder`] or [`MarginError::InvalidOrderState`].
    /// - [`MarginError::ExecutionMismatch`] if the execution's instrument or
    ///   side differs from the order, its quantity exceeds the remaining
    ///   quantity, or its price is worse than the limit.
    /// - Any [`Ledger::book_execution_with_reservation`] error. On
    ///   [`MarginError::InsufficientBalance`] the gate also trips with
    ///   [`HaltReason::AccountingBreach`]: a fill happened that the
    ///   reservations did not cover.
    pub fn on_fill(&mut self, id: &OrderId, execution: &Execution) -> Result<Booking> {
        if self.ledger.execution_cashflow(&execution.id).is_some() {
            // Duplicate (or conflicting) message: the ledger decides, and
            // neither the order nor its reservation changes.
            return self.ledger.book_execution(execution);
        }
        let order = self.live_order(id, "fill")?;
        let mismatch = |reason: &'static str| MarginError::ExecutionMismatch {
            id: id.to_string(),
            reason,
        };
        if execution.instrument != order.instrument {
            return Err(mismatch("instrument differs from the order"));
        }
        if execution.side != order.side {
            return Err(mismatch("side differs from the order"));
        }
        if execution.quantity > order.remaining() {
            return Err(mismatch("quantity exceeds the remaining quantity"));
        }
        let worse = match order.side {
            Side::Buy => execution.price > order.price,
            Side::Sell => execution.price < order.price,
        };
        if worse {
            return Err(mismatch("price is worse than the limit"));
        }

        let spot = self.spot(&order.instrument)?;
        let limits = self.limits(&order.instrument);
        let remaining_after = sub(order.remaining(), execution.quantity, "remaining - fill")?;
        let (_, reserved_after) =
            self.reservation(&spot, &limits, order.side, order.price, remaining_after)?;
        let booking = match self.ledger.book_execution_with_reservation(
            execution,
            &order.reservation_id()?,
            reserved_after,
        ) {
            Ok(booking) => booking,
            Err(error @ MarginError::InsufficientBalance { .. }) => {
                self.halt.get_or_insert(HaltReason::AccountingBreach);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if let Some(record) = self.orders.get_mut(id) {
            // remaining_after >= 0 was checked above.
            record.filled = add(record.filled, execution.quantity, "filled + fill")?;
            if remaining_after.is_zero() {
                record.state = OrderState::Filled;
            }
        }
        Ok(booking)
    }

    // ----- circuit breaker ----------------------------------------------

    /// Halts trading and withdraws quotes.
    ///
    /// Returns the live orders that had no cancel pending; they are now
    /// [`OrderState::CancelPending`] and the caller must send their cancels.
    /// Calling it again while halted returns no orders and keeps the first
    /// reason.
    pub fn trip(&mut self, reason: HaltReason) -> Vec<OrderId> {
        self.halt.get_or_insert(reason);
        let to_cancel: Vec<OrderId> = self
            .orders
            .values()
            .filter(|o| matches!(o.state, OrderState::PendingNew | OrderState::Open))
            .map(|o| o.id.clone())
            .collect();
        for id in &to_cancel {
            self.set_state(id, OrderState::CancelPending);
        }
        to_cancel
    }

    /// Lifts the halt.
    pub fn resume(&mut self) {
        self.halt = None;
    }
}

fn validate_order(request: &OrderRequest, spot: &SpotInstrument) -> Result<()> {
    if request.price <= Decimal::ZERO {
        return Err(MarginError::invalid_input("price", "must be positive"));
    }
    if request.quantity <= Decimal::ZERO {
        return Err(MarginError::invalid_input("quantity", "must be positive"));
    }
    check_scale(request.price, spot.price_scale(), "price")?;
    check_scale(request.quantity, spot.quantity_scale(), "quantity")?;
    // The reservation id is the order id; validate it here so reserving
    // cannot fail later.
    ReservationId::new(request.id.as_str())?;
    Ok(())
}
