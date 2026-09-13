//! Runtime support for the built-in `bytes` value.
//!
//! `bytes` stores raw octets contiguously instead of boxing every element as a
//! `Value::Int`. The compiler passes the owning `Value` pointer directly to
//! these functions; mutating operations therefore update the value in place.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec};
use crate::std::bytes_result_err;
use crate::{TypeId, Value};
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::sync::{LazyLock, Mutex};

struct CursorEntry {
    bytes: Vec<u8>,
    position: usize,
    names: usize,
}

static CURSORS: LazyLock<Mutex<HashMap<i64, CursorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_CURSOR: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static CURSOR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "BytesCursor",
        std::mem::size_of::<i64>(),
        Some(drop_cursor as extern "C" fn(*mut std::ffi::c_void)),
        Some(copy_cursor as extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void)),
    )
});

fn error(message: impl Into<String>) -> *mut Value {
    bytes_result_err(message)
}

fn ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn cursor_error(message: impl Into<String>) -> *mut Value {
    error(message)
}

fn cursor_lock() -> std::sync::MutexGuard<'static, HashMap<i64, CursorEntry>> {
    CURSORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_cursor() -> i64 {
    let mut next = NEXT_CURSOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    id
}

fn cursor_object(id: i64) -> *mut Value {
    let ptr = alloc_object(*CURSOR_TYPE_ID);
    if ptr.is_null() {
        return ptr;
    }
    let data = unsafe { get_object_ptr(ptr) };
    if data.is_null() {
        unsafe { mux_rc_dec(ptr) };
        return std::ptr::null_mut();
    }
    unsafe { *data.cast::<i64>() = id };
    ptr
}

fn cursor_value(id: i64) -> Result<Value, String> {
    let ptr = cursor_object(id);
    if ptr.is_null() {
        return Err("could not allocate bytes cursor".to_string());
    }
    let value = unsafe { (*ptr).clone() };
    unsafe { mux_rc_dec(ptr) };
    Ok(value)
}

fn cursor_id(value: *const Value) -> Result<i64, String> {
    if value.is_null() {
        return Err("cursor is null".to_string());
    }
    if unsafe { get_object_type_id(value) } != *CURSOR_TYPE_ID {
        return Err("expected bytes cursor".to_string());
    }
    let data = unsafe { get_object_ptr(value) };
    if data.is_null() {
        return Err("invalid bytes cursor".to_string());
    }
    let id = unsafe { *data.cast::<i64>() };
    if id <= 0 {
        return Err("invalid bytes cursor".to_string());
    }
    Ok(id)
}

fn release_cursor(id: i64) {
    let mut cursors = cursor_lock();
    let remove = cursors.get_mut(&id).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
    if remove {
        cursors.remove(&id);
    }
}

extern "C" fn drop_cursor(data: *mut std::ffi::c_void) {
    if !data.is_null() {
        release_cursor(unsafe { *data.cast::<i64>() });
    }
}

extern "C" fn copy_cursor(source: *mut std::ffi::c_void, destination: *mut std::ffi::c_void) {
    if source.is_null() || destination.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    let mut cursors = cursor_lock();
    let copied = if let Some(entry) = cursors.get_mut(&id) {
        entry.names += 1;
        id
    } else {
        0
    };
    unsafe { *destination.cast::<i64>() = copied };
}

fn cursor_range(entry: &CursorEntry, width: usize) -> Result<std::ops::Range<usize>, String> {
    let end = entry
        .position
        .checked_add(width)
        .ok_or_else(|| "cursor position is too large".to_string())?;
    if end > entry.bytes.len() {
        return Err("not enough bytes remaining".to_string());
    }
    Ok(entry.position..end)
}

fn cursor_width(width: i64) -> Result<usize, String> {
    let width = usize::try_from(width).map_err(|_| "width must be positive".to_string())?;
    if !(1..=8).contains(&width) {
        return Err("integer width must be between 1 and 8 bytes".to_string());
    }
    Ok(width)
}

fn cursor_read_uint(
    entry: &mut CursorEntry,
    width: i64,
    little_endian: bool,
) -> Result<i64, String> {
    let width = cursor_width(width)?;
    let range = cursor_range(entry, width)?;
    let mut value = 0u64;
    for (position, byte) in entry.bytes[range.clone()].iter().enumerate() {
        let shift = if little_endian {
            position
        } else {
            width - position - 1
        } * 8;
        value |= u64::from(*byte) << shift;
    }
    let value = i64::try_from(value)
        .map_err(|_| "decoded unsigned integer does not fit in int".to_string())?;
    entry.position = range.end;
    Ok(value)
}

