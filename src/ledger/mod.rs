//! Single accounting authority: cash, positions and fees.
//!
//! A [`Ledger`] holds, for one account:
//!
//! - a balance per asset, split into reserved and available;
//! - a position per instrument, with signed cost basis and realized PnL;
//! - the fees paid per asset;
//! - the journal of booked executions, each with its cashflow
//!   reconciliation ([`ExecutionCashflow`]);
//! - the cash settlements of derivative fills held outside the ledger
//!   ([`DerivativeSettlement`]), with the realized PnL they paid per asset.
//!
//! Strategy, risk, backtest and environment code read this state instead of
//! keeping their own balances, so there is one place where cash and
//! positions change.
//!
//! # Booking rules
//!
//! - Executions are booked idempotently by [`ExecutionId`]; transfers by
//!   [`TransferId`]. Repeating an identical entry changes nothing; reusing an
//!   identifier for different content is a [`MarginError::ConflictingEntry`].
//! - Every operation is atomic: on error the ledger is left unchanged.
//! - A booking that would leave an asset's available balance negative is
//!   rejected with [`MarginError::InsufficientBalance`], except a real
//!   execution booked with [`Ledger::book_execution_allowing_overdraft`]
//!   and a derivative settlement ([`Ledger::book_settlement`]), which are
//!   mandatory and report the shortfall instead.
//! - Amounts are exact decimals. Inputs with more fractional digits than
//!   allowed are rejected, never truncated. The only rounding steps are
//!   the notional (`price * quantity` to the quote asset scale) and the cost
//!   released when a position is partly reduced; both use the configured
//!   [`Rounding`] and are recorded so that totals stay exact.
//!
//! # Example
//!
//! ```
//! use margin_rs::ledger::{Execution, Ledger, Action};
//! use margin_rs::{Asset, ExecutionId, InstrumentId, SpotInstrument, TransferId};
//! use rust_decimal_macros::dec;
//!
//! # fn main() -> margin_rs::Result<()> {
//! let usdc = Asset::new("USDC")?;
//! let btc = Asset::new("BTC")?;
//! let pair = InstrumentId::new("BTC-USDC")?;
//!
//! let mut ledger = Ledger::default();
//! ledger.register_asset(usdc.clone(), 6)?;
//! ledger.register_asset(btc.clone(), 8)?;
//! ledger.register_instrument(SpotInstrument::new(pair.clone(), btc.clone(), usdc.clone(), 2, 8)?)?;
//! ledger.deposit(TransferId::new("d1")?, usdc.clone(), dec!(1000))?;
//!
//! let buy = Execution::new(ExecutionId::new("e1")?, pair.clone(), Action::Buy, dec!(100), dec!(2))
//!     .with_fee(usdc.clone(), dec!(0.2));
//! assert!(ledger.book_execution(&buy)?.is_new());
//! assert!(!ledger.book_execution(&buy)?.is_new()); // duplicate: no effect
//!
//! assert_eq!(ledger.balance(&usdc).map(|b| b.total), Some(dec!(799.8)));
//! assert_eq!(ledger.balance(&btc).map(|b| b.total), Some(dec!(2)));
//! assert_eq!(ledger.position(&pair).map(|p| p.cost_basis), Some(dec!(200)));
//! ledger.reconcile()?;
//! # Ok(())
//! # }
//! ```

pub(crate) mod derivative;
mod execution;
mod settlement;
mod snapshot;
mod valuation;

use std::collections::BTreeMap;

use rust_decimal::Decimal;

pub use derivative::{
    DerivativeFill, ExpiryCashflow, ExpirySettlement, FundingCashflow, FundingPayment,
    InverseResidual, PostingBooking,
};
pub use execution::{Action, Booking, Execution, ExecutionCashflow, Fee};
pub use settlement::{DerivativeSettlement, SettlementBooking, SettlementCashflow};

pub use snapshot::{BalanceSnapshot, LedgerSnapshot, PositionSnapshot};
pub use valuation::{AssetPnl, PositionValuation, Valuation};

use crate::decimal::{
    self, Rounding, Wide, add, check_configured_scale, check_scale, decimal_to_wide, mul,
    mul_div_round, neg, sub,
};
use crate::direction::Dir;
use crate::error::{MarginError, Result};
use crate::ids::{Asset, ExecutionId, InstrumentId, ReservationId, TransferId};
use crate::instrument::{
    ContractSize, DECIMAL_DIGITS, DerivativeContract, Instrument, PositionKind,
};

/// Result of [`Ledger::book_execution_allowing_overdraft`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverdraftBooking {
    /// The booking, new or duplicate.
    pub booking: Booking,
    /// Assets this booking left below their reservations, with the
    /// shortfall it created. Empty when the balance covered it.
    pub shortfalls: BTreeMap<Asset, Decimal>,
}

/// Result of a successful [`Ledger::reconcile`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reconciliation {
    /// Assets below their reservations, with the shortfall. See
    /// [`Ledger::overdrafts`].
    pub overdrafts: BTreeMap<Asset, Decimal>,
}

impl Reconciliation {
    /// True when no asset is overdrawn.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.overdrafts.is_empty()
    }
}

/// Ledger configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LedgerConfig {
    /// Rounding mode for notionals and released cost basis.
    pub rounding: Rounding,
}

/// Direction of a [`Transfer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferKind {
    /// Funds enter the account.
    Deposit,
    /// Funds leave the account.
    Withdrawal,
}

/// A deposit or withdrawal of one asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// Identifier, unique within the ledger.
    pub id: TransferId,
    /// Deposit or withdrawal.
    pub kind: TransferKind,
    /// Asset moved.
    pub asset: Asset,
    /// Amount moved; always positive.
    pub amount: Decimal,
}

impl Transfer {
    fn signed_amount(&self) -> Decimal {
        match self.kind {
            TransferKind::Deposit => self.amount,
            TransferKind::Withdrawal => neg(self.amount),
        }
    }
}

/// A balance. `available = total - reserved` is computed exactly when the
/// balance changes, so reading it never does arithmetic.
#[derive(Debug, Clone, Copy, Default)]
struct Balance {
    total: Decimal,
    reserved: Decimal,
    available: Decimal,
}

