//! Unit tests for the sync primitives (feature-gated behind `sync`).
//!
//! All in-process: mutex/rwlock lock-unlock, condvar signal/broadcast, and a
//! spawn+join round trip. `condvar_wait` is intentionally not tested (it blocks
//! until signalled from another thread).
#![cfg(feature = "sync")]

use std::ffi::c_void;

use mux_runtime::refcount::mux_rc_dec;
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::sync::*;

mod common;
use common::assert_ok;

extern "C" fn thread_body() {}

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
    // Mirror the compiler's ClosureRepr: [function_ptr, captures_ptr].
    let repr: [*mut c_void; 2] = [
        thread_body as *const () as *mut c_void,
        std::ptr::null_mut(),
    ];
    let spawn_res = mux_sync_spawn(repr.as_ptr() as *mut c_void);
    assert!(mux_result_is_ok(spawn_res));

    let thread_obj = mux_result_data(spawn_res);
    assert!(!thread_obj.is_null());
    assert_ok(mux_thread_join(thread_obj));

    assert!(mux_rc_dec(thread_obj));
    assert!(mux_rc_dec(spawn_res));
}

#[test]
fn spawn_and_detach() {
    let repr: [*mut c_void; 2] = [
        thread_body as *const () as *mut c_void,
        std::ptr::null_mut(),
    ];
    let spawn_res = mux_sync_spawn(repr.as_ptr() as *mut c_void);
    assert!(mux_result_is_ok(spawn_res));
    let thread_obj = mux_result_data(spawn_res);
    assert_ok(mux_thread_detach(thread_obj));
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
    assert!(!mux_result_is_ok(mux_mutex_lock(std::ptr::null_mut())));
    assert!(!mux_result_is_ok(mux_thread_join(std::ptr::null_mut())));
}
