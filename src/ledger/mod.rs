//! Single accounting authority: cash, positions and fees.
//!
//! A [`Ledger`] holds, for one account:
//!
//! - a balance per asset, split into reserved and available;
//! - a position per instrument, with signed cost basis and realized PnL;
//! - the fees paid per asset;
//! - the journal of booked executions, each with its cashflow
//!   reconciliation ([`ExecutionCashflow`]).
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
//!   rejected with [`MarginError::InsufficientBalance`].
//! - Amounts are exact decimals. Inputs with more fractional digits than
//!   allowed are rejected, never truncated. The only rounding steps are
//!   the notional (`price * quantity` to the quote asset scale) and the cost
//!   released when a position is partly reduced; both use the configured
//!   [`Rounding`] and are recorded so that totals stay exact.
//!
//! # Example
//!
//! ```
//! use margin_rs::ledger::{Execution, Ledger, Side};
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
//! let buy = Execution::new(ExecutionId::new("e1")?, pair.clone(), Side::Buy, dec!(100), dec!(2))
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

mod execution;
mod snapshot;
mod valuation;

use std::collections::BTreeMap;

use rust_decimal::Decimal;

pub use execution::{Booking, Execution, ExecutionCashflow, Fee, Side};
pub use snapshot::{BalanceSnapshot, LedgerSnapshot, PositionSnapshot};
pub use valuation::{AssetPnl, PositionValuation, Valuation};

use crate::decimal::{
    self, Rounding, add, check_configured_scale, check_scale, mul, mul_div_round, sub,
};
use crate::error::{MarginError, Result};
use crate::ids::{Asset, ExecutionId, InstrumentId, ReservationId, TransferId};
use crate::instrument::Instrument;

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
            TransferKind::Withdrawal => -self.amount,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Balance {
    total: Decimal,
    reserved: Decimal,
}

