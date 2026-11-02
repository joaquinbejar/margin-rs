//! Margin rules. Each rule is versioned and cites its primary
//! specification.
//!
//! | Rule | Products | Primary specification |
//! |---|---|---|
//! | [`TieredLinearIm::xynthex_v1`] (`xynthex-linear-im/1`) | Linear USDC futures and perpetuals | Xynthex `docs/design/economics.md` section 11.2 at commit `2b6a97330ef056673c97f3b1a48da4e62cdb67f3` |
//!
//! External venue rules (for example Binance USDⓈ-M leverage brackets) are
//! future rules: each will cite its venue specification pinned by date and
//! get its own identifier, never reuse this one.

pub mod initial_linear;

pub use initial_linear::{MarginTier, TieredLinearIm};
