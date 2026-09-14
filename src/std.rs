use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_shared_object_type,
};
use crate::{
    list::List,
    map::Map,
    refcount::{mux_rc_alloc, mux_rc_dec},
    set::Set,
    Tuple, TypeId, Value,
};
use std::collections::HashMap;
use std::env as sys_env;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{LazyLock, Mutex};

static ENV_MUTATION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Structured error categories are selected by the operation that fails.
///
/// Error text is for humans; it is deliberately never inspected to infer a
/// category. This keeps the public `kind` field stable when an upstream
/// dependency changes its wording.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(crate) enum StdErrorKind {
    Invalid,
    Parse,
    Io,
    Os,
    Timeout,
    Unsupported,
    NotFound,
    Permission,
    Resolve,
    Overflow,
    Range,
    Protocol,
    Certificate,
    Handshake,
    Capture,
    Authentication,
    State,
    Logger,
    Status,
    Transport,
    Callback,
    Config,
    NotUnicode,
    Spawn,
    Closed,
    Domain,
    Match,
}

macro_rules! define_data_error_kind {
    (
        $name:ident,
        [$($variant:ident => $text:literal),+ $(,)?],
        $invalid:ident,
        $range:ident,
        $io:ident,
        $utf8:ident,
        $other:ident
    ) => {
        #[allow(dead_code)]
        #[derive(Clone, Copy)]
        #[repr(i32)]
        enum $name {
            $($variant),+
        }

        impl $name {
            const fn from_std_kind(kind: StdErrorKind) -> Self {
                match kind {
                    StdErrorKind::Invalid => Self::$invalid,
                    StdErrorKind::Range | StdErrorKind::Overflow => Self::$range,
                    StdErrorKind::Io | StdErrorKind::Os => Self::$io,
                    StdErrorKind::NotUnicode => Self::$utf8,
                    _ => Self::$other,
                }
            }

            const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            fn value(self) -> Value {
                Value::Opaque(
                    (self as i32)
                        .to_ne_bytes()
                        .to_vec()
                        .into_boxed_slice(),
                )
            }
        }
    };
}

macro_rules! define_error_kind {
    (
        $name:ident,
        [$($variant:ident => $text:literal),+ $(,)?],
        {$($pattern:pat => $mapped:ident),+ $(,)?}
    ) => {
        #[allow(dead_code)]
        #[derive(Clone, Copy)]
        #[repr(i32)]
        enum $name { $($variant),+ }

        impl $name {
            const fn from_std_kind(kind: StdErrorKind) -> Self {
                match kind { $($pattern => Self::$mapped),+ }
            }
            const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            fn value(self) -> Value {
                Value::Opaque((self as i32).to_ne_bytes().to_vec().into_boxed_slice())
            }
        }
    };
}

define_data_error_kind!(
    JsonErrorKind,
    [Invalid => "invalid", Parse => "parse", Type => "type", Missing => "missing", Duplicate => "duplicate", Limit => "limit", Io => "io"],
    Invalid, Limit, Io, Parse, Parse
);

define_error_kind!(
    CliErrorKind,
    [Invalid => "invalid", Parse => "parse", Io => "io"],
    {
        StdErrorKind::Io | StdErrorKind::Os => Io,
        StdErrorKind::Invalid | StdErrorKind::Parse => Parse,
        _ => Invalid,
    }
);
define_error_kind!(
    CryptoErrorKind,
    [Invalid => "invalid", Unsupported => "unsupported", Authentication => "authentication", Io => "io"],
    {
        StdErrorKind::Io | StdErrorKind::Os => Io,
        StdErrorKind::Unsupported => Unsupported,
        StdErrorKind::Authentication => Authentication,
        _ => Invalid,
    }
);
define_error_kind!(
    RegexErrorKind,
    [Invalid => "invalid", Parse => "parse", Match => "match", Capture => "capture"],
    {
        StdErrorKind::Parse => Parse,
        StdErrorKind::Match => Match,
        StdErrorKind::Capture => Capture,
        _ => Invalid,
    }
);
define_error_kind!(
    LogErrorKind,
    [Invalid => "invalid", Io => "io", Config => "config", State => "state"],
    {
        StdErrorKind::Io | StdErrorKind::Os => Io,
        StdErrorKind::Config => Config,
        StdErrorKind::State => State,
        _ => Invalid,
    }
);
define_data_error_kind!(
    CsvErrorKind,
    [Invalid => "invalid", Parse => "parse", Type => "type", Limit => "limit", Io => "io"],
    Invalid, Limit, Io, Parse, Parse
);
define_data_error_kind!(
    ByteErrorKind,
    [Invalid => "invalid", Parse => "parse", Range => "range", Overflow => "overflow", DivideByZero => "divide_by_zero", Shift => "shift", Io => "io"],
    Invalid, Range, Io, Parse, Parse
);
define_data_error_kind!(
    BytesErrorKind,
    [Invalid => "invalid", Parse => "parse", Range => "range", Overflow => "overflow", Bounds => "bounds", Utf8 => "utf8", Io => "io"],
    Invalid, Range, Io, Utf8, Parse
);

macro_rules! define_data_error {
    (
        $entry:ident,
        $errors:ident,
        $next:ident,
        $type_id:ident,
        $drop:ident,
        $value:ident,
        $result_err:ident,
        $from:ident,
        $kind:ident,
        $detail:ident,
        $message:ident,
        $to_string:ident,
        $handle:ident,
        $field:ident,
        $text:ident,
        $type_name:literal,
        $invalid_detail:literal,
        $kind_type:ident
    ) => {
        #[derive(Clone)]
        struct $entry {
            kind: $kind_type,
            detail: String,
        }

        static $errors: LazyLock<Mutex<HashMap<i64, $entry>>> =
            LazyLock::new(|| Mutex::new(HashMap::new()));
        static $next: AtomicI64 = AtomicI64::new(1);
        static $type_id: LazyLock<TypeId> = LazyLock::new(|| {
            register_shared_object_type(
                $type_name,
                std::mem::size_of::<i64>(),
                Some($drop as extern "C" fn(*mut c_void)),
            )
        });

        extern "C" fn $drop(ptr: *mut c_void) {
            if ptr.is_null() {
                return;
            }
            let handle = unsafe { *ptr.cast::<i64>() };
            if handle != 0 {
                $errors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&handle);
            }
        }

        fn $value(kind: StdErrorKind, detail: String) -> Value {
            let handle = $next.fetch_add(1, Ordering::Relaxed);
            $errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    handle,
                    $entry {
                        kind: $kind_type::from_std_kind(kind),
                        detail,
                    },
                );
            let value = alloc_object(*$type_id);
            if value.is_null() {
                $errors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&handle);
                return Value::String(format!("could not allocate {}", $type_name));
            }
            let ptr = unsafe { get_object_ptr(value) };
            if ptr.is_null() {
                unsafe { mux_rc_dec(value) };
                $errors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&handle);
                return Value::String(format!("could not allocate {}", $type_name));
            }
            unsafe { *ptr.cast::<i64>() = handle };
            let cloned = unsafe { (&*value).clone() };
            unsafe { mux_rc_dec(value) };
            cloned
        }

        pub(crate) fn $result_err(detail: impl Into<String>) -> *mut Value {
            mux_rc_alloc(Value::Result(Err(Box::new($value(
                StdErrorKind::Parse,
                detail.into(),
            )))))
        }

        fn $handle(error: *const Value, expected: TypeId) -> Option<i64> {
            if error.is_null() || unsafe { get_object_type_id(error) } != expected {
                return None;
            }
            let ptr = unsafe { get_object_ptr(error) };
            (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
        }

        fn $field(
            error: *const Value,
            get: fn($entry) -> Value,
            expected: TypeId,
            errors: &Mutex<HashMap<i64, $entry>>,
        ) -> Value {
            let value = $handle(error, expected).and_then(|handle| {
                errors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&handle)
                    .cloned()
            });
            value
                .map(get)
                .unwrap_or_else(|| Value::String(format!("invalid {} handle", $type_name)))
        }

        fn $text(
            error: *const Value,
            decorated: bool,
            expected: TypeId,
            errors: &Mutex<HashMap<i64, $entry>>,
        ) -> String {
            let value = $handle(error, expected).and_then(|handle| {
                errors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&handle)
                    .cloned()
            });
            let Some(entry) = value else {
                return format!("invalid {} handle", $type_name);
            };
            if decorated {
                format!("{}: {}", entry.kind.as_str(), entry.detail)
            } else {
                entry.detail
            }
        }

        #[unsafe(no_mangle)]
        /// Create a typed data error from a displayable message.
        ///
        /// # Safety
        /// `message` must be null or point to a live Mux `Value` for the duration.
        pub unsafe extern "C" fn $from(message: *const Value) -> *mut Value {
            let detail = message
                .as_ref()
                .map_or_else(|| $invalid_detail.to_string(), ToString::to_string);
            mux_rc_alloc($value(StdErrorKind::Parse, detail))
        }

        #[unsafe(no_mangle)]
        /// Read the stable category from a typed data error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        pub unsafe extern "C" fn $kind(error: *const Value) -> *mut Value {
            mux_rc_alloc($field(
                error,
                |entry| entry.kind.value(),
                *$type_id,
                &*$errors,
            ))
        }

        #[unsafe(no_mangle)]
        /// Read the detail from a typed data error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        pub unsafe extern "C" fn $detail(error: *const Value) -> *mut Value {
            mux_rc_alloc($field(
                error,
                |entry| Value::String(entry.detail),
                *$type_id,
                &*$errors,
            ))
        }

        #[unsafe(no_mangle)]
        /// Return the detail message from a typed data error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        pub unsafe extern "C" fn $message(error: *const Value) -> *mut Value {
            mux_rc_alloc(Value::String($text(error, false, *$type_id, &*$errors)))
        }

        #[unsafe(no_mangle)]
        /// Return a decorated string representation of a typed data error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        pub unsafe extern "C" fn $to_string(error: *const Value) -> *mut Value {
            mux_rc_alloc(Value::String($text(error, true, *$type_id, &*$errors)))
        }
    };
}

define_data_error!(
    JsonErrorEntry,
    JSON_ERRORS,
    NEXT_JSON_ERROR_HANDLE,
    JSON_ERROR_TYPE_ID,
    drop_json_error,
    json_error_value,
    json_result_err,
    mux_json_error_from_message,
    mux_json_error_kind,
    mux_json_error_detail,
    mux_json_error_message,
    mux_json_error_to_string,
    json_error_handle,
    json_error_field,
    json_error_text,
    "JsonError",
    "invalid JSON error detail",
    JsonErrorKind
);

