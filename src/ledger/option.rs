//! Options held in the ledger: premium at the fill, settlement by outcome.
//!
//! # Premium
//!
//! A position is signed contracts `Q` with a signed basis, as for a linear
//! contract: a fill of `n` contracts at premium `p` has the value
//! `N = rhe(p * n * M)` in the settlement asset, rounded half-even to its
//! scale, and the position and the realized PnL follow the same rules. The
//! difference is the cash: the premium **is** exchanged, `-N` for a buy and
//! `+N` for a sell, so the realized PnL moves no further cash. The basis of a
//! long is the premium paid, and that of a short (negative) the premium
//! received. Fills are mandatory, as for every derivative. A premium of zero
//! is a valid price: a worthless option can trade at it.
//!
//! # Settlement
//!
//! [`Ledger::settle_option`] closes `n` contracts, all or part of the
//! position, by an [`OptionOutcome`] the caller gives. The ledger prices
//! nothing and decides no exercise.
//!
//! | Outcome | Value `V` closed at | Cash in the settlement asset |
//! |---|---|---|
//! | [`OptionOutcome::Cash`] linear, at `P` | `rhe(n * M * d)` | `+V` long, `-V` short |
//! | [`OptionOutcome::Cash`] inverse, at `P` | `rhe(n * M * d / P)`, with its [`InverseResidual`] | `+V` long, `-V` short |
//! | [`OptionOutcome::Delivery`] | `0` | none here: a spot execution at the strike |
//! | [`OptionOutcome::Lapsed`] | `0` | none |
//!
//! with `d = max(0, P - K)` for a call and `max(0, K - P)` for a put. With
//! `released = rhe(basis * n / abs(Q))` (all of it on a full close), the
//! realized PnL is `+V - released` for a long and `-V - released` for a short.
//!
//! A delivery is booked as a spot execution of `n * M` at the strike, a buy
//! for a long call or a short put and a sell for a long put or a short call.
//! The premium therefore stays in the option's realized PnL and never enters
//! the cost of the underlying.
//!
//! An option has no terminal state. A fill executed before the expiry and
//! delivered after a settlement reopens a position, which is settled again
//! under another identifier.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use super::derivative::{add_wide, residual};
use super::{Balance, Execution, ExecutionCashflow, InverseResidual, Ledger, Position};
use super::{PositionSnapshot, position_snapshot};
use crate::decimal::{
    MAX_SCALE, Rounding, Wide, add, check_scale, decimal_to_wide, mul, mul_div_round,
    mul_div_round_wide, neg, product, sub, wide_to_decimal,
};
use crate::error::{MarginError, Result};
use crate::ids::{Asset, ExecutionId, InstrumentId};
use crate::instrument::{
    ContractSize, ExerciseStyle, Instrument, OptionDelivery, OptionInstrument, OptionMargining,
    OptionRight,
};
use financial_types::Action;

/// How an option's contracts are closed. See `docs/options.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionOutcome {
    /// A cash settled option, at this price of the underlying in the price
    /// asset. A price at or out of the money pays nothing. Positive.
    Cash {
        /// Settlement price of the underlying.
        price: Decimal,
    },
    /// A physical option exercised or assigned: the underlying is delivered
    /// against the strike as a spot execution.
    Delivery {
        /// A registered spot instrument with the option's underlying as base
        /// and its settlement asset as quote.
        spot: InstrumentId,
        /// Identifier of the spot execution, not used by anything else. It
        /// shares the namespace of execution identifiers.
        delivery_id: ExecutionId,
    },
    /// A physical option not exercised: it closes at no value. A cash settled
    /// option cannot lapse, so a payoff is never dropped by leaving out its
    /// price.
    Lapsed,
}

