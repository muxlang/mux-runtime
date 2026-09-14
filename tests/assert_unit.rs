//! Unit tests for the built-in assertion entry point. Failures route through
//! the unified runtime panic (`panic: ...` on stderr, exit code 1), which
//! terminates the process, so only the passing paths are exercised here.

use std::ffi::CString;

use mux_runtime::assert::mux_assert;

#[test]
fn assertions_with_true_conditions_pass() {
    let msg = CString::new("ok").unwrap();
    unsafe {
        mux_assert(1, msg.as_ptr());
        mux_assert(1, std::ptr::null());
    }
}
