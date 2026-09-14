//! Lossless native filesystem paths for `std.fs`.

#![allow(clippy::missing_safety_doc)]

use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
};
use crate::refcount::mux_rc_dec;
use crate::{TypeId, Value};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path as StdPath, PathBuf};
use std::sync::{LazyLock, Mutex};

struct PathEntry {
    path: PathBuf,
    names: usize,
}

static PATHS: LazyLock<Mutex<HashMap<i64, PathEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(1));
static PATH_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    let id =
        register_object_type_with_copy("Path", size_of::<i64>(), Some(drop_path), Some(copy_path));
    crate::object::mux_register_object_equals(id, path_equals);
    crate::object::mux_register_object_compare(id, path_compare);
    crate::object::mux_register_object_hash(id, path_hash);
    id
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_id() -> i64 {
    let mut next = lock(&NEXT_ID);
    let id = *next;
    *next = next.checked_add(1).unwrap_or(1);
    id
}

fn object(path: PathBuf) -> *mut Value {
    let id = next_id();
    lock(&PATHS).insert(id, PathEntry { path, names: 1 });
    let value = alloc_object(*PATH_TYPE_ID);
    if value.is_null() {
        lock(&PATHS).remove(&id);
        return value;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        unsafe { mux_rc_dec(value) };
        lock(&PATHS).remove(&id);
        return std::ptr::null_mut();
    }
    unsafe { *ptr.cast::<i64>() = id };
    value
}

fn owned_object(path: PathBuf) -> Result<Value, String> {
    let value = object(path);
    if value.is_null() {
        return Err("could not allocate Path".to_string());
    }
    let Value::Object(reference) = (unsafe { &*value }) else {
        unsafe { mux_rc_dec(value) };
        return Err("could not allocate Path".to_string());
    };
    let owned = Value::Object(reference.clone());
    unsafe { mux_rc_dec(value) };
    Ok(owned)
}

fn result(value: Result<Value, String>) -> *mut Value {
    match value {
        Ok(value) => crate::std::fs_result_ok(value),
        Err(error) => crate::std::fs_result_err(error),
    }
}

fn id(value: *const Value) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != *PATH_TYPE_ID {
        return Err("expected Path value".to_string());
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err("Path value is invalid".to_string());
    }
    let id = unsafe { *ptr.cast::<i64>() };
    lock(&PATHS)
        .contains_key(&id)
        .then_some(id)
        .ok_or_else(|| "Path value is closed or invalid".to_string())
}

fn path(value: *const Value) -> Result<PathBuf, String> {
    let id = id(value)?;
    lock(&PATHS)
        .get(&id)
        .map(|entry| entry.path.clone())
        .ok_or_else(|| "Path value is closed or invalid".to_string())
}

extern "C" fn copy_path(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let id = unsafe { *source.cast::<i64>() };
    if let Some(entry) = lock(&PATHS).get_mut(&id) {
        entry.names += 1;
        unsafe { *dest.cast::<i64>() = id };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn drop_path(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let id = unsafe { *ptr.cast::<i64>() };
    let mut paths = lock(&PATHS);
    if paths.get_mut(&id).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    }) {
        paths.remove(&id);
    }
}

fn callback_path(value: *mut Value) -> Option<PathBuf> {
    path(value).ok()
}

extern "C" fn path_equals(a: *mut Value, b: *mut Value) -> bool {
    callback_path(a) == callback_path(b)
}

extern "C" fn path_compare(a: *mut Value, b: *mut Value) -> i32 {
    match callback_path(a).cmp(&callback_path(b)) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

extern "C" fn path_hash(value: *mut Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    callback_path(value).hash(&mut hasher);
    hasher.finish()
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_fs_path_new() -> *mut Value {
    object(PathBuf::new())
}

#[unsafe(no_mangle)]
/// Creates a path from a UTF-8 string.
pub unsafe extern "C" fn mux_fs_path_from_string(value: *const Value) -> *mut Value {
    match unsafe { value.as_ref() } {
        Some(Value::String(value)) => result(owned_object(PathBuf::from(value))),
        _ => result(Err("path source must be a string".to_string())),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_to_string(value: *const Value) -> *mut Value {
    match path(value).and_then(|path| {
        path.into_os_string()
            .into_string()
            .map_err(|_| "Path is not valid UTF-8".to_string())
    }) {
        Ok(value) => result(Ok(Value::String(value))),
        Err(error) => result(Err(error)),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_display(value: *const Value) -> *mut Value {
    match path(value) {
        Ok(path) => result(Ok(Value::String(path.to_string_lossy().into_owned()))),
        Err(error) => result(Err(error)),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_is_absolute(value: *const Value) -> *mut Value {
    result(path(value).map(|p| Value::Bool(p.is_absolute())))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_is_relative(value: *const Value) -> *mut Value {
    result(path(value).map(|p| Value::Bool(p.is_relative())))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_parent(value: *const Value) -> *mut Value {
    match path(value) {
        Ok(path) => match path.parent() {
            Some(parent) => match owned_object(parent.to_path_buf()) {
                Ok(value) => result(Ok(Value::Optional(Some(Box::new(value))))),
                Err(error) => result(Err(error)),
            },
            None => result(Ok(Value::Optional(None))),
        },
        Err(error) => result(Err(error)),
    }
}

fn component(value: *const Value, which: fn(&StdPath) -> Option<&std::ffi::OsStr>) -> *mut Value {
    match path(value) {
        Ok(path) => result(Ok(Value::Optional(
            which(&path)
                .and_then(std::ffi::OsStr::to_str)
                .map(|v| Box::new(Value::String(v.to_string()))),
        ))),
        Err(error) => result(Err(error)),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_file_name(value: *const Value) -> *mut Value {
    component(value, StdPath::file_name)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_extension(value: *const Value) -> *mut Value {
    component(value, StdPath::extension)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_stem(value: *const Value) -> *mut Value {
    component(value, StdPath::file_stem)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_join(value: *const Value, child: *const Value) -> *mut Value {
    let child = match unsafe { child.as_ref() } {
        Some(Value::String(value)) => value.clone(),
        _ => return result(Err("path child must be a string".to_string())),
    };
    match path(value).and_then(|path| owned_object(path.join(child))) {
        Ok(value) => result(Ok(value)),
        Err(error) => result(Err(error)),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_with_file_name(
    value: *const Value,
    name: *const Value,
) -> *mut Value {
    let name = match unsafe { name.as_ref() } {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        _ => return result(Err("file name must be a non-empty string".to_string())),
    };
    match path(value) {
        Ok(mut path) => {
            path.set_file_name(name);
            result(owned_object(path))
        }
        Err(error) => result(Err(error)),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_fs_path_with_extension(
    value: *const Value,
    extension: *const Value,
) -> *mut Value {
    let extension = match unsafe { extension.as_ref() } {
        Some(Value::String(value)) => value.clone(),
        _ => return result(Err("path extension must be a string".to_string())),
    };
    match path(value) {
        Ok(mut path) => {
            path.set_extension(extension);
            result(owned_object(path))
        }
        Err(error) => result(Err(error)),
    }
}
