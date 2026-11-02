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
//! - [`MarginReservationService::apply_fill`] is idempotent by execution
//!   id: it moves the filled contracts from the order to the position.
//! - [`MarginReservationService::release_on_effective_cancel`] closes the
//!   order's remaining quantity; repeating it changes nothing.

use std::collections::{BTreeMap, BTreeSet};

use crate::ids::ReservationId;
use crate::risk::RiskGate;

use rust_decimal::Decimal;

use crate::decimal::{add, neg, sub, sum};
use crate::direction::Dir;
use crate::error::{MarginError, Result};
use crate::ids::{AccountId, Asset, ExecutionId, InstrumentId, OrderId, VenueId};
use crate::initial_margin::{
    ExposurePosition, ExposureView, InitialMarginModel, MarginRequirement, ProductKind,
};
use crate::ledger::Action;

/// Contract terms of a margined instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginInstrument {
    /// Product kind.
    pub product: ProductKind,
    /// Settlement asset.
    pub settlement: Asset,
    /// Contract multiplier.
    pub multiplier: Decimal,
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

/// Margin reservations of one account at one venue. See the
/// [module docs](self).
#[derive(Debug, Clone)]
pub struct MarginReservationService<M> {
    model: M,
    account: AccountId,
    venue: VenueId,
    instruments: BTreeMap<InstrumentId, MarginInstrument>,
    positions: BTreeMap<InstrumentId, Decimal>,
    orders: BTreeMap<OrderId, PendingOrder>,
    closed: BTreeMap<OrderId, MarginOrder>,
    seen_orders: BTreeSet<OrderId>,
    fills: BTreeMap<ExecutionId, (OrderId, Decimal)>,
}

impl<M: InitialMarginModel> MarginReservationService<M> {
    /// A service with no instruments, positions or orders.
    pub fn new(model: M, account: AccountId, venue: VenueId) -> Self {
        Self {
            model,
            account,
            venue,
            instruments: BTreeMap::new(),
            positions: BTreeMap::new(),
            orders: BTreeMap::new(),
            closed: BTreeMap::new(),
            seen_orders: BTreeSet::new(),
            fills: BTreeMap::new(),
        }
    }

    /// Registers an instrument the account may trade.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] for a non positive multiplier.
    pub fn register(&mut self, id: InstrumentId, instrument: MarginInstrument) -> Result<()> {
        if instrument.multiplier <= Decimal::ZERO {
            return Err(MarginError::invalid_input("multiplier", "must be positive"));
        }
        self.positions.entry(id.clone()).or_default();
        self.instruments.insert(id, instrument);
        Ok(())
    }

    /// Current signed position in contracts.
    #[must_use]
    pub fn position(&self, instrument: &InstrumentId) -> Option<Decimal> {
        self.positions.get(instrument).copied()
    }

    /// Remaining contracts of a live order.
    #[must_use]
    pub fn remaining(&self, order: &OrderId) -> Option<Decimal> {
        self.orders.get(order).map(|o| o.remaining)
    }

    /// Requirement of positions and live orders at `marks`.
    ///
    /// # Errors
    ///
    /// [`MarginError::MissingMark`] if an instrument with exposure has no
    /// mark, or any error of the margin rule.
    pub fn requirement(&self, marks: &BTreeMap<InstrumentId, Decimal>) -> Result<MarginState> {
        self.state(&self.orders, marks)
    }

