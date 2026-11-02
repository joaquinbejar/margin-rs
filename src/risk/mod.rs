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
//!   message adjusts it exactly once; fills carry the order's cumulative
//!   quantity ([`FillReport`]) so late and impossible reports are told
//!   apart;
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
//! use margin_rs::risk::{FillReport, InstrumentLimits, OrderRequest, RiskDecision, RiskGate};
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
//! gate.on_fill(&order.id, &FillReport::new(fill, Decimal::from(5)))?;
//! assert_eq!(gate.ledger().balance(&usdc).map(|b| (b.total, b.reserved)), Some((Decimal::from(500), Decimal::ZERO)));
//! # Ok(())
//! # }
//! ```
//!
//! # Reservation size
//!
//! Fixed per order at admission ([`ReservationTerms`]): a whole number of
//! lots times a per lot bound. For buys the bound covers the quote debited
//! by any fragmentation of the order at the limit price or better,
//! including the per fill rounding of the notional and a fee of up to
//! `max_fee_rate` rounded up; for sells it is the base quantity itself.
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
//! [`HaltReason::AccountingBreach`] when a real fill overdraws the account
//! and with [`HaltReason::ExecutionAnomaly`] when a real fill does not match
//! its order; in both cases the fill is booked, never lost, and the quotes
//! are withdrawn exactly as by [`RiskGate::trip`]: the orders to cancel are
//! returned in [`FillOutcome::withdraw`].

mod decision;

use std::collections::BTreeMap;

use rust_decimal::Decimal;

pub use decision::{
    ExecutionAnomaly, FillReport, HaltReason, OrderPurpose, OrderRequest, RiskDecision, RiskReason,
};

use crate::decimal::{Rounding, add, check_scale, mul, mul_div_round, sub, sum, sum_products};
use crate::error::{MarginError, Result};
use crate::ids::{Asset, InstrumentId, OrderId, ReservationId, TransferId};
use crate::instrument::{Instrument, SpotInstrument};
use crate::ledger::{Booking, Ledger, Side};

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
    /// Highest fee rate the venue may charge on a buy, as a fraction (0.001
    /// is 10 bp). Zero by default. The gate assumes that the fee of a fill is
    /// at most `max_fee_rate` times the fill's booked notional, rounded up to
    /// the quote grid, and charged in the quote asset. Fixed per order at
    /// admission: later changes do not affect live orders.
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

/// How an order's reservation is computed. Fixed when the order is
/// admitted, so later limit changes never release budget that a live order
/// may still need.
///
/// The reservation of `remaining` is `round_up(remaining / lot * per_lot)`
/// on the asset grid:
///
/// - sell: `per_lot = lot` of the base asset, so the reservation is the
///   remaining quantity itself;
/// - buy: `per_lot` bounds the quote debited per lot by any fill at the
///   limit price or better, whatever the fragmentation. It is the smaller
///   of two valid bounds, with `x = price * lot` and `r = max_fee_rate`:
///   - `round_up(x) + round_up(r * round_up(x))`, on the grid (rounding up
///     is subadditive);
///   - `(1 + r) * (x + h) + (ulp if r > 0)`, exact, where `h` is the largest
///     error of the ledger's notional rounding (half an ulp for the half
///     modes, 0 toward zero, one ulp away from zero).
///
/// A fill of `k` lots debits a grid amount of at most `k * per_lot`, and the
/// reservation falls by at least the grid floor of that, so each fill frees
/// at least what it debits and the reservation covers any sequence of
/// fills. When `x` is on the grid (for example 100 USDC at a 1e-8 lot) the
/// first bound is exact and nothing is over reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationTerms {
    /// Asset reserved: quote for buys, base for sells.
    pub asset: Asset,
    /// Scale of `asset`.
    pub scale: u32,
    /// Lot of the instrument.
    pub lot: Decimal,
    /// Amount reserved per open lot.
    pub per_lot: Decimal,
}

