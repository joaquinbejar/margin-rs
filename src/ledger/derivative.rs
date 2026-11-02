//! Linear futures and perpetuals held in the ledger.
//!
//! A position is signed contracts `Q` with a signed **basis**: the cash value
//! of the open contracts at their entry prices, in the settlement asset (a
//! long has a positive basis). With multiplier `M`, settlement asset scale
//! `c` and `rhe` the half-even rounding to `c` digits (always half-even for
//! derivatives; [`LedgerConfig::rounding`](crate::ledger::LedgerConfig)
//! applies to spot only):
//!
//! | Fill of `n` contracts at `p` | Effect |
//! |---|---|
//! | virtual notional | `N = rhe(p * n * M)`; no asset is exchanged |
//! | open or increase | `basis += N` for a buy, `-N` for a sell; no cash moves |
//! | reduce, `n <= abs(Q)` | `released = rhe(basis * n / abs(Q))` (all of it on a full close); realized `= +N - released` for a sell, `-N - released` for a buy |
//! | cross zero, `n > abs(Q)` | `C = rhe(p * abs(Q) * M)`, `O = N - C` exactly; realized `= +C - basis` (sell) or `-C - basis` (buy); new basis `= -O` (sell) or `+O` (buy) |
//! | cash | `+ realized - fee`, booked even below the reservations or zero |
//!
//! `N`, `C` and `O` stay in wide integers: only the stored results
//! (realized PnL, basis, cash) must fit a `Decimal`. Every execution is
//! mandatory: it is booked even when it overdraws the account
//! ([`Ledger::book_execution_allowing_overdraft`](crate::ledger::Ledger::book_execution_allowing_overdraft)).

use rust_decimal::Decimal;

use super::Position;
use crate::decimal::{
    Rounding, Wide, add, decimal_to_wide, mul_div_round, mul_div_round_wide, neg, wide_to_decimal,
};
use crate::direction::Dir;
use crate::error::Result;
use crate::ids::{Asset, ExecutionId, InstrumentId};

/// Derivative details of a booked execution
/// ([`ExecutionCashflow::derivative`](crate::ledger::ExecutionCashflow::derivative)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivativeFill {
    /// Contract multiplier.
    pub multiplier: Decimal,
    /// The fill's virtual notional `rhe(p * n * M)`: the exposure it trades,
    /// never exchanged in cash. `None` only when it is out of a `Decimal`'s
    /// range; it is then still exact in the booking, and can be rebuilt in
    /// wide integers from the execution and the instrument.
    pub virtual_notional: Option<Decimal>,
    /// The closing part `rhe(p * abs(Q) * M)` of a fill that crosses zero.
    /// `None` when the fill does not cross zero, or (as above) only when it
    /// is out of a `Decimal`'s range.
    pub closing_notional: Option<Decimal>,
    /// Basis released by the fill, signed like the basis before the fill
    /// (positive for a long): the whole basis on a full close or a crossing,
    /// zero when the fill opens or increases.
    pub released_basis: Decimal,
    /// Set for a late fill on a future already settled at expiry: the
    /// settlement price at which the fill was closed at once. The position
    /// stays flat and the realized PnL is the fill against that price:
    /// `closing_notional` is then `rhe(settlement price * n * M)` and
    /// `released_basis` is zero (the provisional basis is never stored).
    pub closed_at_settlement: Option<Decimal>,
}

/// A funding payment of a perpetual, booked with
/// [`Ledger::book_funding`](crate::ledger::Ledger::book_funding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingPayment {
    /// Identifier. It shares the namespace of execution identifiers.
    pub id: ExecutionId,
    /// Perpetual instrument.
    pub instrument: InstrumentId,
    /// Amount in the settlement asset: positive is received, negative is
    /// paid.
    pub amount: Decimal,
    /// When the funding applies, in milliseconds since the epoch.
    pub as_of_ms: u64,
}

/// How one booked funding payment changed the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingCashflow {
    /// Identifier.
    pub id: ExecutionId,
    /// Position in the funding journal, starting at 0.
    pub sequence: u64,
    /// Perpetual instrument.
    pub instrument: InstrumentId,
    /// Settlement asset.
    pub asset: Asset,
    /// Signed amount: positive is received.
    pub amount: Decimal,
    /// Total balance of the asset after the payment.
    pub balance_after: Decimal,
}

/// The cash settlement of a future at expiry, booked with
/// [`Ledger::settle_expiry`](crate::ledger::Ledger::settle_expiry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpirySettlement {
    /// Identifier. It shares the namespace of execution identifiers.
    pub id: ExecutionId,
    /// Future instrument.
    pub instrument: InstrumentId,
    /// Settlement price.
    pub price: Decimal,
    /// When the settlement is booked, in milliseconds since the epoch; at
    /// or after the future's expiry.
    pub as_of_ms: u64,
}

