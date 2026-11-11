//! Closeout plans read from the ledger and booked back into it (M3-INT).
//!
//! Every expected value is written out by hand from the cash conventions
//! of `docs/liquidation.md`; `rhe` is half-even rounding to the settlement
//! asset scale, `trunc` truncation toward zero.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use margin_rs::initial_margin::{ExposurePosition, ExposureView, ProductKind, RuleId};
use margin_rs::instrument::{LinearFuture, LinearPerpetual, LinearTerms};
use margin_rs::ledger::{Action, Execution, ExpirySettlement, Ledger, LedgerSnapshot};
use margin_rs::liquidation::{
    CloseoutOutcome, CloseoutTarget, LiquidationAccount, LiquidationPlan, LiquidationPolicy,
    LiquidityLevel, plan_liquidation,
};
use margin_rs::rules::{MarginTier, TieredLinearIm};
use margin_rs::{
    AccountId, Asset, ContractSize, Decimal, ExecutionId, InstrumentId, MarginError, ReservationId,
    SpotInstrument, TransferId, VenueId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).expect("asset")
}

fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).expect("id")
}

fn eid(id: &str) -> ExecutionId {
    ExecutionId::new(id).expect("id")
}

fn account() -> AccountId {
    AccountId::new("acct").expect("id")
}

fn venue() -> VenueId {
    VenueId::new("venue").expect("id")
}

/// Single tier: im 10 %, mm 5 % of `|Qc| * M * mark`, in USDC at `scale`.
fn rule(scale: u32) -> TieredLinearIm {
    TieredLinearIm::new(
        RuleId::new("linear-im", 1),
        asset("USDC"),
        scale,
        vec![MarginTier {
            floor: Decimal::ZERO,
            initial_rate: dec!(0.10),
            maintenance_rate: dec!(0.05),
        }],
    )
    .expect("rule")
}

fn perpetual(id: &str, settlement: &str, multiplier: Decimal, price_scale: u32) -> LinearPerpetual {
    LinearPerpetual::new(
        LinearTerms::new(iid(id), asset(settlement), multiplier, price_scale, 0).expect("terms"),
    )
}

/// USDC at `scale` holding `cash`, with:
/// - BTC-PERP: perpetual in USDC, M = 1, price scale 2;
/// - Q-PERP: perpetual in USDC, M = 1, price scale 3;
/// - BTC-DEC: future in USDC expiring at 1000, M = 1, price scale 2;
/// - EUR-PERP: perpetual in EUR, M = 1;
/// - BTC-USDC: spot, BTC at 8 decimals.
fn ledger(scale: u32, cash: Decimal) -> Ledger {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), scale).expect("asset");
    ledger.register_asset(asset("EUR"), 2).expect("asset");
    ledger.register_asset(asset("BTC"), 8).expect("asset");
    for instrument in [
        perpetual("BTC-PERP", "USDC", dec!(1), 2),
        perpetual("Q-PERP", "USDC", dec!(1), 3),
        perpetual("EUR-PERP", "EUR", dec!(1), 2),
    ] {
        ledger.register_instrument(instrument).expect("instrument");
    }
    ledger
        .register_instrument(LinearFuture::new(
            LinearTerms::new(iid("BTC-DEC"), asset("USDC"), dec!(1), 2, 0).expect("terms"),
            1000,
        ))
        .expect("future");
    ledger
        .register_instrument(
            SpotInstrument::new(iid("BTC-USDC"), asset("BTC"), asset("USDC"), 2, 8).expect("spot"),
        )
        .expect("spot");
    if !cash.is_zero() {
        ledger
            .deposit(TransferId::new("d1").expect("id"), asset("USDC"), cash)
            .expect("deposit");
    }
    ledger
}

fn fill(id: &str, instrument: &str, side: Action, price: Decimal, contracts: Decimal) -> Execution {
    Execution::new(eid(id), iid(instrument), side, price, contracts).with_executed_at(500)
}

