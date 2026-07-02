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

/// Decode a C string baked in by codegen (a panic message or a `file:line:col`
/// location). Returns `None` for a null pointer. Centralizes the single unsafe
/// deref so both the message and location paths stay in sync.
fn decode_cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` is non-null (checked above) and codegen always passes a
    // pointer to a valid, null-terminated C string constant that outlives the
    // call.
    Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

/// Panic with a bare message and no source location.
pub fn panic_with_message(msg: &str) -> ! {
    emit_panic(msg, None)
}

/// FFI entry point for a panic with a C-string message and an optional
/// `file:line:col` location.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_cstr(msg: *const c_char, loc: *const c_char) -> ! {
    let message = decode_cstr(msg).unwrap_or_else(|| "(no message)".to_string());
    emit_panic(&message, decode_cstr(loc));
}

/// FFI: panic for a list index outside `[0, length)`. `length` is a list size,
/// so it is `u64` to keep the non-negative invariant in the type.
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_index_oob(index: i64, length: u64, loc: *const c_char) -> ! {
    emit_panic(
        &format!("list index out of bounds: index {}, length {}", index, length),
        decode_cstr(loc),
    );
}

/// FFI: panic for a map lookup on a missing key.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_key_not_found(key: *const Value, loc: *const c_char) -> ! {
    let key_text = if key.is_null() {
        "(unknown)".to_string()
    } else {
        // SAFETY: `key` is non-null (checked above) and codegen passes a valid,
        // live `*const Value` for the duration of the call.
        unsafe { &*key }.to_string()
    };
    emit_panic(
        &format!("key not found in map: key {}", key_text),
        decode_cstr(loc),
    );
}
