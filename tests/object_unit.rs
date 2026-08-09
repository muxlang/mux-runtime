//! Unit tests for the object system: registration, allocation, field access,
//! copy callbacks, destructors, and the comparison/hash a class registers.
// The interior mutability the lint sees is ObjectRef's atomic refcount, not
// anything the registered comparison or hash reads.
#![allow(clippy::mutable_key_type)]

use std::ffi::{c_void, CString};

use mux_runtime::object::*;
use mux_runtime::TypeId;

extern "C" fn noop_destructor(_p: *mut c_void) {}

extern "C" fn copy_u64(src: *mut c_void, dst: *mut c_void) {
    unsafe {
        *(dst as *mut u64) = *(src as *const u64);
    }
}

#[test]
fn register_alloc_access_copy_free() {
    let name = CString::new("Probe").unwrap();
    let tid = mux_register_object_type(name.as_ptr(), std::mem::size_of::<u64>());
    mux_register_object_destructor(tid, noop_destructor);
    mux_register_object_copy(tid, copy_u64);

    let obj = mux_alloc_object(tid);
    assert!(!obj.is_null());
    assert_eq!(mux_get_object_type_id(obj), tid);

    let ptr = mux_get_object_ptr(obj) as *mut u64;
    assert!(!ptr.is_null());
    unsafe {
        *ptr = 0xDEAD_BEEF;
    }

    let copy = mux_copy_object(obj);
    assert!(!copy.is_null());
    let copy_ptr = mux_get_object_ptr(copy) as *const u64;
    unsafe {
        assert_eq!(*copy_ptr, 0xDEAD_BEEF);
    }

    mux_free_object(copy);
    mux_free_object(obj);
}

#[test]
fn copy_without_callback_returns_null() {
    let name = CString::new("NoCopy").unwrap();
    let tid = mux_register_object_type(name.as_ptr(), std::mem::size_of::<u64>());
    let obj = mux_alloc_object(tid);
    assert!(!obj.is_null());
    assert!(mux_copy_object(obj).is_null());
    mux_free_object(obj);
}

#[test]
fn invalid_inputs() {
    assert!(mux_alloc_object(999_999).is_null());
    assert!(mux_get_object_ptr(std::ptr::null()).is_null());
    assert_eq!(mux_get_object_type_id(std::ptr::null()), 0);
    assert!(mux_copy_object(std::ptr::null()).is_null());
}

// A class whose contents are one u64. These stand in for the class methods the
// compiler registers, so like a real method they take the boxed object and read
// its data through `mux_get_object_ptr`.
fn contents(obj: *mut mux_runtime::Value) -> u64 {
    unsafe { *(mux_get_object_ptr(obj) as *const u64) }
}

