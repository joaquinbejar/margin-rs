//! Pretrade risk gate: reservations and hard limits for every order.
//!
//! The [`RiskGate`] owns the [`Ledger`]. Every order (quote, hedge or a
//! policy's proposal) is submitted to [`RiskGate::submit`], which returns a
//! [`RiskDecision`]: accepted, reduced or rejected, with typed reasons. An
//! approved order holds a reservation in the ledger for its worst case
//! debit until the order ends:
//!
//! - a fill shrinks the reservation to what the remaining quantity needs,
//!   in the same atomic step that books the fill, so a repeated fill
//!   message adjusts it exactly once; fills carry the order's cumulative
//!   quantity ([`FillReport`]) so late and impossible reports are told
//!   apart;
//! - a venue reject or a confirmed cancel releases what is left;
//! - a requested cancel keeps the reservation, because the order can still
//!   fill until the venue confirms the cancel.
//!
//! The ledger is only reachable read only ([`RiskGate::ledger`]), so there
//! is no path that books or reserves around the gate:
//!
//! ```compile_fail,E0596
//! # use margin_rs::ledger::{Execution, Ledger, Action};
//! # use margin_rs::risk::RiskGate;
//! # use margin_rs::{ExecutionId, InstrumentId, Decimal};
//! # fn bypass(gate: &mut RiskGate, execution: &Execution) {
//! // Does not compile: the gate only lends `&Ledger`.
//! gate.ledger().book_execution(execution);
//! # }
//! ```
//!
//! # Example
//!
//! ```
//! use margin_rs::ledger::{Execution, Ledger, Action};
//! use margin_rs::risk::{FillReport, InstrumentLimits, OrderRequest, RiskDecision, RiskGate};
//! use margin_rs::{Asset, Decimal, ExecutionId, InstrumentId, OrderId, SpotInstrument, TransferId};
//!
//! # fn main() -> margin_rs::Result<()> {
//! let (usdc, btc) = (Asset::new("USDC")?, Asset::new("BTC")?);
//! let pair = InstrumentId::new("BTC-USDC")?;
//! let mut ledger = Ledger::default();
//! ledger.register_asset(usdc.clone(), 6)?;
//! ledger.register_asset(btc.clone(), 8)?;
//! ledger.register_instrument(SpotInstrument::new(pair.clone(), btc, usdc.clone(), 2, 8)?)?;
//!
//! let mut gate = RiskGate::new(ledger);
//! gate.deposit(TransferId::new("d1")?, usdc.clone(), Decimal::from(1000))?;
//! gate.set_limits(&pair, InstrumentLimits { max_position: Some(Decimal::from(5)), ..Default::default() })?;
//!
//! let order = OrderRequest::new(OrderId::new("q1")?, pair.clone(), Action::Buy, Decimal::from(100), Decimal::from(8))
//!     .reducible();
//! let decision = gate.submit(&order, 0);
//! assert_eq!(decision.approved_quantity(), Some(Decimal::from(5)));
//! assert_eq!(gate.ledger().balance(&usdc).map(|b| b.reserved), Some(Decimal::from(500)));
//!
//! let fill = Execution::new(ExecutionId::new("e1")?, pair, Action::Buy, Decimal::from(100), Decimal::from(5));
//! gate.on_fill(&order.id, &FillReport::new(fill, Decimal::from(5)))?;
//! assert_eq!(gate.ledger().balance(&usdc).map(|b| (b.total, b.reserved)), Some((Decimal::from(500), Decimal::ZERO)));
//! # Ok(())
//! # }
//! ```
//!
//! # Reservation size
//!
//! Fixed per order at admission ([`ReservationTerms`]): a whole number of
//! lots times a per lot bound, in the asset the order pays. A fee is
//! reserved in the asset it is charged in when the order pays that asset,
//! and netted from the fill when the order receives it
//! ([`InstrumentLimits::fee_asset`]):
//!
//! - buys reserve quote: the bound covers the quote debited by any
//!   fragmentation of the order at the limit price or better, including the
//!   per fill rounding of the notional and, for a fee in quote, a fee of up
//!   to `max_fee_rate` rounded up. A fee in base is paid from the base the
//!   fill credits, in the same booking, so nothing is reserved for it;
//! - sells reserve base: the base quantity itself, plus, for a fee in base,
//!   a fee of up to `max_fee_rate` of each lot rounded up. A fee in quote is
//!   paid from the quote the fill credits.
//!
//! # Limits on joint exposure
//!
//! Limits apply to the worst case position: the current position plus every
//! live order on the same side (including orders with a cancel pending,
//! which may still fill) plus the new order. See [`InstrumentLimits`].
//!
//! # Circuit breaker and quote withdrawal
//!
//! [`RiskGate::trip`] halts trading: every later submit is rejected with
//! [`RiskReason::Halted`], and the live orders that have no cancel pending
//! yet are moved to cancel pending and returned, so the caller sends
//! exactly those cancels (quote withdrawal). Their reservations stay until
//! each cancel is confirmed, and fills that race the cancels are booked
//! normally. [`RiskGate::resume`] lifts the halt. The gate trips itself with
//! [`HaltReason::AccountingBreach`] when a real fill overdraws the account
//! and with [`HaltReason::ExecutionAnomaly`] when a real fill does not match
//! its order; in both cases the fill is booked, never lost, and the quotes
//! are withdrawn exactly as by [`RiskGate::trip`]: the orders to cancel are
//! returned in [`FillOutcome::withdraw`].

mod decision;

use std::collections::BTreeMap;

use rust_decimal::Decimal;

pub use decision::{
    ExecutionAnomaly, FillReport, HaltReason, OrderPurpose, OrderRequest, RiskDecision, RiskReason,
};

use crate::decimal::{
    Rounding, Wide, add, check_scale, mul, mul_div_round, mul_div_round_wide,
    mul_wide_div_round_wide, neg, sub, sum, sum_products, sum_terms_round,
    sum_terms_with_wide_round_wide, wide_from_u128, wide_to_decimal,
};
use crate::direction::Dir;
use crate::error::{MarginError, Result};
use crate::ids::{Asset, InstrumentId, OrderId, ReservationId, TransferId};
use crate::instrument::{Instrument, SpotInstrument};
use crate::ledger::{
    Action, Booking, Execution, ExpiryCashflow, ExpirySettlement, FundingCashflow, FundingPayment,
    Ledger, OptionSettlement, OptionSettlementBooking, OverdraftBooking, PostingBooking,
};

/// The asset a venue charges the fees of an instrument in, for the risk
/// gate's reservations. See [`InstrumentLimits::fee_asset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum FeeAsset {
    /// The quote asset. The default.
    #[default]
    Quote,
    /// The base asset.
    Base,
}

/// How a venue rounds the fees it charges in the base asset, declared so
/// that the gate can reserve less than the worst case of rounding every
/// fill up. See [`InstrumentLimits::fee_rounding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FeeRounding {
    /// Each fill of `n` is charged at most `round(max_fee_rate * n)` on the
    /// base grid, in `mode`. With `min_fill`, every fill except the one that
    /// completes the order is at least `min_fill` (a positive multiple of
    /// the lot).
    PerFill {
        /// How each fill's fee is rounded.
        mode: Rounding,
        /// The smallest fill the venue sends, except the last.
        min_fill: Option<Decimal>,
    },
    /// The fees of an order are charged on its cumulative quantity at an
    /// exact, fixed rate: after a cumulative `c` the venue has charged
    /// exactly `round(rate * c)` on the base grid, in `mode`, so a fill of
    /// `n` after `c` is charged `round(rate * (c + n)) - round(rate * c)`.
    /// `max_fee_rate` is not used. When the rate is not known exactly, use
    /// [`FeeRounding::PerFill`].
    Cumulative {
        /// How the cumulative fee is rounded.
        mode: Rounding,
        /// The exact rate, in `[0, 1)`.
        rate: Decimal,
    },
}

impl Default for FeeRounding {
    /// Every fill rounded up, with no minimum fill: the worst case, and the
    /// gate's behaviour before fee rounding could be declared.
    fn default() -> Self {
        Self::PerFill {
            mode: Rounding::AwayFromZero,
            min_fill: None,
        }
    }
}

/// The base fee terms of an order whose fees are charged in the base asset,
/// fixed at admission.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaseFeeTerms {
    /// `max_fee_rate` for [`FeeRounding::PerFill`]; the exact rate for
    /// [`FeeRounding::Cumulative`].
    pub rate: Decimal,
    /// The declared rounding.
    pub rounding: FeeRounding,
    /// The base asset the fees are charged in.
    pub base: Asset,
}