/// The settlement of option contracts, booked with
/// [`Ledger::settle_option`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionSettlement {
    /// Identifier. It shares the namespace of execution identifiers.
    pub id: ExecutionId,
    /// Option instrument.
    pub instrument: InstrumentId,
    /// Contracts closed, positive and at most the open position.
    pub contracts: Decimal,
    /// How they are closed.
    pub outcome: OptionOutcome,
    /// When the settlement applies, in milliseconds since the epoch. Before
    /// the expiry only an American option may settle, and it cannot lapse.
    pub as_of_ms: u64,
}

/// How one option settlement changed the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionSettlementCashflow {
    /// Identifier.
    pub id: ExecutionId,
    /// Position in the option settlement journal, starting at 0.
    pub sequence: u64,
    /// Option instrument.
    pub instrument: InstrumentId,
    /// Settlement asset.
    pub asset: Asset,
    /// The outcome booked.
    pub outcome: OptionOutcome,
    /// Contracts closed, signed like the position before the settlement.
    pub contracts: Decimal,
    /// The value `V` the contracts closed at: never negative, zero for a
    /// delivery or a lapse.
    pub value: Decimal,
    /// For an inverse option, the exact rest of `value`:
    /// `n * M * d / P = value + numerator / denominator`. `None` for a linear
    /// option, whose value is an exact product, for a delivery or a lapse, and
    /// when the rest does not fit a `Decimal`.
    pub residual: Option<InverseResidual>,
    /// Change of the settlement asset balance from the value alone: `+V` for
    /// a long, `-V` for a short. A delivery's cash is in `delivery`.
    pub cash: Decimal,
    /// Realized PnL of the close.
    pub realized_pnl: Decimal,
    /// Basis released, signed like the basis before the settlement.
    pub released_basis: Decimal,
    /// Position after the settlement.
    pub position_after: PositionSnapshot,
    /// The spot execution of a delivery.
    pub delivery: Option<ExecutionCashflow>,
    /// Total balance of the settlement asset after the settlement.
    pub balance_after: Decimal,
}

/// Result of [`Ledger::settle_option`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionSettlementBooking {
    /// The cashflow, recorded the first time the settlement was booked.
    pub cashflow: OptionSettlementCashflow,
    /// True when this call changed the ledger; false for a duplicate.
    pub is_new: bool,
    /// Assets this booking left below their reservations, with the shortfall
    /// it created. Empty when the balance covered it, and for a duplicate.
    pub shortfalls: BTreeMap<Asset, Decimal>,
}

/// A close of `contracts` out of the open `position` at `value`.
struct Close {
    position: Position,
    realized: Decimal,
    released: Decimal,
    /// Signed like the position before the close.
    signed_contracts: Decimal,
    /// `+value` for a long, `-value` for a short.
    cash: Decimal,
}

fn close_at_value(
    position: Position,
    contracts: Decimal,
    value: Wide,
    scale: u32,
) -> Result<Close> {
    let held = position.quantity.abs();
    let is_long = position.quantity > Decimal::ZERO;
    let basis = position.cost_basis;
    let released = if contracts == held {
        basis
    } else {
        mul_div_round(
            &[basis, contracts],
            &[held],
            scale,
            Rounding::HalfEven,
            "released basis",
        )?
    };
    let (signed_contracts, signed_value) = if is_long {
        (contracts, value)
    } else {
        (neg(contracts), value.negated())
    };
    let released_wide = decimal_to_wide(released, scale, "released basis")?;
    let realized = add_wide(signed_value, released_wide.negated(), "realized pnl")?;
    let realized = wide_to_decimal(realized, scale, "realized pnl")?;
    Ok(Close {
        position: Position {
            quantity: sub(position.quantity, signed_contracts, "contracts")?,
            cost_basis: sub(basis, released, "basis - released")?,
            realized_pnl: add(position.realized_pnl, realized, "realized pnl")?,
        },
        realized,
        released,
        signed_contracts,
        cash: wide_to_decimal(signed_value, scale, "option value")?,
    })
}

