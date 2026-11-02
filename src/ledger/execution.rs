//! Executions submitted to the ledger and the cashflows they produce.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::ids::{Asset, ExecutionId, InstrumentId};
use crate::ledger::derivative::DerivativeFill;
use crate::ledger::snapshot::PositionSnapshot;

/// Direction of an execution from the ledger owner's point of view:
/// [`Action::Buy`] receives the base asset and pays the quote asset,
/// [`Action::Sell`] delivers the base asset and receives the quote asset.
/// Any other action is rejected with
/// [`MarginError::UnsupportedAction`](crate::MarginError::UnsupportedAction).
pub use financial_types::Action;

/// Fee charged by one execution.
///
/// A positive `amount` is a cost and is debited from `asset`; a negative
/// `amount` is a rebate and is credited to it. The fee asset may be the
/// base, the quote or any other registered asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fee {
    /// Asset the fee is charged in.
    pub asset: Asset,
    /// Fee amount; negative for a rebate.
    pub amount: Decimal,
}

/// One fill reported by a venue or a simulator.
///
/// Executions are booked idempotently by [`Execution::id`]: booking the
/// same execution twice is a no-op, and booking a different execution under
/// an identifier already used is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    /// Identifier, unique within the ledger.
    pub id: ExecutionId,
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Direction of the fill.
    pub side: Action,
    /// Execution price, in quote asset per unit of base asset. Positive.
    pub price: Decimal,
    /// Executed quantity, in base asset units. Positive.
    pub quantity: Decimal,
    /// Fee charged by this fill, if any.
    pub fee: Option<Fee>,
}

impl Execution {
    /// Builds an execution without fee.
    #[must_use]
    pub fn new(
        id: ExecutionId,
        instrument: InstrumentId,
        side: Action,
        price: Decimal,
        quantity: Decimal,
    ) -> Self {
        Self {
            id,
            instrument,
            side,
            price,
            quantity,
            fee: None,
        }
    }

    /// Returns the execution with a fee of `amount` in `asset`.
    #[must_use]
    pub fn with_fee(mut self, asset: Asset, amount: Decimal) -> Self {
        self.fee = Some(Fee { asset, amount });
        self
    }
}

/// Cashflow reconciliation of one booked execution.
///
/// It states exactly how the execution changed the ledger: the balance
/// delta per asset, the notional before and after rounding, the realized
/// PnL and the resulting position. Summing the `deltas` of every booked
/// execution plus the transfers reproduces the balances; see
/// [`Ledger::reconcile`](crate::ledger::Ledger::reconcile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionCashflow {
    /// Execution identifier.
    pub execution_id: ExecutionId,
    /// Position of the execution in the booking order, starting at 0.
    pub sequence: u64,
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Direction of the fill.
    pub side: Action,
    /// Execution price.
    pub price: Decimal,
    /// Executed quantity.
    pub quantity: Decimal,
    /// `price * quantity`, computed exactly.
    ///
    /// `None` when the exact product needs more than 28 fractional digits
    /// (for example price and quantity scales of 18 each); `notional` is
    /// still the exact product rounded once. `None` for a derivative.
    pub notional_exact: Option<Decimal>,
    /// Notional rounded to the quote asset scale; the amount of quote asset
    /// actually exchanged. Zero for a derivative, which exchanges no
    /// notional: its cash is in `deltas` and its virtual notional in
    /// [`ExecutionCashflow::derivative`].
    pub notional: Decimal,
    /// `notional - notional_exact`, when `notional_exact` is known.
    pub rounding_residual: Option<Decimal>,
    /// Fee charged, if any.
    pub fee: Option<Fee>,
    /// Signed change of the total balance of each asset touched.
    pub deltas: BTreeMap<Asset, Decimal>,
    /// Realized PnL of this execution, in the instrument's quote asset,
    /// gross of fees.
    pub realized_pnl: Decimal,
    /// Position after the execution.
    pub position_after: PositionSnapshot,
    /// Details of a linear future or perpetual fill; `None` for spot.
    pub derivative: Option<DerivativeFill>,
}

/// Result of [`Ledger::book_execution`](crate::ledger::Ledger::book_execution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Booking {
    /// The execution was new and has been applied.
    Booked(ExecutionCashflow),
    /// The identical execution had already been booked; nothing changed.
    /// Carries the cashflow recorded the first time.
    Duplicate(ExecutionCashflow),
}

impl Booking {
    /// The cashflow of the execution, whether new or duplicate.
    #[must_use]
    pub fn cashflow(&self) -> &ExecutionCashflow {
        match self {
            Self::Booked(cashflow) | Self::Duplicate(cashflow) => cashflow,
        }
    }

    /// True when the call changed the ledger.
    #[must_use]
    pub fn is_new(&self) -> bool {
        matches!(self, Self::Booked(_))
    }
}
