//! Unit tests for the math module (pure floating-point helpers).

use mux_runtime::math;

fn approx(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "expected {a} ~= {b}");
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
}

#[test]
fn extern_helpers() {
    assert_eq!(math::mux_int_pow(2, 10), 1024);
    approx(math::mux_math_pi(), std::f64::consts::PI);
    approx(math::mux_math_e(), std::f64::consts::E);
}
