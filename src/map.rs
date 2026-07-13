use crate::refcount::{mux_rc_alloc, mux_rc_count};
use crate::Tuple;
use crate::Value;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt;
use std::os::raw::c_char;

#[derive(Clone, Debug)]
pub struct Map(pub BTreeMap<Value, Value>);

/// Mutate the `BTreeMap` backing a `Value::Map` with copy-on-write semantics.
///
/// When the wrapping `Value` is uniquely owned (`mux_rc_count == 1`) the backing
/// store is mutated in place, so filling a map in a loop stays O(n log n) instead
/// of cloning the whole map on every insert/remove (O(n^2)). When the `Value` is
/// shared, the previous clone-then-write-back behavior is preserved so aliased
/// maps keep value semantics. Returns the closure's result, or `None` when
/// `map_val` is null or does not hold a map.
///
/// # Safety
/// `map_val` must be null or a valid pointer to a ref-counted `Value`.
#[allow(clippy::mutable_key_type)]
#[inline]
unsafe fn with_map_mut<R>(
    map_val: *mut Value,
    f: impl FnOnce(&mut BTreeMap<Value, Value>) -> R,
) -> Option<R> {
    if map_val.is_null() {
        return None;
    }
    unsafe {
        if mux_rc_count(map_val) == 1 {
            if let Value::Map(map_data) = &mut *map_val {
                return Some(f(map_data));
            }
        } else if let Value::Map(map_data) = &*map_val {
            let mut new_map = map_data.clone();
            let result = f(&mut new_map);
            *map_val = Value::Map(new_map);
            return Some(result);
        }
    }
    None
}

impl Map {
    pub fn insert(&mut self, key: Value, val: Value) {
        self.0.insert(key, val);
    }

    pub fn get(&self, key: &Value) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn remove(&mut self, key: &Value) -> Option<Value> {
        self.0.remove(key)
    }

    pub fn contains(&self, key: &Value) -> bool {
        self.0.contains_key(key)
    }
}

impl fmt::Display for Map {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pairs: Vec<String> = self
            .0
            .iter()
            .map(|(k, v)| format!("{}: {}", k, v))
            .collect();
        write!(f, "{{{}}}", pairs.join(", "))
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_value(map: *mut Map) -> *mut Value {
    let owned = unsafe { Box::from_raw(map) };
    mux_rc_alloc(Value::Map(owned.0))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_get(map: *const Map, key: *const Value) -> *mut Value {
    let opt = unsafe { (*map).get(&*key).cloned() };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_put(map: *mut Map, key: *mut Value, val: *mut Value) {
    let map = unsafe { &mut *map };
    let key = unsafe { (*key).clone() };
    let val = unsafe { (*val).clone() };
    map.insert(key, val);
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_put_value(map_val: *mut Value, key: *mut Value, val: *mut Value) {
    if map_val.is_null() || key.is_null() || val.is_null() {
        return;
    }
    let key_clone = unsafe { (*key).clone() };
    let val_clone = unsafe { (*val).clone() };
    unsafe {
        with_map_mut(map_val, |map_data| {
            map_data.insert(key_clone, val_clone);
        });
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_remove(map: *mut Map, key: *const Value) -> *mut Value {
    let opt = unsafe { (*map).remove(&*key) };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_remove_value(map_val: *mut Value, key: *mut Value) -> *mut Value {
    let key = unsafe { (*key).clone() };
    let opt = unsafe { with_map_mut(map_val, |map_data| map_data.remove(&key)).flatten() };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_contains(map: *const Map, key: *const Value) -> bool {
    if map.is_null() || key.is_null() {
        return false;
    }
    unsafe { (*map).contains(&*key) }
}

/// # Safety
/// `map` must be a valid, non-null pointer to a `Map` created by this runtime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_map_size(map: *const Map) -> i64 {
    unsafe { (*map).0.len() as i64 }
}

/// # Safety
/// `map` must be a valid, non-null pointer to a `Map` created by this runtime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_map_is_empty(map: *const Map) -> bool {
    unsafe { (*map).0.is_empty() }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_to_string(map: *const Map) -> *mut c_char {
    let map = unsafe { &*map };
    let s = map.to_string();
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_keys(map: *const Map) -> *mut Value {
    if map.is_null() {
        return mux_rc_alloc(Value::List(Vec::new()));
    }
    let keys: Vec<Value> = unsafe { (*map).0.keys().cloned().collect() };
    mux_rc_alloc(Value::List(keys))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_values(map: *const Map) -> *mut Value {
    if map.is_null() {
        return mux_rc_alloc(Value::List(Vec::new()));
    }
    let values: Vec<Value> = unsafe { (*map).0.values().cloned().collect() };
    mux_rc_alloc(Value::List(values))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_pairs(map: *const Map) -> *mut Value {
    if map.is_null() {
        return mux_rc_alloc(Value::List(Vec::new()));
    }
    let pairs: Vec<Value> = unsafe {
        (*map)
            .0
            .iter()
            .map(|(k, v)| Value::Tuple(Box::new(Tuple(k.clone(), v.clone()))))
            .collect()
    };
    mux_rc_alloc(Value::List(pairs))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_map_merge(a: *const Map, b: *const Map) -> *mut Map {
    if a.is_null() || b.is_null() {
        return std::ptr::null_mut();
    }

    let mut result = unsafe { (*a).0.clone() };
    result.extend(unsafe { (*b).0.clone() });
    Box::into_raw(Box::new(Map(result)))
}
