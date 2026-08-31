//! Unit tests for the managed `BoxedEnum` value (issue #309): FFI
//! boxing/unboxing, the value-semantic `Clone`/`Drop` that run the compiler-
//! emitted glue, structural equality/ordering via the compare glue, and the
//! `Value` trait arms (equality, ordering, hashing, display, type tag).
#![allow(clippy::mutable_key_type)]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};

use mux_runtime::refcount::{mux_rc_dec, mux_value_deep_clone};
use mux_runtime::std::*;
use mux_runtime::{BoxedEnum, Value};

extern "C" fn noop_glue(_bytes: *mut u8) {}

// A compare glue that treats the whole inline byte buffer as the value, ordering
// two enums by their raw bytes. Real glue emitted by the compiler compares
// payloads structurally, but byte order is enough to exercise the plumbing.
/// Hash matching `cmp_bytes`: it compares the same eight bytes, so hashing them
/// keeps equal values hashing equally.
extern "C" fn hash_bytes(a: *mut u8) -> u64 {
    use std::hash::{Hash, Hasher};
    let bytes = unsafe { std::slice::from_raw_parts(a, 8) };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

extern "C" fn cmp_bytes(a: *mut u8, b: *mut u8) -> i32 {
    // The test buffers are 8 bytes wide.
    let av = unsafe { std::slice::from_raw_parts(a, 8) };
    let bv = unsafe { std::slice::from_raw_parts(b, 8) };
    match av.cmp(bv) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

fn boxed_value(bytes: &[u8]) -> Value {
    Value::BoxedEnum(BoxedEnum::from_bytes(
        bytes, noop_glue, noop_glue, cmp_bytes, hash_bytes,
    ))
}

#[test]
fn box_enum_managed_roundtrips_and_tags_as_opaque() {
    unsafe {
        let mut bytes = [9u8, 0, 0, 0, 1, 2, 3, 4];
        let val = mux_box_enum_managed(
            bytes.as_mut_ptr(),
            bytes.len(),
            noop_glue,
            noop_glue,
            cmp_bytes,
            hash_bytes,
        );
        assert!(!val.is_null());
        // A BoxedEnum is indistinguishable from an Opaque to the language (tag 12).
        assert_eq!(mux_value_get_type_tag(val), 12);

        let payload = mux_value_unbox_enum(val);
        assert!(!payload.is_null());
        let view = std::slice::from_raw_parts(payload, bytes.len());
        assert_eq!(view, &bytes);

        assert!(mux_rc_dec(val));
    }
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
    let original = BoxedEnum::from_bytes(&[0u8; 8], clone_a, drop_a, cmp_bytes, hash_bytes);
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
fn value_boxed_enum_structural_eq_ord_hash_display() {
    let a = boxed_value(&[1, 0, 0, 0, 7, 0, 0, 0]);
    let a2 = boxed_value(&[1, 0, 0, 0, 7, 0, 0, 0]);
    let b = boxed_value(&[1, 0, 0, 0, 9, 0, 0, 0]);
    let other_disc = boxed_value(&[2, 0, 0, 0, 0, 0, 0, 0]);

    // Structural equality/ordering come from the compare glue.
    assert_eq!(a, a2);
    assert_ne!(a, b);
    assert!(a < b);

    // Equal values hash equally - the contract a hash table depends on.
    let hash_of = |v: &Value| {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    };
    assert_eq!(hash_of(&a), hash_of(&a2));
    assert_ne!(hash_of(&a), hash_of(&other_disc));
    // The payload participates too. Hashing the discriminant alone was correct
    // but put every value of one variant in a single bucket, which stopped
    // being acceptable once map and set became hash tables.
    assert_ne!(
        hash_of(&a),
        hash_of(&b),
        "payload should participate in the hash"
    );

    assert_eq!(format!("{a}"), "<enum 8 bytes>");
    assert_eq!(a.type_tag(), 12);
    assert_ne!(a.cmp(&Value::Int(0)), std::cmp::Ordering::Equal);
}

static CLONE_B: AtomicUsize = AtomicUsize::new(0);
extern "C" fn clone_b(_bytes: *mut u8) {
    CLONE_B.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn deep_clone_runs_the_clone_glue() {
    unsafe {
        let mut bytes = [1u8, 2, 3, 4];
        // Boxing deep-clones once so the box is independent of the source.
        let val = mux_box_enum_managed(
            bytes.as_mut_ptr(),
            bytes.len(),
            clone_b,
            noop_glue,
            cmp_bytes,
            hash_bytes,
        );
        assert_eq!(CLONE_B.load(Ordering::SeqCst), 1, "boxing deep-clones once");

        let cloned = mux_value_deep_clone(val);
        assert!(!cloned.is_null());
        assert_eq!(
            CLONE_B.load(Ordering::SeqCst),
            2,
            "deep clone runs the glue again"
        );

        assert!(mux_rc_dec(cloned));
        assert!(mux_rc_dec(val));
    }
}

#[test]
fn value_compare_ffi_orders_and_handles_null() {
    unsafe {
        let one = mux_int_value(1);
        let two = mux_int_value(2);
        assert!(mux_value_compare(one, two) < 0);
        assert!(mux_value_compare(two, one) > 0);
        assert_eq!(mux_value_compare(one, one), 0);
        assert_eq!(mux_value_compare(std::ptr::null(), std::ptr::null()), 0);
        assert!(mux_value_compare(std::ptr::null(), one) < 0);
        assert!(mux_value_compare(one, std::ptr::null()) > 0);
        assert!(mux_rc_dec(one));
        assert!(mux_rc_dec(two));
    }
}
