//! Coverage for the remaining C-ABI surface of int/float/optional/result that
//! wasn't exercised by the pure-core tests.

mod common;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use common::{assert_err, assert_ok};
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::std::{mux_free_string, mux_int_value, mux_value_get_float, mux_value_get_int};

fn read_cstr(p: *mut c_char) -> String {
    assert!(!p.is_null());
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    mux_free_string(p);
    s
}

#[test]
fn int_extern_remainder() {
    use mux_runtime::int::*;
    assert_eq!(read_cstr(mux_int_to_string(-7)), "-7");

    let v = mux_int_value(9);
    assert_eq!(unsafe { mux_int_from_value(v) }, 9);
    assert!(mux_rc_dec(v));

    assert_ok(mux_int_div(10, 2));
    assert_err(mux_int_div(1, 0));
}

#[test]
fn float_extern_remainder() {
    use mux_runtime::float::*;
    use mux_runtime::std::mux_int_value;

    assert!(read_cstr(mux_float_to_string(2.5)).starts_with("2.5"));
    assert_ok(mux_float_div(6.0, 2.0));
    assert_err(mux_float_div(1.0, 0.0));

    // Value-based conversions
    let i = mux_int_value(4);
    let as_float = unsafe { mux_int_to_float(i) };
    assert!((mux_value_get_float(as_float) - 4.0).abs() < 1e-9);
    let back = unsafe { mux_float_to_int(as_float) };
    assert_eq!(mux_value_get_int(back), 4);
    assert_eq!(unsafe { mux_float_from_value(as_float) }, 4.0);
    assert!(mux_rc_dec(back));
    assert!(mux_rc_dec(as_float));
    assert!(mux_rc_dec(i));
}

#[test]
fn optional_extern_remainder() {
    use mux_runtime::optional::*;

    for ctor in [
        mux_optional_some_float(1.0),
        mux_optional_some_bool(1),
        mux_optional_some_char('a' as i64),
    ] {
        assert!(mux_optional_is_some(ctor));
        assert!(mux_rc_dec(ctor));
    }

    let inner = mux_int_value(3);
    let some = mux_optional_some_value(inner);
    assert!(mux_optional_is_some(some));
    let data = mux_optional_data(some);
    assert!(!data.is_null());
    assert!(mux_rc_dec(data));
    // identity pass-through
    assert_eq!(mux_optional_into_value(some), some);
    assert_eq!(read_cstr(mux_optional_to_string(some)), "Some(3)");
    assert!(mux_rc_dec(some));
    assert!(mux_rc_dec(inner));

    // some_value(null) yields None; get_value(None) yields null
    let none = mux_optional_some_value(std::ptr::null_mut());
    assert!(mux_optional_is_none(none));
    assert!(mux_optional_get_value(none).is_null());
    assert_eq!(read_cstr(mux_optional_to_string(none)), "None");
    assert!(mux_rc_dec(none));
}

#[test]
fn result_extern_remainder() {
    use mux_runtime::result::*;

    for ctor in [
        mux_result_ok_float(1.0),
        mux_result_ok_bool(1),
        mux_result_ok_char('z' as i64),
    ] {
        assert!(mux_result_is_ok(ctor));
        assert!(mux_rc_dec(ctor));
    }

    let inner = mux_int_value(5);
    let ok = mux_result_ok_value(inner);
    assert!(mux_result_is_ok(ok));
    assert!(mux_rc_dec(ok));

    let err = mux_result_err_value(inner);
    assert!(mux_result_is_err(err));
    let data = mux_result_data(err); // Err branch
    assert!(!data.is_null());
    assert!(mux_rc_dec(data));
    assert!(mux_rc_dec(err));
    assert!(mux_rc_dec(inner));

    let msg = CString::new("boom").unwrap();
    let err2 = unsafe { mux_result_err_str(msg.as_ptr()) };
    assert!(mux_result_is_err(err2));
    assert!(mux_rc_dec(err2));

    // null inputs
    assert!(mux_result_ok_value(std::ptr::null_mut()).is_null());
}