/// The value of `contracts` at the settlement price, rounded once, and the
/// exact rest of an inverse one.
fn cash_value(
    option: &OptionInstrument,
    contracts: Decimal,
    price: Decimal,
    scale: u32,
) -> Result<(Wide, Option<InverseResidual>)> {
    let multiplier = option.terms().multiplier();
    let intrinsic = option.intrinsic(price)?;
    match option.contract().margining {
        OptionMargining::Linear => Ok((
            mul_div_round_wide(
                &[contracts, multiplier, intrinsic],
                &[],
                scale,
                Rounding::HalfEven,
                "contracts * multiplier * intrinsic",
            )?,
            None,
        )),
        OptionMargining::Inverse => {
            let value = mul_div_round_wide(
                &[contracts, multiplier, intrinsic],
                &[price],
                scale,
                Rounding::HalfEven,
                "contracts * multiplier * intrinsic / price",
            )?;
            // The value is booked whether or not its rest can be represented.
            let rest = mul(multiplier, intrinsic, "multiplier * intrinsic")
                .and_then(|size| {
                    residual(
                        ContractSize::ContractValue(size),
                        price,
                        contracts,
                        value,
                        scale,
                    )
                })
                .ok()
                .flatten();
            Ok((value, rest))
        }
    }
}

/// The side of the spot execution that delivers a position.
fn delivery_side(right: OptionRight, is_long: bool) -> Action {
    match (right, is_long) {
        (OptionRight::Call, true) | (OptionRight::Put, false) => Action::Buy,
        (OptionRight::Call, false) | (OptionRight::Put, true) => Action::Sell,
    }
}