fn book(ledger: &mut Ledger, execution: &Execution) {
    let booking = ledger
        .book_execution_allowing_overdraft(execution, None)
        .expect("booked");
    assert!(booking.booking.is_new());
}

fn marks(entries: &[(&str, Decimal)]) -> BTreeMap<InstrumentId, Decimal> {
    entries.iter().map(|(id, mark)| (iid(id), *mark)).collect()
}

fn from_ledger(
    scale: u32,
    snapshot: &LedgerSnapshot,
    marks: &BTreeMap<InstrumentId, Decimal>,
    as_of_ms: u64,
) -> margin_rs::Result<LiquidationAccount> {
    LiquidationAccount::from_ledger(
        &rule(scale),
        snapshot,
        account(),
        venue(),
        &asset("USDC"),
        marks,
        as_of_ms,
    )
}

fn levels(
    instrument: &str,
    entries: &[(Decimal, Decimal)],
) -> BTreeMap<InstrumentId, Vec<LiquidityLevel>> {
    BTreeMap::from([(
        iid(instrument),
        entries
            .iter()
            .map(|(price, contracts)| LiquidityLevel {
                price: *price,
                contracts: *contracts,
            })
            .collect(),
    )])
}

/// Books every forced fill of `plan` as `liq-<i>`, executed at 600.
fn book_plan(ledger: &mut Ledger, plan: &LiquidationPlan) {
    for (i, reduction) in plan.reductions.iter().enumerate() {
        let execution = reduction.execution(eid(&format!("liq-{i}")), plan.currency.clone(), 600);
        book(ledger, &execution);
    }
}

/// The ledger after the plan holds exactly the plan's `after`: cash,
/// contracts and basis; it reconciles, and reading it back gives the same
/// account summary.
fn assert_parity(
    ledger: &Ledger,
    plan: &LiquidationPlan,
    scale: u32,
    marks: &BTreeMap<InstrumentId, Decimal>,
    instrument: &str,
) {
    assert_eq!(
        ledger.balance(&asset("USDC")).expect("balance").total,
        plan.after.cash
    );
    let position = ledger.position(&iid(instrument)).expect("position");
    let expected = plan
        .after
        .positions
        .iter()
        .find(|p| p.instrument == iid(instrument))
        .map_or((dec!(0), dec!(0)), |after| (after.contracts, after.basis));
    assert_eq!((position.quantity, position.cost_basis), expected);
    let realized: Vec<Decimal> = plan.reductions.iter().map(|r| r.realized_pnl).collect();
    let fees: Vec<Decimal> = plan.reductions.iter().map(|r| r.fee).collect();
    let reconciliation = ledger.reconcile().expect("reconcile");
    assert_eq!(
        reconciliation
            .overdrafts
            .get(&asset("USDC"))
            .copied()
            .unwrap_or_default(),
        plan.debt
    );
    let again = from_ledger(scale, &ledger.snapshot(), marks, 600).expect("account");
    assert_eq!(again.cash, plan.after.cash);
    let replanned = plan_liquidation(
        &rule(scale),
        &again,
        &BTreeMap::new(),
        &LiquidationPolicy {
            target: CloseoutTarget::Flat,
            fee_rate: dec!(0),
            fee_rates: BTreeMap::new(),
            contract_step: dec!(1),
        },
    )
    .expect("plan");
    assert_eq!(replanned.before, plan.after);
    // Fees and realized PnL reach the ledger's journal exactly.
    let journal = ledger.journal();
    let booked: Vec<_> = journal
        .iter()
        .filter(|c| c.execution_id.as_str().starts_with("liq-"))
        .collect();
    assert_eq!(
        booked.iter().map(|c| c.realized_pnl).collect::<Vec<_>>(),
        realized
    );
    assert_eq!(
        booked
            .iter()
            .map(|c| c.fee.as_ref().map_or(dec!(0), |f| f.amount))
            .collect::<Vec<_>>(),
        fees
    );
}