impl Balance {
    fn new(total: Decimal, reserved: Decimal) -> Result<Self> {
        Ok(Self {
            total,
            reserved,
            available: sub(total, reserved, "total - reserved")?,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Position {
    pub(crate) quantity: Decimal,
    pub(crate) cost_basis: Decimal,
    pub(crate) realized_pnl: Decimal,
}

/// The accounting authority for one account. See the [module docs](self).
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    config: LedgerConfig,
    assets: BTreeMap<Asset, u32>,
    instruments: BTreeMap<InstrumentId, Instrument>,
    balances: BTreeMap<Asset, Balance>,
    positions: BTreeMap<InstrumentId, Position>,
    fees_paid: BTreeMap<Asset, Decimal>,
    reservations: BTreeMap<ReservationId, (Asset, Decimal)>,
    transfers: BTreeMap<TransferId, Transfer>,
    net_transfers: BTreeMap<Asset, Decimal>,
    executions: BTreeMap<ExecutionId, (Execution, usize)>,
    journal: Vec<ExecutionCashflow>,
    settlements: BTreeMap<ExecutionId, (DerivativeSettlement, usize)>,
    settlement_journal: Vec<SettlementCashflow>,
    settled_pnl: BTreeMap<Asset, Decimal>,
    fundings: BTreeMap<ExecutionId, (FundingPayment, usize)>,
    funding_journal: Vec<FundingCashflow>,
    funding: BTreeMap<Asset, Decimal>,
    expiries: BTreeMap<ExecutionId, (ExpirySettlement, usize)>,
    expiry_journal: Vec<ExpiryCashflow>,
    settled_instruments: BTreeMap<InstrumentId, ExecutionId>,
}

impl Ledger {
    /// Creates an empty ledger.
    #[must_use]
    pub fn new(config: LedgerConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> LedgerConfig {
        self.config
    }

    // ----- registration -------------------------------------------------

    /// Registers `asset` with `scale` fractional digits.
    ///
    /// Registering the same asset again with the same scale is a no-op.
    ///
    /// # Errors
    ///
    /// - [`MarginError::InvalidInput`] if `scale` exceeds
    ///   [`MAX_SCALE`](crate::decimal::MAX_SCALE).
    /// - [`MarginError::AlreadyRegistered`] if the asset exists with another
    ///   scale.
    pub fn register_asset(&mut self, asset: Asset, scale: u32) -> Result<()> {
        check_configured_scale(scale, "asset_scale")?;
        match self.assets.get(&asset) {
            Some(existing) if *existing == scale => Ok(()),
            Some(_) => Err(MarginError::AlreadyRegistered {
                kind: "asset",
                id: asset.to_string(),
            }),
            None => {
                self.assets.insert(asset.clone(), scale);
                self.balances.insert(asset.clone(), Balance::default());
                self.net_transfers.insert(asset.clone(), Decimal::ZERO);
                self.fees_paid.insert(asset.clone(), Decimal::ZERO);
                self.settled_pnl.insert(asset.clone(), Decimal::ZERO);
                self.funding.insert(asset, Decimal::ZERO);
                Ok(())
            }
        }
    }

    /// Registers an instrument. Its assets must be registered first.
    ///
    /// Registering an identical instrument again is a no-op. A future or
    /// perpetual (linear or inverse) is held as signed contracts with a basis in its
    /// settlement asset; every figure it books is rounded half-even to that
    /// asset's scale, whatever [`LedgerConfig::rounding`] says (which
    /// applies to spot only), so the ledger and
    /// [`plan_liquidation`](crate::liquidation::plan_liquidation) agree to
    /// the unit.
    ///
    /// # Errors
    ///
    /// - [`MarginError::UnknownAsset`] if the base, quote or settlement asset
    ///   is not registered.
    /// - [`MarginError::InvalidInput`] if the quantity scale exceeds the base
    ///   asset scale.
    /// - [`MarginError::AlreadyRegistered`] if a different instrument uses the
    ///   same identifier, or a derivative settlement was booked under it.
    pub fn register_instrument(&mut self, instrument: impl Into<Instrument>) -> Result<()> {
        let instrument = instrument.into();
        match &instrument {
            Instrument::Spot(spot) => {
                let base_scale = self.asset_scale_or_err(spot.base())?;
                self.asset_scale_or_err(spot.quote())?;
                if spot.quantity_scale() > base_scale {
                    return Err(MarginError::invalid_input(
                        "quantity_scale",
                        "must not exceed the base asset scale",
                    ));
                }
            }
            Instrument::LinearFuture(future) => {
                self.asset_scale_or_err(future.terms().settlement())?;
            }
            Instrument::LinearPerpetual(perpetual) => {
                self.asset_scale_or_err(perpetual.terms().settlement())?;
            }
            Instrument::InverseFuture(_) | Instrument::InversePerpetual(_) => {
                let terms = instrument
                    .inverse_terms()
                    .ok_or(MarginError::arithmetic("inverse terms"))?;
                let scale = self.asset_scale_or_err(terms.settlement())?;
                // Every value's residual `n * CV - V * p` is then an exact
                // Decimal.
                if scale.saturating_add(terms.price_scale()) > DECIMAL_DIGITS {
                    return Err(MarginError::invalid_input(
                        "price_scale",
                        "the settlement asset scale plus price_scale must not exceed 28",
                    ));
                }
            }
        }
        let id = instrument.id().clone();
        if self.settlement_journal.iter().any(|s| s.instrument == id) {
            return Err(MarginError::AlreadyRegistered {
                kind: "derivative instrument",
                id: id.to_string(),
            });
        }
        match self.instruments.get(&id) {
            Some(existing) if *existing == instrument => Ok(()),
            Some(_) => Err(MarginError::AlreadyRegistered {
                kind: "instrument",
                id: id.to_string(),
            }),
            None => {
                self.instruments.insert(id.clone(), instrument);
                self.positions.insert(id, Position::default());
                Ok(())
            }
        }
    }

    /// Scale of a registered asset.
    #[must_use]
    pub fn asset_scale(&self, asset: &Asset) -> Option<u32> {
        self.assets.get(asset).copied()
    }

    /// A registered instrument.
    #[must_use]
    pub fn instrument(&self, id: &InstrumentId) -> Option<&Instrument> {
        self.instruments.get(id)
    }

    fn asset_scale_or_err(&self, asset: &Asset) -> Result<u32> {
        self.asset_scale(asset)
            .ok_or_else(|| MarginError::UnknownAsset {
                asset: asset.clone(),
            })
    }

    fn validated_amount(&self, asset: &Asset, amount: Decimal, field: &'static str) -> Result<()> {
        let scale = self.asset_scale_or_err(asset)?;
        if amount <= Decimal::ZERO {
            return Err(MarginError::invalid_input(field, "must be positive"));
        }
        check_scale(amount, scale, field)
    }

    fn balance_of(&self, asset: &Asset) -> Result<Balance> {
        self.balances
            .get(asset)
            .copied()
            .ok_or_else(|| MarginError::UnknownAsset {
                asset: asset.clone(),
            })
    }

    // ----- transfers ----------------------------------------------------

    /// Deposits `amount` of `asset`. Returns `false` if this exact transfer
    /// was already applied.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownAsset`], [`MarginError::InvalidInput`] (amount
    /// not positive), [`MarginError::PrecisionExceeded`],
    /// [`MarginError::ConflictingEntry`] or [`MarginError::Arithmetic`].
    pub fn deposit(&mut self, id: TransferId, asset: Asset, amount: Decimal) -> Result<bool> {
        self.transfer(Transfer {
            id,
            kind: TransferKind::Deposit,
            asset,
            amount,
        })
    }

    /// Withdraws `amount` of `asset` from the available balance. Returns
    /// `false` if this exact transfer was already applied.
    ///
    /// # Errors
    ///
    /// As [`Ledger::deposit`], plus [`MarginError::InsufficientBalance`].
    pub fn withdraw(&mut self, id: TransferId, asset: Asset, amount: Decimal) -> Result<bool> {
        self.transfer(Transfer {
            id,
            kind: TransferKind::Withdrawal,
            asset,
            amount,
        })
    }

    fn transfer(&mut self, transfer: Transfer) -> Result<bool> {
        if let Some(existing) = self.transfers.get(&transfer.id) {
            if *existing == transfer {
                return Ok(false);
            }
            return Err(MarginError::ConflictingEntry {
                kind: "transfer",
                id: transfer.id.to_string(),
            });
        }
        self.validated_amount(&transfer.asset, transfer.amount, "amount")?;
        let balance = self.balance_of(&transfer.asset)?;
        let total = add(
            balance.total,
            transfer.signed_amount(),
            "balance + transfer",
        )?;
        if transfer.kind == TransferKind::Withdrawal {
            let available = balance.available;
            if transfer.amount > available {
                return Err(MarginError::InsufficientBalance {
                    asset: transfer.asset,
                    required: transfer.amount,
                    available,
                });
            }
        }
        let net = self
            .net_transfers
            .get(&transfer.asset)
            .copied()
            .unwrap_or_default();
        let net = add(net, transfer.signed_amount(), "net transfers")?;
        self.balances.insert(
            transfer.asset.clone(),
            Balance::new(total, balance.reserved)?,
        );
        self.net_transfers.insert(transfer.asset.clone(), net);
        self.transfers.insert(transfer.id.clone(), transfer);
        Ok(true)
    }

    // ----- reservations -------------------------------------------------

    /// Holds back `amount` of `asset` from the available balance.
    ///
    /// Returns `false` if an identical reservation is already open. Once a
    /// reservation is released its identifier may be used again.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownAsset`], [`MarginError::InvalidInput`],
    /// [`MarginError::PrecisionExceeded`], [`MarginError::ConflictingEntry`]
    /// (an open reservation with the same id differs) or
    /// [`MarginError::InsufficientBalance`].
    pub fn reserve(&mut self, id: ReservationId, asset: Asset, amount: Decimal) -> Result<bool> {
        if let Some((existing_asset, existing_amount)) = self.reservations.get(&id) {
            if *existing_asset == asset && *existing_amount == amount {
                return Ok(false);
            }
            return Err(MarginError::ConflictingEntry {
                kind: "reservation",
                id: id.to_string(),
            });
        }
        self.validated_amount(&asset, amount, "amount")?;
        let balance = self.balance_of(&asset)?;
        let available = balance.available;
        if amount > available {
            return Err(MarginError::InsufficientBalance {
                asset,
                required: amount,
                available,
            });
        }
        let reserved = add(balance.reserved, amount, "reserved + amount")?;
        self.balances
            .insert(asset.clone(), Balance::new(balance.total, reserved)?);
        self.reservations.insert(id, (asset, amount));
        Ok(true)
    }

    /// Releases an open reservation and returns its asset and amount.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownReservation`] if no reservation with this id is
    /// open.
    pub fn release(&mut self, id: &ReservationId) -> Result<(Asset, Decimal)> {
        self.release_inner(id)
    }

    /// Sets reservation `id` to exactly `amount` of `asset` in one atomic
    /// step, creating, resizing, moving or (with zero) removing it.
    ///
    /// Only an increase needs cover: the asset's total must stay at or
    /// above its reservations. Shrinking or removing a reservation always
    /// succeeds, even on an overdrawn balance. On error nothing changes.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownAsset`], [`MarginError::InvalidInput`]
    /// (negative amount), [`MarginError::PrecisionExceeded`],
    /// [`MarginError::InsufficientBalance`] for an increase that is not
    /// covered, or [`MarginError::Arithmetic`].
    pub fn set_reservation(
        &mut self,
        id: &ReservationId,
        asset: Asset,
        amount: Decimal,
    ) -> Result<()> {
        let scale = self.asset_scale_or_err(&asset)?;
        if amount < Decimal::ZERO {
            return Err(MarginError::invalid_input("amount", "must not be negative"));
        }
        check_scale(amount, scale, "amount")?;
        let previous = self.reservations.get(id).cloned();
        // New reserved totals of the affected assets, computed first.
        let mut updates: BTreeMap<Asset, Balance> = BTreeMap::new();
        if let Some((old_asset, old_amount)) = &previous {
            let balance = self.balance_of(old_asset)?;
            updates.insert(
                old_asset.clone(),
                Balance::new(
                    balance.total,
                    sub(balance.reserved, *old_amount, "reserved - previous")?,
                )?,
            );
        }
        let current = match updates.get(&asset) {
            Some(balance) => *balance,
            None => self.balance_of(&asset)?,
        };
        let reserved = add(current.reserved, amount, "reserved + amount")?;
        let before = self.balance_of(&asset)?;
        if reserved > before.reserved && current.total < reserved {
            return Err(MarginError::InsufficientBalance {
                asset,
                required: amount,
                available: sub(current.total, current.reserved, "total - reserved")?,
            });
        }
        updates.insert(asset.clone(), Balance::new(current.total, reserved)?);
        // Commit.
        for (asset, balance) in updates {
            self.balances.insert(asset, balance);
        }
        if amount.is_zero() {
            self.reservations.remove(id);
        } else {
            self.reservations.insert(id.clone(), (asset, amount));
        }
        Ok(())
    }

    /// An open reservation's asset and amount.
    #[must_use]
    pub fn reservation(&self, id: &ReservationId) -> Option<(&Asset, Decimal)> {
        self.reservations
            .get(id)
            .map(|(asset, amount)| (asset, *amount))
    }

    fn release_inner(&mut self, id: &ReservationId) -> Result<(Asset, Decimal)> {
        let (asset, amount) = self
            .reservations
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownReservation { id: id.to_string() })?;
        let balance = self.balance_of(&asset)?;
        let reserved = sub(balance.reserved, amount, "reserved - amount")?;
        self.balances
            .insert(asset.clone(), Balance::new(balance.total, reserved)?);
        self.reservations.remove(id);
        Ok((asset, amount))
    }

    // ----- executions ---------------------------------------------------

    /// Books one execution.
    ///
    /// Returns [`Booking::Booked`] with the cashflow of a new execution, or
    /// [`Booking::Duplicate`] with the original cashflow if this exact
    /// execution was booked before (nothing changes).
    ///
    /// # Errors
    ///
    /// The ledger is unchanged on any error.
    ///
    /// - [`MarginError::UnsupportedAction`]: the side is not `Buy` or `Sell`
    ///   (for example `Action::Other`), refused before anything else.
    /// - [`MarginError::ConflictingEntry`]: the id was booked with different
    ///   content.
    /// - [`MarginError::UnknownInstrument`] or [`MarginError::UnknownAsset`]
    ///   (fee asset).
    /// - [`MarginError::InvalidInput`]: price or quantity not positive.
    /// - [`MarginError::PrecisionExceeded`]: price, quantity or fee has more
    ///   fractional digits than the instrument or asset allows.
    /// - [`MarginError::InsufficientBalance`]: an asset debited by the
    ///   execution would end with a negative available balance.
    /// - [`MarginError::InvalidInput`]: the instrument is a future or
    ///   perpetual (linear or inverse). Their executions are mandatory and are booked with
    ///   [`Ledger::book_execution_allowing_overdraft`].
    /// - [`MarginError::Arithmetic`]: overflow or a non exact intermediate.
    pub fn book_execution(&mut self, execution: &Execution) -> Result<Booking> {
        self.book(execution, None, false)
            .map(|(booking, _)| booking)
    }

    /// Books one execution that consumes an open reservation, in one atomic
    /// step.
    ///
    /// The reservation `reservation` is shrunk to `remaining_reserved` (and
    /// removed when that is zero) and the execution is booked against the
    /// balance freed this way. A duplicate execution returns
    /// [`Booking::Duplicate`] and leaves the reservation untouched, so a
    /// repeated fill message adjusts the reservation exactly once.
    ///
    /// # Errors
    ///
    /// As [`Ledger::book_execution`], plus:
    ///
    /// - [`MarginError::UnknownReservation`] if the reservation is not open;
    /// - [`MarginError::InvalidInput`] if `remaining_reserved` is negative or
    ///   larger than the current reservation (a fill never grows it);
    /// - [`MarginError::PrecisionExceeded`] if `remaining_reserved` is finer
    ///   than the reserved asset scale.
    pub fn book_execution_with_reservation(
        &mut self,
        execution: &Execution,
        reservation: &ReservationId,
        remaining_reserved: Decimal,
    ) -> Result<Booking> {
        self.book(execution, Some((reservation, remaining_reserved)), false)
            .map(|(booking, _)| booking)
    }

    /// Books an execution that **already happened** at a venue, even if it
    /// overdraws the account. It is the authoritative path for executions of
    /// futures and perpetuals, linear or inverse, which are always mandatory (see
    /// "Derivative positions" in `docs/ledger.md`).
    ///
    /// Same rules as [`Ledger::book_execution_with_reservation`] (or
    /// [`Ledger::book_execution`] when `reservation` is `None`), except that
    /// a debit beyond the available balance is booked anyway: the balance
    /// goes below its reservations or below zero, and the shortfall per
    /// asset is returned. A real execution must never be lost; the caller
    /// is expected to halt trading on a shortfall. Overdrafts stay visible
    /// in [`Ledger::overdrafts`] and [`Ledger::reconcile`] until funds are
    /// deposited.
    ///
    /// # Errors
    ///
    /// Every error of [`Ledger::book_execution_with_reservation`] except
    /// [`MarginError::InsufficientBalance`]. For a future or perpetual
    /// (linear or inverse), after the duplicate and conflict checks:
    ///
    /// - [`MarginError::InvalidInput`]: a reservation is given (a derivative
    ///   fill consumes none), or the execution has no `executed_at_ms`.
    /// - [`MarginError::ExecutedAfterExpiry`]: a future's fill executed at or
    ///   after its expiry, whether or not the future was settled. A fill
    ///   executed before the expiry and delivered after the settlement is
    ///   booked and closed at the settlement price.
    /// - [`MarginError::InvalidInput`] or [`MarginError::PrecisionExceeded`]:
    ///   price, contracts or fee off their grids.
    /// - [`MarginError::Arithmetic`]: a stored result (realized PnL, basis,
    ///   cash or contracts) does not fit a `Decimal`. Intermediates such as
    ///   the virtual notional never fail on their own.
    pub fn book_execution_allowing_overdraft(
        &mut self,
        execution: &Execution,
        reservation: Option<(&ReservationId, Decimal)>,
    ) -> Result<OverdraftBooking> {
        self.book(execution, reservation, true)
            .map(|(booking, shortfalls)| OverdraftBooking {
                booking,
                shortfalls,
            })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the whole booking is one validate-then-commit sequence: every check runs before any balance changes, so it stays in one function"
    )]
    fn book(
        &mut self,
        execution: &Execution,
        reservation: Option<(&ReservationId, Decimal)>,
        allow_overdraft: bool,
    ) -> Result<(Booking, BTreeMap<Asset, Decimal>)> {
        // Refused before the duplicate check and before any change.
        let dir = Dir::from_action(execution.side, "execution side")?;
        if let Some((previous, index)) = self.executions.get(&execution.id) {
            if previous == execution {
                return Ok((
                    Booking::Duplicate(
                        self.journal
                            .get(*index)
                            .cloned()
                            .ok_or(MarginError::arithmetic("execution journal index"))?,
                    ),
                    BTreeMap::new(),
                ));
            }
            return Err(MarginError::ConflictingEntry {
                kind: "execution",
                id: execution.id.to_string(),
            });
        }
        if self.id_used_by_posting(&execution.id) {
            return Err(MarginError::ConflictingEntry {
                kind: "execution",
                id: execution.id.to_string(),
            });
        }

        let instrument = self.instruments.get(&execution.instrument).ok_or_else(|| {
            MarginError::UnknownInstrument {
                instrument: execution.instrument.clone(),
            }
        })?;
        let spot = match instrument {
            Instrument::Spot(spot) => spot.clone(),
            Instrument::LinearFuture(_)
            | Instrument::LinearPerpetual(_)
            | Instrument::InverseFuture(_)
            | Instrument::InversePerpetual(_) => {
                let contract = instrument
                    .derivative()
                    .ok_or(MarginError::arithmetic("derivative contract"))?;
                return self.book_derivative(
                    execution,
                    dir,
                    &contract,
                    reservation,
                    allow_overdraft,
                );
            }
        };

        if execution.price <= Decimal::ZERO {
            return Err(MarginError::invalid_input("price", "must be positive"));
        }
        if execution.quantity <= Decimal::ZERO {
            return Err(MarginError::invalid_input("quantity", "must be positive"));
        }
        check_scale(execution.price, spot.price_scale(), "price")?;
        check_scale(execution.quantity, spot.quantity_scale(), "quantity")?;
        if let Some(fee) = &execution.fee {
            let fee_scale = self.asset_scale_or_err(&fee.asset)?;
            check_scale(fee.amount, fee_scale, "fee")?;
        }

        let rounding = self.config.rounding;
        let quote_scale = self.asset_scale_or_err(spot.quote())?;
        let notional = mul_div_round(
            &[execution.price, execution.quantity],
            &[],
            quote_scale,
            rounding,
            "price * quantity",
        )?;
        // The exact product needs up to price_scale + quantity_scale digits;
        // it is recorded when a Decimal can hold it.
        let notional_exact = mul(execution.price, execution.quantity, "price * quantity").ok();
        let rounding_residual =
            notional_exact.and_then(|exact| sub(notional, exact, "notional - exact").ok());

        // Signed quantity and signed quote value (positive when buying).
        let (signed_quantity, signed_value) = match dir {
            Dir::Buy => (execution.quantity, notional),
            Dir::Sell => (neg(execution.quantity), neg(notional)),
        };

        // Changes per asset, each summed in one exact step (a fee in the
        // base or quote asset joins that asset's change).
        let mut changes: BTreeMap<Asset, Vec<Decimal>> = BTreeMap::new();
        changes
            .entry(spot.base().clone())
            .or_default()
            .push(signed_quantity);
        changes
            .entry(spot.quote().clone())
            .or_default()
            .push(neg(signed_value));
        if let Some(fee) = &execution.fee {
            changes
                .entry(fee.asset.clone())
                .or_default()
                .push(neg(fee.amount));
        }
        let mut deltas: BTreeMap<Asset, Decimal> = BTreeMap::new();
        for (asset, values) in changes {
            deltas.insert(asset, decimal::sum(&values, "delta accumulation")?);
        }

        // Reservation consumed by this execution: (id, asset, remaining,
        // released).
        let reservation_change = match reservation {
            Some((id, remaining)) => {
                let (asset, amount) = self
                    .reservations
                    .get(id)
                    .cloned()
                    .ok_or_else(|| MarginError::UnknownReservation { id: id.to_string() })?;
                if remaining < Decimal::ZERO || remaining > amount {
                    return Err(MarginError::invalid_input(
                        "remaining_reserved",
                        "must be between zero and the current reservation",
                    ));
                }
                check_scale(
                    remaining,
                    self.asset_scale_or_err(&asset)?,
                    "remaining_reserved",
                )?;
                let released = sub(amount, remaining, "reservation - remaining")?;
                Some((id.clone(), asset, remaining, released))
            }
            None => None,
        };

        let mut touched: Vec<&Asset> = deltas.keys().collect();
        if let Some((_, asset, _, _)) = &reservation_change
            && !deltas.contains_key(asset)
        {
            touched.push(asset);
        }
        let mut new_balances = Vec::with_capacity(touched.len());
        let mut shortfalls = BTreeMap::new();
        for asset in touched {
            let balance = self.balance_of(asset)?;
            let delta = deltas.get(asset).copied().unwrap_or_default();
            let reserved = match &reservation_change {
                Some((_, reserved_asset, _, released)) if reserved_asset == asset => {
                    sub(balance.reserved, *released, "reserved - released")?
                }
                _ => balance.reserved,
            };
            let total = add(balance.total, delta, "balance + delta")?;
            if delta < Decimal::ZERO {
                let available_after = sub(total, reserved, "total - reserved")?;
                if available_after < Decimal::ZERO && allow_overdraft {
                    shortfalls.insert(asset.clone(), neg(available_after));
                } else if available_after < Decimal::ZERO {
                    return Err(MarginError::InsufficientBalance {
                        asset: asset.clone(),
                        required: neg(delta),
                        available: sub(balance.total, reserved, "total - reserved")?,
                    });
                }
            }
            new_balances.push((asset.clone(), Balance::new(total, reserved)?));
        }

        let position = self
            .positions
            .get(&execution.instrument)
            .copied()
            .unwrap_or_default();
        let (new_position, realized_pnl) = apply_fill(
            position,
            signed_quantity,
            signed_value,
            execution.price,
            quote_scale,
            rounding,
        )?;

        let new_fee_total = match &execution.fee {
            Some(fee) => {
                let paid = self.fees_paid.get(&fee.asset).copied().unwrap_or_default();
                Some((fee.asset.clone(), add(paid, fee.amount, "fees_paid + fee")?))
            }
            None => None,
        };

        // Commit: nothing below can fail.
        for (asset, balance) in new_balances {
            self.balances.insert(asset, balance);
        }
        self.positions
            .insert(execution.instrument.clone(), new_position);
        if let Some((asset, total)) = new_fee_total {
            self.fees_paid.insert(asset, total);
        }
        if let Some((id, asset, remaining, _)) = reservation_change {
            if remaining.is_zero() {
                self.reservations.remove(&id);
            } else {
                self.reservations.insert(id, (asset, remaining));
            }
        }
        let index = self.journal.len();
        let cashflow = ExecutionCashflow {
            execution_id: execution.id.clone(),
            sequence: index as u64,
            instrument: execution.instrument.clone(),
            side: execution.side,
            price: execution.price,
            quantity: execution.quantity,
            notional_exact,
            notional,
            rounding_residual,
            fee: execution.fee.clone(),
            deltas,
            realized_pnl,
            position_after: position_snapshot(
                &execution.instrument,
                spot.quote(),
                PositionKind::Spot,
                ContractSize::Multiplier(Decimal::ONE),
                new_position,
            ),
            derivative: None,
        };
        self.journal.push(cashflow.clone());
        self.executions
            .insert(execution.id.clone(), (execution.clone(), index));
        Ok((Booking::Booked(cashflow), shortfalls))
    }

    /// True if `id` is already used by a settlement, a funding payment or an
    /// expiry settlement (the namespace shared with executions).
    fn id_used_by_posting(&self, id: &ExecutionId) -> bool {
        self.settlements.contains_key(id)
            || self.fundings.contains_key(id)
            || self.expiries.contains_key(id)
    }

    /// Books an execution of a linear or inverse future or perpetual (see
    /// the `derivative` module): mandatory, so a debit beyond the available
    /// balance is booked and reported as a shortfall.
    #[expect(
        clippy::too_many_lines,
        reason = "one validate-then-commit sequence, as for spot: every check runs before any balance changes"
    )]
    fn book_derivative(
        &mut self,
        execution: &Execution,
        dir: Dir,
        contract: &DerivativeContract,
        reservation: Option<(&ReservationId, Decimal)>,
        allow_overdraft: bool,
    ) -> Result<(Booking, BTreeMap<Asset, Decimal>)> {
        if !allow_overdraft {
            return Err(MarginError::invalid_input(
                "execution",
                "a derivative execution is mandatory: book it with book_execution_allowing_overdraft",
            ));
        }
        if reservation.is_some() {
            return Err(MarginError::invalid_input(
                "reservation",
                "a derivative execution does not consume a reservation",
            ));
        }
        // Every derivative fill carries its execution time (the ledger reads
        // no clock), and a future's must be before its expiry.
        let Some(executed_at_ms) = execution.executed_at_ms else {
            return Err(MarginError::invalid_input(
                "executed_at_ms",
                "a derivative execution needs its execution time",
            ));
        };
        if let Some(expiry_ms) = contract.expiry_ms
            && executed_at_ms >= expiry_ms
        {
            return Err(MarginError::ExecutedAfterExpiry {
                instrument: execution.instrument.clone(),
                executed_at_ms,
                expiry_ms,
            });
        }
        // A real fill executed before the expiry but delivered after the
        // future was settled is still booked: it is closed at once at the
        // settlement price.
        let settlement_price = match self.settled_instruments.get(&execution.instrument) {
            Some(id) => Some(
                self.expiries
                    .get(id)
                    .map(|(settlement, _)| settlement.price)
                    .ok_or(MarginError::arithmetic("expiry settlement index"))?,
            ),
            None => None,
        };
        if execution.price <= Decimal::ZERO {
            return Err(MarginError::invalid_input("price", "must be positive"));
        }
        if execution.quantity <= Decimal::ZERO {
            return Err(MarginError::invalid_input("quantity", "must be positive"));
        }
        check_scale(execution.price, contract.price_scale, "price")?;
        check_scale(execution.quantity, contract.contract_scale, "quantity")?;
        if let Some(fee) = &execution.fee {
            let fee_scale = self.asset_scale_or_err(&fee.asset)?;
            check_scale(fee.amount, fee_scale, "fee")?;
        }
        let settlement = &contract.settlement;
        let scale = self.asset_scale_or_err(settlement)?;
        let position = self
            .positions
            .get(&execution.instrument)
            .copied()
            .unwrap_or_default();
        let effect = match settlement_price {
            None => derivative::apply_derivative_fill(
                position,
                dir,
                execution.price,
                execution.quantity,
                contract.size,
                scale,
            )?,
            Some(price) => derivative::close_late_fill(
                position,
                dir,
                execution.price,
                execution.quantity,
                contract.size,
                (price, scale),
            )?,
        };

        // Only the realized PnL and the fee move cash.
        let mut changes: BTreeMap<Asset, Vec<Decimal>> = BTreeMap::new();
        changes
            .entry(settlement.clone())
            .or_default()
            .push(effect.realized);
        if let Some(fee) = &execution.fee {
            changes
                .entry(fee.asset.clone())
                .or_default()
                .push(neg(fee.amount));
        }
        let mut deltas: BTreeMap<Asset, Decimal> = BTreeMap::new();
        for (asset, values) in changes {
            deltas.insert(asset, decimal::sum(&values, "delta accumulation")?);
        }
        let mut new_balances = Vec::with_capacity(deltas.len());
        let mut shortfalls = BTreeMap::new();
        for (asset, delta) in &deltas {
            let balance = self.balance_of(asset)?;
            let total = add(balance.total, *delta, "balance + delta")?;
            let available_after = sub(total, balance.reserved, "total - reserved")?;
            if *delta < Decimal::ZERO && available_after < Decimal::ZERO {
                shortfalls.insert(asset.clone(), neg(available_after));
            }
            new_balances.push((asset.clone(), Balance::new(total, balance.reserved)?));
        }
        let new_fee_total = match &execution.fee {
            Some(fee) => {
                let paid = self.fees_paid.get(&fee.asset).copied().unwrap_or_default();
                Some((fee.asset.clone(), add(paid, fee.amount, "fees_paid + fee")?))
            }
            None => None,
        };
        let instrument = self
            .instruments
            .get(&execution.instrument)
            .cloned()
            .ok_or_else(|| MarginError::UnknownInstrument {
                instrument: execution.instrument.clone(),
            })?;

        // Commit: nothing below can fail.
        for (asset, balance) in new_balances {
            self.balances.insert(asset, balance);
        }
        self.positions
            .insert(execution.instrument.clone(), effect.position);
        if let Some((asset, total)) = new_fee_total {
            self.fees_paid.insert(asset, total);
        }
        let index = self.journal.len();
        let cashflow = ExecutionCashflow {
            execution_id: execution.id.clone(),
            sequence: index as u64,
            instrument: execution.instrument.clone(),
            side: execution.side,
            price: execution.price,
            quantity: execution.quantity,
            // A derivative exchanges no notional: its cash is in `deltas`.
            notional_exact: None,
            notional: Decimal::ZERO,
            rounding_residual: None,
            fee: execution.fee.clone(),
            deltas,
            realized_pnl: effect.realized,
            position_after: position_snapshot(
                &execution.instrument,
                instrument.quote(),
                instrument.kind(),
                contract.size,
                effect.position,
            ),
            derivative: Some(effect.fill),
        };
        self.journal.push(cashflow.clone());
        self.executions
            .insert(execution.id.clone(), (execution.clone(), index));
        Ok((Booking::Booked(cashflow), shortfalls))
    }

