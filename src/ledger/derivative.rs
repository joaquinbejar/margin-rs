//! Linear and inverse futures and perpetuals held in the ledger.
//!
//! # Linear contracts
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
//!
//! # Inverse contracts
//!
//! An inverse contract settles in the base asset: with contract value `CV`
//! (quote units per contract), base scale `c` and prices in the quote
//! currency of `CV`, a fill of `n` contracts at `p` has the virtual value
//! `V = rhe(n * CV / p)` in the base. The basis is the signed value at entry
//! prices (a long has a positive basis) and evolves exactly as above with
//! `V` in place of `N`; the realized PnL has the opposite sign, because a
//! long gains when its value in the base falls:
//!
//! | Fill of `n` contracts at `p` | Effect |
//! |---|---|
//! | open or increase | `basis += V` for a buy, `-V` for a sell |
//! | reduce, `n <= abs(Q)` | `released = rhe(basis * n / abs(Q))` (all of it on a full close); realized `= released - V` for a sell (a long), `released + V` for a buy (a short) |
//! | cross zero, `n > abs(Q)` | `C = rhe(abs(Q) * CV / p)`, `O = V - C` exactly; realized `= basis - C` (sell) or `basis + C` (buy); new basis `= -O` (sell) or `+O` (buy) |
//! | cash | `+ realized - fee`, in the base |
//!
//! So `realized = Q * CV * (1/entry - 1/exit)` for each closed contract, up
//! to the half-even rounding of each value. Every value `V` (and `C`, and
//! the values of an expiry or a late fill) is a quotient rounded once; its
//! exact rest is recorded as an [`InverseResidual`], with
//! `n * CV / p = V + numerator / denominator` exactly. The released basis
//! is an allocation of the basis, as for linear contracts, and has no
//! residual. Funding is booked as an amount already computed by the caller
//! ([`FundingPayment`]); half-even at the base scale is this ledger's
//! policy for every value it computes.

use rust_decimal::Decimal;

use super::Position;
use crate::decimal::{
    Rounding, Wide, add, decimal_to_wide, mul_div_round, mul_div_round_wide, neg, wide_to_decimal,
};
use crate::direction::Dir;
use crate::error::{MarginError, Result};
use crate::ids::{Asset, ExecutionId, InstrumentId};
use crate::instrument::ContractSize;

/// The exact rest of an inverse value rounded to the settlement scale:
/// `n * CV / p = V + numerator / denominator`, exactly, for the booked
/// value `V` of `n` contracts at `p`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InverseResidual {
    /// `n * CV - V * p`, exact and signed: positive when the booked value
    /// is below the exact quotient. Zero only when the quotient is exact.
    pub numerator: Decimal,
    /// The price `p` of the quotient, positive.
    pub denominator: Decimal,
}

/// Derivative details of a booked execution
/// ([`ExecutionCashflow::derivative`](crate::ledger::ExecutionCashflow::derivative)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivativeFill {
    /// Contract size: the multiplier of a linear contract, the contract
    /// value of an inverse one.
    pub size: ContractSize,
    /// The fill's virtual value in the settlement asset: `rhe(p * n * M)`
    /// for a linear contract, `rhe(n * CV / p)` for an inverse one. It is
    /// the exposure the fill trades, never exchanged in cash. `None` only
    /// when it is out of a `Decimal`'s range; it is then still exact in the
    /// booking, and can be rebuilt in wide integers from the execution and
    /// the instrument.
    pub virtual_notional: Option<Decimal>,
    /// The closing part (`rhe(p * abs(Q) * M)`, or `rhe(abs(Q) * CV / p)`
    /// for an inverse contract) of a fill that crosses zero, or, for a late
    /// fill on a settled future ([`DerivativeFill::closed_at_settlement`]),
    /// its close at the settlement price `P` (`rhe(P * n * M)` or
    /// `rhe(n * CV / P)`). `None` when the fill does not cross zero and is
    /// not late, or (as above) only when it is out of a `Decimal`'s range.
    pub closing_notional: Option<Decimal>,
    /// For an inverse contract, the exact rest of `virtual_notional`;
    /// `None` for a linear one, whose value is an exact product.
    pub residual: Option<InverseResidual>,
    /// For an inverse contract, the exact rest of `closing_notional` when
    /// there is one; `None` otherwise.
    pub closing_residual: Option<InverseResidual>,
    /// Basis released by the fill, signed like the basis before the fill
    /// (positive for a long): the whole basis on a full close or a crossing,
    /// zero when the fill opens or increases, and zero for a late fill on a
    /// settled future, which never stores a basis.
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

