//! Account valuation and stress fixtures (M2-08), hand calculated.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::OrderId;
use margin_rs::ledger::{Execution, Fee, Ledger, Side};
use margin_rs::risk::{FillReport, InstrumentLimits, OrderRequest, RiskGate};
use margin_rs::scenarios::{HypotheticalFill, Scenario, stress};
use margin_rs::valuation::{
    InvalidMarkPolicy, Mark, MarkSet, MarkStatus, PriceSource, ValuationPolicy, value,
};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, SpotInstrument, TransferId,
};
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

fn asset(code: &str) -> Asset {
    Asset::new(code).expect("valid asset")
}

fn pair() -> InstrumentId {
    InstrumentId::new("BTC-USDC").expect("valid id")
}

fn exec(id: &str, side: Side, price: Decimal, quantity: Decimal) -> Execution {
    Execution::new(
        ExecutionId::new(id).expect("id"),
        pair(),
        side,
        price,
        quantity,
    )
}

/// USDC 6, BTC 8, BNB 8; BTC-USDC at price scale 2, quantity scale 8;
/// 1000 USDC deposited and 2 BTC bought at 100 with a 0.2 USDC fee:
/// USDC 799.8, BTC 2, cost basis 200.
fn long_two_btc() -> Ledger {
    let mut ledger = Ledger::default();
    for (code, scale) in [("USDC", 6), ("BTC", 8), ("BNB", 8)] {
        ledger.register_asset(asset(code), scale).expect("register");
    }
    ledger
        .register_instrument(
            SpotInstrument::new(pair(), asset("BTC"), asset("USDC"), 2, 8).expect("spot"),
        )
        .expect("register");
    ledger
        .deposit(
            TransferId::new("d1").expect("id"),
            asset("USDC"),
            dec!(1000),
        )
        .expect("deposit");
    ledger
        .book_execution(
            &exec("b1", Side::Buy, dec!(100), dec!(2)).with_fee(asset("USDC"), dec!(0.2)),
        )
        .expect("book");
    ledger
}

fn policy() -> ValuationPolicy {
    ValuationPolicy::new(asset("USDC"), 1000)
}

fn btc_at(price: Decimal, timestamp_ms: u64) -> MarkSet {
    MarkSet::new(5000).with(
        asset("BTC"),
        Mark::new(price, timestamp_ms, PriceSource::Mid),
    )
}

/// Flat price: equity 799.8 + 2 * 100 = 999.8, net PnL -0.2, which is the
/// fee counted once (realized 0, unrealized 2 * 100 - 200 = 0).
#[test]
fn flat_price_reconciles_exactly() {
    let ledger = long_two_btc();
    let v = value(&ledger.snapshot(), &btc_at(dec!(100), 5000), &policy()).expect("value");
    assert!(v.is_clean());
    assert_eq!(v.equity, Some(dec!(999.8)));
    assert_eq!(v.net_transfers, Some(dec!(1000)));
    assert_eq!(v.net_pnl, Some(dec!(-0.2)));
    let a = v.attribution.expect("attribution");
    assert_eq!(
        (a.realized, a.unrealized, a.fees),
        (dec!(0), dec!(0), dec!(0.2))
    );
    assert_eq!(a.rounding_difference, dec!(0));
    assert_eq!(v.collateral_equity, Some(dec!(999.8)));
    assert_eq!(v.assets[&asset("BTC")].value, Some(dec!(200)));
}

/// Round trip: selling the 2 BTC back at 100 with another 0.2 fee leaves
/// 999.6 USDC, net PnL -0.4, realized 0. No BTC mark is needed any more.
#[test]
fn round_trip_reconciles_without_marks() {
    let mut ledger = long_two_btc();
    ledger
        .book_execution(
            &exec("s1", Side::Sell, dec!(100), dec!(2)).with_fee(asset("USDC"), dec!(0.2)),
        )
        .expect("book");
    let v = value(&ledger.snapshot(), &MarkSet::new(5000), &policy()).expect("value");
    assert!(v.is_clean());
    assert_eq!(v.equity, Some(dec!(999.6)));
    assert_eq!(v.net_pnl, Some(dec!(-0.4)));
    let a = v.attribution.expect("attribution");
    assert_eq!(
        (a.realized, a.unrealized, a.fees),
        (dec!(0), dec!(0), dec!(0.4))
    );
    assert_eq!(v.collateral_equity, Some(dec!(999.6)));
}

