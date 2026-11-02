//! Risk gate fixtures for venue-declared fee rounding (#38).
//!
//! A sell with fees in base reserves `R + fee bound`. Per fill the bound is
//! `min(A, B)`: `A = (R / lot) * round_up(r * lot)` and
//! `B = r * R + h * ceil(R / m)` (h one grid unit away from zero, half of it
//! in a half mode, 0 toward zero; m the minimum fill). Cumulative, at an
//! exact rate, the bound is `round(r * Q) - round(r * filled)`. Amounts in
//! comments are in units of the 1e-8 BTC grid unless stated. Expected
//! values are written out by hand.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::ledger::{Action, Booking, Execution, Ledger};
use margin_rs::risk::{
    BaseFeeTerms, FeeAsset, FeeBreach, FeeRounding, FillReport, HaltReason, InstrumentLimits,
    OrderRequest, ReservationTerms, RiskDecision, RiskGate, RiskReason,
};
use margin_rs::{
    Asset, Decimal, ExecutionId, InstrumentId, MarginError, OrderId, Rounding, SpotInstrument,
    TransferId,
};
use rust_decimal_macros::dec;

fn asset(code: &str) -> Asset {
    Asset::new(code).unwrap()
}
fn oid(id: &str) -> OrderId {
    OrderId::new(id).unwrap()
}
fn iid(id: &str) -> InstrumentId {
    InstrumentId::new(id).unwrap()
}
/// `k` units of the 1e-8 grid.
fn u(k: i64) -> Decimal {
    Decimal::new(k, 8)
}

fn no_breach() -> Vec<FeeBreach> {
    Vec::new()
}
fn limits(rate: Decimal, rounding: FeeRounding) -> InstrumentLimits {
    InstrumentLimits {
        max_fee_rate: rate,
        fee_asset: FeeAsset::Base,
        fee_rounding: rounding,
        ..InstrumentLimits::default()
    }
}
fn per_fill(mode: Rounding, min_fill: Option<Decimal>) -> FeeRounding {
    FeeRounding::PerFill { mode, min_fill }
}
fn cumulative(mode: Rounding, rate: Decimal) -> FeeRounding {
    FeeRounding::Cumulative { mode, rate }
}

/// USDC (6 decimals), BTC and ETH (8); BTC-USDC and ETH-USDC with price
/// scale 2 and quantity scale 8 (the lot is one grid unit).
fn gate(usdc: Decimal, btc: Decimal, limits: InstrumentLimits) -> RiskGate {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).unwrap();
    ledger.register_asset(asset("BTC"), 8).unwrap();
    ledger.register_asset(asset("ETH"), 8).unwrap();
    for (id, base) in [("BTC-USDC", "BTC"), ("ETH-USDC", "ETH")] {
        ledger
            .register_instrument(
                SpotInstrument::new(iid(id), asset(base), asset("USDC"), 2, 8).unwrap(),
            )
            .unwrap();
    }
    let mut gate = RiskGate::new(ledger);
    for (code, amount) in [("USDC", usdc), ("BTC", btc)] {
        if amount > Decimal::ZERO {
            gate.deposit(
                TransferId::new(format!("d-{code}")).unwrap(),
                asset(code),
                amount,
            )
            .unwrap();
        }
    }
    gate.set_limits(&iid("BTC-USDC"), limits).unwrap();
    gate
}

fn sell(id: &str, quantity: Decimal) -> OrderRequest {
    OrderRequest::new(oid(id), iid("BTC-USDC"), Action::Sell, dec!(100), quantity)
}
fn buy(id: &str, quantity: Decimal) -> OrderRequest {
    OrderRequest::new(oid(id), iid("BTC-USDC"), Action::Buy, dec!(100), quantity)
}

fn fill(
    id: &str,
    side: Action,
    n: Decimal,
    fee: Option<(&str, Decimal)>,
    cum: Decimal,
) -> FillReport {
    let mut e = Execution::new(
        ExecutionId::new(id).unwrap(),
        iid("BTC-USDC"),
        side,
        dec!(100),
        n,
    );
    if let Some((code, amount)) = fee {
        e = e.with_fee(asset(code), amount);
    }
    FillReport::new(e, cum)
}

/// (total, reserved, available) of BTC.
fn btc(gate: &RiskGate) -> (Decimal, Decimal, Decimal) {
    let b = gate.ledger().balance(&asset("BTC")).unwrap();
    (b.total, b.reserved, b.available)
}

#[test]
fn test_reservation_per_declared_rounding_for_a_two_btc_sell() {
    // r 0.001, lot 1e-8: A = 2e8 lots * 2e-8 = 4. B = 2 + 0.002 + h * 2e8:
    // 4.002 away from zero (A wins: 4), 3.002 half, 2.002 toward zero;
    // cumulative 2 + round(0.002) = 2.002.
    for (rounding, reserved) in [
        (FeeRounding::default(), dec!(4)),
        (per_fill(Rounding::AwayFromZero, None), dec!(4)),
        (per_fill(Rounding::HalfEven, None), dec!(3.002)),
        (per_fill(Rounding::HalfAwayFromZero, None), dec!(3.002)),
        (per_fill(Rounding::TowardZero, None), dec!(2.002)),
        (cumulative(Rounding::HalfEven, dec!(0.001)), dec!(2.002)),
    ] {
        let mut gate = gate(dec!(0), dec!(5), limits(dec!(0.001), rounding));
        assert_eq!(
            gate.submit(&sell("s1", dec!(2)), 0),
            RiskDecision::Accepted { quantity: dec!(2) },
            "{rounding:?}"
        );
        assert_eq!(btc(&gate).1, reserved, "{rounding:?}");
    }
}

