//! The order direction inside the crate.
//!
//! The public API uses [`Action`] from `financial_types`, shared with the
//! other crates of the stack. `Action` is `#[non_exhaustive]` and has an
//! `Other` variant that this crate cannot trade, so every public entry turns
//! it into a private [`Dir`] first: [`Dir::from_action`] is the only place
//! with a wildcard arm, and every match inside the crate is on `Dir`,
//! exhaustive.

use financial_types::Action;

use crate::error::{MarginError, Result};

/// A direction the crate can trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dir {
    /// Receive the base asset or the contracts.
    Buy,
    /// Deliver the base asset or the contracts.
    Sell,
}

impl Dir {
    /// The direction of `action`, read from `field`.
    ///
    /// # Errors
    ///
    /// [`MarginError::UnsupportedAction`] for anything but `Buy` and `Sell`.
    pub(crate) fn from_action(action: Action, field: &'static str) -> Result<Self> {
        match action {
            Action::Buy => Ok(Self::Buy),
            Action::Sell => Ok(Self::Sell),
            // `Action` is non-exhaustive: `Other` and any later variant.
            _ => Err(MarginError::UnsupportedAction { field, action }),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn test_dir_from_action_buy_and_sell_map_to_their_direction() {
        assert_eq!(Dir::from_action(Action::Buy, "side"), Ok(Dir::Buy));
        assert_eq!(Dir::from_action(Action::Sell, "side"), Ok(Dir::Sell));
    }

    #[test]
    fn test_dir_from_action_other_is_unsupported() {
        assert_eq!(
            Dir::from_action(Action::Other, "order side"),
            Err(MarginError::UnsupportedAction {
                field: "order side",
                action: Action::Other
            })
        );
    }
}
