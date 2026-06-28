//! Coverage for the remaining std-layer C-ABI: some/ok/err wrappers, container
//! extraction, list slicing, env access, enum boxing, and no-op frees.
#![allow(clippy::mutable_key_type)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;

use mux_runtime::optional::{mux_optional_is_none, mux_optional_is_some};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_is_err, mux_result_is_ok};
use mux_runtime::std::*;
use mux_runtime::Value;

#[test]
fn some_none_ok_err_wrappers() {
    let i = mux_int_value(1);
    let some = mux_some(i);
    assert!(mux_optional_is_some(some));
    assert!(mux_rc_dec(some));

    let none = mux_none();
    assert!(mux_optional_is_none(none));
    assert!(mux_rc_dec(none));

    let ok = mux_ok(i);
    assert!(mux_result_is_ok(ok));
    assert!(mux_rc_dec(ok));

    let err = mux_err(CString::new("e").unwrap().as_ptr());
    assert!(mux_result_is_err(err));
    assert!(mux_rc_dec(err));

    assert!(mux_rc_dec(i));
}

#[test]
fn container_extraction() {
    let mut m = BTreeMap::new();
    m.insert(Value::String("k".into()), Value::Int(1));
    let map_val = mux_rc_alloc(Value::Map(m));
    let raw_map = mux_value_get_map(map_val);
    assert!(!raw_map.is_null());
    mux_free_map(raw_map);
    assert!(mux_rc_dec(map_val));

    let mut s = BTreeSet::new();
    s.insert(Value::Int(7));
    let set_val = mux_rc_alloc(Value::Set(s));
    let raw_set = mux_value_get_set(set_val);
    assert!(!raw_set.is_null());
    mux_free_set(raw_set);
    assert!(mux_rc_dec(set_val));

    let list_val = mux_rc_alloc(Value::List(vec![Value::Int(1), Value::Int(2)]));
    let raw_list = mux_value_to_list(list_val);
    assert!(!raw_list.is_null());
    mux_free_list(raw_list);
    assert!(mux_rc_dec(list_val));

    // Non-matching value types extract to null.
    let not_a_map = mux_int_value(0);
    assert!(mux_value_get_map(not_a_map).is_null());
    assert!(mux_value_get_set(not_a_map).is_null());
    assert!(mux_value_to_list(not_a_map).is_null());
    assert!(mux_rc_dec(not_a_map));
}

#[test]
fn list_index_and_slice() {
    let list_val = mux_rc_alloc(Value::List(vec![
        Value::Int(10),
        Value::Int(20),
        Value::Int(30),
    ]));
    assert_eq!(mux_value_list_length(list_val), 3);

    let elem = mux_value_list_get_value(list_val, 1);
    assert_eq!(mux_value_get_int(elem), 20);
    assert!(mux_rc_dec(elem));

    let slice = mux_value_list_slice(list_val, 0, 2);
    assert_eq!(mux_value_list_length(slice), 2);
    assert!(mux_rc_dec(slice));

    // out-of-range index yields null
    assert!(mux_value_list_get_value(list_val, 99).is_null());

    assert!(mux_rc_dec(list_val));
}

#[test]
fn env_access() {
    // A variable we set is visible.
    std::env::set_var("MUX_TEST_ENV_VAR", "present");
    let got = mux_env_get(CString::new("MUX_TEST_ENV_VAR").unwrap().as_ptr());
    assert!(mux_optional_is_some(got));
    assert!(mux_rc_dec(got));

    let missing = mux_env_get(CString::new("MUX_DEFINITELY_UNSET_XYZ").unwrap().as_ptr());
    assert!(mux_optional_is_none(missing));
    assert!(mux_rc_dec(missing));

    let null_key = mux_env_get(std::ptr::null());
    assert!(mux_optional_is_none(null_key));
    assert!(mux_rc_dec(null_key));
}

#[test]
fn box_enum_and_noop_frees() {
    let mut bytes = [1u8, 2, 3, 4];
    let boxed = mux_box_enum(bytes.as_mut_ptr(), bytes.len());
    assert_eq!(mux_value_get_type_tag(boxed), 12); // Opaque
    assert!(mux_rc_dec(boxed));

    // no-op frees must be safe to call
    mux_free_optional(std::ptr::null_mut());
    mux_free_result(std::ptr::null_mut());
}