/// Mark 110: equity 799.8 + 220 = 1019.8; unrealized 20; net 19.8.
#[test]
fn price_move_is_unrealized_pnl() {
    let ledger = long_two_btc();
    let v = value(&ledger.snapshot(), &btc_at(dec!(110), 4500), &policy()).expect("value");
    assert_eq!(v.equity, Some(dec!(1019.8)));
    assert_eq!(v.net_pnl, Some(dec!(19.8)));
    let a = v.attribution.expect("attribution");
    assert_eq!(
        (a.realized, a.unrealized, a.fees),
        (dec!(0), dec!(20), dec!(0.2))
    );
    assert_eq!(v.collateral_equity, Some(dec!(1019.8)));
}

/// Acceptance 2: an invalid mark is flagged and cannot raise collateral
/// equity unless a policy allows it.
///
/// A stale mark of 200 (age 4000 ms > 1000) would show equity 1199.8, but
/// collateral stays at the 799.8 cash. With a 50 % stale haircut it counts
/// 2 * 200 * 0.5 = 200. Missing, non positive and future marks are flagged
/// too and never count.
#[test]
fn invalid_marks_are_flagged_and_do_not_create_collateral() {
    let snapshot = long_two_btc().snapshot();
    let btc = asset("BTC");

    let stale = value(&snapshot, &btc_at(dec!(200), 1000), &policy()).expect("value");
    assert_eq!(
        stale.flags,
        BTreeMap::from([(btc.clone(), MarkStatus::Stale { age_ms: 4000 })])
    );
    assert_eq!(stale.equity, Some(dec!(1199.8)));
    assert_eq!(stale.collateral_equity, Some(dec!(799.8)));
    assert_eq!(stale.assets[&btc].collateral_value, Some(dec!(0)));

    let haircut = ValuationPolicy {
        invalid_marks: InvalidMarkPolicy::HaircutStale { haircut: dec!(0.5) },
        ..policy()
    };
    let v = value(&snapshot, &btc_at(dec!(200), 1000), &haircut).expect("value");
    assert_eq!(v.collateral_equity, Some(dec!(999.8)));

    let missing = value(&snapshot, &MarkSet::new(5000), &policy()).expect("value");
    assert_eq!(
        missing.flags,
        BTreeMap::from([(btc.clone(), MarkStatus::Missing)])
    );
    assert_eq!(
        (missing.equity, missing.net_pnl, missing.attribution),
        (None, None, None)
    );
    assert_eq!(missing.collateral_equity, Some(dec!(799.8)));

    let zero = value(&snapshot, &btc_at(dec!(0), 5000), &haircut).expect("value");
    assert_eq!(zero.flags[&btc], MarkStatus::NonPositive);
    assert_eq!(zero.equity, None);
    assert_eq!(zero.collateral_equity, Some(dec!(799.8)));

    let future = value(&snapshot, &btc_at(dec!(150), 6000), &haircut).expect("value");
    assert_eq!(future.flags[&btc], MarkStatus::Future { ahead_ms: 1000 });
    assert_eq!(future.equity, Some(dec!(1099.8)));
    assert_eq!(future.collateral_equity, Some(dec!(799.8)));
}

