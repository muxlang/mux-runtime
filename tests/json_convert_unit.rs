//! Unit tests for JSON <-> Value conversion (pure helpers) and the C-ABI layer.
#![cfg(feature = "json")]
#![allow(clippy::mutable_key_type)]

mod common;

use std::collections::BTreeMap;
use std::ffi::CString;

use common::{assert_err, assert_ok};
use mux_runtime::json::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::Value;

// --- pure conversions --------------------------------------------------------

#[test]
fn value_to_json_scalars_and_collections() {
    assert_eq!(value_to_json(&Value::Unit).unwrap(), Json::Null);
    assert_eq!(value_to_json(&Value::Bool(true)).unwrap(), Json::Bool(true));
    assert_eq!(value_to_json(&Value::Int(3)).unwrap(), Json::Number(3.0));
    assert_eq!(
        value_to_json(&Value::Float(ordered_float::OrderedFloat(1.5))).unwrap(),
        Json::Number(1.5)
    );
    assert_eq!(
        value_to_json(&Value::String("s".into())).unwrap(),
        Json::String("s".into())
    );
    let list = Value::List(vec![Value::Int(1), Value::Int(2)]);
    assert!(matches!(value_to_json(&list).unwrap(), Json::Array(a) if a.len() == 2));
}

#[test]
fn value_to_json_error_cases() {
    assert!(value_to_json(&Value::Float(ordered_float::OrderedFloat(f64::NAN))).is_err());
    assert!(value_to_json(&Value::Float(ordered_float::OrderedFloat(f64::INFINITY))).is_err());

    let mut bad_key = BTreeMap::new();
    bad_key.insert(Value::Int(1), Value::Int(2)); // non-string key
    assert!(value_to_json(&Value::Map(bad_key)).is_err());

    // Set is not representable as JSON.
    let set = Value::Set(std::collections::BTreeSet::new());
    assert!(value_to_json(&set).is_err());
}

#[test]
fn json_to_value_roundtrip() {
    let j = Json::parse(r#"{"a": [1, true, null], "b": "x"}"#).unwrap();
    let v = json_to_value(&j);
    assert!(matches!(v, Value::Map(_)));
    // numbers become floats
    assert_eq!(
        json_to_value(&Json::Number(2.0)),
        Value::Float(ordered_float::OrderedFloat(2.0))
    );
    assert_eq!(json_to_value(&Json::Null), Value::Unit);
}

// --- C-ABI -------------------------------------------------------------------

#[test]
fn json_parse_extern() {
    assert_ok(mux_json_parse(CString::new("[1, 2, 3]").unwrap().as_ptr()));
    assert_err(mux_json_parse(CString::new("{bad").unwrap().as_ptr()));
    assert_err(mux_json_parse(std::ptr::null()));
}

#[test]
fn json_stringify_extern() {
    let v = mux_rc_alloc(Value::List(vec![Value::Int(1), Value::Int(2)]));

    // no indent (null option)
    assert_ok(mux_json_stringify(v, std::ptr::null_mut()));

    // with indent via Optional(Some(Int))
    let indent = mux_rc_alloc(Value::Optional(Some(Box::new(Value::Int(2)))));
    assert_ok(mux_json_stringify(v, indent));
    assert!(mux_rc_dec(indent));

    // null value -> error
    assert_err(mux_json_stringify(std::ptr::null(), std::ptr::null_mut()));

    assert!(mux_rc_dec(v));
}

#[test]
fn json_from_and_to_map_extern() {
    let mut map = BTreeMap::new();
    map.insert(Value::String("k".into()), Value::Int(1));
    let map_val = mux_rc_alloc(Value::Map(map));

    assert_ok(mux_json_from_map(map_val));
    assert_ok(mux_json_to_map(map_val));
    assert!(mux_rc_dec(map_val));

    // non-map inputs are errors
    let int_val = mux_rc_alloc(Value::Int(1));
    assert_err(mux_json_from_map(int_val));
    assert_err(mux_json_to_map(int_val));
    assert!(mux_rc_dec(int_val));
}
