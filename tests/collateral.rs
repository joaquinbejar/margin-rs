//! Collateral fixtures (M3-03A), hand calculated.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::collateral::{BalanceBasis, CollateralPolicy, StalePolicy, value_collateral};
use margin_rs::ledger::{Execution, Ledger, Side};
use margin_rs::valuation::{Mark, MarkSet, MarkStatus, PriceSource};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, ReservationId, SpotInstrument,
    TransferId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).expect("asset")
}

/// USDC 6, BTC 8, ETH 8, DOGE 8. Deposits: 1000 USDC, 0.5 BTC, 10 ETH,
/// 1000 DOGE. Instruments BTC-USDC and ETH-USDC (price scale 2, lot 1e-8).
fn ledger() -> Ledger {
    let mut ledger = Ledger::default();
    for (code, scale) in [("USDC", 6), ("BTC", 8), ("ETH", 8), ("DOGE", 8)] {
        ledger.register_asset(asset(code), scale).expect("register");
    }
    for (base, id) in [("BTC", "BTC-USDC"), ("ETH", "ETH-USDC")] {
        ledger
            .register_instrument(
                SpotInstrument::new(
                    InstrumentId::new(id).expect("id"),
                    asset(base),
                    asset("USDC"),
                    2,
                    8,
                )
                .expect("spot"),
            )
            .expect("register");
    }
    for (i, (code, amount)) in [
        ("USDC", dec!(1000)),
        ("BTC", dec!(0.5)),
        ("ETH", dec!(10)),
        ("DOGE", dec!(1000)),
    ]
    .into_iter()
    .enumerate()
    {
        ledger
            .deposit(
                TransferId::new(format!("d{i}")).expect("id"),
                asset(code),
                amount,
            )
            .expect("deposit");
    }
    ledger
}

fn mark(price: Decimal, at: u64) -> Mark {
    Mark::new(price, at, PriceSource::Index)
}

/// BTC 60000, ETH 3000, DOGE 0.1, all fresh at t = 10000.
fn fresh() -> MarkSet {
    MarkSet::new(10_000)
        .with(asset("BTC"), mark(dec!(60000), 10_000))
        .with(asset("ETH"), mark(dec!(3000), 10_000))
        .with(asset("DOGE"), mark(dec!(0.1), 10_000))
}

/// USDC 0 %, BTC 20 %, ETH 25 %; DOGE not eligible. Max mark age 1000 ms.
fn policy() -> CollateralPolicy {
    CollateralPolicy::new(asset("USDC"), 1000)
        .with_haircut(asset("USDC"), dec!(0))
        .with_haircut(asset("BTC"), dec!(0.2))
        .with_haircut(asset("ETH"), dec!(0.25))
}

/// Acceptance 1: the multi-balance haircut calculation.
///
/// | asset | balance | price | haircut | collateral |
/// |---|---|---|---|---|
/// | USDC | 1000 | 1 | 0 % | 1000 |
/// | BTC | 0.5 | 60000 | 20 % | 0.5 * 60000 * 0.8 = 24000 |
/// | ETH | 10 | 3000 | 25 % | 10 * 3000 * 0.75 = 22500 |
/// | DOGE | 1000 | 0.1 | not eligible | 0 |
///
/// Total 47500.
#[test]
fn multi_balance_haircuts() {
    let v = value_collateral(&ledger().snapshot(), &fresh(), &policy()).expect("value");
    let line = |code: &str| v.lines[&asset(code)].value;
    assert_eq!(line("USDC"), Some(dec!(1000)));
    assert_eq!(line("BTC"), Some(dec!(24000)));
    assert_eq!(line("ETH"), Some(dec!(22500)));
    assert_eq!(line("DOGE"), Some(dec!(0)));
    assert_eq!(v.lines[&asset("DOGE")].haircut, None);
    assert_eq!(v.lines[&asset("BTC")].haircut, Some(dec!(0.2)));
    assert_eq!(v.total, Some(dec!(47500)));
    assert!(v.flags.is_empty());
}

/// Reserved cash is not collateral by default: reserving 200 USDC leaves
/// 47300; on the total basis it is 47500 again.
#[test]
fn available_basis_excludes_reservations() {
    let mut ledger = ledger();
    ledger
        .reserve(
            ReservationId::new("r1").expect("id"),
            asset("USDC"),
            dec!(200),
        )
        .expect("reserve");
    let snapshot = ledger.snapshot();
    let available = value_collateral(&snapshot, &fresh(), &policy()).expect("value");
    assert_eq!(available.total, Some(dec!(47300)));
    let total = CollateralPolicy {
        basis: BalanceBasis::Total,
        ..policy()
    };
    assert_eq!(
        value_collateral(&snapshot, &fresh(), &total)
            .expect("value")
            .total,
        Some(dec!(47500))
    );
}