/// Fees in a third asset are valued at its own mark and counted once.
///
/// Deposit 1 BNB too; buy 1 BTC at 100 with a 0.01 BNB fee. At BTC 100 and
/// BNB 300: equity 699.8 + 300 + 0.99 * 300 = 1296.8, transfers
/// 1000 + 300 = 1300, net -3.2 = -(0.2 USDC + 0.01 BNB * 300).
#[test]
fn fees_in_another_asset_are_counted_once() {
    let mut ledger = long_two_btc();
    ledger
        .deposit(TransferId::new("d2").expect("id"), asset("BNB"), dec!(1))
        .expect("deposit");
    ledger
        .book_execution(
            &exec("b2", Side::Buy, dec!(100), dec!(1)).with_fee(asset("BNB"), dec!(0.01)),
        )
        .expect("book");
    let marks =
        btc_at(dec!(100), 5000).with(asset("BNB"), Mark::new(dec!(300), 5000, PriceSource::Last));
    let v = value(&ledger.snapshot(), &marks, &policy()).expect("value");
    assert_eq!(v.equity, Some(dec!(1296.8)));
    assert_eq!(v.net_transfers, Some(dec!(1300)));
    assert_eq!(v.net_pnl, Some(dec!(-3.2)));
    let a = v.attribution.expect("attribution");
    assert_eq!(
        (a.realized, a.unrealized, a.fees),
        (dec!(0), dec!(0), dec!(3.2))
    );
    assert_eq!(a.rounding_difference, dec!(0));

    // Without the BNB mark the fee cannot be valued: no PnL, flagged.
    let v = value(&ledger.snapshot(), &btc_at(dec!(100), 5000), &policy()).expect("value");
    assert_eq!(
        v.flags,
        BTreeMap::from([(asset("BNB"), MarkStatus::Missing)])
    );
    assert_eq!(v.net_pnl, None);
}

/// Off grid marks: every figure is rounded once, the identity
/// `net_pnl = equity - net_transfers` holds exactly, and the attribution
/// differs from it by rounding only.
#[test]
fn off_grid_marks_round_once() {
    let mut ledger = long_two_btc();
    ledger
        .book_execution(&exec("b2", Side::Buy, dec!(100.01), dec!(0.33333333)))
        .expect("book");
    let v = value(
        &ledger.snapshot(),
        &btc_at(dec!(100.0150001), 5000),
        &policy(),
    )
    .expect("value");
    let (equity, transfers, net) = (
        v.equity.expect("e"),
        v.net_transfers.expect("t"),
        v.net_pnl.expect("n"),
    );
    assert_eq!(net, equity - transfers);
    let a = v.attribution.expect("attribution");
    assert!(a.rounding_difference.abs() <= dec!(0.000002));
}

/// The reporting asset must not be priced, and priced assets must exist.
#[test]
fn mark_set_validation() {
    let snapshot = long_two_btc().snapshot();
    let usdc_priced =
        MarkSet::new(0).with(asset("USDC"), Mark::new(dec!(1), 0, PriceSource::Index));
    assert_eq!(
        value(&snapshot, &usdc_priced, &policy()),
        Err(MarginError::invalid_input(
            "marks",
            "must not price the reporting asset"
        ))
    );
    let unknown = MarkSet::new(0).with(asset("ETH"), Mark::new(dec!(1), 0, PriceSource::Index));
    assert_eq!(
        value(&snapshot, &unknown, &policy()),
        Err(MarginError::UnknownAsset {
            asset: asset("ETH")
        })
    );
    let bad_haircut = ValuationPolicy {
        invalid_marks: InvalidMarkPolicy::HaircutStale { haircut: dec!(1.5) },
        ..policy()
    };
    assert!(value(&snapshot, &MarkSet::new(0), &bad_haircut).is_err());
}

fn hedge() -> HypotheticalFill {
    HypotheticalFill {
        instrument: pair(),
        side: Side::Sell,
        price: dec!(100),
        quantity: dec!(2),
        fee: Some(Fee {
            asset: asset("USDC"),
            amount: dec!(0.2),
        }),
    }
}