#[test]
fn test_per_fill_toward_zero_covers_fills_at_their_maximum_fee() {
    // 10 lots, r 0.25: B = 10 + 2.5 = 12.5 -> 13 (A = 20). Fills of 4, 4 and
    // 2 lots at their maximum fees floor(1), floor(1), floor(0.5) = 1, 1, 0:
    // debits 5, 5, 2; reservations 13 -> 8 (B: 6 + 1.5) -> 3 (2 + 0.5) -> 0.
    // On exactly 13 there is no shortfall and 1 is left.
    let mut gate = gate(
        dec!(0),
        u(13),
        limits(dec!(0.25), per_fill(Rounding::TowardZero, None)),
    );
    gate.submit(&sell("s1", u(10)), 0);
    assert_eq!(btc(&gate), (u(13), u(13), u(0)));
    for (i, (n, fee, reserved)) in [(4, 1, 8), (4, 1, 3), (2, 0, 0)].into_iter().enumerate() {
        let cum: i64 = [4, 8, 10][i];
        let outcome = gate
            .on_fill(
                &oid("s1"),
                &fill(
                    &format!("e{i}"),
                    Action::Sell,
                    u(n),
                    (fee > 0).then(|| ("BTC", u(fee))),
                    u(cum),
                ),
            )
            .unwrap();
        assert!(outcome.shortfalls.is_empty() && outcome.fee_breaches.is_empty());
        assert_eq!(btc(&gate).1, u(reserved));
    }
    assert_eq!(btc(&gate), (u(1), u(0), u(1)));
    assert_eq!(gate.halted(), None);
}

#[test]
fn test_per_fill_bound_switches_branch_mid_order() {
    // Away from zero, r 0.25, minimum fill 4 lots: A = 2R, B = 1.25R +
    // ceil(R / 4). For 9 lots B = 11.25 + 3 -> 15; after 4, R 5: B = 8.25
    // -> 9; after 8, R 1: A = 2 < B = 3. Fills of 4 at fee ceil(1) = 1 debit
    // 5 each (frees 6, then 7); the last lot at ceil(0.25) = 1 debits 2 and
    // frees 2.
    let rounding = per_fill(Rounding::AwayFromZero, Some(u(4)));
    let mut gate = gate(dec!(0), u(15), limits(dec!(0.25), rounding));
    gate.submit(&sell("s1", u(9)), 0);
    assert_eq!(btc(&gate).1, u(15));
    for (i, (n, cum, reserved)) in [(4, 4, 9), (4, 8, 2), (1, 9, 0)].into_iter().enumerate() {
        let outcome = gate
            .on_fill(
                &oid("s1"),
                &fill(
                    &format!("e{i}"),
                    Action::Sell,
                    u(n),
                    Some(("BTC", u(1))),
                    u(cum),
                ),
            )
            .unwrap();
        assert!(
            outcome.shortfalls.is_empty() && outcome.fee_breaches.is_empty(),
            "fill {i}"
        );
        assert_eq!(btc(&gate).1, u(reserved), "fill {i}");
    }
    assert_eq!(btc(&gate), (u(3), u(0), u(3)));
}

#[test]
fn test_half_even_with_a_minimum_fill_covers_its_worst_fills() {
    // r 0.25, minimum 2 lots: B = 10 + 2.5 + 0.5 * 5 = 15 (A = 20). Fills of
    // 3, 3, 2 and 2 lots at round_half_even(0.75, 0.75, 0.5, 0.5) = 1, 1,
    // 0, 0: debits 12 against 15.
    let rounding = per_fill(Rounding::HalfEven, Some(u(2)));
    let mut gate = gate(dec!(0), u(15), limits(dec!(0.25), rounding));
    gate.submit(&sell("s1", u(10)), 0);
    assert_eq!(btc(&gate).1, u(15));
    for (i, (n, fee, cum)) in [(3, 1, 3), (3, 1, 6), (2, 0, 8), (2, 0, 10)]
        .into_iter()
        .enumerate()
    {
        let outcome = gate
            .on_fill(
                &oid("s1"),
                &fill(
                    &format!("e{i}"),
                    Action::Sell,
                    u(n),
                    (fee > 0).then(|| ("BTC", u(fee))),
                    u(cum),
                ),
            )
            .unwrap();
        assert!(
            outcome.shortfalls.is_empty() && outcome.fee_breaches.is_empty(),
            "fill {i}"
        );
    }
    assert_eq!(btc(&gate), (u(3), u(0), u(3)));
}

