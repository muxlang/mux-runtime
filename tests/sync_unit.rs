//! Unit tests for the sync primitives (feature-gated behind `sync`).
//!
//! All in-process: mutex/rwlock lock-unlock, condvar signal/broadcast, and a
//! spawn+join round trip. `condvar_wait` is intentionally not tested (it blocks
//! until signalled from another thread).
#![cfg(feature = "sync")]

use std::ffi::c_void;

use mux_runtime::closure::mux_closure_release;
use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::sync::*;

mod common;
use common::assert_ok;

extern "C" fn thread_body() {}

/// Allocate a closure matching exactly what the compiler produces, so it can be
/// retained/released by the runtime's closure lifetime functions:
///
///   [ i64 refcount=1 | fn_ptr | captures_ptr=null | i64 capture_count=0 ]
///
/// The pointer handed to the runtime points AT the closure struct (the fn_ptr
/// field), i.e. 8 bytes past the refcount header. It is allocated with
/// `libc::malloc` because `mux_closure_release` frees it with `libc::free` once
/// the last reference is dropped. Capture-free, so `captures_ptr` is null.
unsafe fn make_capture_free_closure(func: extern "C" fn()) -> *mut c_void {
    // 4 machine words: refcount, fn_ptr, captures_ptr, capture_count.
    let base = libc::malloc(4 * std::mem::size_of::<usize>()) as *mut usize;
    assert!(!base.is_null());
    *base.add(0) = 1; // refcount header
    *(base.add(1) as *mut *mut c_void) = func as *const () as *mut c_void; // fn_ptr
    *(base.add(2) as *mut *mut c_void) = std::ptr::null_mut(); // captures_ptr
    *base.add(3) = 0; // capture_count
    base.add(1) as *mut c_void // closure struct pointer (at fn_ptr)
}

#[test]
fn mutex_lock_unlock() {
    let m = mux_mutex_new();
    assert!(!m.is_null());
    assert_ok(mux_mutex_lock(m));
    assert_ok(mux_mutex_unlock(m));
    assert!(mux_rc_dec(m));
}

#[test]
fn rwlock_read_and_write() {
    let rw = mux_rwlock_new();
    assert!(!rw.is_null());
    assert_ok(mux_rwlock_read_lock(rw));
    assert_ok(mux_rwlock_unlock(rw));
    assert_ok(mux_rwlock_write_lock(rw));
    assert_ok(mux_rwlock_unlock(rw));
    assert!(mux_rc_dec(rw));
}

#[test]
fn condvar_signal_broadcast() {
    let cv = mux_condvar_new();
    assert!(!cv.is_null());
    assert_ok(mux_condvar_signal(cv));
    assert_ok(mux_condvar_broadcast(cv));
    assert!(mux_rc_dec(cv));
}

#[test]
fn spawn_and_join() {
    // The test owns the initial reference; mux_sync_spawn retains a second one
    // for the worker thread, which releases it when the body returns. Joining
    // guarantees that release has happened, then we drop our own reference,
    // which frees the closure.
    let closure = unsafe { make_capture_free_closure(thread_body) };
    let spawn_res = mux_sync_spawn(closure);
    assert!(mux_result_is_ok(spawn_res));

    let thread_obj = mux_result_data(spawn_res);
    assert!(!thread_obj.is_null());
    assert_ok(mux_thread_join(thread_obj));

    mux_closure_release(closure);
    assert!(mux_rc_dec(thread_obj));
    assert!(mux_rc_dec(spawn_res));
}

#[test]
fn spawn_and_detach() {
    // Detached: the worker releases its reference whenever it finishes; we drop
    // ours here. The atomic refcount guarantees exactly one of the two releases
    // frees the closure, regardless of ordering.
    let closure = unsafe { make_capture_free_closure(thread_body) };
    let spawn_res = mux_sync_spawn(closure);
    assert!(mux_result_is_ok(spawn_res));
    let thread_obj = mux_result_data(spawn_res);
    assert_ok(mux_thread_detach(thread_obj));
    mux_closure_release(closure);
    assert!(mux_rc_dec(thread_obj));
    assert!(mux_rc_dec(spawn_res));
}

#[test]
fn sleep_is_noop_for_nonpositive() {
    mux_sync_sleep(0);
    mux_sync_sleep(-5);
    mux_sync_sleep(1); // tiny real sleep
}

#[test]
fn null_handles_error() {
    // Each call returns an owned error result that must be released.
    let lock_err = mux_mutex_lock(std::ptr::null_mut());
    assert!(!mux_result_is_ok(lock_err));
    assert!(mux_rc_dec(lock_err));
    let join_err = mux_thread_join(std::ptr::null_mut());
    assert!(!mux_result_is_ok(join_err));
    assert!(mux_rc_dec(join_err));
}
