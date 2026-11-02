//! Margin rules. Each rule is versioned and cites its primary
//! specification.
//!
//! | Rule | Products | Primary specification |
//! |---|---|---|
//! | [`TieredLinearIm::linear_v1`] (`linear-im/1`) | Linear USDC futures and perpetuals | [`initial_linear`] and `docs/initial-margin.md` |
//!
//! External venue rules (for example Binance USDⓈ-M leverage brackets) are
//! future rules: each will cite its venue specification pinned by date and
//! get its own identifier, never reuse this one.

pub mod initial_linear;

pub use initial_linear::{MarginTier, TieredLinearIm};
