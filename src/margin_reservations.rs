//! Initial margin reserved for pending derivative orders of one account.
//!
//! A [`MarginReservationService`] connects the order lifecycle (reserve,
//! fills, effective cancel) to one [`InitialMarginModel`], for one account
//! at one venue.
//!
//! # Requirement
//!
//! The requirement is recomputed from state, never accumulated, so it is
//! always the exact requirement of what is open now:
//!
//! - per instrument, the worst case position is the larger in absolute
//!   value of `position + open buys` and `position - open sells` (every
//!   live order of the side filling, which is what a margin call must
//!   survive);
//! - the account requirement is the rule's requirement of the worst case
//!   positions; the part due to pending orders is that minus the
//!   requirement of the current positions alone.
//!
//! Portfolio offsets are disabled: each instrument is margined on its own
//! worst case, exactly as the rule does (it nets nothing across
//! instruments, accounts or venues).
//!
//! # Available capital and double reservation
//!
//! [`MarginReservationService::reserve`] is a pure decision against the
//! `collateral` it is given; on its own it does not stop a spot order from
//! spending the same cash. Used with a [`RiskGate`], the account must go
//! through [`MarginReservationService::reserve_with_gate`] and
//! [`MarginReservationService::sync_hold`], which keep the whole margin
//! requirement as one external hold in the gate's ledger
//! ([`RiskGate::set_hold`]):
//!
//! - the margin check uses the settlement asset's available cash plus the
//!   current hold, so spot reservations already made are excluded;
//! - the accepted requirement becomes the hold, so later spot orders see
//!   it as reserved.
//!
//! Either order of admission is therefore safe: no unit of cash is held by
//! a spot reservation and by margin at the same time. Unknown collateral
//! rejects every new order.
//!
//! # Positions: own or read from the ledger
//!
//! A service built with [`MarginReservationService::new`] keeps its own
//! position per instrument, moved by [`MarginReservationService::apply_fill`].
//!
//! A service built with [`MarginReservationService::on_ledger`] keeps no
//! positions: every decision reads them from the ledger of the
//! [`RiskGate`] it is given, where the futures and perpetuals are
//! registered and their executions booked. In that mode:
//!
//! - the caller books each fill first
//!   ([`RiskGate::book_derivative_execution`], mandatory), then reports it
//!   with [`MarginReservationService::apply_booked_fill`], which takes the
//!   contracts from the booking, moves only the order's remaining quantity,
//!   flags an overfill ([`BookedFill::Applied`]) and sets the hold, all in
//!   one atomic step; a fill that is not booked yet is
//!   [`MarginError::FillNotBooked`] and changes nothing;
//! - the hold is the service's own [`ReservationId`], fixed at
//!   construction, and covers the whole account in the settlement asset:
//!   the requirement margins the registered instruments (with their
//!   orders) and every other open future or perpetual of the ledger
//!   settled in that asset, with the ledger's terms. Use one such service
//!   per account and settlement asset;
//! - collateral is `available + own hold + sum(upnl)` in the settlement
//!   asset, one exact sum, where the unrealized PnL covers **every** open
//!   future or perpetual in the ledger settled in that asset, the
//!   ones this service does not trade included: a loss on another
//!   instrument is never left out. Each needs a positive mark;
//! - every registered instrument must be a future or perpetual of the
//!   ledger with the same product, settlement asset and contract size;
//!   orders can only be placed on registered instruments;
//! - the operations of the other mode
//!   ([`MarginReservationService::reserve`],
//!   [`MarginReservationService::requirement`],
//!   [`MarginReservationService::apply_fill`]) are
//!   [`MarginError::WrongReservationMode`], and so is
//!   [`MarginReservationService::apply_booked_fill`] on a service with its
//!   own positions.
//!
//! # Lifecycle
//!
//! - [`MarginReservationService::reserve`] accepts an order if the
//!   requirement with it stays within collateral, or if it is risk
//!   reducing (opposite side to
//!   the position, size at most `|position|` minus the reducing orders
//!   already resting, so it can neither flip nor extend the position),
//!   which is accepted even when the account is under margined; otherwise
//!   it is rejected with the requirement, the collateral and the shortfall. Orders are
//!   decided one at a time against the requirement of everything already
//!   accepted, so two orders can never use the same capital.
//! - [`MarginReservationService::apply_fill`] (and
//!   [`MarginReservationService::apply_booked_fill`]) is idempotent by
//!   execution id: it moves the filled contracts from the order to the
//!   position (to the ledger's position, already booked).
//! - [`MarginReservationService::release_on_effective_cancel`] closes the
//!   order's remaining quantity; repeating it changes nothing.

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;