define_data_error!(
    CsvErrorEntry,
    CSV_ERRORS,
    NEXT_CSV_ERROR_HANDLE,
    CSV_ERROR_TYPE_ID,
    drop_csv_error,
    csv_error_value,
    csv_result_err,
    mux_csv_error_from_message,
    mux_csv_error_kind,
    mux_csv_error_detail,
    mux_csv_error_message,
    mux_csv_error_to_string,
    csv_error_handle,
    csv_error_field,
    csv_error_text,
    "CsvError",
    "invalid CSV error detail",
    CsvErrorKind
);

define_data_error!(
    ByteErrorEntry,
    BYTE_ERRORS,
    NEXT_BYTE_ERROR_HANDLE,
    BYTE_ERROR_TYPE_ID,
    drop_byte_error,
    byte_error_value,
    byte_result_err,
    mux_byte_error_from_message,
    mux_byte_error_kind,
    mux_byte_error_detail,
    mux_byte_error_message,
    mux_byte_error_to_string,
    byte_error_handle,
    byte_error_field,
    byte_error_text,
    "ByteError",
    "invalid byte error detail",
    ByteErrorKind
);

define_data_error!(
    BytesErrorEntry,
    BYTES_ERRORS,
    NEXT_BYTES_ERROR_HANDLE,
    BYTES_ERROR_TYPE_ID,
    drop_bytes_error,
    bytes_error_value,
    bytes_result_err,
    mux_bytes_error_from_message,
    mux_bytes_error_kind,
    mux_bytes_error_detail,
    mux_bytes_error_message,
    mux_bytes_error_to_string,
    bytes_error_handle,
    bytes_error_field,
    bytes_error_text,
    "BytesError",
    "invalid bytes error detail",
    BytesErrorKind
);

/// Stable categories exposed by `EnvError.kind`.
///
/// Keep this separate from the shared native taxonomy. Environment callers
/// only need categories that can actually arise from environment operations;
/// human-readable detail stays in the error's string fields.
#[derive(Clone, Copy)]
#[repr(i32)]
enum EnvErrorKind {
    Invalid = 0,
    NotUnicode = 1,
    Os = 2,
}

impl EnvErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Invalid => Self::Invalid,
            StdErrorKind::NotUnicode => Self::NotUnicode,
            _ => Self::Os,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::NotUnicode => "not_unicode",
            Self::Os => "os",
        }
    }
}

#[derive(Clone)]
struct EnvErrorEntry {
    kind: EnvErrorKind,
    detail: String,
    key: String,
}