/// What a fill did outside the venue's declared fees. The fill is always
/// booked; the gate records the first such breach of an instrument and
/// stops admitting orders under that declaration (see
/// [`RiskGate::fee_declaration_breaches`]). Repeated breaches on one
/// instrument mean the declaration is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FeeBreach {
    /// The base fee is above its declared bound.
    AboveBound {
        /// The declared bound for this fill.
        bound: Decimal,
        /// The fee charged.
        fee: Decimal,
    },
    /// With cumulative charging at an exact rate, the base fee differs from
    /// the declared increment, above or below it (a missing fee conforms
    /// only when the increment is zero).
    DeclaredFeeMismatch {
        /// The declared increment for this fill.
        expected: Decimal,
        /// The fee charged in base.
        fee: Decimal,
    },
    /// A fill below the declared minimum that does not complete the order.
    BelowMinimumFill {
        /// The declared minimum fill.
        min_fill: Decimal,
        /// The fill's quantity.
        quantity: Decimal,
    },
    /// The fee is charged in an asset other than the declared base asset.
    UnexpectedAsset {
        /// The asset charged.
        asset: Asset,
        /// The fee charged.
        fee: Decimal,
    },
}

/// The first fee breach recorded for an instrument, with the declaration
/// it breached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeDeclarationBreach {
    /// The order whose fill breached it.
    pub order: OrderId,
    /// What happened.
    pub breach: FeeBreach,
    /// The declaration breached: the order's base fee terms.
    pub declaration: BaseFeeTerms,
}

/// The largest mantissa of a `Decimal`, `2^96 - 1`.
const MAX_MANTISSA: i128 = 79_228_162_514_264_337_593_543_950_335;

/// Hard limits for one instrument. `None` disables a limit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstrumentLimits {
    /// Maximum quantity of a single order.
    pub max_order_quantity: Option<Decimal>,
    /// Maximum absolute worst case position, in base units.
    pub max_position: Option<Decimal>,
    /// Maximum absolute worst case position valued at the order price, in
    /// the quote asset.
    pub max_position_notional: Option<Decimal>,
    /// Maximum number of live orders.
    pub max_open_orders: Option<usize>,
    /// Minimum order quantity; defaults to one lot (`10^-quantity_scale`).
    pub min_quantity: Option<Decimal>,
    /// Highest fee rate the venue may charge, as a fraction (0.001 is
    /// 10 bp). Zero by default. The gate assumes that the fee of a fill is
    /// charged in [`InstrumentLimits::fee_asset`] and is at most:
    ///
    /// - with [`FeeAsset::Quote`], `max_fee_rate` times the fill's booked
    ///   notional, rounded up to the quote grid. It is reserved for buys;
    ///   sells pay it from the quote they receive;
    /// - with [`FeeAsset::Base`], `max_fee_rate` times the fill's quantity,
    ///   rounded up to the base grid. It is reserved for sells, per lot:
    ///   `lot + round_up(max_fee_rate * lot)`; buys pay it from the base
    ///   they receive, so it must be below 1.
    ///
    /// Rounding up per fill makes a fine lot expensive: with a lot equal to
    /// the base grid (1e-8 BTC on an 8 digit grid) and any positive rate,
    /// every lot can be charged one more unit, so a sell reserves twice its
    /// quantity unless the venue declares a rounding
    /// ([`InstrumentLimits::fee_rounding`]). Fixed per
    /// order at admission: later changes do not affect live orders.
    pub max_fee_rate: Decimal,
    /// Asset fees are charged in. Quote by default. Fixed per order at
    /// admission, like `max_fee_rate`.
    pub fee_asset: FeeAsset,
    /// How the venue rounds fees in the base asset. The default rounds
    /// every fill up, the worst case; a declared rounding lets a sell with
    /// fees in base reserve less. Only [`FeeAsset::Base`] accepts a value
    /// other than the default. Fixed per order at admission.
    ///
    /// The reservation of a sell of 2 BTC on an 8 digit grid with a lot of
    /// 1e-8 and a rate of 0.001 is 4 BTC by default, 3.002 with a half mode
    /// and 2.002 toward zero or cumulative; see `docs/risk-gate.md`.
    pub fee_rounding: FeeRounding,
    /// Maximum relative distance between the limit price and the last mark
    /// (0.05 is 5 %). Requires market data.
    pub price_band: Option<Decimal>,
    /// Maximum age of the last mark, in milliseconds. Requires market data.
    pub max_data_age_ms: Option<u64>,
}

impl InstrumentLimits {
    fn validate(&self, spot: &SpotInstrument) -> Result<()> {
        let positive = |value: Option<Decimal>, field: &'static str| -> Result<()> {
            match value {
                Some(v) if v <= Decimal::ZERO => {
                    Err(MarginError::invalid_input(field, "must be positive"))
                }
                _ => Ok(()),
            }
        };
        positive(self.max_order_quantity, "max_order_quantity")?;
        positive(self.max_position, "max_position")?;
        positive(self.max_position_notional, "max_position_notional")?;
        positive(self.min_quantity, "min_quantity")?;
        positive(self.price_band, "price_band")?;
        if let Some(min) = self.min_quantity {
            check_scale(min, spot.quantity_scale(), "min_quantity")?;
        }
        if self.max_fee_rate < Decimal::ZERO {
            return Err(MarginError::invalid_input(
                "max_fee_rate",
                "must not be negative",
            ));
        }
        match (self.fee_asset, self.fee_rounding) {
            (
                _,
                FeeRounding::PerFill {
                    min_fill: Some(m), ..
                },
            ) if m <= Decimal::ZERO => {
                return Err(MarginError::invalid_input("min_fill", "must be positive"));
            }
            (
                _,
                FeeRounding::PerFill {
                    min_fill: Some(m), ..
                },
            ) => {
                check_scale(m, spot.quantity_scale(), "min_fill")?;
            }
            (_, FeeRounding::Cumulative { rate, .. })
                if rate < Decimal::ZERO || rate >= Decimal::ONE =>
            {
                return Err(MarginError::invalid_input("fee rate", "must be in [0, 1)"));
            }
            (_, FeeRounding::PerFill { min_fill: None, .. } | FeeRounding::Cumulative { .. }) => {}
        }
        match self.fee_asset {
            FeeAsset::Quote => {
                if self.fee_rounding != FeeRounding::default() {
                    return Err(MarginError::invalid_input(
                        "fee_rounding",
                        "only fees charged in the base asset can declare a rounding",
                    ));
                }
            }
            // A buy pays a base fee from the base it receives: the fee must
            // stay below the quantity.
            FeeAsset::Base => {
                if self.max_fee_rate >= Decimal::ONE {
                    return Err(MarginError::invalid_input(
                        "max_fee_rate",
                        "must be below 1 for fees charged in the base asset",
                    ));
                }
            }
        }
        if self.max_open_orders == Some(0) {
            return Err(MarginError::invalid_input(
                "max_open_orders",
                "must be positive",
            ));
        }
        Ok(())
    }
}

/// Lifecycle state of an order known to the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderState {
    /// Approved and sent; not yet acknowledged by the venue.
    PendingNew,
    /// Acknowledged and resting.
    Open,
    /// A cancel was requested; the order may still fill.
    CancelPending,
    /// Completely filled.
    Filled,
    /// Cancel confirmed.
    Canceled,
    /// Rejected by the gate or by the venue.
    Rejected,
}

impl OrderState {
    /// True while the order may still fill.
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::PendingNew | Self::Open | Self::CancelPending)
    }

    /// Name used in errors.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::PendingNew => "pending_new",
            Self::Open => "open",
            Self::CancelPending => "cancel_pending",
            Self::Filled => "filled",
            Self::Canceled => "canceled",
            Self::Rejected => "rejected",
        }
    }
}

/// How an order's reservation is computed. Fixed when the order is
/// admitted, so later limit changes never release budget that a live order
/// may still need.
///
/// The reservation of `remaining` is `round_up(remaining / lot * per_lot)`
/// on the asset grid:
///
/// - sell: `per_lot` of the base asset is `lot` with a fee in quote (the
///   reservation is the remaining quantity itself), and
///   `lot + round_up(r * lot)` with a fee in base, `r = max_fee_rate`;
/// - buy: `per_lot` bounds the quote debited per lot by any fill at the
///   limit price or better, whatever the fragmentation. With a fee in base
///   the fee is paid from the base the fill credits and `r` is 0 here. It
///   is the smaller of two valid bounds, with `x = price * lot` and
///   `r = max_fee_rate`:
///   - `round_up(x) + round_up(r * round_up(x))`, on the grid (rounding up
///     is subadditive);
///   - `(1 + r) * (x + h) + (ulp if r > 0)`, exact, where `h` is the largest
///     error of the ledger's notional rounding (half an ulp for the half
///     modes, 0 toward zero, one ulp away from zero).
///
/// A fill of `k` lots debits a grid amount of at most `k * per_lot`, and the
/// reservation falls by at least the grid floor of that, so each fill frees
/// at least what it debits and the reservation covers any sequence of
/// fills. When `x` is on the grid (for example 100 USDC at a 1e-8 lot) the
/// first bound is exact and nothing is over reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationTerms {
    /// Asset reserved: quote for buys, base for sells.
    pub asset: Asset,
    /// Scale of `asset`.
    pub scale: u32,
    /// Lot of the instrument.
    pub lot: Decimal,
    /// Amount reserved per open lot.
    pub per_lot: Decimal,
    /// The base fee terms when fees are charged in the base asset (for a
    /// sell they shape the reservation; for a buy they bound the fee that
    /// the fill pays from what it receives). `None` for fees in quote.
    pub base_fee: Option<BaseFeeTerms>,
}