    // ----- derivative settlements --------------------------------------

    /// Books the cash settlement of a derivative fill. See
    /// [`DerivativeSettlement`].
    ///
    /// The asset's total balance changes by `realized_pnl - fee`, the fee is
    /// added to the fees paid and the realized PnL to
    /// [`Ledger::settled_pnl`]. The settlement is mandatory: it is booked
    /// even when it leaves the balance below its reservations or negative,
    /// and the result reports that shortfall. Booking the identical
    /// settlement again changes nothing.
    ///
    /// # Errors
    ///
    /// The ledger is unchanged on any error.
    ///
    /// - [`MarginError::ConflictingEntry`]: the id was booked as a different
    ///   settlement or as a spot execution.
    /// - [`MarginError::UnknownAsset`]: the asset is not registered.
    /// - [`MarginError::InvalidInput`]: the instrument is a registered spot
    ///   instrument.
    /// - [`MarginError::PrecisionExceeded`]: an amount has more fractional
    ///   digits than the asset allows.
    /// - [`MarginError::Arithmetic`]: an amount overflows.
    pub fn book_settlement(
        &mut self,
        settlement: &DerivativeSettlement,
    ) -> Result<SettlementBooking> {
        if let Some((previous, index)) = self.settlements.get(&settlement.id) {
            if previous == settlement {
                return Ok(SettlementBooking {
                    cashflow: self
                        .settlement_journal
                        .get(*index)
                        .cloned()
                        .ok_or(MarginError::arithmetic("settlement journal index"))?,
                    is_new: false,
                    shortfall: None,
                });
            }
            return Err(MarginError::ConflictingEntry {
                kind: "settlement",
                id: settlement.id.to_string(),
            });
        }
        if self.executions.contains_key(&settlement.id)
            || self.fundings.contains_key(&settlement.id)
            || self.expiries.contains_key(&settlement.id)
        {
            return Err(MarginError::ConflictingEntry {
                kind: "settlement",
                id: settlement.id.to_string(),
            });
        }
        if self.instruments.contains_key(&settlement.instrument) {
            return Err(MarginError::invalid_input(
                "instrument",
                "a settlement is for a derivative held outside the ledger",
            ));
        }
        let scale = self.asset_scale_or_err(&settlement.asset)?;
        check_scale(settlement.realized_pnl, scale, "realized_pnl")?;
        check_scale(settlement.fee, scale, "fee")?;

        let balance = self.balance_of(&settlement.asset)?;
        let delta = sub(settlement.realized_pnl, settlement.fee, "realized - fee")?;
        let total = add(balance.total, delta, "balance + settlement")?;
        let available_after = sub(total, balance.reserved, "total - reserved")?;
        let shortfall = (delta < Decimal::ZERO && available_after < Decimal::ZERO)
            .then_some(neg(available_after));
        let fees = add(
            self.fees_paid(&settlement.asset),
            settlement.fee,
            "fees_paid + fee",
        )?;
        let settled = add(
            self.settled_pnl(&settlement.asset),
            settlement.realized_pnl,
            "settled_pnl + realized",
        )?;

        // Commit: nothing below can fail.
        self.balances.insert(
            settlement.asset.clone(),
            Balance::new(total, balance.reserved)?,
        );
        self.fees_paid.insert(settlement.asset.clone(), fees);
        self.settled_pnl.insert(settlement.asset.clone(), settled);
        let index = self.settlement_journal.len();
        let cashflow = SettlementCashflow {
            id: settlement.id.clone(),
            sequence: index as u64,
            instrument: settlement.instrument.clone(),
            asset: settlement.asset.clone(),
            realized_pnl: settlement.realized_pnl,
            fee: settlement.fee,
            delta,
            balance_after: total,
        };
        self.settlement_journal.push(cashflow.clone());
        self.settlements
            .insert(settlement.id.clone(), (settlement.clone(), index));
        Ok(SettlementBooking {
            cashflow,
            is_new: true,
            shortfall,
        })
    }