/// The liquidation.md fixture, from the ledger: 10 BTC-PERP long bought at
/// 100 (basis 1000), mark 95, cash 90. upnl = 950 - 1000 = -50, mm 47.5,
/// collateral 40 < 47.5.
///
/// The plan sells 3 at 94: fill cash 282, released 300, realized -18, fee
/// trunc(0.0005 * 282) = 0.141; cash 90 - 18.141 = 71.859, contracts 7,
/// basis 700.
#[test]
fn liquidation_fixture_round_trips_through_the_ledger() {
    let mut ledger = ledger(6, dec!(90));
    book(
        &mut ledger,
        &fill("open", "BTC-PERP", Action::Buy, dec!(100), dec!(10)),
    );
    let marks = marks(&[("BTC-PERP", dec!(95))]);
    let mut account = from_ledger(6, &ledger.snapshot(), &marks, 600).expect("account");
    assert_eq!(
        account,
        LiquidationAccount {
            view: ExposureView {
                account: account.view.account.clone(),
                venue: venue(),
                positions: vec![ExposurePosition {
                    instrument: iid("BTC-PERP"),
                    product: ProductKind::LinearPerpetual,
                    settlement: asset("USDC"),
                    contracts: dec!(10),
                    size: ContractSize::Multiplier(dec!(1)),
                    mark: dec!(95),
                }],
            },
            basis: BTreeMap::from([(iid("BTC-PERP"), dec!(1000))]),
            cash: dec!(90),
            resting_orders: vec![],
        }
    );
    account.resting_orders = vec![margin_rs::OrderId::new("o1").expect("id")];
    let plan = plan_liquidation(
        &rule(6),
        &account,
        &levels("BTC-PERP", &[(dec!(94), dec!(3)), (dec!(93), dec!(100))]),
        &LiquidationPolicy {
            target: CloseoutTarget::Maintenance,
            fee_rate: dec!(0.0005),
            fee_rates: BTreeMap::new(),
            contract_step: dec!(1),
        },
    )
    .expect("plan");
    assert_eq!(plan.outcome, CloseoutOutcome::TargetMet);
    assert_eq!(plan.reductions.len(), 1);
    let reduction = &plan.reductions[0];
    assert_eq!(
        reduction.execution(eid("liq-0"), asset("USDC"), 600),
        Execution::new(
            eid("liq-0"),
            iid("BTC-PERP"),
            Action::Sell,
            dec!(94),
            dec!(3)
        )
        .with_fee(asset("USDC"), dec!(0.141))
        .with_executed_at(600)
    );
    assert_eq!(plan.after.cash, dec!(71.859));

    book_plan(&mut ledger, &plan);
    assert_parity(&ledger, &plan, 6, &marks, "BTC-PERP");
    assert!(ledger.reconcile().expect("reconcile").is_clean());
    let position = ledger.position(&iid("BTC-PERP")).expect("position");
    assert_eq!(
        (position.quantity, position.cost_basis),
        (dec!(7), dec!(700))
    );
    assert_eq!(position.realized_pnl, dec!(-18));
    assert_eq!(ledger.fees_paid(&asset("USDC")), dec!(0.141));
}