/// Acceptance 2: stale marks cannot silently increase collateral.
///
/// A stale BTC mark of 70000 (age 5000 ms) would be worth 28000 after the
/// haircut. Under the default policy BTC counts nothing (total 23500,
/// flagged); only an explicit extra haircut uses it (0.5 * 70000 * 0.7 =
/// 24500); the reject policy fails.
#[test]
fn stale_marks_follow_the_declared_policy() {
    let snapshot = ledger().snapshot();
    let stale = fresh().with(asset("BTC"), mark(dec!(70000), 5000));

    let excluded = value_collateral(&snapshot, &stale, &policy()).expect("value");
    assert_eq!(excluded.lines[&asset("BTC")].value, Some(dec!(0)));
    assert_eq!(
        excluded.flags[&asset("BTC")],
        MarkStatus::Stale { age_ms: 5000 }
    );
    assert_eq!(excluded.total, Some(dec!(23500)));

    let extra = CollateralPolicy {
        stale: StalePolicy::ExtraHaircut { extra: dec!(0.1) },
        ..policy()
    };
    let v = value_collateral(&snapshot, &stale, &extra).expect("value");
    assert_eq!(v.lines[&asset("BTC")].value, Some(dec!(24500)));

    let reject = CollateralPolicy {
        stale: StalePolicy::Reject,
        ..policy()
    };
    assert_eq!(
        value_collateral(&snapshot, &stale, &reject),
        Err(MarginError::InvalidCollateralMark {
            asset: asset("BTC")
        })
    );
    // A stale mark on an ineligible asset (DOGE) does not matter.
    let stale_doge = fresh().with(asset("DOGE"), mark(dec!(0.1), 0));
    assert_eq!(
        value_collateral(&snapshot, &stale_doge, &reject)
            .expect("value")
            .total,
        Some(dec!(47500))
    );
}

/// Missing, non positive and future marks count nothing for held assets.
#[test]
fn missing_and_bad_marks_count_nothing() {
    let snapshot = ledger().snapshot();
    let mut no_eth = fresh();
    no_eth.prices.remove(&asset("ETH"));
    let v = value_collateral(&snapshot, &no_eth, &policy()).expect("value");
    assert_eq!(v.flags[&asset("ETH")], MarkStatus::Missing);
    assert_eq!(v.total, Some(dec!(25000)));

    let future = fresh().with(asset("ETH"), mark(dec!(3000), 20_000));
    let v = value_collateral(&snapshot, &future, &policy()).expect("value");
    assert_eq!(
        v.flags[&asset("ETH")],
        MarkStatus::Future { ahead_ms: 10_000 }
    );
    assert_eq!(v.total, Some(dec!(25000)));

    let zero = fresh().with(asset("ETH"), mark(dec!(0), 10_000));
    assert_eq!(
        value_collateral(&snapshot, &zero, &policy())
            .expect("value")
            .total,
        Some(dec!(25000))
    );
    let reject = CollateralPolicy {
        stale: StalePolicy::Reject,
        ..policy()
    };
    assert_eq!(
        value_collateral(&snapshot, &no_eth, &reject),
        Err(MarginError::InvalidCollateralMark {
            asset: asset("ETH")
        })
    );
}

/// Debts count in full and never shrink with a haircut; a debt without a
/// trusted mark makes collateral unknown.
///
/// A real overdrawn sale of 1 BTC (0.5 held) at 60000 leaves BTC -0.5 and
/// USDC 61000. Fresh: 61000 - 0.5 * 60000 + 22500 = 53500. Stale BTC: total
/// unknown; with a 10 % extra haircut the debt counts 0.5 * 60000 * 1.1 =
/// 33000, total 61000 - 33000 + 22500 = 50500.
#[test]
fn debts_count_in_full() {
    let mut ledger = ledger();
    let sale = Execution::new(
        ExecutionId::new("e1").expect("id"),
        InstrumentId::new("BTC-USDC").expect("id"),
        Side::Sell,
        dec!(60000),
        dec!(1),
    );
    ledger
        .book_execution_allowing_overdraft(&sale, None)
        .expect("a real execution is booked");
    let snapshot = ledger.snapshot();

    let v = value_collateral(&snapshot, &fresh(), &policy()).expect("value");
    assert_eq!(v.lines[&asset("BTC")].value, Some(dec!(-30000)));
    assert_eq!(v.lines[&asset("BTC")].haircut, None);
    assert_eq!(v.total, Some(dec!(53500)));

    let old_marks = fresh().with(asset("BTC"), mark(dec!(60000), 0));
    let v = value_collateral(&snapshot, &old_marks, &policy()).expect("value");
    assert_eq!(v.lines[&asset("BTC")].value, None);
    assert_eq!(v.total, None);

    let extra = CollateralPolicy {
        stale: StalePolicy::ExtraHaircut { extra: dec!(0.1) },
        ..policy()
    };
    let v = value_collateral(&snapshot, &old_marks, &extra).expect("value");
    assert_eq!(v.lines[&asset("BTC")].value, Some(dec!(-33000)));
    assert_eq!(v.total, Some(dec!(50500)));

    let mut no_btc = fresh();
    no_btc.prices.remove(&asset("BTC"));
    assert_eq!(
        value_collateral(&snapshot, &no_btc, &policy())
            .expect("value")
            .total,
        None
    );
}