    /// Booked settlements in booking order.
    #[must_use]
    pub fn settlement_journal(&self) -> &[SettlementCashflow] {
        &self.settlement_journal
    }

    /// Cumulative realized PnL paid by settlements in `asset`, gross of
    /// fees (zero if none or unregistered).
    #[must_use]
    pub fn settled_pnl(&self, asset: &Asset) -> Decimal {
        self.settled_pnl.get(asset).copied().unwrap_or_default()
    }

    // ----- derivative postings ------------------------------------------

    /// Books a funding payment of a perpetual (linear or inverse).
    ///
    /// The settlement asset's total balance changes by `amount` (positive is
    /// received, negative is paid), and the amount joins
    /// [`Ledger::funding`], which valuation counts once as realized PnL. A
    /// payment with a flat position is valid (funding of a period already
    /// accrued) and creates no position. It is mandatory: it is booked even
    /// when it leaves the balance below its reservations or negative, and
    /// the result reports that shortfall. Booking the identical payment
    /// again changes nothing.
    ///
    /// # Errors
    ///
    /// The ledger is unchanged on any error. Checked in this order:
    ///
    /// - [`MarginError::ConflictingEntry`]: the id was booked with different
    ///   content, or as an execution, settlement or expiry (an identical
    ///   payment is a duplicate before any other check).
    /// - [`MarginError::UnknownInstrument`]: the instrument is not
    ///   registered.
    /// - [`MarginError::InvalidInput`]: the instrument is not a perpetual, or
    ///   the amount is zero.
    /// - [`MarginError::PrecisionExceeded`]: the amount is finer than the
    ///   settlement asset scale.
    /// - [`MarginError::Arithmetic`]: an amount overflows.
    pub fn book_funding(
        &mut self,
        payment: &FundingPayment,
    ) -> Result<PostingBooking<FundingCashflow>> {
        if let Some((previous, index)) = self.fundings.get(&payment.id) {
            if previous == payment {
                return Ok(PostingBooking {
                    cashflow: self
                        .funding_journal
                        .get(*index)
                        .cloned()
                        .ok_or(MarginError::arithmetic("funding journal index"))?,
                    is_new: false,
                    shortfall: None,
                });
            }
            return Err(MarginError::ConflictingEntry {
                kind: "funding",
                id: payment.id.to_string(),
            });
        }
        if self.executions.contains_key(&payment.id)
            || self.settlements.contains_key(&payment.id)
            || self.expiries.contains_key(&payment.id)
        {
            return Err(MarginError::ConflictingEntry {
                kind: "funding",
                id: payment.id.to_string(),
            });
        }
        let asset = match self.instruments.get(&payment.instrument) {
            Some(Instrument::LinearPerpetual(perpetual)) => perpetual.terms().settlement().clone(),
            Some(Instrument::InversePerpetual(perpetual)) => perpetual.terms().settlement().clone(),
            Some(
                Instrument::Spot(_) | Instrument::LinearFuture(_) | Instrument::InverseFuture(_),
            ) => {
                return Err(MarginError::invalid_input(
                    "instrument",
                    "funding is paid on a perpetual",
                ));
            }
            None => {
                return Err(MarginError::UnknownInstrument {
                    instrument: payment.instrument.clone(),
                });
            }
        };
        if payment.amount.is_zero() {
            return Err(MarginError::invalid_input("amount", "must not be zero"));
        }
        let scale = self.asset_scale_or_err(&asset)?;
        check_scale(payment.amount, scale, "amount")?;
        let balance = self.balance_of(&asset)?;
        let total = add(balance.total, payment.amount, "balance + funding")?;
        let available_after = sub(total, balance.reserved, "total - reserved")?;
        let shortfall = (payment.amount < Decimal::ZERO && available_after < Decimal::ZERO)
            .then_some(neg(available_after));
        let funding = add(self.funding(&asset), payment.amount, "funding + amount")?;

        // Commit: nothing below can fail.
        self.balances
            .insert(asset.clone(), Balance::new(total, balance.reserved)?);
        self.funding.insert(asset.clone(), funding);
        let index = self.funding_journal.len();
        let cashflow = FundingCashflow {
            id: payment.id.clone(),
            sequence: index as u64,
            instrument: payment.instrument.clone(),
            asset,
            amount: payment.amount,
            balance_after: total,
        };
        self.funding_journal.push(cashflow.clone());
        self.fundings
            .insert(payment.id.clone(), (payment.clone(), index));
        Ok(PostingBooking {
            cashflow,
            is_new: true,
            shortfall,
        })
    }