impl Balance {
    fn available(self) -> Result<Decimal> {
        sub(self.total, self.reserved, "total - reserved")
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Position {
    quantity: Decimal,
    cost_basis: Decimal,
    realized_pnl: Decimal,
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
    executions: BTreeMap<ExecutionId, (Execution, usize)>,
    journal: Vec<ExecutionCashflow>,
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
                self.fees_paid.insert(asset, Decimal::ZERO);
                Ok(())
            }
        }
    }

    /// Registers an instrument. Its assets must be registered first.
    ///
    /// Registering an identical instrument again is a no-op.
    ///
    /// # Errors
    ///
    /// - [`MarginError::UnknownAsset`] if the base or quote asset is not
    ///   registered.
    /// - [`MarginError::InvalidInput`] if the quantity scale exceeds the base
    ///   asset scale.
    /// - [`MarginError::AlreadyRegistered`] if a different instrument uses the
    ///   same identifier.
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
        }
        let id = instrument.id().clone();
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
            let available = balance.available()?;
            if transfer.amount > available {
                return Err(MarginError::InsufficientBalance {
                    asset: transfer.asset,
                    required: transfer.amount,
                    available,
                });
            }
        }
        self.balances.insert(
            transfer.asset.clone(),
            Balance {
                total,
                reserved: balance.reserved,
            },
        );
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
        let available = balance.available()?;
        if amount > available {
            return Err(MarginError::InsufficientBalance {
                asset,
                required: amount,
                available,
            });
        }
        let reserved = add(balance.reserved, amount, "reserved + amount")?;
        self.balances.insert(
            asset.clone(),
            Balance {
                total: balance.total,
                reserved,
            },
        );
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
        let (asset, amount) = self
            .reservations
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownReservation { id: id.to_string() })?;
        let balance = self.balance_of(&asset)?;
        let reserved = sub(balance.reserved, amount, "reserved - amount")?;
        self.balances.insert(
            asset.clone(),
            Balance {
                total: balance.total,
                reserved,
            },
        );
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
    /// - [`MarginError::ConflictingEntry`]: the id was booked with different
    ///   content.
    /// - [`MarginError::UnknownInstrument`] or [`MarginError::UnknownAsset`]
    ///   (fee asset).
    /// - [`MarginError::InvalidInput`]: price or quantity not positive.
    /// - [`MarginError::PrecisionExceeded`]: price, quantity or fee has more
    ///   fractional digits than the instrument or asset allows.
    /// - [`MarginError::InsufficientBalance`]: an asset debited by the
    ///   execution would end with a negative available balance.
    /// - [`MarginError::Arithmetic`]: overflow or a non exact intermediate.
    pub fn book_execution(&mut self, execution: &Execution) -> Result<Booking> {
        if let Some((previous, index)) = self.executions.get(&execution.id) {
            if previous == execution {
                return Ok(Booking::Duplicate(self.journal[*index].clone()));
            }
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
        let (signed_quantity, signed_value) = match execution.side {
            Side::Buy => (execution.quantity, notional),
            Side::Sell => (-execution.quantity, -notional),
        };

        let mut deltas: BTreeMap<Asset, Decimal> = BTreeMap::new();
        accumulate(&mut deltas, spot.base(), signed_quantity)?;
        accumulate(&mut deltas, spot.quote(), -signed_value)?;
        if let Some(fee) = &execution.fee {
            accumulate(&mut deltas, &fee.asset, -fee.amount)?;
        }

        let mut new_balances = Vec::with_capacity(deltas.len());
        for (asset, delta) in &deltas {
            let balance = self.balance_of(asset)?;
            let total = add(balance.total, *delta, "balance + delta")?;
            if *delta < Decimal::ZERO {
                let available_after = sub(total, balance.reserved, "total - reserved")?;
                if available_after < Decimal::ZERO {
                    return Err(MarginError::InsufficientBalance {
                        asset: asset.clone(),
                        required: -*delta,
                        available: balance.available()?,
                    });
                }
            }
            new_balances.push((
                asset.clone(),
                Balance {
                    total,
                    reserved: balance.reserved,
                },
            ));
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
            position_after: position_snapshot(&execution.instrument, spot.quote(), new_position),
        };
        self.journal.push(cashflow.clone());
        self.executions
            .insert(execution.id.clone(), (execution.clone(), index));
        Ok(Booking::Booked(cashflow))
    }

    // ----- reads --------------------------------------------------------

    /// Balance of a registered asset.
    #[must_use]
    pub fn balance(&self, asset: &Asset) -> Option<BalanceSnapshot> {
        self.balances.get(asset).map(|b| BalanceSnapshot {
            total: b.total,
            reserved: b.reserved,
            // Both terms are non negative and reserved <= total (checked on
            // every write), so this difference cannot overflow.
            available: b.total - b.reserved,
        })
    }

    /// Position in a registered instrument.
    #[must_use]
    pub fn position(&self, instrument: &InstrumentId) -> Option<PositionSnapshot> {
        let position = self.positions.get(instrument)?;
        let quote = self.instruments.get(instrument)?.quote();
        Some(position_snapshot(instrument, quote, *position))
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
            reservations: self.reservations.clone(),
            executions_booked: self.journal.len() as u64,
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
        for (id, position) in &self.positions {
            let quote = self
                .instruments
                .get(id)
                .map(Instrument::quote)
                .ok_or_else(|| MarginError::UnknownInstrument {
                    instrument: id.clone(),
                })?;
            let quote_scale = self.asset_scale_or_err(quote)?;
            let (mark, market_value) = if position.quantity.is_zero() {
                (None, Decimal::ZERO)
            } else {
                let mark = *marks.get(id).ok_or_else(|| MarginError::MissingMark {
                    instrument: id.clone(),
                })?;
                if mark <= Decimal::ZERO {
                    return Err(MarginError::invalid_input("mark", "must be positive"));
                }
                check_scale(mark, decimal::MAX_SCALE, "mark")?;
                let value = mul_div_round(
                    &[position.quantity, mark],
                    &[],
                    quote_scale,
                    rounding,
                    "quantity * mark",
                )?;
                (Some(mark), value)
            };
            let unrealized_pnl = sub(market_value, position.cost_basis, "market_value - cost")?;
            let line = by_asset.entry(quote.clone()).or_default();
            line.realized_pnl = add(line.realized_pnl, position.realized_pnl, "realized sum")?;
            line.unrealized_pnl = add(line.unrealized_pnl, unrealized_pnl, "unrealized sum")?;
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
        for line in by_asset.values_mut() {
            let gross = add(
                line.realized_pnl,
                line.unrealized_pnl,
                "realized + unrealized",
            )?;
            line.net_pnl = sub(gross, line.fees_paid, "gross - fees")?;
        }
        Ok(Valuation {
            positions,
            by_asset,
            scales: self.assets.clone(),
            rounding,
        })
    }

    /// Recomputes the state from the transfers and the execution journal and
    /// compares it with the stored state.
    ///
    /// Checks, per asset, that the total balance equals deposits minus
    /// withdrawals plus the sum of execution deltas, that the reserved
    /// balance equals the open reservations and is covered by the total, and
    /// that the fees paid equal the sum of execution fees. Per instrument, it
    /// checks that the position quantity equals the net traded quantity and
    /// that the quote cash exchanged by trades equals
    /// `realized_pnl - cost_basis` (cost is conserved exactly).
    ///
    /// # Errors
    ///
    /// [`MarginError::ReconciliationMismatch`] naming the first failed check,
    /// or [`MarginError::Arithmetic`] on overflow.
    pub fn reconcile(&self) -> Result<()> {
        let mut totals: BTreeMap<&Asset, Decimal> = BTreeMap::new();
        let mut reserved: BTreeMap<&Asset, Decimal> = BTreeMap::new();
        let mut fees: BTreeMap<&Asset, Decimal> = BTreeMap::new();
        let mut quantities: BTreeMap<&InstrumentId, Decimal> = BTreeMap::new();
        let mut trade_cash: BTreeMap<&InstrumentId, Decimal> = BTreeMap::new();

        for transfer in self.transfers.values() {
            let entry = totals.entry(&transfer.asset).or_default();
            *entry = add(*entry, transfer.signed_amount(), "reconcile transfers")?;
        }
        for (asset, amount) in self.reservations.values() {
            let entry = reserved.entry(asset).or_default();
            *entry = add(*entry, *amount, "reconcile reservations")?;
        }
        for cashflow in &self.journal {
            for (asset, delta) in &cashflow.deltas {
                let entry = totals.entry(asset).or_default();
                *entry = add(*entry, *delta, "reconcile deltas")?;
            }
            if let Some(fee) = &cashflow.fee {
                let entry = fees.entry(&fee.asset).or_default();
                *entry = add(*entry, fee.amount, "reconcile fees")?;
            }
            let (quantity, cash) = match cashflow.side {
                Side::Buy => (cashflow.quantity, -cashflow.notional),
                Side::Sell => (-cashflow.quantity, cashflow.notional),
            };
            let entry = quantities.entry(&cashflow.instrument).or_default();
            *entry = add(*entry, quantity, "reconcile quantity")?;
            let entry = trade_cash.entry(&cashflow.instrument).or_default();
            *entry = add(*entry, cash, "reconcile trade cash")?;
        }

        for (asset, balance) in &self.balances {
            let expected = totals.get(asset).copied().unwrap_or_default();
            mismatch("balance.total", asset.as_str(), expected, balance.total)?;
            let expected = reserved.get(asset).copied().unwrap_or_default();
            mismatch(
                "balance.reserved",
                asset.as_str(),
                expected,
                balance.reserved,
            )?;
            if balance.total < balance.reserved {
                return Err(MarginError::ReconciliationMismatch {
                    check: "balance.total >= balance.reserved",
                    key: asset.to_string(),
                    expected: balance.reserved,
                    actual: balance.total,
                });
            }
            let expected = fees.get(asset).copied().unwrap_or_default();
            let actual = self.fees_paid.get(asset).copied().unwrap_or_default();
            mismatch("fees_paid", asset.as_str(), expected, actual)?;
        }
        for (id, position) in &self.positions {
            let expected = quantities.get(id).copied().unwrap_or_default();
            mismatch(
                "position.quantity",
                id.as_str(),
                expected,
                position.quantity,
            )?;
            let expected = trade_cash.get(id).copied().unwrap_or_default();
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
        Ok(())
    }
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

fn accumulate(deltas: &mut BTreeMap<Asset, Decimal>, asset: &Asset, delta: Decimal) -> Result<()> {
    let entry = deltas.entry(asset.clone()).or_default();
    *entry = add(*entry, delta, "delta accumulation")?;
    Ok(())
}

fn position_snapshot(
    instrument: &InstrumentId,
    quote: &Asset,
    position: Position,
) -> PositionSnapshot {
    PositionSnapshot {
        instrument: instrument.clone(),
        quote: quote.clone(),
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
fn apply_fill(
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
        let realized = sub(-signed_value, released, "realized pnl")?;
        (realized, sub(cost, released, "cost - released")?)
    } else {
        let close_value =
            mul_div_round(&[price, held], &[], quote_scale, rounding, "price * held")?;
        let close_value = if signed_value.is_sign_negative() {
            -close_value
        } else {
            close_value
        };
        let open_value = sub(signed_value, close_value, "open value")?;
        let realized = sub(-close_value, cost, "realized pnl")?;
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