/// Gap, spread and failed hedge scenarios from the flat case (equity
/// 999.8 at BTC 100).
///
/// - Gap -20 %: BTC at 80, equity 799.8 + 160 = 959.8, change -40.
/// - Spread 0.5 %: BTC valued at 99.5, equity 998.8, change -1.
/// - Hedge (sell 2 at 100, fee 0.2) then gap: USDC 999.6, BTC 0, change
///   -0.2. The same gap with the hedge failed: change -40, so the failed
///   hedge costs 39.8.
#[test]
fn spread_gap_and_failed_hedge() {
    let snapshot = long_two_btc().snapshot();
    let marks = btc_at(dec!(100), 5000);
    let btc = asset("BTC");

    let gap = stress(
        &snapshot,
        &marks,
        &policy(),
        &Scenario::new("gap").shock(btc.clone(), dec!(-0.2)),
    )
    .expect("stress");
    assert_eq!(gap.stressed.equity, Some(dec!(959.8)));
    assert_eq!(gap.equity_change, Some(dec!(-40)));
    assert_eq!(gap.collateral_change, Some(dec!(-40)));
    assert_eq!(
        gap.stressed.attribution.map(|a| a.unrealized),
        Some(dec!(-40))
    );

    let spread = stress(
        &snapshot,
        &marks,
        &policy(),
        &Scenario::new("spread").spread(dec!(0.005)),
    )
    .expect("stress");
    assert_eq!(spread.stressed.equity, Some(dec!(998.8)));
    assert_eq!(spread.equity_change, Some(dec!(-1)));

    let hedged = stress(
        &snapshot,
        &marks,
        &policy(),
        &Scenario::new("hedged gap")
            .fill(hedge())
            .shock(btc.clone(), dec!(-0.2)),
    )
    .expect("stress");
    assert_eq!(hedged.stressed.equity, Some(dec!(999.6)));
    assert_eq!(hedged.equity_change, Some(dec!(-0.2)));
    let a = hedged.stressed.attribution.expect("attribution");
    assert_eq!(
        (a.realized, a.unrealized, a.fees),
        (dec!(0), dec!(0), dec!(0.4))
    );

    let failed = gap.equity_change.expect("gap") - hedged.equity_change.expect("hedged");
    assert_eq!(failed, dec!(-39.8));
    // Stress never touches the ledger snapshot it was given.
    assert_eq!(snapshot, long_two_btc().snapshot());
}

/// Invalid marks stay invalid under stress, and bad parameters fail.
#[test]
fn stress_keeps_invalid_marks_out_of_collateral() {
    let snapshot = long_two_btc().snapshot();
    let stale = btc_at(dec!(200), 1000);
    let up = stress(
        &snapshot,
        &stale,
        &policy(),
        &Scenario::new("up").shock(asset("BTC"), dec!(0.5)),
    )
    .expect("stress");
    assert_eq!(up.stressed.collateral_equity, Some(dec!(799.8)));
    assert_eq!(up.collateral_change, Some(dec!(0)));
    assert_eq!(up.equity_change, Some(dec!(200)));

    let marks = btc_at(dec!(100), 5000);
    assert!(
        stress(
            &snapshot,
            &marks,
            &policy(),
            &Scenario::new("x").shock(asset("BTC"), dec!(-1))
        )
        .is_err()
    );
    assert!(
        stress(
            &snapshot,
            &marks,
            &policy(),
            &Scenario::new("x").spread(dec!(1))
        )
        .is_err()
    );
    let unknown = HypotheticalFill {
        instrument: InstrumentId::new("ETH-USDC").expect("id"),
        ..hedge()
    };
    assert_eq!(
        stress(
            &snapshot,
            &marks,
            &policy(),
            &Scenario::new("x").fill(unknown)
        ),
        Err(MarginError::UnknownInstrument {
            instrument: InstrumentId::new("ETH-USDC").expect("id"),
        })
    );
}

fn sell_btc(quantity: Decimal) -> HypotheticalFill {
    HypotheticalFill {
        instrument: pair(),
        side: Side::Sell,
        price: dec!(100),
        quantity,
        fee: None,
    }
}