    /// Settles a future (linear or inverse) at expiry: closes its whole position at
    /// `price`, with the closing rules of an execution and no fee, and marks
    /// the instrument as settled.
    ///
    /// Settlement is terminal: the instrument cannot be settled again. The
    /// ledger has no clock, so a real fill delivered after the settlement
    /// (a late fill) is still booked, never rejected: it is closed at once
    /// at this settlement price, leaving the position flat, its realized PnL
    /// is the fill against the settlement price, and the cashflow carries
    /// [`DerivativeFill::closed_at_settlement`] so the caller can
    /// investigate. A flat position is settled with no cash. The cash is
    /// mandatory, as for an execution.
    ///
    /// # Errors
    ///
    /// The ledger is unchanged on any error. Checked in this order:
    ///
    /// - [`MarginError::ConflictingEntry`]: the id was booked with different
    ///   content, or as an execution, settlement or funding payment, or the
    ///   instrument was already settled under another id (an identical
    ///   settlement is a duplicate before any other check).
    /// - [`MarginError::UnknownInstrument`]: the instrument is not
    ///   registered.
    /// - [`MarginError::InvalidInput`]: the instrument is not a future,
    ///   `as_of_ms` is before its expiry, or the price is not positive.
    /// - [`MarginError::PrecisionExceeded`]: the price is finer than the
    ///   instrument's price scale.
    /// - [`MarginError::Arithmetic`]: an amount overflows.
    #[expect(
        clippy::too_many_lines,
        reason = "one validate-then-commit sequence: the idempotency and terminal checks, then the close, then the commit"
    )]
    pub fn settle_expiry(
        &mut self,
        settlement: &ExpirySettlement,
    ) -> Result<PostingBooking<ExpiryCashflow>> {
        if let Some((previous, index)) = self.expiries.get(&settlement.id) {
            if previous == settlement {
                return Ok(PostingBooking {
                    cashflow: self
                        .expiry_journal
                        .get(*index)
                        .cloned()
                        .ok_or(MarginError::arithmetic("expiry journal index"))?,
                    is_new: false,
                    shortfall: None,
                });
            }
            return Err(MarginError::ConflictingEntry {
                kind: "expiry",
                id: settlement.id.to_string(),
            });
        }
        if self.executions.contains_key(&settlement.id)
            || self.settlements.contains_key(&settlement.id)
            || self.fundings.contains_key(&settlement.id)
            || self
                .settled_instruments
                .contains_key(&settlement.instrument)
        {
            return Err(MarginError::ConflictingEntry {
                kind: "expiry",
                id: settlement.id.to_string(),
            });
        }
        let (contract, expiry_ms) = match self.instruments.get(&settlement.instrument) {
            Some(future @ (Instrument::LinearFuture(_) | Instrument::InverseFuture(_))) => {
                let contract = future
                    .derivative()
                    .ok_or(MarginError::arithmetic("derivative contract"))?;
                let expiry_ms = contract
                    .expiry_ms
                    .ok_or(MarginError::arithmetic("future expiry"))?;
                (contract, expiry_ms)
            }
            Some(
                Instrument::Spot(_)
                | Instrument::LinearPerpetual(_)
                | Instrument::InversePerpetual(_),
            ) => {
                return Err(MarginError::invalid_input(
                    "instrument",
                    "only a future expires",
                ));
            }
            None => {
                return Err(MarginError::UnknownInstrument {
                    instrument: settlement.instrument.clone(),
                });
            }
        };
        if settlement.as_of_ms < expiry_ms {
            return Err(MarginError::invalid_input(
                "as_of_ms",
                "a future is settled at or after its expiry",
            ));
        }
        if settlement.price <= Decimal::ZERO {
            return Err(MarginError::invalid_input("price", "must be positive"));
        }
        check_scale(settlement.price, contract.price_scale, "price")?;
        let asset = contract.settlement.clone();
        let scale = self.asset_scale_or_err(&asset)?;
        let position = self
            .positions
            .get(&settlement.instrument)
            .copied()
            .unwrap_or_default();
        let contracts = position.quantity;
        let (new_position, realized, fill) = if contracts.is_zero() {
            (
                position,
                Decimal::ZERO,
                DerivativeFill {
                    size: contract.size,
                    virtual_notional: Some(Decimal::ZERO),
                    closing_notional: None,
                    residual: None,
                    closing_residual: None,
                    released_basis: Decimal::ZERO,
                    closed_at_settlement: None,
                },
            )
        } else {
            let dir = if contracts > Decimal::ZERO {
                Dir::Sell
            } else {
                Dir::Buy
            };
            let effect = derivative::apply_derivative_fill(
                position,
                dir,
                settlement.price,
                contracts.abs(),
                contract.size,
                scale,
            )?;
            (effect.position, effect.realized, effect.fill)
        };
        let balance = self.balance_of(&asset)?;
        let total = add(balance.total, realized, "balance + realized")?;
        let available_after = sub(total, balance.reserved, "total - reserved")?;
        let shortfall = (realized < Decimal::ZERO && available_after < Decimal::ZERO)
            .then_some(neg(available_after));

        // Commit: nothing below can fail.
        self.balances
            .insert(asset.clone(), Balance::new(total, balance.reserved)?);
        self.positions
            .insert(settlement.instrument.clone(), new_position);
        self.settled_instruments
            .insert(settlement.instrument.clone(), settlement.id.clone());
        let index = self.expiry_journal.len();
        let cashflow = ExpiryCashflow {
            id: settlement.id.clone(),
            sequence: index as u64,
            instrument: settlement.instrument.clone(),
            asset,
            price: settlement.price,
            contracts,
            realized_pnl: realized,
            derivative: fill,
            balance_after: total,
        };
        self.expiry_journal.push(cashflow.clone());
        self.expiries
            .insert(settlement.id.clone(), (settlement.clone(), index));
        Ok(PostingBooking {
            cashflow,
            is_new: true,
            shortfall,
        })
    }

    /// Booked funding payments in booking order.
    #[must_use]
    pub fn funding_journal(&self) -> &[FundingCashflow] {
        &self.funding_journal
    }

    /// Booked expiry settlements in booking order.
    #[must_use]
    pub fn expiry_journal(&self) -> &[ExpiryCashflow] {
        &self.expiry_journal
    }

    /// Net funding received in `asset` (negative when paid; zero if none or
    /// unregistered). Valuation counts it once as realized PnL.
    #[must_use]
    pub fn funding(&self, asset: &Asset) -> Decimal {
        self.funding.get(asset).copied().unwrap_or_default()
    }

    /// True once a future was settled at expiry.
    #[must_use]
    pub fn is_settled(&self, instrument: &InstrumentId) -> bool {
        self.settled_instruments.contains_key(instrument)
    }

    // ----- reads --------------------------------------------------------

    /// Balance of a registered asset.
    #[must_use]
    pub fn balance(&self, asset: &Asset) -> Option<BalanceSnapshot> {
        self.balances.get(asset).map(|b| BalanceSnapshot {
            total: b.total,
            reserved: b.reserved,
            // Computed exactly when the balance changed; negative only after
            // an overdraft.
            available: b.available,
        })
    }

    /// Position in a registered instrument.
    #[must_use]
    pub fn position(&self, instrument: &InstrumentId) -> Option<PositionSnapshot> {
        let position = self.positions.get(instrument)?;
        let registered = self.instruments.get(instrument)?;
        Some(position_snapshot(
            instrument,
            registered.quote(),
            registered.kind(),
            registered.contract_size(),
            *position,
        ))
    }

    /// Cumulative fees paid in `asset` (zero if none or unregistered).
    #[must_use]
    pub fn fees_paid(&self, asset: &Asset) -> Decimal {
        self.fees_paid.get(asset).copied().unwrap_or_default()
    }

    /// Booked executions in booking order.
    #[must_use]
    pub fn journal(&self) -> &[ExecutionCashflow] {
        &self.journal
    }

    /// Cashflow of a booked execution.
    #[must_use]
    pub fn execution_cashflow(&self, id: &ExecutionId) -> Option<&ExecutionCashflow> {
        self.executions
            .get(id)
            .and_then(|(_, index)| self.journal.get(*index))
    }

    /// Applied transfers, ordered by identifier.
    pub fn transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers.values()
    }

    /// Copy of the full state.
    #[must_use]
    pub fn snapshot(&self) -> LedgerSnapshot {
        LedgerSnapshot {
            assets: self.assets.clone(),
            instruments: self.instruments.clone(),
            net_transfers: self.net_transfers.clone(),
            balances: self
                .balances
                .keys()
                .filter_map(|asset| self.balance(asset).map(|b| (asset.clone(), b)))
                .collect(),
            positions: self
                .positions
                .keys()
                .filter_map(|id| self.position(id).map(|p| (id.clone(), p)))
                .collect(),
            fees_paid: self.fees_paid.clone(),
            settled_pnl: self.settled_pnl.clone(),
            funding: self.funding.clone(),
            reservations: self.reservations.clone(),
            executions_booked: self.journal.len() as u64,
            settlements_booked: self.settlement_journal.len() as u64,
            settled_instruments: self.settled_instruments.keys().cloned().collect(),
        }
    }

    /// Values every position at the given marks. See the
    /// [valuation policy](Valuation#valuation-policy).
    ///
    /// Flat positions need no mark. `by_asset` has one line per registered
    /// asset.
    ///
    /// # Errors
    ///
    /// - [`MarginError::MissingMark`] if an open position has no mark.
    /// - [`MarginError::InvalidInput`] if a used mark is not positive.
    /// - [`MarginError::PrecisionExceeded`] if a used mark has more than
    ///   [`MAX_SCALE`](crate::decimal::MAX_SCALE) fractional digits.
    /// - [`MarginError::Arithmetic`] on overflow.
    pub fn valuation(&self, marks: &BTreeMap<InstrumentId, Decimal>) -> Result<Valuation> {
        let rounding = self.config.rounding;
        let mut by_asset: BTreeMap<Asset, AssetPnl> = self
            .fees_paid
            .iter()
            .map(|(asset, fees)| {
                (
                    asset.clone(),
                    AssetPnl {
                        fees_paid: *fees,
                        ..AssetPnl::default()
                    },
                )
            })
            .collect();
        let mut positions = BTreeMap::new();
        let mut asset_terms: BTreeMap<Asset, (Vec<Decimal>, Vec<Decimal>)> = BTreeMap::new();
        for (id, position) in &self.positions {
            let registered =
                self.instruments
                    .get(id)
                    .ok_or_else(|| MarginError::UnknownInstrument {
                        instrument: id.clone(),
                    })?;
            let quote = registered.quote();
            let quote_scale = self.asset_scale_or_err(quote)?;
            let (mark, market_value) = if position.quantity.is_zero() {
                (None, Decimal::ZERO)
            } else {
                let mark = *marks.get(id).ok_or_else(|| MarginError::MissingMark {
                    instrument: id.clone(),
                })?;
                let value =
                    position_value(registered, position.quantity, mark, quote_scale, rounding)?;
                (Some(mark), value)
            };
            let unrealized_pnl = derivative::unrealized_pnl(
                registered.contract_size(),
                market_value,
                position.cost_basis,
            )?;
            let terms = asset_terms.entry(quote.clone()).or_default();
            terms.0.push(position.realized_pnl);
            terms.1.push(unrealized_pnl);
            positions.insert(
                id.clone(),
                PositionValuation {
                    quote: quote.clone(),
                    quantity: position.quantity,
                    mark,
                    market_value,
                    cost_basis: position.cost_basis,
                    unrealized_pnl,
                    realized_pnl: position.realized_pnl,
                },
            );
        }
        // Per asset sums and net PnL, each one exact wide sum. Settlements
        // and funding are realized PnL already in cash, counted once.
        for (asset, realized) in self.settled_pnl.iter().chain(&self.funding) {
            if !realized.is_zero() {
                asset_terms
                    .entry(asset.clone())
                    .or_default()
                    .0
                    .push(*realized);
            }
        }
        for (asset, line) in &mut by_asset {
            if let Some((realized, unrealized)) = asset_terms.get(asset) {
                line.realized_pnl = decimal::sum(realized, "realized sum")?;
                line.unrealized_pnl = decimal::sum(unrealized, "unrealized sum")?;
            }
            let mut net = asset_terms
                .get(asset)
                .map(|(r, u)| r.iter().chain(u).copied().collect::<Vec<_>>())
                .unwrap_or_default();
            net.push(neg(line.fees_paid));
            line.net_pnl = decimal::sum(&net, "realized + unrealized - fees")?;
        }
        Ok(Valuation {
            positions,
            by_asset,
            scales: self.assets.clone(),
            rounding,
        })
    }

    /// Assets whose total balance is below their reservations, with the
    /// shortfall (`reserved - total`). Empty unless a real execution was
    /// booked with [`Ledger::book_execution_allowing_overdraft`].
    #[must_use]
    pub fn overdrafts(&self) -> BTreeMap<Asset, Decimal> {
        self.balances
            .iter()
            .filter(|(_, b)| b.total < b.reserved)
            .map(|(asset, b)| (asset.clone(), neg(b.available)))
            .collect()
    }

    /// Recomputes the state from the transfers and the execution journal and
    /// compares it with the stored state.
    ///
    /// Checks, per asset, that the total balance equals deposits minus
    /// withdrawals plus the sum of execution deltas, that the reserved
    /// balance equals the open reservations and is covered by the total, and
    /// that the fees paid equal the sum of execution fees. Settlements,
    /// funding payments and expiry settlements join the asset totals, and
    /// the net funding is checked per asset. Per instrument, it checks that
    /// the position quantity equals the net traded quantity and that the
    /// quote cash exchanged by trades equals `realized_pnl - cost_basis`
    /// (cost is conserved exactly). For a future or perpetual the
    /// same identity holds with the signed virtual notionals (`-N` for a
    /// buy, `+N` for a sell, the expiry close included), recomputed from
    /// every fill and compared in wide integers.
    ///
    /// An overdraft (a balance below its reservations, after booking a real
    /// execution with [`Ledger::book_execution_allowing_overdraft`]) is not
    /// a mismatch: the sums still agree. It is reported in the returned
    /// [`Reconciliation`].
    ///
    /// # Errors
    ///
    /// [`MarginError::ReconciliationMismatch`] naming the first failed check,
    /// or [`MarginError::Arithmetic`] on overflow.
    #[expect(
        clippy::too_many_lines,
        reason = "one replay of the journal followed by one comparison per balance and position; splitting it would duplicate the replayed state"
    )]
    pub fn reconcile(&self) -> Result<Reconciliation> {
        // Every figure is the exact wide sum of its terms, independent of
        // order: partial sums never need to be representable.
        let mut totals: BTreeMap<&Asset, Vec<Decimal>> = BTreeMap::new();
        let mut reserved_terms: BTreeMap<&Asset, Vec<Decimal>> = BTreeMap::new();
        let mut fee_terms: BTreeMap<&Asset, Vec<Decimal>> = BTreeMap::new();
        let mut quantity_terms: BTreeMap<&InstrumentId, Vec<Decimal>> = BTreeMap::new();
        let mut cash_terms: BTreeMap<&InstrumentId, Vec<Decimal>> = BTreeMap::new();
        let mut transfer_terms: BTreeMap<&Asset, Vec<Decimal>> = BTreeMap::new();
        let mut settled_terms: BTreeMap<&Asset, Vec<Decimal>> = BTreeMap::new();
        for settlement in &self.settlement_journal {
            totals
                .entry(&settlement.asset)
                .or_default()
                .push(settlement.delta);
            fee_terms
                .entry(&settlement.asset)
                .or_default()
                .push(settlement.fee);
            settled_terms
                .entry(&settlement.asset)
                .or_default()
                .push(settlement.realized_pnl);
            mismatch(
                "settlement delta = realized - fee",
                settlement.id.as_str(),
                sub(settlement.realized_pnl, settlement.fee, "realized - fee")?,
                settlement.delta,
            )?;
        }
        for transfer in self.transfers.values() {
            totals
                .entry(&transfer.asset)
                .or_default()
                .push(transfer.signed_amount());
            transfer_terms
                .entry(&transfer.asset)
                .or_default()
                .push(transfer.signed_amount());
        }
        for (asset, amount) in self.reservations.values() {
            reserved_terms.entry(asset).or_default().push(*amount);
        }
        let mut funding_terms: BTreeMap<&Asset, Vec<Decimal>> = BTreeMap::new();
        for payment in &self.funding_journal {
            totals
                .entry(&payment.asset)
                .or_default()
                .push(payment.amount);
            funding_terms
                .entry(&payment.asset)
                .or_default()
                .push(payment.amount);
        }
        // Derivatives: the virtual notionals are recomputed in wide integers
        // from each fill, since their sum may not fit a Decimal even when the
        // stored realized PnL and basis do.
        let mut virtual_cash: BTreeMap<&InstrumentId, Wide> = BTreeMap::new();
        for cashflow in &self.journal {
            for (asset, delta) in &cashflow.deltas {
                totals.entry(asset).or_default().push(*delta);
            }
            if let Some(fee) = &cashflow.fee {
                fee_terms.entry(&fee.asset).or_default().push(fee.amount);
            }
            let dir = Dir::from_action(cashflow.side, "cashflow side")?;
            let quantity = match dir {
                Dir::Buy => cashflow.quantity,
                Dir::Sell => neg(cashflow.quantity),
            };
            quantity_terms
                .entry(&cashflow.instrument)
                .or_default()
                .push(quantity);
            if let Some(fill) = &cashflow.derivative {
                let scale = self.asset_scale_or_err(&cashflow.position_after.quote)?;
                let term = derivative::signed_virtual_cash(
                    dir,
                    cashflow.price,
                    cashflow.quantity,
                    fill.size,
                    scale,
                )?;
                add_virtual(&mut virtual_cash, &cashflow.instrument, term)?;
                // A fill after the settlement is closed at the settlement
                // price: the opposite leg, with no net contracts.
                if let Some(price) = fill.closed_at_settlement {
                    let (opposite, back) = match dir {
                        Dir::Buy => (Dir::Sell, neg(cashflow.quantity)),
                        Dir::Sell => (Dir::Buy, cashflow.quantity),
                    };
                    let term = derivative::signed_virtual_cash(
                        opposite,
                        price,
                        cashflow.quantity,
                        fill.size,
                        scale,
                    )?;
                    add_virtual(&mut virtual_cash, &cashflow.instrument, term)?;
                    quantity_terms
                        .entry(&cashflow.instrument)
                        .or_default()
                        .push(back);
                }
            } else {
                let cash = match dir {
                    Dir::Buy => neg(cashflow.notional),
                    Dir::Sell => cashflow.notional,
                };
                cash_terms
                    .entry(&cashflow.instrument)
                    .or_default()
                    .push(cash);
            }
        }
        for expiry in &self.expiry_journal {
            totals
                .entry(&expiry.asset)
                .or_default()
                .push(expiry.realized_pnl);
            if !expiry.contracts.is_zero() {
                // The settlement closes the contracts: a sell of a long, a
                // buy of a short.
                let dir = if expiry.contracts > Decimal::ZERO {
                    Dir::Sell
                } else {
                    Dir::Buy
                };
                quantity_terms
                    .entry(&expiry.instrument)
                    .or_default()
                    .push(neg(expiry.contracts));
                let scale = self.asset_scale_or_err(&expiry.asset)?;
                let term = derivative::signed_virtual_cash(
                    dir,
                    expiry.price,
                    expiry.contracts.abs(),
                    expiry.derivative.size,
                    scale,
                )?;
                add_virtual(&mut virtual_cash, &expiry.instrument, term)?;
            }
        }
        let total_of = |terms: Option<&Vec<Decimal>>, op| match terms {
            Some(values) => decimal::sum(values, op),
            None => Ok(Decimal::ZERO),
        };

        for (asset, balance) in &self.balances {
            let expected = total_of(totals.get(asset), "reconcile balance")?;
            mismatch("balance.total", asset.as_str(), expected, balance.total)?;
            let expected = total_of(reserved_terms.get(asset), "reconcile reservations")?;
            mismatch(
                "balance.reserved",
                asset.as_str(),
                expected,
                balance.reserved,
            )?;
            if balance.reserved < Decimal::ZERO {
                return Err(MarginError::ReconciliationMismatch {
                    check: "balance.reserved >= 0",
                    key: asset.to_string(),
                    expected: Decimal::ZERO,
                    actual: balance.reserved,
                });
            }
            let expected = total_of(fee_terms.get(asset), "reconcile fees")?;
            let actual = self.fees_paid.get(asset).copied().unwrap_or_default();
            mismatch("fees_paid", asset.as_str(), expected, actual)?;
            let expected = total_of(transfer_terms.get(asset), "reconcile transfers")?;
            let actual = self.net_transfers.get(asset).copied().unwrap_or_default();
            mismatch("net_transfers", asset.as_str(), expected, actual)?;
            let expected = total_of(settled_terms.get(asset), "reconcile settlements")?;
            mismatch(
                "settled_pnl",
                asset.as_str(),
                expected,
                self.settled_pnl(asset),
            )?;
            let expected = total_of(funding_terms.get(asset), "reconcile funding")?;
            mismatch("funding", asset.as_str(), expected, self.funding(asset))?;
        }
        for (id, position) in &self.positions {
            let expected = total_of(quantity_terms.get(id), "reconcile quantity")?;
            mismatch(
                "position.quantity",
                id.as_str(),
                expected,
                position.quantity,
            )?;
            let derivative = match self.instruments.get(id) {
                Some(Instrument::Spot(_)) | None => None,
                Some(
                    registered @ (Instrument::LinearFuture(_)
                    | Instrument::LinearPerpetual(_)
                    | Instrument::InverseFuture(_)
                    | Instrument::InversePerpetual(_)),
                ) => Some((
                    self.asset_scale_or_err(registered.quote())?,
                    registered.contract_size(),
                )),
            };
            if let Some((scale, size)) = derivative {
                // Virtual identity, in wide integers: sum of the signed
                // virtual values == realized - basis for a linear contract,
                // -realized - basis for an inverse one.
                let expected = virtual_cash.get(id).copied().unwrap_or(Wide::ZERO);
                let realized = decimal_to_wide(position.realized_pnl, scale, "realized pnl")?;
                let realized = match size {
                    ContractSize::Multiplier(_) => realized,
                    ContractSize::ContractValue(_) => realized.negated(),
                };
                let actual = derivative::add_wide(
                    realized,
                    decimal_to_wide(position.cost_basis, scale, "basis")?.negated(),
                    "realized - basis",
                )?;
                if expected != actual {
                    return Err(MarginError::ReconciliationMismatch {
                        check: "virtual notional = realized - basis",
                        key: id.to_string(),
                        expected: expected.to_decimal(scale).unwrap_or(Decimal::MAX),
                        actual: actual.to_decimal(scale).unwrap_or(Decimal::MAX),
                    });
                }
                continue;
            }
            let expected = total_of(cash_terms.get(id), "reconcile trade cash")?;
            let actual = sub(
                position.realized_pnl,
                position.cost_basis,
                "realized - cost",
            )?;
            mismatch(
                "trade cash = realized - cost",
                id.as_str(),
                expected,
                actual,
            )?;
        }
        Ok(Reconciliation {
            overdrafts: self.overdrafts(),
        })
    }
}

