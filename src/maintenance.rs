//! Maintenance threshold: the account state a margin requirement implies.
//!
//! [`assess`] compares one account's collateral with the maintenance
//! margin of a [`MarginRequirement`] and returns a [`MaintenanceDecision`]:
//! the new [`AccountState`], why, and how far the account is from the
//! threshold. It is a pure function of its inputs and the previous state.
//!
//! The decision never triggers anything. When a venue keeps its own
//! account state, it owns the trigger, that state and the cancellation of
//! resting orders; margin-rs computes the state it reports and, when asked,
//! the [liquidation plan](crate::liquidation).
//!
//! # Rule
//!
//! The rule, in order:
//!
//! | Condition | State |
//! |---|---|
//! | `cash < 0` or `equity < 0` | [`AccountState::Default`] |
//! | previous state `Default`, unless `cash >= 0` and `collateral >= mm` | [`AccountState::Default`] |
//! | `collateral < mm`, or collateral or equity unknown | [`AccountState::ReduceOnly`] |
//! | otherwise | [`AccountState::Healthy`] |
//!
//! Equality is healthy: an account with `collateral == mm` is
//! [`AccountState::Healthy`]; the threshold is crossed only when collateral
//! is strictly below maintenance. An unknown figure (a debt without a
//! trusted mark) never counts as healthy: it is treated as below the
//! threshold, never as above it.

use rust_decimal::Decimal;

use crate::decimal::{neg, sub};
use crate::error::{MarginError, Result};
use crate::ids::Asset;
use crate::initial_margin::{MarginRequirement, RuleId};

/// State of an account with respect to its margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AccountState {
    /// Collateral covers maintenance; every order type is allowed.
    #[default]
    Healthy,
    /// Collateral is below maintenance: only risk reducing orders.
    ReduceOnly,
    /// Cash or equity is negative, or the account has not yet left a
    /// default: only risk reducing orders, and no new opening of any kind.
    Default,
}

impl AccountState {
    /// True when the account may send risk increasing orders.
    #[must_use]
    pub fn may_increase_risk(self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// What [`assess`] reads, in the requirement's currency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollateralView {
    /// Currency of every amount.
    pub currency: Asset,
    /// Cash balance in that currency; negative after a mandatory debit
    /// that the balance did not cover.
    pub cash: Decimal,
    /// Collateral (for example cash plus the unrealized PnL of futures and
    /// perpetuals, minus the marks of short options); `None` when unknown.
    pub collateral: Option<Decimal>,
    /// Account equity; `None` when unknown.
    pub equity: Option<Decimal>,
}

/// Why an account is not [`AccountState::Healthy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StateReason {
    /// `cash < 0`.
    NegativeCash,
    /// `equity < 0`.
    NegativeEquity,
    /// The previous state was `Default` and the exit condition
    /// (`cash >= 0` and `collateral >= mm`) does not hold.
    DefaultNotCleared,
    /// `collateral < mm`.
    BelowMaintenance,
    /// Collateral is unknown.
    CollateralUnknown,
    /// Equity is unknown.
    EquityUnknown,
}

/// Result of [`assess`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceDecision {
    /// Rule that produced the requirement.
    pub rule: RuleId,
    /// Currency of every amount.
    pub currency: Asset,
    /// State before the assessment.
    pub previous: AccountState,
    /// State after the assessment.
    pub state: AccountState,
    /// Every condition that holds against the account, in a fixed order;
    /// empty when healthy.
    pub reasons: Vec<StateReason>,
    /// Collateral used.
    pub collateral: Option<Decimal>,
    /// Account initial margin.
    pub initial: Decimal,
    /// Account maintenance margin.
    pub maintenance: Decimal,
    /// `collateral - maintenance`, exact; `None` when collateral is
    /// unknown. Negative below the threshold, zero at it.
    pub excess: Option<Decimal>,
    /// `max(0, maintenance - collateral)`; `None` when collateral is
    /// unknown.
    pub maintenance_shortfall: Option<Decimal>,
    /// `max(0, -cash)`: the recorded debt.
    pub debt: Decimal,
}

impl MaintenanceDecision {
    /// True when the state changed.
    #[must_use]
    pub fn is_transition(&self) -> bool {
        self.previous != self.state
    }
}

/// Assesses the account state. See the [module docs](self).
///
/// # Errors
///
/// - [`MarginError::CurrencyMismatch`] if the view and the requirement are
///   in different currencies.
/// - [`MarginError::Arithmetic`] if a difference cannot be represented.
pub fn assess(
    previous: AccountState,
    view: &CollateralView,
    requirement: &MarginRequirement,
) -> Result<MaintenanceDecision> {
    if view.currency != requirement.currency {
        return Err(MarginError::CurrencyMismatch {
            field: "collateral view",
            expected: requirement.currency.clone(),
            found: view.currency.clone(),
        });
    }
    let maintenance = requirement.maintenance;
    let excess = view
        .collateral
        .map(|c| sub(c, maintenance, "collateral - maintenance"))
        .transpose()?;
    let covers_maintenance = excess.is_some_and(|e| e >= Decimal::ZERO);

    let mut reasons = Vec::new();
    if view.cash < Decimal::ZERO {
        reasons.push(StateReason::NegativeCash);
    }
    if view.equity.is_some_and(|e| e < Decimal::ZERO) {
        reasons.push(StateReason::NegativeEquity);
    }
    let defaulting = !reasons.is_empty();
    let stays_in_default =
        previous == AccountState::Default && !(view.cash >= Decimal::ZERO && covers_maintenance);
    if stays_in_default {
        reasons.push(StateReason::DefaultNotCleared);
    }
    match excess {
        Some(e) if e < Decimal::ZERO => reasons.push(StateReason::BelowMaintenance),
        Some(_) => {}
        None => reasons.push(StateReason::CollateralUnknown),
    }
    if view.equity.is_none() {
        reasons.push(StateReason::EquityUnknown);
    }

    let state = if defaulting || stays_in_default {
        AccountState::Default
    } else if reasons.is_empty() {
        AccountState::Healthy
    } else {
        AccountState::ReduceOnly
    };
    Ok(MaintenanceDecision {
        rule: requirement.rule.clone(),
        currency: requirement.currency.clone(),
        previous,
        state,
        reasons,
        collateral: view.collateral,
        initial: requirement.initial,
        maintenance,
        excess,
        maintenance_shortfall: excess.map(|e| neg(e).max(Decimal::ZERO)),
        debt: neg(view.cash).max(Decimal::ZERO),
    })
}