    fn state(
        &self,
        orders: &BTreeMap<OrderId, PendingOrder>,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<MarginState> {
        let worst = self.view(orders, marks, true)?;
        let current = self.view(orders, marks, false)?;
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
        for (id, instrument) in &self.instruments {
            let position = self.positions.get(id).copied().unwrap_or_default();
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
                multiplier: instrument.multiplier,
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
    pub fn reserve(
        &mut self,
        order: MarginOrder,
        collateral: Option<Decimal>,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<MarginDecision> {
        // Refused before the order id is used and before any change.
        Dir::from_action(order.side, "order side")?;
        let (decision, candidate) = self.decide(&order, collateral, marks)?;
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
    fn is_risk_reducing(&self, order: &MarginOrder) -> Result<bool> {
        let position = self
            .positions
            .get(&order.instrument)
            .copied()
            .unwrap_or_default();
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
        let before = match self.state(&self.orders, marks) {
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
        let after = match self.state(&with_order, marks) {
            Ok(state) => state,
            Err(MarginError::MissingMark { instrument }) => {
                return reject(MarginRejection::MissingMark { instrument });
            }
            Err(error) => return Err(error),
        };
        // Risk reducing exception: accepted
        // whatever the collateral, even when under margined.
        if after.total > collateral && !self.is_risk_reducing(order)? {
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
    /// [`MarginError::InvalidInput`] for non positive contracts.
    pub fn apply_fill(
        &mut self,
        order: &OrderId,
        execution: ExecutionId,
        contracts: Decimal,
    ) -> Result<bool> {
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
        let pending = match (self.orders.get(order), self.closed.get(order)) {
            (Some(live), _) => live.clone(),
            (None, Some(closed)) => PendingOrder {
                order: closed.clone(),
                remaining: Decimal::ZERO,
            },
            (None, None) => {
                return Err(MarginError::UnknownOrder {
                    id: order.to_string(),
                });
            }
        };
        let signed = match Dir::from_action(pending.order.side, "pending order side")? {
            Dir::Buy => contracts,
            Dir::Sell => neg(contracts),
        };
        let position = self
            .positions
            .get(&pending.order.instrument)
            .copied()
            .unwrap_or_default();
        let new_position = add(position, signed, "position + fill")?;
        let remaining = sub(pending.remaining, contracts, "remaining - fill")?.max(Decimal::ZERO);
        self.positions
            .insert(pending.order.instrument.clone(), new_position);
        if remaining.is_zero() {
            if let Some(closed) = self.orders.remove(order) {
                self.closed.insert(order.clone(), closed.order);
            }
        } else if let Some(entry) = self.orders.get_mut(order) {
            entry.remaining = remaining;
        }
        self.fills.insert(execution, (order.clone(), contracts));
        Ok(true)
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
    /// current hold (cash collateral). On acceptance the
    /// hold is set to the new requirement.
    ///
    /// # Errors
    ///
    /// As [`MarginReservationService::reserve`], plus
    /// [`MarginError::CurrencyMismatch`] if instruments settle in different
    /// assets and the errors of [`RiskGate::set_hold`].
    pub fn reserve_with_gate(
        &mut self,
        order: MarginOrder,
        gate: &mut RiskGate,
        hold: &ReservationId,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<MarginDecision> {
        let currency = self.settlement()?;
        let available = gate
            .ledger()
            .balance(&currency)
            .map(|b| b.available)
            .unwrap_or_default();
        let collateral = add(available, gate.hold(hold), "available + hold")?;
        // Decide without changing anything, set the hold, and only then
        // commit the order: on any error neither the service nor the gate
        // changes, and the order id stays unused.
        let (decision, candidate) = self.decide(&order, Some(collateral), marks)?;
        if let MarginDecision::Reserved { required, .. } = &decision {
            // A risk reducing order may be accepted while the requirement
            // exceeds the cash; the hold then takes whatever cash there is,
            // never a negative amount (the account may be in debt).
            let holdable = collateral.max(Decimal::ZERO);
            gate.set_hold(hold, currency, (*required).min(holdable))?;
        }
        self.seen_orders.insert(order.id);
        if let Some(orders) = candidate {
            self.orders = orders;
        }
        Ok(decision)
    }

    /// Sets the gate's external hold to the current requirement, after
    /// fills, effective cancels or new marks. Returns the new hold.
    ///
    /// # Errors
    ///
    /// The errors of [`MarginReservationService::requirement`] and of
    /// [`RiskGate::set_hold`] (a requirement that grew beyond the available
    /// cash, for example after a mark move, is reported and the previous
    /// hold kept). Atomic: on error neither the service nor the gate
    /// changes and the order id stays unused.
    pub fn sync_hold(
        &self,
        gate: &mut RiskGate,
        hold: &ReservationId,
        marks: &BTreeMap<InstrumentId, Decimal>,
    ) -> Result<Decimal> {
        let currency = self.settlement()?;
        let required = self.requirement(marks)?.total;
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
