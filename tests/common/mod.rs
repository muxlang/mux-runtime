//! Shared helpers for runtime integration tests.
//!
//! Cargo compiles this module into each test crate that declares `mod common;`,
//! so not every helper is used everywhere; silence the resulting dead-code lint.
#![allow(dead_code)]

use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::{mux_result_data, mux_result_is_err, mux_result_is_ok};
use mux_runtime::Value;

/// Assert a `Result` value is Ok and free it.
pub fn assert_ok(r: *mut Value) {
    assert!(mux_result_is_ok(r), "expected Ok result");
    assert!(mux_rc_dec(r));
}

/// Assert a `Result` value is Err and free it.
pub fn assert_err(r: *mut Value) {
    assert!(mux_result_is_err(r), "expected Err result");
    assert!(mux_rc_dec(r));
}

/// Extract the inner `i64` from a `Result(Ok(Int))`, freeing everything.
pub fn ok_int(r: *mut Value) -> i64 {
    assert!(mux_result_is_ok(r), "expected Ok result");
    let data = mux_result_data(r);
    let out = unsafe {
        match &*data {
            Value::Int(i) => *i,
            other => panic!("expected Int, got {other:?}"),
        }
    };
    assert!(mux_rc_dec(data));
    assert!(mux_rc_dec(r));
    out
}

/// Extract the inner `bool` from a `Result(Ok(Bool))`, freeing everything.
pub fn ok_bool(r: *mut Value) -> bool {
    assert!(mux_result_is_ok(r), "expected Ok result");
    let data = mux_result_data(r);
    let out = unsafe {
        match &*data {
            Value::Bool(b) => *b,
            other => panic!("expected Bool, got {other:?}"),
        }
    };
    assert!(mux_rc_dec(data));
    assert!(mux_rc_dec(r));
    out
}

/// Extract the inner `String` from a `Result(Ok(String))`, freeing everything.
pub fn ok_string(r: *mut Value) -> String {
    assert!(mux_result_is_ok(r), "expected Ok result");
    let data = mux_result_data(r);
    let out = unsafe {
        match &*data {
            Value::String(s) => s.clone(),
            other => panic!("expected String, got {other:?}"),
        }
    };
    assert!(mux_rc_dec(data));
    assert!(mux_rc_dec(r));
    out
}

/// Extract the inner list length from a `Result(Ok(List))`, freeing everything.
pub fn ok_list_len(r: *mut Value) -> usize {
    assert!(mux_result_is_ok(r), "expected Ok result");
    let data = mux_result_data(r);
    let out = unsafe {
        match &*data {
            Value::List(items) => items.len(),
            other => panic!("expected List, got {other:?}"),
        }
    };
    assert!(mux_rc_dec(data));
    assert!(mux_rc_dec(r));
    out
}
