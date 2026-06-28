//! Unit tests for the PRNG. The generator state is process-global behind a
//! `Once`, so exact values are not asserted; instead we check the documented
//! ranges and the boundary behaviour, which are deterministic.

use mux_runtime::random::*;

const RAND_MAX: i64 = 2147483647;

#[test]
fn int_within_range() {
    mux_rand_init(12345);
    for _ in 0..100 {
        let v = mux_rand_int();
        assert!((0..=RAND_MAX).contains(&v), "out of range: {v}");
    }
}

#[test]
fn range_bounds() {
    // Degenerate range returns the lower bound.
    assert_eq!(mux_rand_range(5, 5), 5);
    assert_eq!(mux_rand_range(10, 3), 10);
    for _ in 0..100 {
        let v = mux_rand_range(10, 20);
        assert!((10..20).contains(&v), "out of range: {v}");
    }
}

#[test]
fn float_within_unit_interval() {
    for _ in 0..100 {
        let v = mux_rand_float();
        assert!((0.0..1.0).contains(&v), "out of range: {v}");
    }
}

#[test]
fn bool_callable() {
    // Just exercise the path; value is non-deterministic.
    let _ = mux_rand_bool();
}
