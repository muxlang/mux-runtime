//! Copy-on-write correctness for the `*_value` collection mutators.
//!
//! These FFI helpers mutate the container backing a ref-counted `Value` in place
//! when it is uniquely owned (refcount == 1) and fall back to clone-then-write
//! when it is shared. Both paths must produce identical, correct contents; see
//! muxlang/mux-runtime#15.
#![allow(clippy::mutable_key_type)] // Value keys are logically immutable here.

use mux_runtime::list::{
    mux_list_pop_back_value, mux_list_pop_value, mux_list_push_back_value, mux_list_push_value,
    mux_list_set_value,
};
use mux_runtime::map::{mux_map_put_value, mux_map_remove_value};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_count, mux_rc_dec, mux_rc_inc};
use mux_runtime::set::{mux_set_add_value, mux_set_remove_value};
use mux_runtime::Value;

/// Allocate a ref-counted `Value` scalar and free it after use in `f`.
fn with_scalar(value: Value, f: impl FnOnce(*mut Value)) {
    let ptr = mux_rc_alloc(value);
    f(ptr);
    mux_rc_dec(ptr);
}

/// Push integers 0..n into a fresh (uniquely-owned) list value and return it.
fn build_list(n: i64) -> *mut Value {
    let list_val = mux_rc_alloc(Value::List(Vec::new()));
    assert_eq!(
        mux_rc_count(list_val),
        1,
        "fresh value must be uniquely owned"
    );
    for i in 0..n {
        with_scalar(Value::Int(i), |elem| {
            mux_list_push_back_value(list_val, elem)
        });
    }
    list_val
}

fn list_contents(list_val: *const Value) -> Vec<Value> {
    match unsafe { &*list_val } {
        Value::List(v) => v.clone(),
        other => panic!("expected list, got {other:?}"),
    }
}

// --- List --------------------------------------------------------------------

#[test]
fn list_push_back_builds_in_order_when_uniquely_owned() {
    let list_val = build_list(1000);
    let contents = list_contents(list_val);
    assert_eq!(contents.len(), 1000);
    assert_eq!(contents[0], Value::Int(0));
    assert_eq!(contents[999], Value::Int(999));
    mux_rc_dec(list_val);
}

#[test]
fn list_push_front_prepends() {
    let list_val = mux_rc_alloc(Value::List(Vec::new()));
    for i in 0..5 {
        with_scalar(Value::Int(i), |elem| mux_list_push_value(list_val, elem));
    }
    // Front insertion reverses the order.
    let contents = list_contents(list_val);
    assert_eq!(
        contents,
        vec![
            Value::Int(4),
            Value::Int(3),
            Value::Int(2),
            Value::Int(1),
            Value::Int(0),
        ]
    );
    mux_rc_dec(list_val);
}

#[test]
fn list_set_value_overwrites_and_extends() {
    let list_val = build_list(3);
    with_scalar(Value::Int(42), |elem| mux_list_set_value(list_val, 1, elem));
    // Negative index writes from the end.
    with_scalar(Value::Int(99), |elem| {
        mux_list_set_value(list_val, -1, elem)
    });
    // Index past the end extends with default fill (Int(0)) then writes.
    with_scalar(Value::Int(7), |elem| mux_list_set_value(list_val, 5, elem));
    let contents = list_contents(list_val);
    assert_eq!(
        contents,
        vec![
            Value::Int(0),
            Value::Int(42),
            Value::Int(99),
            Value::Int(0),
            Value::Int(0),
            Value::Int(7),
        ]
    );
    mux_rc_dec(list_val);
}

#[test]
fn list_pop_back_and_front_return_ends() {
    let list_val = build_list(3); // [0, 1, 2]
    let back = mux_list_pop_back_value(list_val);
    assert_eq!(
        unsafe { &*back },
        &Value::Optional(Some(Box::new(Value::Int(2))))
    );
    mux_rc_dec(back);
    let front = mux_list_pop_value(list_val);
    assert_eq!(
        unsafe { &*front },
        &Value::Optional(Some(Box::new(Value::Int(0))))
    );
    mux_rc_dec(front);
    assert_eq!(list_contents(list_val), vec![Value::Int(1)]);
    mux_rc_dec(list_val);
}

#[test]
fn list_pop_on_empty_returns_none() {
    let list_val = mux_rc_alloc(Value::List(Vec::new()));
    let popped = mux_list_pop_back_value(list_val);
    assert_eq!(unsafe { &*popped }, &Value::Optional(None));
    mux_rc_dec(popped);
    mux_rc_dec(list_val);
}

