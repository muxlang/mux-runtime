//! Unit tests for the C-ABI collection layer (list/map/set/tuple), covering
//! both the `*mut List/Map/Set` (Box) entry points and the `*mut Value` ones.
//!
//! Ownership rules followed here:
//!   - `*mut List/Map/Set` from `mux_new_*`/`*_concat`/`*_merge`/`*_union` -> `mux_free_*`
//!   - `*mut Value` from rc constructors/getters -> `mux_rc_dec`
//!   - `*mut c_char` from `*_to_string` -> `mux_free_string`
//!   - `mux_*_value` consumes its Box pointer (do not free it again)
//!   - `mux_value_get_tuple` borrows into the Value (do not free the result)
#![allow(clippy::mutable_key_type)]

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use mux_runtime::list::*;
use mux_runtime::map::*;
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::set::*;
use mux_runtime::std::{
    mux_free_list, mux_free_map, mux_free_set, mux_free_string, mux_int_value, mux_list_value,
    mux_new_list, mux_new_map, mux_new_set, mux_string_value,
};
use mux_runtime::tuple::*;
use mux_runtime::Value;

fn int(n: i64) -> *mut mux_runtime::Value {
    mux_int_value(n)
}

fn read_cstr(p: *mut c_char) -> String {
    assert!(!p.is_null());
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { mux_free_string(p) };
    s
}

#[test]
fn container_value_constructors_transfer_owned_boxes() {
    let list = unsafe { mux_list_value(mux_new_list()) };
    assert_eq!(unsafe { &*list }, &Value::List(vec![]));
    assert!(unsafe { mux_rc_dec(list) });

    let map = unsafe { mux_map_value(mux_new_map()) };
    assert!(matches!(unsafe { &*map }, Value::Map(value) if value.is_empty()));
    assert!(unsafe { mux_rc_dec(map) });

    let set = unsafe { mux_set_value(mux_new_set()) };
    assert!(matches!(unsafe { &*set }, Value::Set(value) if value.is_empty()));
    assert!(unsafe { mux_rc_dec(set) });
}

// --- List (Box layer) --------------------------------------------------------

#[test]
fn list_box_layer() {
    let list = mux_new_list();
    let a = int(10);
    let b = int(20);

    unsafe { mux_list_push_back(list, a) };
    unsafe { mux_list_push(list, b) };
    assert_eq!(unsafe { mux_list_length(list) }, 2);
    assert!(!unsafe { mux_list_is_empty(list) });
    assert!(unsafe { mux_list_contains(list, a) });

    let got = unsafe { mux_list_get_value(list, 0) };
    assert!(!got.is_null());
    assert!(unsafe { mux_rc_dec(got) });

    let opt = unsafe { mux_list_get(list, 0) }; // Optional(Some)
    assert!(unsafe { mux_rc_dec(opt) });

    let c = int(99);
    unsafe { mux_list_set(list, 0, c) };
    unsafe { mux_list_insert(list, 0, c) };
    assert_eq!(unsafe { mux_list_length(list) }, 3);

    let popped = unsafe { mux_list_pop_back(list) }; // Optional(Some(20))
    assert_eq!(
        unsafe { &*popped },
        &Value::Optional(Some(Box::new(Value::Int(20))))
    );
    assert!(unsafe { mux_rc_dec(popped) });

    assert_eq!(read_cstr(unsafe { mux_list_to_string(list) }), "[99, 99]");

    let other = mux_new_list();
    unsafe { mux_list_push_back(other, a) };
    let joined = unsafe { mux_list_concat(list, other) };
    assert!(!joined.is_null());
    unsafe { mux_free_list(joined) };

    unsafe { mux_free_list(list) };
    unsafe { mux_free_list(other) };
    assert!(unsafe { mux_rc_dec(a) });
    assert!(unsafe { mux_rc_dec(b) });
    assert!(unsafe { mux_rc_dec(c) });
}

// --- List (Value layer) ------------------------------------------------------

#[test]
fn list_box_pop_front_and_empty() {
    let list = mux_new_list();
    let a = int(1);
    let b = int(2);
    unsafe { mux_list_push_back(list, a) };
    unsafe { mux_list_push_back(list, b) };

    let front = unsafe { mux_list_pop(list) }; // Optional(Some) from front
    assert!(unsafe { mux_rc_dec(front) });

    // get out of range yields Optional(None)
    let oob = unsafe { mux_list_get(list, 50) };
    assert!(unsafe { mux_rc_dec(oob) });

    // drain then pop empty -> Optional(None)
    let drained = unsafe { mux_list_pop(list) }; // Optional(Some) - owned, must be released
    assert!(unsafe { mux_rc_dec(drained) });
    let empty = unsafe { mux_list_pop(list) };
    assert!(unsafe { mux_rc_dec(empty) });
    let empty_back = unsafe { mux_list_pop_back(list) };
    assert_eq!(unsafe { &*empty_back }, &Value::Optional(None));
    assert!(unsafe { mux_rc_dec(empty_back) });
    assert!(unsafe { mux_list_is_empty(list) });

    unsafe { mux_free_list(list) };
    assert!(unsafe { mux_rc_dec(a) });
    assert!(unsafe { mux_rc_dec(b) });
}

// --- Map ---------------------------------------------------------------------

