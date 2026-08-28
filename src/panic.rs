use crate::Value;
use std::ffi::CStr;
use std::os::raw::c_char;

/// Stable codes for terminating failures raised by the Mux runtime.
///
/// These are intentionally independent from the compiler's diagnostic enum:
/// the runtime is a separately versioned library and must remain usable by
/// older and non-Rust code generators. Values are part of the public ABI and
/// must never be reused.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeErrorCode {
    IndexOutOfBounds = 600,
    KeyNotFound = 601,
    DivisionByZero = 602,
    AssertionFailed = 603,
    WhereConstraintViolation = 604,
    IntegerOverflow = 605,
    InternalRuntime = 699,
}

impl RuntimeErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IndexOutOfBounds => "E0600",
            Self::KeyNotFound => "E0601",
            Self::DivisionByZero => "E0602",
            Self::AssertionFailed => "E0603",
            Self::WhereConstraintViolation => "E0604",
            Self::IntegerOverflow => "E0605",
            Self::InternalRuntime => "E0699",
        }
    }

    pub const fn all() -> &'static [Self] {
        &[
            Self::IndexOutOfBounds,
            Self::KeyNotFound,
            Self::DivisionByZero,
            Self::AssertionFailed,
            Self::WhereConstraintViolation,
            Self::IntegerOverflow,
            Self::InternalRuntime,
        ]
    }

    pub const fn from_ffi(value: i32) -> Self {
        match value {
            600 => Self::IndexOutOfBounds,
            601 => Self::KeyNotFound,
            602 => Self::DivisionByZero,
            603 => Self::AssertionFailed,
            604 => Self::WhereConstraintViolation,
            605 => Self::IntegerOverflow,
            _ => Self::InternalRuntime,
        }
    }
}

impl std::fmt::Display for RuntimeErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Terminate the program with a runtime panic rendered in the compiler's
/// diagnostic style: `panic[E####]: <message>` followed by a `--> file:line:col`
/// location line when one is available. Any dynamic detail (offending index,
/// key, etc.) is folded into `message`. Writes to stderr and exits with 1.
fn emit_panic(code: RuntimeErrorCode, message: &str, loc: Option<String>) -> ! {
    eprintln!("panic[{}]: {}", code, message);
    if let Some(loc) = loc {
        eprintln!("--> {}", loc);
    }
    // A panic bypasses global teardown, so tell the leak-check (when built in)
    // not to report the still-live blocks or override this exit code.
    crate::refcount::note_panic_for_leak_check();
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
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Panic with a bare message and no source location.
pub fn panic_with_message(msg: &str) -> ! {
    emit_panic(RuntimeErrorCode::InternalRuntime, msg, None)
}

/// Terminate with a typed runtime failure and no source location.
pub fn panic_with_code(code: RuntimeErrorCode, msg: &str) -> ! {
    emit_panic(code, msg, None)
}

/// FFI entry point for a panic with a C-string message and an optional
/// `file:line:col` location.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_cstr(msg: *const c_char, loc: *const c_char) -> ! {
    let message = decode_cstr(msg).unwrap_or_else(|| "(no message)".to_string());
    emit_panic(
        RuntimeErrorCode::InternalRuntime,
        &message,
        decode_cstr(loc),
    );
}

/// FFI entry point for a typed runtime failure. Unknown values intentionally
/// collapse to `E0699` so a newer code generator cannot make an older runtime
/// print an unstable or misleading code.
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_cstr_code(code: i32, msg: *const c_char, loc: *const c_char) -> ! {
    let message = decode_cstr(msg).unwrap_or_else(|| "(no message)".to_string());
    emit_panic(RuntimeErrorCode::from_ffi(code), &message, decode_cstr(loc));
}

/// FFI: panic for a list index outside `[0, length)`. `length` is a list size,
/// so it is `u64` to keep the non-negative invariant in the type.
#[unsafe(no_mangle)]
pub extern "C" fn mux_panic_index_oob(index: i64, length: u64, loc: *const c_char) -> ! {
    emit_panic(
        RuntimeErrorCode::IndexOutOfBounds,
        &format!(
            "list index out of bounds: index {}, length {}",
            index, length
        ),
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
        RuntimeErrorCode::KeyNotFound,
        &format!("key not found in map: key {}", key_text),
        decode_cstr(loc),
    );
}