static ENV_ERRORS: LazyLock<Mutex<HashMap<i64, EnvErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ENV_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static ENV_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "EnvError",
        std::mem::size_of::<i64>(),
        Some(drop_env_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_env_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        ENV_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn env_error_value(kind: EnvErrorKind, message: String, key: String) -> Value {
    let handle = NEXT_ENV_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    ENV_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            EnvErrorEntry {
                kind,
                detail: message,
                key,
            },
        );
    let value = alloc_object(*ENV_ERROR_TYPE_ID);
    if value.is_null() {
        ENV_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate environment error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        ENV_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate environment error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

/// Box an environment category using the raw `{ i32 discriminant }` layout
/// used by compiler codegen for the payload-less `EnvErrorKind` enum.
fn env_error_kind_value(kind: EnvErrorKind) -> Value {
    Value::Opaque((kind as i32).to_ne_bytes().to_vec().into_boxed_slice())
}

fn env_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *ENV_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn env_error_field_value(error: *const Value, get: fn(EnvErrorEntry) -> Value) -> Value {
    let value = env_error_handle(error).and_then(|handle| {
        ENV_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(|| Value::String("invalid EnvError handle".to_string()), get)
}

fn env_error_text(error: *const Value, decorated: bool) -> String {
    let value = env_error_handle(error).and_then(|handle| {
        ENV_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid EnvError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

/// Stable categories exposed by `FsError.kind`.
#[derive(Clone, Copy)]
#[repr(i32)]
enum FsErrorKind {
    Invalid = 0,
    Io = 1,
    NotFound = 2,
    Permission = 3,
    NotUnicode = 4,
    Os = 5,
}

impl FsErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Invalid => Self::Invalid,
            StdErrorKind::NotFound => Self::NotFound,
            StdErrorKind::Permission => Self::Permission,
            StdErrorKind::NotUnicode => Self::NotUnicode,
            StdErrorKind::Io => Self::Io,
            _ => Self::Os,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Io => "io",
            Self::NotFound => "not_found",
            Self::Permission => "permission",
            Self::NotUnicode => "not_unicode",
            Self::Os => "os",
        }
    }
}

#[derive(Clone)]
struct FsErrorEntry {
    kind: FsErrorKind,
    detail: String,
    path: String,
}

static FS_ERRORS: LazyLock<Mutex<HashMap<i64, FsErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_FS_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static FS_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "FsError",
        std::mem::size_of::<i64>(),
        Some(drop_fs_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_fs_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        FS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

/// Construct a filesystem error from already-separated fields.
///
/// The path is supplied by the operation that knows it.  It is intentionally
/// not recovered from the human-readable detail string: diagnostics are not a
/// stable serialization format and must never be parsed for structured data.
pub(crate) fn fs_error_value(kind: StdErrorKind, detail: String, path: String) -> Value {
    let handle = NEXT_FS_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    FS_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            FsErrorEntry {
                kind: FsErrorKind::from_std_kind(kind),
                path,
                detail,
            },
        );
    let value = alloc_object(*FS_ERROR_TYPE_ID);
    if value.is_null() {
        FS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate filesystem error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        FS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate filesystem error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

fn fs_error_kind_value(kind: FsErrorKind) -> Value {
    Value::Opaque((kind as i32).to_ne_bytes().to_vec().into_boxed_slice())
}

pub(crate) fn fs_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

pub(crate) fn fs_result_err(detail: String) -> *mut Value {
    fs_result_err_kind(StdErrorKind::Io, detail)
}

pub(crate) fn fs_result_err_kind(kind: StdErrorKind, detail: String) -> *mut Value {
    fs_result_err_kind_path(kind, detail, String::new())
}

pub(crate) fn fs_result_err_kind_path(
    kind: StdErrorKind,
    detail: String,
    path: impl Into<String>,
) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(fs_error_value(
        kind,
        detail,
        path.into(),
    )))))
}

fn fs_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *FS_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn fs_error_field_value(error: *const Value, get: fn(FsErrorEntry) -> Value) -> Value {
    let value = fs_error_handle(error).and_then(|handle| {
        FS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(|| Value::String("invalid FsError handle".to_string()), get)
}

fn fs_error_text(error: *const Value, decorated: bool) -> String {
    let value = fs_error_handle(error).and_then(|handle| {
        FS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid FsError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

/// Stable categories exposed by `IoError.kind`.
///
/// The category is carried separately from the human-facing detail message so
/// callers never need to parse diagnostic text. Keep this enum limited to
/// categories that can arise from the standard I/O operations.
#[derive(Clone, Copy)]
#[repr(i32)]
enum IoErrorKind {
    Invalid = 0,
    Io = 1,
    NotFound = 2,
    Permission = 3,
    NotUnicode = 4,
    Closed = 5,
    Os = 6,
}

impl IoErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Invalid => Self::Invalid,
            StdErrorKind::NotFound => Self::NotFound,
            StdErrorKind::Permission => Self::Permission,
            StdErrorKind::NotUnicode => Self::NotUnicode,
            StdErrorKind::Closed => Self::Closed,
            StdErrorKind::Io => Self::Io,
            _ => Self::Os,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Io => "io",
            Self::NotFound => "not_found",
            Self::Permission => "permission",
            Self::NotUnicode => "not_unicode",
            Self::Closed => "closed",
            Self::Os => "os",
        }
    }
}

#[derive(Clone)]
struct IoErrorEntry {
    kind: IoErrorKind,
    detail: String,
    operation: String,
}

static IO_ERRORS: LazyLock<Mutex<HashMap<i64, IoErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_IO_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static IO_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "IoError",
        std::mem::size_of::<i64>(),
        Some(drop_io_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_io_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        IO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn io_error_value(kind: StdErrorKind, detail: String, operation: String) -> Value {
    let handle = NEXT_IO_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    IO_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            IoErrorEntry {
                kind: IoErrorKind::from_std_kind(kind),
                detail,
                operation,
            },
        );
    let value = alloc_object(*IO_ERROR_TYPE_ID);
    if value.is_null() {
        IO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate I/O error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        IO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate I/O error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn io_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

pub(crate) fn io_result_err(detail: String) -> *mut Value {
    io_result_err_kind(StdErrorKind::Io, detail, String::new())
}

pub(crate) fn io_result_err_kind(
    kind: StdErrorKind,
    detail: String,
    operation: impl Into<String>,
) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(io_error_value(
        kind,
        detail,
        operation.into(),
    )))))
}

fn io_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *IO_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn io_error_field_value(error: *const Value, get: fn(IoErrorEntry) -> Value) -> Value {
    let value = io_error_handle(error).and_then(|handle| {
        IO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(|| Value::String("invalid IoError handle".to_string()), get)
}

fn io_error_text(error: *const Value, decorated: bool) -> String {
    let value = io_error_handle(error).and_then(|handle| {
        IO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid IoError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

/// Stable categories exposed by `NetError.kind`.
#[derive(Clone, Copy)]
#[repr(i32)]
enum NetErrorKind {
    Invalid = 0,
    Timeout = 1,
    Resolve = 2,
    Unsupported = 3,
    Io = 4,
}

impl NetErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Invalid
            | StdErrorKind::Parse
            | StdErrorKind::Range
            | StdErrorKind::Overflow => Self::Invalid,
            StdErrorKind::Timeout => Self::Timeout,
            StdErrorKind::Resolve => Self::Resolve,
            StdErrorKind::Unsupported => Self::Unsupported,
            _ => Self::Io,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Timeout => "timeout",
            Self::Resolve => "resolve",
            Self::Unsupported => "unsupported",
            Self::Io => "io",
        }
    }
}

#[derive(Clone)]
struct NetErrorEntry {
    kind: NetErrorKind,
    detail: String,
    address: String,
}

static NET_ERRORS: LazyLock<Mutex<HashMap<i64, NetErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_NET_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static NET_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "NetError",
        std::mem::size_of::<i64>(),
        Some(drop_net_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_net_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        NET_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

/// Construct a network error from operation-owned fields.
///
/// `address` is supplied separately by the operation that has the address;
/// human-readable diagnostics are never parsed to reconstruct it.
fn net_error_value(kind: StdErrorKind, detail: String, address: String) -> Value {
    let handle = NEXT_NET_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    NET_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            NetErrorEntry {
                kind: NetErrorKind::from_std_kind(kind),
                detail,
                address,
            },
        );
    let value = alloc_object(*NET_ERROR_TYPE_ID);
    if value.is_null() {
        NET_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate network error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        NET_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate network error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn net_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

pub(crate) fn net_result_err(detail: String) -> *mut Value {
    net_result_err_kind(StdErrorKind::Io, detail)
}

pub(crate) fn net_result_err_kind(kind: StdErrorKind, detail: String) -> *mut Value {
    net_result_err_kind_address(kind, detail, String::new())
}

pub(crate) fn net_result_err_kind_address(
    kind: StdErrorKind,
    detail: String,
    address: impl Into<String>,
) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(net_error_value(
        kind,
        detail,
        address.into(),
    )))))
}

fn net_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *NET_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn net_error_field_value(error: *const Value, get: fn(NetErrorEntry) -> Value) -> Value {
    let value = net_error_handle(error).and_then(|handle| {
        NET_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(|| Value::String("invalid NetError handle".to_string()), get)
}

fn net_error_text(error: *const Value, decorated: bool) -> String {
    let value = net_error_handle(error).and_then(|handle| {
        NET_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid NetError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

/// Stable categories exposed by `UrlError.kind`.
#[derive(Clone, Copy)]
#[repr(i32)]
enum UrlErrorKind {
    Invalid = 0,
    Unsupported = 1,
    Parse = 2,
}

impl UrlErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Unsupported => Self::Unsupported,
            StdErrorKind::Parse | StdErrorKind::NotUnicode => Self::Parse,
            _ => Self::Invalid,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Unsupported => "unsupported",
            Self::Parse => "parse",
        }
    }
}

#[derive(Clone)]
struct UrlErrorEntry {
    kind: UrlErrorKind,
    detail: String,
    url: String,
}

static URL_ERRORS: LazyLock<Mutex<HashMap<i64, UrlErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_URL_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static URL_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "UrlError",
        std::mem::size_of::<i64>(),
        Some(drop_url_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_url_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        URL_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

/// Construct a URL error from operation-owned fields. URL context is never
/// recovered from display text.
fn url_error_value(kind: StdErrorKind, detail: String, url: String) -> Value {
    let handle = NEXT_URL_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    URL_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            UrlErrorEntry {
                kind: UrlErrorKind::from_std_kind(kind),
                detail,
                url,
            },
        );
    let value = alloc_object(*URL_ERROR_TYPE_ID);
    if value.is_null() {
        URL_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate URL error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        URL_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate URL error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn url_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

pub(crate) fn url_result_err(detail: String) -> *mut Value {
    url_result_err_kind(StdErrorKind::Parse, detail)
}

pub(crate) fn url_result_err_kind(kind: StdErrorKind, detail: String) -> *mut Value {
    url_result_err_kind_url(kind, detail, String::new())
}

pub(crate) fn url_result_err_kind_url(
    kind: StdErrorKind,
    detail: String,
    url: impl Into<String>,
) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(url_error_value(
        kind,
        detail,
        url.into(),
    )))))
}

fn url_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *URL_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn url_error_field_value(error: *const Value, get: fn(UrlErrorEntry) -> Value) -> Value {
    let value = url_error_handle(error).and_then(|handle| {
        URL_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(|| Value::String("invalid UrlError handle".to_string()), get)
}

fn url_error_text(error: *const Value, decorated: bool) -> String {
    let value = url_error_handle(error).and_then(|handle| {
        URL_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid UrlError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

/// Stable categories exposed by `UuidError.kind`.
#[derive(Clone, Copy)]
#[repr(i32)]
enum UuidErrorKind {
    Invalid = 0,
    Parse = 1,
    NotUnicode = 2,
}

impl UuidErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::NotUnicode => Self::NotUnicode,
            StdErrorKind::Parse => Self::Parse,
            _ => Self::Invalid,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Parse => "parse",
            Self::NotUnicode => "not_unicode",
        }
    }
}

#[derive(Clone)]
struct UuidErrorEntry {
    kind: UuidErrorKind,
    detail: String,
}

static UUID_ERRORS: LazyLock<Mutex<HashMap<i64, UuidErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_UUID_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static UUID_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "UuidError",
        std::mem::size_of::<i64>(),
        Some(drop_uuid_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_uuid_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        UUID_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn uuid_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_UUID_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    UUID_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            UuidErrorEntry {
                kind: UuidErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*UUID_ERROR_TYPE_ID);
    if value.is_null() {
        UUID_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate UUID error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        UUID_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate UUID error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn uuid_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

pub(crate) fn uuid_result_err(detail: String) -> *mut Value {
    uuid_result_err_kind(StdErrorKind::Parse, detail)
}

pub(crate) fn uuid_result_err_kind(kind: StdErrorKind, detail: String) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(uuid_error_value(kind, detail)))))
}

fn uuid_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *UUID_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn uuid_error_field_value(error: *const Value, get: fn(UuidErrorEntry) -> Value) -> Value {
    let value = uuid_error_handle(error).and_then(|handle| {
        UUID_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(
        || Value::String("invalid UuidError handle".to_string()),
        get,
    )
}

fn uuid_error_text(error: *const Value, decorated: bool) -> String {
    let value = uuid_error_handle(error).and_then(|handle| {
        UUID_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid UuidError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_range(start: i64, end: i64) -> *mut List {
    let mut vec = Vec::new();
    for i in start..end {
        vec.push(Value::Int(i));
    }
    Box::into_raw(Box::new(List(vec)))
}

#[unsafe(no_mangle)]
/// Wrap a cloned value in `Some`.
///
/// # Safety
/// `val` must be a non-null pointer to a live, initialized `Value` for the
/// duration of this call. The pointed-to value is borrowed and remains
/// caller-owned.
pub unsafe extern "C" fn mux_some(val: *mut Value) -> *mut Value {
    let value = unsafe { (*val).clone() };
    mux_rc_alloc(Value::Optional(Some(Box::new(value))))
}

// Value creation functions for codegen - using reference counting
#[unsafe(no_mangle)]
pub extern "C" fn mux_int_value(i: i64) -> *mut Value {
    mux_rc_alloc(Value::Int(i))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_bool_value(b: i32) -> *mut Value {
    mux_rc_alloc(Value::Bool(b != 0))
}

#[unsafe(no_mangle)]
/// Construct a string value by copying a NUL-terminated C string.
///
/// # Safety
/// `s` must point to a valid NUL-terminated C string that is readable for the
/// duration of this call. The string is copied and the caller retains
/// ownership of its storage.
pub unsafe extern "C" fn mux_string_value(s: *const c_char) -> *mut Value {
    let c_str = unsafe { CStr::from_ptr(s) };
    let string = c_str.to_string_lossy().into_owned();
    mux_rc_alloc(Value::String(string))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_none() -> *mut Value {
    mux_rc_alloc(Value::Optional(None))
}

#[unsafe(no_mangle)]
/// Wrap a cloned value in an `Ok` result.
///
/// # Safety
/// `val` must point to a live, initialized `Value` for the duration of this
/// call. The pointed-to value is borrowed and remains caller-owned.
pub unsafe extern "C" fn mux_ok(val: *mut Value) -> *mut Value {
    let value = unsafe { (*val).clone() };
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

#[unsafe(no_mangle)]
/// Construct an error result by copying a NUL-terminated C string.
///
/// # Safety
/// `msg` must point to a valid NUL-terminated C string that is readable for
/// the duration of this call. The string is copied and remains caller-owned.
pub unsafe extern "C" fn mux_err(msg: *const c_char) -> *mut Value {
    let c_str = unsafe { CStr::from_ptr(msg) };
    let msg_str = c_str.to_string_lossy().to_string();
    mux_rc_alloc(Value::Result(Err(Box::new(Value::String(msg_str)))))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_new_list() -> *mut List {
    Box::into_raw(Box::new(List(Vec::new())))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_new_map() -> *mut Map {
    Box::into_raw(Box::new(Map(crate::ordered::OrderedMap::new())))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_new_set() -> *mut Set {
    Box::into_raw(Box::new(Set(crate::ordered::OrderedSet::new())))
}

#[unsafe(no_mangle)]
/// Add two values using Mux's value addition rules.
///
/// # Safety
/// Each non-null pointer must point to a live, initialized `Value` readable
/// for the duration of this call. The values are borrowed and remain
/// caller-owned. Null pointers are not valid operands.
pub unsafe extern "C" fn mux_value_add(a: *mut Value, b: *mut Value) -> *mut Value {
    let a = unsafe { &*a };
    let b = unsafe { &*b };
    let result = match (a, b) {
        (Value::Int(a), Value::Int(b)) => Value::Int(a + b),
        (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
        (Value::String(a), Value::String(b)) => Value::String(a.clone() + b),
        (Value::String(a), Value::Int(b)) => Value::String(a.clone() + &b.to_string()),
        (Value::Int(a), Value::String(b)) => Value::String(a.to_string() + b),
        (Value::String(a), Value::Float(b)) => Value::String(a.clone() + &b.to_string()),
        (Value::Float(a), Value::String(b)) => Value::String(a.to_string() + b),
        (Value::String(a), Value::Bool(b)) => Value::String(a.clone() + &b.to_string()),
        (Value::Bool(a), Value::String(b)) => Value::String(a.to_string() + b),
        _ => Value::Int(0), // error
    };
    mux_rc_alloc(result)
}

#[unsafe(no_mangle)]
/// Consume an owned list allocation and wrap it in a managed value.
///
/// # Safety
/// `list` must be a non-null pointer returned by `mux_new_list` or another
/// runtime list-producing function, and ownership must not have been consumed
/// previously. The allocation is consumed exactly once by this call.
pub unsafe extern "C" fn mux_list_value(list: *mut List) -> *mut Value {
    let owned = unsafe { Box::from_raw(list) };
    mux_rc_alloc(Value::List(owned.0))
}

#[unsafe(no_mangle)]
/// Clone a list value into an owned raw list allocation.
///
/// # Safety
/// `val` may be null (which returns null); otherwise it must point to a live,
/// initialized `Value` readable for the duration of this call. The value is
/// borrowed and remains caller-owned.
pub unsafe extern "C" fn mux_value_get_list(val: *mut Value) -> *mut List {
    if val.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        match &*val {
            Value::List(list_data) => Box::into_raw(Box::new(List(list_data.clone()))),
            _ => std::ptr::null_mut(),
        }
    }
}

/// Look up `key` in a map Value and return an owned `Optional` wrapper, reading
/// the live map without cloning it. Mirrors `mux_map_get` but takes the map
/// `Value` directly, so indexing a map in a loop stays O(log n) per read instead
/// of the O(n) whole-map clone that `mux_value_get_map` + `mux_map_get` incurs.
#[unsafe(no_mangle)]
/// Look up a key in a map value and return an owned optional value.
///
/// # Safety
/// Null `val` or `key` is accepted and returns `None`. Any non-null pointer
/// must point to a live, initialized `Value` readable for the duration of
/// this call. Both values are borrowed and remain caller-owned.
pub unsafe extern "C" fn mux_value_map_get_value(
    val: *const Value,
    key: *const Value,
) -> *mut Value {
    if val.is_null() || key.is_null() {
        return mux_rc_alloc(Value::Optional(None));
    }
    let opt = unsafe {
        match &*val {
            Value::Map(map_data) => map_data.get(&*key).cloned(),
            _ => None,
        }
    };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[unsafe(no_mangle)]
/// Clone a map value into an owned raw map allocation.
///
/// # Safety
/// `val` may be null (which returns null); otherwise it must point to a live,
/// initialized `Value` readable for the duration of this call. The value is
/// borrowed and remains caller-owned.
pub unsafe extern "C" fn mux_value_get_map(val: *mut Value) -> *mut Map {
    if val.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        match &*val {
            Value::Map(map_data) => Box::into_raw(Box::new(Map(map_data.clone()))),
            _ => std::ptr::null_mut(),
        }
    }
}

#[unsafe(no_mangle)]
/// Clone a set value into an owned raw set allocation.
///
/// # Safety
/// `val` may be null (which returns null); otherwise it must point to a live,
/// initialized `Value` readable for the duration of this call. The value is
/// borrowed and remains caller-owned.
pub unsafe extern "C" fn mux_value_get_set(val: *mut Value) -> *mut Set {
    if val.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        match &*val {
            Value::Set(set_data) => Box::into_raw(Box::new(Set(set_data.clone()))),
            _ => std::ptr::null_mut(),
        }
    }
}

#[unsafe(no_mangle)]
/// Render a value as an owned NUL-terminated C string.
///
/// # Safety
/// `val` must be a non-null pointer to a live, initialized `Value` readable
/// for the duration of this call. The returned string must be released with
/// `mux_free_string`.
pub unsafe extern "C" fn mux_value_to_string(val: *mut Value) -> *mut c_char {
    let value = unsafe { &*val };
    let s = value.to_string();
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
/// Return the length of a list value, or zero for another value.
///
/// # Safety
/// `val` must be a non-null pointer to a live, initialized `Value` readable
/// for the duration of this call.
pub unsafe extern "C" fn mux_value_list_length(val: *const Value) -> i64 {
    let val = unsafe { &*val };
    if let Value::List(vec) = val {
        vec.len() as i64
    } else {
        0
    }
}

#[unsafe(no_mangle)]
/// Clone one list element into a managed value.
///
/// # Safety
/// `val` must be a non-null pointer to a live, initialized `Value` readable
/// for the duration of this call. The value is borrowed and remains
/// caller-owned.
pub unsafe extern "C" fn mux_value_list_get_value(val: *const Value, index: i64) -> *mut Value {
    let val = unsafe { &*val };
    if let Value::List(vec) = val {
        if index >= 0 && (index as usize) < vec.len() {
            let cloned = vec[index as usize].clone();
            mux_rc_alloc(cloned)
        } else {
            std::ptr::null_mut()
        }
    } else {
        std::ptr::null_mut()
    }
}

#[unsafe(no_mangle)]
/// Clone a list range into a managed list value.
///
/// # Safety
/// `val` must be a non-null pointer to a live, initialized `Value` readable
/// for the duration of this call. The value is borrowed and remains
/// caller-owned.
pub unsafe extern "C" fn mux_value_list_slice(
    val: *const Value,
    start: i64,
    end: i64,
) -> *mut Value {
    let val = unsafe { &*val };
    if let Value::List(vec) = val {
        // Convert and clamp in `usize` so negative bounds cannot wrap to a
        // huge index, and large positive i64 values cannot truncate on 32-bit
        // targets.
        let s = usize::try_from(start.max(0))
            .unwrap_or(usize::MAX)
            .min(vec.len());
        let e = usize::try_from(end.max(0))
            .unwrap_or(usize::MAX)
            .min(vec.len());
        let sliced = if s < e {
            vec[s..e].to_vec()
        } else {
            Vec::new()
        };
        mux_rc_alloc(Value::List(sliced))
    } else {
        mux_rc_alloc(Value::List(Vec::new()))
    }
}

/// Clone a list value into an owned raw list allocation.
///
/// # Safety
/// `val` may be null (which returns null); otherwise it must point to a live,
/// initialized `Value` readable for the duration of this call. The value is
/// borrowed and remains caller-owned.
pub unsafe extern "C" fn mux_value_to_list(val: *mut Value) -> *mut crate::list::List {
    if val.is_null() {
        return std::ptr::null_mut();
    }
    let val = unsafe { (*val).clone() };
    if let Value::List(vec) = val {
        Box::into_raw(Box::new(crate::list::List(vec)))
    } else {
        std::ptr::null_mut()
    }
}

/// # Safety
/// `s` must be a valid pointer returned by a mux-runtime string function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}

/// # Safety
/// `list` must be a valid pointer returned by a mux-runtime list function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_free_list(list: *mut List) {
    if !list.is_null() {
        unsafe { drop(Box::from_raw(list)) };
    }
}

/// # Safety
/// `map` must be a valid pointer returned by a mux-runtime map function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_free_map(map: *mut Map) {
    if !map.is_null() {
        unsafe { drop(Box::from_raw(map)) };
    }
}

/// # Safety
/// `set` must be a valid pointer returned by a mux-runtime set function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_free_set(set: *mut Set) {
    if !set.is_null() {
        unsafe { drop(Box::from_raw(set)) };
    }
}

/// No-op: optional values are now *mut Value managed by reference counting.
/// The pointer is intentionally ignored; use `mux_rc_dec` to release a value.
#[unsafe(no_mangle)]
pub extern "C" fn mux_free_optional(_val: *mut Value) {}

#[unsafe(no_mangle)]
/// Read an environment variable and return `Result<Optional<String>, EnvError>`.
/// A missing variable is `Ok(None)`; invalid names or non-Unicode values are
/// reported as `Err` so they are not confused with absence.
///
/// # Safety
/// A null `key` returns an error. Otherwise `key` must point to a valid
/// NUL-terminated C string readable for the duration of this call.
pub unsafe extern "C" fn mux_env_get(key: *const c_char) -> *mut Value {
    if key.is_null() {
        return env_result_err_kind(StdErrorKind::Invalid, "environment key must not be null");
    }
    let Ok(key) = (unsafe { CStr::from_ptr(key) }).to_str() else {
        return env_result_err_kind(
            StdErrorKind::NotUnicode,
            "environment variable name is not valid UTF-8",
        );
    };
    if let Err(error) = validate_env_key(key) {
        return env_result_err_kind_for_key(StdErrorKind::Invalid, error, key);
    }
    let _guard = ENV_MUTATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match sys_env::var(key) {
        Ok(value) => env_result_ok(Value::Optional(Some(Box::new(Value::String(value))))),
        Err(sys_env::VarError::NotPresent) => env_result_ok(Value::Optional(None)),
        Err(sys_env::VarError::NotUnicode(_)) => env_result_err_kind_for_key(
            StdErrorKind::NotUnicode,
            "environment variable value is not valid UTF-8",
            key,
        ),
    }
}

fn env_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn env_result_err_kind(kind: StdErrorKind, message: impl Into<String>) -> *mut Value {
    env_result_err_kind_for_key(kind, message, String::new())
}

fn env_result_err_kind_for_key(
    kind: StdErrorKind,
    message: impl Into<String>,
    key: impl Into<String>,
) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(env_error_value(
        EnvErrorKind::from_std_kind(kind),
        message.into(),
        key.into(),
    )))))
}

fn validate_env_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("environment variable name cannot be empty".to_string());
    }
    if key.contains('=') {
        return Err("environment variable name cannot contain '='".to_string());
    }
    Ok(())
}

#[unsafe(no_mangle)]
/// Set an environment variable for this process.
///
/// The mutation is serialized because the process environment is shared by
/// all threads. Invalid names are reported as a result instead of panicking.
///
/// # Safety
/// `key` and `value` must be valid, NUL-terminated C strings for the duration
/// of this call.
pub unsafe extern "C" fn mux_env_set(key: *const c_char, value: *const c_char) -> *mut Value {
    if key.is_null() || value.is_null() {
        return env_result_err_kind(
            StdErrorKind::Invalid,
            "environment key and value must not be null",
        );
    }
    let Ok(key) = (unsafe { CStr::from_ptr(key) }).to_str() else {
        return env_result_err_kind(
            StdErrorKind::NotUnicode,
            "environment variable name is not valid UTF-8",
        );
    };
    let Ok(value) = (unsafe { CStr::from_ptr(value) }).to_str() else {
        return env_result_err_kind(
            StdErrorKind::NotUnicode,
            "environment variable value is not valid UTF-8",
        );
    };
    if let Err(error) = validate_env_key(key) {
        return env_result_err_kind_for_key(StdErrorKind::Invalid, error, key);
    }
    let _guard = ENV_MUTATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: the key and value have been validated and contain no NUL bytes.
    unsafe { sys_env::set_var(key, value) };
    env_result_ok(Value::Unit)
}

#[unsafe(no_mangle)]
/// Remove an environment variable for this process. Removing a missing key
/// is successful and idempotent.
///
/// # Safety
/// `key` must be a valid, NUL-terminated C string for the duration of this call.
pub unsafe extern "C" fn mux_env_remove(key: *const c_char) -> *mut Value {
    if key.is_null() {
        return env_result_err_kind(StdErrorKind::Invalid, "environment key must not be null");
    }
    let Ok(key) = (unsafe { CStr::from_ptr(key) }).to_str() else {
        return env_result_err_kind(
            StdErrorKind::NotUnicode,
            "environment variable name is not valid UTF-8",
        );
    };
    if let Err(error) = validate_env_key(key) {
        return env_result_err_kind_for_key(StdErrorKind::Invalid, error, key);
    }
    let _guard = ENV_MUTATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: the key has been validated and contains no NUL bytes.
    unsafe { sys_env::remove_var(key) };
    env_result_ok(Value::Unit)
}

#[unsafe(no_mangle)]
/// Check whether an environment variable exists, reporting invalid Unicode
/// values rather than silently converting them.
///
/// # Safety
/// `key` must be a valid, NUL-terminated C string for the duration of this call.
pub unsafe extern "C" fn mux_env_contains(key: *const c_char) -> *mut Value {
    if key.is_null() {
        return env_result_err_kind(StdErrorKind::Invalid, "environment key must not be null");
    }
    let Ok(key) = (unsafe { CStr::from_ptr(key) }).to_str() else {
        return env_result_err_kind(
            StdErrorKind::NotUnicode,
            "environment variable name is not valid UTF-8",
        );
    };
    if let Err(error) = validate_env_key(key) {
        return env_result_err_kind_for_key(StdErrorKind::Invalid, error, key);
    }
    let _guard = ENV_MUTATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match sys_env::var(key) {
        Ok(_) => env_result_ok(Value::Bool(true)),
        Err(sys_env::VarError::NotPresent) => env_result_ok(Value::Bool(false)),
        Err(sys_env::VarError::NotUnicode(_)) => env_result_err_kind_for_key(
            StdErrorKind::NotUnicode,
            "environment variable value is not valid UTF-8",
            key,
        ),
    }
}

#[unsafe(no_mangle)]
/// Return all process environment entries as sorted `(key, value)` tuples.
/// Invalid Unicode in either component is reported as an error rather than
/// being lossy-converted.
pub extern "C" fn mux_env_entries() -> *mut Value {
    let _guard = ENV_MUTATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut entries = Vec::new();
    for (key, value) in sys_env::vars_os() {
        let Ok(key) = key.into_string() else {
            return env_result_err_kind(
                StdErrorKind::NotUnicode,
                "environment variable name is not valid UTF-8",
            );
        };
        let Ok(value) = value.into_string() else {
            return env_result_err_kind(
                StdErrorKind::NotUnicode,
                "environment variable value is not valid UTF-8",
            );
        };
        entries.push((key, value));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let values = entries
        .into_iter()
        .map(|(key, value)| Value::Tuple(Box::new(Tuple(Value::String(key), Value::String(value)))))
        .collect();
    env_result_ok(Value::List(values))
}

/// Create a typed environment error from a displayable message.
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_env_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid environment error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(env_error_value(EnvErrorKind::Os, detail, String::new()))
}

macro_rules! env_error_string_getter {
    ($name:ident, $get:expr) => {
        /// Read one string field from a typed environment error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(env_error_field_value(error, $get))
        }
    };
}

env_error_string_getter!(mux_env_error_detail, |entry| Value::String(entry.detail));
env_error_string_getter!(mux_env_error_key, |entry| Value::String(entry.key));

/// Read the stable category from a typed environment error.
///
/// The payload-less enum crosses the native ABI as an opaque discriminant;
/// compiler codegen unboxes it into `EnvErrorKind` for normal Mux use.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_env_error_kind(error: *const Value) -> *mut Value {
    mux_rc_alloc(env_error_field_value(error, |entry| {
        env_error_kind_value(entry.kind)
    }))
}

/// Return the detail message from a typed environment error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_env_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(env_error_text(error, false)))
}

/// Return a decorated string representation of a typed environment error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_env_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(env_error_text(error, true)))
}

