//! An account's margin state at one sequence point.
//!
//! [`AccountMarginView`] gathers, read-only, what describes an account's
//! margin: the requirement (`im`, `mm`), the state of [`assess`], the
//! collateral figures, the debt, the open positions and the resting
//! risk-increasing orders (reservations), all tagged with one sequence
//! number the caller supplies. It computes nothing new: every figure comes
//! from the caller's values, from the rule's requirement or from
//! [`assess`], so a view and an assessment built from the same inputs
//! always agree.
//!
//! Positions and reservations are ordered maps, keyed by instrument and by
//! order; there are no floats.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::error::{MarginError, Result};
use crate::ids::{AccountId, Asset, InstrumentId, OrderId, VenueId};
use crate::initial_margin::{ExposureView, InitialMarginModel, ProductKind, RuleId};
use crate::instrument::ContractSize;
use crate::maintenance::{AccountState, CollateralView, StateReason, assess};
use financial_types::Action;

/// One open position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewPosition {
    /// Product kind.
    pub product: ProductKind,
    /// Signed contracts.
    pub contracts: Decimal,
    /// Contract size.
    pub size: ContractSize,
    /// Mark.
    pub mark: Decimal,
    /// Signed cost basis; zero when the caller gave none.
    pub basis: Decimal,
}

/// One resting risk-increasing order, as the caller declares it: the view
/// does not check that the order would increase the position's risk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewReservation {
    /// Instrument.
    pub instrument: InstrumentId,
    /// Buy or sell.
    pub side: Action,
    /// Contracts left, positive.
    pub contracts: Decimal,
    /// Limit price, at least zero.
    pub limit_price: Decimal,
}

/// The margin state of one account at one sequence point. See the
/// [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountMarginView {
    /// The caller's sequence number every figure belongs to.
    pub sequence: u64,
    /// Account.
    pub account: AccountId,
    /// Venue.
    pub venue: VenueId,
    /// Rule of the requirement.
    pub rule: RuleId,
    /// Currency of every amount.
    pub currency: Asset,
    /// State after the assessment.
    pub state: AccountState,
    /// Conditions that hold against the account, as [`assess`] gives them.
    pub reasons: Vec<StateReason>,
    /// Cash in the currency.
    pub cash: Decimal,
    /// Collateral; `None` when unknown.
    pub collateral: Option<Decimal>,
    /// Equity; `None` when unknown.
    pub equity: Option<Decimal>,
    /// Initial margin.
    pub initial: Decimal,
    /// Maintenance margin.
    pub maintenance: Decimal,
    /// `collateral - maintenance`; `None` when collateral is unknown.
    pub excess: Option<Decimal>,
    /// `max(0, -cash)`.
    pub debt: Decimal,
    /// Open positions by instrument.
    pub positions: BTreeMap<InstrumentId, ViewPosition>,
    /// Resting risk-increasing orders by order.
    pub reservations: BTreeMap<OrderId, ViewReservation>,
}

impl AccountMarginView {
    /// The view of `exposure` at `sequence`: its requirement under `model`,
    /// assessed with [`assess`] from `previous` against `collateral`, with
    /// each position's `basis` (zero when absent) and the resting
    /// `reservations`.
    ///
    /// # Errors
    ///
    /// - The errors of the rule's requirement and of [`assess`].
    /// - [`MarginError::InvalidInput`] for a repeated instrument in
    ///   `exposure`, a basis for an instrument without a position, a
    ///   reservation whose contracts are not positive or whose limit is
    ///   negative.
    pub fn assess<M: InitialMarginModel>(
        sequence: u64,
        model: &M,
        previous: AccountState,
        exposure: &ExposureView,
        basis: &BTreeMap<InstrumentId, Decimal>,
        collateral: &CollateralView,
        reservations: BTreeMap<OrderId, ViewReservation>,
    ) -> Result<Self> {
        let mut positions = BTreeMap::new();
        for position in &exposure.positions {
            let line = ViewPosition {
                product: position.product,
                contracts: position.contracts,
                size: position.size,
                mark: position.mark,
                basis: basis.get(&position.instrument).copied().unwrap_or_default(),
            };
            if positions
                .insert(position.instrument.clone(), line)
                .is_some()
            {
                return Err(MarginError::invalid_input(
                    "positions",
                    "must not repeat an instrument",
                ));
            }
        }
        if basis.keys().any(|id| !positions.contains_key(id)) {
            return Err(MarginError::invalid_input(
                "basis",
                "every basis needs a position",
            ));
        }
        for reservation in reservations.values() {
            if reservation.contracts <= Decimal::ZERO {
                return Err(MarginError::invalid_input(
                    "reservations",
                    "contracts must be positive",
                ));
            }
            if reservation.limit_price < Decimal::ZERO {
                return Err(MarginError::invalid_input(
                    "reservations",
                    "the limit price must be at least zero",
                ));
            }
        }
        let requirement = model.requirement(exposure)?;
        let decision = assess(previous, collateral, &requirement)?;
        Ok(Self {
            sequence,
            account: exposure.account.clone(),
            venue: exposure.venue.clone(),
            rule: decision.rule,
            currency: decision.currency,
            state: decision.state,
            reasons: decision.reasons,
            cash: collateral.cash,
            collateral: decision.collateral,
            equity: collateral.equity,
            initial: decision.initial,
            maintenance: decision.maintenance,
            excess: decision.excess,
            debt: decision.debt,
            positions,
            reservations,
        })
    }
}
