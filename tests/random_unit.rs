//! Unit tests for the process-global PRNG.

use mux_runtime::random::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::{mux_random_error_detail, mux_random_error_kind};
use mux_runtime::Value;
use std::sync::{Mutex, MutexGuard, OnceLock};

const RAND_MAX: i64 = 2_147_483_647;

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn int_within_range() {
    let _guard = test_lock();
    mux_rand_init(12345);
    for _ in 0..100 {
        let v = mux_rand_int();
        assert!((0..=RAND_MAX).contains(&v), "out of range: {v}");
    }
}

#[test]
fn range_bounds() {
    let _guard = test_lock();
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
    let _guard = test_lock();
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
    let _guard = test_lock();
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
    let _guard = test_lock();
    for _ in 0..100 {
        let v = mux_rand_float();
        assert!((0.0..1.0).contains(&v), "out of range: {v}");
    }
}

#[test]
fn bool_callable() {
    let _guard = test_lock();
    // Just exercise the path; value is non-deterministic.
    let _ = mux_rand_bool();
}

#[test]
fn reseeding_replays_the_same_sequence() {
    let _guard = test_lock();
    mux_rand_init(20_260_812);
    let first = [
        mux_rand_int(),
        mux_rand_int(),
        mux_rand_int(),
        mux_rand_int(),
    ];

    mux_rand_init(20_260_812);
    let second = [
        mux_rand_int(),
        mux_rand_int(),
        mux_rand_int(),
        mux_rand_int(),
    ];

    assert_eq!(first, second);
}

#[test]
fn bytes_are_seeded_and_length_checked() {
    let _guard = test_lock();
    mux_rand_init(7);
    let first = mux_rand_bytes(8);
    let second = mux_rand_bytes(8);
    unsafe {
        assert!(
            matches!(&*first, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::Bytes(bytes) if bytes.len() == 8))
        );
        assert!(
            matches!(&*second, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::Bytes(bytes) if bytes.len() == 8))
        );
        assert!(mux_rc_dec(first));
        assert!(mux_rc_dec(second));
    }

    let invalid = mux_rand_bytes(-1);
    unsafe {
        assert!(!mux_result_is_ok(invalid));
        let error = mux_result_data(invalid);
        let kind = mux_random_error_kind(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes()));
        assert!(mux_rc_dec(kind));
        assert!(direct_string(mux_random_error_detail(error)).contains("negative"));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(invalid));
    }
}

#[test]
fn distributions_validate_parameters_and_return_results() {
    let _guard = test_lock();
    mux_rand_init(42);
    let normal = mux_rand_normal(10.0, 2.0);
    let zero_width = mux_rand_normal(10.0, 0.0);
    let bad_normal = mux_rand_normal(10.0, -1.0);
    let exponential = mux_rand_exponential(2.0);
    let bad_exponential = mux_rand_exponential(0.0);
    unsafe {
        assert!(
            matches!(&*normal, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::Float(_)))
        );
        assert!(
            matches!(&*zero_width, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::Float(_)))
        );
        assert!(matches!(&*bad_normal, Value::Result(Err(_))));
        assert!(
            matches!(&*exponential, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::Float(_)))
        );
        assert!(matches!(&*bad_exponential, Value::Result(Err(_))));
        for value in [normal, zero_width, bad_normal, exponential, bad_exponential] {
            assert!(mux_rc_dec(value));
        }
    }
}

#[test]
fn independent_random_objects_are_deterministic_and_unbiased() {
    let _guard = test_lock();
    let first = mux_random_seeded(42);
    let second = mux_random_seeded(42);
    let system = mux_random_system();
    assert!(!first.is_null() && !second.is_null() && !system.is_null());
    let first_values = [
        mux_random_next_int(first),
        mux_random_next_int(first),
        mux_random_next_int(first),
    ];
    let second_values = [
        mux_random_next_int(second),
        mux_random_next_int(second),
        mux_random_next_int(second),
    ];
    assert_eq!(first_values, second_values);
    for _ in 0..100 {
        let value = mux_random_next_range(first, -10, 10);
        assert!((-10..10).contains(&value));
        assert!((0.0..1.0).contains(&mux_random_next_float(first)));
    }
    let normal = mux_random_normal(first, 0.0, 1.0);
    let exponential = mux_random_exponential(first, 2.0);
    unsafe {
        assert!(
            matches!(&*normal, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::Float(_)))
        );
        assert!(
            matches!(&*exponential, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::Float(_)))
        );
        assert!(mux_rc_dec(normal));
        assert!(mux_rc_dec(exponential));
    }
    unsafe {
        assert!(mux_rc_dec(first));
        assert!(mux_rc_dec(second));
        assert!(mux_rc_dec(system));
    }
}

#[test]
fn collection_sampling_operations_preserve_value_contracts() {
    let _guard = test_lock();
    mux_rand_init(7);
    let values = mux_rc_alloc(Value::List(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(3),
        Value::Int(4),
    ]));
    let weights = mux_rc_alloc(Value::List(vec![
        Value::Float(1.0.into()),
        Value::Float(2.0.into()),
        Value::Float(3.0.into()),
        Value::Float(4.0.into()),
    ]));
    unsafe {
        let chosen = mux_random_choose(values);
        assert!(
            matches!(&*chosen, Value::Optional(Some(value)) if matches!(value.as_ref(), Value::Int(1..=4)))
        );
        assert!(mux_rc_dec(chosen));

        let sampled = mux_random_sample(values, 2);
        assert!(
            matches!(&*sampled, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::List(items) if items.len() == 2))
        );
        assert!(mux_rc_dec(sampled));

        let weighted = mux_random_weighted_choice(values, weights);
        assert!(
            matches!(&*weighted, Value::Result(Ok(value)) if matches!(value.as_ref(), Value::Int(1..=4)))
        );
        assert!(mux_rc_dec(weighted));

        let overflowing_weights = mux_rc_alloc(Value::List(vec![
            Value::Float(f64::MAX.into()),
            Value::Float(f64::MAX.into()),
            Value::Float(1.0.into()),
            Value::Float(1.0.into()),
        ]));
        let overflowing = mux_random_weighted_choice(values, overflowing_weights);
        assert!(!mux_result_is_ok(overflowing));
        let error = mux_result_data(overflowing);
        assert!(direct_string(mux_random_error_detail(error)).contains("finite, positive total"));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(overflowing));
        assert!(mux_rc_dec(overflowing_weights));

        mux_random_shuffle(values);
        assert!(matches!(&*values, Value::List(items) if items.len() == 4));
        assert!(mux_rc_dec(values));
        assert!(mux_rc_dec(weights));
    }
}

fn direct_string(value: *mut Value) -> String {
    unsafe {
        let output = match &*value {
            Value::String(value) => value.clone(),
            other => panic!("expected String, got {other:?}"),
        };
        assert!(mux_rc_dec(value));
        output
    }
}
