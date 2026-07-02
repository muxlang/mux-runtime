use crate::Value;
use std::ffi::CStr;
use std::os::raw::c_char;

/// Terminate the program with a runtime panic rendered in the compiler's
/// diagnostic style: `panic: <message>` followed by a `--> file:line:col`
/// location line when one is available. Any dynamic detail (offending index,
/// key, etc.) is folded into `message`. Writes to stderr and exits with 1.
fn emit_panic(message: &str, loc: Option<String>) -> ! {
    eprintln!("panic: {}", message);
    if let Some(loc) = loc {
        eprintln!("--> {}", loc);
    }
    std::process::exit(1);
}

/// Decode an optional `file:line:col` location pointer baked in by codegen.
fn location(loc: *const c_char) -> Option<String> {
    if loc.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(loc) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Panic with a bare message and no source location.
pub fn panic_with_message(msg: &str) -> ! {
    emit_panic(msg, None)
}

/// FFI entry point for a panic carrying a boxed string message (no location).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic(msg: *mut Value) -> ! {
    if msg.is_null() {
        emit_panic("(no message)", None);
    }
    match unsafe { &*msg } {
        Value::String(s) => emit_panic(s, None),
        other => emit_panic(&other.to_string(), None),
    }
}

/// FFI entry point for a panic with a C-string message and an optional
/// `file:line:col` location.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_cstr(msg: *const c_char, loc: *const c_char) -> ! {
    let message = if msg.is_null() {
        "(no message)".to_string()
    } else {
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    };
    emit_panic(&message, location(loc));
}

/// FFI: panic for a list index outside `[0, length)`.
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_index_oob(index: i64, length: i64, loc: *const c_char) -> ! {
    emit_panic(
        &format!(
            "list index out of bounds: index {}, length {}",
            index, length
        ),
        location(loc),
    );
}

/// FFI: panic for a map lookup on a missing key.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_key_not_found(key: *mut Value, loc: *const c_char) -> ! {
    let key_text = if key.is_null() {
        "(unknown)".to_string()
    } else {
        unsafe { &*key }.to_string()
    };
    emit_panic(
        &format!("key not found in map: key {}", key_text),
        location(loc),
    );
}

/// FFI: panic for integer division or modulo by zero.
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_div_by_zero(loc: *const c_char) -> ! {
    emit_panic("division by zero", location(loc));
}