extern "C" fn compare_u64(a: *mut mux_runtime::Value, b: *mut mux_runtime::Value) -> i32 {
    match contents(a).cmp(&contents(b)) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

extern "C" fn equals_u64(a: *mut mux_runtime::Value, b: *mut mux_runtime::Value) -> bool {
    contents(a) == contents(b)
}

extern "C" fn hash_u64(p: *mut mux_runtime::Value) -> u64 {
    contents(p)
}

fn hash_of(value: &mux_runtime::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn alloc_u64_object(tid: TypeId, contents: u64) -> *mut mux_runtime::Value {
    let obj = mux_alloc_object(tid);
    assert!(!obj.is_null());
    unsafe {
        *(mux_get_object_ptr(obj) as *mut u64) = contents;
    }
    obj
}

#[test]
fn registered_compare_and_hash_make_objects_structural() {
    let name = CString::new("Keyed").unwrap();
    let tid = mux_register_object_type(name.as_ptr(), std::mem::size_of::<u64>());
    // Content keying requires copyability: a key has to be snapshotted away
    // from the caller's handle. A class always registers this.
    mux_register_object_copy(tid, copy_u64);
    mux_register_object_compare(tid, compare_u64);
    mux_register_object_equals(tid, equals_u64);
    mux_register_object_hash(tid, hash_u64);

    let a = alloc_u64_object(tid, 7);
    let b = alloc_u64_object(tid, 7);
    let c = alloc_u64_object(tid, 9);

    unsafe {
        // Two separate allocations with the same contents are one value now.
        assert_eq!(&*a, &*b);
        assert_ne!(&*a, &*c);
        assert!(*a < *c);
        // Equal values hash equally - what lets such a class key a map.
        assert_eq!(hash_of(&*a), hash_of(&*b));
        assert_ne!(hash_of(&*a), hash_of(&*c));
    }

    mux_free_object(c);
    mux_free_object(b);
    mux_free_object(a);
}

#[test]
fn objects_are_map_keys_by_contents() {
    use std::collections::HashMap;

    let name = CString::new("MapKey").unwrap();
    let tid = mux_register_object_type(name.as_ptr(), std::mem::size_of::<u64>());
    mux_register_object_copy(tid, copy_u64);
    mux_register_object_equals(tid, equals_u64);
    mux_register_object_hash(tid, hash_u64);

    let a = alloc_u64_object(tid, 42);
    let b = alloc_u64_object(tid, 42);

    unsafe {
        let mut map: HashMap<&mux_runtime::Value, i32> = HashMap::new();
        map.insert(&*a, 1);
        // A lookup with a different allocation of the same contents hits, and
        // re-inserting overwrites rather than growing the map.
        assert_eq!(map.get(&&*b), Some(&1));
        map.insert(&*b, 2);
        assert_eq!(map.len(), 1);
    }

    mux_free_object(b);
    mux_free_object(a);
}

#[test]
fn objects_without_registration_stay_identities() {
    let name = CString::new("Unkeyed").unwrap();
    let tid = mux_register_object_type(name.as_ptr(), std::mem::size_of::<u64>());

    let a = alloc_u64_object(tid, 5);
    let b = alloc_u64_object(tid, 5);

    unsafe {
        // Same contents, but the class declared no capability: only the same
        // allocation is equal to itself.
        assert_eq!(&*a, &*a);
        assert_ne!(&*a, &*b);
        assert_eq!(hash_of(&*a), hash_of(&*a));
    }

    mux_free_object(b);
    mux_free_object(a);
}

#[test]
fn compare_does_not_cross_types() {
    let left = CString::new("LeftType").unwrap();
    let right = CString::new("RightType").unwrap();
    let left_tid = mux_register_object_type(left.as_ptr(), std::mem::size_of::<u64>());
    let right_tid = mux_register_object_type(right.as_ptr(), std::mem::size_of::<u64>());
    mux_register_object_copy(left_tid, copy_u64);
    mux_register_object_compare(left_tid, compare_u64);
    mux_register_object_equals(left_tid, equals_u64);
    mux_register_object_hash(left_tid, hash_u64);

    let a = alloc_u64_object(left_tid, 3);
    let b = alloc_u64_object(right_tid, 3);

    unsafe {
        // Identical contents, different classes: never equal, and the left
        // type's comparison is never handed a right-type pointer.
        assert_ne!(&*a, &*b);
    }

    mux_free_object(b);
    mux_free_object(a);
}

#[test]
fn equality_without_a_hash_still_hashes_consistently() {
    let name = CString::new("EqOnly").unwrap();
    let tid = mux_register_object_type(name.as_ptr(), std::mem::size_of::<u64>());
    mux_register_object_copy(tid, copy_u64);
    mux_register_object_equals(tid, equals_u64);

    let a = alloc_u64_object(tid, 4);
    let b = alloc_u64_object(tid, 4);
    let c = alloc_u64_object(tid, 5);

    unsafe {
        assert_eq!(&*a, &*b);
        assert_ne!(&*a, &*c);
        // The class registered no hash, so every instance has to hash alike:
        // any two of them may turn out to be equal, and equal values that
        // hashed differently would be lost in a table.
        assert_eq!(hash_of(&*a), hash_of(&*b));
        assert_eq!(hash_of(&*a), hash_of(&*c));
        // Ordering follows the same key, so it stays a total order rather than
        // mixing content equality with addresses.
        assert_eq!((*a).cmp(&*b), std::cmp::Ordering::Equal);
        assert_eq!((*a).cmp(&*c), std::cmp::Ordering::Equal);
    }

    mux_free_object(c);
    mux_free_object(b);
    mux_free_object(a);
}

#[test]
fn a_registered_hash_keeps_unequal_instances_apart() {
    let name = CString::new("EqAndHash").unwrap();
    let tid = mux_register_object_type(name.as_ptr(), std::mem::size_of::<u64>());
    mux_register_object_copy(tid, copy_u64);
    mux_register_object_equals(tid, equals_u64);
    mux_register_object_hash(tid, hash_u64);

    let a = alloc_u64_object(tid, 4);
    let b = alloc_u64_object(tid, 4);
    let c = alloc_u64_object(tid, 5);

    unsafe {
        assert_eq!(hash_of(&*a), hash_of(&*b));
        assert_ne!(hash_of(&*a), hash_of(&*c));
        assert_eq!((*a).cmp(&*b), std::cmp::Ordering::Equal);
        assert_ne!((*a).cmp(&*c), std::cmp::Ordering::Equal);
    }

    mux_free_object(c);
    mux_free_object(b);
    mux_free_object(a);
}

#[test]
fn content_keying_without_a_copy_callback_falls_back_to_identity() {
    let name = CString::new("NoCopyKeyed").unwrap();
    let tid = mux_register_object_type(name.as_ptr(), std::mem::size_of::<u64>());
    // Equality and a hash, but nothing to snapshot with. A key is stored at a
    // position derived from its contents, so without a copy the caller keeps a
    // handle to the very object the table is keyed on and could move the key
    // out from under its entry. Identity is stable under that, so the type is
    // keyed the way it was before it registered anything.
    mux_register_object_equals(tid, equals_u64);
    mux_register_object_hash(tid, hash_u64);

    let a = alloc_u64_object(tid, 8);
    let b = alloc_u64_object(tid, 8);

    unsafe {
        assert_ne!(&*a, &*b, "same contents, but keyed by identity");
        assert_eq!(&*a, &*a);
        assert_ne!(hash_of(&*a), hash_of(&*b));
    }

    mux_free_object(b);
    mux_free_object(a);
}
