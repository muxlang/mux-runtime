//! Unit tests for the object system: registration, allocation, field access,
//! copy callbacks, and destructors.

use std::ffi::{c_void, CString};

use mux_runtime::object::*;

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