/// Lines round down once: 0.00000003 ETH at 0.33 with a 25 % haircut is
/// 7.425e-9, collateral 0 at the 6 digit USDC scale.
#[test]
fn lines_round_down() {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).expect("register");
    ledger.register_asset(asset("ETH"), 8).expect("register");
    ledger
        .deposit(
            TransferId::new("d").expect("id"),
            asset("ETH"),
            dec!(0.00000003),
        )
        .expect("deposit");
    let marks = MarkSet::new(0).with(asset("ETH"), mark(dec!(0.33), 0));
    let policy = CollateralPolicy::new(asset("USDC"), 1000).with_haircut(asset("ETH"), dec!(0.25));
    let v = value_collateral(&ledger.snapshot(), &marks, &policy).expect("value");
    assert_eq!(v.total, Some(dec!(0)));
}

/// Policy validation.
#[test]
fn policy_validation() {
    let snapshot = ledger().snapshot();
    let bad = policy().with_haircut(asset("BTC"), dec!(1.2));
    assert!(value_collateral(&snapshot, &fresh(), &bad).is_err());
    let too_much = CollateralPolicy {
        stale: StalePolicy::ExtraHaircut { extra: dec!(0.8) },
        ..policy()
    };
    assert_eq!(
        value_collateral(&snapshot, &fresh(), &too_much),
        Err(MarginError::invalid_input(
            "extra",
            "haircut + extra must not exceed 1"
        ))
    );
    let unknown = policy().with_haircut(asset("SOL"), dec!(0.5));
    assert_eq!(
        value_collateral(&snapshot, &fresh(), &unknown),
        Err(MarginError::UnknownAsset {
            asset: asset("SOL")
        })
    );
}

/// M303A-C1: the total is the exact sum of the rounded lines, whatever the
/// asset names and their order.
///
/// Balances MAX, 0.1 and MIN (a debt), all at a fresh mark of 1 with no
/// haircut: each line is representable and the total is exactly 0.1. A
/// total that is not representable is an error; an unknown line makes the
/// total unknown in any order.
#[test]
fn total_is_exact_whatever_the_order() {
    use margin_rs::ledger::{BalanceSnapshot, LedgerSnapshot};
    use std::collections::BTreeMap;

    let snapshot = |balances: &[(&str, Decimal)]| {
        let mut assets = BTreeMap::from([(asset("USDC"), 6)]);
        let mut map = BTreeMap::new();
        for (code, amount) in balances {
            assets.insert(asset(code), 6);
            map.insert(
                asset(code),
                BalanceSnapshot {
                    total: *amount,
                    reserved: Decimal::ZERO,
                    available: *amount,
                },
            );
        }
        LedgerSnapshot {
            assets,
            instruments: BTreeMap::new(),
            net_transfers: BTreeMap::new(),
            balances: map,
            positions: BTreeMap::new(),
            fees_paid: BTreeMap::new(),
            settled_pnl: BTreeMap::new(),
            reservations: BTreeMap::new(),
            executions_booked: 0,
            settlements_booked: 0,
        }
    };
    let names = ["AAA", "BBB", "CCC"];
    let amounts = [Decimal::MAX, dec!(0.1), Decimal::MIN];
    // Every assignment of the three amounts to the three names (the
    // snapshot iterates by name, so this permutes the summation order).
    let perms = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for perm in perms {
        let balances: Vec<(&str, Decimal)> = names
            .iter()
            .zip(perm)
            .map(|(n, i)| (*n, amounts[i]))
            .collect();
        let mut marks = MarkSet::new(0);
        let mut policy = CollateralPolicy::new(asset("USDC"), 1000);
        for name in names {
            marks = marks.with(asset(name), mark(dec!(1), 0));
            policy = policy.with_haircut(asset(name), dec!(0));
        }
        let v = value_collateral(&snapshot(&balances), &marks, &policy).expect("value");
        assert_eq!(v.total, Some(dec!(0.1)), "{perm:?}");

        // The same with the debt's mark missing: unknown in every order.
        let debt = balances
            .iter()
            .find(|(_, a)| *a < Decimal::ZERO)
            .expect("debt")
            .0;
        marks.prices.remove(&asset(debt));
        let v = value_collateral(&snapshot(&balances), &marks, &policy).expect("value");
        assert_eq!(v.total, None, "{perm:?}");
    }

    // MAX + MAX is not representable: an error, not a saturation.
    let both = [("AAA", Decimal::MAX), ("BBB", Decimal::MAX)];
    let marks = MarkSet::new(0)
        .with(asset("AAA"), mark(dec!(1), 0))
        .with(asset("BBB"), mark(dec!(1), 0));
    let policy = CollateralPolicy::new(asset("USDC"), 1000)
        .with_haircut(asset("AAA"), dec!(0))
        .with_haircut(asset("BBB"), dec!(0));
    assert_eq!(
        value_collateral(&snapshot(&both), &marks, &policy),
        Err(MarginError::arithmetic("collateral total"))
    );
}