/// M208-1 / C1: a debt always counts against collateral.
///
/// From the long of 2 BTC (USDC 799.8), a hypothetical sale of 3 BTC at 100
/// leaves USDC 1099.8 and BTC -1. With a fresh BTC mark of 100, equity and
/// collateral are 999.8. A stale, future, missing or non positive mark
/// cannot drop the debt: collateral is unknown, or under a 50 % stale
/// haircut the debt counts at 100 * 1.5 = 150 (collateral 949.8).
#[test]
fn debts_count_against_collateral_whatever_the_mark() {
    let snapshot = long_two_btc().snapshot();
    let short = Scenario::new("short").fill(sell_btc(dec!(3)));
    let btc = asset("BTC");

    let fresh = stress(&snapshot, &btc_at(dec!(100), 5000), &policy(), &short).expect("stress");
    assert_eq!(fresh.stressed.equity, Some(dec!(999.8)));
    assert_eq!(fresh.stressed.collateral_equity, Some(dec!(999.8)));
    assert_eq!(
        fresh.stressed.assets[&btc].collateral_value,
        Some(dec!(-100))
    );

    for marks in [
        btc_at(dec!(100), 1000),
        btc_at(dec!(100), 6000),
        btc_at(dec!(0), 5000),
        MarkSet::new(5000),
    ] {
        let r = stress(&snapshot, &marks, &policy(), &short).expect("stress");
        assert_eq!(r.stressed.collateral_equity, None, "{marks:?}");
        assert_eq!(r.stressed.assets[&btc].collateral_value, None);
        assert_eq!(r.collateral_change, None);
    }
    let stale = stress(&snapshot, &btc_at(dec!(100), 1000), &policy(), &short).expect("stress");
    assert_eq!(stale.stressed.equity, Some(dec!(999.8)));

    let haircut = ValuationPolicy {
        invalid_marks: InvalidMarkPolicy::HaircutStale { haircut: dec!(0.5) },
        ..policy()
    };
    let r = stress(&snapshot, &btc_at(dec!(100), 1000), &haircut, &short).expect("stress");
    assert_eq!(r.stressed.collateral_equity, Some(dec!(949.8)));
}

/// M208-1: the reviewer's hedge on another quote asset.
///
/// 1000 USDC; BTC-USDT; BTC mark valid at 100, USDT stale. A hypothetical
/// buy of 1 BTC at 100 USDT creates a 100 USDT debt: collateral is unknown
/// rather than 1100; with a 10 % stale haircut the debt counts 110
/// (collateral 1000 + 100 - 110 = 990). Equity stays 1000.
#[test]
fn hedge_debt_in_a_stale_quote_asset() {
    let mut ledger = Ledger::default();
    for (code, scale) in [("USDC", 6), ("USDT", 6), ("BTC", 8)] {
        ledger.register_asset(asset(code), scale).expect("register");
    }
    let btc_usdt = InstrumentId::new("BTC-USDT").expect("id");
    ledger
        .register_instrument(
            SpotInstrument::new(btc_usdt.clone(), asset("BTC"), asset("USDT"), 2, 8).expect("spot"),
        )
        .expect("register");
    ledger
        .deposit(TransferId::new("d").expect("id"), asset("USDC"), dec!(1000))
        .expect("deposit");
    let marks = MarkSet::new(5000)
        .with(asset("BTC"), Mark::new(dec!(100), 5000, PriceSource::Mid))
        .with(asset("USDT"), Mark::new(dec!(1), 1000, PriceSource::Index));
    let hedge = Scenario::new("hedge").fill(HypotheticalFill {
        instrument: btc_usdt,
        side: Side::Buy,
        price: dec!(100),
        quantity: dec!(1),
        fee: None,
    });
    let r = stress(&ledger.snapshot(), &marks, &policy(), &hedge).expect("stress");
    assert_eq!(r.base.collateral_equity, Some(dec!(1000)));
    assert_eq!(r.stressed.collateral_equity, None);
    assert_eq!(r.stressed.equity, Some(dec!(1000)));

    let haircut = ValuationPolicy {
        invalid_marks: InvalidMarkPolicy::HaircutStale { haircut: dec!(0.1) },
        ..policy()
    };
    let r = stress(&ledger.snapshot(), &marks, &haircut, &hedge).expect("stress");
    assert_eq!(r.stressed.collateral_equity, Some(dec!(990)));
}