#[test]
fn list_mutation_on_shared_value_matches_unique_path() {
    // Force the clone-on-shared branch by bumping the refcount above 1, then
    // confirm the result is identical to the uniquely-owned fast path.
    let shared = mux_rc_alloc(Value::List(vec![Value::Int(1), Value::Int(2)]));
    mux_rc_inc(shared);
    assert_eq!(
        mux_rc_count(shared),
        2,
        "value must be shared for this path"
    );
    with_scalar(Value::Int(3), |elem| mux_list_push_back_value(shared, elem));
    assert_eq!(
        list_contents(shared),
        vec![Value::Int(1), Value::Int(2), Value::Int(3)]
    );
    // Drop the two references we hold.
    mux_rc_dec(shared);
    mux_rc_dec(shared);
}

// --- Map ---------------------------------------------------------------------

#[test]
fn map_put_and_remove_uniquely_owned() {
    let map_val = mux_rc_alloc(Value::Map(std::collections::BTreeMap::new()));
    for i in 0..500 {
        with_scalar(Value::Int(i), |k| {
            with_scalar(Value::Int(i * 10), |v| mux_map_put_value(map_val, k, v));
        });
    }
    let Value::Map(m) = (unsafe { &*map_val }) else {
        panic!("expected map");
    };
    assert_eq!(m.len(), 500);
    assert_eq!(m.get(&Value::Int(499)), Some(&Value::Int(4990)));

    // Remove returns the previous value wrapped in an optional.
    let removed = with_scalar_ret(Value::Int(499), |k| mux_map_remove_value(map_val, k));
    assert_eq!(
        unsafe { &*removed },
        &Value::Optional(Some(Box::new(Value::Int(4990))))
    );
    mux_rc_dec(removed);
    let Value::Map(m) = (unsafe { &*map_val }) else {
        panic!("expected map");
    };
    assert_eq!(m.len(), 499);
    mux_rc_dec(map_val);
}

#[test]
fn map_put_on_shared_value_matches_unique_path() {
    let shared = mux_rc_alloc(Value::Map(std::collections::BTreeMap::new()));
    mux_rc_inc(shared);
    with_scalar(Value::Int(1), |k| {
        with_scalar(Value::Int(2), |v| mux_map_put_value(shared, k, v));
    });
    let Value::Map(m) = (unsafe { &*shared }) else {
        panic!("expected map");
    };
    assert_eq!(m.get(&Value::Int(1)), Some(&Value::Int(2)));
    mux_rc_dec(shared);
    mux_rc_dec(shared);
}

// --- Set ---------------------------------------------------------------------

#[test]
fn set_add_and_remove_uniquely_owned() {
    let set_val = mux_rc_alloc(Value::Set(std::collections::BTreeSet::new()));
    for i in 0..500 {
        with_scalar(Value::Int(i % 250), |v| mux_set_add_value(set_val, v));
    }
    let Value::Set(s) = (unsafe { &*set_val }) else {
        panic!("expected set");
    };
    assert_eq!(s.len(), 250, "duplicates must be de-duplicated");

    let removed = with_scalar_ret_bool(Value::Int(0), |v| mux_set_remove_value(set_val, v));
    assert!(removed);
    let missing = with_scalar_ret_bool(Value::Int(9999), |v| mux_set_remove_value(set_val, v));
    assert!(!missing);
    mux_rc_dec(set_val);
}

#[test]
fn set_add_on_shared_value_matches_unique_path() {
    let shared = mux_rc_alloc(Value::Set(std::collections::BTreeSet::new()));
    mux_rc_inc(shared);
    with_scalar(Value::Int(7), |v| mux_set_add_value(shared, v));
    let Value::Set(s) = (unsafe { &*shared }) else {
        panic!("expected set");
    };
    assert!(s.contains(&Value::Int(7)));
    mux_rc_dec(shared);
    mux_rc_dec(shared);
}

// --- helpers that return a value from the scalar-scoped closure ---------------

fn with_scalar_ret<R>(value: Value, f: impl FnOnce(*mut Value) -> R) -> R {
    let ptr = mux_rc_alloc(value);
    let r = f(ptr);
    mux_rc_dec(ptr);
    r
}

fn with_scalar_ret_bool(value: Value, f: impl FnOnce(*mut Value) -> bool) -> bool {
    with_scalar_ret(value, f)
}