fn cursor_write_uint(
    entry: &mut CursorEntry,
    number: i64,
    width: i64,
    little_endian: bool,
) -> Result<(), String> {
    if number < 0 {
        return Err("unsigned integer value must not be negative".to_string());
    }
    let width = cursor_width(width)?;
    let number = u64::try_from(number).map_err(|_| "value is out of range".to_string())?;
    if width < 8 && number >= (1u64 << (width * 8)) {
        return Err("integer value does not fit requested width".to_string());
    }
    let end = entry
        .position
        .checked_add(width)
        .ok_or_else(|| "cursor position is too large".to_string())?;
    if end > entry.bytes.len() {
        entry.bytes.resize(end, 0);
    }
    for position in 0..width {
        let shift = if little_endian {
            position
        } else {
            width - position - 1
        } * 8;
        entry.bytes[entry.position + position] = (number >> shift) as u8;
    }
    entry.position = end;
    Ok(())
}

fn byte_value(value: *const Value) -> Result<u8, String> {
    if value.is_null() {
        return Err("null byte value".to_string());
    }
    match unsafe { &*value } {
        Value::Int(number) if (0..=255).contains(number) => Ok(*number as u8),
        _ => Err("expected a byte value".to_string()),
    }
}

fn bytes_ref<'a>(value: *const Value) -> Option<&'a Vec<u8>> {
    if value.is_null() {
        return None;
    }
    // The returned reference is only used during the calling FFI operation;
    // the lifetime is tied to the raw pointer by the caller's ownership.
    unsafe { (&*value).as_bytes() }
}

fn bytes_mut<'a>(value: *mut Value) -> Option<&'a mut Vec<u8>> {
    if value.is_null() {
        return None;
    }
    unsafe { (&mut *value).as_bytes_mut() }
}