use crate::decimal::{MAX_SCALE, add, check_scale, neg, sub, sum};
use crate::direction::Dir;
use crate::error::{MarginError, Result};
use crate::ids::{AccountId, Asset, ExecutionId, InstrumentId, OrderId, ReservationId, VenueId};
use crate::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement, ProductKind, check_size,
};
use crate::instrument::{ContractSize, DerivativeContract, Instrument};
use crate::ledger::{Action, LedgerSnapshot};
use crate::risk::RiskGate;
use crate::valuation::net_with_upnl;

/// Contract terms of a margined instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginInstrument {
    /// Product kind.
    pub product: ProductKind,
    /// Settlement asset.
    pub settlement: Asset,
    /// Contract size: a multiplier for a linear product, a contract value
    /// for an inverse one.
    pub size: ContractSize,
}

/// A derivative order to margin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginOrder {
    /// Identifier, unique within the service.
    pub id: OrderId,
    /// Instrument.
    pub instrument: InstrumentId,
    /// Direction.
    pub side: Action,
    /// Contracts, positive.
    pub contracts: Decimal,
}

/// Why an order was not reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MarginRejection {
    /// The order id was already used.
    DuplicateOrderId,
    /// The instrument is not registered with the service.
    UnknownInstrument,
    /// Contracts are not positive.
    InvalidQuantity,
    /// An instrument with exposure has no mark.
    MissingMark {
        /// Instrument without a mark.
        instrument: InstrumentId,
    },
    /// Collateral is unknown (a debt without a trusted mark).
    CollateralUnknown,
    /// The requirement with this order exceeds collateral.
    InsufficientMargin {
        /// Account requirement if the order is accepted.
        required: Decimal,
        /// Collateral.
        collateral: Decimal,
        /// `required - collateral`.
        shortfall: Decimal,
    },
}

/// Decision on [`MarginReservationService::reserve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarginDecision {
    /// Accepted; `reserved` is the increase of the account requirement due
    /// to this order at the time it was accepted. It is accounting against
    /// the collateral passed in, not a ledger reservation (see
    /// [`MarginReservationService::reserve_with_gate`] for that).
    Reserved {
        /// Account requirement with the order.
        required: Decimal,
        /// Increase due to this order.
        reserved: Decimal,
    },
    /// Not accepted; nothing changed.
    Rejected {
        /// Why.
        reason: MarginRejection,
    },
}

/// Result of [`MarginReservationService::apply_booked_fill`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookedFill {
    /// The fill was already reported for this order: nothing changed.
    Duplicate,
    /// The fill was applied.
    Applied {
        /// Contracts of the booking.
        contracts: Decimal,
        /// Contracts of the order still live after the fill (zero when the
        /// fill closed it).
        remaining: Decimal,
        /// Contracts beyond what was remaining (all of them for a fill
        /// reported after the order closed): booked in the ledger and
        /// flagged here, never refused. `None` when the fill fit.
        overfill: Option<Decimal>,
        /// Account requirement after the fill.
        required: Decimal,
        /// The service's hold after the fill: the requirement, capped at
        /// the cash (`available + hold`, never negative). Below `required`
        /// when the cash does not cover it.
        hold: Decimal,
    },
}

/// Requirement of the account now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginState {
    /// Requirement of the current positions alone.
    pub positions: Decimal,
    /// Additional requirement of the live orders (worst case minus
    /// positions).
    pub pending: Decimal,
    /// `positions + pending`.
    pub total: Decimal,
    /// The rule's breakdown of the worst case.
    pub breakdown: MarginRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingOrder {
    order: MarginOrder,
    remaining: Decimal,
}

/// Where the positions come from.
#[derive(Debug, Clone)]
enum Mode {
    /// The service's own positions, moved by `apply_fill`.
    Own(BTreeMap<InstrumentId, Decimal>),
    /// The ledger of the gate, with this hold.
    OnLedger(ReservationId),
}

impl Mode {
    fn name(&self) -> &'static str {
        match self {
            Self::Own(_) => "own positions",
            Self::OnLedger(_) => "on ledger",
        }
    }
}

/// Margin reservations of one account at one venue. See the
/// [module docs](self).
#[derive(Debug, Clone)]
pub struct MarginReservationService<M> {
    model: M,
    account: AccountId,
    venue: VenueId,
    instruments: BTreeMap<InstrumentId, MarginInstrument>,
    mode: Mode,
    orders: BTreeMap<OrderId, PendingOrder>,
    closed: BTreeMap<OrderId, MarginOrder>,
    seen_orders: BTreeSet<OrderId>,
    fills: BTreeMap<ExecutionId, (OrderId, Decimal)>,
}

impl<M: InitialMarginModel> MarginReservationService<M> {
    /// A service with its own positions and no instruments, positions or
    /// orders.
    pub fn new(model: M, account: AccountId, venue: VenueId) -> Self {
        Self::with_mode(model, account, venue, Mode::Own(BTreeMap::new()))
    }

