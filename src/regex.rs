//! Unicode regular expressions backed by Rust's linear-time regex engine.
//!
//! Patterns use character-oriented matching. Match offsets returned to Mux are
//! character indices, not UTF-8 byte offsets.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::mux_rc_alloc;
use crate::{TypeId, Value};
use rust_std::collections::HashMap;
use rust_std::ffi::c_void;
use rust_std::sync::{LazyLock, Mutex};

type RustRegex = ::regex::Regex;

struct RegexEntry {
    regex: RustRegex,
    full_match_regex: RustRegex,
    names: Vec<Option<String>>,
    refs: usize,
}

struct MatchEntry {
    text: String,
    start: i64,
    end: i64,
    captures: Vec<Option<String>>,
    names: Vec<Option<String>>,
    refs: usize,
}

#[repr(C)]
struct ClosureRepr {
    function_ptr: *mut c_void,
    captures_ptr: *mut c_void,
}

static REGEXES: LazyLock<Mutex<HashMap<i64, RegexEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MATCHES: LazyLock<Mutex<HashMap<i64, MatchEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));

static REGEX_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Regex",
        size_of::<i64>(),
        Some(drop_regex as extern "C" fn(*mut c_void)),
        Some(copy_regex as extern "C" fn(*mut c_void, *mut c_void)),
    )
});
static MATCH_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "RegexMatch",
        size_of::<i64>(),
        Some(drop_match as extern "C" fn(*mut c_void)),
        Some(copy_match as extern "C" fn(*mut c_void, *mut c_void)),
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

fn error(message: impl Into<String>) -> *mut Value {
    crate::std::regex_result_err(message.into())
}