/// Half-quantum rounding at USDC 2 decimals, long. Bought 3 Q-PERP at
/// 33.335: N = rhe(100.005) = 100.00, basis 100.00. Cash 50. Fee rate
/// 0.001, target flat, bids 1 at 30.005 then 5 at 30.001:
///
/// - 1 at 30.005: fill cash rhe(30.005) = 30.00, released
///   rhe(100 / 3) = 33.33, realized -3.33, fee trunc(0.030005) = 0.03;
///   cash 46.64, contracts 2, basis 66.67.
/// - 2 at 30.001: fill cash rhe(60.002) = 60.00, released all 66.67,
///   realized -6.67, fee trunc(0.060002) = 0.06; cash 39.91, flat.
#[test]
fn quantum_rounding_matches_the_ledger_long() {
    let mut ledger = ledger(2, dec!(50));
    book(
        &mut ledger,
        &fill("open", "Q-PERP", Action::Buy, dec!(33.335), dec!(3)),
    );
    assert_eq!(
        ledger
            .position(&iid("Q-PERP"))
            .expect("position")
            .cost_basis,
        dec!(100.00)
    );
    let marks = marks(&[("Q-PERP", dec!(33.335))]);
    let account = from_ledger(2, &ledger.snapshot(), &marks, 600).expect("account");
    let plan = plan_liquidation(
        &rule(2),
        &account,
        &levels(
            "Q-PERP",
            &[(dec!(30.005), dec!(1)), (dec!(30.001), dec!(5))],
        ),
        &LiquidationPolicy {
            target: CloseoutTarget::Flat,
            fee_rate: dec!(0.001),
            fee_rates: BTreeMap::new(),
            contract_step: dec!(1),
        },
    )
    .expect("plan");
    let figures: Vec<_> = plan
        .reductions
        .iter()
        .map(|r| {
            (
                r.contracts,
                r.fill_cash,
                r.released_basis,
                r.realized_pnl,
                r.fee,
            )
        })
        .collect();
    assert_eq!(
        figures,
        vec![
            (dec!(1), dec!(30.00), dec!(33.33), dec!(-3.33), dec!(0.03)),
            (dec!(2), dec!(60.00), dec!(66.67), dec!(-6.67), dec!(0.06)),
        ]
    );
    assert_eq!(plan.after.cash, dec!(39.91));
    book_plan(&mut ledger, &plan);
    assert_parity(&ledger, &plan, 2, &marks, "Q-PERP");
    assert!(ledger.reconcile().expect("reconcile").is_clean());
    assert_eq!(
        ledger
            .position(&iid("Q-PERP"))
            .expect("position")
            .realized_pnl,
        dec!(-10.00)
    );
}

/// The same, short. Sold 3 Q-PERP at 33.335: basis -100.00. Asks 1 at
/// 30.005 then 5 at 30.011:
///
/// - buy 1 at 30.005: fill cash -30.00, released rhe(-100 / 3) = -33.33,
///   realized 3.33, fee 0.03; cash 53.30, contracts -2, basis -66.67.
/// - buy 2 at 30.011: fill cash -rhe(60.022) = -60.02, released -66.67,
///   realized 6.65, fee trunc(0.060022) = 0.06; cash 59.89, flat.
#[test]
fn quantum_rounding_matches_the_ledger_short() {
    let mut ledger = ledger(2, dec!(50));
    book(
        &mut ledger,
        &fill("open", "Q-PERP", Action::Sell, dec!(33.335), dec!(3)),
    );
    let marks = marks(&[("Q-PERP", dec!(33.335))]);
    let account = from_ledger(2, &ledger.snapshot(), &marks, 600).expect("account");
    assert_eq!(account.basis[&iid("Q-PERP")], dec!(-100.00));
    let plan = plan_liquidation(
        &rule(2),
        &account,
        &levels(
            "Q-PERP",
            &[(dec!(30.005), dec!(1)), (dec!(30.011), dec!(5))],
        ),
        &LiquidationPolicy {
            target: CloseoutTarget::Flat,
            fee_rate: dec!(0.001),
            fee_rates: BTreeMap::new(),
            contract_step: dec!(1),
        },
    )
    .expect("plan");
    let figures: Vec<_> = plan
        .reductions
        .iter()
        .map(|r| (r.side, r.fill_cash, r.released_basis, r.realized_pnl, r.fee))
        .collect();
    assert_eq!(
        figures,
        vec![
            (
                Action::Buy,
                dec!(-30.00),
                dec!(-33.33),
                dec!(3.33),
                dec!(0.03)
            ),
            (
                Action::Buy,
                dec!(-60.02),
                dec!(-66.67),
                dec!(6.65),
                dec!(0.06)
            ),
        ]
    );
    assert_eq!(plan.after.cash, dec!(59.89));
    book_plan(&mut ledger, &plan);
    assert_parity(&ledger, &plan, 2, &marks, "Q-PERP");
    assert!(ledger.reconcile().expect("reconcile").is_clean());
}