impl ReservationTerms {
    /// Reservation for `remaining` open quantity (a multiple of the lot),
    /// rounded up to the asset grid.
    ///
    /// # Errors
    ///
    /// [`MarginError::Arithmetic`] if the amount cannot be represented.
    pub fn amount(&self, remaining: Decimal) -> Result<Decimal> {
        mul_div_round(
            &[remaining, self.per_lot],
            &[self.lot],
            self.scale,
            Rounding::AwayFromZero,
            "remaining lots * per lot",
        )
    }

    /// Largest multiple of the lot whose reservation fits in `available`.
    ///
    /// # Errors
    ///
    /// [`MarginError::Arithmetic`] if the quantity cannot be represented.
    pub fn affordable(&self, available: Decimal) -> Result<Decimal> {
        let lots = mul_div_round(
            &[available.max(Decimal::ZERO)],
            &[self.per_lot],
            0,
            Rounding::TowardZero,
            "available / per lot",
        )?;
        mul(lots, self.lot, "lots * lot")
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
    /// Reservation terms fixed at admission; `None` if the gate rejected
    /// the order.
    pub terms: Option<ReservationTerms>,
}

impl OrderRecord {
    /// Quantity still open: zero once filled, and never negative, even
    /// after an overfill (where `filled > quantity`).
    #[must_use]
    pub fn remaining(&self) -> Decimal {
        // Both are small exact quantities, so the difference cannot overflow.
        (self.quantity - self.filled).max(Decimal::ZERO)
    }

    fn reservation_id(&self) -> Result<ReservationId> {
        ReservationId::new(self.id.as_str())
    }
}

/// Result of [`RiskGate::on_fill`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillOutcome {
    /// The booking, new or duplicate.
    pub booking: Booking,
    /// What was wrong with the fill, if anything; the gate halted.
    pub anomalies: Vec<ExecutionAnomaly>,
    /// Assets the fill overdrew, with the shortfall; the gate halted.
    pub shortfalls: BTreeMap<Asset, Decimal>,
    /// Orders whose cancel the caller must send because this fill halted
    /// the gate: every live order that had no cancel pending, now
    /// [`OrderState::CancelPending`] (as returned by [`RiskGate::trip`]).
    /// Empty when the fill did not halt the gate, for a duplicate, and when
    /// every live order already had a cancel pending.
    pub withdraw: Vec<OrderId>,
}

impl FillOutcome {
    /// True for a new fill that matched its order and was covered.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.booking.is_new() && self.anomalies.is_empty() && self.shortfalls.is_empty()
    }
}