/// Create a typed filesystem error from a displayable message.
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid filesystem error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(fs_error_value(StdErrorKind::Io, detail, String::new()))
}

macro_rules! fs_error_string_getter {
    ($name:ident, $get:expr) => {
        /// Read one string field from a typed filesystem error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(fs_error_field_value(error, $get))
        }
    };
}

fs_error_string_getter!(mux_fs_error_detail, |entry| Value::String(entry.detail));
fs_error_string_getter!(mux_fs_error_path, |entry| Value::String(entry.path));

/// Read the stable category from a typed filesystem error.
///
/// The payload-less enum crosses the native ABI as an opaque discriminant;
/// compiler codegen unboxes it into `FsErrorKind` for normal Mux use.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_error_kind(error: *const Value) -> *mut Value {
    mux_rc_alloc(fs_error_field_value(error, |entry| {
        fs_error_kind_value(entry.kind)
    }))
}

#[unsafe(no_mangle)]
/// Return the detail message from a typed filesystem error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_fs_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(fs_error_text(error, false)))
}

#[unsafe(no_mangle)]
/// Return a decorated string representation of a typed filesystem error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_fs_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(fs_error_text(error, true)))
}

/// Create a typed I/O error from a displayable message.
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_io_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid I/O error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(io_error_value(StdErrorKind::Io, detail, String::new()))
}

