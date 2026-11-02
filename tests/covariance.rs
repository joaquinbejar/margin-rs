//! Portfolio covariance fixtures (M3-02), hand calculated.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::covariance::{ValidatedCovariance, portfolio_variance};
use margin_rs::factors::{Factor, FactorExposure, FactorKind};
use margin_rs::{Asset, Decimal, FactorId, MarginError};
use rust_decimal_macros::dec;

fn usdc() -> Asset {
    Asset::new("USDC").expect("asset")
}

fn fid(id: &str) -> FactorId {
    FactorId::new(id).expect("id")
}

fn price(id: &str) -> Factor {
    Factor::new(fid(id), FactorKind::PriceReturn)
}

fn delta(id: &str, amount: Decimal) -> FactorExposure {
    FactorExposure::new(fid(id), FactorKind::PriceReturn, usdc(), amount)
}

fn two_factor(rho: Decimal) -> ValidatedCovariance {
    ValidatedCovariance::from_correlation(
        vec![price("BTC"), price("ETH")],
        vec![dec!(0.02), dec!(0.03)],
        vec![vec![dec!(1), rho], vec![rho, dec!(1)]],
    )
    .expect("valid")
}

fn variance(exposures: &[FactorExposure], cov: &ValidatedCovariance) -> Decimal {
    portfolio_variance(exposures, cov, 6)
        .expect("variance")
        .variance
}

/// One factor: 1000 USDC per unit return, daily sigma 2 % (variance
/// 0.0004): variance 1000^2 * 0.0004 = 400 USDC^2, sigma 20 USDC.
#[test]
fn one_factor() {
    let cov =
        ValidatedCovariance::new(vec![price("BTC")], vec![vec![dec!(0.0004)]]).expect("valid");
    assert_eq!(variance(&[delta("BTC", dec!(1000))], &cov), dec!(400));
    assert_eq!(variance(&[delta("BTC", dec!(-1000))], &cov), dec!(400));
    // Offsetting positions in the same factor net to zero.
    assert_eq!(
        variance(&[delta("BTC", dec!(1000)), delta("BTC", dec!(-1000))], &cov),
        dec!(0)
    );
}

/// Two factors: sigma 2 % and 3 %, rho 0.5, so c11 = 0.0004, c22 = 0.0009,
/// c12 = 0.5 * 0.02 * 0.03 = 0.0003. Exposures 1000 and -500:
/// 1e6 * 0.0004 + 250000 * 0.0009 + 2 * 1000 * (-500) * 0.0003
/// = 400 + 225 - 300 = 325.
#[test]
fn two_factors() {
    let cov = two_factor(dec!(0.5));
    assert_eq!(cov.get(0, 1), Some(dec!(0.0003)));
    let result = portfolio_variance(
        &[delta("BTC", dec!(1000)), delta("ETH", dec!(-500))],
        &cov,
        6,
    )
    .expect("variance");
    assert_eq!(result.variance, dec!(325));
    assert_eq!(result.currency, usdc());
    assert_eq!(result.net_exposures[&fid("ETH")], Some(dec!(-500)));
}

/// Extreme correlations, both factors at sigma 2 %.
///
/// - rho = 1, exposures 1000 and -1000: a perfect hedge, variance 0.
/// - rho = 1, exposures 1000 and 1000: concentrated, (20 + 20)^2 = 1600.
/// - rho = -1, exposures 1000 and -1000: the "hedge" doubles risk, 1600.
/// - rho = -1, exposures 1000 and 1000: perfect offset, 0.
#[test]
fn extreme_correlations() {
    let at = |rho: Decimal| {
        ValidatedCovariance::from_correlation(
            vec![price("A"), price("B")],
            vec![dec!(0.02), dec!(0.02)],
            vec![vec![dec!(1), rho], vec![rho, dec!(1)]],
        )
        .expect("valid")
    };
    let pair = |a: Decimal, b: Decimal| [delta("A", a), delta("B", b)];
    assert_eq!(
        variance(&pair(dec!(1000), dec!(-1000)), &at(dec!(1))),
        dec!(0)
    );
    assert_eq!(
        variance(&pair(dec!(1000), dec!(1000)), &at(dec!(1))),
        dec!(1600)
    );
    assert_eq!(
        variance(&pair(dec!(1000), dec!(-1000)), &at(dec!(-1))),
        dec!(1600)
    );
    assert_eq!(
        variance(&pair(dec!(1000), dec!(1000)), &at(dec!(-1))),
        dec!(0)
    );
    // Uncorrelated: 400 + 400.
    assert_eq!(
        variance(&pair(dec!(1000), dec!(-1000)), &at(dec!(0))),
        dec!(800)
    );
}