impl ReservationTerms {
    /// Reservation for `remaining` open quantity (a multiple of the lot),
    /// as a whole order (with a cumulative base fee, nothing filled yet). A
    /// live order's reservation is [`ReservationTerms::amount_of_order`].
    /// Both are the hold `H`: the smallest Decimal at least the exact
    /// requirement on the asset grid (see `amount_of_order`).
    ///
    /// # Errors
    ///
    /// [`MarginError::Arithmetic`] if no Decimal is at least the
    /// requirement.
    pub fn amount(&self, remaining: Decimal) -> Result<Decimal> {
        self.hold(self.amount_of_lots(self.lots(remaining)?)?)
    }

    /// Reservation of an order of `quantity` that has `filled` so far (the
    /// real cumulative, overfills and late fills included): zero once
    /// nothing is left. The exact requirement, in units of the asset grid,
    /// is, for a sell with fees in base:
    ///
    /// - per fill: `R + min(A, B)` rounded up, with `R` the remaining
    ///   quantity, `A = (R / lot) * round_up(r * lot)` (every fill rounded
    ///   up, valid for any mode) and `B = r * R + h * ceil(R / m)`, where
    ///   `h` is one unit of the base grid when rounding away from zero,
    ///   half of it in a half mode and 0 toward zero, and `m` is the
    ///   minimum fill (the lot if none). Each fill of at least `m` (or the
    ///   last) frees at least its fee under each bound, so under their
    ///   minimum too: when the minimum switches from `A` to `B`, the fall is
    ///   `A(R) - B(R - n) >= A(R) - A(R - n)`, and the other way round.
    ///   With the default rounding `A <= B`, the bound before declared
    ///   rounding;
    /// - cumulative: `R + round(r * quantity) - round(r * filled)`, exactly
    ///   the fees still to be charged;
    ///
    /// and `R * per_lot / lot` otherwise. Every count and bound stays wide;
    /// the reservation is the hold `H`, the smallest Decimal at least that
    /// requirement (it equals it whenever the requirement is a Decimal).
    /// `H` is monotone, and a fill stays covered: if the order's hold `B` is
    /// at least the requirement `A(R)`, and a fill within the declaration
    /// debits `d` with `A(R') <= A(R) - d`, then `A(R') <= B - d`; whenever
    /// `B - d` is a Decimal (a state the ledger can book), `H(A(R')) <= B -
    /// d` by the definition of `H`: the new hold stays within what is left of
    /// the old one after the debit, so the fill causes no shortfall.
    ///
    /// # Errors
    ///
    /// [`MarginError::Arithmetic`] if no Decimal is at least the
    /// requirement.
    pub fn amount_of_order(&self, quantity: Decimal, filled: Decimal) -> Result<Decimal> {
        if filled >= quantity {
            return Ok(Decimal::ZERO);
        }
        let remaining = self
            .lots(quantity)?
            .checked_add(self.lots(filled)?.negated())
            .ok_or(MarginError::arithmetic("remaining lots"))?;
        let units = match self.sell_base_fee() {
            Some(BaseFeeTerms {
                rate,
                rounding: FeeRounding::Cumulative { mode, .. },
                ..
            }) => {
                let left = mul_wide_div_round_wide(
                    remaining,
                    &[self.lot],
                    &[],
                    self.scale,
                    Rounding::AwayFromZero,
                    "remaining",
                )?;
                let total =
                    mul_div_round_wide(&[*rate, quantity], &[], self.scale, *mode, "order fee")?;
                let charged =
                    mul_div_round_wide(&[*rate, filled], &[], self.scale, *mode, "charged fee")?;
                left.checked_add(total)
                    .and_then(|w| w.checked_add(charged.negated()))
                    .ok_or(MarginError::arithmetic("remaining + fees to charge"))?
            }
            _ => self.amount_of_lots(remaining)?,
        };
        self.hold(units)
    }

    /// `quantity / lot`, a whole number of lots, as a wide integer.
    fn lots(&self, quantity: Decimal) -> Result<Wide> {
        mul_div_round_wide(&[quantity], &[self.lot], 0, Rounding::TowardZero, "lots")
    }

    /// The hold for a requirement of `units` grid units: the smallest Decimal
    /// at least `units * 10^-scale`, the requirement itself when it is one,
    /// otherwise rounded up to the fewest digits that make it one.
    fn hold(&self, units: Wide) -> Result<Decimal> {
        for digits in 0..=self.scale {
            let coarse = mul_wide_div_round_wide(
                units,
                &[Decimal::new(1, digits)],
                &[],
                0,
                Rounding::AwayFromZero,
                "hold",
            )?;
            let scale = self
                .scale
                .checked_sub(digits)
                .ok_or(MarginError::arithmetic("hold"))?;
            if let Ok(hold) = wide_to_decimal(coarse, scale, "hold") {
                return Ok(hold);
            }
        }
        Err(MarginError::arithmetic("reservation"))
    }

    /// The base fee terms when they shape this reservation: a sell, which
    /// reserves the base asset its fees are charged in.
    fn sell_base_fee(&self) -> Option<&BaseFeeTerms> {
        self.base_fee.as_ref().filter(|fee| fee.base == self.asset)
    }