macro_rules! io_error_string_getter {
    ($name:ident, $get:expr) => {
        /// Read one string field from a typed I/O error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(io_error_field_value(error, $get))
        }
    };
}

io_error_string_getter!(mux_io_error_kind, |entry| Value::Opaque(
    (entry.kind as i32)
        .to_ne_bytes()
        .to_vec()
        .into_boxed_slice()
));
io_error_string_getter!(mux_io_error_detail, |entry| Value::String(entry.detail));
io_error_string_getter!(mux_io_error_operation, |entry| Value::String(
    entry.operation
));

#[unsafe(no_mangle)]
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_io_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(io_error_text(error, false)))
}

#[unsafe(no_mangle)]
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_io_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(io_error_text(error, true)))
}

/// Create a typed network error from a displayable message.
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid network error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(net_error_value(StdErrorKind::Io, detail, String::new()))
}

macro_rules! net_error_string_getter {
    ($name:ident, $get:expr) => {
        /// Read one string field from a typed network error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(net_error_field_value(error, $get))
        }
    };
}

net_error_string_getter!(mux_net_error_detail, |entry| Value::String(entry.detail));
net_error_string_getter!(mux_net_error_address, |entry| Value::String(entry.address));

/// Read the typed category from a network error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_error_kind(error: *const Value) -> *mut Value {
    mux_rc_alloc(net_error_field_value(error, |entry| {
        Value::Opaque(
            (entry.kind as i32)
                .to_ne_bytes()
                .to_vec()
                .into_boxed_slice(),
        )
    }))
}

/// Return the detail message from a typed network error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(net_error_text(error, false)))
}

/// Return a decorated string representation of a typed network error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_net_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(net_error_text(error, true)))
}

/// Create a typed URL error from a displayable message.
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid URL error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(url_error_value(StdErrorKind::Parse, detail, String::new()))
}

macro_rules! url_error_string_getter {
    ($name:ident, $get:expr) => {
        /// Read one string field from a typed URL error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(url_error_field_value(error, $get))
        }
    };
}

url_error_string_getter!(mux_url_error_detail, |entry| Value::String(entry.detail));
url_error_string_getter!(mux_url_error_url, |entry| Value::String(entry.url));

/// Read the typed category from a URL error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_error_kind(error: *const Value) -> *mut Value {
    mux_rc_alloc(url_error_field_value(error, |entry| {
        Value::Opaque(
            (entry.kind as i32)
                .to_ne_bytes()
                .to_vec()
                .into_boxed_slice(),
        )
    }))
}

/// Return the detail message from a typed URL error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(url_error_text(error, false)))
}

/// Return a decorated string representation of a typed URL error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_url_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(url_error_text(error, true)))
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
#[repr(i32)]
enum MathErrorKind {
    Invalid = 0,
    Range = 1,
    Overflow = 2,
    Domain = 3,
}

impl MathErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Range => Self::Range,
            StdErrorKind::Overflow => Self::Overflow,
            StdErrorKind::Domain => Self::Domain,
            _ => Self::Invalid,
        }
    }
    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Range => "range",
            Self::Overflow => "overflow",
            Self::Domain => "domain",
        }
    }
    fn value(self) -> Value {
        Value::Opaque((self as i32).to_ne_bytes().to_vec().into_boxed_slice())
    }
}

#[derive(Clone)]
struct MathErrorEntry {
    kind: MathErrorKind,
    detail: String,
}

static MATH_ERRORS: LazyLock<Mutex<HashMap<i64, MathErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_MATH_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static MATH_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "MathError",
        std::mem::size_of::<i64>(),
        Some(drop_math_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_math_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        MATH_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn math_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_MATH_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    MATH_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            MathErrorEntry {
                kind: MathErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*MATH_ERROR_TYPE_ID);
    if value.is_null() {
        MATH_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate math error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        MATH_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate math error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn math_result_err(detail: String) -> *mut Value {
    math_result_err_kind(StdErrorKind::Invalid, detail)
}

pub(crate) fn math_result_err_kind(kind: StdErrorKind, detail: String) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(math_error_value(kind, detail)))))
}

fn math_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *MATH_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn math_error_field_value(error: *const Value, get: fn(MathErrorEntry) -> Value) -> Value {
    let value = math_error_handle(error).and_then(|handle| {
        MATH_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(
        || Value::String("invalid MathError handle".to_string()),
        get,
    )
}

fn math_error_text(error: *const Value, decorated: bool) -> String {
    let value = math_error_handle(error).and_then(|handle| {
        MATH_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid MathError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_math_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid math error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(math_error_value(StdErrorKind::Invalid, detail))
}

macro_rules! math_error_string_getter {
    ($name:ident, $get:expr) => {
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(math_error_field_value(error, $get))
        }
    };
}

math_error_string_getter!(mux_math_error_kind, |entry| entry.kind.value());
math_error_string_getter!(mux_math_error_detail, |entry| Value::String(entry.detail));

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_math_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(math_error_text(error, false)))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_math_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(math_error_text(error, true)))
}

#[derive(Clone)]
struct ProcessErrorEntry {
    kind: ProcessErrorKind,
    detail: String,
}

/// Stable categories exposed by `ProcessError.kind`.
#[allow(dead_code)]
#[derive(Clone, Copy)]
#[repr(i32)]
enum ProcessErrorKind {
    Invalid = 0,
    Io = 1,
    Spawn = 2,
    Timeout = 3,
    State = 4,
    NotFound = 5,
}

