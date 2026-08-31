//! Unit tests for the PRNG. The generator state is process-global behind a
//! `Once`, so exact values are not asserted; instead we check the documented
//! ranges and the boundary behaviour, which are deterministic.

use mux_runtime::random::*;

const RAND_MAX: i64 = 2_147_483_647;

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
fn range_handles_i64_domain_boundaries() {
    mux_rand_init(29);

    for (min, max) in [
        (i64::MIN, i64::MAX),
        (i64::MIN, 0),
        (0, i64::MAX),
        (i64::MIN, i64::MIN + 1),
        (i64::MAX - 1, i64::MAX),
    ] {
        for _ in 0..32 {
            let value = mux_rand_range(min, max);
            assert!(
                (min..max).contains(&value),
                "value {value} escaped range [{min}, {max})"
            );
        }
    }
}

/// A containment assertion cannot catch a range that is too NARROW: `[10, 15)`
/// sits happily inside `[10, 20)`, which is why `range_bounds` above passed
/// while `mux_rand_range` returned only the lower half of every range.
///
/// So assert coverage instead - every value in a small range must appear, and a
/// large range must reach its top. Both fail if the scaling is off by a factor.
#[test]
fn range_covers_whole_span() {
    mux_rand_init(20_260_812);

    let mut seen = [false; 6];
    for _ in 0..6000 {
        let v = mux_rand_range(0, 6);
        assert!((0..6).contains(&v), "out of range: {v}");
        seen[v as usize] = true;
    }
    for (value, hit) in seen.iter().enumerate() {
        assert!(*hit, "value {value} never produced by mux_rand_range(0, 6)");
    }

    // With 6000 draws over 100 values, never reaching the top half would mean
    // the scaling is wrong, not that we were unlucky.
    let highest = (0..6000).map(|_| mux_rand_range(0, 100)).max().unwrap_or(0);
    assert!(
        highest >= 50,
        "mux_rand_range(0, 100) never exceeded {highest}; expected values across the full span"
    );
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