    /// Largest multiple of the lot, at most `max`, whose reservation as a
    /// whole order ([`ReservationTerms::amount`], the hold) fits in
    /// `available`. The hold is monotone in the quantity, so it is found
    /// exactly by an integer bisection over lot counts, with every count and
    /// requirement kept wide (a candidate with no Decimal hold does not
    /// fit). The answer is converted once; if it has more digits
    /// than a Decimal holds, it is the largest multiple of the lot below it
    /// that does.
    ///
    /// # Errors
    ///
    /// [`MarginError::Arithmetic`] if a quantity cannot be represented.
    pub fn affordable(&self, available: Decimal, max: Decimal) -> Result<Decimal> {
        // A candidate fits when its hold is at most the available balance;
        // the hold is monotone, and a requirement with no Decimal above it
        // does not fit. Lot counts and requirements stay wide.
        let fits = |lots: Wide| -> Result<bool> {
            match self.hold(self.amount_of_lots(lots)?) {
                Ok(hold) => Ok(hold <= available),
                Err(MarginError::Arithmetic { .. }) => Ok(false),
                Err(other) => Err(other),
            }
        };
        let one = wide_from_u128(1);
        let mut hi = mul_div_round_wide(
            &[max.max(Decimal::ZERO)],
            &[self.lot],
            0,
            Rounding::TowardZero,
            "max lots",
        )?;
        let best = if fits(hi)? {
            hi
        } else {
            // Integer bisection over lot counts: `lo` fits, `hi` does not,
            // and the gap shrinks every step.
            let mut lo = Wide::ZERO;
            loop {
                let gap = hi
                    .checked_add(lo.negated())
                    .ok_or(MarginError::arithmetic("gap"))?;
                if gap.cmp_signed(&one).is_le() {
                    break lo;
                }
                let half = mul_wide_div_round_wide(
                    gap,
                    &[],
                    &[Decimal::TWO],
                    0,
                    Rounding::TowardZero,
                    "half gap",
                )?;
                let mid = lo.checked_add(half).ok_or(MarginError::arithmetic("mid"))?;
                if fits(mid)? {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
        };
        self.quantity_of_lots(best)
    }

    /// The reservation of `lots` whole lots as a whole order, in grid units,
    /// wide: the same bounds as [`ReservationTerms::amount`].
    fn amount_of_lots(&self, lots: Wide) -> Result<Wide> {
        let scale = self.scale;
        let linear = || {
            sum_terms_with_wide_round_wide(
                &[],
                &[(lots, &[self.per_lot])],
                scale,
                Rounding::AwayFromZero,
                "lots * per lot",
            )
        };
        let Some(fee) = self.sell_base_fee() else {
            return linear();
        };
        match fee.rounding {
            FeeRounding::PerFill { mode, min_fill } => {
                let ulp = Decimal::new(1, scale);
                let half = match mode {
                    Rounding::AwayFromZero => Decimal::ONE,
                    Rounding::HalfEven | Rounding::HalfAwayFromZero => Decimal::new(5, 1),
                    Rounding::TowardZero => Decimal::ZERO,
                };
                let m = min_fill.map_or(self.lot, |m| m.max(self.lot));
                let fills = mul_wide_div_round_wide(
                    lots,
                    &[self.lot],
                    &[m],
                    0,
                    Rounding::AwayFromZero,
                    "fills",
                )?;
                let by_lot = linear()?;
                let by_fill = sum_terms_with_wide_round_wide(
                    &[],
                    &[
                        (lots, &[self.lot]),
                        (lots, &[fee.rate, self.lot]),
                        (fills, &[ulp, half]),
                    ],
                    scale,
                    Rounding::AwayFromZero,
                    "lots + fee bound",
                )?;
                Ok(if by_fill.cmp_signed(&by_lot).is_lt() {
                    by_fill
                } else {
                    by_lot
                })
            }
            FeeRounding::Cumulative { mode, .. } => {
                let quantity = mul_wide_div_round_wide(
                    lots,
                    &[self.lot],
                    &[],
                    scale,
                    Rounding::AwayFromZero,
                    "quantity",
                )?;
                let fee_part = mul_wide_div_round_wide(
                    lots,
                    &[fee.rate, self.lot],
                    &[],
                    scale,
                    mode,
                    "order fee",
                )?;
                quantity
                    .checked_add(fee_part)
                    .ok_or(MarginError::arithmetic("quantity + fee"))
            }
        }
    }

    /// The largest multiple of the lot that a Decimal represents and that is
    /// at most `lots * lot`: `lots * lot` itself when it fits, otherwise the
    /// best candidate over scales `s <= lot.scale()`, either `lots * lot`
    /// truncated to `s` digits or, when that mantissa is too large, the
    /// largest mantissa at `s` (which is then below it), each floored to the
    /// lot.
    fn quantity_of_lots(&self, lots: Wide) -> Result<Decimal> {
        let floor_to_lot = |q: Decimal| -> Result<Decimal> {
            let rest = q
                .checked_rem(self.lot)
                .ok_or(MarginError::arithmetic("quantity % lot"))?;
            sub(q, rest, "quantity - rest")
        };
        // From the finest scale down: the first scale at which the
        // truncation is a Decimal gives the best value at that scale and
        // above every coarser one; finer scales contribute their largest
        // Decimal, which is below the value.
        let mut best = Decimal::ZERO;
        for scale in (0..=self.lot.scale()).rev() {
            let value = mul_wide_div_round_wide(
                lots,
                &[self.lot],
                &[],
                scale,
                Rounding::TowardZero,
                "quantity",
            )?;
            if let Ok(q) = wide_to_decimal(value, scale, "quantity") {
                return Ok(best.max(floor_to_lot(q)?));
            }
            let largest = Decimal::try_from_i128_with_scale(MAX_MANTISSA, scale)
                .map_err(|_| MarginError::arithmetic("largest decimal"))?;
            best = best.max(floor_to_lot(largest)?);
        }
        Ok(best)
    }
}

/// An order known to the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRecord {
    /// Identifier.
    pub id: OrderId,
    /// Instrument.
    pub instrument: InstrumentId,
    /// Direction.
    pub side: Action,
    /// Limit price.
    pub price: Decimal,
    /// Approved quantity (zero for an order rejected by the gate).
    pub quantity: Decimal,
    /// Quantity filled so far.
    pub filled: Decimal,
    /// Current state.
    pub state: OrderState,
    /// Decision taken at submission.
    pub decision: RiskDecision,
    /// Reservation terms fixed at admission; `None` if the gate rejected
    /// the order.
    pub terms: Option<ReservationTerms>,
}

impl OrderRecord {
    /// Quantity still open: `quantity - filled`, zero once filled, and never
    /// negative, even after an overfill (where `filled > quantity`).
    ///
    /// # Errors
    ///
    /// [`MarginError::Arithmetic`] if `quantity - filled` cannot be
    /// represented exactly. Records kept by the [`RiskGate`] never fail, but
    /// the fields are public, so any values can be passed.
    pub fn remaining(&self) -> Result<Decimal> {
        if self.filled >= self.quantity {
            return Ok(Decimal::ZERO);
        }
        sub(self.quantity, self.filled, "quantity - filled")
    }

    fn reservation_id(&self) -> Result<ReservationId> {
        ReservationId::new(self.id.as_str())
    }
}

/// Result of [`RiskGate::on_fill`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FillOutcome {
    /// The booking, new or duplicate.
    pub booking: Booking,
    /// What was wrong with the fill, if anything; the gate halted.
    pub anomalies: Vec<ExecutionAnomaly>,
    /// Assets the fill overdrew, with the shortfall; the gate halted.
    pub shortfalls: BTreeMap<Asset, Decimal>,
    /// Orders whose cancel the caller must send because this fill halted
    /// the gate: every live order that had no cancel pending, now
    /// [`OrderState::CancelPending`] (as returned by [`RiskGate::trip`]).
    /// Empty when the fill did not halt the gate, for a duplicate, and when
    /// every live order already had a cancel pending.
    pub withdraw: Vec<OrderId>,
    /// What the fill did outside the venue's declared base fees (empty for
    /// fees in quote and for a duplicate). The fill is booked anyway; the
    /// first breach latches the instrument
    /// ([`RiskGate::fee_declaration_breaches`]).
    pub fee_breaches: Vec<FeeBreach>,
}

impl FillOutcome {
    /// True for a new fill that matched its order and was covered.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.booking.is_new() && self.anomalies.is_empty() && self.shortfalls.is_empty()
    }
}

/// The pretrade risk gate. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct RiskGate {
    ledger: Ledger,
    limits: BTreeMap<InstrumentId, InstrumentLimits>,
    marks: BTreeMap<InstrumentId, (Decimal, u64)>,
    orders: BTreeMap<OrderId, OrderRecord>,
    halts: Vec<HaltReason>,
    fee_breach: BTreeMap<InstrumentId, Vec<FeeDeclarationBreach>>,
}

impl RiskGate {
    /// Wraps `ledger`. Every later change to the ledger goes through the
    /// gate.
    #[must_use]
    pub fn new(ledger: Ledger) -> Self {
        Self {
            ledger,
            limits: BTreeMap::new(),
            marks: BTreeMap::new(),
            orders: BTreeMap::new(),
            halts: Vec::new(),
            fee_breach: BTreeMap::new(),
        }
    }

    /// Read only access to the ledger.
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Deposits into the ledger. See [`Ledger::deposit`].
    ///
    /// # Errors
    ///
    /// As [`Ledger::deposit`].
    pub fn deposit(&mut self, id: TransferId, asset: Asset, amount: Decimal) -> Result<bool> {
        self.ledger.deposit(id, asset, amount)
    }

    /// Withdraws from the available balance. See [`Ledger::withdraw`].
    ///
    /// # Errors
    ///
    /// As [`Ledger::withdraw`].
    pub fn withdraw(&mut self, id: TransferId, asset: Asset, amount: Decimal) -> Result<bool> {
        self.ledger.withdraw(id, asset, amount)
    }

