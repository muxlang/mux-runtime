//! Unit tests for the std-layer C-ABI (value constructors, getters, range,
//! container allocation/free, value addition and stringification).
//!
//! Pointer ownership follows the runtime's conventions:
//!   - `*mut Value` from rc-based constructors is freed with `mux_rc_dec`.
//!   - `*mut List/Map/Set` from `Box::into_raw` is freed with `mux_free_*`.

use std::ffi::{CStr, CString};

use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::std::*;

fn read_and_free_cstr(p: *mut std::os::raw::c_char) -> String {
    assert!(!p.is_null());
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    mux_free_string(p);
    s
}

#[test]
fn int_and_bool_value_roundtrip() {
    let i = mux_int_value(42);
    assert_eq!(mux_value_get_int(i), 42);
    assert_eq!(mux_value_get_type_tag(i), 1);
    assert!(mux_rc_dec(i));

    let b = mux_bool_value(1);
    assert_eq!(mux_value_get_bool(b), 1);
    assert_eq!(mux_value_get_type_tag(b), 0);
    assert!(mux_rc_dec(b));
}

#[test]
fn value_add_numbers_and_strings() {
    let a = mux_int_value(2);
    let b = mux_int_value(3);
    let sum = mux_value_add(a, b);
    assert_eq!(mux_value_get_int(sum), 5);
    assert!(mux_rc_dec(sum));
    assert!(mux_rc_dec(a));
    assert!(mux_rc_dec(b));

    let foo = CString::new("foo").unwrap();
    let bar = CString::new("bar").unwrap();
    let sa = mux_string_value(foo.as_ptr());
    let sb = mux_string_value(bar.as_ptr());
    let cat = mux_value_add(sa, sb);
    assert_eq!(read_and_free_cstr(mux_value_to_string(cat)), "foobar");
    assert!(mux_rc_dec(cat));
    assert!(mux_rc_dec(sa));
    assert!(mux_rc_dec(sb));
}

#[test]
fn value_equality() {
    let a = mux_int_value(1);
    let b = mux_int_value(1);
    let c = mux_int_value(2);
    assert_eq!(mux_value_equal(a, b), 1);
    assert_eq!(mux_value_equal(a, c), 0);
    assert_eq!(mux_value_not_equal(a, c), 1);
    assert!(mux_rc_dec(a));
    assert!(mux_rc_dec(b));
    assert!(mux_rc_dec(c));
}

#[test]
fn range_into_list_value() {
    let list_ptr = mux_range(1, 4); // *mut List [1, 2, 3]
    let list_val = mux_list_value(list_ptr); // consumes list_ptr, returns rc Value
    assert_eq!(mux_value_list_length(list_val), 3);

    let first = mux_value_list_get_value(list_val, 0);
    assert_eq!(mux_value_get_int(first), 1);
    assert!(mux_rc_dec(first));

    assert!(mux_rc_dec(list_val));
}

#[test]
fn container_alloc_and_free() {
    mux_free_list(mux_new_list());
    mux_free_map(mux_new_map());
    mux_free_set(mux_new_set());
}

#[test]
fn value_get_list_from_value() {
    let list_val = mux_list_value(mux_range(0, 2));
    let raw = mux_value_get_list(list_val); // *mut List clone
    assert!(!raw.is_null());
    mux_free_list(raw);
    assert!(mux_rc_dec(list_val));
}

#[test]
fn value_to_string_scalar() {
    let i = mux_int_value(99);
    assert_eq!(read_and_free_cstr(mux_value_to_string(i)), "99");
    assert!(mux_rc_dec(i));
}
