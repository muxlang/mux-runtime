//! Synchronous structured logging.
//!
//! Logger values are shared resource handles. Logging writes one atomic text
//! line to stderr, so a logger never starts a background worker or owns a
//! queue. The default logger can be replaced explicitly by the caller.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::mux_rc_alloc;
use crate::std::StdErrorKind;
use crate::Value;
use rust_std::collections::HashMap;
use rust_std::ffi::{c_char, c_void, CStr};
use rust_std::io::Write;
use rust_std::sync::{LazyLock, Mutex};
use rust_std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
struct LoggerEntry {
    level: LogLevel,
    name: String,
    fields: Vec<(String, String)>,
    writer: Option<i64>,
    refs: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warn" | "warning" => Ok(Self::Warn),
            "error" => Ok(Self::Error),
            _ => Err("log level must be trace, debug, info, warn, or error".to_string()),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

static LOGGERS: LazyLock<Mutex<HashMap<i64, LoggerEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static DEFAULT_LOGGER: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(0));
static OUTPUT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static LOGGER_TYPE_ID: LazyLock<crate::TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Logger",
        size_of::<i64>(),
        Some(drop_logger as extern "C" fn(*mut c_void)),
        Some(copy_logger as extern "C" fn(*mut c_void, *mut c_void)),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> rust_std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(rust_std::sync::PoisonError::into_inner)
}

fn next_handle() -> i64 {
    let mut next = lock(&NEXT_HANDLE);
    let handle = *next;
    *next = next.checked_add(1).unwrap_or(1);
    handle
}

fn result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn result_err(message: impl Into<String>) -> *mut Value {
    crate::std::log_result_err(message.into())
}

fn logger_value(handle: i64) -> *mut Value {
    let value = alloc_object(*LOGGER_TYPE_ID);
    if value.is_null() {
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { crate::refcount::mux_rc_dec(value) };
        return rust_std::ptr::null_mut();
    }
    unsafe { *ptr.cast::<i64>() = handle };
    value
}

fn create_logger() -> *mut Value {
    let handle = next_handle();
    lock(&LOGGERS).insert(
        handle,
        LoggerEntry {
            level: LogLevel::Info,
            name: String::new(),
            fields: Vec::new(),
            writer: None,
            refs: 1,
        },
    );
    let value = logger_value(handle);
    if value.is_null() {
        lock(&LOGGERS).remove(&handle);
    }
    value
}