/// The variance is rounded up once: 1 * 0.00000015 at scale 6 is 0.000001.
#[test]
fn variance_rounds_up() {
    let cov =
        ValidatedCovariance::new(vec![price("X")], vec![vec![dec!(0.00000015)]]).expect("valid");
    assert_eq!(variance(&[delta("X", dec!(1))], &cov), dec!(0.000001));
}

/// Acceptance 2: invalid matrices are rejected, so risk can never come out
/// negative.
#[test]
fn invalid_matrices_are_rejected() {
    let reject = |factors: Vec<Factor>, matrix: Vec<Vec<Decimal>>| match ValidatedCovariance::new(
        factors, matrix,
    ) {
        Err(MarginError::InvalidCovariance { reason }) => reason,
        other => panic!("expected rejection, got {other:?}"),
    };
    // Not positive semidefinite: |c12| > sqrt(c11 c22). With exposures 1
    // and -1 it would give 0.0004 + 0.0004 - 2 * 0.0005 = -0.0002.
    assert_eq!(
        reject(
            vec![price("A"), price("B")],
            vec![
                vec![dec!(0.0004), dec!(0.0005)],
                vec![dec!(0.0005), dec!(0.0004)]
            ]
        ),
        "the matrix must be positive semidefinite"
    );
    assert_eq!(
        reject(
            vec![price("A"), price("B")],
            vec![
                vec![dec!(0.0004), dec!(0.0001)],
                vec![dec!(0.0002), dec!(0.0004)]
            ]
        ),
        "the matrix must be exactly symmetric"
    );
    assert_eq!(
        reject(vec![price("A")], vec![vec![dec!(-0.0001)]]),
        "variances must not be negative"
    );
    assert_eq!(
        reject(vec![price("A"), price("B")], vec![vec![dec!(0.0004)]]),
        "the matrix must have one row and one column per factor"
    );
    assert_eq!(
        reject(
            vec![price("A"), price("A")],
            vec![vec![dec!(1), dec!(0)], vec![dec!(0), dec!(1)]]
        ),
        "factor identifiers must be unique"
    );
    assert_eq!(reject(vec![], vec![]), "at least one factor is required");
    let three = vec![vec![dec!(1), dec!(0), dec!(0)]; 3];
    assert_eq!(
        reject(vec![price("A"), price("B"), price("C")], three),
        "only one and two factor matrices are supported"
    );
    // Correlation outside [-1, 1] or a diagonal other than 1.
    for corr in [
        vec![vec![dec!(1), dec!(1.01)], vec![dec!(1.01), dec!(1)]],
        vec![vec![dec!(0.9), dec!(0)], vec![dec!(0), dec!(1)]],
    ] {
        assert!(matches!(
            ValidatedCovariance::from_correlation(
                vec![price("A"), price("B")],
                vec![dec!(0.02), dec!(0.02)],
                corr
            ),
            Err(MarginError::InvalidCovariance { .. })
        ));
    }
    // A singular but positive semidefinite matrix is valid.
    assert!(
        ValidatedCovariance::new(
            vec![price("A"), price("B")],
            vec![
                vec![dec!(0.0004), dec!(0.0004)],
                vec![dec!(0.0004), dec!(0.0004)]
            ]
        )
        .is_ok()
    );
}