#[test]
fn map_box_layer() {
    let map = mux_new_map();
    let k = unsafe { mux_string_value(CString::new("k").unwrap().as_ptr()) };
    let v = int(42);

    unsafe { mux_map_put(map, k, v) };
    assert!(unsafe { mux_map_contains(map, k) });
    assert_eq!(unsafe { mux_map_size(map) }, 1);
    assert!(!unsafe { mux_map_is_empty(map) });

    let got = unsafe { mux_map_get(map, k) }; // Optional(Some)
    assert_eq!(
        unsafe { &*got },
        &Value::Optional(Some(Box::new(Value::Int(42))))
    );
    assert!(unsafe { mux_rc_dec(got) });

    let missing_key = CString::new("missing").unwrap();
    let missing_key = unsafe { mux_string_value(missing_key.as_ptr()) };
    assert!(!unsafe { mux_map_contains(map, missing_key) });
    let missing = unsafe { mux_map_get(map, missing_key) };
    assert_eq!(unsafe { &*missing }, &Value::Optional(None));
    assert!(unsafe { mux_rc_dec(missing) });
    assert!(unsafe { mux_rc_dec(missing_key) });

    let keys = unsafe { mux_map_keys(map) };
    let values = unsafe { mux_map_values(map) };
    let pairs = unsafe { mux_map_pairs(map) };
    assert!(unsafe { mux_rc_dec(keys) });
    assert!(unsafe { mux_rc_dec(values) });
    assert!(unsafe { mux_rc_dec(pairs) });

    assert!(!read_cstr(unsafe { mux_map_to_string(map) }).is_empty());

    let removed = unsafe { mux_map_remove(map, k) }; // Optional(Some)
    assert_eq!(
        unsafe { &*removed },
        &Value::Optional(Some(Box::new(Value::Int(42))))
    );
    assert!(unsafe { mux_rc_dec(removed) });
    assert!(unsafe { mux_map_is_empty(map) });

    let other = mux_new_map();
    let merged = unsafe { mux_map_merge(map, other) };
    assert!(!merged.is_null());
    unsafe { mux_free_map(merged) };

    unsafe { mux_free_map(map) };
    unsafe { mux_free_map(other) };
    assert!(unsafe { mux_rc_dec(k) });
    assert!(unsafe { mux_rc_dec(v) });
}

// --- Set ---------------------------------------------------------------------

#[test]
fn set_box_layer() {
    let set = mux_new_set();
    let v = int(5);

    unsafe { mux_set_add(set, v) };
    unsafe { mux_set_add(set, v) };
    assert!(unsafe { mux_set_contains(set, v) });
    assert_eq!(
        unsafe { mux_set_size(set) },
        1,
        "duplicate insertions are ignored"
    );
    assert!(!unsafe { mux_set_is_empty(set) });

    let as_list = unsafe { mux_set_to_list(set) };
    assert!(unsafe { mux_rc_dec(as_list) });
    assert!(!read_cstr(unsafe { mux_set_to_string(set) }).is_empty());

    assert!(unsafe { mux_set_remove(set, v) });
    assert!(!unsafe { mux_set_remove(set, v) });

    let other = mux_new_set();
    let unioned = unsafe { mux_set_union(set, other) };
    assert!(!unioned.is_null());
    unsafe { mux_free_set(unioned) };

    unsafe { mux_free_set(set) };
    unsafe { mux_free_set(other) };
    assert!(unsafe { mux_rc_dec(v) });
}

#[test]
fn list_slice_bounds() {
    let xs = Value::List(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(3),
        Value::Int(4),
        Value::Int(5),
    ]);

    let check = |start: i64, end: i64, want: Vec<i64>| {
        let got = unsafe { mux_list_slice_value(&raw const xs, start, end) };
        let expected = Value::List(want.into_iter().map(Value::Int).collect());
        assert_eq!(
            unsafe { &*got },
            &expected,
            "slice [{start}:{end}] did not match"
        );
        assert!(unsafe { mux_rc_dec(got) });
    };

    check(1, 3, vec![2, 3]);
    check(0, 5, vec![1, 2, 3, 4, 5]);
    check(2, 5, vec![3, 4, 5]);
    check(-2, 5, vec![4, 5]);
    check(0, -1, vec![1, 2, 3, 4]);
    check(2, 99, vec![3, 4, 5]);
    check(-99, 2, vec![1, 2]);
    check(4, 2, vec![]);
    check(99, 100, vec![]);

    let not_a_list = Value::Int(7);
    let got = unsafe { mux_list_slice_value(&raw const not_a_list, 0, 1) };
    assert_eq!(unsafe { &*got }, &Value::List(vec![]));
    assert!(unsafe { mux_rc_dec(got) });
}

// --- Tuple -------------------------------------------------------------------

#[test]
fn tuple_ops() {
    let l = int(1);
    let r = int(2);

    let t1 = unsafe { mux_new_tuple(l, r) };
    let t2 = unsafe { mux_new_tuple(l, r) };
    assert!(unsafe { mux_tuple_eq(t1, t2) });

    let left = unsafe { mux_tuple_left(t1) };
    let right = unsafe { mux_tuple_right(t1) };
    assert!(unsafe { mux_rc_dec(left) });
    assert!(unsafe { mux_rc_dec(right) });
    assert_eq!(read_cstr(unsafe { mux_tuple_to_string(t1) }), "(1, 2)");

    // mux_tuple_value consumes the Box; the borrowed get_tuple result must NOT be freed.
    let tv = unsafe { mux_tuple_value(t1) };
    let borrowed = unsafe { mux_value_get_tuple(tv) };
    assert!(!borrowed.is_null());
    assert!(unsafe { mux_rc_dec(tv) });

    let tv2 = unsafe { mux_tuple_value(t2) };
    assert!(unsafe { mux_rc_dec(tv2) });

    assert!(unsafe { mux_rc_dec(l) });
    assert!(unsafe { mux_rc_dec(r) });
}

#[test]
fn tuple_null_queries_preserve_false_or_null_results() {
    assert!(!unsafe { mux_tuple_eq(std::ptr::null_mut(), std::ptr::null_mut()) });
    assert!(unsafe { mux_value_get_tuple(std::ptr::null_mut()) }.is_null());
}
