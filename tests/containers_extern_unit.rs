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

fn int(n: i64) -> *mut mux_runtime::Value {
    mux_int_value(n)
}

fn read_cstr(p: *mut c_char) -> String {
    assert!(!p.is_null());
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    mux_free_string(p);
    s
}

// --- List (Box layer) --------------------------------------------------------

#[test]
fn list_box_layer() {
    let list = mux_new_list();
    let a = int(10);
    let b = int(20);

    mux_list_push_back(list, a);
    mux_list_push(list, b);
    assert_eq!(unsafe { mux_list_length(list) }, 2);
    assert!(!unsafe { mux_list_is_empty(list) });
    assert!(mux_list_contains(list, a));

    let got = mux_list_get_value(list, 0);
    assert!(!got.is_null());
    assert!(mux_rc_dec(got));

    let opt = mux_list_get(list, 0); // Optional(Some)
    assert!(mux_rc_dec(opt));

    let c = int(99);
    mux_list_set(list, 0, c);
    mux_list_insert(list, 0, c);
    assert_eq!(unsafe { mux_list_length(list) }, 3);

    let popped = mux_list_pop_back(list); // Optional
    assert!(mux_rc_dec(popped));

    assert_eq!(read_cstr(mux_list_to_string(list)), "[99, 99]");

    let other = mux_new_list();
    mux_list_push_back(other, a);
    let joined = mux_list_concat(list, other);
    assert!(!joined.is_null());
    mux_free_list(joined);

    mux_free_list(list);
    mux_free_list(other);
    assert!(mux_rc_dec(a));
    assert!(mux_rc_dec(b));
    assert!(mux_rc_dec(c));
}

// --- List (Value layer) ------------------------------------------------------

#[test]
fn list_value_layer() {
    let lv = mux_list_value(mux_new_list());
    let v = int(7);
    mux_list_push_back_value(lv, v);
    mux_list_push_value(lv, v); // push to front
    mux_list_set_value(lv, 0, v);
    // negative index wraps from the end; out-of-range extends with defaults
    mux_list_set_value(lv, -1, v);
    mux_list_set_value(lv, 5, v);
    let popped = mux_list_pop_back_value(lv);
    assert!(mux_rc_dec(popped));
    let front = mux_list_pop_value(lv); // pop from front
    assert!(mux_rc_dec(front));
    assert!(mux_rc_dec(lv));
    assert!(mux_rc_dec(v));
}

#[test]
fn list_box_pop_front_and_empty() {
    let list = mux_new_list();
    let a = int(1);
    let b = int(2);
    mux_list_push_back(list, a);
    mux_list_push_back(list, b);

    let front = mux_list_pop(list); // Optional(Some) from front
    assert!(mux_rc_dec(front));

    // get out of range yields Optional(None)
    let oob = mux_list_get(list, 50);
    assert!(mux_rc_dec(oob));

    // drain then pop empty -> Optional(None)
    let _ = mux_list_pop(list);
    let empty = mux_list_pop(list);
    assert!(mux_rc_dec(empty));
    assert!(unsafe { mux_list_is_empty(list) });

    mux_free_list(list);
    assert!(mux_rc_dec(a));
    assert!(mux_rc_dec(b));
}

// --- Map ---------------------------------------------------------------------

#[test]
fn map_box_layer() {
    let map = mux_new_map();
    let k = mux_string_value(CString::new("k").unwrap().as_ptr());
    let v = int(42);

    mux_map_put(map, k, v);
    assert!(mux_map_contains(map, k));
    assert_eq!(unsafe { mux_map_size(map) }, 1);
    assert!(!unsafe { mux_map_is_empty(map) });

    let got = mux_map_get(map, k); // Optional(Some)
    assert!(mux_rc_dec(got));

    let keys = mux_map_keys(map);
    let values = mux_map_values(map);
    let pairs = mux_map_pairs(map);
    assert!(mux_rc_dec(keys));
    assert!(mux_rc_dec(values));
    assert!(mux_rc_dec(pairs));

    assert!(!read_cstr(mux_map_to_string(map)).is_empty());

    let removed = mux_map_remove(map, k); // Optional(Some)
    assert!(mux_rc_dec(removed));
    assert!(unsafe { mux_map_is_empty(map) });

    let other = mux_new_map();
    let merged = mux_map_merge(map, other);
    assert!(!merged.is_null());
    mux_free_map(merged);

    mux_free_map(map);
    mux_free_map(other);
    assert!(mux_rc_dec(k));
    assert!(mux_rc_dec(v));
}

#[test]
fn map_value_layer() {
    let mv = mux_map_value(mux_new_map());
    let k = mux_string_value(CString::new("x").unwrap().as_ptr());
    let v = int(1);
    mux_map_put_value(mv, k, v);
    let removed = mux_map_remove_value(mv, k);
    assert!(mux_rc_dec(removed));
    assert!(mux_rc_dec(mv));
    assert!(mux_rc_dec(k));
    assert!(mux_rc_dec(v));
}

// --- Set ---------------------------------------------------------------------

#[test]
fn set_box_layer() {
    let set = mux_new_set();
    let v = int(5);

    mux_set_add(set, v);
    assert!(mux_set_contains(set, v));
    assert_eq!(unsafe { mux_set_size(set) }, 1);
    assert!(!unsafe { mux_set_is_empty(set) });

    let as_list = mux_set_to_list(set);
    assert!(mux_rc_dec(as_list));
    assert!(!read_cstr(mux_set_to_string(set)).is_empty());

    assert!(mux_set_remove(set, v));
    assert!(!mux_set_remove(set, v));

    let other = mux_new_set();
    let unioned = mux_set_union(set, other);
    assert!(!unioned.is_null());
    mux_free_set(unioned);

    mux_free_set(set);
    mux_free_set(other);
    assert!(mux_rc_dec(v));
}

#[test]
fn set_value_layer() {
    let sv = mux_set_value(mux_new_set());
    let v = int(8);
    mux_set_add_value(sv, v);
    assert!(mux_set_remove_value(sv, v));
    assert!(!mux_set_remove_value(sv, v));
    assert!(mux_rc_dec(sv));
    assert!(mux_rc_dec(v));
}

// --- Tuple -------------------------------------------------------------------

#[test]
fn tuple_ops() {
    let l = int(1);
    let r = int(2);

    let t1 = mux_new_tuple(l, r);
    let t2 = mux_new_tuple(l, r);
    assert!(mux_tuple_eq(t1, t2));

    let left = mux_tuple_left(t1);
    let right = mux_tuple_right(t1);
    assert!(mux_rc_dec(left));
    assert!(mux_rc_dec(right));
    assert_eq!(read_cstr(mux_tuple_to_string(t1)), "(1, 2)");

    // mux_tuple_value consumes the Box; the borrowed get_tuple result must NOT be freed.
    let tv = mux_tuple_value(t1);
    let borrowed = mux_value_get_tuple(tv);
    assert!(!borrowed.is_null());
    assert!(mux_rc_dec(tv));

    let tv2 = mux_tuple_value(t2);
    assert!(mux_rc_dec(tv2));

    assert!(mux_rc_dec(l));
    assert!(mux_rc_dec(r));
}
