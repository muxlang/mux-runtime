//! Unit tests for the managed `BoxedEnum` value (issue #309): FFI
//! boxing/unboxing, the value-semantic `Clone`/`Drop` that run the compiler-
//! emitted glue, and the `Value` trait arms (equality, ordering, hashing,
//! display, type tag, deep clone).
#![allow(clippy::mutable_key_type)]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};

use mux_runtime::refcount::{mux_rc_dec, mux_value_deep_clone};
use mux_runtime::std::*;
use mux_runtime::{BoxedEnum, Value};

extern "C" fn noop_glue(_bytes: *mut u8) {}

fn boxed_value(bytes: &[u8]) -> Value {
    Value::BoxedEnum(BoxedEnum::from_bytes(bytes, noop_glue, noop_glue))
}

#[test]
fn box_enum_managed_roundtrips_and_tags_as_opaque() {
    let mut bytes = [9u8, 0, 0, 0, 1, 2, 3, 4];
    let val = mux_box_enum_managed(bytes.as_mut_ptr(), bytes.len(), noop_glue, noop_glue);
    assert!(!val.is_null());
    // A BoxedEnum is indistinguishable from an Opaque to the language (tag 12).
    assert_eq!(mux_value_get_type_tag(val), 12);

    let payload = mux_value_unbox_enum(val);
    assert!(!payload.is_null());
    let view = unsafe { std::slice::from_raw_parts(payload, bytes.len()) };
    assert_eq!(view, &bytes);

    assert!(mux_rc_dec(val));
}

// Per-test counters keep the parallel test runner from racing shared state.
static CLONE_A: AtomicUsize = AtomicUsize::new(0);
static DROP_A: AtomicUsize = AtomicUsize::new(0);
extern "C" fn clone_a(_bytes: *mut u8) {
    CLONE_A.fetch_add(1, Ordering::SeqCst);
}
extern "C" fn drop_a(_bytes: *mut u8) {
    DROP_A.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn clone_and_drop_run_the_glue() {
    let original = BoxedEnum::from_bytes(&[0u8; 8], clone_a, drop_a);
    let copy = original.clone();
    assert_eq!(
        CLONE_A.load(Ordering::SeqCst),
        1,
        "Clone runs clone_glue once"
    );

    drop(copy);
    drop(original);
    assert_eq!(DROP_A.load(Ordering::SeqCst), 2, "each Drop runs drop_glue");
}

#[test]
fn value_boxed_enum_eq_ord_hash_display() {
    let a = boxed_value(&[1, 2, 3, 4]);
    let a2 = boxed_value(&[1, 2, 3, 4]);
    let b = boxed_value(&[9, 9, 9, 9]);

    assert_eq!(a, a2);
    assert_ne!(a, b);
    assert!(a < b);

    let mut ha = DefaultHasher::new();
    a.hash(&mut ha);
    let mut ha2 = DefaultHasher::new();
    a2.hash(&mut ha2);
    assert_eq!(ha.finish(), ha2.finish(), "equal values hash equally");

    assert_eq!(format!("{}", a), "<enum 4 bytes>");
    assert_eq!(a.type_tag(), 12);
    // Ordering against a different variant falls back to variant order.
    assert_ne!(a.cmp(&Value::Int(0)), std::cmp::Ordering::Equal);
}

static CLONE_B: AtomicUsize = AtomicUsize::new(0);
extern "C" fn clone_b(_bytes: *mut u8) {
    CLONE_B.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn deep_clone_runs_the_clone_glue() {
    let mut bytes = [1u8, 2, 3, 4];
    // Boxing deep-clones once so the box is independent of the source.
    let val = mux_box_enum_managed(bytes.as_mut_ptr(), bytes.len(), clone_b, noop_glue);
    assert_eq!(CLONE_B.load(Ordering::SeqCst), 1, "boxing deep-clones once");

    let cloned = unsafe { mux_value_deep_clone(val) };
    assert!(!cloned.is_null());
    assert_eq!(
        CLONE_B.load(Ordering::SeqCst),
        2,
        "deep clone runs the glue again"
    );

    assert!(mux_rc_dec(cloned));
    assert!(mux_rc_dec(val));
}