impl ProcessErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Invalid => Self::Invalid,
            StdErrorKind::Spawn => Self::Spawn,
            StdErrorKind::Timeout => Self::Timeout,
            StdErrorKind::NotFound => Self::NotFound,
            StdErrorKind::Io | StdErrorKind::Os => Self::Io,
            _ => Self::State,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Io => "io",
            Self::Spawn => "spawn",
            Self::Timeout => "timeout",
            Self::State => "state",
            Self::NotFound => "not_found",
        }
    }

    fn value(self) -> Value {
        Value::Opaque((self as i32).to_ne_bytes().to_vec().into_boxed_slice())
    }
}

static PROCESS_ERRORS: LazyLock<Mutex<HashMap<i64, ProcessErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_PROCESS_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static PROCESS_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "ProcessError",
        std::mem::size_of::<i64>(),
        Some(drop_process_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_process_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        PROCESS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn process_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_PROCESS_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    PROCESS_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            ProcessErrorEntry {
                kind: ProcessErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*PROCESS_ERROR_TYPE_ID);
    if value.is_null() {
        PROCESS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate process error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        PROCESS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate process error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn process_result_err(detail: impl Into<String>) -> *mut Value {
    process_result_err_kind(StdErrorKind::Io, detail)
}

pub(crate) fn process_result_err_kind(kind: StdErrorKind, detail: impl Into<String>) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(process_error_value(
        kind,
        detail.into(),
    )))))
}

fn process_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *PROCESS_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn process_error_field_value(error: *const Value, get: fn(ProcessErrorEntry) -> Value) -> Value {
    let value = process_error_handle(error).and_then(|handle| {
        PROCESS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(
        || Value::String("invalid ProcessError handle".to_string()),
        get,
    )
}

fn process_error_text(error: *const Value, decorated: bool) -> String {
    let value = process_error_handle(error).and_then(|handle| {
        PROCESS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid ProcessError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_process_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid process error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(process_error_value(StdErrorKind::Io, detail))
}

macro_rules! process_error_string_getter {
    ($name:ident, $get:expr) => {
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(process_error_field_value(error, $get))
        }
    };
}

process_error_string_getter!(mux_process_error_kind, |entry| entry.kind.value());
process_error_string_getter!(mux_process_error_detail, |entry| Value::String(
    entry.detail
));

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_process_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(process_error_text(error, false)))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_process_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(process_error_text(error, true)))
}

#[derive(Clone)]
struct TlsErrorEntry {
    kind: TlsErrorKind,
    detail: String,
}

/// Stable categories exposed by `TlsError.kind`.
#[allow(dead_code)]
#[derive(Clone, Copy)]
#[repr(i32)]
enum TlsErrorKind {
    Invalid = 0,
    Io = 1,
    Timeout = 2,
    Handshake = 3,
    Certificate = 4,
    Unsupported = 5,
    Protocol = 6,
}

impl TlsErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Timeout => Self::Timeout,
            StdErrorKind::Handshake => Self::Handshake,
            StdErrorKind::Certificate => Self::Certificate,
            StdErrorKind::Unsupported => Self::Unsupported,
            StdErrorKind::Protocol => Self::Protocol,
            StdErrorKind::Io | StdErrorKind::Os => Self::Io,
            _ => Self::Invalid,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Io => "io",
            Self::Timeout => "timeout",
            Self::Handshake => "handshake",
            Self::Certificate => "certificate",
            Self::Unsupported => "unsupported",
            Self::Protocol => "protocol",
        }
    }

    fn value(self) -> Value {
        Value::Opaque((self as i32).to_ne_bytes().to_vec().into_boxed_slice())
    }
}

static TLS_ERRORS: LazyLock<Mutex<HashMap<i64, TlsErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_TLS_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static TLS_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "TlsError",
        std::mem::size_of::<i64>(),
        Some(drop_tls_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_tls_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        TLS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn tls_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_TLS_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    TLS_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            TlsErrorEntry {
                kind: TlsErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*TLS_ERROR_TYPE_ID);
    if value.is_null() {
        TLS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate TLS error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        TLS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate TLS error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn tls_result_err(detail: impl Into<String>) -> *mut Value {
    tls_result_err_kind(StdErrorKind::Io, detail)
}

pub(crate) fn tls_result_err_kind(kind: StdErrorKind, detail: impl Into<String>) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(tls_error_value(
        kind,
        detail.into(),
    )))))
}

fn tls_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *TLS_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn tls_error_field_value(error: *const Value, get: fn(TlsErrorEntry) -> Value) -> Value {
    let value = tls_error_handle(error).and_then(|handle| {
        TLS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(|| Value::String("invalid TlsError handle".to_string()), get)
}

fn tls_error_text(error: *const Value, decorated: bool) -> String {
    let value = tls_error_handle(error).and_then(|handle| {
        TLS_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid TlsError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_tls_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid TLS error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(tls_error_value(StdErrorKind::Io, detail))
}

macro_rules! tls_error_string_getter {
    ($name:ident, $get:expr) => {
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(tls_error_field_value(error, $get))
        }
    };
}

tls_error_string_getter!(mux_tls_error_kind, |entry| entry.kind.value());
tls_error_string_getter!(mux_tls_error_detail, |entry| Value::String(entry.detail));

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_tls_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(tls_error_text(error, false)))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_tls_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(tls_error_text(error, true)))
}

/// Stable categories exposed by `DateTimeError.kind`.
#[derive(Clone, Copy)]
#[repr(i32)]
enum DateTimeErrorKind {
    Invalid = 0,
    Parse = 1,
    Range = 2,
    Format = 3,
    System = 4,
}

impl DateTimeErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Invalid => Self::Invalid,
            StdErrorKind::Range | StdErrorKind::Overflow => Self::Range,
            StdErrorKind::Parse => Self::Parse,
            StdErrorKind::Io | StdErrorKind::Os => Self::System,
            _ => Self::Format,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Parse => "parse",
            Self::Range => "range",
            Self::Format => "format",
            Self::System => "system",
        }
    }
}

#[derive(Clone)]
struct DateTimeErrorEntry {
    kind: DateTimeErrorKind,
    detail: String,
}

static DATETIME_ERRORS: LazyLock<Mutex<HashMap<i64, DateTimeErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_DATETIME_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static DATETIME_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "DateTimeError",
        std::mem::size_of::<i64>(),
        Some(drop_datetime_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_datetime_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        DATETIME_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn datetime_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_DATETIME_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    DATETIME_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            DateTimeErrorEntry {
                kind: DateTimeErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*DATETIME_ERROR_TYPE_ID);
    if value.is_null() {
        DATETIME_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate datetime error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        DATETIME_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate datetime error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn datetime_result_err(detail: impl Into<String>) -> *mut Value {
    datetime_result_err_kind(StdErrorKind::Parse, detail)
}

pub(crate) fn datetime_result_err_kind(
    kind: StdErrorKind,
    detail: impl Into<String>,
) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(datetime_error_value(
        kind,
        detail.into(),
    )))))
}

fn datetime_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *DATETIME_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn datetime_error_field_value(error: *const Value, get: fn(DateTimeErrorEntry) -> Value) -> Value {
    let value = datetime_error_handle(error).and_then(|handle| {
        DATETIME_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(
        || Value::String("invalid DateTimeError handle".to_string()),
        get,
    )
}

fn datetime_error_text(error: *const Value, decorated: bool) -> String {
    let value = datetime_error_handle(error).and_then(|handle| {
        DATETIME_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid DateTimeError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_datetime_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid datetime error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(datetime_error_value(StdErrorKind::Parse, detail))
}

macro_rules! datetime_error_string_getter {
    ($name:ident, $get:expr) => {
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(datetime_error_field_value(error, $get))
        }
    };
}

datetime_error_string_getter!(mux_datetime_error_kind, |entry| Value::Opaque(
    (entry.kind as i32)
        .to_ne_bytes()
        .to_vec()
        .into_boxed_slice()
));
datetime_error_string_getter!(mux_datetime_error_detail, |entry| Value::String(
    entry.detail
));

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_datetime_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(datetime_error_text(error, false)))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_datetime_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(datetime_error_text(error, true)))
}

#[derive(Clone)]
struct CliErrorEntry {
    kind: CliErrorKind,
    detail: String,
}

static CLI_ERRORS: LazyLock<Mutex<HashMap<i64, CliErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_CLI_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static CLI_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "CliError",
        std::mem::size_of::<i64>(),
        Some(drop_cli_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_cli_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        CLI_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn cli_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_CLI_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    CLI_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            CliErrorEntry {
                kind: CliErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*CLI_ERROR_TYPE_ID);
    if value.is_null() {
        CLI_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate CLI error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        CLI_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate CLI error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn cli_result_err(detail: impl Into<String>) -> *mut Value {
    cli_result_err_kind(StdErrorKind::Parse, detail)
}

pub(crate) fn cli_result_err_kind(kind: StdErrorKind, detail: impl Into<String>) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(cli_error_value(
        kind,
        detail.into(),
    )))))
}

fn cli_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *CLI_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn cli_error_field_value(error: *const Value, get: fn(CliErrorEntry) -> Value) -> Value {
    let value = cli_error_handle(error).and_then(|handle| {
        CLI_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(|| Value::String("invalid CliError handle".to_string()), get)
}

fn cli_error_text(error: *const Value, decorated: bool) -> String {
    let value = cli_error_handle(error).and_then(|handle| {
        CLI_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid CliError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_cli_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid CLI error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(cli_error_value(StdErrorKind::Parse, detail))
}

macro_rules! cli_error_string_getter {
    ($name:ident, $get:expr) => {
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(cli_error_field_value(error, $get))
        }
    };
}

cli_error_string_getter!(mux_cli_error_kind, |entry| entry.kind.value());
cli_error_string_getter!(mux_cli_error_detail, |entry| Value::String(entry.detail));

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_cli_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(cli_error_text(error, false)))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_cli_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(cli_error_text(error, true)))
}

#[derive(Clone)]
struct CryptoErrorEntry {
    kind: CryptoErrorKind,
    detail: String,
}