/// How a future's expiry settlement changed the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiryCashflow {
    /// Identifier.
    pub id: ExecutionId,
    /// Position in the expiry journal, starting at 0.
    pub sequence: u64,
    /// Future instrument.
    pub instrument: InstrumentId,
    /// Settlement asset.
    pub asset: Asset,
    /// Settlement price.
    pub price: Decimal,
    /// Contracts closed (signed like the position before settlement; zero
    /// when it was flat).
    pub contracts: Decimal,
    /// Realized PnL of the close, gross of fees (there is no fee).
    pub realized_pnl: Decimal,
    /// The details of the close, as for an execution.
    pub derivative: DerivativeFill,
    /// Total balance of the asset after the settlement.
    pub balance_after: Decimal,
}

/// Result of [`Ledger::book_funding`](crate::ledger::Ledger::book_funding)
/// and [`Ledger::settle_expiry`](crate::ledger::Ledger::settle_expiry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingBooking<T> {
    /// The cashflow, recorded the first time the posting was booked.
    pub cashflow: T,
    /// True when this call changed the ledger; false for a duplicate.
    pub is_new: bool,
    /// Amount by which this posting left the asset's total below its
    /// reservations; `None` when it did not, and for a duplicate.
    pub shortfall: Option<Decimal>,
}

/// A derivative fill applied to a position.
pub(crate) struct FillEffect {
    pub(crate) position: Position,
    pub(crate) realized: Decimal,
    pub(crate) fill: DerivativeFill,
}

/// The half-even rounding every derivative figure uses.
const DERIVATIVE_ROUNDING: Rounding = Rounding::HalfEven;

/// Applies a fill of `contracts > 0` at `price` in direction `dir` to
/// `position`, with multiplier `multiplier` and settlement scale `scale`.
/// Only the stored results must fit a `Decimal`.
pub(crate) fn apply_derivative_fill(
    position: Position,
    dir: Dir,
    price: Decimal,
    contracts: Decimal,
    multiplier: Decimal,
    scale: u32,
) -> Result<FillEffect> {
    let held = position.quantity;
    let basis = position.cost_basis;
    let notional = mul_div_round_wide(
        &[price, contracts, multiplier],
        &[],
        scale,
        DERIVATIVE_ROUNDING,
        "price * contracts * multiplier",
    )?;
    // Signed like a buy: + for a buy, - for a sell.
    let (signed_contracts, signed_value) = match dir {
        Dir::Buy => (contracts, notional),
        Dir::Sell => (neg(contracts), notional.negated()),
    };
    let quantity = add(held, signed_contracts, "contracts")?;
    let virtual_notional = notional.to_decimal(scale);
    let same_side =
        held.is_zero() || held.is_sign_negative() == signed_contracts.is_sign_negative();

    if same_side {
        let basis_w = decimal_to_wide(basis, scale, "basis")?;
        let new_basis = add_wide(basis_w, signed_value, "basis + notional")?;
        return Ok(FillEffect {
            position: Position {
                quantity,
                cost_basis: wide_to_decimal(new_basis, scale, "basis")?,
                realized_pnl: position.realized_pnl,
            },
            realized: Decimal::ZERO,
            fill: DerivativeFill {
                multiplier,
                virtual_notional,
                closing_notional: None,
                released_basis: Decimal::ZERO,
                closed_at_settlement: None,
            },
        });
    }

    let held_abs = held.abs();
    if contracts <= held_abs {
        // Reduce: release a share of the basis, all of it on a full close.
        let released = if contracts == held_abs {
            basis
        } else {
            mul_div_round(
                &[basis, contracts],
                &[held_abs],
                scale,
                DERIVATIVE_ROUNDING,
                "released basis",
            )?
        };
        // realized = -signed_value - released.
        let released_w = decimal_to_wide(released, scale, "released basis")?;
        let realized_w = add_wide(signed_value.negated(), released_w.negated(), "realized pnl")?;
        let realized = wide_to_decimal(realized_w, scale, "realized pnl")?;
        return Ok(FillEffect {
            position: Position {
                quantity,
                cost_basis: crate::decimal::sub(basis, released, "basis - released")?,
                realized_pnl: add(position.realized_pnl, realized, "realized pnl")?,
            },
            realized,
            fill: DerivativeFill {
                multiplier,
                virtual_notional,
                closing_notional: None,
                released_basis: released,
                closed_at_settlement: None,
            },
        });
    }

    // Cross zero: close abs(Q) (one rounding), open the rest as the exact
    // difference, so the fill as a whole keeps its single rounding.
    let closing = mul_div_round_wide(
        &[price, held_abs, multiplier],
        &[],
        scale,
        DERIVATIVE_ROUNDING,
        "price * held * multiplier",
    )?;
    let signed_close = if signed_value.negative {
        closing.negated()
    } else {
        closing
    };
    let opening = add_wide(signed_value, signed_close.negated(), "opening notional")?;
    let basis_w = decimal_to_wide(basis, scale, "basis")?;
    let realized_w = add_wide(signed_close.negated(), basis_w.negated(), "realized pnl")?;
    let realized = wide_to_decimal(realized_w, scale, "realized pnl")?;
    Ok(FillEffect {
        position: Position {
            quantity,
            cost_basis: wide_to_decimal(opening, scale, "basis")?,
            realized_pnl: add(position.realized_pnl, realized, "realized pnl")?,
        },
        realized,
        fill: DerivativeFill {
            multiplier,
            virtual_notional,
            closing_notional: closing.to_decimal(scale),
            released_basis: basis,
            closed_at_settlement: None,
        },
    })
}