/// The market value of `quantity` of `instrument` at a positive `mark`:
/// `round(quantity * mark)` for spot (under the ledger's rounding), and for
/// a derivative always half-even: `rhe(Q * M * mark)`, or `rhe(Q * CV /
/// mark)` in the base for an inverse one.
fn position_value(
    instrument: &Instrument,
    quantity: Decimal,
    mark: Decimal,
    scale: u32,
    rounding: Rounding,
) -> Result<Decimal> {
    if mark <= Decimal::ZERO {
        return Err(MarginError::invalid_input("mark", "must be positive"));
    }
    check_scale(mark, decimal::MAX_SCALE, "mark")?;
    if let Instrument::Spot(_) = instrument {
        mul_div_round(&[quantity, mark], &[], scale, rounding, "quantity * mark")
    } else {
        derivative::market_value(quantity, instrument.contract_size(), mark, scale)
    }
}

fn add_virtual<'a>(
    sums: &mut BTreeMap<&'a InstrumentId, Wide>,
    instrument: &'a InstrumentId,
    term: Wide,
) -> Result<()> {
    let sum = sums.entry(instrument).or_insert(Wide::ZERO);
    *sum = derivative::add_wide(*sum, term, "virtual notional sum")?;
    Ok(())
}

fn mismatch(check: &'static str, key: &str, expected: Decimal, actual: Decimal) -> Result<()> {
    if expected == actual {
        Ok(())
    } else {
        Err(MarginError::ReconciliationMismatch {
            check,
            key: key.to_string(),
            expected,
            actual,
        })
    }
}