static CRYPTO_ERRORS: LazyLock<Mutex<HashMap<i64, CryptoErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_CRYPTO_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static CRYPTO_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "CryptoError",
        std::mem::size_of::<i64>(),
        Some(drop_crypto_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_crypto_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        CRYPTO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn crypto_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_CRYPTO_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    CRYPTO_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            CryptoErrorEntry {
                kind: CryptoErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*CRYPTO_ERROR_TYPE_ID);
    if value.is_null() {
        CRYPTO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate crypto error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        CRYPTO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate crypto error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn crypto_result_err(detail: String) -> *mut Value {
    crypto_result_err_kind(StdErrorKind::Invalid, detail)
}

pub(crate) fn crypto_result_err_kind(kind: StdErrorKind, detail: String) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(crypto_error_value(
        kind, detail,
    )))))
}

fn crypto_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *CRYPTO_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn crypto_error_field_value(error: *const Value, get: fn(CryptoErrorEntry) -> Value) -> Value {
    let value = crypto_error_handle(error).and_then(|handle| {
        CRYPTO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(
        || Value::String("invalid CryptoError handle".to_string()),
        get,
    )
}

fn crypto_error_text(error: *const Value, decorated: bool) -> String {
    let value = crypto_error_handle(error).and_then(|handle| {
        CRYPTO_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid CryptoError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_crypto_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid crypto error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(crypto_error_value(StdErrorKind::Invalid, detail))
}

macro_rules! crypto_error_string_getter {
    ($name:ident, $get:expr) => {
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(crypto_error_field_value(error, $get))
        }
    };
}

crypto_error_string_getter!(mux_crypto_error_kind, |entry| entry.kind.value());
crypto_error_string_getter!(mux_crypto_error_detail, |entry| Value::String(entry.detail));

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_crypto_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(crypto_error_text(error, false)))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_crypto_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(crypto_error_text(error, true)))
}

#[derive(Clone)]
struct RegexErrorEntry {
    kind: RegexErrorKind,
    detail: String,
}

static REGEX_ERRORS: LazyLock<Mutex<HashMap<i64, RegexErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_REGEX_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static REGEX_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "RegexError",
        std::mem::size_of::<i64>(),
        Some(drop_regex_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_regex_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        REGEX_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn regex_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_REGEX_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    REGEX_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            RegexErrorEntry {
                kind: RegexErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*REGEX_ERROR_TYPE_ID);
    if value.is_null() {
        REGEX_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate regex error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        REGEX_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate regex error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn regex_result_err(detail: String) -> *mut Value {
    regex_result_err_kind(StdErrorKind::Invalid, detail)
}

pub(crate) fn regex_result_err_kind(kind: StdErrorKind, detail: String) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(regex_error_value(
        kind, detail,
    )))))
}

fn regex_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *REGEX_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn regex_error_field_value(error: *const Value, get: fn(RegexErrorEntry) -> Value) -> Value {
    let value = regex_error_handle(error).and_then(|handle| {
        REGEX_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(
        || Value::String("invalid RegexError handle".to_string()),
        get,
    )
}

fn regex_error_text(error: *const Value, decorated: bool) -> String {
    let value = regex_error_handle(error).and_then(|handle| {
        REGEX_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid RegexError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_regex_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid regex error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(regex_error_value(StdErrorKind::Invalid, detail))
}

macro_rules! regex_error_string_getter {
    ($name:ident, $get:expr) => {
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(regex_error_field_value(error, $get))
        }
    };
}

regex_error_string_getter!(mux_regex_error_kind, |entry| entry.kind.value());
regex_error_string_getter!(mux_regex_error_detail, |entry| Value::String(entry.detail));

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_regex_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(regex_error_text(error, false)))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_regex_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(regex_error_text(error, true)))
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
#[repr(i32)]
enum RandomErrorKind {
    Invalid = 0,
    Range = 1,
    Unsupported = 2,
    Io = 3,
}

impl RandomErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Range | StdErrorKind::Overflow => Self::Range,
            StdErrorKind::Unsupported => Self::Unsupported,
            StdErrorKind::Io | StdErrorKind::Os => Self::Io,
            _ => Self::Invalid,
        }
    }
    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Range => "range",
            Self::Unsupported => "unsupported",
            Self::Io => "io",
        }
    }
    fn value(self) -> Value {
        Value::Opaque((self as i32).to_ne_bytes().to_vec().into_boxed_slice())
    }
}

#[derive(Clone)]
struct RandomErrorEntry {
    kind: RandomErrorKind,
    detail: String,
}

static RANDOM_ERRORS: LazyLock<Mutex<HashMap<i64, RandomErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_RANDOM_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static RANDOM_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "RandomError",
        std::mem::size_of::<i64>(),
        Some(drop_random_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_random_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        RANDOM_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn random_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_RANDOM_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    RANDOM_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            RandomErrorEntry {
                kind: RandomErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*RANDOM_ERROR_TYPE_ID);
    if value.is_null() {
        RANDOM_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate random error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        RANDOM_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate random error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn random_result_err(detail: String) -> *mut Value {
    random_result_err_kind(StdErrorKind::Invalid, detail)
}

pub(crate) fn random_result_err_kind(kind: StdErrorKind, detail: String) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(random_error_value(
        kind, detail,
    )))))
}

fn random_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *RANDOM_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn random_error_field_value(error: *const Value, get: fn(RandomErrorEntry) -> Value) -> Value {
    let value = random_error_handle(error).and_then(|handle| {
        RANDOM_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(
        || Value::String("invalid RandomError handle".to_string()),
        get,
    )
}

fn random_error_text(error: *const Value, decorated: bool) -> String {
    let value = random_error_handle(error).and_then(|handle| {
        RANDOM_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid RandomError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_random_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid random error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(random_error_value(StdErrorKind::Invalid, detail))
}

macro_rules! random_error_string_getter {
    ($name:ident, $get:expr) => {
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(random_error_field_value(error, $get))
        }
    };
}

random_error_string_getter!(mux_random_error_kind, |entry| entry.kind.value());
random_error_string_getter!(mux_random_error_detail, |entry| Value::String(entry.detail));

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_random_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(random_error_text(error, false)))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_random_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(random_error_text(error, true)))
}

#[derive(Clone)]
struct LogErrorEntry {
    kind: LogErrorKind,
    detail: String,
}

static LOG_ERRORS: LazyLock<Mutex<HashMap<i64, LogErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_LOG_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static LOG_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "LogError",
        std::mem::size_of::<i64>(),
        Some(drop_log_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_log_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        LOG_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn log_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_LOG_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    LOG_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            LogErrorEntry {
                kind: LogErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*LOG_ERROR_TYPE_ID);
    if value.is_null() {
        LOG_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate log error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        LOG_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate log error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn log_result_err(detail: String) -> *mut Value {
    log_result_err_kind(StdErrorKind::Logger, detail)
}

pub(crate) fn log_result_err_kind(kind: StdErrorKind, detail: String) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(log_error_value(kind, detail)))))
}

fn log_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *LOG_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn log_error_field_value(error: *const Value, get: fn(LogErrorEntry) -> Value) -> Value {
    let value = log_error_handle(error).and_then(|handle| {
        LOG_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(|| Value::String("invalid LogError handle".to_string()), get)
}

fn log_error_text(error: *const Value, decorated: bool) -> String {
    let value = log_error_handle(error).and_then(|handle| {
        LOG_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid LogError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_log_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid log error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(log_error_value(StdErrorKind::Logger, detail))
}

macro_rules! log_error_string_getter {
    ($name:ident, $get:expr) => {
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(log_error_field_value(error, $get))
        }
    };
}

log_error_string_getter!(mux_log_error_kind, |entry| entry.kind.value());
log_error_string_getter!(mux_log_error_detail, |entry| Value::String(entry.detail));

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_log_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(log_error_text(error, false)))
}

#[unsafe(no_mangle)]
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_log_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(log_error_text(error, true)))
}

/// Stable categories exposed by `SyncError.kind`.
#[derive(Clone, Copy)]
#[repr(i32)]
enum SyncErrorKind {
    Invalid = 0,
    State = 1,
    Timeout = 2,
    Closed = 3,
    Callback = 4,
    Spawn = 5,
    Io = 6,
}

impl SyncErrorKind {
    const fn from_std_kind(kind: StdErrorKind) -> Self {
        match kind {
            StdErrorKind::Invalid => Self::Invalid,
            StdErrorKind::Timeout => Self::Timeout,
            StdErrorKind::Closed => Self::Closed,
            StdErrorKind::Callback => Self::Callback,
            StdErrorKind::Spawn => Self::Spawn,
            StdErrorKind::Io | StdErrorKind::Os => Self::Io,
            _ => Self::State,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::State => "state",
            Self::Timeout => "timeout",
            Self::Closed => "closed",
            Self::Callback => "callback",
            Self::Spawn => "spawn",
            Self::Io => "io",
        }
    }

    fn value(self) -> Value {
        Value::Opaque((self as i32).to_ne_bytes().to_vec().into_boxed_slice())
    }
}

#[derive(Clone)]
struct SyncErrorEntry {
    kind: SyncErrorKind,
    detail: String,
}

static SYNC_ERRORS: LazyLock<Mutex<HashMap<i64, SyncErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_SYNC_ERROR_HANDLE: AtomicI64 = AtomicI64::new(1);
static SYNC_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "SyncError",
        std::mem::size_of::<i64>(),
        Some(drop_sync_error as extern "C" fn(*mut c_void)),
    )
});

extern "C" fn drop_sync_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        SYNC_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn sync_error_value(kind: StdErrorKind, detail: String) -> Value {
    let handle = NEXT_SYNC_ERROR_HANDLE.fetch_add(1, Ordering::Relaxed);
    SYNC_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            SyncErrorEntry {
                kind: SyncErrorKind::from_std_kind(kind),
                detail,
            },
        );
    let value = alloc_object(*SYNC_ERROR_TYPE_ID);
    if value.is_null() {
        SYNC_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate sync error".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        SYNC_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
        return Value::String("could not allocate sync error".to_string());
    }
    unsafe { *ptr.cast::<i64>() = handle };
    let cloned = unsafe { (&*value).clone() };
    unsafe { mux_rc_dec(value) };
    cloned
}

pub(crate) fn sync_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

pub(crate) fn sync_result_err(detail: impl Into<String>) -> *mut Value {
    mux_rc_alloc(Value::Result(Err(Box::new(sync_error_value(
        StdErrorKind::State,
        detail.into(),
    )))))
}

fn sync_error_handle(error: *const Value) -> Option<i64> {
    if error.is_null() || unsafe { get_object_type_id(error) } != *SYNC_ERROR_TYPE_ID {
        return None;
    }
    let ptr = unsafe { get_object_ptr(error) };
    (!ptr.is_null()).then(|| unsafe { *ptr.cast::<i64>() })
}