/// A closeout into debt is booked in full by the mandatory path: the
/// balance goes negative by exactly the plan's debt. 10 BTC-PERP at 100,
/// cash 150, mark 80, all sold at 70: realized -300, fee
/// trunc(0.0005 * 700) = 0.35, cash -150.35.
#[test]
fn closeout_into_debt_is_booked_in_full() {
    let mut ledger = ledger(6, dec!(150));
    book(
        &mut ledger,
        &fill("open", "BTC-PERP", Action::Buy, dec!(100), dec!(10)),
    );
    let marks = marks(&[("BTC-PERP", dec!(80))]);
    let account = from_ledger(6, &ledger.snapshot(), &marks, 600).expect("account");
    let plan = plan_liquidation(
        &rule(6),
        &account,
        &levels("BTC-PERP", &[(dec!(70), dec!(100))]),
        &LiquidationPolicy {
            target: CloseoutTarget::Initial,
            fee_rate: dec!(0.0005),
            fee_rates: BTreeMap::new(),
            contract_step: dec!(1),
        },
    )
    .expect("plan");
    assert_eq!(plan.debt, dec!(150.35));
    book_plan(&mut ledger, &plan);
    assert_parity(&ledger, &plan, 6, &marks, "BTC-PERP");
    assert_eq!(
        ledger.balance(&asset("USDC")).expect("balance").total,
        dec!(-150.35)
    );
    assert_eq!(
        ledger.reconcile().expect("reconcile").overdrafts[&asset("USDC")],
        plan.debt
    );
}

/// A basis rounded to zero is read and planned: 1 Q-PERP bought at 0.004
/// has N = rhe(0.004) = 0.00 at USDC 2 decimals.
#[test]
fn zero_basis_is_read_and_planned() {
    let mut ledger = ledger(2, dec!(10));
    book(
        &mut ledger,
        &fill("open", "Q-PERP", Action::Buy, dec!(0.004), dec!(1)),
    );
    let marks = marks(&[("Q-PERP", dec!(0.004))]);
    let account = from_ledger(2, &ledger.snapshot(), &marks, 600).expect("account");
    assert_eq!(account.basis[&iid("Q-PERP")], dec!(0));
    let plan = plan_liquidation(
        &rule(2),
        &account,
        &levels("Q-PERP", &[(dec!(0.004), dec!(1))]),
        &LiquidationPolicy {
            target: CloseoutTarget::Flat,
            fee_rate: dec!(0),
            fee_rates: BTreeMap::new(),
            contract_step: dec!(1),
        },
    )
    .expect("plan");
    book_plan(&mut ledger, &plan);
    assert_parity(&ledger, &plan, 2, &marks, "Q-PERP");
}

/// Cash is the total balance, reservations included; spot holdings are not
/// part of the plan; flat derivatives need no mark.
#[test]
fn cash_is_the_total_and_spot_is_left_out() {
    let mut ledger = ledger(6, dec!(1000));
    book(
        &mut ledger,
        &Execution::new(
            eid("spot"),
            iid("BTC-USDC"),
            Action::Buy,
            dec!(100),
            dec!(1),
        ),
    );
    ledger
        .reserve(
            ReservationId::new("r").expect("id"),
            asset("USDC"),
            dec!(400),
        )
        .expect("reserve");
    book(
        &mut ledger,
        &fill("open", "BTC-PERP", Action::Buy, dec!(100), dec!(1)),
    );
    book(
        &mut ledger,
        &fill("close", "BTC-PERP", Action::Sell, dec!(100), dec!(1)),
    );
    let account = from_ledger(6, &ledger.snapshot(), &BTreeMap::new(), 600).expect("account");
    assert_eq!(account.cash, dec!(900));
    assert_eq!(account.view.positions, vec![]);
    assert_eq!(account.basis, BTreeMap::new());
}

