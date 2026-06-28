//! Unit tests for the assert helpers. Failures call `std::process::abort()`
//! (not a catchable panic), so only the passing paths are exercised here.

use std::ffi::CString;

use mux_runtime::assert::*;
use mux_runtime::optional::{mux_optional_none, mux_optional_some_int};
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::{mux_result_ok_int, mux_value_result_discriminant};
use mux_runtime::std::{mux_err, mux_int_value};

#[test]
fn boolean_asserts_pass() {
    mux_assert_true(1);
    mux_assert_false(0);
    mux_assert_assert(1, std::ptr::null());
    let msg = CString::new("ok").unwrap();
    mux_assert_assert(1, msg.as_ptr());
}

#[test]
fn eq_ne_asserts_pass() {
    let a = mux_int_value(5);
    let b = mux_int_value(5);
    let c = mux_int_value(6);
    mux_assert_eq(a, b);
    mux_assert_ne(a, c);
    assert!(mux_rc_dec(a));
    assert!(mux_rc_dec(b));
    assert!(mux_rc_dec(c));
}

#[test]
fn optional_asserts_pass() {
    let some = mux_optional_some_int(1);
    let none = mux_optional_none();
    mux_assert_some(some);
    mux_assert_none(none);
    assert!(mux_rc_dec(some));
    assert!(mux_rc_dec(none));
}

#[test]
fn result_asserts_pass() {
    let ok = mux_result_ok_int(1);
    let err = mux_err(CString::new("boom").unwrap().as_ptr());
    assert_eq!(mux_value_result_discriminant(err), 1);
    mux_assert_ok(ok);
    mux_assert_err(err);
    assert!(mux_rc_dec(ok));
    assert!(mux_rc_dec(err));
}
