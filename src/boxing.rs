use crate::Value;
use std::ffi::c_char;

// These helpers exist to match older IR/codegen expectations where primitives
// are "boxed" into `*mut Value` before being stored in `Optional`/`Result` or
// passed through generic code.

#[unsafe(no_mangle)]
pub extern "C" fn mux_box_int(v: i64) -> *mut Value {
    crate::std::mux_int_value(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_box_float(v: f64) -> *mut Value {
    crate::mux_float_value(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_box_bool(v: i32) -> *mut Value {
    crate::std::mux_bool_value(v)
}

#[unsafe(no_mangle)]
/// Construct a managed string value by copying a NUL-terminated C string.
///
/// # Safety
/// `v` must point to a valid NUL-terminated C string readable for the
/// duration of this call. The string is copied and remains caller-owned.
pub unsafe extern "C" fn mux_box_str(v: *const c_char) -> *mut Value {
    unsafe { crate::std::mux_string_value(v) }
}