#[test]
fn test_cumulative_reserves_exactly_the_fees_still_to_charge() {
    // 10 lots at exactly 0.25, half even: 10 + round(2.5) = 12. One lot
    // fills charge round(0.25 c) increments 0, 0, 1, 0, 0, 1, 0, 0, 0, 0;
    // the reservation is R + 2 - round(0.25 c) after each, ending at 0 on
    // exactly 12 with nothing left.
    let rounding = cumulative(Rounding::HalfEven, dec!(0.25));
    let mut gate = gate(dec!(0), u(12), limits(dec!(0), rounding));
    gate.submit(&sell("s1", u(10)), 0);
    assert_eq!(btc(&gate), (u(12), u(12), u(0)));
    let charged = [0, 0, 1, 0, 0, 1, 0, 0, 0, 0];
    let mut paid = 0;
    for (c, fee) in (1..=10_i64).zip(charged) {
        let i = c - 1;
        paid += fee;
        let outcome = gate
            .on_fill(
                &oid("s1"),
                &fill(
                    &format!("e{i}"),
                    Action::Sell,
                    u(1),
                    (fee > 0).then(|| ("BTC", u(fee))),
                    u(c),
                ),
            )
            .unwrap();
        assert!(
            outcome.shortfalls.is_empty() && outcome.fee_breaches.is_empty(),
            "fill {i}"
        );
        assert_eq!(btc(&gate).1, u(10 - c + 2 - paid), "fill {i}");
    }
    assert_eq!(btc(&gate), (u(0), u(0), u(0)));
}

#[test]
fn test_cumulative_at_a_wrong_rate_is_caught_and_latches() {
    // Declared exactly 0.6 (away from zero), the venue charges at 0.4: 3
    // lots reserve 3 + ceil(1.8) = 5. Declared increments 1, 1, 0; charged
    // 1, 0, 1. The second fill is the first mismatch (expected 1, charged
    // 0): booked, reported, and the declaration is latched before any new
    // admission under it. Another order under the default declaration may
    // still take the freed unit; the third fill then overdraws and halts.
    let rounding = cumulative(Rounding::AwayFromZero, dec!(0.6));
    let mut gate = gate(dec!(1000), u(5), limits(dec!(0), rounding));
    gate.submit(&sell("s1", u(3)), 0);
    assert_eq!(btc(&gate).1, u(5));
    let f1 = gate
        .on_fill(
            &oid("s1"),
            &fill("e1", Action::Sell, u(1), Some(("BTC", u(1))), u(1)),
        )
        .unwrap();
    assert_eq!(f1.fee_breaches, no_breach());
    assert_eq!(gate.fee_declaration_breaches(&iid("BTC-USDC")).len(), 0);
    let f2 = gate
        .on_fill(&oid("s1"), &fill("e2", Action::Sell, u(1), None, u(2)))
        .unwrap();
    assert_eq!(
        f2.fee_breaches,
        vec![FeeBreach::DeclaredFeeMismatch {
            expected: u(1),
            fee: u(0)
        }]
    );
    assert_eq!(btc(&gate), (u(2), u(1), u(1)));
    let breach = gate.fee_declaration_breaches(&iid("BTC-USDC"))[0].clone();
    assert_eq!(
        (breach.order.clone(), breach.declaration.rounding),
        (oid("s1"), rounding)
    );
    // No new order under the breached declaration takes the freed unit.
    assert_eq!(
        gate.submit(&sell("same", u(1)), 0),
        RiskDecision::Rejected {
            reasons: vec![RiskReason::FeeDeclarationBreached { breach }]
        }
    );
    // Under the default declaration it is admitted (a one lot sell with fees
    // in quote reserves the lot).
    gate.set_limits(&iid("BTC-USDC"), InstrumentLimits::default())
        .unwrap();
    assert!(!gate.submit(&sell("other", u(1)), 0).is_rejected());
    assert_eq!(btc(&gate), (u(2), u(2), u(0)));
    let f3 = gate
        .on_fill(
            &oid("s1"),
            &fill("e3", Action::Sell, u(1), Some(("BTC", u(1))), u(3)),
        )
        .unwrap();
    assert_eq!(
        f3.fee_breaches,
        vec![FeeBreach::DeclaredFeeMismatch {
            expected: u(0),
            fee: u(1)
        }]
    );
    assert_eq!(f3.shortfalls.get(&asset("BTC")), Some(&u(1)));
    assert_eq!(gate.halted(), Some(HaltReason::AccountingBreach));
}