/// The virtual value of `contracts` at `price`, rounded once to `scale`:
/// `rhe(price * contracts * M)` or `rhe(contracts * CV / price)`, signed
/// like `contracts`, as a wide integer.
pub(crate) fn value_wide(
    size: ContractSize,
    price: Decimal,
    contracts: Decimal,
    scale: u32,
) -> Result<Wide> {
    match size {
        ContractSize::Multiplier(multiplier) => mul_div_round_wide(
            &[price, contracts, multiplier],
            &[],
            scale,
            DERIVATIVE_ROUNDING,
            "price * contracts * multiplier",
        ),
        ContractSize::ContractValue(contract_value) => mul_div_round_wide(
            &[contracts, contract_value],
            &[price],
            scale,
            DERIVATIVE_ROUNDING,
            "contracts * contract value / price",
        ),
    }
}

/// The exact rest of the inverse value `value` (at `scale`) of `contracts`
/// at `price`; `None` for a linear contract.
pub(crate) fn residual(
    size: ContractSize,
    price: Decimal,
    contracts: Decimal,
    value: Wide,
    scale: u32,
) -> Result<Option<InverseResidual>> {
    let ContractSize::ContractValue(contract_value) = size else {
        return Ok(None);
    };
    let error = || MarginError::arithmetic("inverse residual");
    let (n, n_scale) = Wide::from_decimal(contracts.normalize());
    let (cv, cv_scale) = Wide::from_decimal(contract_value.normalize());
    let (p, p_scale) = Wide::from_decimal(price.normalize());
    // n * CV and V * p, exact, aligned to one scale.
    let exact = n.checked_mul(cv).ok_or_else(error)?;
    let exact_scale = n_scale.checked_add(cv_scale).ok_or_else(error)?;
    let booked = value.checked_mul(p).ok_or_else(error)?;
    let booked_scale = scale.checked_add(p_scale).ok_or_else(error)?;
    let common = exact_scale.max(booked_scale);
    let exact = exact.align(exact_scale, common).ok_or_else(error)?;
    let booked = booked.align(booked_scale, common).ok_or_else(error)?;
    let numerator = add_wide(exact, booked.negated(), "inverse residual")?;
    Ok(Some(InverseResidual {
        numerator: wide_to_decimal(numerator, common, "inverse residual")?,
        denominator: price,
    }))
}

/// The realized PnL of an inverse contract is the opposite of the linear
/// formula applied to its values.
fn signed_pnl(size: ContractSize, linear: Wide) -> Wide {
    match size {
        ContractSize::Multiplier(_) => linear,
        ContractSize::ContractValue(_) => linear.negated(),
    }
}

/// Applies a fill of `contracts > 0` at `price` in direction `dir` to
/// `position`, with contract size `size` and settlement scale `scale`.
/// Only the stored results must fit a `Decimal`.
pub(crate) fn apply_derivative_fill(
    position: Position,
    dir: Dir,
    price: Decimal,
    contracts: Decimal,
    size: ContractSize,
    scale: u32,
) -> Result<FillEffect> {
    let held = position.quantity;
    let basis = position.cost_basis;
    let notional = value_wide(size, price, contracts, scale)?;
    let fill_residual = residual(size, price, contracts, notional, scale)?;
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
                size,
                virtual_notional,
                closing_notional: None,
                residual: fill_residual,
                closing_residual: None,
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
        // Linear: realized = -signed_value - released; inverse: the
        // opposite.
        let released_w = decimal_to_wide(released, scale, "released basis")?;
        let linear = add_wide(signed_value.negated(), released_w.negated(), "realized pnl")?;
        let realized = wide_to_decimal(signed_pnl(size, linear), scale, "realized pnl")?;
        return Ok(FillEffect {
            position: Position {
                quantity,
                cost_basis: crate::decimal::sub(basis, released, "basis - released")?,
                realized_pnl: add(position.realized_pnl, realized, "realized pnl")?,
            },
            realized,
            fill: DerivativeFill {
                size,
                virtual_notional,
                closing_notional: None,
                residual: fill_residual,
                closing_residual: None,
                released_basis: released,
                closed_at_settlement: None,
            },
        });
    }

    // Cross zero: close abs(Q) (one rounding), open the rest as the exact
    // difference, so the fill as a whole keeps its single rounding.
    let closing = value_wide(size, price, held_abs, scale)?;
    let closing_residual = residual(size, price, held_abs, closing, scale)?;
    let signed_close = if signed_value.negative {
        closing.negated()
    } else {
        closing
    };
    let opening = add_wide(signed_value, signed_close.negated(), "opening notional")?;
    let basis_w = decimal_to_wide(basis, scale, "basis")?;
    // Linear: realized = -signed_close - basis; inverse: the opposite.
    let linear = add_wide(signed_close.negated(), basis_w.negated(), "realized pnl")?;
    let realized = wide_to_decimal(signed_pnl(size, linear), scale, "realized pnl")?;
    Ok(FillEffect {
        position: Position {
            quantity,
            cost_basis: wide_to_decimal(opening, scale, "basis")?,
            realized_pnl: add(position.realized_pnl, realized, "realized pnl")?,
        },
        realized,
        fill: DerivativeFill {
            size,
            virtual_notional,
            closing_notional: closing.to_decimal(scale),
            residual: fill_residual,
            closing_residual,
            released_basis: basis,
            closed_at_settlement: None,
        },
    })
}

