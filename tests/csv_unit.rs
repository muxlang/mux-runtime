//! Unit tests for the CSV layer (feature-gated behind `csv`).
#![cfg(feature = "csv")]
#![allow(clippy::mutable_key_type)]

use std::collections::BTreeMap;
use std::ffi::CString;

use mux_runtime::data::{mux_csv_parse, mux_csv_parse_with_headers, mux_csv_to_string};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_is_err, mux_result_is_ok};
use mux_runtime::Value;

#[test]
fn parse_plain_csv() {
    let input = CString::new("a,b\n1,2\n").unwrap();
    let res = mux_csv_parse(input.as_ptr());
    assert!(mux_result_is_ok(res));
    assert!(mux_rc_dec(res));
}

#[test]
fn parse_with_headers() {
    let input = CString::new("name,age\nAlice,30\nBob,25\n").unwrap();
    let res = mux_csv_parse_with_headers(input.as_ptr());
    assert!(mux_result_is_ok(res));
    assert!(mux_rc_dec(res));
}

#[test]
fn parse_null_is_error() {
    let res = mux_csv_parse(std::ptr::null());
    assert!(mux_result_is_err(res));
    assert!(mux_rc_dec(res));
}

#[test]
fn to_string_roundtrip() {
    // Build the {headers, rows} map shape the writer expects.
    let mut map = BTreeMap::new();
    map.insert(
        Value::String("headers".to_string()),
        Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]),
    );
    map.insert(
        Value::String("rows".to_string()),
        Value::List(vec![Value::List(vec![
            Value::String("1".to_string()),
            Value::String("2".to_string()),
        ])]),
    );
    let csv_val = mux_rc_alloc(Value::Map(map));

    let res = mux_csv_to_string(csv_val);
    assert!(mux_result_is_ok(res));

    assert!(mux_rc_dec(res));
    assert!(mux_rc_dec(csv_val));
}

#[test]
fn to_string_rejects_non_map() {
    let bad = mux_rc_alloc(Value::Int(1));
    let res = mux_csv_to_string(bad);
    assert!(mux_result_is_err(res));
    assert!(mux_rc_dec(res));
    assert!(mux_rc_dec(bad));
}
