//! Unit tests for the primitive value types (int, float, bool, string).
//!
//! These exercise the pure-Rust cores directly plus the scalar C-ABI wrappers
//! that take and return plain numbers, so no unsafe pointer handling is needed.

use mux_runtime::bool::Bool;
use mux_runtime::float::Float;
use mux_runtime::int::Int;
use mux_runtime::string::MuxString;
use ordered_float::OrderedFloat;

fn f(x: f64) -> Float {
    Float(OrderedFloat(x))
}

// --- Int ---------------------------------------------------------------------

#[test]
fn int_arithmetic() {
    assert_eq!(Int(6).add(&Int(4)).0, 10);
    assert_eq!(Int(6).sub(&Int(4)).0, 2);
    assert_eq!(Int(6).mul(&Int(4)).0, 24);
    assert_eq!(Int(7).div(&Int(2)).unwrap().0, 3);
    assert_eq!(Int(7).rem(&Int(2)).unwrap().0, 1);
}

#[test]
fn int_div_and_rem_by_zero() {
    assert!(Int(1).div(&Int(0)).is_err());
    assert!(Int(1).rem(&Int(0)).is_err());
}

#[test]
fn int_compare_and_convert() {
    assert!(Int(1).lt(&Int(2)));
    assert!(!Int(2).lt(&Int(1)));
    assert_eq!(Int(3), Int(3));
    assert_eq!(Int(3).to_float(), 3.0);
    assert_eq!(format!("{}", Int(-5)), "-5");
}

#[test]
fn int_extern_scalars() {
    use mux_runtime::int::*;
    assert_eq!(mux_int_add(2, 3), 5);
    assert_eq!(mux_int_sub(2, 3), -1);
    assert_eq!(mux_int_mul(2, 3), 6);
    assert_eq!(mux_int_rem(7, 3), 1);
    assert_eq!(mux_int_rem(7, 0), 0); // safe fallback
    assert!(mux_int_eq(4, 4));
    assert!(mux_int_lt(1, 2));
}

// --- Float -------------------------------------------------------------------

#[test]
fn float_arithmetic_and_rounding() {
    assert_eq!(f(1.5).add(&f(2.0)).0, OrderedFloat(3.5));
    assert_eq!(f(5.0).sub(&f(2.0)).0, OrderedFloat(3.0));
    assert_eq!(f(2.0).mul(&f(3.0)).0, OrderedFloat(6.0));
    assert_eq!(f(6.0).div(&f(2.0)).unwrap().0, OrderedFloat(3.0));
    assert!(f(1.0).div(&f(0.0)).is_err());
    assert_eq!(f(-2.5).abs().0, OrderedFloat(2.5));
    assert_eq!(f(1.4).round().0, OrderedFloat(1.0));
    assert_eq!(f(1.7).floor().0, OrderedFloat(1.0));
    assert_eq!(f(1.2).ceil().0, OrderedFloat(2.0));
}

#[test]
fn float_compare_and_convert() {
    assert!(f(1.0).lt(&f(2.0)));
    assert_eq!(f(3.9).to_int(), 3);
    assert_eq!(format!("{}", f(2.5)), "2.5");
}

#[test]
fn float_extern_scalars() {
    use mux_runtime::float::*;
    assert_eq!(mux_float_add(1.5, 2.5), 4.0);
    assert_eq!(mux_float_sub(5.0, 1.0), 4.0);
    assert_eq!(mux_float_mul(2.0, 4.0), 8.0);
    assert!(mux_float_eq(1.0, 1.0));
    assert!(mux_float_lt(1.0, 2.0));
}

// --- Bool --------------------------------------------------------------------

#[test]
fn bool_convert_and_display() {
    assert_eq!(Bool(true).to_int(), 1);
    assert_eq!(Bool(false).to_int(), 0);
    assert_eq!(format!("{}", Bool(true)), "true");
    assert_eq!(format!("{}", Bool(false)), "false");
}

// --- String ------------------------------------------------------------------

#[test]
fn string_parse() {
    assert_eq!(MuxString("42".to_string()).to_int().unwrap(), 42);
    assert!(MuxString("nope".to_string()).to_int().is_err());
    assert_eq!(MuxString("3.5".to_string()).to_float().unwrap(), 3.5);
    assert!(MuxString("nope".to_string()).to_float().is_err());
}

#[test]
fn string_ops() {
    let joined = MuxString("foo".to_string()).concat(&MuxString("bar".to_string()));
    assert_eq!(joined.0, "foobar");
    assert_eq!(MuxString("hello".to_string()).length(), 5);
    assert_eq!(format!("{}", MuxString("hi".to_string())), "hi");
    // hash is stable for equal inputs.
    let a = MuxString("same".to_string()).hash();
    let b = MuxString("same".to_string()).hash();
    assert_eq!(a, b);
}