unsafe fn handle(value: *const Value) -> Result<i64, String> {
    if value.is_null() {
        return Err("Logger value is null".to_string());
    }
    if unsafe { get_object_type_id(value) } != *LOGGER_TYPE_ID {
        return Err("value is not a Logger".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("value is not a Logger".to_string());
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle <= 0 || !lock(&LOGGERS).contains_key(&handle) {
        return Err("invalid Logger handle".to_string());
    }
    Ok(handle)
}

fn value_string(value: *const Value, label: &str) -> Result<String, String> {
    if value.is_null() {
        return Err(format!("{label} must be a string"));
    }
    match unsafe { &*value } {
        Value::String(value) => Ok(value.clone()),
        _ => Err(format!("{label} must be a string")),
    }
}

fn c_string(value: *const c_char, label: &str) -> Result<String, String> {
    if value.is_null() {
        return Err(format!("{label} must be a string"));
    }
    Ok(unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned())
}

fn release_logger(handle: i64) {
    let mut loggers = lock(&LOGGERS);
    let mut writer = None;
    let remove = loggers.get_mut(&handle).is_some_and(|entry| {
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs == 0 {
            writer = entry.writer;
            true
        } else {
            false
        }
    });
    if remove {
        loggers.remove(&handle);
        drop(loggers);
        let mut default = lock(&DEFAULT_LOGGER);
        if *default == handle {
            *default = 0;
        }
        if let Some(writer) = writer {
            crate::stream::release_writer(writer);
        }
    }
}

fn retain_logger(handle: i64) -> Result<(), String> {
    let mut loggers = lock(&LOGGERS);
    let entry = loggers
        .get_mut(&handle)
        .ok_or_else(|| "invalid Logger handle".to_string())?;
    entry.refs = entry
        .refs
        .checked_add(1)
        .ok_or_else(|| "Logger reference count overflow".to_string())?;
    Ok(())
}

fn copy_logger_handle(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let handle = unsafe { *source.cast::<i64>() };
    let mut loggers = lock(&LOGGERS);
    if let Some(entry) = loggers.get_mut(&handle) {
        entry.refs += 1;
        unsafe { *dest.cast::<i64>() = handle };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn copy_logger(source: *mut c_void, dest: *mut c_void) {
    copy_logger_handle(source, dest);
}

extern "C" fn drop_logger(ptr: *mut c_void) {
    if !ptr.is_null() {
        release_logger(unsafe { *ptr.cast::<i64>() });
    }
}

fn default_handle() -> Result<i64, String> {
    let handle = *lock(&DEFAULT_LOGGER);
    if handle > 0 && lock(&LOGGERS).contains_key(&handle) {
        return Ok(handle);
    }
    drop(lock(&DEFAULT_LOGGER));
    let value = create_logger();
    if value.is_null() {
        return Err("failed to allocate default Logger".to_string());
    }
    let handle = unsafe { *get_object_ptr(value).cast::<i64>() };
    retain_logger(handle)?;
    unsafe { crate::refcount::mux_rc_dec(value) };
    *lock(&DEFAULT_LOGGER) = handle;
    Ok(handle)
}

fn write_line(entry: &LoggerEntry, level: LogLevel, message: &str) -> Result<(), String> {
    if level < entry.level {
        return Ok(());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("clock error: {error}"))?;
    let mut line = format!(
        "{}.{:03} {}",
        now.as_secs(),
        now.subsec_millis(),
        level.label()
    );
    if !entry.name.is_empty() {
        line.push_str(" name=");
        line.push_str(&entry.name);
    }
    for (key, value) in &entry.fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(value);
    }
    line.push_str(" message=");
    line.push_str(message);
    line.push('\n');
    let _guard = lock(&OUTPUT_LOCK);
    if let Some(writer) = entry.writer {
        return crate::stream::write_writer(writer, line.as_bytes())
            .map_err(|error| format!("log writer failed: {error}"));
    }
    rust_std::io::stderr()
        .write_all(line.as_bytes())
        .map_err(|error| format!("log write failed: {error}"))
}

fn emit(handle: i64, level: LogLevel, message: &str) -> Result<(), String> {
    let entry = lock(&LOGGERS)
        .get(&handle)
        .cloned()
        .ok_or_else(|| "invalid Logger handle".to_string())?;
    write_line(&entry, level, message)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_log_logger_new() -> *mut Value {
    create_logger()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_log_default() -> *mut Value {
    match default_handle().and_then(|handle| {
        retain_logger(handle)?;
        let value = logger_value(handle);
        if value.is_null() {
            release_logger(handle);
            return Err("failed to allocate Logger handle".to_string());
        }
        Ok(value)
    }) {
        Ok(value) => value,
        Err(_) => mux_rc_alloc(Value::Unit),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_set_default(logger: *const Value) -> *mut Value {
    let result: Result<(), String> = (|| {
        let handle = unsafe { handle(logger) }?;
        retain_logger(handle)?;
        let previous = *lock(&DEFAULT_LOGGER);
        *lock(&DEFAULT_LOGGER) = handle;
        if previous > 0 {
            release_logger(previous);
        }
        Ok(())
    })();
    match result {
        Ok(()) => result_ok(Value::Unit),
        Err(error) => result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_logger_set_level(
    logger: *const Value,
    level: *const Value,
) -> *mut Value {
    let result: Result<(), String> = (|| {
        let handle = unsafe { handle(logger) }?;
        let level = LogLevel::parse(&value_string(level, "log level")?)?;
        lock(&LOGGERS)
            .get_mut(&handle)
            .ok_or_else(|| "invalid Logger handle".to_string())?
            .level = level;
        Ok(())
    })();
    match result {
        Ok(()) => result_ok(Value::Unit),
        Err(error) => crate::std::log_result_err_kind(StdErrorKind::Invalid, error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_logger_set_name(
    logger: *const Value,
    name: *const Value,
) -> *mut Value {
    let result: Result<(), String> = (|| {
        let handle = unsafe { handle(logger) }?;
        let name = value_string(name, "logger name")?;
        lock(&LOGGERS)
            .get_mut(&handle)
            .ok_or_else(|| "invalid Logger handle".to_string())?
            .name = name;
        Ok(())
    })();
    match result {
        Ok(()) => result_ok(Value::Unit),
        Err(error) => result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_logger_field(
    logger: *const Value,
    key: *const Value,
    value: *const Value,
) -> *mut Value {
    let result: Result<(), String> = (|| {
        let handle = unsafe { handle(logger) }?;
        let key = value_string(key, "field name")?;
        if key.is_empty() || key.chars().any(char::is_whitespace) {
            return Err("field name must be non-empty and contain no whitespace".to_string());
        }
        let value = value_string(value, "field value")?;
        let mut loggers = lock(&LOGGERS);
        let entry = loggers
            .get_mut(&handle)
            .ok_or_else(|| "invalid Logger handle".to_string())?;
        if let Some(existing) = entry.fields.iter_mut().find(|(name, _)| name == &key) {
            existing.1 = value;
        } else {
            entry.fields.push((key, value));
            entry.fields.sort_by(|left, right| left.0.cmp(&right.0));
        }
        Ok(())
    })();
    match result {
        Ok(()) => result_ok(Value::Unit),
        Err(error) => crate::std::log_result_err_kind(StdErrorKind::Invalid, error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_logger_set_writer(
    logger: *const Value,
    writer: *const Value,
) -> *mut Value {
    let result: Result<(), String> = (|| {
        let handle = unsafe { handle(logger) }?;
        let writer_handle = crate::stream::writer_handle(writer)?;
        crate::stream::retain_writer(writer_handle)?;
        let previous = {
            let mut loggers = lock(&LOGGERS);
            let Some(entry) = loggers.get_mut(&handle) else {
                crate::stream::release_writer(writer_handle);
                return Err("invalid Logger handle".to_string());
            };
            let previous = entry.writer;
            entry.writer = Some(writer_handle);
            previous
        };
        if let Some(previous) = previous {
            crate::stream::release_writer(previous);
        }
        Ok(())
    })();
    match result {
        Ok(()) => result_ok(Value::Unit),
        Err(error) => result_err(error),
    }
}

unsafe fn logger_message(
    logger: *const Value,
    level: LogLevel,
    message: *const Value,
) -> *mut Value {
    let result = (|| {
        let handle = unsafe { handle(logger) }?;
        let message = value_string(message, "log message")?;
        emit(handle, level, &message)
    })();
    match result {
        Ok(()) => result_ok(Value::Unit),
        Err(error) => result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_logger_trace(
    logger: *const Value,
    message: *const Value,
) -> *mut Value {
    logger_message(logger, LogLevel::Trace, message)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_logger_debug(
    logger: *const Value,
    message: *const Value,
) -> *mut Value {
    logger_message(logger, LogLevel::Debug, message)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_logger_info(
    logger: *const Value,
    message: *const Value,
) -> *mut Value {
    logger_message(logger, LogLevel::Info, message)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_logger_warn(
    logger: *const Value,
    message: *const Value,
) -> *mut Value {
    logger_message(logger, LogLevel::Warn, message)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_logger_error(
    logger: *const Value,
    message: *const Value,
) -> *mut Value {
    logger_message(logger, LogLevel::Error, message)
}

unsafe fn default_message(level: LogLevel, message: *const c_char) -> *mut Value {
    let result = (|| {
        let handle = default_handle()?;
        let message = c_string(message, "log message")?;
        emit(handle, level, &message)
    })();
    match result {
        Ok(()) => result_ok(Value::Unit),
        Err(error) => result_err(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_trace(message: *const c_char) -> *mut Value {
    default_message(LogLevel::Trace, message)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_debug(message: *const c_char) -> *mut Value {
    default_message(LogLevel::Debug, message)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_info(message: *const c_char) -> *mut Value {
    default_message(LogLevel::Info, message)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_warn(message: *const c_char) -> *mut Value {
    default_message(LogLevel::Warn, message)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_log_error(message: *const c_char) -> *mut Value {
    default_message(LogLevel::Error, message)
}
