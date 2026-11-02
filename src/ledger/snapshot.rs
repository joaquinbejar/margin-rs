//! Point in time copies of the ledger state.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::ids::{Asset, InstrumentId, ReservationId};
use crate::instrument::Instrument;

/// Balance of one asset.
///
/// `available = total - reserved`. Only the available part can be spent by
/// new executions, withdrawals or reservations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BalanceSnapshot {
    /// Amount held.
    pub total: Decimal,
    /// Amount held back by open reservations.
    pub reserved: Decimal,
    /// `total - reserved`.
    pub available: Decimal,
}

/// Position in one instrument.
///
/// `quantity` is the net quantity traded on the instrument since the ledger
/// was opened: positive long, negative short. It is not the base asset
/// balance, which also moves with transfers and fees paid in the base
/// asset. `cost_basis` is the signed quote amount still attributed to the
/// open quantity (positive for a long, negative for a short), and
/// `realized_pnl` accumulates closed PnL gross of fees. Both are in the
/// `quote` asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionSnapshot {
    /// Instrument.
    pub instrument: InstrumentId,
    /// Asset of `cost_basis` and `realized_pnl`.
    pub quote: Asset,
    /// Net traded quantity; positive long, negative short.
    pub quantity: Decimal,
    /// Signed quote amount attributed to the open quantity.
    pub cost_basis: Decimal,
    /// Realized PnL gross of fees.
    pub realized_pnl: Decimal,
}

impl PositionSnapshot {
    /// True when the position is flat.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        self.quantity.is_zero()
    }

    /// `cost_basis / quantity`, or `None` when flat.
    ///
    /// Informational: it is computed to 28 significant digits and is never
    /// used for booking, which works on `cost_basis` directly.
    #[must_use]
    pub fn average_price(&self) -> Option<Decimal> {
        if self.quantity.is_zero() {
            None
        } else {
            self.cost_basis.checked_div(self.quantity)
        }
    }
}

/// Copy of the full accounting state, with deterministic ordering.
///
/// A snapshot is immutable and self contained: it carries the asset scales
/// and instrument definitions, so valuation and stress functions
/// ([`crate::valuation::value`], [`crate::scenarios::stress`]) work on it
/// without touching the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSnapshot {
    /// Registered assets and their scales.
    pub assets: BTreeMap<Asset, u32>,
    /// Registered instruments.
    pub instruments: BTreeMap<InstrumentId, Instrument>,
    /// Deposits minus withdrawals, per asset.
    pub net_transfers: BTreeMap<Asset, Decimal>,
    /// Balance per asset, for every registered asset.
    pub balances: BTreeMap<Asset, BalanceSnapshot>,
    /// Position per instrument, for every registered instrument.
    pub positions: BTreeMap<InstrumentId, PositionSnapshot>,
    /// Cumulative fees per asset, derivative settlement fees included;
    /// negative when rebates exceed fees.
    pub fees_paid: BTreeMap<Asset, Decimal>,
    /// Cumulative realized PnL paid by derivative settlements, per asset,
    /// gross of fees.
    pub settled_pnl: BTreeMap<Asset, Decimal>,
    /// Open reservations.
    pub reservations: BTreeMap<ReservationId, (Asset, Decimal)>,
    /// Number of executions booked.
    pub executions_booked: u64,
    /// Number of derivative settlements booked.
    pub settlements_booked: u64,
}
