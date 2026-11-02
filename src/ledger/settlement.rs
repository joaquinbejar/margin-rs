//! Cash settlement of derivative positions held outside the ledger.
//!
//! The ledger books spot instruments only. A linear future or perpetual is
//! margined elsewhere (see [`crate::liquidation`]); when such a position is
//! reduced, its realized PnL and fee are paid in cash. A
//! [`DerivativeSettlement`] is that cash movement: one signed amount in one
//! asset, identified like a fill by its [`ExecutionId`].
//!
//! Settlements are mandatory debits: they are always booked in full, never
//! rejected for lack of balance, deferred or reduced. A settlement that leaves an asset below its
//! reservations (or negative) is booked and reports the shortfall, which
//! then shows in [`Ledger::overdrafts`](crate::ledger::Ledger::overdrafts).

use rust_decimal::Decimal;

use crate::ids::{Asset, ExecutionId, InstrumentId};

/// Cash paid or received for a derivative fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivativeSettlement {
    /// Execution identifier of the fill. It shares the namespace of
    /// [`Execution`](crate::ledger::Execution) identifiers: one id is one
    /// fill, booked once.
    pub id: ExecutionId,
    /// Derivative instrument. It must not be a spot instrument registered
    /// in the ledger.
    pub instrument: InstrumentId,
    /// Settlement asset.
    pub asset: Asset,
    /// Realized PnL of the fill, gross of fees: positive is a credit.
    pub realized_pnl: Decimal,
    /// Fee of the fill in the settlement asset; negative for a rebate.
    pub fee: Decimal,
}

/// How one booked settlement changed the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementCashflow {
    /// Execution identifier.
    pub id: ExecutionId,
    /// Position of the settlement in the settlement journal, starting at 0.
    pub sequence: u64,
    /// Derivative instrument.
    pub instrument: InstrumentId,
    /// Settlement asset.
    pub asset: Asset,
    /// Realized PnL, gross of fees.
    pub realized_pnl: Decimal,
    /// Fee.
    pub fee: Decimal,
    /// `realized_pnl - fee`: the change of the asset's total balance.
    pub delta: Decimal,
    /// Total balance of the asset after the settlement.
    pub balance_after: Decimal,
}

/// Result of [`Ledger::book_settlement`](crate::ledger::Ledger::book_settlement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementBooking {
    /// The cashflow, recorded the first time the settlement was booked.
    pub cashflow: SettlementCashflow,
    /// True when this call changed the ledger; false for a duplicate.
    pub is_new: bool,
    /// Amount by which this booking left the asset's total below its
    /// reservations (`reserved - total`); `None` when it did not, and for
    /// a duplicate.
    pub shortfall: Option<Decimal>,
}