fn ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn object(type_id: TypeId, handle: i64) -> *mut Value {
    let value = alloc_object(type_id);
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

unsafe fn handle(value: *const Value, expected_type: TypeId) -> Option<i64> {
    if value.is_null() || unsafe { get_object_type_id(value) } != expected_type {
        return None;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        None
    } else {
        let handle = unsafe { *ptr.cast::<i64>() };
        (handle > 0).then_some(handle)
    }
}

fn copy_handle<T>(
    map: &Mutex<HashMap<i64, T>>,
    source: *mut c_void,
    dest: *mut c_void,
    add: impl Fn(&mut T),
) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let source_handle = unsafe { *source.cast::<i64>() };
    let mut entries = lock(map);
    if let Some(entry) = entries.get_mut(&source_handle) {
        add(entry);
        unsafe { *dest.cast::<i64>() = source_handle };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn copy_regex(source: *mut c_void, dest: *mut c_void) {
    copy_handle(&REGEXES, source, dest, |entry: &mut RegexEntry| {
        entry.refs += 1;
    });
}

extern "C" fn copy_match(source: *mut c_void, dest: *mut c_void) {
    copy_handle(&MATCHES, source, dest, |entry: &mut MatchEntry| {
        entry.refs += 1;
    });
}

fn release_regex(handle: i64) {
    let mut entries = lock(&REGEXES);
    let remove = entries.get_mut(&handle).is_some_and(|entry| {
        entry.refs = entry.refs.saturating_sub(1);
        entry.refs == 0
    });
    if remove {
        entries.remove(&handle);
    }
}

fn release_match(handle: i64) {
    let mut entries = lock(&MATCHES);
    let remove = entries.get_mut(&handle).is_some_and(|entry| {
        entry.refs = entry.refs.saturating_sub(1);
        entry.refs == 0
    });
    if remove {
        entries.remove(&handle);
    }
}

extern "C" fn drop_regex(ptr: *mut c_void) {
    if !ptr.is_null() {
        release_regex(unsafe { *ptr.cast::<i64>() });
    }
}

extern "C" fn drop_match(ptr: *mut c_void) {
    if !ptr.is_null() {
        release_match(unsafe { *ptr.cast::<i64>() });
    }
}

fn value_string(value: *const Value, label: &str) -> Result<String, String> {
    if value.is_null() {
        return Err(format!("{label} must not be null"));
    }
    match unsafe { &*value } {
        Value::String(value) => Ok(value.clone()),
        _ => Err(format!("{label} must be a string")),
    }
}

fn object_value(type_id: TypeId, handle: i64) -> Result<Value, String> {
    let ptr = object(type_id, handle);
    if ptr.is_null() {
        return Err("could not allocate regex object".to_string());
    }
    let value = match unsafe { &*ptr } {
        Value::Object(reference) => Value::Object(reference.clone()),
        _ => return Err("could not allocate regex object".to_string()),
    };
    unsafe { crate::refcount::mux_rc_dec(ptr) };
    Ok(value)
}

fn compile_regex(pattern: &str, flags: &str) -> Result<RustRegex, String> {
    let mut builder = ::regex::RegexBuilder::new(pattern);
    parse_flags(flags, &mut builder)?;
    builder.build().map_err(|error| error.to_string())
}

fn compile_full_match_regex(pattern: &str, flags: &str) -> Result<RustRegex, String> {
    let mut parser = ::regex_syntax::ParserBuilder::new();
    for flag in flags.chars() {
        match flag {
            'i' => parser.case_insensitive(true),
            'm' => parser.multi_line(true),
            's' => parser.dot_matches_new_line(true),
            'U' => parser.swap_greed(true),
            'x' => parser.ignore_whitespace(true),
            _ => return Err(format!("unsupported regex flag '{flag}'")),
        };
    }
    let hir = parser
        .build()
        .parse(pattern)
        .map_err(|error| error.to_string())?;
    let anchored = ::regex_syntax::hir::Hir::concat(vec![
        ::regex_syntax::hir::Hir::look(::regex_syntax::hir::Look::Start),
        hir,
        ::regex_syntax::hir::Hir::look(::regex_syntax::hir::Look::End),
    ]);
    compile_regex(&anchored.to_string(), "")
}

fn build_regex_entry(pattern: &str, flags: &str) -> Result<RegexEntry, String> {
    let regex = compile_regex(pattern, flags)?;
    let full_match_regex = compile_full_match_regex(pattern, flags)?;
    let names = regex
        .capture_names()
        .map(|name| name.map(str::to_owned))
        .collect();
    Ok(RegexEntry {
        regex,
        full_match_regex,
        names,
        refs: 1,
    })
}

fn insert_regex(entry: RegexEntry) -> Result<Value, String> {
    let handle = next_handle();
    lock(&REGEXES).insert(handle, entry);
    match object_value(*REGEX_TYPE_ID, handle) {
        Ok(value) => Ok(value),
        Err(error) => {
            lock(&REGEXES).remove(&handle);
            Err(error)
        }
    }
}

fn char_index(text: &str, byte_index: usize) -> i64 {
    text[..byte_index].chars().count() as i64
}

fn insert_match(
    text: &str,
    regex: &RegexEntry,
    captures: &::regex::Captures<'_>,
) -> Result<Value, String> {
    let whole = captures
        .get(0)
        .ok_or_else(|| "regex match did not contain a whole match".to_string())?;
    let values = captures
        .iter()
        .map(|capture| capture.map(|value| value.as_str().to_owned()))
        .collect();
    let entry = MatchEntry {
        text: whole.as_str().to_owned(),
        start: char_index(text, whole.start()),
        end: char_index(text, whole.end()),
        captures: values,
        names: regex.names.clone(),
        refs: 1,
    };
    let handle = next_handle();
    lock(&MATCHES).insert(handle, entry);
    match object_value(*MATCH_TYPE_ID, handle) {
        Ok(value) => Ok(value),
        Err(error) => {
            lock(&MATCHES).remove(&handle);
            Err(error)
        }
    }
}

fn regex_entry(handle: i64) -> Result<RegexEntryGuard, String> {
    let entries = REGEXES
        .lock()
        .unwrap_or_else(rust_std::sync::PoisonError::into_inner);
    if entries.contains_key(&handle) {
        Ok(RegexEntryGuard { entries, handle })
    } else {
        Err("invalid regex handle".to_string())
    }
}

struct RegexEntryGuard {
    entries: rust_std::sync::MutexGuard<'static, HashMap<i64, RegexEntry>>,
    handle: i64,
}

impl RegexEntryGuard {
    fn entry(&self) -> &RegexEntry {
        &self.entries[&self.handle]
    }
}

fn optional_match(regex: &RegexEntry, text: &str) -> Result<Value, String> {
    match regex.regex.captures(text) {
        Some(captures) => Ok(Value::Optional(Some(Box::new(insert_match(
            text, regex, &captures,
        )?)))),
        None => Ok(Value::Optional(None)),
    }
}

fn parse_flags(flags: &str, builder: &mut ::regex::RegexBuilder) -> Result<(), String> {
    for flag in flags.chars() {
        match flag {
            'i' => builder.case_insensitive(true),
            'm' => builder.multi_line(true),
            's' => builder.dot_matches_new_line(true),
            'U' => builder.swap_greed(true),
            'x' => builder.ignore_whitespace(true),
            _ => return Err(format!("unsupported regex flag '{flag}'")),
        };
    }
    Ok(())
}

unsafe fn invoke_string_callback(
    callback: *mut c_void,
    argument: *mut Value,
) -> Result<*mut Value, String> {
    if callback.is_null() {
        return Err("replacement callback is null".to_string());
    }
    let repr = unsafe { &*(callback as *const ClosureRepr) };
    if repr.function_ptr.is_null() {
        return Err("replacement callback function is null".to_string());
    }
    let result = if repr.captures_ptr.is_null() {
        let function: extern "C" fn(*mut Value) -> *mut Value =
            unsafe { std::mem::transmute(repr.function_ptr) };
        function(argument)
    } else {
        let function: extern "C" fn(*mut c_void, *mut Value) -> *mut Value =
            unsafe { std::mem::transmute(repr.function_ptr) };
        function(repr.captures_ptr, argument)
    };
    if result.is_null() {
        return Err("replacement callback returned null".to_string());
    }
    Ok(result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_new() -> *mut Value {
    match build_regex_entry("", "") {
        Ok(entry) => insert_regex(entry).map_or_else(error, ok),
        Err(error_value) => error(format!("invalid default regex: {error_value}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_from_pattern(pattern: *const Value) -> *mut Value {
    let pattern = match value_string(pattern, "pattern") {
        Ok(pattern) => pattern,
        Err(message) => return error(message),
    };
    match build_regex_entry(&pattern, "") {
        Ok(entry) => insert_regex(entry).map_or_else(error, ok),
        Err(error_value) => error(format!("invalid regex: {error_value}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_from_pattern_with_flags(
    pattern: *const Value,
    flags: *const Value,
) -> *mut Value {
    let pattern = match value_string(pattern, "pattern") {
        Ok(pattern) => pattern,
        Err(message) => return error(message),
    };
    let flags = match value_string(flags, "flags") {
        Ok(flags) => flags,
        Err(message) => return error(message),
    };
    match build_regex_entry(&pattern, &flags) {
        Ok(entry) => insert_regex(entry).map_or_else(error, ok),
        Err(error_value) => error(format!("invalid regex: {error_value}")),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_escape(value: *const Value) -> *mut Value {
    match value_string(value, "text") {
        Ok(text) => mux_rc_alloc(Value::String(::regex::escape(&text))),
        Err(_) => mux_rc_alloc(Value::String(String::new())),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_is_match(regex: *const Value, text: *const Value) -> *mut Value {
    let Some(handle) = handle(regex, *REGEX_TYPE_ID) else {
        return error("invalid regex handle");
    };
    let text = match value_string(text, "text") {
        Ok(text) => text,
        Err(message) => return error(message),
    };
    match regex_entry(handle) {
        Ok(entry) => ok(Value::Bool(entry.entry().regex.is_match(&text))),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_full_match(
    regex: *const Value,
    text: *const Value,
) -> *mut Value {
    let Some(handle) = handle(regex, *REGEX_TYPE_ID) else {
        return error("invalid regex handle");
    };
    let text = match value_string(text, "text") {
        Ok(text) => text,
        Err(message) => return error(message),
    };
    match regex_entry(handle) {
        Ok(entry) => {
            let matched = entry.entry().full_match_regex.is_match(&text);
            ok(Value::Bool(matched))
        }
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_find(regex: *const Value, text: *const Value) -> *mut Value {
    let Some(handle) = handle(regex, *REGEX_TYPE_ID) else {
        return error("invalid regex handle");
    };
    let text = match value_string(text, "text") {
        Ok(text) => text,
        Err(message) => return error(message),
    };
    match regex_entry(handle) {
        Ok(entry) => optional_match(entry.entry(), &text).map_or_else(error, ok),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_find_all(regex: *const Value, text: *const Value) -> *mut Value {
    let Some(handle) = handle(regex, *REGEX_TYPE_ID) else {
        return error("invalid regex handle");
    };
    let text = match value_string(text, "text") {
        Ok(text) => text,
        Err(message) => return error(message),
    };
    match regex_entry(handle) {
        Ok(entry) => {
            let regex = entry.entry();
            let mut values = Vec::new();
            for captures in regex.regex.captures_iter(&text) {
                match insert_match(&text, regex, &captures) {
                    Ok(value) => values.push(value),
                    Err(message) => return error(message),
                }
            }
            ok(Value::List(values))
        }
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_replace(
    regex: *const Value,
    text: *const Value,
    replacement: *const Value,
) -> *mut Value {
    replace(regex, text, replacement, false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_replace_first(
    regex: *const Value,
    text: *const Value,
    replacement: *const Value,
) -> *mut Value {
    replace(regex, text, replacement, true)
}

#[unsafe(no_mangle)]
/// Replace every match by invoking a synchronous Mux callback with its text.
///
/// # Safety
/// `regex`, `text`, and `callback` must be valid pointers for the duration of
/// the call. `callback` must point to a compiler-produced closure whose
/// signature is `func(string) returns string`; the closure is borrowed and is
/// not retained by this function.
pub unsafe extern "C" fn mux_regex_replace_with(
    regex: *const Value,
    text: *const Value,
    callback: *mut c_void,
) -> *mut Value {
    let Some(handle) = handle(regex, *REGEX_TYPE_ID) else {
        return error("invalid regex handle");
    };
    let text = match value_string(text, "text") {
        Ok(text) => text,
        Err(message) => return error(message),
    };
    let regex = match regex_entry(handle) {
        Ok(entry) => entry.entry().regex.clone(),
        Err(message) => return error(message),
    };
    let mut output = String::with_capacity(text.len());
    let mut previous_end = 0;
    for captures in regex.captures_iter(&text) {
        let Some(whole) = captures.get(0) else {
            return error("regex match did not contain a whole match");
        };
        output.push_str(&text[previous_end..whole.start()]);
        let argument = mux_rc_alloc(Value::String(whole.as_str().to_owned()));
        let replacement = match unsafe { invoke_string_callback(callback, argument) } {
            Ok(value) => value,
            Err(message) => {
                unsafe { crate::refcount::mux_rc_dec(argument) };
                return error(message);
            }
        };
        unsafe { crate::refcount::mux_rc_dec(argument) };
        let replacement_text = if let Some(Value::String(value)) = unsafe { replacement.as_ref() } {
            value.clone()
        } else {
            unsafe { crate::refcount::mux_rc_dec(replacement) };
            return error("replacement callback must return a string");
        };
        unsafe { crate::refcount::mux_rc_dec(replacement) };
        output.push_str(&replacement_text);
        previous_end = whole.end();
    }
    output.push_str(&text[previous_end..]);
    ok(Value::String(output))
}

unsafe fn replace(
    regex: *const Value,
    text: *const Value,
    replacement: *const Value,
    first: bool,
) -> *mut Value {
    let Some(handle) = handle(regex, *REGEX_TYPE_ID) else {
        return error("invalid regex handle");
    };
    let text = match value_string(text, "text") {
        Ok(text) => text,
        Err(message) => return error(message),
    };
    let replacement = match value_string(replacement, "replacement") {
        Ok(replacement) => replacement,
        Err(message) => return error(message),
    };
    match regex_entry(handle) {
        Ok(entry) => {
            let regex = &entry.entry().regex;
            let value = if first {
                regex.replace(&text, replacement.as_str())
            } else {
                regex.replace_all(&text, replacement.as_str())
            };
            ok(Value::String(value.into_owned()))
        }
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_split(regex: *const Value, text: *const Value) -> *mut Value {
    let Some(handle) = handle(regex, *REGEX_TYPE_ID) else {
        return error("invalid regex handle");
    };
    let text = match value_string(text, "text") {
        Ok(text) => text,
        Err(message) => return error(message),
    };
    match regex_entry(handle) {
        Ok(entry) => ok(Value::List(
            entry
                .entry()
                .regex
                .split(&text)
                .map(|value| Value::String(value.to_owned()))
                .collect(),
        )),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_group_count(regex: *const Value) -> *mut Value {
    let Some(handle) = handle(regex, *REGEX_TYPE_ID) else {
        return error("invalid regex handle");
    };
    match regex_entry(handle) {
        Ok(entry) => ok(Value::Int(entry.entry().regex.captures_len() as i64 - 1)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_named_groups(regex: *const Value) -> *mut Value {
    let Some(handle) = handle(regex, *REGEX_TYPE_ID) else {
        return error("invalid regex handle");
    };
    match regex_entry(handle) {
        Ok(entry) => ok(Value::List(
            entry
                .entry()
                .names
                .iter()
                .filter_map(|name| name.clone().map(Value::String))
                .collect(),
        )),
        Err(message) => error(message),
    }
}

fn match_entry(value: *const Value) -> Result<(i64, MatchEntryGuard), String> {
    let Some(handle) = (unsafe { handle(value, *MATCH_TYPE_ID) }) else {
        return Err("invalid regex match handle".to_string());
    };
    let entries = MATCHES
        .lock()
        .unwrap_or_else(rust_std::sync::PoisonError::into_inner);
    if entries.contains_key(&handle) {
        Ok((handle, MatchEntryGuard { entries }))
    } else {
        Err("invalid regex match handle".to_string())
    }
}

struct MatchEntryGuard {
    entries: rust_std::sync::MutexGuard<'static, HashMap<i64, MatchEntry>>,
}

impl MatchEntryGuard {
    fn entry(&self, handle: i64) -> &MatchEntry {
        &self.entries[&handle]
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_match_start(value: *const Value) -> *mut Value {
    match match_entry(value) {
        Ok((handle, entries)) => ok(Value::Int(entries.entry(handle).start)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_match_end(value: *const Value) -> *mut Value {
    match match_entry(value) {
        Ok((handle, entries)) => ok(Value::Int(entries.entry(handle).end)),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_match_text(value: *const Value) -> *mut Value {
    match match_entry(value) {
        Ok((handle, entries)) => ok(Value::String(entries.entry(handle).text.clone())),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_match_capture(value: *const Value, index: i64) -> *mut Value {
    if index < 0 {
        return ok(Value::Optional(None));
    }
    match match_entry(value) {
        Ok((handle, entries)) => {
            let capture = entries
                .entry(handle)
                .captures
                .get(index as usize)
                .and_then(|value| value.as_ref())
                .map(|value| Box::new(Value::String(value.clone())));
            ok(Value::Optional(capture))
        }
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_match_capture_named(
    value: *const Value,
    name: *const Value,
) -> *mut Value {
    let name = match value_string(name, "capture name") {
        Ok(name) => name,
        Err(message) => return error(message),
    };
    match match_entry(value) {
        Ok((handle, entries)) => {
            let entry = entries.entry(handle);
            let index = entry
                .names
                .iter()
                .position(|candidate| candidate.as_deref() == Some(name.as_str()));
            let capture = index
                .and_then(|index| entry.captures.get(index))
                .and_then(|value| value.as_ref())
                .map(|value| Box::new(Value::String(value.clone())));
            ok(Value::Optional(capture))
        }
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_regex_match_captures(value: *const Value) -> *mut Value {
    match match_entry(value) {
        Ok((handle, entries)) => ok(Value::List(
            entries
                .entry(handle)
                .captures
                .iter()
                .map(|value| {
                    Value::Optional(
                        value
                            .as_ref()
                            .map(|value| Box::new(Value::String(value.clone()))),
                    )
                })
                .collect(),
        )),
        Err(message) => error(message),
    }
}
