//! Unit tests for the math module (pure floating-point helpers).

mod common;

use common::{assert_err, ok_int};
use mux_runtime::math;
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::mux_result_data;
use mux_runtime::std::{mux_math_error_detail, mux_math_error_kind};
use mux_runtime::Value;

fn approx(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "expected {a} ~= {b}");
}

fn ok_float(result: *mut mux_runtime::Value) -> f64 {
    assert!(unsafe { mux_runtime::result::mux_result_is_ok(result) });
    let data = unsafe { mux_runtime::result::mux_result_data(result) };
    let value = unsafe {
        match &*data {
            mux_runtime::Value::Float(value) => value.into_inner(),
            other => panic!("expected Float, got {other:?}"),
        }
    };
    assert!(unsafe { mux_runtime::refcount::mux_rc_dec(data) });
    assert!(unsafe { mux_runtime::refcount::mux_rc_dec(result) });
    value
}

#[test]
fn powers_and_roots() {
    approx(math::pow(2.0, 10.0), 1024.0);
    approx(math::sqrt(9.0), 3.0);
    approx(math::hypot(3.0, 4.0), 5.0);
    approx(math::exp(0.0), 1.0);
}

#[test]
fn logarithms() {
    approx(math::ln(std::f64::consts::E), 1.0);
    approx(math::log2(8.0), 3.0);
    approx(math::log10(1000.0), 3.0);
    approx(math::log(81.0, 3.0), 4.0);
}

#[test]
fn trigonometry() {
    approx(math::sin(0.0), 0.0);
    approx(math::cos(0.0), 1.0);
    approx(math::tan(0.0), 0.0);
    approx(math::asin(0.0), 0.0);
    approx(math::acos(1.0), 0.0);
    approx(math::atan(0.0), 0.0);
    approx(math::atan2(0.0, 1.0), 0.0);
}

#[test]
fn rounding_and_extremes() {
    approx(math::abs(-2.0), 2.0);
    approx(math::floor(1.7), 1.0);
    approx(math::ceil(1.2), 2.0);
    approx(math::round(1.5), 2.0);
    approx(math::min(2.0, 5.0), 2.0);
    approx(math::max(2.0, 5.0), 5.0);
    approx(math::trunc(-1.75), -1.0);
    approx(math::fract(-1.75), -0.75);
    approx(math::sinh(0.0), 0.0);
    approx(math::cosh(0.0), 1.0);
    approx(math::tanh(0.0), 0.0);
    approx(math::asinh(0.0), 0.0);
    approx(math::acosh(1.0), 0.0);
    approx(math::atanh(0.0), 0.0);
    approx(math::to_degrees(std::f64::consts::PI), 180.0);
    approx(math::to_radians(180.0), std::f64::consts::PI);
    approx(math::exp2(10.0), 1024.0);
    approx(math::exp_m1(1.0), std::f64::consts::E - 1.0);
    approx(math::ln_1p(1.0), 2.0_f64.ln());
    approx(math::cbrt(27.0), 3.0);
    approx(math::signum(-4.0), -1.0);
    approx(math::erf(0.0), 0.0);
    approx(math::gamma(5.0), 24.0);
}

#[test]
fn classification_and_integer_helpers() {
    assert!(math::mux_math_is_nan(f64::NAN));
    assert!(math::mux_math_is_infinite(f64::INFINITY));
    assert!(math::mux_math_is_finite(1.0));
    assert_eq!(math::mux_math_gcd(84, -30), 6);
    assert_eq!(math::mux_math_lcm(21, 6), 42);
    assert_eq!(math::mux_math_gcd(i64::MIN, 0), i64::MAX);
    assert_eq!(math::mux_math_lcm(i64::MIN, 1), i64::MAX);
    assert_eq!(math::mux_math_isqrt(0), 0);
    assert_eq!(math::mux_math_isqrt(15), 3);
    assert_eq!(math::mux_math_isqrt(i64::MAX), 3_037_000_499);
}

#[test]
fn interpolation_and_combinatorics() {
    approx(math::clamp(12.0, 0.0, 10.0), 10.0);
    approx(math::clamp(3.0, 10.0, 0.0), 3.0);
    approx(math::lerp(10.0, 20.0, 0.25), 12.5);
    approx(math::clamp_checked(4.0, 0.0, 10.0).unwrap(), 4.0);
    assert!(math::clamp_checked(4.0, 10.0, 0.0).is_err());
    approx(math::inverse_lerp(10.0, 20.0, 15.0).unwrap(), 0.5);
    assert!(math::inverse_lerp(1.0, 1.0, 1.0).is_err());
    approx(math::smoothstep(0.0, 1.0, 0.5).unwrap(), 0.5);
    assert!(math::smoothstep(1.0, 0.0, 0.5).is_err());
    assert_eq!(math::factorial(10), Ok(3_628_800));
    assert!(math::factorial(-1).is_err());
    assert_eq!(math::combinations(10, 3), Ok(120));
    assert_eq!(math::permutations(10, 3), Ok(720));
    assert!(math::combinations(3, 4).is_err());
}

#[test]
fn compensated_sequence_reductions() {
    let values = [1.0e16, 1.0, -1.0e16];
    approx(math::sum(&values), 1.0);
    approx(math::product(&[1.5, 2.0, 4.0]), 12.0);
    approx(math::sum(&[]), 0.0);
    approx(math::product(&[]), 1.0);

    let list = mux_runtime::refcount::mux_rc_alloc(mux_runtime::Value::List(
        values
            .iter()
            .copied()
            .map(|value| mux_runtime::Value::Float(ordered_float::OrderedFloat(value)))
            .collect(),
    ));
    approx(ok_float(unsafe { math::mux_math_sum(list) }), 1.0);
    assert!(unsafe { mux_runtime::refcount::mux_rc_dec(list) });
}

#[test]
fn checked_math_externs_return_results() {
    assert_eq!(ok_int(math::mux_math_factorial(5)), 120);
    assert_eq!(ok_int(math::mux_math_combinations(5, 2)), 10);
    assert_eq!(ok_int(math::mux_math_permutations(5, 2)), 20);
    let factorial_error = math::mux_math_factorial(-1);
    assert!(!unsafe { mux_runtime::result::mux_result_is_ok(factorial_error) });
    let error = unsafe { mux_result_data(factorial_error) };
    let kind = unsafe { mux_math_error_kind(error) };
    assert!(
        matches!(unsafe { &*kind }, Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes())
    );
    assert!(unsafe { mux_rc_dec(kind) });
    assert!(direct_string(unsafe { mux_math_error_detail(error) }).contains("negative"));
    assert!(unsafe { mux_rc_dec(error) });
    assert!(unsafe { mux_rc_dec(factorial_error) });
    assert_err(math::mux_math_inverse_lerp(1.0, 1.0, 0.0));
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

#[test]
fn extern_helpers() {
    assert_eq!(math::mux_int_pow(2, 10), 1024);
    approx(math::mux_math_pi(), std::f64::consts::PI);
    approx(math::mux_math_e(), std::f64::consts::E);
}
