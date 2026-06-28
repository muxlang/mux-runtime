//! Unit tests for the C-ABI string/bool/boxing layer.

mod common;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use common::{assert_err, assert_ok, ok_int};
use mux_runtime::boxing::*;
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::std::{mux_free_string, mux_string_value, mux_value_get_int};
use mux_runtime::string::*;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn read_cstr(p: *mut c_char) -> String {
    assert!(!p.is_null());
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    mux_free_string(p);
    s
}

#[test]
fn string_scalar_ops() {
    assert_eq!(
        read_cstr(mux_string_concat(cs("foo").as_ptr(), cs("bar").as_ptr())),
        "foobar"
    );
    assert_eq!(mux_string_length(cs("hello").as_ptr()), 5);
    assert_eq!(
        mux_string_hash(cs("x").as_ptr()),
        mux_string_hash(cs("x").as_ptr())
    );
    assert_eq!(read_cstr(mux_string_to_string(cs("hi").as_ptr())), "hi");
}

#[test]
fn string_parsing() {
    assert_eq!(ok_int(mux_string_to_int(cs("42").as_ptr())), 42);
    assert_err(mux_string_to_int(cs("nope").as_ptr()));
    assert_ok(mux_string_to_float(cs("3.5").as_ptr()));
    assert_err(mux_string_to_float(cs("nope").as_ptr()));
}

#[test]
fn string_equality() {
    assert_eq!(mux_string_equal(cs("a").as_ptr(), cs("a").as_ptr()), 1);
    assert_eq!(mux_string_equal(cs("a").as_ptr(), cs("b").as_ptr()), 0);
    assert_eq!(mux_string_not_equal(cs("a").as_ptr(), cs("b").as_ptr()), 1);
}

#[test]
fn string_containment() {
    let hay = mux_string_value(cs("hello world").as_ptr());
    let needle = mux_string_value(cs("world").as_ptr());
    assert!(mux_string_contains(hay, needle));
    assert!(mux_string_contains_char(hay, 'o' as i64));
    assert!(!mux_string_contains_char(hay, 'z' as i64));
    assert!(mux_rc_dec(hay));
    assert!(mux_rc_dec(needle));
}

#[test]
fn char_conversions() {
    assert_eq!(ok_int(mux_string_to_char(cs("a").as_ptr())), 'a' as i64);
    assert_err(mux_string_to_char(cs("ab").as_ptr()));
    assert_eq!(ok_int(mux_char_to_int('5' as i64)), 5);
    assert_err(mux_char_to_int('a' as i64));
    assert_eq!(read_cstr(mux_char_to_string('A' as i64)), "A");
}

#[test]
fn string_from_value_roundtrip() {
    let v = mux_new_string_from_cstr(cs("data").as_ptr());
    assert!(!v.is_null());
    assert_eq!(read_cstr(unsafe { mux_string_from_value(v) }), "data");
    assert!(mux_rc_dec(v));
}

#[test]
fn bool_extern() {
    use mux_runtime::bool::*;
    use mux_runtime::std::mux_bool_value;

    assert_eq!(read_cstr(mux_bool_to_string(1)), "true");
    assert_eq!(read_cstr(mux_bool_to_string(0)), "false");

    let bv = mux_bool_value(1);
    assert_eq!(unsafe { mux_bool_from_value(bv) }, 1);
    let as_int = unsafe { mux_bool_to_int(bv) };
    assert_eq!(mux_value_get_int(as_int), 1);
    assert!(mux_rc_dec(as_int));
    let as_float = unsafe { mux_bool_to_float(bv) };
    assert!(!as_float.is_null());
    assert!(mux_rc_dec(as_float));
    assert!(mux_rc_dec(bv));
}

#[test]
fn boxing_roundtrips() {
    let i = mux_box_int(5);
    assert_eq!(mux_value_get_int(i), 5);
    assert!(mux_rc_dec(i));

    let f = mux_box_float(1.5);
    assert!(!f.is_null());
    assert!(mux_rc_dec(f));

    let b = mux_box_bool(1);
    assert!(!b.is_null());
    assert!(mux_rc_dec(b));

    let s = mux_box_str(cs("hi").as_ptr());
    assert!(!s.is_null());
    assert!(mux_rc_dec(s));
}