    /// A service that reads its positions from the ledger of the
    /// [`RiskGate`] passed to each call and keeps its whole requirement as
    /// the hold `hold` there. See
    /// [positions](self#positions-own-or-read-from-the-ledger).
    pub fn on_ledger(model: M, account: AccountId, venue: VenueId, hold: ReservationId) -> Self {
        Self::with_mode(model, account, venue, Mode::OnLedger(hold))
    }

    fn with_mode(model: M, account: AccountId, venue: VenueId, mode: Mode) -> Self {
        Self {
            model,
            account,
            venue,
            instruments: BTreeMap::new(),
            mode,
            orders: BTreeMap::new(),
            closed: BTreeMap::new(),
            seen_orders: BTreeSet::new(),
            fills: BTreeMap::new(),
        }
    }

    /// The service's own positions, or [`MarginError::WrongReservationMode`].
    fn own_positions(&self, operation: &'static str) -> Result<&BTreeMap<InstrumentId, Decimal>> {
        match &self.mode {
            Mode::Own(positions) => Ok(positions),
            Mode::OnLedger(_) => Err(self.wrong_mode(operation)),
        }
    }

    fn wrong_mode(&self, operation: &'static str) -> MarginError {
        MarginError::WrongReservationMode {
            operation,
            mode: self.mode.name(),
        }
    }

    /// The hold to use with `gate`: the one passed in for a service with
    /// its own positions; for one on the ledger, its own hold, which `hold`
    /// must name.
    fn hold_id<'a>(&'a self, hold: &'a ReservationId) -> Result<&'a ReservationId> {
        match &self.mode {
            Mode::Own(_) => Ok(hold),
            Mode::OnLedger(own) if own == hold => Ok(own),
            Mode::OnLedger(_) => Err(MarginError::invalid_input(
                "hold",
                "is not the hold of this service",
            )),
        }
    }

    /// The instruments the requirement covers, with their terms and
    /// current positions.
    ///
    /// - Own positions: the registered instruments.
    /// - On the ledger: the registered instruments, each checked against
    ///   the ledger's definition, and every other open future or
    ///   perpetual of the ledger settled in the service's asset, with the
    ///   ledger's terms: the hold covers the whole account in that asset.
    fn book_in(&self, snapshot: &LedgerSnapshot) -> Result<Book> {
        if let Mode::Own(_) = self.mode {
            return Ok(self.own_book());
        }
        let quantity = |id: &InstrumentId| {
            snapshot
                .positions
                .get(id)
                .map_or(Decimal::ZERO, |p| p.quantity)
        };
        let mut book = Book::new();
        for (id, registered) in &self.instruments {
            let instrument =
                snapshot
                    .instruments
                    .get(id)
                    .ok_or_else(|| MarginError::UnknownInstrument {
                        instrument: id.clone(),
                    })?;
            let Some((product, terms)) = derivative_terms(instrument) else {
                return Err(MarginError::invalid_input(
                    "instrument",
                    "is not a future or perpetual in the ledger",
                ));
            };
            if product != registered.product {
                return Err(MarginError::invalid_input(
                    "product",
                    "differs from the ledger instrument",
                ));
            }
            if terms.settlement != registered.settlement {
                return Err(MarginError::CurrencyMismatch {
                    field: "settlement",
                    expected: terms.settlement.clone(),
                    found: registered.settlement.clone(),
                });
            }
            if terms.size != registered.size {
                return Err(MarginError::invalid_input(
                    "size",
                    "differs from the ledger instrument",
                ));
            }
            book.insert(id.clone(), (registered.clone(), quantity(id)));
        }
        let currency = self.settlement()?;
        for (id, position) in &snapshot.positions {
            if position.quantity.is_zero() || book.contains_key(id) {
                continue;
            }
            let instrument =
                snapshot
                    .instruments
                    .get(id)
                    .ok_or_else(|| MarginError::UnknownInstrument {
                        instrument: id.clone(),
                    })?;
            let Some((product, terms)) = derivative_terms(instrument) else {
                continue;
            };
            if terms.settlement != currency {
                continue;
            }
            let margined = MarginInstrument {
                product,
                settlement: currency.clone(),
                size: terms.size,
            };
            book.insert(id.clone(), (margined, position.quantity));
        }
        Ok(book)
    }

    /// Registers an instrument the account may trade.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] for a size that is not positive or not
    /// of the product's kind (a multiplier for a linear product, a contract
    /// value for an inverse one).
    pub fn register(&mut self, id: InstrumentId, instrument: MarginInstrument) -> Result<()> {
        check_size(instrument.product, instrument.size)?;
        if let Mode::Own(positions) = &mut self.mode {
            positions.entry(id.clone()).or_default();
        }
        self.instruments.insert(id, instrument);
        Ok(())
    }

    /// Current signed position in contracts of a service with its own
    /// positions; `None` for an unregistered instrument and always `None`
    /// on the ledger (read the ledger's position instead).
    #[must_use]
    pub fn position(&self, instrument: &InstrumentId) -> Option<Decimal> {
        match &self.mode {
            Mode::Own(positions) => positions.get(instrument).copied(),
            Mode::OnLedger(_) => None,
        }
    }