/// A late fill of `contracts` at `price` on a flat, settled future, closed
/// at once at `settlement_price`. Linear: realized `= rhe(P * n * M) -
/// rhe(p * n * M)` for a buy and the opposite for a sell. Inverse: realized
/// `= rhe(n * CV / p) - rhe(n * CV / P)` for a buy (a long) and the
/// opposite for a sell. Computed in wide integers, so that only the
/// realized PnL must fit a `Decimal`. The position stays flat.
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
    size: ContractSize,
    (settlement_price, scale): (Decimal, u32),
) -> Result<FillEffect> {
    if !position.quantity.is_zero() {
        return Err(crate::error::MarginError::invalid_input(
            "position",
            "a settled future has no open position",
        ));
    }
    let traded = value_wide(size, price, contracts, scale)?;
    let settled = value_wide(size, settlement_price, contracts, scale)?;
    let linear = match dir {
        Dir::Buy => add_wide(settled, traded.negated(), "realized pnl")?,
        Dir::Sell => add_wide(traded, settled.negated(), "realized pnl")?,
    };
    let realized = wide_to_decimal(signed_pnl(size, linear), scale, "realized pnl")?;
    Ok(FillEffect {
        position: Position {
            quantity: position.quantity,
            cost_basis: position.cost_basis,
            realized_pnl: add(position.realized_pnl, realized, "realized pnl")?,
        },
        realized,
        fill: DerivativeFill {
            size,
            virtual_notional: traded.to_decimal(scale),
            closing_notional: settled.to_decimal(scale),
            residual: residual(size, price, contracts, traded, scale)?,
            closing_residual: residual(size, settlement_price, contracts, settled, scale)?,
            released_basis: Decimal::ZERO,
            closed_at_settlement: Some(settlement_price),
        },
    })
}

/// The virtual value of a fill signed as the cash a linear fill would
/// exchange (negative for a buy), as a wide integer at `scale`: the term of
/// the virtual reconciliation identity, `sum == realized_pnl - basis` for a
/// linear contract and `sum == -realized_pnl - basis` for an inverse one.
pub(crate) fn signed_virtual_cash(
    dir: Dir,
    price: Decimal,
    contracts: Decimal,
    size: ContractSize,
    scale: u32,
) -> Result<Wide> {
    let notional = value_wide(size, price, contracts, scale)?;
    Ok(match dir {
        Dir::Buy => notional.negated(),
        Dir::Sell => notional,
    })
}

/// The market value of a derivative position at `mark`, signed like the
/// contracts: `rhe(quantity * M * mark)` for a linear contract,
/// `rhe(quantity * CV / mark)` (its value in the base) for an inverse one.
pub(crate) fn market_value(
    quantity: Decimal,
    size: ContractSize,
    mark: Decimal,
    scale: u32,
) -> Result<Decimal> {
    match size {
        ContractSize::Multiplier(multiplier) => mul_div_round(
            &[quantity, multiplier, mark],
            &[],
            scale,
            DERIVATIVE_ROUNDING,
            "contracts * multiplier * mark",
        ),
        ContractSize::ContractValue(contract_value) => mul_div_round(
            &[quantity, contract_value],
            &[mark],
            scale,
            DERIVATIVE_ROUNDING,
            "contracts * contract value / mark",
        ),
    }
}

/// The unrealized PnL of a position with `basis` and `market_value`:
/// `market_value - basis` for a linear contract, `basis - market_value`
/// for an inverse one (a long gains when its value in the base falls).
pub(crate) fn unrealized_pnl(
    size: ContractSize,
    market_value: Decimal,
    basis: Decimal,
) -> Result<Decimal> {
    match size {
        ContractSize::Multiplier(_) => crate::decimal::sub(market_value, basis, "value - basis"),
        ContractSize::ContractValue(_) => crate::decimal::sub(basis, market_value, "basis - value"),
    }
}

pub(crate) fn add_wide(a: Wide, b: Wide, operation: &'static str) -> Result<Wide> {
    a.checked_add(b)
        .ok_or(crate::error::MarginError::arithmetic(operation))
}