    /// Books an execution of a future or perpetual that already
    /// happened at a venue, through
    /// [`Ledger::book_execution_allowing_overdraft`] with no reservation: it
    /// is mandatory and booked even if it overdraws the account, and a
    /// repeated identical execution changes nothing. Spot executions go
    /// through the gate's orders ([`RiskGate::on_fill`]).
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] for a spot instrument; otherwise every
    /// error of [`Ledger::book_execution_allowing_overdraft`].
    pub fn book_derivative_execution(&mut self, execution: &Execution) -> Result<OverdraftBooking> {
        if let Some(Instrument::Spot(_)) = self.ledger.instrument(&execution.instrument) {
            return Err(MarginError::invalid_input(
                "execution",
                "spot executions are booked through the gate's orders",
            ));
        }
        self.ledger
            .book_execution_allowing_overdraft(execution, None)
    }

    /// Books a perpetual's funding payment. See [`Ledger::book_funding`].
    ///
    /// # Errors
    ///
    /// As [`Ledger::book_funding`].
    pub fn book_funding(
        &mut self,
        payment: &FundingPayment,
    ) -> Result<PostingBooking<FundingCashflow>> {
        self.ledger.book_funding(payment)
    }

    /// Settles a future at expiry. See [`Ledger::settle_expiry`].
    ///
    /// # Errors
    ///
    /// As [`Ledger::settle_expiry`].
    pub fn settle_expiry(
        &mut self,
        settlement: &ExpirySettlement,
    ) -> Result<PostingBooking<ExpiryCashflow>> {
        self.ledger.settle_expiry(settlement)
    }

    /// Closes option contracts by an outcome the caller gives. See
    /// [`Ledger::settle_option`].
    ///
    /// # Errors
    ///
    /// As [`Ledger::settle_option`].
    pub fn settle_option(
        &mut self,
        settlement: &OptionSettlement,
    ) -> Result<OptionSettlementBooking> {
        self.ledger.settle_option(settlement)
    }

    /// Sets an external hold: a named ledger reservation of exactly
    /// `amount` of `asset`, replacing any previous amount under `id`
    /// (zero removes it). Used to make capital committed elsewhere, such as
    /// derivative margin
    /// ([`MarginReservationService`](crate::margin_reservations::MarginReservationService)),
    /// unavailable to spot orders. Atomic ([`Ledger::set_reservation`]): on
    /// error the previous hold is kept, and shrinking a hold always
    /// succeeds, even on an overdrawn balance.
    ///
    /// # Errors
    ///
    /// [`MarginError::InvalidInput`] if `id` is also an order id of this
    /// gate; [`MarginError::InsufficientBalance`] if an increase is not
    /// covered by the available balance plus the previous hold; the
    /// ledger's input errors.
    pub fn set_hold(&mut self, id: &ReservationId, asset: Asset, amount: Decimal) -> Result<()> {
        if OrderId::new(id.as_str()).is_ok_and(|order| self.orders.contains_key(&order)) {
            return Err(MarginError::invalid_input(
                "hold",
                "the id belongs to an order",
            ));
        }
        self.ledger.set_reservation(id, asset, amount)
    }

    /// Amount of an external hold set with [`RiskGate::set_hold`].
    #[must_use]
    pub fn hold(&self, id: &ReservationId) -> Decimal {
        self.ledger
            .reservation(id)
            .map_or(Decimal::ZERO, |(_, amount)| amount)
    }

    /// Sets the limits of a registered instrument.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownInstrument`], or [`MarginError::InvalidInput`]
    /// / [`MarginError::PrecisionExceeded`] for an invalid limit.
    pub fn set_limits(
        &mut self,
        instrument: &InstrumentId,
        limits: InstrumentLimits,
    ) -> Result<()> {
        let spot = self.spot(instrument)?;
        limits.validate(&spot)?;
        self.limits.insert(instrument.clone(), limits);
        Ok(())
    }

    /// The breaches of declared base fees recorded on `instrument`: the
    /// first breach of each declaration, in order. While a declaration is
    /// recorded, new orders under it (the same base fee rate, rounding and
    /// asset) are rejected with [`RiskReason::FeeDeclarationBreached`];
    /// cancels, confirmations and fill reports go on, and an order under a
    /// declaration not recorded (for example the default, worst case
    /// rounding) is admitted.
    #[must_use]
    pub fn fee_declaration_breaches(&self, instrument: &InstrumentId) -> &[FeeDeclarationBreach] {
        self.fee_breach.get(instrument).map_or(&[], Vec::as_slice)
    }

    /// The first breach of the fee declaration latches the instrument (only
    /// that declaration's new orders are refused).
    fn record_fee_breach(&mut self, id: &OrderId, order: &OrderRecord, breaches: &[FeeBreach]) {
        if let (Some(breach), Some(declaration)) = (
            breaches.first(),
            order.terms.as_ref().and_then(|t| t.base_fee.clone()),
        ) {
            let recorded = self.fee_breach.entry(order.instrument.clone()).or_default();
            // The first breach of each declaration is kept.
            if !recorded.iter().any(|b| b.declaration == declaration) {
                recorded.push(FeeDeclarationBreach {
                    order: id.clone(),
                    breach: breach.clone(),
                    declaration,
                });
            }
        }
    }

    /// Clears every recorded fee breach of `instrument`, once its
    /// declarations are checked or corrected. Returns `false` if none was
    /// recorded.
    /// Executions, reservations and the terms of live orders are kept.
    pub fn reset_fee_declaration(&mut self, instrument: &InstrumentId) -> bool {
        self.fee_breach.remove(instrument).is_some()
    }

    /// The breaches of the declared base fees by a new fill of `order`
    /// (before it is applied, so `order.filled` is the cumulative before
    /// it). A buy's fee in base is at most what its fill credits: in units
    /// of the base grid the fill `n` is an integer and `b = r * n < n`, so
    /// `ceil(a + b) - ceil(a) <= ceil(b) <= n`, a half mode gives at most
    /// `floor(b + 1) <= n` and toward zero at most `ceil(b) <= n`; both a
    /// per fill fee and a cumulative increment stay within `n`.
    fn fee_breaches(
        &self,
        order: &OrderRecord,
        execution: &Execution,
        filled_after: Decimal,
    ) -> Result<Vec<FeeBreach>> {
        let Some(fee_terms) = order.terms.as_ref().and_then(|t| t.base_fee.as_ref()) else {
            return Ok(Vec::new());
        };
        let scale =
            self.ledger
                .asset_scale(&fee_terms.base)
                .ok_or_else(|| MarginError::UnknownAsset {
                    asset: fee_terms.base.clone(),
                })?;
        let mut breaches = Vec::new();
        let charged = match &execution.fee {
            None => Decimal::ZERO,
            Some(fee) if fee.asset == fee_terms.base => fee.amount,
            Some(fee) => {
                breaches.push(FeeBreach::UnexpectedAsset {
                    asset: fee.asset.clone(),
                    fee: fee.amount,
                });
                Decimal::ZERO
            }
        };
        let n = execution.quantity;
        match fee_terms.rounding {
            FeeRounding::PerFill { mode, min_fill } => {
                if let Some(m) = min_fill
                    && n < m
                    && filled_after < order.quantity
                {
                    breaches.push(FeeBreach::BelowMinimumFill {
                        min_fill: m,
                        quantity: n,
                    });
                }
                let bound =
                    mul_div_round(&[fee_terms.rate, n], &[], scale, mode, "fill fee bound")?;
                if charged > bound {
                    breaches.push(FeeBreach::AboveBound {
                        bound,
                        fee: charged,
                    });
                }
            }
            FeeRounding::Cumulative { mode, .. } => {
                let after = mul_div_round(
                    &[fee_terms.rate, filled_after],
                    &[],
                    scale,
                    mode,
                    "fee after",
                )?;
                let before = mul_div_round(
                    &[fee_terms.rate, order.filled],
                    &[],
                    scale,
                    mode,
                    "fee before",
                )?;
                let expected = sub(after, before, "fee increment")?;
                if charged != expected {
                    breaches.push(FeeBreach::DeclaredFeeMismatch {
                        expected,
                        fee: charged,
                    });
                }
            }
        }
        Ok(breaches)
    }

    /// Limits in force for `instrument` (all disabled if never set).
    #[must_use]
    pub fn limits(&self, instrument: &InstrumentId) -> InstrumentLimits {
        self.limits.get(instrument).cloned().unwrap_or_default()
    }

    /// Records a mark price received at `timestamp_ms`.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownInstrument`]; [`MarginError::InvalidInput`] if
    /// the mark is not positive or the timestamp is older than the last one.
    pub fn on_market_data(
        &mut self,
        instrument: &InstrumentId,
        mark: Decimal,
        timestamp_ms: u64,
    ) -> Result<()> {
        self.spot(instrument)?;
        if mark <= Decimal::ZERO {
            return Err(MarginError::invalid_input("mark", "must be positive"));
        }
        if let Some((_, last)) = self.marks.get(instrument)
            && timestamp_ms < *last
        {
            return Err(MarginError::invalid_input(
                "timestamp_ms",
                "must not go backwards",
            ));
        }
        self.marks.insert(instrument.clone(), (mark, timestamp_ms));
        Ok(())
    }

    /// Current halt, if any: the most severe reason recorded since the last
    /// [`RiskGate::resume`] (see [`HaltReason::severity`]); among equally
    /// severe reasons, the first one.
    #[must_use]
    pub fn halted(&self) -> Option<HaltReason> {
        let mut most: Option<&HaltReason> = None;
        for reason in &self.halts {
            if most.is_none_or(|m| reason.severity() > m.severity()) {
                most = Some(reason);
            }
        }
        most.cloned()
    }

    /// Every halt reason recorded since the last [`RiskGate::resume`], in
    /// the order they happened.
    #[must_use]
    pub fn halt_reasons(&self) -> &[HaltReason] {
        &self.halts
    }

    fn record_halt(&mut self, reason: HaltReason) {
        if !self.halts.contains(&reason) {
            self.halts.push(reason);
        }
    }

    /// An order known to the gate.
    #[must_use]
    pub fn order(&self, id: &OrderId) -> Option<&OrderRecord> {
        self.orders.get(id)
    }

    /// Orders that may still fill, ordered by id.
    pub fn live_orders(&self) -> impl Iterator<Item = &OrderRecord> {
        self.orders.values().filter(|o| o.state.is_live())
    }

    // ----- submission ---------------------------------------------------

    /// Decides on an order at time `now_ms` and, if approved, reserves for
    /// it and records it as [`OrderState::PendingNew`].
    ///
    /// The decision is recorded under the order id, including rejections,
    /// so a repeated id is always rejected with
    /// [`RiskReason::DuplicateOrderId`] and never reserves twice.
    ///
    /// One exception: a side that is not `Buy` or `Sell` (for example
    /// `Action::Other`) is checked first, before the duplicate check, and
    /// rejected with [`RiskReason::InvalidOrder`] carrying
    /// [`MarginError::UnsupportedAction`]. Nothing is recorded: the id is not
    /// consumed, an existing record under it is untouched, and nothing is
    /// reserved. Every other rejection keeps the contract above.
    pub fn submit(&mut self, request: &OrderRequest, now_ms: u64) -> RiskDecision {
        if let Err(error) = Dir::from_action(request.side, "order side") {
            return RiskDecision::Rejected {
                reasons: vec![RiskReason::InvalidOrder { error }],
            };
        }
        if self.orders.contains_key(&request.id) {
            return RiskDecision::Rejected {
                reasons: vec![RiskReason::DuplicateOrderId],
            };
        }
        let decision = self.decide(request, now_ms);
        let mut terms = None;
        let decision = match decision.approved_quantity() {
            Some(quantity) => match self.reserve_for(request, quantity) {
                Ok(order_terms) => {
                    terms = Some(order_terms);
                    decision
                }
                // Unreachable by construction (sizing used the same
                // numbers); reported rather than hidden.
                Err(error) => RiskDecision::Rejected {
                    reasons: vec![RiskReason::InvalidOrder { error }],
                },
            },
            None => decision,
        };
        let (quantity, state) = match decision.approved_quantity() {
            Some(quantity) => (quantity, OrderState::PendingNew),
            None => (Decimal::ZERO, OrderState::Rejected),
        };
        self.orders.insert(
            request.id.clone(),
            OrderRecord {
                id: request.id.clone(),
                instrument: request.instrument.clone(),
                side: request.side,
                price: request.price,
                quantity,
                filled: Decimal::ZERO,
                state,
                decision: decision.clone(),
                terms,
            },
        );
        decision
    }

    fn decide(&self, request: &OrderRequest, now_ms: u64) -> RiskDecision {
        let mut reasons = Vec::new();
        if let Some(reason) = self.halted() {
            reasons.push(RiskReason::Halted { reason });
        }
        let spot = match self.spot(&request.instrument) {
            Ok(spot) => spot,
            // A registered derivative: named as such, not as unknown.
            Err(error @ MarginError::UnsupportedInstrument { .. }) => {
                reasons.push(RiskReason::InvalidOrder { error });
                return RiskDecision::Rejected { reasons };
            }
            Err(_) => {
                reasons.push(RiskReason::UnknownInstrument);
                return RiskDecision::Rejected { reasons };
            }
        };
        if let Err(error) = validate_order(request, &spot) {
            reasons.push(RiskReason::InvalidOrder { error });
            return RiskDecision::Rejected { reasons };
        }
        let limits = self.limits(&request.instrument);
        if let Some(declaration) = base_fee_terms(&limits, &spot)
            && let Some(breach) = self
                .fee_breach
                .get(&request.instrument)
                .and_then(|all| all.iter().find(|b| b.declaration == declaration))
        {
            reasons.push(RiskReason::FeeDeclarationBreached {
                breach: breach.clone(),
            });
        }
        self.check_market_data(request, &limits, now_ms, &mut reasons);
        if let Some(limit) = limits.max_open_orders {
            let live = self
                .live_orders()
                .filter(|o| o.instrument == request.instrument)
                .count();
            if live >= limit {
                reasons.push(RiskReason::MaxOpenOrders { limit });
            }
        }
        if !reasons.is_empty() {
            return RiskDecision::Rejected { reasons };
        }

        match self.size(request, &spot, &limits) {
            Ok((allowed, mut caps)) => {
                let minimum = limits
                    .min_quantity
                    .unwrap_or_else(|| Decimal::new(1, spot.quantity_scale()));
                if allowed < minimum {
                    caps.push(RiskReason::BelowMinimumQuantity { minimum, allowed });
                    return RiskDecision::Rejected { reasons: caps };
                }
                if allowed == request.quantity {
                    RiskDecision::Accepted { quantity: allowed }
                } else if request.allow_reduce {
                    RiskDecision::Reduced {
                        requested: request.quantity,
                        quantity: allowed,
                        reasons: caps,
                    }
                } else {
                    RiskDecision::Rejected { reasons: caps }
                }
            }
            Err(error) => RiskDecision::Rejected {
                reasons: vec![RiskReason::InvalidOrder { error }],
            },
        }
    }

    fn check_market_data(
        &self,
        request: &OrderRequest,
        limits: &InstrumentLimits,
        now_ms: u64,
        reasons: &mut Vec<RiskReason>,
    ) {
        if limits.max_data_age_ms.is_none() && limits.price_band.is_none() {
            return;
        }
        let Some((mark, at)) = self.marks.get(&request.instrument).copied() else {
            reasons.push(RiskReason::NoMarketData);
            return;
        };
        if at > now_ms {
            // A mark from the future cannot be used at `now_ms`.
            reasons.push(RiskReason::LookAheadMarketData {
                mark_ms: at,
                now_ms,
            });
            return;
        }
        if let Some(max_age_ms) = limits.max_data_age_ms {
            // `at <= now_ms` here: a later mark returned above.
            let age_ms = now_ms.abs_diff(at);
            if age_ms > max_age_ms {
                reasons.push(RiskReason::StaleMarketData { age_ms, max_age_ms });
            }
        }
        if let Some(band) = limits.price_band {
            // |price - mark| > band * mark, decided exactly: the distance
            // lies on the grid of its own scale, and for a value on that
            // grid `x > w` holds exactly when `x > floor(w)` at that scale.
            let outside = sub(request.price, mark, "price - mark")
                .and_then(|distance| {
                    mul_div_round(
                        &[band, mark],
                        &[],
                        distance.scale(),
                        Rounding::TowardZero,
                        "band * mark",
                    )
                    .map(|width| distance.abs() > width)
                })
                // An overflow cannot be decided: treat it as outside.
                .unwrap_or(true);
            if outside {
                reasons.push(RiskReason::PriceOutsideBand { mark, band });
            }
        }
    }

    /// Applies every quantity cap; returns the allowed quantity and the
    /// caps that bound it.
    fn size(
        &self,
        request: &OrderRequest,
        spot: &SpotInstrument,
        limits: &InstrumentLimits,
    ) -> Result<(Decimal, Vec<RiskReason>)> {
        let lot_scale = spot.quantity_scale();
        let floor = |value: Decimal| {
            mul_div_round(
                &[value.max(Decimal::ZERO)],
                &[],
                lot_scale,
                Rounding::TowardZero,
                "floor to lot",
            )
        };
        let mut allowed = request.quantity;
        let mut caps = Vec::new();

        if let Some(limit) = limits.max_order_quantity
            && allowed > limit
        {
            allowed = floor(limit)?;
            caps.push(RiskReason::MaxOrderQuantity { limit });
        }

        let position = self
            .ledger
            .position(&request.instrument)
            .map(|p| p.quantity)
            .unwrap_or_default();
        let (open_buys, open_sells) = self.open_quantities(&request.instrument)?;
        // Largest quantity this order can add before the worst case position
        // on its side reaches `bound` in absolute value.
        let headroom = |bound: Decimal| -> Result<Decimal> {
            // bound - worst case, in one exact step.
            let room = match Dir::from_action(request.side, "request side")? {
                Dir::Buy => sum(
                    &[bound, neg(position), neg(open_buys)],
                    "limit - worst case",
                )?,
                Dir::Sell => sum(&[bound, neg(open_sells), position], "limit - worst case")?,
            };
            floor(room)
        };
        if let Some(limit) = limits.max_position {
            let room = headroom(limit)?;
            if allowed > room {
                allowed = room;
                caps.push(RiskReason::MaxPosition {
                    limit,
                    headroom: room,
                });
            }
        }
        if let Some(limit) = limits.max_position_notional {
            let bound = mul_div_round(
                &[limit],
                &[request.price],
                lot_scale,
                Rounding::TowardZero,
                "notional / price",
            )?;
            let room = headroom(bound)?;
            if allowed > room {
                allowed = room;
                caps.push(RiskReason::MaxPositionNotional {
                    limit,
                    headroom: room,
                });
            }
        }

        let terms = self.terms_for(spot, limits, request.side, request.price)?;
        let required = terms.amount(allowed)?;
        let available = self
            .ledger
            .balance(&terms.asset)
            .map(|b| b.available)
            .unwrap_or_default();
        if required > available {
            let affordable = terms.affordable(available, allowed)?;
            caps.push(RiskReason::InsufficientBalance {
                asset: terms.asset.clone(),
                required,
                available,
            });
            allowed = allowed.min(affordable);
        }
        Ok((allowed, caps))
    }

    /// Reservation terms for an order on `spot` under `limits`.
    fn terms_for(
        &self,
        spot: &SpotInstrument,
        limits: &InstrumentLimits,
        side: Action,
        price: Decimal,
    ) -> Result<ReservationTerms> {
        let lot = Decimal::new(1, spot.quantity_scale());
        let scale_of = |asset: &Asset| {
            self.ledger
                .asset_scale(asset)
                .ok_or_else(|| MarginError::UnknownAsset {
                    asset: asset.clone(),
                })
        };
        let base_fee = base_fee_terms(limits, spot);
        match Dir::from_action(side, "side")? {
            Dir::Sell => {
                let scale = scale_of(spot.base())?;
                let per_lot = match limits.fee_asset {
                    // The fee is paid from the quote the fill credits.
                    FeeAsset::Quote => lot,
                    // A fill of k lots is charged at most round_up(r * k *
                    // lot) <= k * round_up(r * lot) (rounding up is
                    // subadditive). The lot is on the base grid
                    // (quantity_scale <= base scale), so this is also never
                    // above the exact bound lot * (1 + r) + ulp:
                    // round_up(r * lot) < r * lot + ulp.
                    // As the lot is on the grid, lot + round_up(r * lot) is
                    // round_up(lot + r * lot): one exact sum, rounded once.
                    FeeAsset::Base => sum_terms_round(
                        &[
                            &[lot],
                            &[base_fee.as_ref().map_or(Decimal::ZERO, |f| f.rate), lot],
                        ],
                        scale,
                        Rounding::AwayFromZero,
                        "lot + round up lot base fee",
                    )?,
                };
                Ok(ReservationTerms {
                    asset: spot.base().clone(),
                    scale,
                    lot,
                    per_lot,
                    base_fee,
                })
            }
            Dir::Buy => {
                let scale = scale_of(spot.quote())?;
                let ulp = Decimal::new(1, scale);
                // A fee in base is paid from the base the fill credits, in
                // the same booking: the quote reservation covers the
                // notional only.
                let rate = match limits.fee_asset {
                    FeeAsset::Quote => limits.max_fee_rate,
                    FeeAsset::Base => Decimal::ZERO,
                };
                let up = |value: Decimal, op| {
                    mul_div_round(&[value], &[], scale, Rounding::AwayFromZero, op)
                };
                // Cost of one lot at the limit price, exact.
                let lot_cost = mul(price, lot, "price * lot")?;
                // Bound 1, on the grid: round_up(lot cost) plus its fee
                // rounded up. Rounding up is subadditive, so k lots cost at
                // most k times this.
                let cash = up(lot_cost, "round up lot cost")?;
                let fee_part = if rate.is_zero() {
                    Decimal::ZERO
                } else {
                    mul_div_round(
                        &[rate, cash],
                        &[],
                        scale,
                        Rounding::AwayFromZero,
                        "round up lot fee",
                    )?
                };
                let on_grid = add(cash, fee_part, "lot cash + fee")?;
                // Bound 2, exact: (1 + rate) * (lot cost + h) + fee ulp,
                // where h is the largest error of the ledger's notional
                // rounding. A fill of k lots debits at most k times this.
                let half_ulp = Decimal::new(
                    5,
                    scale
                        .checked_add(1)
                        .ok_or(MarginError::arithmetic("half ulp scale"))?,
                );
                let h = match self.ledger.config().rounding {
                    Rounding::HalfEven | Rounding::HalfAwayFromZero => half_ulp,
                    Rounding::TowardZero => Decimal::ZERO,
                    Rounding::AwayFromZero => ulp,
                };
                // (1 + rate) * (lot cost + h) + fee ulp, in one exact step.
                let fee_ulp = if rate.is_zero() { Decimal::ZERO } else { ulp };
                let exact = add(Decimal::ONE, rate, "1 + rate").and_then(|one_plus| {
                    sum_products(
                        &[&[one_plus, lot_cost], &[one_plus, h], &[fee_ulp]],
                        "exact lot bound",
                    )
                });
                let per_lot = match exact {
                    Ok(bound) if bound < on_grid => bound,
                    _ => on_grid,
                };
                Ok(ReservationTerms {
                    asset: spot.quote().clone(),
                    scale,
                    lot,
                    per_lot,
                    base_fee,
                })
            }
        }
    }

    fn reserve_for(
        &mut self,
        request: &OrderRequest,
        quantity: Decimal,
    ) -> Result<ReservationTerms> {
        let spot = self.spot(&request.instrument)?;
        let limits = self.limits(&request.instrument);
        let terms = self.terms_for(&spot, &limits, request.side, request.price)?;
        let amount = terms.amount(quantity)?;
        self.ledger.reserve(
            ReservationId::new(request.id.as_str())?,
            terms.asset.clone(),
            amount,
        )?;
        Ok(terms)
    }

    fn open_quantities(&self, instrument: &InstrumentId) -> Result<(Decimal, Decimal)> {
        let mut buys = Vec::new();
        let mut sells = Vec::new();
        for order in self.live_orders().filter(|o| &o.instrument == instrument) {
            match Dir::from_action(order.side, "order side")? {
                Dir::Buy => buys.push(order.remaining()?),
                Dir::Sell => sells.push(order.remaining()?),
            }
        }
        Ok((sum(&buys, "open buys")?, sum(&sells, "open sells")?))
    }

    fn spot(&self, instrument: &InstrumentId) -> Result<SpotInstrument> {
        match self.ledger.instrument(instrument) {
            Some(Instrument::Spot(spot)) => Ok(spot.clone()),
            Some(
                Instrument::LinearFuture(_)
                | Instrument::LinearPerpetual(_)
                | Instrument::InverseFuture(_)
                | Instrument::InversePerpetual(_)
                | Instrument::Option(_),
            ) => Err(MarginError::UnsupportedInstrument {
                rule: "risk-gate".to_string(),
                instrument: instrument.clone(),
                reason: "the risk gate handles spot orders; derivatives reserve margin with MarginReservationService",
            }),
            None => Err(MarginError::UnknownInstrument {
                instrument: instrument.clone(),
            }),
        }
    }

    fn live_order(&self, id: &OrderId, operation: &'static str) -> Result<OrderRecord> {
        let order = self
            .orders
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        if !order.state.is_live() {
            return Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation,
            });
        }
        Ok(order)
    }

    fn set_state(&mut self, id: &OrderId, state: OrderState) {
        if let Some(order) = self.orders.get_mut(id) {
            order.state = state;
        }
    }

    // ----- lifecycle ----------------------------------------------------

    /// The venue acknowledged the order. Idempotent while the order is live.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`] for
    /// an order the gate rejected.
    pub fn on_venue_accepted(&mut self, id: &OrderId) -> Result<()> {
        let order = self
            .orders
            .get(id)
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        match order.state {
            OrderState::PendingNew => {
                self.set_state(id, OrderState::Open);
                Ok(())
            }
            // A late or repeated ack changes nothing.
            OrderState::Open
            | OrderState::CancelPending
            | OrderState::Filled
            | OrderState::Canceled => Ok(()),
            OrderState::Rejected => Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation: "accept",
            }),
        }
    }

    /// The venue rejected the order: its reservation is released and
    /// returned. A repeated reject returns zero.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`] if
    /// the order already filled in part or ended otherwise.
    pub fn on_venue_rejected(&mut self, id: &OrderId) -> Result<Decimal> {
        let order = self
            .orders
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        if order.state == OrderState::Rejected {
            return Ok(Decimal::ZERO);
        }
        if !order.state.is_live() || !order.filled.is_zero() {
            return Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation: "reject",
            });
        }
        let (_, released) = self.ledger.release(&order.reservation_id()?)?;
        self.set_state(id, OrderState::Rejected);
        Ok(released)
    }

    /// Requests a cancel. The reservation stays until
    /// [`RiskGate::on_cancel_confirmed`]. Returns `false` if a cancel was
    /// already pending.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`] if
    /// the order is no longer live.
    pub fn request_cancel(&mut self, id: &OrderId) -> Result<bool> {
        let order = self.live_order(id, "cancel")?;
        if order.state == OrderState::CancelPending {
            return Ok(false);
        }
        self.set_state(id, OrderState::CancelPending);
        Ok(true)
    }

    /// The venue confirmed the cancel: what is left of the reservation is
    /// released and returned. Confirming a cancel for an order that already
    /// ended (for example filled first) returns zero and changes nothing.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`] for
    /// an order the gate rejected.
    pub fn on_cancel_confirmed(&mut self, id: &OrderId) -> Result<Decimal> {
        let order = self
            .orders
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        match order.state {
            OrderState::PendingNew | OrderState::Open | OrderState::CancelPending => {
                let (_, released) = self.ledger.release(&order.reservation_id()?)?;
                self.set_state(id, OrderState::Canceled);
                Ok(released)
            }
            OrderState::Filled | OrderState::Canceled => Ok(Decimal::ZERO),
            OrderState::Rejected => Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation: "confirm cancel",
            }),
        }
    }

    /// Books a real fill of order `id`. A real execution is never lost.
    ///
    /// The report carries the order's cumulative filled quantity after the
    /// fill (as FIX `CumQty`). It must equal the quantity filled so far plus
    /// the fill: that orders one order's fills causally. Only a report that
    /// cannot be causally true is rejected; anything that did happen is
    /// booked:
    ///
    /// - Live order: the fill is booked and the reservation shrunk to what
    ///   the remaining quantity needs, atomically. By the
    ///   [`ReservationTerms`] bound the reservation frees at least the
    ///   debit; anything above it (a fee over `max_fee_rate`) comes from the
    ///   free balance.
    /// - Canceled or filled order (a late report): booked against the free
    ///   balance.
    /// - An overfill, a fill after the order was filled, or a price worse
    ///   than the limit is booked too, with only the reserved part released,
    ///   reported in [`FillOutcome::anomalies`], and the gate halts with
    ///   [`HaltReason::ExecutionAnomaly`].
    /// - If the free balance does not cover the debit, the fill is still
    ///   booked: the ledger records the shortfall (an overdraft, see
    ///   [`Ledger::overdrafts`]), it is reported in
    ///   [`FillOutcome::shortfalls`], and the gate halts with
    ///   [`HaltReason::AccountingBreach`].
    ///
    /// A repeated execution (same [`ExecutionId`](crate::ExecutionId) and
    /// content) returns a [`Booking::Duplicate`] outcome and changes
    /// nothing.
    ///
    /// # Errors
    ///
    /// Nothing changes on error. These are reports that cannot be true:
    ///
    /// - [`MarginError::UnsupportedAction`] for an execution side that is not
    ///   `Buy` or `Sell`, refused first, before the duplicate check;
    /// - [`MarginError::UnknownOrder`]; [`MarginError::InvalidOrderState`]
    ///   for an order the gate rejected (it was never sent);
    /// - [`MarginError::ExecutionMismatch`] if the instrument or side differs
    ///   from the order, or the cumulative quantity is not the filled
    ///   quantity plus the fill;
    /// - [`MarginError::ConflictingEntry`] for a reused execution id with
    ///   different content, and the ledger's input errors (precision,
    ///   positivity).
    pub fn on_fill(&mut self, id: &OrderId, report: &FillReport) -> Result<FillOutcome> {
        let execution = &report.execution;
        // An action the crate cannot trade is refused before anything else:
        // before the duplicate check, the order lookup and any halt.
        Dir::from_action(execution.side, "execution side")?;
        if self.ledger.execution_cashflow(&execution.id).is_some() {
            // Duplicate (or conflicting) message: the ledger decides, and
            // neither the order nor its reservation changes.
            return Ok(FillOutcome {
                booking: self.ledger.book_execution(execution)?,
                anomalies: Vec::new(),
                shortfalls: BTreeMap::new(),
                withdraw: Vec::new(),
                fee_breaches: Vec::new(),
            });
        }
        let order = self
            .orders
            .get(id)
            .cloned()
            .ok_or_else(|| MarginError::UnknownOrder { id: id.to_string() })?;
        if order.state == OrderState::Rejected {
            return Err(MarginError::InvalidOrderState {
                id: id.to_string(),
                state: order.state.name(),
                operation: "fill",
            });
        }
        let mismatch = |reason: &'static str| MarginError::ExecutionMismatch {
            id: id.to_string(),
            reason,
        };
        if execution.instrument != order.instrument {
            return Err(mismatch("instrument differs from the order"));
        }
        if execution.side != order.side {
            return Err(mismatch("side differs from the order"));
        }
        let filled_after = add(order.filled, execution.quantity, "filled + fill")?;
        if report.cumulative_quantity != filled_after {
            return Err(mismatch(
                "cumulative quantity is not the filled quantity plus the fill",
            ));
        }

        let remaining = order.remaining()?;
        let mut anomalies = Vec::new();
        if order.state == OrderState::Filled {
            anomalies.push(ExecutionAnomaly::FillAfterFilled);
        } else if execution.quantity > remaining {
            anomalies.push(ExecutionAnomaly::Overfill {
                remaining,
                quantity: execution.quantity,
            });
        }
        let worse = match Dir::from_action(order.side, "order side")? {
            Dir::Buy => execution.price > order.price,
            Dir::Sell => execution.price < order.price,
        };
        if worse {
            anomalies.push(ExecutionAnomaly::WorsePrice {
                limit: order.price,
                price: execution.price,
            });
        }

        let fee_breaches = self.fee_breaches(&order, execution, filled_after)?;

        // Only a live order still holds a reservation; shrink it to what the
        // rest needs (nothing after an overfill).
        let reservation_id = order.reservation_id()?;
        let reservation = match (&order.terms, order.state.is_live()) {
            (Some(terms), true) => Some(terms.amount_of_order(order.quantity, filled_after)?),
            _ => None,
        };
        let booked = self.ledger.book_execution_allowing_overdraft(
            execution,
            reservation.map(|amount| (&reservation_id, amount)),
        )?;

        self.record_fee_breach(id, &order, &fee_breaches);
        // Both are recorded when both happen; halted() reports the more
        // severe one (the accounting breach).
        if let Some(anomaly) = anomalies.first() {
            self.record_halt(HaltReason::ExecutionAnomaly {
                order: id.clone(),
                anomaly: anomaly.clone(),
            });
        }
        if !booked.shortfalls.is_empty() {
            self.record_halt(HaltReason::AccountingBreach);
        }
        if let Some(record) = self.orders.get_mut(id) {
            record.filled = filled_after;
            if filled_after >= record.quantity {
                record.state = OrderState::Filled;
            }
        }
        // An internal halt withdraws quotes like trip(), after the order's
        // own update, so a filled order is not cancelled and a partly filled
        // one is.
        let withdraw = if anomalies.is_empty() && booked.shortfalls.is_empty() {
            Vec::new()
        } else {
            self.withdraw_live()
        };
        Ok(FillOutcome {
            booking: booked.booking,
            anomalies,
            shortfalls: booked.shortfalls,
            withdraw,
            fee_breaches,
        })
    }

    // ----- circuit breaker ----------------------------------------------

    /// Halts trading and withdraws quotes.
    ///
    /// Returns the live orders that had no cancel pending; they are now
    /// [`OrderState::CancelPending`] and the caller must send their cancels.
    /// Calling it again while halted returns no orders; the reason is added
    /// to [`RiskGate::halt_reasons`] and [`RiskGate::halted`] reports the
    /// most severe one.
    pub fn trip(&mut self, reason: HaltReason) -> Vec<OrderId> {
        self.record_halt(reason);
        self.withdraw_live()
    }

    /// Moves every live order without a cancel pending to cancel pending
    /// and returns them, in order id order. Reservations stay until each
    /// cancel is confirmed.
    fn withdraw_live(&mut self) -> Vec<OrderId> {
        let to_cancel: Vec<OrderId> = self
            .orders
            .values()
            .filter(|o| matches!(o.state, OrderState::PendingNew | OrderState::Open))
            .map(|o| o.id.clone())
            .collect();
        for id in &to_cancel {
            self.set_state(id, OrderState::CancelPending);
        }
        to_cancel
    }

    /// Lifts the halt and clears every recorded reason.
    pub fn resume(&mut self) {
        self.halts.clear();
    }
}

/// The base fee terms an order on `spot` gets under `limits`: `None` for
/// fees in quote.
fn base_fee_terms(limits: &InstrumentLimits, spot: &SpotInstrument) -> Option<BaseFeeTerms> {
    match limits.fee_asset {
        FeeAsset::Quote => None,
        FeeAsset::Base => Some(BaseFeeTerms {
            rate: match limits.fee_rounding {
                FeeRounding::PerFill { .. } => limits.max_fee_rate,
                FeeRounding::Cumulative { rate, .. } => rate,
            },
            rounding: limits.fee_rounding,
            base: spot.base().clone(),
        }),
    }
}

fn validate_order(request: &OrderRequest, spot: &SpotInstrument) -> Result<()> {
    if request.price <= Decimal::ZERO {
        return Err(MarginError::invalid_input("price", "must be positive"));
    }
    if request.quantity <= Decimal::ZERO {
        return Err(MarginError::invalid_input("quantity", "must be positive"));
    }
    check_scale(request.price, spot.price_scale(), "price")?;
    check_scale(request.quantity, spot.quantity_scale(), "quantity")?;
    // The reservation id is the order id; validate it here so reserving
    // cannot fail later.
    ReservationId::new(request.id.as_str())?;
    Ok(())
}