#[test]
fn test_a_breach_latches_only_its_declaration_until_reset() {
    let rounding = per_fill(Rounding::TowardZero, None);
    let mut gate = gate(dec!(1000), dec!(5), limits(dec!(0.001), rounding));
    gate.set_limits(&iid("ETH-USDC"), limits(dec!(0.001), rounding))
        .unwrap();
    gate.submit(&sell("s1", dec!(2)), 0);
    gate.submit(&sell("s2", dec!(1)), 0);
    // A fill of 1 at the bound floor(0.001) = 0.001 is fine; one unit more
    // is a breach, booked anyway.
    let ok = gate
        .on_fill(
            &oid("s1"),
            &fill(
                "e1",
                Action::Sell,
                dec!(1),
                Some(("BTC", dec!(0.001))),
                dec!(1),
            ),
        )
        .unwrap();
    assert_eq!(ok.fee_breaches, no_breach());
    let over = gate
        .on_fill(
            &oid("s1"),
            &fill(
                "e2",
                Action::Sell,
                dec!(0.5),
                Some(("BTC", dec!(0.00050001))),
                dec!(1.5),
            ),
        )
        .unwrap();
    assert_eq!(
        over.fee_breaches,
        vec![FeeBreach::AboveBound {
            bound: dec!(0.0005),
            fee: dec!(0.00050001)
        }]
    );
    assert_eq!(over.shortfalls.len(), 0);
    // Not a halt: only new orders under that declaration are refused.
    assert_eq!(gate.halted(), None);
    let breach = gate.fee_declaration_breaches(&iid("BTC-USDC"))[0].clone();
    assert!(matches!(
        gate.submit(&sell("s3", dec!(0.1)), 0),
        RiskDecision::Rejected { reasons } if reasons == vec![RiskReason::FeeDeclarationBreached { breach: breach.clone() }]
    ));
    // A buy under the same declaration too (no exception for reducing).
    assert!(gate.submit(&buy("b1", dec!(0.1)), 0).is_rejected());
    // Cancels and late reports go on; another instrument is not affected.
    assert_eq!(
        gate.on_cancel_confirmed(&oid("s2"))
            .map(|r| r > Decimal::ZERO),
        Ok(true)
    );
    assert_eq!(gate.fee_declaration_breaches(&iid("ETH-USDC")).len(), 0);
    // The default, worst case declaration is a different one: admitted.
    gate.set_limits(
        &iid("BTC-USDC"),
        limits(dec!(0.001), FeeRounding::default()),
    )
    .unwrap();
    assert!(!gate.submit(&sell("s4", dec!(0.1)), 0).is_rejected());
    // Back to the breached declaration: refused until reset, then admitted.
    gate.set_limits(&iid("BTC-USDC"), limits(dec!(0.001), rounding))
        .unwrap();
    assert!(gate.submit(&sell("s5", dec!(0.1)), 0).is_rejected());
    assert!(gate.reset_fee_declaration(&iid("BTC-USDC")));
    assert!(!gate.reset_fee_declaration(&iid("BTC-USDC")));
    assert!(!gate.submit(&sell("s6", dec!(0.1)), 0).is_rejected());
    gate.ledger().reconcile().unwrap();
}

#[test]
fn test_a_small_fill_that_does_not_complete_the_order_is_a_breach() {
    // Minimum fill 4 lots: a fill of 2 with no fee that leaves the order
    // open is outside the declaration; the last 1 lot is not.
    let rounding = per_fill(Rounding::TowardZero, Some(u(4)));
    let mut gate = gate(dec!(0), dec!(1), limits(dec!(0.25), rounding));
    gate.submit(&sell("s1", u(9)), 0);
    let small = gate
        .on_fill(&oid("s1"), &fill("e1", Action::Sell, u(2), None, u(2)))
        .unwrap();
    assert_eq!(
        small.fee_breaches,
        vec![FeeBreach::BelowMinimumFill {
            min_fill: u(4),
            quantity: u(2)
        }]
    );
    gate.reset_fee_declaration(&iid("BTC-USDC"));
    for (id, n, cum) in [("e2", 4, 6), ("e3", 2, 8)] {
        // 2 lots after 6 still leaves 1 open: a breach again.
        let o = gate
            .on_fill(&oid("s1"), &fill(id, Action::Sell, u(n), None, u(cum)))
            .unwrap();
        assert_eq!(o.fee_breaches.is_empty(), id == "e2", "{id}");
    }
    gate.reset_fee_declaration(&iid("BTC-USDC"));
    let last = gate
        .on_fill(&oid("s1"), &fill("e4", Action::Sell, u(1), None, u(9)))
        .unwrap();
    assert_eq!(last.fee_breaches, no_breach());
}

#[test]
fn test_a_fee_in_another_asset_is_booked_and_reported() {
    // Fees declared in base, toward zero at 0.001; a fill of 0.2 of a sell
    // of 0.5 is charged 0.05 USDC. Booked in USDC, reported, latched; the
    // reservation is what the rest needs whatever the fee: R 0.3 reserves
    // 0.3 + 0.0003 = 0.3003.
    let mut gate = gate(
        dec!(10),
        dec!(1),
        limits(dec!(0.001), per_fill(Rounding::TowardZero, None)),
    );
    gate.submit(&sell("s1", dec!(0.5)), 0);
    assert_eq!(btc(&gate).1, dec!(0.5005));
    let o = gate
        .on_fill(
            &oid("s1"),
            &fill(
                "e1",
                Action::Sell,
                dec!(0.2),
                Some(("USDC", dec!(0.05))),
                dec!(0.2),
            ),
        )
        .unwrap();
    assert_eq!(
        o.fee_breaches,
        vec![FeeBreach::UnexpectedAsset {
            asset: asset("USDC"),
            fee: dec!(0.05)
        }]
    );
    assert_eq!(btc(&gate), (dec!(0.8), dec!(0.3003), dec!(0.4997)));
    // Booked in USDC: 10 + 20 - 0.05.
    let usdc = gate.ledger().balance(&asset("USDC")).unwrap();
    assert_eq!(usdc.total, dec!(29.95));
    assert!(matches!(
        gate.fee_declaration_breaches(&iid("BTC-USDC"))
            .first()
            .map(|b| &b.breach),
        Some(FeeBreach::UnexpectedAsset { .. })
    ));
}