impl Value {
    fn as_bytes(&self) -> Option<&Vec<u8>> {
        match self {
            Value::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    fn as_bytes_mut(&mut self) -> Option<&mut Vec<u8>> {
        match self {
            Value::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_bytes_new() -> *mut Value {
    mux_rc_alloc(Value::Bytes(Vec::new()))
}

#[unsafe(no_mangle)]
/// Construct bytes from an arbitrary octet buffer.
pub unsafe extern "C" fn mux_bytes_from_data(data: *const u8, length: i64) -> *mut Value {
    if data.is_null() || length <= 0 {
        return mux_rc_alloc(Value::Bytes(Vec::new()));
    }
    let Ok(length) = usize::try_from(length) else {
        return mux_rc_alloc(Value::Bytes(Vec::new()));
    };
    let slice = unsafe { std::slice::from_raw_parts(data, length) };
    mux_rc_alloc(Value::Bytes(slice.to_vec()))
}

#[unsafe(no_mangle)]
/// Explicitly convert a general-purpose list of integer octets to `bytes`.
/// This is an opt-in collection conversion, not a compatibility representation.
///
/// # Safety
/// `list` may be null; otherwise it must point to a live `Value`.
pub unsafe extern "C" fn mux_bytes_from_list(list: *const Value) -> *mut Value {
    let Some(Value::List(values)) = list.as_ref() else {
        return error("expected a list of integer octets");
    };
    let mut bytes = Vec::with_capacity(values.len());
    for value in values {
        match value {
            Value::Int(number) if (0..=255).contains(number) => bytes.push(*number as u8),
            _ => return error("list contains a value outside byte range 0..255"),
        }
    }
    ok(Value::Bytes(bytes))
}

#[unsafe(no_mangle)]
/// Explicitly convert `bytes` to a `list<int>` containing the same octets.
///
/// # Safety
/// `value` may be null; otherwise it must point to a live `Value`.
pub unsafe extern "C" fn mux_bytes_to_list(value: *const Value) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return crate::refcount::mux_rc_alloc(Value::List(Vec::new()));
    };
    crate::refcount::mux_rc_alloc(Value::List(
        bytes
            .iter()
            .map(|byte| Value::Int(i64::from(*byte)))
            .collect(),
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_length(value: *const Value) -> i64 {
    bytes_ref(value).map_or(0, |bytes| bytes.len() as i64)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_is_empty(value: *const Value) -> bool {
    bytes_ref(value).is_none_or(Vec::is_empty)
}

#[unsafe(no_mangle)]
/// Return an optional byte as an integer Value. Negative indices wrap from the
/// end, matching list indexing; out-of-range indices return `None`.
pub unsafe extern "C" fn mux_bytes_get(value: *const Value, index: i64) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return mux_rc_alloc(Value::Optional(None));
    };
    let len = bytes.len() as i64;
    let normalized = if index < 0 { len + index } else { index };
    match bytes.get(normalized as usize) {
        Some(byte) if normalized >= 0 && normalized < len => mux_rc_alloc(Value::Optional(Some(
            Box::new(Value::Int(i64::from(*byte))),
        ))),
        _ => mux_rc_alloc(Value::Optional(None)),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_push_back(value: *mut Value, byte: *const Value) {
    let Ok(byte) = byte_value(byte) else { return };
    if let Some(bytes) = bytes_mut(value) {
        bytes.push(byte);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_push_front(value: *mut Value, byte: *const Value) {
    let Ok(byte) = byte_value(byte) else { return };
    if let Some(bytes) = bytes_mut(value) {
        bytes.insert(0, byte);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_pop_back(value: *mut Value) -> *mut Value {
    let popped = bytes_mut(value).and_then(Vec::pop);
    mux_rc_alloc(Value::Optional(
        popped.map(|byte| Box::new(Value::Int(i64::from(byte)))),
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_pop_front(value: *mut Value) -> *mut Value {
    let popped = bytes_mut(value).and_then(|bytes| {
        if bytes.is_empty() {
            None
        } else {
            Some(bytes.remove(0))
        }
    });
    mux_rc_alloc(Value::Optional(
        popped.map(|byte| Box::new(Value::Int(i64::from(byte)))),
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_clear(value: *mut Value) {
    if let Some(bytes) = bytes_mut(value) {
        bytes.clear();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_reserve(value: *mut Value, additional: i64) {
    let Ok(additional) = usize::try_from(additional) else {
        return;
    };
    if let Some(bytes) = bytes_mut(value) {
        let _ = bytes.try_reserve(additional);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_truncate(value: *mut Value, length: i64) {
    if let Some(bytes) = bytes_mut(value) {
        bytes.truncate(length.max(0) as usize);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_resize(value: *mut Value, length: i64, fill: *const Value) {
    let Ok(fill) = byte_value(fill) else { return };
    let Ok(length) = usize::try_from(length) else {
        return;
    };
    if let Some(bytes) = bytes_mut(value) {
        if length > bytes.len() && bytes.try_reserve(length - bytes.len()).is_err() {
            return;
        }
        bytes.resize(length, fill);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_fill(value: *mut Value, fill: *const Value) {
    let Ok(fill) = byte_value(fill) else { return };
    if let Some(bytes) = bytes_mut(value) {
        bytes.fill(fill);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_extend(value: *mut Value, other: *const Value) {
    let Some(other) = bytes_ref(other).cloned() else {
        return;
    };
    if let Some(bytes) = bytes_mut(value) {
        bytes.extend(other);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_insert(value: *mut Value, index: i64, byte: *const Value) {
    let Ok(byte) = byte_value(byte) else { return };
    if let Some(bytes) = bytes_mut(value) {
        let len = bytes.len() as i64;
        let index = if index < 0 { len + index } else { index }.clamp(0, len) as usize;
        bytes.insert(index, byte);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_remove(value: *mut Value, index: i64) -> *mut Value {
    let Some(bytes) = bytes_mut(value) else {
        return mux_rc_alloc(Value::Optional(None));
    };
    let len = bytes.len() as i64;
    let index = if index < 0 { len + index } else { index };
    let removed = (index >= 0 && index < len).then(|| bytes.remove(index as usize));
    mux_rc_alloc(Value::Optional(
        removed.map(|byte| Box::new(Value::Int(i64::from(byte)))),
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_copy_within(
    value: *mut Value,
    destination: i64,
    source: i64,
    length: i64,
) {
    let Some(bytes) = bytes_mut(value) else {
        return;
    };
    if length <= 0 {
        return;
    }
    let len = bytes.len() as i64;
    let source = if source < 0 { len + source } else { source }.clamp(0, len);
    let destination = if destination < 0 {
        len + destination
    } else {
        destination
    }
    .clamp(0, len);
    let count = length.min(len - source).min(len - destination).max(0) as usize;
    if count == 0 {
        return;
    }
    bytes.copy_within(
        source as usize..source as usize + count,
        destination as usize,
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_slice(value: *const Value, start: i64, end: i64) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return mux_rc_alloc(Value::Bytes(Vec::new()));
    };
    let len = bytes.len() as i64;
    let normalize = |index: i64, default: i64| {
        let index = if index == i64::MAX { default } else { index };
        (if index < 0 { len + index } else { index }).clamp(0, len)
    };
    let start = normalize(start, 0);
    let end = normalize(end, len);
    mux_rc_alloc(Value::Bytes(if start <= end {
        bytes[start as usize..end as usize].to_vec()
    } else {
        Vec::new()
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_to_utf8(value: *const Value) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return error("expected bytes");
    };
    match String::from_utf8(bytes.clone()) {
        Ok(text) => ok(Value::String(text)),
        Err(_) => error("bytes are not valid UTF-8"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_to_utf8_lossy(value: *const Value) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return mux_rc_alloc(Value::String(String::new()));
    };
    mux_rc_alloc(Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

#[unsafe(no_mangle)]
/// Construct bytes from a UTF-8 string's raw representation.
pub unsafe extern "C" fn mux_bytes_from_utf8(value: *const Value) -> *mut Value {
    if value.is_null() {
        return error("expected string");
    }
    let Value::String(text) = (unsafe { &*value }) else {
        return error("expected string");
    };
    mux_rc_alloc(Value::Bytes(text.as_bytes().to_vec()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_contains(value: *const Value, byte: *const Value) -> bool {
    let Ok(byte) = byte_value(byte) else {
        return false;
    };
    bytes_ref(value).is_some_and(|bytes| bytes.contains(&byte))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_find(value: *const Value, byte: *const Value) -> *mut Value {
    let Ok(byte) = byte_value(byte) else {
        return mux_rc_alloc(Value::Optional(None));
    };
    let found = bytes_ref(value).and_then(|bytes| bytes.iter().position(|item| *item == byte));
    mux_rc_alloc(Value::Optional(
        found.map(|index| Box::new(Value::Int(index as i64))),
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_to_cstr(value: *const Value) -> *mut c_char {
    let Some(bytes) = bytes_ref(value) else {
        return CString::default().into_raw();
    };
    CString::new(bytes.as_slice())
        .unwrap_or_default()
        .into_raw()
}

#[unsafe(no_mangle)]
/// Build bytes from a NUL-terminated string without interpreting its encoding.
pub unsafe extern "C" fn mux_bytes_from_cstr(value: *const c_char) -> *mut Value {
    if value.is_null() {
        return error("null input");
    }
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes().to_vec();
    mux_rc_alloc(Value::Bytes(bytes))
}

fn format_bytes(bytes: &[u8], radix: i64, width: i64) -> Result<String, String> {
    const MAX_FORMAT_WIDTH: i64 = 64;
    let (radix, minimum_width) = match radix {
        2 => (2, 8),
        8 => (8, 3),
        10 => (10, 3),
        16 => (16, 2),
        _ => return Err("radix must be one of 2, 8, 10, or 16".to_string()),
    };
    let width = if width == 0 { minimum_width } else { width };
    if width < minimum_width {
        return Err(format!(
            "width must be at least {minimum_width} for radix {radix}"
        ));
    }
    if width > MAX_FORMAT_WIDTH {
        return Err(format!(
            "width must not exceed {MAX_FORMAT_WIDTH} characters"
        ));
    }
    let width = width as usize;
    let capacity = bytes
        .len()
        .checked_mul(width)
        .ok_or_else(|| "formatted output is too large".to_string())?;
    let mut output = String::with_capacity(capacity);
    for byte in bytes {
        let digit = match radix {
            2 => format!("{byte:0width$b}"),
            8 => format!("{byte:0width$o}"),
            10 => format!("{byte:0width$}"),
            16 => format!("{byte:0width$x}"),
            _ => return Err("unsupported byte formatting radix".to_string()),
        };
        output.push_str(&digit);
    }
    Ok(output)
}

fn checked_range(
    bytes: &[u8],
    offset: i64,
    width: usize,
) -> Result<std::ops::Range<usize>, String> {
    if offset < 0 {
        return Err("offset must not be negative".to_string());
    }
    let start = offset as usize;
    let end = start
        .checked_add(width)
        .ok_or_else(|| "offset is too large".to_string())?;
    if end > bytes.len() {
        return Err("not enough bytes for requested value".to_string());
    }
    Ok(start..end)
}

fn read_uint(bytes: &[u8], offset: i64, width: i64, little_endian: bool) -> Result<i64, String> {
    let width = usize::try_from(width).map_err(|_| "width must be positive".to_string())?;
    if !(1..=8).contains(&width) {
        return Err("integer width must be between 1 and 8 bytes".to_string());
    }
    let range = checked_range(bytes, offset, width)?;
    let mut value = 0u64;
    for (position, byte) in bytes[range].iter().enumerate() {
        let shift = if little_endian {
            position
        } else {
            width - position - 1
        } * 8;
        value |= u64::from(*byte) << shift;
    }
    i64::try_from(value).map_err(|_| "decoded unsigned integer does not fit in int".to_string())
}

fn write_uint(
    bytes: &mut [u8],
    offset: i64,
    value: i64,
    width: i64,
    little_endian: bool,
) -> Result<(), String> {
    if value < 0 {
        return Err("unsigned integer value must not be negative".to_string());
    }
    let width = usize::try_from(width).map_err(|_| "width must be positive".to_string())?;
    if !(1..=8).contains(&width) {
        return Err("integer width must be between 1 and 8 bytes".to_string());
    }
    let value = u64::try_from(value).map_err(|_| "value is out of range".to_string())?;
    if width < 8 && value >= (1u64 << (width * 8)) {
        return Err("integer value does not fit requested width".to_string());
    }
    let range = checked_range(bytes, offset, width)?;
    for (position, slot) in bytes[range].iter_mut().enumerate() {
        let shift = if little_endian {
            position
        } else {
            width - position - 1
        } * 8;
        *slot = (value >> shift) as u8;
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_read_uint(
    value: *const Value,
    offset: i64,
    width: i64,
    little_endian: bool,
) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return error("expected bytes");
    };
    match read_uint(bytes, offset, width, little_endian) {
        Ok(number) => ok(Value::Int(number)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_write_uint(
    value: *mut Value,
    offset: i64,
    number: i64,
    width: i64,
    little_endian: bool,
) -> *mut Value {
    let Some(bytes) = bytes_mut(value) else {
        return error("expected bytes");
    };
    match write_uint(bytes, offset, number, width, little_endian) {
        Ok(()) => ok(Value::Unit),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_read_float(
    value: *const Value,
    offset: i64,
    little_endian: bool,
) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return error("expected bytes");
    };
    let Ok(range) = checked_range(bytes, offset, 8) else {
        return error("not enough bytes for an 8-byte float");
    };
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[range]);
    if !little_endian {
        raw.reverse();
    }
    ok(Value::Float(ordered_float::OrderedFloat(f64::from_bits(
        u64::from_ne_bytes(raw),
    ))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_write_float(
    value: *mut Value,
    offset: i64,
    number: *const Value,
    little_endian: bool,
) -> *mut Value {
    let Some(number) = number.as_ref() else {
        return error("expected float");
    };
    let Value::Float(number) = number else {
        return error("expected float");
    };
    let Some(bytes) = bytes_mut(value) else {
        return error("expected bytes");
    };
    let Ok(range) = checked_range(bytes, offset, 8) else {
        return error("not enough bytes for an 8-byte float");
    };
    let mut raw = number.to_bits().to_ne_bytes();
    if !little_endian {
        raw.reverse();
    }
    bytes[range].copy_from_slice(&raw);
    ok(Value::Unit)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_read_varint(value: *const Value, offset: i64) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return error("expected bytes");
    };
    if offset < 0 {
        return error("offset must not be negative");
    }
    if offset as usize >= bytes.len() {
        return error("offset is outside the byte sequence");
    }
    let mut result = 0u64;
    let mut shift = 0u32;
    for byte in bytes.iter().skip(offset as usize) {
        if shift >= 64 {
            return error("varint is too large");
        }
        let payload = u64::from(byte & 0x7f);
        // A u64 varint has at most ten bytes.  On the tenth byte only the
        // least-significant payload bit belongs to the value; shifting any
        // larger payload by 63 would wrap in release builds and could turn a
        // malformed encoding into a different, valid integer.
        if shift == 63 && (payload > 1 || byte & 0x80 != 0) {
            return error("varint is too large");
        }
        result |= payload << shift;
        if byte & 0x80 == 0 {
            return i64::try_from(result).map_or_else(
                |_| error("decoded varint does not fit in int"),
                |number| ok(Value::Int(number)),
            );
        }
        shift += 7;
    }
    error("truncated varint")
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_write_varint(
    value: *mut Value,
    offset: i64,
    number: i64,
) -> *mut Value {
    if number < 0 {
        return error("varint value must not be negative");
    }
    let Some(bytes) = bytes_mut(value) else {
        return error("expected bytes");
    };
    if offset < 0 {
        return error("offset must not be negative");
    }
    let mut encoded = Vec::new();
    let mut number = number as u64;
    loop {
        let mut byte = (number & 0x7f) as u8;
        number >>= 7;
        if number != 0 {
            byte |= 0x80;
        }
        encoded.push(byte);
        if number == 0 {
            break;
        }
    }
    let Ok(start) = usize::try_from(offset) else {
        return error("offset is too large");
    };
    let Some(end) = start.checked_add(encoded.len()) else {
        return error("offset is too large");
    };
    if end > bytes.len() {
        return error("not enough space for encoded varint");
    }
    bytes[start..end].copy_from_slice(&encoded);
    ok(Value::Int(encoded.len() as i64))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_bytes_format(
    value: *const Value,
    radix: i64,
    width: i64,
) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return error("expected bytes");
    };
    match format_bytes(bytes, radix, width) {
        Ok(text) => ok(Value::String(text)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
/// Create a sequential cursor over a copy of a byte sequence. Writes grow the
/// cursor's private buffer; call `mux_bytes_cursor_into_bytes` to obtain it.
///
/// # Safety
/// `value` must be null or point to a live `bytes` value for the duration of
/// this call.
pub unsafe extern "C" fn mux_bytes_cursor_new(value: *const Value) -> *mut Value {
    let Some(bytes) = bytes_ref(value) else {
        return cursor_error("expected bytes");
    };
    let id = next_cursor();
    cursor_lock().insert(
        id,
        CursorEntry {
            bytes: bytes.clone(),
            position: 0,
            names: 1,
        },
    );
    match cursor_value(id) {
        Ok(value) => ok(value),
        Err(message) => {
            release_cursor(id);
            cursor_error(message)
        }
    }
}

#[unsafe(no_mangle)]
/// Return the cursor's current byte offset.
pub unsafe extern "C" fn mux_bytes_cursor_position(cursor: *const Value) -> *mut Value {
    let id = match cursor_id(cursor) {
        Ok(id) => id,
        Err(message) => return cursor_error(message),
    };
    match cursor_lock().get(&id) {
        Some(entry) => ok(Value::Int(entry.position as i64)),
        None => cursor_error("invalid bytes cursor"),
    }
}

#[unsafe(no_mangle)]
/// Return the number of unread bytes remaining in the cursor.
pub unsafe extern "C" fn mux_bytes_cursor_remaining(cursor: *const Value) -> *mut Value {
    let id = match cursor_id(cursor) {
        Ok(id) => id,
        Err(message) => return cursor_error(message),
    };
    match cursor_lock().get(&id) {
        Some(entry) => ok(Value::Int((entry.bytes.len() - entry.position) as i64)),
        None => cursor_error("invalid bytes cursor"),
    }
}

#[unsafe(no_mangle)]
/// Read and advance by `length` bytes.
pub unsafe extern "C" fn mux_bytes_cursor_read_bytes(
    cursor: *mut Value,
    length: i64,
) -> *mut Value {
    if length < 0 {
        return cursor_error("length must not be negative");
    }
    let id = match cursor_id(cursor) {
        Ok(id) => id,
        Err(message) => return cursor_error(message),
    };
    let mut cursors = cursor_lock();
    let Some(entry) = cursors.get_mut(&id) else {
        return cursor_error("invalid bytes cursor");
    };
    let Ok(length) = usize::try_from(length) else {
        return cursor_error("length is too large");
    };
    let range = match cursor_range(entry, length) {
        Ok(range) => range,
        Err(message) => return cursor_error(message),
    };
    let bytes = entry.bytes[range.clone()].to_vec();
    entry.position = range.end;
    ok(Value::Bytes(bytes))
}

#[unsafe(no_mangle)]
/// Read an unsigned integer and advance by its width.
pub unsafe extern "C" fn mux_bytes_cursor_read_uint(
    cursor: *mut Value,
    width: i64,
    little_endian: bool,
) -> *mut Value {
    let id = match cursor_id(cursor) {
        Ok(id) => id,
        Err(message) => return cursor_error(message),
    };
    let mut cursors = cursor_lock();
    let Some(entry) = cursors.get_mut(&id) else {
        return cursor_error("invalid bytes cursor");
    };
    match cursor_read_uint(entry, width, little_endian) {
        Ok(number) => ok(Value::Int(number)),
        Err(message) => cursor_error(message),
    }
}

#[unsafe(no_mangle)]
/// Write an unsigned integer and advance by its width.
pub unsafe extern "C" fn mux_bytes_cursor_write_uint(
    cursor: *mut Value,
    number: i64,
    width: i64,
    little_endian: bool,
) -> *mut Value {
    let id = match cursor_id(cursor) {
        Ok(id) => id,
        Err(message) => return cursor_error(message),
    };
    let mut cursors = cursor_lock();
    let Some(entry) = cursors.get_mut(&id) else {
        return cursor_error("invalid bytes cursor");
    };
    match cursor_write_uint(entry, number, width, little_endian) {
        Ok(()) => ok(Value::Unit),
        Err(message) => cursor_error(message),
    }
}

#[unsafe(no_mangle)]
/// Read an IEEE-754 64-bit float and advance by eight bytes.
pub unsafe extern "C" fn mux_bytes_cursor_read_float(
    cursor: *mut Value,
    little_endian: bool,
) -> *mut Value {
    let id = match cursor_id(cursor) {
        Ok(id) => id,
        Err(message) => return cursor_error(message),
    };
    let mut cursors = cursor_lock();
    let Some(entry) = cursors.get_mut(&id) else {
        return cursor_error("invalid bytes cursor");
    };
    let range = match cursor_range(entry, 8) {
        Ok(range) => range,
        Err(message) => return cursor_error(message),
    };
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&entry.bytes[range.clone()]);
    if !little_endian {
        raw.reverse();
    }
    entry.position = range.end;
    ok(Value::Float(ordered_float::OrderedFloat(f64::from_bits(
        u64::from_ne_bytes(raw),
    ))))
}

#[unsafe(no_mangle)]
/// Write an IEEE-754 64-bit float and advance by eight bytes.
pub unsafe extern "C" fn mux_bytes_cursor_write_float(
    cursor: *mut Value,
    number: *const Value,
    little_endian: bool,
) -> *mut Value {
    let Some(Value::Float(number)) = number.as_ref() else {
        return cursor_error("expected float");
    };
    let id = match cursor_id(cursor) {
        Ok(id) => id,
        Err(message) => return cursor_error(message),
    };
    let mut cursors = cursor_lock();
    let Some(entry) = cursors.get_mut(&id) else {
        return cursor_error("invalid bytes cursor");
    };
    let Some(end) = entry.position.checked_add(8) else {
        return cursor_error("cursor position is too large");
    };
    if end > entry.bytes.len() {
        entry.bytes.resize(end, 0);
    }
    let mut raw = number.to_bits().to_ne_bytes();
    if !little_endian {
        raw.reverse();
    }
    entry.bytes[entry.position..end].copy_from_slice(&raw);
    entry.position = end;
    ok(Value::Unit)
}

#[unsafe(no_mangle)]
/// Return the unread bytes as a non-consuming value.
pub unsafe extern "C" fn mux_bytes_cursor_into_bytes(cursor: *const Value) -> *mut Value {
    let id = match cursor_id(cursor) {
        Ok(id) => id,
        Err(message) => return cursor_error(message),
    };
    match cursor_lock().get(&id) {
        Some(entry) => ok(Value::Bytes(entry.bytes.clone())),
        None => cursor_error("invalid bytes cursor"),
    }
}
