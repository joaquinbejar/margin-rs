//! Margin rules. Each rule is versioned and cites its primary
//! specification.
//!
//! | Rule | Products | Primary specification |
//! |---|---|---|
//! | [`TieredLinearIm::linear_v1`] (`linear-im/1`) | Linear futures and perpetuals, in the settlement asset | [`initial_linear`] and `docs/initial-margin.md` |
//! | [`TieredInverseIm::inverse_v1`] (`inverse-im/1`) | Inverse futures and perpetuals, in the base | [`initial_inverse`] and `docs/initial-margin.md` |
//!
//! External venue rules (for example Binance USDⓈ-M leverage brackets) are
//! future rules: each will cite its venue specification pinned by date and
//! get its own identifier, never reuse this one.

pub mod initial_inverse;
pub mod initial_linear;
pub(crate) mod tiers;

pub use initial_inverse::TieredInverseIm;
pub use initial_linear::{MarginTier, TieredLinearIm};