#[test]
fn test_a_replayed_fill_is_not_checked_or_charged_twice() {
    let mut gate = gate(
        dec!(0),
        u(12),
        limits(dec!(0), cumulative(Rounding::HalfEven, dec!(0.25))),
    );
    gate.submit(&sell("s1", u(10)), 0);
    let f = fill("e1", Action::Sell, u(3), Some(("BTC", u(1))), u(3));
    gate.on_fill(&oid("s1"), &f).unwrap();
    let before = btc(&gate);
    let again = gate.on_fill(&oid("s1"), &f).unwrap();
    assert!(matches!(again.booking, Booking::Duplicate(_)));
    assert_eq!(again.fee_breaches, no_breach());
    assert_eq!(btc(&gate), before);
}

#[test]
fn test_cumulative_late_and_over_fills_use_the_real_cumulative() {
    // 4 lots at exactly 0.5 away from zero: 4 + 2 = 6 reserved. A fill of 2
    // (fee ceil(1) = 1), a cancel, then a late fill of 2 (increment
    // ceil(2) - 1 = 1) and an overfill of 1 (ceil(2.5) - 2 = 1): no breach
    // at the declared increments.
    let mut gate = gate(
        dec!(0),
        u(20),
        limits(dec!(0), cumulative(Rounding::AwayFromZero, dec!(0.5))),
    );
    gate.submit(&sell("s1", u(4)), 0);
    assert_eq!(btc(&gate).1, u(6));
    gate.on_fill(
        &oid("s1"),
        &fill("e1", Action::Sell, u(2), Some(("BTC", u(1))), u(2)),
    )
    .unwrap();
    gate.on_cancel_confirmed(&oid("s1")).unwrap();
    assert_eq!(btc(&gate).1, u(0));
    let late = gate
        .on_fill(
            &oid("s1"),
            &fill("e2", Action::Sell, u(2), Some(("BTC", u(1))), u(4)),
        )
        .unwrap();
    assert_eq!(late.fee_breaches, no_breach());
    let over = gate
        .on_fill(
            &oid("s1"),
            &fill("e3", Action::Sell, u(1), Some(("BTC", u(1))), u(5)),
        )
        .unwrap();
    assert_eq!(over.fee_breaches, no_breach());
    // A further lot charged 2 units does not match its declared increment.
    let more = gate
        .on_fill(
            &oid("s1"),
            &fill("e4", Action::Sell, u(1), Some(("BTC", u(2))), u(6)),
        )
        .unwrap();
    // After 5, ceil(3) - ceil(2.5) = 0 is declared for one more lot.
    assert_eq!(
        more.fee_breaches,
        vec![FeeBreach::DeclaredFeeMismatch {
            expected: u(0),
            fee: u(2)
        }]
    );
}

#[test]
fn test_affordable_is_exact_for_each_declared_rounding() {
    // Toward zero, r 0.25: amount(10 lots) = 13, amount(9) = 9 + 2.25 -> 12.
    // Cumulative half even 0.25: amount(10) = 10 + round(2.5) = 12,
    // amount(9) = 9 + round(2.25) = 11. On exactly amount(10) the order is
    // accepted; one unit less it is rejected with that requirement, or
    // reduced to 9 lots whose reservation fits exactly.
    for (rounding, rate, full, nine) in [
        (per_fill(Rounding::TowardZero, None), dec!(0.25), 13, 12),
        (cumulative(Rounding::HalfEven, dec!(0.25)), dec!(0), 12, 11),
    ] {
        let mut exact = gate(dec!(0), u(full), limits(rate, rounding));
        assert!(
            !exact.submit(&sell("s1", u(10)), 0).is_rejected(),
            "{rounding:?}"
        );
        assert_eq!(btc(&exact), (u(full), u(full), u(0)));

        let short_by_one = || gate(dec!(0), u(full - 1), limits(rate, rounding));
        let reason = RiskReason::InsufficientBalance {
            asset: asset("BTC"),
            required: u(full),
            available: u(full - 1),
        };
        assert_eq!(
            short_by_one().submit(&sell("s1", u(10)), 0),
            RiskDecision::Rejected {
                reasons: vec![reason.clone()]
            },
            "{rounding:?}"
        );
        let mut reduced = short_by_one();
        assert_eq!(
            reduced.submit(&sell("s1", u(10)).reducible(), 0),
            RiskDecision::Reduced {
                requested: u(10),
                quantity: u(9),
                reasons: vec![reason]
            },
            "{rounding:?}"
        );
        assert_eq!(btc(&reduced).1, u(nine), "{rounding:?}");
    }
}

