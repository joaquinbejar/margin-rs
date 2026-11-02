//! Integration tests for the public error boundary.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use margin_rs::{MarginError, Result};

fn checked_add(a: u8, b: u8) -> Result<u8> {
    a.checked_add(b)
        .ok_or(MarginError::arithmetic("checked_add"))
}

fn double_sum(a: u8, b: u8) -> Result<u8> {
    let sum = checked_add(a, b)?;
    checked_add(sum, sum)
}

#[test]
fn question_mark_propagates_margin_error() {
    assert_eq!(double_sum(10, 20), Ok(60));
    assert_eq!(
        double_sum(100, 100),
        Err(MarginError::Arithmetic {
            operation: "checked_add",
        })
    );
}

#[test]
fn error_is_a_thread_safe_std_error() {
    fn assert_bounds<E: std::error::Error + Send + Sync + 'static>() {}
    assert_bounds::<MarginError>();

    let boxed: Box<dyn std::error::Error + Send + Sync> =
        Box::new(MarginError::invalid_input("price", "must be positive"));
    assert_eq!(boxed.to_string(), "invalid input `price`: must be positive");
}

#[test]
fn callers_match_with_a_wildcard_arm() {
    // MarginError is non_exhaustive: downstream crates must keep a wildcard.
    let label = match MarginError::arithmetic("x") {
        MarginError::InvalidInput { .. } => "input",
        MarginError::Arithmetic { .. } => "arithmetic",
        _ => "other",
    };
    assert_eq!(label, "arithmetic");
}
