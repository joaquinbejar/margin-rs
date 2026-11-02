//! Point in time copies of the ledger state.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::decimal::{Rounding, mul_div_round};
use crate::ids::{Asset, InstrumentId, ReservationId};

use crate::instrument::{ContractSize, DECIMAL_DIGITS, Instrument, PositionKind};

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
    /// Asset of `cost_basis` and `realized_pnl`: the quote asset of a spot
    /// instrument, the settlement asset of a derivative (the base asset of
    /// an inverse one).
    pub quote: Asset,
    /// Spot, or a linear or inverse future or perpetual.
    pub kind: PositionKind,
    /// Size of one unit of `quantity`: `Multiplier(1)` for spot, the
    /// multiplier of a linear derivative, the contract value of an inverse
    /// one.
    pub size: ContractSize,
    /// Net traded quantity (contracts for a derivative); positive long,
    /// negative short.
    pub quantity: Decimal,
    /// Signed amount attributed to the open quantity: the quote cost of a
    /// spot position, the basis (cash value at entry prices) of a linear
    /// derivative, the value in the base at entry prices of an inverse
    /// one.
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

    /// Average entry price: `cost_basis / (quantity * multiplier)` for spot
    /// and linear derivatives, and for an inverse derivative the harmonic
    /// mean `quantity * contract_value / cost_basis`.
    ///
    /// `None` when the position is flat, when an inverse position has a
    /// zero basis (its entry value rounded to zero), or when the quotient
    /// cannot be represented as a `Decimal` at any scale.
    ///
    /// Informational: it is rounded once (half-even for a derivative) to the
    /// largest scale, up to 28, that fits a `Decimal`, and is never used for
    /// booking or risk, which work on `cost_basis` directly. For a
    /// derivative it is the price per unit of the underlying (in the quote
    /// currency of the contract value for an inverse one), not per
    /// contract, and the quotient is computed in wide integers.
    #[must_use]
    pub fn average_price(&self) -> Option<Decimal> {
        if self.quantity.is_zero() {
            return None;
        }
        let (numerators, denominators) = match self.size {
            ContractSize::Multiplier(multiplier) if multiplier == Decimal::ONE => {
                return self.cost_basis.checked_div(self.quantity);
            }
            ContractSize::Multiplier(multiplier) => {
                ([self.cost_basis, Decimal::ONE], [self.quantity, multiplier])
            }
            ContractSize::ContractValue(contract_value) => {
                if self.cost_basis.is_zero() {
                    return None;
                }
                (
                    [self.quantity, contract_value],
                    [self.cost_basis, Decimal::ONE],
                )
            }
        };
        // The largest scale (up to a Decimal's 28) whose rounded quotient
        // still fits, as checked_div gives for spot.
        (0..=DECIMAL_DIGITS).rev().find_map(|scale| {
            mul_div_round(
                &numerators,
                &denominators,
                scale,
                Rounding::HalfEven,
                "average price",
            )
            .ok()
        })
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
    /// Net funding received by perpetuals, per asset (negative when paid).
    pub funding: BTreeMap<Asset, Decimal>,
    /// Open reservations.
    pub reservations: BTreeMap<ReservationId, (Asset, Decimal)>,
    /// Number of executions booked.
    pub executions_booked: u64,
    /// Number of derivative settlements booked.
    pub settlements_booked: u64,
    /// Instruments settled at expiry.
    pub settled_instruments: Vec<InstrumentId>,
}