#[test]
fn test_declared_rounding_is_validated_before_any_change() {
    let mut gate = gate(
        dec!(0),
        dec!(1),
        limits(dec!(0.001), FeeRounding::default()),
    );
    let bad = [
        // Only fees in base can declare a rounding.
        InstrumentLimits {
            fee_rounding: per_fill(Rounding::TowardZero, None),
            ..InstrumentLimits::default()
        },
        limits(dec!(0.001), per_fill(Rounding::TowardZero, Some(u(0)))),
        limits(
            dec!(0.001),
            per_fill(Rounding::TowardZero, Some(dec!(0.000000001))),
        ),
        limits(dec!(0), cumulative(Rounding::HalfEven, dec!(1))),
        limits(dec!(0), cumulative(Rounding::HalfEven, dec!(-0.1))),
    ];
    for limits_ in bad {
        assert!(matches!(
            gate.set_limits(&iid("BTC-USDC"), limits_),
            Err(MarginError::InvalidInput { .. } | MarginError::PrecisionExceeded { .. })
        ));
        assert_eq!(
            gate.limits(&iid("BTC-USDC")),
            limits(dec!(0.001), FeeRounding::default())
        );
    }
    assert_eq!(
        InstrumentLimits::default().fee_rounding,
        FeeRounding::default()
    );
}

#[test]
fn test_declared_terms_are_fixed_at_admission() {
    // Admitted toward zero (2 BTC reserves 2.002); the limits then return to
    // the default: the live order keeps its terms.
    let mut gate = gate(
        dec!(0),
        dec!(5),
        limits(dec!(0.001), per_fill(Rounding::TowardZero, None)),
    );
    gate.submit(&sell("s1", dec!(2)), 0);
    gate.set_limits(
        &iid("BTC-USDC"),
        limits(dec!(0.001), FeeRounding::default()),
    )
    .unwrap();
    gate.on_fill(
        &oid("s1"),
        &fill(
            "e1",
            Action::Sell,
            dec!(1),
            Some(("BTC", dec!(0.001))),
            dec!(1),
        ),
    )
    .unwrap();
    assert_eq!(btc(&gate), (dec!(3.999), dec!(1.001), dec!(2.998)));
}

#[test]
fn test_a_base_fee_buy_nets_a_one_unit_increment() {
    // Cumulative at exactly 0.5, half away from zero: one lot fills are
    // charged round(0.5) = 1, then round(1) - 1 = 0. Each fee is at most the
    // fill, so the buy never draws on base held elsewhere: no breach, no
    // shortfall with no BTC at all.
    let mut gate = gate(
        dec!(1),
        dec!(0),
        limits(dec!(0), cumulative(Rounding::HalfAwayFromZero, dec!(0.5))),
    );
    gate.submit(&buy("b1", u(2)), 0);
    for (id, fee, cum) in [("e1", Some(("BTC", u(1))), 1), ("e2", None, 2)] {
        let o = gate
            .on_fill(&oid("b1"), &fill(id, Action::Buy, u(1), fee, u(cum)))
            .unwrap();
        assert!(o.fee_breaches.is_empty() && o.shortfalls.is_empty(), "{id}");
    }
    assert_eq!(btc(&gate).0, u(1));
}

#[test]
fn test_every_breached_declaration_stays_latched_until_reset() {
    // Declaration A (toward zero) is breached; B (half even) is admitted,
    // then breached too: both stay refused until the reset clears both.
    let a = per_fill(Rounding::TowardZero, None);
    let b = per_fill(Rounding::HalfEven, None);
    let mut gate = gate(dec!(0), u(100), limits(dec!(0.001), a));
    let breach = |gate: &mut RiskGate, id: &str| {
        gate.submit(&sell(id, u(1)), 0);
        let o = gate
            .on_fill(
                &oid(id),
                &fill(
                    &format!("{id}-e"),
                    Action::Sell,
                    u(1),
                    Some(("BTC", u(1))),
                    u(1),
                ),
            )
            .unwrap();
        assert_eq!(o.fee_breaches.len(), 1, "{id}");
    };
    breach(&mut gate, "a1");
    assert!(gate.submit(&sell("a2", u(1)), 0).is_rejected());
    gate.set_limits(&iid("BTC-USDC"), limits(dec!(0.001), b))
        .unwrap();
    breach(&mut gate, "b1");
    assert!(gate.submit(&sell("b2", u(1)), 0).is_rejected());
    let recorded: Vec<FeeRounding> = gate
        .fee_declaration_breaches(&iid("BTC-USDC"))
        .iter()
        .map(|r| r.declaration.rounding)
        .collect();
    assert_eq!(recorded, vec![a, b]);
    gate.set_limits(&iid("BTC-USDC"), limits(dec!(0.001), a))
        .unwrap();
    assert!(gate.submit(&sell("a3", u(1)), 0).is_rejected());
    assert!(gate.reset_fee_declaration(&iid("BTC-USDC")));
    assert!(!gate.submit(&sell("a4", u(1)), 0).is_rejected());
}