/// Every open derivative must be valued in the plan's currency at a
/// positive mark; nothing is left out.
#[test]
fn invalid_ledger_accounts_are_rejected() {
    let mut ledger = ledger(6, dec!(100));
    book(
        &mut ledger,
        &fill("open", "BTC-PERP", Action::Buy, dec!(100), dec!(1)),
    );
    let snapshot = ledger.snapshot();
    assert_eq!(
        from_ledger(6, &snapshot, &BTreeMap::new(), 600),
        Err(MarginError::MissingMark {
            instrument: iid("BTC-PERP"),
        })
    );
    for mark in [dec!(0), dec!(-1)] {
        assert_eq!(
            from_ledger(6, &snapshot, &marks(&[("BTC-PERP", mark)]), 600),
            Err(MarginError::invalid_input(
                "instrument mark",
                "must be positive"
            ))
        );
    }
    assert!(matches!(
        from_ledger(
            6,
            &snapshot,
            &marks(&[("BTC-PERP", dec!(0.0000000000000000001))]),
            600
        ),
        Err(MarginError::PrecisionExceeded { .. })
    ));
    // The rule rounds to 2 decimals, the ledger to 6.
    assert_eq!(
        from_ledger(2, &snapshot, &marks(&[("BTC-PERP", dec!(100))]), 600),
        Err(MarginError::invalid_input(
            "currency",
            "its ledger scale must be the rule's cash scale"
        ))
    );
    assert_eq!(
        LiquidationAccount::from_ledger(
            &rule(6),
            &snapshot,
            account(),
            venue(),
            &asset("GBP"),
            &BTreeMap::new(),
            600
        ),
        Err(MarginError::UnknownAsset {
            asset: asset("GBP"),
        })
    );

    // An open derivative settled in another asset.
    book(
        &mut ledger,
        &fill("eur", "EUR-PERP", Action::Buy, dec!(10), dec!(1)),
    );
    assert_eq!(
        from_ledger(
            6,
            &ledger.snapshot(),
            &marks(&[("BTC-PERP", dec!(100)), ("EUR-PERP", dec!(10))]),
            600
        ),
        Err(MarginError::CurrencyMismatch {
            field: "settlement",
            expected: asset("USDC"),
            found: asset("EUR"),
        })
    );
}

/// An open future at or after its expiry cannot be closed by forced fills:
/// it is settled first, and is then flat.
#[test]
fn expired_futures_are_settled_not_liquidated() {
    let mut ledger = ledger(6, dec!(100));
    book(
        &mut ledger,
        &fill("open", "BTC-DEC", Action::Buy, dec!(100), dec!(1)),
    );
    let marks = marks(&[("BTC-DEC", dec!(90))]);
    assert_eq!(
        from_ledger(6, &ledger.snapshot(), &marks, 999)
            .expect("account")
            .view
            .positions
            .len(),
        1
    );
    assert_eq!(
        from_ledger(6, &ledger.snapshot(), &marks, 1000),
        Err(MarginError::ExpiredUnsettled {
            instrument: iid("BTC-DEC"),
            expiry_ms: 1000,
            as_of_ms: 1000,
        })
    );
    ledger
        .settle_expiry(&ExpirySettlement {
            id: eid("expiry"),
            instrument: iid("BTC-DEC"),
            price: dec!(90),
            as_of_ms: 1000,
        })
        .expect("settled");
    let account = from_ledger(6, &ledger.snapshot(), &BTreeMap::new(), 2000).expect("account");
    assert_eq!(account.view.positions, vec![]);
    assert_eq!(account.cash, dec!(90));
}