fn sync_error_field_value(error: *const Value, get: fn(SyncErrorEntry) -> Value) -> Value {
    let value = sync_error_handle(error).and_then(|handle| {
        SYNC_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    value.map_or_else(
        || Value::String("invalid SyncError handle".to_string()),
        get,
    )
}

fn sync_error_text(error: *const Value, decorated: bool) -> String {
    let value = sync_error_handle(error).and_then(|handle| {
        SYNC_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle)
            .cloned()
    });
    let Some(entry) = value else {
        return "invalid SyncError handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail
    }
}

#[unsafe(no_mangle)]
/// Create a typed synchronization error from a displayable message.
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_sync_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid synchronization error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(sync_error_value(StdErrorKind::State, detail))
}

macro_rules! sync_error_string_getter {
    ($name:ident, $get:expr) => {
        /// Read one string field from a typed synchronization error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(sync_error_field_value(error, $get))
        }
    };
}

sync_error_string_getter!(mux_sync_error_kind, |entry| entry.kind.value());
sync_error_string_getter!(mux_sync_error_detail, |entry| Value::String(entry.detail));

#[unsafe(no_mangle)]
/// Return the detail message from a typed synchronization error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_sync_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(sync_error_text(error, false)))
}

#[unsafe(no_mangle)]
/// Return a decorated string representation of a typed synchronization error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
pub unsafe extern "C" fn mux_sync_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(sync_error_text(error, true)))
}

/// Create a typed UUID error from a displayable message.
///
/// # Safety
/// `message` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_error_from_message(message: *const Value) -> *mut Value {
    let detail = message.as_ref().map_or_else(
        || "invalid UUID error detail".to_string(),
        ToString::to_string,
    );
    mux_rc_alloc(uuid_error_value(StdErrorKind::Parse, detail))
}

macro_rules! uuid_error_string_getter {
    ($name:ident, $get:expr) => {
        /// Read one string field from a typed UUID error.
        ///
        /// # Safety
        /// `error` must be null or point to a live Mux `Value` for the duration.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            mux_rc_alloc(uuid_error_field_value(error, $get))
        }
    };
}

uuid_error_string_getter!(mux_uuid_error_detail, |entry| Value::String(entry.detail));

/// Read the typed category from a UUID error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_error_kind(error: *const Value) -> *mut Value {
    mux_rc_alloc(uuid_error_field_value(error, |entry| {
        Value::Opaque(
            (entry.kind as i32)
                .to_ne_bytes()
                .to_vec()
                .into_boxed_slice(),
        )
    }))
}

/// Return the detail message from a typed UUID error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(uuid_error_text(error, false)))
}

/// Return a decorated string representation of a typed UUID error.
///
/// # Safety
/// `error` must be null or point to a live Mux `Value` for the duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_uuid_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(uuid_error_text(error, true)))
}

/// No-op: result values are now *mut Value managed by reference counting.
#[unsafe(no_mangle)]
pub extern "C" fn mux_free_result(_val: *mut Value) {}

// Value extraction functions - don't take ownership
#[unsafe(no_mangle)]
/// Extract an integer, returning zero for null or a value of another type.
///
/// # Safety
/// A null `val` is accepted. Otherwise it must point to a live, initialized
/// `Value` readable for the duration of this call.
pub unsafe extern "C" fn mux_value_get_int(val: *const Value) -> i64 {
    if val.is_null() {
        return 0;
    }
    unsafe {
        match &*val {
            Value::Int(i) => *i,
            _ => 0, // Return default value instead of panicking
        }
    }
}

#[unsafe(no_mangle)]
/// Extract a float, returning zero for null or a value of another type.
///
/// # Safety
/// A null `val` is accepted. Otherwise it must point to a live, initialized
/// `Value` readable for the duration of this call.
pub unsafe extern "C" fn mux_value_get_float(val: *const Value) -> f64 {
    if val.is_null() {
        return 0.0;
    }
    unsafe {
        match &*val {
            Value::Float(f) => f.into_inner(),
            _ => 0.0, // Return default value instead of panicking
        }
    }
}

#[unsafe(no_mangle)]
/// Extract a boolean as `0` or `1`, returning zero for null or another type.
///
/// # Safety
/// A null `val` is accepted. Otherwise it must point to a live, initialized
/// `Value` readable for the duration of this call.
pub unsafe extern "C" fn mux_value_get_bool(val: *const Value) -> i32 {
    if val.is_null() {
        return 0;
    }
    unsafe {
        match &*val {
            Value::Bool(b) => i32::from(*b),
            _ => 0,
        }
    }
}

#[unsafe(no_mangle)]
/// Return a value's type tag, or `-1` for null.
///
/// # Safety
/// A null `val` is accepted. Otherwise it must point to a live, initialized
/// `Value` readable for the duration of this call.
pub unsafe extern "C" fn mux_value_get_type_tag(val: *const Value) -> i32 {
    if val.is_null() {
        return -1;
    }
    let value = unsafe { &*val };
    value.type_tag()
}

#[unsafe(no_mangle)]
/// Compare two values for equality; null pointers compare equal only to null.
/// Returns `1` when equal and `0` otherwise.
///
/// # Safety
/// Null pointers are accepted. Every non-null pointer must point to a live,
/// initialized `Value` readable for the duration of this call.
pub unsafe extern "C" fn mux_value_equal(a: *const Value, b: *const Value) -> i32 {
    if a.is_null() || b.is_null() {
        return i32::from(a == b);
    }
    unsafe { i32::from(*a == *b) }
}

#[unsafe(no_mangle)]
/// Compare two values, ordering null before non-null.
/// Returns `-1`, `0`, or `1`, like `Ord::cmp`. Used by the compiler's enum
/// comparison glue to order payload fields by value (issue #309).
///
/// # Safety
/// Null pointers are accepted. Every non-null pointer must point to a live,
/// initialized `Value` readable for the duration of this call.
pub unsafe extern "C" fn mux_value_compare(a: *const Value, b: *const Value) -> i32 {
    match (a.is_null(), b.is_null()) {
        (true, true) => 0,
        (true, false) => -1,
        (false, true) => 1,
        (false, false) => match unsafe { (*a).cmp(&*b) } {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        },
    }
}

#[unsafe(no_mangle)]
/// Compare two values for inequality; null pointers compare equal only to null.
/// Returns `1` when unequal and `0` otherwise.
///
/// # Safety
/// Null pointers are accepted. Every non-null pointer must point to a live,
/// initialized `Value` readable for the duration of this call.
pub unsafe extern "C" fn mux_value_not_equal(a: *const Value, b: *const Value) -> i32 {
    i32::from(unsafe { mux_value_equal(a, b) } != 1)
}

#[unsafe(no_mangle)]
/// Copy an enum payload into an opaque managed value.
///
/// # Safety
/// `ptr` must be non-null and point to at least `size` readable bytes. The
/// bytes are copied before this function returns; the caller retains ownership
/// of the source allocation.
pub unsafe extern "C" fn mux_box_enum(ptr: *mut u8, size: usize) -> *mut Value {
    let slice = unsafe { std::slice::from_raw_parts(ptr, size) };
    let boxed: Box<[u8]> = slice.to_vec().into_boxed_slice();
    mux_rc_alloc(Value::Opaque(boxed))
}

#[unsafe(no_mangle)]
/// Return a borrowed pointer to an opaque or boxed-enum payload.
/// The pointer aliases the buffer owned by `val` and is valid only while `val`
/// is alive. It must not be written through; the `*mut` return type is a C-ABI
/// convention. Generated code loads the enum struct immediately, before
/// releasing `val`.
///
/// # Safety
/// A null `val` is accepted and returns null. Otherwise `val` must point to a
/// live, initialized `Value`. The returned pointer is borrowed and must not be
/// used after `val` is released or moved, nor written through.
pub unsafe extern "C" fn mux_value_unbox_enum(val: *mut Value) -> *mut u8 {
    if val.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        match &*val {
            Value::Opaque(data) => data.as_ptr().cast_mut(),
            // A payload-carrying enum is a managed BoxedEnum rather than a raw
            // Opaque, but its inline struct bytes are read the same way (from an
            // 8-aligned backing store).
            Value::BoxedEnum(be) => be.as_ptr().cast_mut(),
            _ => std::ptr::null_mut(),
        }
    }
}

#[unsafe(no_mangle)]
/// Copy and deep-clone an enum payload into a managed boxed-enum value.
/// The `size` bytes at `ptr` are copied and then deep-cloned via `clone_glue`,
/// so the returned value owns payloads independent of the source (which the
/// caller still releases). `clone_glue`, `drop_glue`, `cmp_glue`, and
/// `hash_glue` are retained by the returned value and must remain valid for
/// its entire lifetime.
///
/// # Safety
/// `ptr` must be non-null and point to at least `size` readable bytes laid out
/// as the enum expected by `clone_glue`, `drop_glue`, `cmp_glue`, and
/// `hash_glue`. Each callback must be valid for that layout and callable for
/// the duration of this call and for every operation on the returned value.
/// The source remains caller-owned.
pub unsafe extern "C" fn mux_box_enum_managed(
    ptr: *mut u8,
    size: usize,
    clone_glue: crate::EnumGlueFn,
    drop_glue: crate::EnumGlueFn,
    cmp_glue: crate::EnumCmpFn,
    hash_glue: crate::EnumHashFn,
) -> *mut Value {
    let slice = unsafe { std::slice::from_raw_parts(ptr, size) };
    let mut boxed = crate::BoxedEnum::from_bytes(slice, clone_glue, drop_glue, cmp_glue, hash_glue);
    // The byte copy still aliases the source's payloads; deep-clone them so the
    // boxed value is independent of the source.
    (clone_glue)(boxed.as_mut_ptr());
    mux_rc_alloc(Value::BoxedEnum(boxed))
}

/// Hash of any `Value`, for the compiler-emitted enum hash glue to use on a
/// pointer payload (a string, a collection, a nested boxed enum).
///
/// Consistent with `mux_value_compare` because `Value`'s `Hash` and `Eq` impls
/// agree with each other, which is what lets the enum glue keep its own hash
/// agreeing with `cmp_glue`.
///
/// # Safety
/// `value` must be null or a valid pointer to a ref-counted `Value`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_value_hash(value: *const Value) -> u64 {
    use std::hash::{Hash, Hasher};
    if value.is_null() {
        return 0;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    unsafe { (*value).hash(&mut hasher) };
    hasher.finish()
}

// Proper Value cleanup function
