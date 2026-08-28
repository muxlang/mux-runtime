//! Failure-path tests for the unified runtime panic.
//!
//! The panic entry points are diverging (`-> !`; they call `std::process::exit`),
//! so they cannot be exercised in-process without terminating the test runner.
//! Each test therefore re-execs this test binary in a child process (via the
//! `MUX_PANIC_CHILD` env flag) to trigger exactly one panic, then asserts the
//! child's stderr and exit code. Coverage from the child is still recorded
//! because `std::process::exit` runs the profiler's atexit handler.

use std::ffi::CString;
use std::process::{Command, Output};

use mux_runtime::assert::mux_assert_eq;
use mux_runtime::panic::{
    mux_panic_cstr, mux_panic_cstr_code, mux_panic_index_oob, mux_panic_key_not_found,
    RuntimeErrorCode,
};
use mux_runtime::std::mux_int_value;

const CHILD_ENV: &str = "MUX_PANIC_CHILD";

fn cstr(s: &str) -> CString {
    CString::new(s).expect("test string has no interior nul")
}

fn in_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// Re-exec this test binary to run only `test` with `CHILD_ENV` set, and return
/// the child's captured output. `--nocapture` is required so the child's panic
/// reaches the real stderr (libtest otherwise redirects per-test output).
fn run_child(test: &str) -> Output {
    Command::new(std::env::current_exe().expect("current_exe"))
        .env(CHILD_ENV, "1")
        .arg(test)
        .arg("--exact")
        .arg("--nocapture")
        .output()
        .expect("failed to re-exec test binary")
}

fn assert_panicked(out: &Output, expected: &[&str]) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1; stderr:\n{stderr}"
    );
    for line in expected {
        assert!(
            stderr.contains(line),
            "missing {line:?} in stderr:\n{stderr}"
        );
    }
}

#[test]
fn index_oob_panics() {
    if in_child() {
        let loc = cstr("main.mux:9:7");
        mux_panic_index_oob(5, 3, loc.as_ptr());
    }
    let out = run_child("index_oob_panics");
    assert_panicked(
        &out,
        &[
            "panic[E0600]: list index out of bounds: index 5, length 3",
            "--> main.mux:9:7",
        ],
    );
}

#[test]
fn key_not_found_panics() {
    if in_child() {
        let loc = cstr("lookup.mux:12:12");
        let key = mux_int_value(42);
        mux_panic_key_not_found(key.cast_const(), loc.as_ptr());
    }
    let out = run_child("key_not_found_panics");
    assert_panicked(
        &out,
        &[
            "panic[E0601]: key not found in map: key 42",
            "--> lookup.mux:12:12",
        ],
    );
}

#[test]
fn cstr_with_location_panics() {
    if in_child() {
        let msg = cstr("division by zero");
        let loc = cstr("math.mux:4:16");
        mux_panic_cstr(msg.as_ptr(), loc.as_ptr());
    }
    let out = run_child("cstr_with_location_panics");
    assert_panicked(
        &out,
        &["panic[E0699]: division by zero", "--> math.mux:4:16"],
    );
}

#[test]
fn cstr_without_location_omits_locator() {
    if in_child() {
        let msg = cstr("boom");
        mux_panic_cstr(msg.as_ptr(), std::ptr::null());
    }
    let out = run_child("cstr_without_location_omits_locator");
    assert_panicked(&out, &["panic[E0699]: boom"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("-->"),
        "no locator expected; stderr:\n{stderr}"
    );
}

#[test]
fn assert_failure_panics() {
    if in_child() {
        // 1 != 2 routes through panic_assert -> the unified panic. If it ever
        // failed to diverge, return rather than re-exec (avoids a fork loop).
        mux_assert_eq(mux_int_value(1), mux_int_value(2));
        return;
    }
    let out = run_child("assert_failure_panics");
    assert_panicked(&out, &["panic[E0603]: assertion failed: expected 2, got 1"]);
}

#[test]
fn typed_cstr_uses_registry_code() {
    if in_child() {
        let msg = cstr("where constraint violated");
        let loc = cstr("model.mux:3:9");
        mux_panic_cstr_code(
            RuntimeErrorCode::WhereConstraintViolation as i32,
            msg.as_ptr(),
            loc.as_ptr(),
        );
    }
    let out = run_child("typed_cstr_uses_registry_code");
    assert_panicked(
        &out,
        &[
            "panic[E0604]: where constraint violated",
            "--> model.mux:3:9",
        ],
    );
}

#[test]
fn unknown_ffi_code_is_internal() {
    if in_child() {
        let msg = cstr("future failure");
        mux_panic_cstr_code(9999, msg.as_ptr(), std::ptr::null());
    }
    let out = run_child("unknown_ffi_code_is_internal");
    assert_panicked(&out, &["panic[E0699]: future failure"]);
}

#[test]
fn registry_codes_are_unique_and_stable() {
    let codes = RuntimeErrorCode::all();
    let mut numeric = codes.iter().map(|code| *code as i32).collect::<Vec<_>>();
    numeric.sort_unstable();
    numeric.dedup();
    assert_eq!(numeric.len(), codes.len());
    assert_eq!(RuntimeErrorCode::IndexOutOfBounds.as_str(), "E0600");
    assert_eq!(RuntimeErrorCode::InternalRuntime.as_str(), "E0699");
}