/// Acceptance 2: a vega exposure cannot be read as price variance.
#[test]
fn vega_is_never_converted_into_price_variance() {
    let cov =
        ValidatedCovariance::new(vec![price("BTC")], vec![vec![dec!(0.0004)]]).expect("valid");
    let vega = FactorExposure::new(fid("BTC"), FactorKind::VolatilityPoint, usdc(), dec!(50));
    assert_eq!(
        portfolio_variance(std::slice::from_ref(&vega), &cov, 6),
        Err(MarginError::FactorUnitMismatch {
            factor: fid("BTC"),
            expected: "price return",
            found: "volatility point",
        })
    );
    // Against a volatility factor (vol of vol: 2 points per day, variance
    // 4 points^2), the same vega has variance 50^2 * 4 = 10000.
    let vol = ValidatedCovariance::new(
        vec![Factor::new(fid("BTC-IV"), FactorKind::VolatilityPoint)],
        vec![vec![dec!(4)]],
    )
    .expect("valid");
    let vega = FactorExposure::new(fid("BTC-IV"), FactorKind::VolatilityPoint, usdc(), dec!(50));
    assert_eq!(variance(&[vega], &vol), dec!(10000));
}

/// Mixed price and volatility factors with a cross covariance.
///
/// Delta 1000 per return (sigma 2 %), vega 50 per vol point (sigma 2
/// points), correlation -0.5: c11 = 0.0004, c22 = 4, c12 = -0.5 * 0.02 * 2
/// = -0.02. Variance 400 + 2500 * 4 + 2 * 1000 * 50 * (-0.02)
/// = 400 + 10000 - 2000 = 8400.
#[test]
fn price_and_volatility_factors() {
    let cov = ValidatedCovariance::from_correlation(
        vec![
            price("BTC"),
            Factor::new(fid("BTC-IV"), FactorKind::VolatilityPoint),
        ],
        vec![dec!(0.02), dec!(2)],
        vec![vec![dec!(1), dec!(-0.5)], vec![dec!(-0.5), dec!(1)]],
    )
    .expect("valid");
    let exposures = [
        delta("BTC", dec!(1000)),
        FactorExposure::new(fid("BTC-IV"), FactorKind::VolatilityPoint, usdc(), dec!(50)),
    ];
    assert_eq!(variance(&exposures, &cov), dec!(8400));
}

/// Exposure validation.
#[test]
fn exposure_validation() {
    let cov = two_factor(dec!(0.5));
    assert_eq!(
        portfolio_variance(&[delta("SOL", dec!(1))], &cov, 6),
        Err(MarginError::UnknownFactor { factor: fid("SOL") })
    );
    let usdt = FactorExposure::new(
        fid("ETH"),
        FactorKind::PriceReturn,
        Asset::new("USDT").expect("asset"),
        dec!(1),
    );
    assert_eq!(
        portfolio_variance(&[delta("BTC", dec!(1)), usdt], &cov, 6),
        Err(MarginError::CurrencyMismatch {
            field: "exposure currency",
            expected: usdc(),
            found: Asset::new("USDT").expect("asset"),
        })
    );
    assert!(portfolio_variance(&[], &cov, 6).is_err());
}

/// M302-C1: positive semidefiniteness is decided exactly, even when the
/// determinant is not a representable Decimal.
#[test]
fn psd_check_is_exact_at_any_scale() {
    let tiny = Decimal::new(1, 16);
    let m = |a: Decimal, b: Decimal, c: Decimal| {
        ValidatedCovariance::new(vec![price("A"), price("B")], vec![vec![a, b], vec![b, c]])
    };
    // det = 1e-32 > 0, not a Decimal.
    assert!(m(tiny, Decimal::ZERO, tiny).is_ok());
    // Singular boundary: det = 0 exactly.
    assert!(m(tiny, tiny, tiny).is_ok());
    // Just past the boundary: c12 = 1.0000000001e-16.
    assert!(matches!(
        m(tiny, dec!(0.00000000000000010000000001), tiny),
        Err(MarginError::InvalidCovariance { .. })
    ));
    // Large scale: c11 * c22 = 1e40 is far beyond Decimal::MAX.
    let big = dec!(100000000000000000000);
    assert!(m(big, big, big).is_ok());
    assert!(matches!(
        m(big, dec!(100000000000000000001), big),
        Err(MarginError::InvalidCovariance { .. })
    ));
}