fn position_snapshot(
    instrument: &InstrumentId,
    quote: &Asset,
    kind: PositionKind,
    size: ContractSize,
    position: Position,
) -> PositionSnapshot {
    PositionSnapshot {
        instrument: instrument.clone(),
        quote: quote.clone(),
        kind,
        size,
        quantity: position.quantity,
        cost_basis: position.cost_basis,
        realized_pnl: position.realized_pnl,
    }
}

/// Applies a fill to a position with average cost accounting.
///
/// `signed_quantity` is positive for a buy; `signed_value` is the rounded
/// notional with the same sign. Returns the new position and the PnL
/// realized by this fill.
///
/// - Opening or increasing: quantity and cost basis grow by the fill.
/// - Reducing by `r` out of `|q|`: the cost released is
///   `round(cost_basis * r / |q|)`, or the whole cost basis when the fill
///   closes the position, so the cost released over a full close sums to
///   the cost paid exactly. Realized PnL is the opposite of the fill value
///   minus the released cost.
/// - Crossing zero: the closing part is valued at `round(price * |q|)` and
///   the rest of the fill's rounded notional opens the new position, so the
///   cash of the fill is split without loss.
pub(crate) fn apply_fill(
    position: Position,
    signed_quantity: Decimal,
    signed_value: Decimal,
    price: Decimal,
    quote_scale: u32,
    rounding: Rounding,
) -> Result<(Position, Decimal)> {
    let quantity = position.quantity;
    let cost = position.cost_basis;
    let same_direction =
        quantity.is_zero() || quantity.is_sign_negative() == signed_quantity.is_sign_negative();
    let new_quantity = add(quantity, signed_quantity, "position quantity")?;

    if same_direction {
        let new_cost = add(cost, signed_value, "cost basis")?;
        return Ok((
            Position {
                quantity: new_quantity,
                cost_basis: new_cost,
                realized_pnl: position.realized_pnl,
            },
            Decimal::ZERO,
        ));
    }

    let held = quantity.abs();
    let traded = signed_quantity.abs();
    let (realized, new_cost) = if traded <= held {
        let released = if traded == held {
            cost
        } else {
            mul_div_round(
                &[cost, traded],
                &[held],
                quote_scale,
                rounding,
                "released cost",
            )?
        };
        let realized = sub(neg(signed_value), released, "realized pnl")?;
        (realized, sub(cost, released, "cost - released")?)
    } else {
        let close_value =
            mul_div_round(&[price, held], &[], quote_scale, rounding, "price * held")?;
        let close_value = if signed_value.is_sign_negative() {
            neg(close_value)
        } else {
            close_value
        };
        let open_value = sub(signed_value, close_value, "open value")?;
        let realized = sub(neg(close_value), cost, "realized pnl")?;
        (realized, open_value)
    };
    let realized_pnl = add(position.realized_pnl, realized, "realized sum")?;
    Ok((
        Position {
            quantity: new_quantity,
            cost_basis: new_cost,
            realized_pnl,
        },
        realized,
    ))
}