/// The pretrade risk gate. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct RiskGate {
    ledger: Ledger,
    limits: BTreeMap<InstrumentId, InstrumentLimits>,
    marks: BTreeMap<InstrumentId, (Decimal, u64)>,
    orders: BTreeMap<OrderId, OrderRecord>,
    halts: Vec<HaltReason>,
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
            halts: Vec::new(),
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

    /// Sets an external hold: a named ledger reservation of exactly
    /// `amount` of `asset`, replacing any previous amount under `id`
    /// (zero removes it). Used to make capital committed elsewhere, such as
    /// derivative margin
    /// ([`MarginReservationService`](crate::margin_reservations::MarginReservationService)),
    /// unavailable to spot orders. Atomic ([`Ledger::set_reservation`]): on
    /// error the previous hold is kept, and shrinking a hold always
    /// succeeds, even on an overdrawn balance.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if `id` is also an order id of this
    /// gate; [`MarginError::InsufficientBalance`] if an increase is not
    /// covered by the available balance plus the previous hold; the
    /// ledger's input errors.
    pub fn set_hold(&mut self, id: &ReservationId, asset: Asset, amount: Decimal) -> Result<()> {
        if OrderId::new(id.as_str()).is_ok_and(|order| self.orders.contains_key(&order)) {
            return Err(MarginError::invalid_input(
                "hold",
                "the id belongs to an order",
            ));
        }
        self.ledger.set_reservation(id, asset, amount)
    }

    /// Amount of an external hold set with [`RiskGate::set_hold`].
    #[must_use]
    pub fn hold(&self, id: &ReservationId) -> Decimal {
        self.ledger
            .reservation(id)
            .map_or(Decimal::ZERO, |(_, amount)| amount)
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

    /// Current halt, if any: the most severe reason recorded since the last
    /// [`RiskGate::resume`] (see [`HaltReason::severity`]); among equally
    /// severe reasons, the first one.
    #[must_use]
    pub fn halted(&self) -> Option<HaltReason> {
        let mut most: Option<&HaltReason> = None;
        for reason in &self.halts {
            if most.is_none_or(|m| reason.severity() > m.severity()) {
                most = Some(reason);
            }
        }
        most.cloned()
    }

    /// Every halt reason recorded since the last [`RiskGate::resume`], in
    /// the order they happened.
    #[must_use]
    pub fn halt_reasons(&self) -> &[HaltReason] {
        &self.halts
    }

    fn record_halt(&mut self, reason: HaltReason) {
        if !self.halts.contains(&reason) {
            self.halts.push(reason);
        }
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
        let mut terms = None;
        let decision = match decision.approved_quantity() {
            Some(quantity) => match self.reserve_for(request, quantity) {
                Ok(order_terms) => {
                    terms = Some(order_terms);
                    decision
                }
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
                terms,
            },
        );
        decision
    }

    fn decide(&self, request: &OrderRequest, now_ms: u64) -> RiskDecision {
        let mut reasons = Vec::new();
        if let Some(reason) = self.halted() {
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
        if at > now_ms {
            // A mark from the future cannot be used at `now_ms`.
            reasons.push(RiskReason::LookAheadMarketData {
                mark_ms: at,
                now_ms,
            });
            return;
        }
        if let Some(max_age_ms) = limits.max_data_age_ms {
            let age_ms = now_ms - at;
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
            // bound - worst case, in one exact step.
            let room = match request.side {
                Side::Buy => sum(&[bound, -position, -open_buys], "limit - worst case")?,
                Side::Sell => sum(&[bound, -open_sells, position], "limit - worst case")?,
            };
            floor(room)
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

        let terms = self.terms_for(spot, limits, request.side, request.price)?;
        let required = terms.amount(allowed)?;
        let available = self
            .ledger
            .balance(&terms.asset)
            .map(|b| b.available)
            .unwrap_or_default();
        if required > available {
            let affordable = terms.affordable(available)?;
            caps.push(RiskReason::InsufficientBalance {
                asset: terms.asset.clone(),
                required,
                available,
            });
            allowed = allowed.min(affordable);
        }
        Ok((allowed, caps))
    }

    /// Reservation terms for an order on `spot` under `limits`.
    fn terms_for(
        &self,
        spot: &SpotInstrument,
        limits: &InstrumentLimits,
        side: Side,
        price: Decimal,
    ) -> Result<ReservationTerms> {
        let lot = Decimal::new(1, spot.quantity_scale());
        let scale_of = |asset: &Asset| {
            self.ledger
                .asset_scale(asset)
                .ok_or_else(|| MarginError::UnknownAsset {
                    asset: asset.clone(),
                })
        };
        match side {
            Side::Sell => Ok(ReservationTerms {
                asset: spot.base().clone(),
                scale: scale_of(spot.base())?,
                lot,
                per_lot: lot,
            }),
            Side::Buy => {
                let scale = scale_of(spot.quote())?;
                let ulp = Decimal::new(1, scale);
                let rate = limits.max_fee_rate;
                let up = |value: Decimal, op| {
                    mul_div_round(&[value], &[], scale, Rounding::AwayFromZero, op)
                };
                // Cost of one lot at the limit price, exact.
                let lot_cost = mul(price, lot, "price * lot")?;
                // Bound 1, on the grid: round_up(lot cost) plus its fee
                // rounded up. Rounding up is subadditive, so k lots cost at
                // most k times this.
                let cash = up(lot_cost, "round up lot cost")?;
                let fee_part = if rate.is_zero() {
                    Decimal::ZERO
                } else {
                    mul_div_round(
                        &[rate, cash],
                        &[],
                        scale,
                        Rounding::AwayFromZero,
                        "round up lot fee",
                    )?
                };
                let on_grid = add(cash, fee_part, "lot cash + fee")?;
                // Bound 2, exact: (1 + rate) * (lot cost + h) + fee ulp,
                // where h is the largest error of the ledger's notional
                // rounding. A fill of k lots debits at most k times this.
                let half_ulp = Decimal::new(5, scale + 1);
                let h = match self.ledger.config().rounding {
                    Rounding::HalfEven | Rounding::HalfAwayFromZero => half_ulp,
                    Rounding::TowardZero => Decimal::ZERO,
                    Rounding::AwayFromZero => ulp,
                };
                // (1 + rate) * (lot cost + h) + fee ulp, in one exact step.
                let fee_ulp = if rate.is_zero() { Decimal::ZERO } else { ulp };
                let exact = add(Decimal::ONE, rate, "1 + rate").and_then(|one_plus| {
                    sum_products(
                        &[&[one_plus, lot_cost], &[one_plus, h], &[fee_ulp]],
                        "exact lot bound",
                    )
                });
                let per_lot = match exact {
                    Ok(bound) if bound < on_grid => bound,
                    _ => on_grid,
                };
                Ok(ReservationTerms {
                    asset: spot.quote().clone(),
                    scale,
                    lot,
                    per_lot,
                })
            }
        }
    }

    fn reserve_for(
        &mut self,
        request: &OrderRequest,
        quantity: Decimal,
    ) -> Result<ReservationTerms> {
        let spot = self.spot(&request.instrument)?;
        let limits = self.limits(&request.instrument);
        let terms = self.terms_for(&spot, &limits, request.side, request.price)?;
        let amount = terms.amount(quantity)?;
        self.ledger.reserve(
            ReservationId::new(request.id.as_str())?,
            terms.asset.clone(),
            amount,
        )?;
        Ok(terms)
    }

    fn open_quantities(&self, instrument: &InstrumentId) -> Result<(Decimal, Decimal)> {
        let mut buys = Vec::new();
        let mut sells = Vec::new();
        for order in self.live_orders().filter(|o| &o.instrument == instrument) {
            match order.side {
                Side::Buy => buys.push(order.remaining()),
                Side::Sell => sells.push(order.remaining()),
            }
        }
        Ok((sum(&buys, "open buys")?, sum(&sells, "open sells")?))
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

    /// Books a real fill of order `id`. A real execution is never lost.
    ///
    /// The report carries the order's cumulative filled quantity after the
    /// fill (as FIX `CumQty`). It must equal the quantity filled so far plus
    /// the fill: that orders one order's fills causally. Only a report that
    /// cannot be causally true is rejected; anything that did happen is
    /// booked:
    ///
    /// - Live order: the fill is booked and the reservation shrunk to what
    ///   the remaining quantity needs, atomically. By the
    ///   [`ReservationTerms`] bound the reservation frees at least the
    ///   debit; anything above it (a fee over `max_fee_rate`) comes from the
    ///   free balance.
    /// - Canceled or filled order (a late report): booked against the free
    ///   balance.
    /// - An overfill, a fill after the order was filled, or a price worse
    ///   than the limit is booked too, with only the reserved part released,
    ///   reported in [`FillOutcome::anomalies`], and the gate halts with
    ///   [`HaltReason::ExecutionAnomaly`].
    /// - If the free balance does not cover the debit, the fill is still
    ///   booked: the ledger records the shortfall (an overdraft, see
    ///   [`Ledger::overdrafts`]), it is reported in
    ///   [`FillOutcome::shortfalls`], and the gate halts with
    ///   [`HaltReason::AccountingBreach`].
    ///
    /// A repeated execution (same [`ExecutionId`](crate::ExecutionId) and
    /// content) returns a [`Booking::Duplicate`] outcome and changes
    /// nothing.
    ///
    /// # Errors
    ///
    /// Nothing changes on error. These are reports that cannot be true:
    ///
    /// - [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`]
    ///   for an order the gate rejected (it was never sent);
    /// - [`MarginError::ExecutionMismatch`] if the instrument or side differs
    ///   from the order, or the cumulative quantity is not the filled
    ///   quantity plus the fill;
    /// - [`MarginError::ConflictingEntry`] for a reused execution id with
    ///   different content, and the ledger's input errors (precision,
    ///   positivity).
    pub fn on_fill(&mut self, id: &OrderId, report: &FillReport) -> Result<FillOutcome> {
        let execution = &report.execution;
        if self.ledger.execution_cashflow(&execution.id).is_some() {
            // Duplicate (or conflicting) message: the ledger decides, and
            // neither the order nor its reservation changes.
            return Ok(FillOutcome {
                booking: self.ledger.book_execution(execution)?,
                anomalies: Vec::new(),
                shortfalls: BTreeMap::new(),
                withdraw: Vec::new(),
            });
        }
        let order = self
            .orders
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        if order.state == OrderState::Rejected {
            return Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation: "fill",
            });
        }
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
        let filled_after = add(order.filled, execution.quantity, "filled + fill")?;
        if report.cumulative_quantity != filled_after {
            return Err(mismatch(
                "cumulative quantity is not the filled quantity plus the fill",
            ));
        }

        let remaining = order.remaining();
        let mut anomalies = Vec::new();
        if order.state == OrderState::Filled {
            anomalies.push(ExecutionAnomaly::FillAfterFilled);
        } else if execution.quantity > remaining {
            anomalies.push(ExecutionAnomaly::Overfill {
                remaining,
                quantity: execution.quantity,
            });
        }
        let worse = match order.side {
            Side::Buy => execution.price > order.price,
            Side::Sell => execution.price < order.price,
        };
        if worse {
            anomalies.push(ExecutionAnomaly::WorsePrice {
                limit: order.price,
                price: execution.price,
            });
        }

        // Only a live order still holds a reservation; shrink it to what the
        // rest needs (nothing after an overfill).
        let remaining_after =
            sub(remaining, execution.quantity, "remaining - fill")?.max(Decimal::ZERO);
        let reservation_id = order.reservation_id()?;
        let reservation = match (&order.terms, order.state.is_live()) {
            (Some(terms), true) => Some(terms.amount(remaining_after)?),
            _ => None,
        };
        let booked = self.ledger.book_execution_allowing_overdraft(
            execution,
            reservation.map(|amount| (&reservation_id, amount)),
        )?;

        // Both are recorded when both happen; halted() reports the more
        // severe one (the accounting breach).
        if let Some(anomaly) = anomalies.first() {
            self.record_halt(HaltReason::ExecutionAnomaly {
                order: id.clone(),
                anomaly: anomaly.clone(),
            });
        }
        if !booked.shortfalls.is_empty() {
            self.record_halt(HaltReason::AccountingBreach);
        }
        if let Some(record) = self.orders.get_mut(id) {
            record.filled = filled_after;
            if filled_after >= record.quantity {
                record.state = OrderState::Filled;
            }
        }
        // An internal halt withdraws quotes like trip(), after the order's
        // own update, so a filled order is not cancelled and a partly filled
        // one is.
        let withdraw = if anomalies.is_empty() && booked.shortfalls.is_empty() {
            Vec::new()
        } else {
            self.withdraw_live()
        };
        Ok(FillOutcome {
            booking: booked.booking,
            anomalies,
            shortfalls: booked.shortfalls,
            withdraw,
        })
    }

    // ----- circuit breaker ----------------------------------------------

    /// Halts trading and withdraws quotes.
    ///
    /// Returns the live orders that had no cancel pending; they are now
    /// [`OrderState::CancelPending`] and the caller must send their cancels.
    /// Calling it again while halted returns no orders; the reason is added
    /// to [`RiskGate::halt_reasons`] and [`RiskGate::halted`] reports the
    /// most severe one.
    pub fn trip(&mut self, reason: HaltReason) -> Vec<OrderId> {
        self.record_halt(reason);
        self.withdraw_live()
    }

    /// Moves every live order without a cancel pending to cancel pending
    /// and returns them, in order id order. Reservations stay until each
    /// cancel is confirmed.
    fn withdraw_live(&mut self) -> Vec<OrderId> {
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

    /// Lifts the halt and clears every recorded reason.
    pub fn resume(&mut self) {
        self.halts.clear();
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
