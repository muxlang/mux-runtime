//! Unit tests for closure reference counting and teardown (src/closure.rs).
//!
//! These build closures with exactly the layout the Mux compiler emits and
//! drive `mux_closure_retain` / `mux_closure_release` directly, covering the
//! capture-walking teardown path (which the spawn tests, being capture-free, do
//! not exercise).

use std::ffi::c_void;

use mux_runtime::closure::{mux_closure_release, mux_closure_retain};
use mux_runtime::refcount::{mux_rc_dec, mux_rc_inc};
use mux_runtime::std::mux_int_value;
use mux_runtime::Value;

const WORD: usize = std::mem::size_of::<usize>();

extern "C" fn noop() {}

/// Allocate a closure matching the compiler's layout:
///
///   [ i64 refcount=1 | fn_ptr | captures_ptr | i64 capture_count ]
///
/// `captures` are the already-owned (+1) values the closure holds one reference
/// to; each is wrapped in its own heap-storage cell, exactly like codegen. The
/// whole thing is `libc::malloc`'d so `mux_closure_release` can `libc::free` it.
/// Returns the pointer to the closure struct (the `fn_ptr` field, 8 bytes past
/// the refcount header).
unsafe fn make_closure(func: extern "C" fn(), captures: &[*mut Value]) -> *mut c_void {
    let base = unsafe { libc::malloc(4 * WORD) } as *mut usize;
    assert!(!base.is_null());
    unsafe {
        *base.add(0) = 1; // refcount header
        *(base.add(1) as *mut *mut c_void) = func as *const () as *mut c_void;

        let captures_ptr = if captures.is_empty() {
            std::ptr::null_mut()
        } else {
            let arr = libc::malloc(captures.len() * WORD) as *mut *mut c_void;
            assert!(!arr.is_null());
            for (i, &value) in captures.iter().enumerate() {
                let cell = libc::malloc(WORD) as *mut *mut Value;
                assert!(!cell.is_null());
                *cell = value;
                *arr.add(i) = cell as *mut c_void;
            }
            arr as *mut c_void
        };
        *(base.add(2) as *mut *mut c_void) = captures_ptr;
        *base.add(3) = captures.len();

        base.add(1) as *mut c_void
    }
}

#[test]
fn retain_and_release_are_null_safe() {
    mux_closure_retain(std::ptr::null_mut());
    mux_closure_release(std::ptr::null_mut());
}

#[test]
fn capture_free_release_frees_closure() {
    // No captures: release should just free the closure allocation.
    let closure = unsafe { make_closure(noop, &[]) };
    mux_closure_release(closure);
}

#[test]
fn retain_delays_free_until_last_release() {
    let closure = unsafe { make_closure(noop, &[]) };
    mux_closure_retain(closure); // refcount 1 -> 2
    mux_closure_release(closure); // 2 -> 1, must NOT free
                                  // If retain had not incremented, the line above would have freed the
                                  // allocation and this second release would be a use-after-free / double
                                  // free. Reaching here cleanly proves the refcount was 2.
    mux_closure_release(closure); // 1 -> 0, frees
}

#[test]
fn release_drops_one_reference_per_capture() {
    unsafe {
        // Each captured value carries an extra reference "owned" by the closure
        // (mirroring the rc_inc codegen performs when capturing). We keep our own
        // reference so we can observe that release dropped exactly one.
        let a = mux_int_value(11);
        mux_rc_inc(a); // refcount 2: one ours, one the closure's
        let b = mux_int_value(22);
        mux_rc_inc(b); // refcount 2

        let closure = make_closure(noop, &[a, b]);
        mux_closure_release(closure); // teardown: rc_dec each capture, free everything

        // Each value is now back to a single reference (ours); dropping it frees
        // it, so mux_rc_dec returns true. If release had failed to drop the
        // closure's reference, refcount would still be 2 and this would return
        // false.
        assert!(
            mux_rc_dec(a),
            "closure did not release its reference to `a`"
        );
        assert!(
            mux_rc_dec(b),
            "closure did not release its reference to `b`"
        );
    }
}

#[test]
fn shared_closure_frees_capture_only_on_last_release() {
    unsafe {
        let v = mux_int_value(7);
        mux_rc_inc(v); // refcount 2: ours + the closure's

        let closure = make_closure(noop, &[v]);
        mux_closure_retain(closure); // closure refcount 1 -> 2

        mux_closure_release(closure); // 2 -> 1: not the last reference, no teardown
        mux_closure_release(closure); // 1 -> 0: teardown drops the capture reference

        // The capture reference survived the first (non-final) release and was
        // dropped exactly once by the final one, so we now hold the last one.
        assert!(
            mux_rc_dec(v),
            "closure did not release its capture on final release"
        );
    }
}