impl Ledger {
    /// Closes option contracts by an outcome the caller gives. See
    /// `docs/options.md`.
    ///
    /// The settlement is mandatory: a debit beyond the available balance is
    /// booked and reported in [`OptionSettlementBooking::shortfalls`]. A
    /// delivery books its spot execution and the close of the option in one
    /// atomic step. An identical settlement is a duplicate before any other
    /// check, and changes nothing.
    ///
    /// # Errors
    ///
    /// The ledger is unchanged on any error. Checked in this order:
    ///
    /// - [`MarginError::ConflictingEntry`]: the id was booked with different
    ///   content or as another kind of entry, or a delivery id is already used
    ///   or equal to the id.
    /// - [`MarginError::UnknownInstrument`]: the option or the delivery's spot
    ///   instrument is not registered.
    /// - [`MarginError::InvalidInput`]: the instrument is not an option; the
    ///   contracts are not positive, or exceed the open position; the outcome
    ///   does not match the option's delivery; the settlement is before the
    ///   expiry and the option is European or lapses; the settlement price is
    ///   not positive; or the delivery's instrument is not a spot on the
    ///   option's underlying and settlement asset.
    /// - [`MarginError::PrecisionExceeded`]: the contracts are finer than the
    ///   contract scale, the settlement price finer than
    ///   [`MAX_SCALE`], or the strike or the
    ///   quantity off the spot's grid, or the delivered cash finer than the
    ///   settlement asset's scale. A delivery is never rounded.
    /// - [`MarginError::Arithmetic`]: an amount overflows.
    #[expect(
        clippy::too_many_lines,
        reason = "one validate-then-commit sequence: the idempotency and input checks, then the close, then the commit"
    )]
    pub fn settle_option(
        &mut self,
        settlement: &OptionSettlement,
    ) -> Result<OptionSettlementBooking> {
        if let Some((previous, index)) = self.option_settlements.get(&settlement.id) {
            if previous == settlement {
                return Ok(OptionSettlementBooking {
                    cashflow: self
                        .option_settlement_journal
                        .get(*index)
                        .cloned()
                        .ok_or(MarginError::arithmetic("option settlement journal index"))?,
                    is_new: false,
                    shortfalls: BTreeMap::new(),
                });
            }
            return Err(conflict(&settlement.id));
        }
        if self.executions.contains_key(&settlement.id) || self.id_used_by_posting(&settlement.id) {
            return Err(conflict(&settlement.id));
        }
        if let OptionOutcome::Delivery { delivery_id, .. } = &settlement.outcome
            && (*delivery_id == settlement.id
                || self.executions.contains_key(delivery_id)
                || self.id_used_by_posting(delivery_id))
        {
            return Err(conflict(delivery_id));
        }
        let option = self.option_of(&settlement.instrument)?;
        let terms = option.terms();
        let contract = option.contract();
        let asset = terms.settlement().clone();
        let scale = self.asset_scale_or_err(&asset)?;

        if settlement.contracts <= Decimal::ZERO {
            return Err(MarginError::invalid_input("contracts", "must be positive"));
        }
        check_scale(settlement.contracts, terms.contract_scale(), "contracts")?;
        let position = self
            .positions
            .get(&settlement.instrument)
            .copied()
            .unwrap_or_default();
        if settlement.contracts > position.quantity.abs() {
            return Err(MarginError::invalid_input(
                "contracts",
                "must not exceed the open position",
            ));
        }
        match (&settlement.outcome, contract.delivery) {
            (OptionOutcome::Cash { .. }, OptionDelivery::Cash)
            | (OptionOutcome::Delivery { .. } | OptionOutcome::Lapsed, OptionDelivery::Physical) => {
            }
            (OptionOutcome::Cash { .. }, OptionDelivery::Physical) => {
                return Err(MarginError::invalid_input(
                    "outcome",
                    "a physical option is delivered or lapses",
                ));
            }
            (OptionOutcome::Delivery { .. } | OptionOutcome::Lapsed, OptionDelivery::Cash) => {
                return Err(MarginError::invalid_input(
                    "outcome",
                    "a cash settled option settles at a price",
                ));
            }
        }
        if settlement.as_of_ms < contract.expiry_ms {
            if contract.exercise == ExerciseStyle::European {
                return Err(MarginError::invalid_input(
                    "as_of_ms",
                    "a European option settles at or after its expiry",
                ));
            }
            if settlement.outcome == OptionOutcome::Lapsed {
                return Err(MarginError::invalid_input(
                    "as_of_ms",
                    "an option lapses at or after its expiry",
                ));
            }
        }
        let (value, rest) = match &settlement.outcome {
            OptionOutcome::Cash { price } => {
                if *price <= Decimal::ZERO {
                    return Err(MarginError::invalid_input("price", "must be positive"));
                }
                check_scale(*price, MAX_SCALE, "price")?;
                cash_value(&option, settlement.contracts, *price, scale)?
            }
            OptionOutcome::Delivery { .. } | OptionOutcome::Lapsed => (Wide::ZERO, None),
        };
        let close = close_at_value(position, settlement.contracts, value, scale)?;
        let delivery = self.delivery_execution(settlement, &option, position)?;

        // Only the value moves cash here; a delivery's cash is its execution.
        let balance = self.balance_of(&asset)?;
        let total = add(balance.total, close.cash, "balance + option value")?;
        let available_after = sub(total, balance.reserved, "total - reserved")?;
        let new_balance = Balance::new(total, balance.reserved)?;
        let mut shortfalls = BTreeMap::new();
        if close.cash < Decimal::ZERO && available_after < Decimal::ZERO {
            shortfalls.insert(asset.clone(), neg(available_after));
        }
        let value = wide_to_decimal(value, scale, "option value")?;
        let size = ContractSize::Multiplier(terms.multiplier());

        // The delivery checks everything it needs and changes nothing on an
        // error; it comes first because it is the only step that can fail.
        let delivered = match delivery {
            Some(execution) => {
                let (booking, delivery_shortfalls) = self.book(&execution, None, true)?;
                shortfalls.extend(delivery_shortfalls);
                Some(booking.cashflow().clone())
            }
            None => None,
        };

        // Commit: nothing below can fail.
        if delivered.is_none() {
            self.balances.insert(asset.clone(), new_balance);
        }
        self.positions
            .insert(settlement.instrument.clone(), close.position);
        let balance_after = self
            .balances
            .get(&asset)
            .map_or(Decimal::ZERO, |balance| balance.total);
        let index = self.option_settlement_journal.len();
        let cashflow = OptionSettlementCashflow {
            id: settlement.id.clone(),
            sequence: index as u64,
            instrument: settlement.instrument.clone(),
            asset: asset.clone(),
            outcome: settlement.outcome.clone(),
            contracts: close.signed_contracts,
            value,
            residual: rest,
            cash: close.cash,
            realized_pnl: close.realized,
            released_basis: close.released,
            position_after: position_snapshot(
                &settlement.instrument,
                &asset,
                crate::instrument::PositionKind::Option,
                size,
                close.position,
            ),
            delivery: delivered,
            balance_after,
        };
        self.option_settlement_journal.push(cashflow.clone());
        self.option_settlements
            .insert(settlement.id.clone(), (settlement.clone(), index));
        Ok(OptionSettlementBooking {
            cashflow,
            is_new: true,
            shortfalls,
        })
    }

    /// The journal of option settlements, in booking order.
    #[must_use]
    pub fn option_settlement_journal(&self) -> &[OptionSettlementCashflow] {
        &self.option_settlement_journal
    }

    fn option_of(&self, id: &InstrumentId) -> Result<OptionInstrument> {
        match self.instruments.get(id) {
            Some(Instrument::Option(option)) => Ok(option.clone()),
            Some(
                Instrument::Spot(_)
                | Instrument::LinearFuture(_)
                | Instrument::LinearPerpetual(_)
                | Instrument::InverseFuture(_)
                | Instrument::InversePerpetual(_),
            ) => Err(MarginError::invalid_input(
                "instrument",
                "only an option settles with settle_option",
            )),
            None => Err(MarginError::UnknownInstrument {
                instrument: id.clone(),
            }),
        }
    }

    /// The spot execution of a delivery, checked so that what it moves is
    /// exactly `n * M` of the underlying and `K * n * M` of the settlement
    /// asset; `None` for any other outcome.
    fn delivery_execution(
        &self,
        settlement: &OptionSettlement,
        option: &OptionInstrument,
        position: Position,
    ) -> Result<Option<Execution>> {
        let OptionOutcome::Delivery { spot, delivery_id } = &settlement.outcome else {
            return Ok(None);
        };
        let contract = option.contract();
        let settlement_asset = option.terms().settlement();
        let instrument = match self.instruments.get(spot) {
            Some(Instrument::Spot(instrument)) => instrument,
            Some(
                Instrument::LinearFuture(_)
                | Instrument::LinearPerpetual(_)
                | Instrument::InverseFuture(_)
                | Instrument::InversePerpetual(_)
                | Instrument::Option(_),
            ) => {
                return Err(MarginError::invalid_input(
                    "spot",
                    "a delivery is booked on a spot instrument",
                ));
            }
            None => {
                return Err(MarginError::UnknownInstrument {
                    instrument: spot.clone(),
                });
            }
        };
        if *instrument.base() != contract.underlying || instrument.quote() != settlement_asset {
            return Err(MarginError::invalid_input(
                "spot",
                "must trade the option's underlying against its settlement asset",
            ));
        }
        let quantity = product(
            &[settlement.contracts, option.terms().multiplier()],
            "contracts * multiplier",
        )?;
        let cash = product(&[contract.strike, quantity], "strike * quantity")?;
        check_scale(
            cash,
            self.asset_scale_or_err(settlement_asset)?,
            "delivery cash",
        )?;
        let side = delivery_side(contract.right, position.quantity > Decimal::ZERO);
        Ok(Some(
            Execution::new(
                delivery_id.clone(),
                spot.clone(),
                side,
                contract.strike,
                quantity,
            )
            .with_executed_at(settlement.as_of_ms),
        ))
    }
}

fn conflict(id: &ExecutionId) -> MarginError {
    MarginError::ConflictingEntry {
        kind: "option settlement",
        id: id.to_string(),
    }
}