    /// Remaining contracts of a live order.
    #[must_use]
    pub fn remaining(&self, order: &OrderId) -> Option<Decimal> {
        self.orders.get(order).map(|o| o.remaining)
    }

    /// Requirement of positions and live orders at `marks`, for a service
    /// with its own positions.
    ///
    /// # Errors
    ///
    /// [`MarginError::MissingMark`] if an instrument with exposure has no
    /// mark, or any error of the margin rule;
    /// [`MarginError::WrongReservationMode`] on the ledger (use
    /// [`MarginReservationService::requirement_with_gate`]).
    pub fn requirement(&self, marks: &BTreeMap<InstrumentId, Decimal>) -> Result<MarginState> {
        self.own_positions("requirement")?;
        let book = self.own_book();
        self.state(&self.orders, &book, marks)
    }

    /// Requirement of positions and live orders at `marks`, with the
    /// positions read from `gate`'s ledger on the ledger (the service's own
    /// otherwise).
    ///
    /// # Errors
    ///
    /// As [`MarginReservationService::requirement`], plus, on the ledger,
    /// [`MarginError::UnknownInstrument`], [`MarginError::InvalidInput`] or
    /// [`MarginError::CurrencyMismatch`] for a registered instrument that
    /// differs from the ledger's, and [`MarginError::MissingMark`] for any
    /// open derivative in the settlement asset without a mark, registered
    /// or not.
    pub fn requirement_with_gate(
        &self,
        gate: &RiskGate,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<MarginState> {
        let book = self.book_in(&gate.ledger().snapshot())?;
        self.state(&self.orders, &book, marks)
    }

    /// The book of a service with its own positions.
    fn own_book(&self) -> Book {
        let positions = match &self.mode {
            Mode::Own(positions) => Some(positions),
            Mode::OnLedger(_) => None,
        };
        self.instruments
            .iter()
            .map(|(id, terms)| {
                let position = positions
                    .and_then(|p| p.get(id).copied())
                    .unwrap_or_default();
                (id.clone(), (terms.clone(), position))
            })
            .collect()
    }

    fn state(
        &self,
        orders: &BTreeMap<OrderId, PendingOrder>,
        book: &Book,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<MarginState> {
        let worst = self.view(orders, book, marks, true)?;
        let current = self.view(orders, book, marks, false)?;
        let worst = self.model.requirement(&worst)?;
        let current = self.model.requirement(&current)?;
        let pending = sub(worst.initial, current.initial, "worst - positions")?;
        Ok(MarginState {
            positions: current.initial,
            pending,
            total: worst.initial,
            breakdown: worst,
        })
    }

    /// Exposure view at the worst case (`with_orders`) or of positions only.
    fn view(
        &self,
        orders: &BTreeMap<OrderId, PendingOrder>,
        book: &Book,
        marks: &BTreeMap<InstrumentId, Decimal>,
        with_orders: bool,
    ) -> Result<ExposureView> {
        let mut buys: BTreeMap<&InstrumentId, Vec<Decimal>> = BTreeMap::new();
        let mut sells: BTreeMap<&InstrumentId, Vec<Decimal>> = BTreeMap::new();
        if with_orders {
            for pending in orders.values() {
                let side = match Dir::from_action(pending.order.side, "pending order side")? {
                    Dir::Buy => &mut buys,
                    Dir::Sell => &mut sells,
                };
                side.entry(&pending.order.instrument)
                    .or_default()
                    .push(pending.remaining);
            }
        }
        let mut positions = Vec::new();
        for (id, (instrument, position)) in book {
            let position = *position;
            let open_buys = sum(buys.get(id).map_or(&[][..], Vec::as_slice), "open buys")?;
            let open_sells = sum(sells.get(id).map_or(&[][..], Vec::as_slice), "open sells")?;
            let long = add(position, open_buys, "position + buys")?;
            let short = sub(position, open_sells, "position - sells")?;
            let worst = if long.abs() >= short.abs() {
                long
            } else {
                short
            };
            if worst.is_zero() {
                continue;
            }
            let mark = *marks.get(id).ok_or_else(|| MarginError::MissingMark {
                instrument: id.clone(),
            })?;
            positions.push(ExposurePosition {
                instrument: id.clone(),
                product: instrument.product,
                settlement: instrument.settlement.clone(),
                contracts: worst,
                size: instrument.size,
                mark,
            });
        }
        Ok(ExposureView {
            account: self.account.clone(),
            venue: self.venue.clone(),
            positions,
        })
    }

    /// Reserves margin for `order` if the account requirement with it stays
    /// within `collateral`. See the [module docs](self).
    ///
    /// The id is recorded even when the order is rejected, so a repeated id
    /// is always rejected.
    ///
    /// # Errors
    ///
    /// Errors of the margin rule (for example an unsupported product);
    /// policy outcomes are [`MarginDecision::Rejected`].
    /// [`MarginError::UnsupportedAction`] for an order side that is not
    /// `Buy` or `Sell`, before the order id is used.
    /// [`MarginError::WrongReservationMode`] on the ledger (use
    /// [`MarginReservationService::reserve_with_gate`]).
    pub fn reserve(
        &mut self,
        order: MarginOrder,
        collateral: Option<Decimal>,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<MarginDecision> {
        self.own_positions("reserve")?;
        // Refused before the order id is used and before any change.
        Dir::from_action(order.side, "order side")?;
        let book = self.own_book();
        let (decision, candidate) = self.decide(&order, collateral, &book, marks)?;
        self.seen_orders.insert(order.id);
        if let Some(orders) = candidate {
            self.orders = orders;
        }
        Ok(decision)
    }

    /// Risk reducing: the order is on the side opposite to the
    /// position, and its size is at most `|position|` minus the live
    /// reducing orders already resting on that instrument, so no
    /// combination of fills can flip or extend the position.
    fn is_risk_reducing(&self, order: &MarginOrder, book: &Book) -> Result<bool> {
        let position = book
            .get(&order.instrument)
            .map_or(Decimal::ZERO, |(_, position)| *position);
        let reducing_side = match position.cmp(&Decimal::ZERO) {
            core::cmp::Ordering::Greater => Action::Sell,
            core::cmp::Ordering::Less => Action::Buy,
            core::cmp::Ordering::Equal => return Ok(false),
        };
        if order.side != reducing_side {
            return Ok(false);
        }
        let resting: Vec<Decimal> = self
            .orders
            .values()
            .filter(|p| p.order.instrument == order.instrument && p.order.side == reducing_side)
            .map(|p| p.remaining)
            .collect();
        let room = sum(
            &[position.abs(), neg(sum(&resting, "resting reducing")?)],
            "reducing room",
        )?;
        Ok(order.contracts <= room)
    }

    /// The decision on `order` and, if accepted, the order book with it.
    /// Changes nothing.
    fn decide(
        &self,
        order: &MarginOrder,
        collateral: Option<Decimal>,
        book: &Book,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<(MarginDecision, Option<BTreeMap<OrderId, PendingOrder>>)> {
        let reject = |reason| Ok((MarginDecision::Rejected { reason }, None));
        if self.seen_orders.contains(&order.id) {
            return reject(MarginRejection::DuplicateOrderId);
        }
        if !self.instruments.contains_key(&order.instrument) {
            return reject(MarginRejection::UnknownInstrument);
        }
        if order.contracts <= Decimal::ZERO {
            return reject(MarginRejection::InvalidQuantity);
        }
        let Some(collateral) = collateral else {
            return reject(MarginRejection::CollateralUnknown);
        };
        let before = match self.state(&self.orders, book, marks) {
            Ok(state) => state,
            Err(MarginError::MissingMark { instrument }) => {
                return reject(MarginRejection::MissingMark { instrument });
            }
            Err(error) => return Err(error),
        };
        let mut with_order = self.orders.clone();
        with_order.insert(
            order.id.clone(),
            PendingOrder {
                remaining: order.contracts,
                order: order.clone(),
            },
        );
        let after = match self.state(&with_order, book, marks) {
            Ok(state) => state,
            Err(MarginError::MissingMark { instrument }) => {
                return reject(MarginRejection::MissingMark { instrument });
            }
            Err(error) => return Err(error),
        };
        // Risk reducing exception: accepted
        // whatever the collateral, even when under margined.
        if after.total > collateral && !self.is_risk_reducing(order, book)? {
            return reject(MarginRejection::InsufficientMargin {
                required: after.total,
                collateral,
                shortfall: sub(after.total, collateral, "required - collateral")?,
            });
        }
        let reserved = sub(after.total, before.total, "after - before")?;
        Ok((
            MarginDecision::Reserved {
                required: after.total,
                reserved,
            },
            Some(with_order),
        ))
    }

    /// Applies a fill of `contracts` to `order`, once per `execution`.
    /// Returns `false` for a repeated identical fill (nothing changes); the
    /// same execution id with another order or quantity is a
    /// [`MarginError::ConflictingEntry`] and changes nothing.
    ///
    /// A real execution is never lost: a fill larger than the remaining
    /// quantity, or one reported after the order was filled or its cancel
    /// took effect, is still applied to the position (and reserves
    /// nothing, since the order is closed).
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownOrder`] if the order was never reserved;
    /// [`MarginError::InvalidInput`] for non positive contracts;
    /// [`MarginError::WrongReservationMode`] on the ledger (use
    /// [`MarginReservationService::apply_booked_fill`]).
    pub fn apply_fill(
        &mut self,
        order: &OrderId,
        execution: ExecutionId,
        contracts: Decimal,
    ) -> Result<bool> {
        self.own_positions("apply_fill")?;
        if let Some((seen_order, seen_contracts)) = self.fills.get(&execution) {
            if seen_order == order && *seen_contracts == contracts {
                return Ok(false);
            }
            return Err(MarginError::ConflictingEntry {
                kind: "fill",
                id: execution.to_string(),
            });
        }
        if contracts <= Decimal::ZERO {
            return Err(MarginError::invalid_input("contracts", "must be positive"));
        }
        let pending = self.pending(order)?;
        let signed = match Dir::from_action(pending.order.side, "pending order side")? {
            Dir::Buy => contracts,
            Dir::Sell => neg(contracts),
        };
        let positions = self.own_positions("apply_fill")?;
        let position = positions
            .get(&pending.order.instrument)
            .copied()
            .unwrap_or_default();
        let new_position = add(position, signed, "position + fill")?;
        let remaining = sub(pending.remaining, contracts, "remaining - fill")?.max(Decimal::ZERO);
        if let Mode::Own(positions) = &mut self.mode {
            positions.insert(pending.order.instrument.clone(), new_position);
        }
        self.commit_fill(order, execution, contracts, remaining);
        Ok(true)
    }

    /// The order's remaining quantity after a fill, and the fill recorded.
    fn commit_fill(
        &mut self,
        order: &OrderId,
        execution: ExecutionId,
        contracts: Decimal,
        remaining: Decimal,
    ) {
        if remaining.is_zero() {
            if let Some(closed) = self.orders.remove(order) {
                self.closed.insert(order.clone(), closed.order);
            }
        } else if let Some(entry) = self.orders.get_mut(order) {
            entry.remaining = remaining;
        }
        self.fills.insert(execution, (order.clone(), contracts));
    }

    /// The live or closed order `order`.
    fn pending(&self, order: &OrderId) -> Result<PendingOrder> {
        match (self.orders.get(order), self.closed.get(order)) {
            (Some(live), _) => Ok(live.clone()),
            (None, Some(closed)) => Ok(PendingOrder {
                order: closed.clone(),
                remaining: Decimal::ZERO,
            }),
            (None, None) => Err(MarginError::UnknownOrder {
                id: order.to_string(),
            }),
        }
    }

    /// Reports a fill of `order` already booked in `gate`'s ledger under
    /// `execution`, for a service on the ledger, and sets the service's hold
    /// to the new requirement. See
    /// [positions](self#positions-own-or-read-from-the-ledger).
    ///
    /// The contracts are those of the booking; the order's remaining
    /// quantity falls by them (the position is the ledger's). A fill larger
    /// than the remaining quantity, or reported after the order closed, is
    /// applied and its excess reported in [`BookedFill::Applied`]'s
    /// `overfill`. The hold becomes the requirement at `marks`, capped at
    /// the cash (`available + hold`, never negative), as in
    /// [`MarginReservationService::reserve_with_gate`]: a real fill is
    /// never refused for lack of cash, and an uncovered requirement shows as
    /// `hold < required`.
    ///
    /// Atomic: the new remaining quantity and hold are computed first, and
    /// the hold, the remaining quantity and the execution id are only
    /// recorded together. On any error neither the service nor the hold
    /// changes (the booking stays in the ledger), so the same execution can
    /// be reported again, for example once a missing mark is supplied.
    ///
    /// # Errors
    ///
    /// - [`MarginError::WrongReservationMode`] for a service with its own
    ///   positions.
    /// - [`MarginError::ConflictingEntry`] if the execution was reported for
    ///   another order, or its booking has another instrument or side than
    ///   the order.
    /// - [`MarginError::UnknownOrder`] if the order was never reserved.
    /// - [`MarginError::FillNotBooked`] if the ledger has no execution with
    ///   this id.
    /// - The errors of [`MarginReservationService::requirement_with_gate`]
    ///   (a missing mark among them) and of [`RiskGate::set_hold`].
    pub fn apply_booked_fill(
        &mut self,
        gate: &mut RiskGate,
        order: &OrderId,
        execution: &ExecutionId,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<BookedFill> {
        let Mode::OnLedger(hold) = &self.mode else {
            return Err(self.wrong_mode("apply_booked_fill"));
        };
        let hold = hold.clone();
        let conflict = || MarginError::ConflictingEntry {
            kind: "fill",
            id: execution.to_string(),
        };
        if let Some((seen_order, _)) = self.fills.get(execution) {
            if seen_order == order {
                return Ok(BookedFill::Duplicate);
            }
            return Err(conflict());
        }
        let pending = self.pending(order)?;
        let snapshot = gate.ledger().snapshot();
        let booked = gate.ledger().execution_cashflow(execution).ok_or_else(|| {
            MarginError::FillNotBooked {
                id: execution.to_string(),
            }
        })?;
        if booked.instrument != pending.order.instrument || booked.side != pending.order.side {
            return Err(conflict());
        }
        let contracts = booked.quantity;
        let left = sub(pending.remaining, contracts, "remaining - fill")?;
        let (remaining, overfill) = if left < Decimal::ZERO {
            (Decimal::ZERO, Some(neg(left)))
        } else {
            (left, None)
        };
        // The order book with the fill, then the requirement and the hold
        // it implies, all before anything is recorded.
        let mut orders = self.orders.clone();
        if remaining.is_zero() {
            orders.remove(order);
        } else if let Some(entry) = orders.get_mut(order) {
            entry.remaining = remaining;
        }
        let currency = self.settlement()?;
        let book = self.book_in(&snapshot)?;
        let required = self.state(&orders, &book, marks)?.total;
        let cash = Self::cash_collateral_of(&snapshot, &hold, &currency)?;
        let new_hold = required.min(cash.max(Decimal::ZERO));
        gate.set_hold(&hold, currency, new_hold)?;
        self.commit_fill(order, execution.clone(), contracts, remaining);
        Ok(BookedFill::Applied {
            contracts,
            remaining,
            overfill,
            required,
            hold: new_hold,
        })
    }

    /// Settlement asset of the registered instruments.
    fn settlement(&self) -> Result<Asset> {
        let mut assets = self.instruments.values().map(|i| &i.settlement);
        let first = assets
            .next()
            .ok_or_else(|| MarginError::invalid_input("instruments", "none registered"))?;
        if let Some(other) = assets.find(|a| *a != first) {
            return Err(MarginError::CurrencyMismatch {
                field: "settlement",
                expected: first.clone(),
                found: other.clone(),
            });
        }
        Ok(first.clone())
    }

    /// [`MarginReservationService::reserve`] against a [`RiskGate`]'s
    /// ledger, keeping the account's whole requirement as the external hold
    /// `hold` so spot orders cannot spend the same cash. See the
    /// [module docs](self).
    ///
    /// Collateral is the settlement asset's available balance plus the
    /// current hold (cash collateral), and on the ledger also the
    /// unrealized PnL of every open future or perpetual settled in
    /// that asset (see
    /// [positions](self#positions-own-or-read-from-the-ledger)). On
    /// acceptance the hold is set to the new requirement, capped at the
    /// cash (available plus hold): a hold never holds unrealized gains.
    ///
    /// # Errors
    ///
    /// As [`MarginReservationService::reserve`] (without its mode error),
    /// plus [`MarginError::CurrencyMismatch`] if instruments settle in
    /// different assets and the errors of [`RiskGate::set_hold`]. On the
    /// ledger, also: [`MarginError::InvalidInput`] if `hold` is not the
    /// service's hold; the errors of
    /// [`MarginReservationService::requirement_with_gate`];
    /// [`MarginError::CurrencyMismatch`] if the service's hold is in another
    /// asset; [`MarginError::InvalidInput`] or
    /// [`MarginError::PrecisionExceeded`] for a mark of an open derivative
    /// that is not positive or has more than 18 decimals. A missing mark is
    /// [`MarginRejection::MissingMark`].
    pub fn reserve_with_gate(
        &mut self,
        order: MarginOrder,
        gate: &mut RiskGate,
        hold: &ReservationId,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<MarginDecision> {
        // Refused before the order id is used and before any change.
        Dir::from_action(order.side, "order side")?;
        let hold = self.hold_id(hold)?.clone();
        let currency = self.settlement()?;
        let snapshot = gate.ledger().snapshot();
        let book = self.book_in(&snapshot)?;
        let (cash, upnl) = self.gate_collateral(&snapshot, &hold, &currency, marks)?;
        // Decide without changing anything, set the hold, and only then
        // commit the order: on any error neither the service nor the gate
        // changes, and the order id stays unused.
        let (decision, candidate) = match upnl {
            Ok(terms) => {
                let collateral = net_with_upnl(cash, &terms)?;
                self.decide(&order, Some(collateral), &book, marks)?
            }
            Err(instrument) => (
                MarginDecision::Rejected {
                    reason: MarginRejection::MissingMark { instrument },
                },
                None,
            ),
        };
        if let MarginDecision::Reserved { required, .. } = &decision {
            // A risk reducing order may be accepted while the requirement
            // exceeds the cash; the hold then takes whatever cash there is,
            // never a negative amount (the account may be in debt).
            let holdable = cash.max(Decimal::ZERO);
            gate.set_hold(&hold, currency, (*required).min(holdable))?;
        }
        self.seen_orders.insert(order.id);
        if let Some(orders) = candidate {
            self.orders = orders;
        }
        Ok(decision)
    }

    /// `available + own hold` in `currency`: the cash collateral, the
    /// service's own hold counted once and no other.
    fn cash_collateral_of(
        snapshot: &LedgerSnapshot,
        hold: &ReservationId,
        currency: &Asset,
    ) -> Result<Decimal> {
        let available = snapshot
            .balances
            .get(currency)
            .map_or(Decimal::ZERO, |b| b.available);
        let own = match snapshot.reservations.get(hold) {
            None => Decimal::ZERO,
            Some((asset, amount)) if asset == currency => *amount,
            Some((asset, _)) => {
                return Err(MarginError::CurrencyMismatch {
                    field: "hold",
                    expected: currency.clone(),
                    found: asset.clone(),
                });
            }
        };
        add(available, own, "available + hold")
    }

    /// The cash collateral (`available + own hold` in `currency`) and, on
    /// the ledger, the unrealized PnL terms of every open future or
    /// perpetual settled in `currency` (market value, then minus basis, so
    /// the caller sums them with the cash once), or the instrument whose
    /// mark is missing. A service with its own positions counts no
    /// unrealized PnL.
    fn gate_collateral(
        &self,
        snapshot: &LedgerSnapshot,
        hold: &ReservationId,
        currency: &Asset,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<(Decimal, core::result::Result<Vec<Decimal>, InstrumentId>)> {
        let cash = Self::cash_collateral_of(snapshot, hold, currency)?;
        if let Mode::Own(_) = self.mode {
            return Ok((cash, Ok(Vec::new())));
        }
        let scale = *snapshot
            .assets
            .get(currency)
            .ok_or_else(|| MarginError::UnknownAsset {
                asset: currency.clone(),
            })?;
        let mut terms = Vec::new();
        for (id, position) in &snapshot.positions {
            if position.quantity.is_zero() {
                continue;
            }
            let instrument =
                snapshot
                    .instruments
                    .get(id)
                    .ok_or_else(|| MarginError::UnknownInstrument {
                        instrument: id.clone(),
                    })?;
            let Some((_, contract)) = derivative_terms(instrument) else {
                continue;
            };
            if contract.settlement != *currency {
                continue;
            }
            let Some(mark) = marks.get(id) else {
                return Ok((cash, Err(id.clone())));
            };
            let is_option = matches!(instrument, Instrument::Option(_));
            if *mark < Decimal::ZERO || (*mark == Decimal::ZERO && !is_option) {
                return Err(MarginError::invalid_input(
                    "instrument mark",
                    "must be positive",
                ));
            }
            check_scale(*mark, MAX_SCALE, "instrument mark")?;
            let value = crate::ledger::derivative::market_value(
                position.quantity,
                contract.size,
                *mark,
                scale,
            )?;
            // An option's basis already moved cash: a short deducts its value
            // and a long counts nothing.
            if is_option {
                terms.push(value.min(Decimal::ZERO));
                continue;
            }
            // upnl: value - basis (linear), basis - value (inverse).
            match contract.size {
                ContractSize::Multiplier(_) => {
                    terms.push(value);
                    terms.push(neg(position.cost_basis));
                }
                ContractSize::ContractValue(_) => {
                    terms.push(position.cost_basis);
                    terms.push(neg(value));
                }
            }
        }
        Ok((cash, Ok(terms)))
    }

    /// Sets the gate's external hold to the current requirement, after
    /// fills, effective cancels or new marks. Returns the new hold.
    ///
    /// On the ledger the positions are read from `gate`'s ledger, so the
    /// hold follows every fill booked there.
    ///
    /// # Errors
    ///
    /// The errors of [`MarginReservationService::requirement_with_gate`]
    /// and of [`RiskGate::set_hold`] (a requirement that grew beyond the
    /// available cash, for example after a mark move, is reported and the
    /// previous hold kept); on the ledger, [`MarginError::InvalidInput`] if
    /// `hold` is not the service's hold. Atomic: on error neither the
    /// service nor the gate changes.
    pub fn sync_hold(
        &self,
        gate: &mut RiskGate,
        hold: &ReservationId,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<Decimal> {
        let hold = self.hold_id(hold)?;
        let currency = self.settlement()?;
        let required = self.requirement_with_gate(gate, marks)?.total;
        gate.set_hold(hold, currency, required)?;
        Ok(required)
    }

    /// Releases what is left of `order` once its cancel is effective, and
    /// returns the contracts released (zero if the order is no longer
    /// live, so repeating it is harmless).
    pub fn release_on_effective_cancel(&mut self, order: &OrderId) -> Decimal {
        match self.orders.remove(order) {
            Some(pending) => {
                self.closed.insert(order.clone(), pending.order);
                pending.remaining
            }
            None => Decimal::ZERO,
        }
    }
}

/// Instruments with their terms and current positions.
type Book = BTreeMap<InstrumentId, (MarginInstrument, Decimal)>;

/// The product and contract of a future or perpetual; `None` for spot.
fn derivative_terms(instrument: &Instrument) -> Option<(ProductKind, DerivativeContract)> {
    Some((instrument.derivative_product()?, instrument.derivative()?))
}