/// M302-C1: `from_correlation` builds rho * `sigma_i` * `sigma_j` as one exact
/// product. rho = 0.123456789012345 times sigma 1e-14 needs 29 digits, but
/// times sigma 1e14 as well it is rho itself.
#[test]
fn correlation_products_are_exact() {
    let rho = dec!(0.123456789012345);
    let cov = ValidatedCovariance::from_correlation(
        vec![price("A"), price("B")],
        vec![Decimal::new(1, 14), dec!(100000000000000)],
        vec![vec![dec!(1), rho], vec![rho, dec!(1)]],
    )
    .expect("valid");
    assert_eq!(cov.get(0, 1), Some(rho));
    assert_eq!(cov.get(0, 0), Some(Decimal::new(1, 28)));
}

/// M302-C2: netting is exact and order independent. MAX + 0.1 - MAX on one
/// factor with variance 1 nets to 0.1, variance 0.01, in every order.
#[test]
fn netting_is_exact_in_any_order() {
    let cov = ValidatedCovariance::new(vec![price("A")], vec![vec![dec!(1)]]).expect("valid");
    let orders = [
        [Decimal::MAX, dec!(0.1), Decimal::MIN],
        [dec!(0.1), Decimal::MAX, Decimal::MIN],
        [Decimal::MAX, Decimal::MIN, dec!(0.1)],
    ];
    for amounts in orders {
        let exposures: Vec<FactorExposure> = amounts.iter().map(|a| delta("A", *a)).collect();
        let result = portfolio_variance(&exposures, &cov, 6).expect("variance");
        assert_eq!(result.net_exposures[&fid("A")], Some(dec!(0.1)));
        assert_eq!(result.variance, dec!(0.01));
    }
}

/// n2: arithmetic failures while building or validating a matrix are
/// reported as `InvalidCovariance`. sigma 1e15 squared is 1e30 > MAX.
#[test]
fn unrepresentable_covariance_is_invalid() {
    assert_eq!(
        ValidatedCovariance::from_correlation(
            vec![price("A")],
            vec![dec!(1000000000000000)],
            vec![vec![dec!(1)]],
        ),
        Err(MarginError::InvalidCovariance {
            reason: "a covariance rho * sigma_i * sigma_j cannot be represented",
        })
    );
}

/// M302-C3: many exposures net per factor first, so cost is linear in the
/// number of exposures. 100000 exposures of +1 and -1 on two factors plus
/// one of 1000 on A: net (1000, 0), variance 1000^2 * 0.0004 = 400.
#[test]
fn many_exposures_net_first() {
    let cov = two_factor(dec!(0.5));
    let mut exposures = Vec::with_capacity(100_001);
    for i in 0..100_000 {
        let factor = if i % 2 == 0 { "BTC" } else { "ETH" };
        let sign = if (i / 2) % 2 == 0 { dec!(1) } else { dec!(-1) };
        exposures.push(delta(factor, sign));
    }
    exposures.push(delta("BTC", dec!(1000)));
    let result = portfolio_variance(&exposures, &cov, 6).expect("variance");
    assert_eq!(result.net_exposures[&fid("BTC")], Some(dec!(1000)));
    assert_eq!(result.net_exposures[&fid("ETH")], Some(dec!(0)));
    assert_eq!(result.variance, dec!(400));
}

/// Nets stay exact until the final rounding: a net of 2 * MAX on a factor
/// with zero variance is not a Decimal, yet the variance (0) is exact.
#[test]
fn unrepresentable_net_with_representable_variance() {
    let cov = ValidatedCovariance::new(vec![price("A")], vec![vec![dec!(0)]]).expect("valid");
    let result = portfolio_variance(
        &[delta("A", Decimal::MAX), delta("A", Decimal::MAX)],
        &cov,
        6,
    )
    .expect("variance");
    assert_eq!(result.variance, dec!(0));
    assert_eq!(result.net_exposures[&fid("A")], None);
}