/// A late fill of `contracts` at `price` on a flat, settled future, closed
/// at once at `settlement_price`: realized `= rhe(P * n * M) - rhe(p * n *
/// M)` for a buy and the opposite for a sell, computed in wide integers so
/// that only the realized PnL must fit a `Decimal`. The position stays flat.
///
/// # Errors
///
/// [`MarginError::InvalidInput`](crate::error::MarginError::InvalidInput)
/// if the position is not flat (a settled future always is), or an
/// arithmetic error if the realized PnL does not fit.
pub(crate) fn close_late_fill(
    position: Position,
    dir: Dir,
    price: Decimal,
    contracts: Decimal,
    multiplier: Decimal,
    (settlement_price, scale): (Decimal, u32),
) -> Result<FillEffect> {
    if !position.quantity.is_zero() {
        return Err(crate::error::MarginError::invalid_input(
            "position",
            "a settled future has no open position",
        ));
    }
    let traded = mul_div_round_wide(
        &[price, contracts, multiplier],
        &[],
        scale,
        DERIVATIVE_ROUNDING,
        "price * contracts * multiplier",
    )?;
    let settled = mul_div_round_wide(
        &[settlement_price, contracts, multiplier],
        &[],
        scale,
        DERIVATIVE_ROUNDING,
        "settlement price * contracts * multiplier",
    )?;
    let realized_w = match dir {
        Dir::Buy => add_wide(settled, traded.negated(), "realized pnl")?,
        Dir::Sell => add_wide(traded, settled.negated(), "realized pnl")?,
    };
    let realized = wide_to_decimal(realized_w, scale, "realized pnl")?;
    Ok(FillEffect {
        position: Position {
            quantity: position.quantity,
            cost_basis: position.cost_basis,
            realized_pnl: add(position.realized_pnl, realized, "realized pnl")?,
        },
        realized,
        fill: DerivativeFill {
            multiplier,
            virtual_notional: traded.to_decimal(scale),
            closing_notional: settled.to_decimal(scale),
            released_basis: Decimal::ZERO,
            closed_at_settlement: Some(settlement_price),
        },
    })
}

/// The virtual notional `rhe(p * n * M)` signed as the cash a fill would
/// exchange (negative for a buy), as a wide integer at `scale`: the term of
/// the virtual reconciliation identity `sum == realized_pnl - basis`.
pub(crate) fn signed_virtual_cash(
    dir: Dir,
    price: Decimal,
    contracts: Decimal,
    multiplier: Decimal,
    scale: u32,
) -> Result<Wide> {
    let notional = mul_div_round_wide(
        &[price, contracts, multiplier],
        &[],
        scale,
        DERIVATIVE_ROUNDING,
        "price * contracts * multiplier",
    )?;
    Ok(match dir {
        Dir::Buy => notional.negated(),
        Dir::Sell => notional,
    })
}

/// `rhe(quantity * multiplier * mark)`: the market value of a derivative
/// position, signed like the contracts.
pub(crate) fn market_value(
    quantity: Decimal,
    multiplier: Decimal,
    mark: Decimal,
    scale: u32,
) -> Result<Decimal> {
    mul_div_round(
        &[quantity, multiplier, mark],
        &[],
        scale,
        DERIVATIVE_ROUNDING,
        "contracts * multiplier * mark",
    )
}

pub(crate) fn add_wide(a: Wide, b: Wide, operation: &'static str) -> Result<Wide> {
    a.checked_add(b)
        .ok_or(crate::error::MarginError::arithmetic(operation))
}