#[test]
fn test_affordable_and_amount_near_the_decimal_limit() {
    // A search over quantities: 7e28 at a lot of 1 is found without forming
    // lo + hi past the Decimal range.
    let plain = ReservationTerms {
        asset: asset("B"),
        scale: 0,
        lot: Decimal::ONE,
        per_lot: Decimal::ONE,
        base_fee: None,
    };
    let big = dec!(70000000000000000000000000000);
    assert_eq!(plain.affordable(big, Decimal::MAX).unwrap(), big);
    // The per lot bound 2 * 4e28 does not fit, the per fill bound
    // 4e28 * 1.001 does: only the chosen bound is converted.
    let toward_zero = ReservationTerms {
        asset: asset("B"),
        scale: 0,
        lot: Decimal::ONE,
        per_lot: dec!(2),
        base_fee: Some(BaseFeeTerms {
            rate: dec!(0.001),
            rounding: per_fill(Rounding::TowardZero, None),
            base: asset("B"),
        }),
    };
    let r = dec!(40000000000000000000000000000);
    assert_eq!(
        toward_zero.amount(r).unwrap(),
        dec!(40040000000000000000000000000)
    );
}

#[test]
fn test_fine_lots_and_unaffordable_candidates_near_the_limit() {
    let terms = |scale: u32, lot: Decimal, per_lot: Decimal, base_fee: Option<BaseFeeTerms>| {
        ReservationTerms {
            asset: asset("B"),
            scale,
            lot,
            per_lot,
            base_fee,
        }
    };
    let fee = |rounding| {
        Some(BaseFeeTerms {
            rate: dec!(0.001),
            rounding,
            base: asset("B"),
        })
    };
    let lot = Decimal::new(1, 18);
    // 1e11 at a lot of 1e-18 has 1e29 possible fills: the count stays wide.
    // By default A = 1e11 * 2 = 2e11 is the reservation; toward zero B =
    // 1e11 * 1.001.
    let default = terms(18, lot, Decimal::new(2, 18), fee(FeeRounding::default()));
    assert_eq!(
        default.amount(dec!(100000000000)).unwrap(),
        dec!(200000000000)
    );
    let toward_zero = terms(
        18,
        lot,
        Decimal::new(2, 18),
        fee(per_fill(Rounding::TowardZero, None)),
    );
    assert_eq!(
        toward_zero.amount(dec!(100000000000)).unwrap(),
        dec!(100100000000)
    );
    // affordable(1, 1e11) toward zero: floor(1 / 1.001) on the 1e-18 grid.
    assert_eq!(
        toward_zero.affordable(dec!(1), dec!(100000000000)).unwrap(),
        dec!(0.999000999000999000)
    );
    assert_eq!(
        terms(18, lot, lot, None)
            .affordable(dec!(1), dec!(100000000000))
            .unwrap(),
        dec!(1)
    );
    // 2 * MAX does not fit, so MAX is not affordable at 2 per unit; the
    // answer floor(MAX / 2) does fit.
    let two = terms(0, Decimal::ONE, dec!(2), None);
    assert_eq!(
        two.affordable(Decimal::MAX, Decimal::MAX).unwrap(),
        dec!(39614081257132168796771975167)
    );
    // Near MAX only whole numbers are Decimals, though the lot is 1e-18:
    // the search runs over lot counts and ends, at MAX - 1.
    let near = Decimal::MAX - Decimal::ONE;
    assert_eq!(
        terms(18, lot, lot, None)
            .affordable(near, Decimal::MAX)
            .unwrap(),
        near
    );
    // The true maximum, 7922816251426433759354395033.75 on a 0.01 lot at 4
    // per lot, is not a Decimal at 2 or 1 digits; the largest Decimal below
    // it is 7922816251426433759354395033.5 (the largest mantissa at 1
    // digit), not the truncation to ...033.
    let four = terms(2, dec!(0.01), dec!(0.04), None);
    assert_eq!(
        four.affordable(dec!(31691265005705735037417580135), Decimal::MAX)
            .unwrap(),
        dec!(7922816251426433759354395033.5)
    );
}

/// TOK at 18 decimals, TOK-USDC with quantity scale 5 (lot 1e-5).
fn tok_gate(tok: Decimal, limits: InstrumentLimits) -> RiskGate {
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).unwrap();
    ledger.register_asset(asset("TOK"), 18).unwrap();
    ledger
        .register_instrument(
            SpotInstrument::new(iid("TOK-USDC"), asset("TOK"), asset("USDC"), 2, 5).unwrap(),
        )
        .unwrap();
    let mut gate = RiskGate::new(ledger);
    gate.deposit(TransferId::new("d-tok").unwrap(), asset("TOK"), tok)
        .unwrap();
    gate.set_limits(&iid("TOK-USDC"), limits).unwrap();
    gate
}

#[test]
fn test_affordable_quantity_has_a_reservable_hold() {
    // About 1e21 TOK at 18 decimals: the exact requirement has more digits
    // than a Decimal, so the hold rounds it up. The affordable quantity's
    // hold is a Decimal, at most the balance, and is what the gate reserves.
    let available = dec!(1001296671011439100452);
    let mut gate = tok_gate(
        available,
        limits(dec!(0.001), per_fill(Rounding::HalfEven, None)),
    );
    let order = OrderRequest::new(
        oid("s1"),
        iid("TOK-USDC"),
        Action::Sell,
        dec!(1),
        dec!(2000000000000000000000),
    )
    .reducible();
    let RiskDecision::Reduced { quantity, .. } = gate.submit(&order, 0) else {
        panic!("expected a reduction");
    };
    let terms = gate.order(&oid("s1")).unwrap().terms.clone().unwrap();
    let hold = terms.amount(quantity).unwrap();
    assert!(hold <= available);
    assert_eq!(gate.ledger().balance(&asset("TOK")).unwrap().reserved, hold);
    // One more lot does not fit.
    let next = terms.amount(quantity + dec!(0.00001));
    assert!(next.map_or(true, |h| h > available));
}