/// A fee that overdraws an asset without a mark makes collateral unknown;
/// a debt is bought back across the liquidation spread.
#[test]
fn overdrawing_fees_and_debt_liquidation() {
    let snapshot = long_two_btc().snapshot();
    let marks = btc_at(dec!(100), 5000);
    let bnb_fee = Scenario::new("bnb fee").fill(HypotheticalFill {
        fee: Some(Fee {
            asset: asset("BNB"),
            amount: dec!(0.01),
        }),
        ..sell_btc(dec!(1))
    });
    let r = stress(&snapshot, &marks, &policy(), &bnb_fee).expect("stress");
    assert_eq!(r.stressed.assets[&asset("BNB")].total, dec!(-0.01));
    assert_eq!(r.stressed.collateral_equity, None);
    assert_eq!(r.stressed.flags[&asset("BNB")], MarkStatus::Missing);

    // Short 1 BTC bought back at 100 * 1.01 = 101: 1099.8 - 101 = 998.8.
    let r = stress(
        &snapshot,
        &marks,
        &policy(),
        &Scenario::new("short spread")
            .fill(sell_btc(dec!(3)))
            .spread(dec!(0.01)),
    )
    .expect("stress");
    assert_eq!(r.stressed.equity, Some(dec!(998.8)));
}

/// M208-C2: a closed position needs no base mark for its attribution.
///
/// Sell the 2 BTC at 110 without fee: USDC 1019.8, BTC 0, realized 20. No
/// BTC mark is needed: equity 1019.8, attribution (20, 0, 0.2), clean.
#[test]
fn closed_position_attribution_needs_no_base_mark() {
    let mut ledger = long_two_btc();
    ledger
        .book_execution(&exec("s1", Side::Sell, dec!(110), dec!(2)))
        .expect("book");
    let v = value(&ledger.snapshot(), &MarkSet::new(5000), &policy()).expect("value");
    assert!(v.is_clean());
    assert_eq!(v.equity, Some(dec!(1019.8)));
    assert_eq!(v.net_pnl, Some(dec!(19.8)));
    let a = v.attribution.expect("attribution");
    assert_eq!(
        (a.realized, a.unrealized, a.fees),
        (dec!(20), dec!(0), dec!(0.2))
    );
    assert_eq!(a.rounding_difference, dec!(0));
}

/// Scenario parameters are validated before anything is computed.
#[test]
fn scenario_validation() {
    let snapshot = long_two_btc().snapshot();
    let marks = btc_at(dec!(100), 5000);
    assert_eq!(
        stress(
            &snapshot,
            &marks,
            &policy(),
            &Scenario::new("x").shock(asset("USDC"), dec!(-0.1))
        ),
        Err(MarginError::invalid_input(
            "shock",
            "the reporting asset cannot be shocked"
        ))
    );
    // An invalid shock on an asset without a mark is still rejected.
    assert_eq!(
        stress(
            &snapshot,
            &marks,
            &policy(),
            &Scenario::new("x").shock(asset("BNB"), dec!(-2))
        ),
        Err(MarginError::invalid_input(
            "shock",
            "must be greater than -1"
        ))
    );
    let r = stress(
        &snapshot,
        &marks,
        &policy(),
        &Scenario::new("x").shock(asset("BNB"), dec!(-0.5)),
    )
    .expect("stress");
    assert_eq!(r.ignored_shocks, vec![asset("BNB")]);
    assert_eq!(r.equity_change, Some(dec!(0)));
    let fine = HypotheticalFill {
        price: dec!(100.001),
        ..sell_btc(dec!(1))
    };
    assert_eq!(
        stress(&snapshot, &marks, &policy(), &Scenario::new("x").fill(fine)),
        Err(MarginError::PrecisionExceeded {
            field: "price",
            scale: 3,
            max_scale: 2,
        })
    );
}

