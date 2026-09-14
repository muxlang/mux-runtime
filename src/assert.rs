use std::ffi::CStr;
use std::os::raw::c_char;

/// Assert a boolean condition and terminate with the assertion message when
/// it is false.
///
/// Mux exposes this as the two-argument built-in `assert(condition, message)`.
/// The condition is represented as an `i32` at the C ABI boundary, matching
/// the representation used by the other boolean runtime entry points.
///
/// # Safety
///
/// When `condition` is false, `message` must point to a valid NUL-terminated C
/// string readable for the duration of this call. A null pointer is handled
/// safely as a defensive fallback for direct FFI callers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_assert(condition: i32, message: *const c_char) {
    if condition != 0 {
        return;
    }

    let message = if message.is_null() {
        "(no assertion message)".to_string()
    } else {
        // SAFETY: The caller contract requires `message` to be a valid,
        // NUL-terminated C string whenever the assertion fails.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    };
    crate::panic::panic_with_code(
        crate::panic::RuntimeErrorCode::AssertionFailed,
        &format!("assertion failed: {message}"),
    );
}