#[test]
fn test_exact_funding_of_a_rounded_hold_covers_every_declared_fill() {
    // 1e12 TOK at a rate with 21 digits, toward zero: the requirement has
    // 18 decimals on 13 integer digits, so the hold is rounded up to a
    // Decimal. Funded with exactly that hold, three fills charged below
    // their declared bound (to 8 decimals, so every balance can be booked)
    // leave no shortfall, and the order ends with nothing reserved.
    let rate = dec!(0.001234567890123456789);
    let rounding = per_fill(Rounding::TowardZero, None);
    let probe = tok_gate(dec!(1), limits(rate, rounding));
    let quantity = dec!(1000000000000);
    let mut sizing = probe;
    sizing.submit(
        &OrderRequest::new(
            oid("p"),
            iid("TOK-USDC"),
            Action::Sell,
            dec!(1),
            dec!(0.00001),
        ),
        0,
    );
    let terms = sizing.order(&oid("p")).unwrap().terms.clone().unwrap();
    let hold = terms.amount(quantity).unwrap();
    assert!(hold.scale() < 18, "the hold is rounded: {hold}");

    let mut gate = tok_gate(hold, limits(rate, rounding));
    assert_eq!(
        gate.submit(
            &OrderRequest::new(oid("s1"), iid("TOK-USDC"), Action::Sell, dec!(1), quantity),
            0
        ),
        RiskDecision::Accepted { quantity }
    );
    let n1 = dec!(123456789.12345);
    let n2 = dec!(500000000000.00001);
    let fills = [n1, n2, quantity - n1 - n2];
    let mut cum = Decimal::ZERO;
    for (i, n) in fills.into_iter().enumerate() {
        cum += n;
        let fee = (rate * n).round_dp_with_strategy(8, rust_decimal::RoundingStrategy::ToZero);
        let report = FillReport::new(
            Execution::new(
                ExecutionId::new(format!("e{i}")).unwrap(),
                iid("TOK-USDC"),
                Action::Sell,
                dec!(1),
                n,
            )
            .with_fee(asset("TOK"), fee),
            cum,
        );
        let outcome = gate.on_fill(&oid("s1"), &report).unwrap();
        assert_eq!(outcome.shortfalls.len(), 0, "fill {i}");
        assert_eq!(outcome.fee_breaches, no_breach(), "fill {i}");
    }
    assert_eq!(
        gate.ledger().balance(&asset("TOK")).unwrap().reserved,
        Decimal::ZERO
    );
    assert_eq!(gate.halted(), None);
    gate.ledger().reconcile().unwrap();
}

#[test]
fn test_a_fill_whose_balance_has_no_decimal_form_fails_and_changes_nothing() {
    // 1e11 TOK at 18 decimals with a lot of 1e-18, held in full by a sell
    // of 1e11. A fill of 1e-18 would leave 99999999999.999999999999999999,
    // which no Decimal holds: the ledger's existing arithmetic error, with
    // nothing booked and no shortfall reported.
    let mut ledger = Ledger::default();
    ledger.register_asset(asset("USDC"), 6).unwrap();
    ledger.register_asset(asset("TOK"), 18).unwrap();
    ledger
        .register_instrument(
            SpotInstrument::new(iid("TOK-USDC"), asset("TOK"), asset("USDC"), 2, 18).unwrap(),
        )
        .unwrap();
    let mut gate = RiskGate::new(ledger);
    gate.deposit(
        TransferId::new("d").unwrap(),
        asset("TOK"),
        dec!(100000000000),
    )
    .unwrap();
    gate.set_limits(
        &iid("TOK-USDC"),
        limits(dec!(0), per_fill(Rounding::TowardZero, None)),
    )
    .unwrap();
    let q = dec!(100000000000);
    gate.submit(
        &OrderRequest::new(oid("s1"), iid("TOK-USDC"), Action::Sell, dec!(1), q),
        0,
    );
    let before = (gate.ledger().snapshot(), gate.order(&oid("s1")).cloned());
    let tiny = Decimal::new(1, 18);
    let report = FillReport::new(
        Execution::new(
            ExecutionId::new("e1").unwrap(),
            iid("TOK-USDC"),
            Action::Sell,
            dec!(1),
            tiny,
        ),
        tiny,
    );
    assert!(matches!(
        gate.on_fill(&oid("s1"), &report),
        Err(MarginError::Arithmetic { .. })
    ));
    assert_eq!(
        (gate.ledger().snapshot(), gate.order(&oid("s1")).cloned()),
        before
    );
    assert_eq!(gate.halted(), None);
}