/// A debt created by a real fill through the gate (an overfilled sell) is
/// valued as a debt.
///
/// 1000 USDC and 1 BTC; sell 1 at 100, filled 2: USDC 1200, BTC -1. At a
/// fresh BTC mark of 100, equity and collateral are 1200 - 100 = 1100. At a
/// stale mark, equity is 1100 (flagged) and collateral is unknown.
#[test]
fn gate_overdraft_is_valued_as_a_debt() {
    let mut gate = RiskGate::new(long_two_btc_free());
    gate.set_limits(&pair(), InstrumentLimits::default())
        .expect("limits");
    let id = OrderId::new("s1").expect("id");
    let order = OrderRequest::new(id.clone(), pair(), Side::Sell, dec!(100), dec!(1));
    assert!(!gate.submit(&order, 0).is_rejected());
    let outcome = gate
        .on_fill(
            &id,
            &FillReport::new(exec("f1", Side::Sell, dec!(100), dec!(2)), dec!(2)),
        )
        .expect("a real fill is booked");
    assert_eq!(
        outcome.shortfalls,
        BTreeMap::from([(asset("BTC"), dec!(1))])
    );
    let snapshot = gate.ledger().snapshot();

    let fresh = value(&snapshot, &btc_at(dec!(100), 5000), &policy()).expect("value");
    assert_eq!(fresh.equity, Some(dec!(1100)));
    assert_eq!(fresh.collateral_equity, Some(dec!(1100)));
    assert_eq!(
        fresh.assets[&asset("BTC")].collateral_value,
        Some(dec!(-100))
    );

    let stale = value(&snapshot, &btc_at(dec!(100), 1000), &policy()).expect("value");
    assert_eq!(stale.equity, Some(dec!(1100)));
    assert_eq!(stale.collateral_equity, None);
    assert_eq!(
        stale.flags[&asset("BTC")],
        MarkStatus::Stale { age_ms: 4000 }
    );
}

/// 1000 USDC and 1 BTC deposited, no trades.
fn long_two_btc_free() -> Ledger {
    let mut ledger = Ledger::default();
    for (code, scale) in [("USDC", 6), ("BTC", 8), ("BNB", 8)] {
        ledger.register_asset(asset(code), scale).expect("register");
    }
    ledger
        .register_instrument(
            SpotInstrument::new(pair(), asset("BTC"), asset("USDC"), 2, 8).expect("spot"),
        )
        .expect("register");
    ledger
        .deposit(
            TransferId::new("d1").expect("id"),
            asset("USDC"),
            dec!(1000),
        )
        .expect("deposit");
    ledger
        .deposit(TransferId::new("d2").expect("id"), asset("BTC"), dec!(1))
        .expect("deposit");
    ledger
}

/// Exact aggregation audit: a stressed price is one exact product.
/// 7e28 * 1.5 overflows, but 7e28 * 1.5 * 0.5 = 5.25e28 does not.
#[test]
fn stressed_price_is_one_exact_product() {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).expect("register");
    ledger.register_asset(asset("BTC"), 8).expect("register");
    ledger
        .deposit(
            TransferId::new("d").expect("id"),
            asset("BTC"),
            dec!(0.000001),
        )
        .expect("deposit");
    let marks = MarkSet::new(0).with(
        asset("BTC"),
        Mark::new(dec!(70000000000000000000000000000), 0, PriceSource::Index),
    );
    let scenario = Scenario::new("x")
        .shock(asset("BTC"), dec!(0.5))
        .spread(dec!(0.5));
    let r = stress(&ledger.snapshot(), &marks, &policy(), &scenario).expect("stress");
    assert_eq!(r.stressed.equity, Some(dec!(52500000000000000000000)));
}

/// Exact aggregation audit: all changes to one asset from a hypothetical
/// fill are applied in one exact step. Holding MAX - 1 BTC, a buy of 2 BTC
/// with a 2 BTC fee leaves MAX - 1 although MAX + 1 never fits.
#[test]
fn hypothetical_fill_changes_apply_in_one_step() {
    let mut ledger = long_two_btc_free();
    ledger
        .deposit(
            TransferId::new("big").expect("id"),
            asset("BTC"),
            Decimal::MAX - dec!(2),
        )
        .expect("deposit");
    let fill = HypotheticalFill {
        instrument: pair(),
        side: Side::Buy,
        price: dec!(1),
        quantity: dec!(2),
        fee: Some(Fee {
            asset: asset("BTC"),
            amount: dec!(2),
        }),
    };
    let marks = btc_at(dec!(0.00000001), 5000);
    let r = stress(
        &ledger.snapshot(),
        &marks,
        &policy(),
        &Scenario::new("x").fill(fill),
    )
    .expect("stress");
    assert_eq!(
        r.stressed.assets[&asset("BTC")].total,
        Decimal::MAX - dec!(1)
    );
}
