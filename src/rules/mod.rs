//! Margin rules. Each rule is versioned and cites its primary
//! specification.
//!
//! | Rule | Products | Primary specification |
//! |---|---|---|
//! | [`TieredLinearIm::linear_v1`] (`linear-im/1`) | Linear futures and perpetuals, in the settlement asset | [`initial_linear`] and `docs/initial-margin.md` |
//! | [`TieredInverseIm::inverse_v1`] (`inverse-im/1`) | Inverse futures and perpetuals, in the base | [`initial_inverse`] and `docs/initial-margin.md` |
//! | [`ScenarioPm::scenario_pm_v1`] (`scenario-pm/1`) | Linear futures, perpetuals and options of one account, netted over caller supplied scenarios | [`scenario_pm`] and `docs/scenario-margin.md` |
//! | [`ShortOptionIm::short_option_v1`] (`short-option-im/1`) | Options, in the settlement asset; only shorts require margin | [`short_option`] and `docs/options.md` |
//!
//! External venue rules (for example Binance USDⓈ-M leverage brackets) are
//! future rules: each will cite its venue specification pinned by date and
//! get its own identifier, never reuse this one.

pub mod initial_inverse;
pub mod initial_linear;
pub mod scenario_pm;
pub mod short_option;
pub(crate) mod tiers;

pub use initial_inverse::TieredInverseIm;
pub use initial_linear::{MarginTier, TieredLinearIm};
pub use scenario_pm::{
    PmBreakdown, PmLeg, PmLegKind, PmOrder, ScenarioPm, ScenarioPmRates, ScenarioPmRequirement,
    ScenarioSets,
};
pub use short_option::{OptionExposure, OptionExposureView, ShortOptionIm, ShortOptionParameters};
