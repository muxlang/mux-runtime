use crate::ordered::OrderedMap;
use crate::refcount::mux_rc_alloc;
use crate::Tuple;
use crate::Value;
use std::ffi::CString;
use std::fmt;
use std::os::raw::c_char;

#[derive(Clone, Debug)]
pub struct Map(pub OrderedMap<Value, Value>);

/// Mutate the `OrderedMap` backing a `Value::Map` in place.
///
/// The `*_value` map mutators are in-place operators by ABI: they return nothing
/// and mutate whatever `map_val` points at, so the change is always observed
/// through that pointer. Mux collections are value types (assignment deep-copies
/// rather than sharing the `Value` allocation), so a mutation site owns its map
/// uniquely; mutating the backing store directly keeps filling a map in a loop
/// O(n log n) instead of cloning the whole map on every insert/remove. Returns
/// the closure's result, or `None` when `map_val` is null or does not hold a map.
///
/// # Safety
/// `map_val` must be null or a valid pointer to a ref-counted `Value`.
#[allow(clippy::mutable_key_type)]
#[inline]
unsafe fn with_map_mut<R>(
    map_val: *mut Value,
    f: impl FnOnce(&mut OrderedMap<Value, Value>) -> R,
) -> Option<R> {
    if map_val.is_null() {
        return None;
    }
    unsafe {
        if let Value::Map(map_data) = &mut *map_val {
            Some(f(map_data))
        } else {
            None
        }
    }
}

impl Map {
    pub fn insert(&mut self, key: Value, val: Value) {
        self.0.insert(key, val);
    }

    #[must_use]
    pub fn get(&self, key: &Value) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn remove(&mut self, key: &Value) -> Option<Value> {
        self.0.remove(key)
    }

    #[must_use]
    pub fn contains(&self, key: &Value) -> bool {
        self.0.contains_key(key)
    }
}

impl fmt::Display for Map {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pairs: Vec<String> = self.0.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        write!(f, "{{{}}}", pairs.join(", "))
    }
}

#[unsafe(no_mangle)]
/// Converts an owned map allocation into a reference-counted value.
///
/// # Safety
///
/// `map` must be a non-null pointer returned by a map constructor or another
/// function that transfers ownership of a `Box<Map>`. The allocation is
/// consumed by this call and must not be used or freed afterward.
pub unsafe extern "C" fn mux_map_value(map: *mut Map) -> *mut Value {
    let owned = unsafe { Box::from_raw(map) };
    mux_rc_alloc(Value::Map(owned.0))
}

#[unsafe(no_mangle)]
/// Returns a map value as an optional value.
///
/// # Safety
///
/// `map` and `key` must be non-null pointers to live values created by the
/// runtime. Both remain owned by the caller.
pub unsafe extern "C" fn mux_map_get(map: *const Map, key: *const Value) -> *mut Value {
    let opt = unsafe { (*map).get(&*key).cloned() };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[unsafe(no_mangle)]
/// Inserts a cloned key and value into a map.
///
/// # Safety
///
/// `map`, `key`, and `val` must be non-null pointers to live values created by
/// the runtime. They remain owned by the caller.
pub unsafe extern "C" fn mux_map_put(map: *mut Map, key: *mut Value, val: *mut Value) {
    let map = unsafe { &mut *map };
    let key = unsafe { crate::refcount::snapshot_key(&*key) };
    let val = unsafe { (*val).clone() };
    map.insert(key, val);
}

#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
/// Inserts clones into a map value in place.
///
/// # Safety
///
/// A null `map_val`, `key`, or `val` preserves the no-op behavior. Non-null
/// pointers must be live runtime `Value` allocations; `map_val` must contain a
/// map, and all pointers remain owned by the caller.
pub unsafe extern "C" fn mux_map_put_value(map_val: *mut Value, key: *mut Value, val: *mut Value) {
    if map_val.is_null() || key.is_null() || val.is_null() {
        return;
    }
    let key_clone = unsafe { crate::refcount::snapshot_key(&*key) };
    let val_clone = unsafe { (*val).clone() };
    unsafe {
        with_map_mut(map_val, |map_data| {
            map_data.insert(key_clone, val_clone);
        });
    }
}

#[unsafe(no_mangle)]
/// Removes and returns a map value as an optional value.
///
/// # Safety
///
/// `map` and `key` must be non-null pointers to live values created by the
/// runtime. Both remain owned by the caller.
pub unsafe extern "C" fn mux_map_remove(map: *mut Map, key: *const Value) -> *mut Value {
    let opt = unsafe { (*map).remove(&*key) };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
/// Removes and returns a map value as an optional value in place.
///
/// # Safety
///
/// `key` must be a non-null pointer to a live runtime `Value`. `map_val` may
/// be null, which returns an empty optional; otherwise it must point to a live
/// `Value` allocation.
pub unsafe extern "C" fn mux_map_remove_value(map_val: *mut Value, key: *mut Value) -> *mut Value {
    let key = unsafe { (*key).clone() };
    let opt = unsafe { with_map_mut(map_val, |map_data| map_data.remove(&key)).flatten() };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[unsafe(no_mangle)]
/// Tests whether a map contains a key.
///
/// # Safety
///
/// Null `map` or `key` returns `false`; otherwise both must be live runtime
/// allocations and remain valid for the duration of this call.
pub unsafe extern "C" fn mux_map_contains(map: *const Map, key: *const Value) -> bool {
    if map.is_null() || key.is_null() {
        return false;
    }
    unsafe { (*map).contains(&*key) }
}

/// # Safety
/// `map` must be a valid, non-null pointer to a `Map` created by this runtime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_map_size(map: *const Map) -> i64 {
    unsafe { (*map).0.len() as i64 }
}

/// # Safety
/// `map` must be a valid, non-null pointer to a `Map` created by this runtime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_map_is_empty(map: *const Map) -> bool {
    unsafe { (*map).0.is_empty() }
}

#[unsafe(no_mangle)]
/// Formats a map as a newly allocated C string.
///
/// # Safety
///
/// `map` must be a non-null pointer to a live map created by the runtime.
pub unsafe extern "C" fn mux_map_to_string(map: *const Map) -> *mut c_char {
    let map = unsafe { &*map };
    let s = map.to_string();
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
/// Returns all map keys as a new list value.
///
/// # Safety
///
/// `map` may be null, which returns an empty list; otherwise it must be a live
/// map created by the runtime.
pub unsafe extern "C" fn mux_map_keys(map: *const Map) -> *mut Value {
    if map.is_null() {
        return mux_rc_alloc(Value::List(Vec::new()));
    }
    let keys: Vec<Value> = unsafe { (*map).0.keys().cloned().collect() };
    mux_rc_alloc(Value::List(keys))
}

#[unsafe(no_mangle)]
/// Returns all map values as a new list value.
///
/// # Safety
///
/// `map` may be null, which returns an empty list; otherwise it must be a live
/// map created by the runtime.
pub unsafe extern "C" fn mux_map_values(map: *const Map) -> *mut Value {
    if map.is_null() {
        return mux_rc_alloc(Value::List(Vec::new()));
    }
    let values: Vec<Value> = unsafe { (*map).0.values().cloned().collect() };
    mux_rc_alloc(Value::List(values))
}

#[unsafe(no_mangle)]
/// Returns all map entries as tuple values in a new list.
///
/// # Safety
///
/// `map` may be null, which returns an empty list; otherwise it must be a live
/// map created by the runtime.
pub unsafe extern "C" fn mux_map_pairs(map: *const Map) -> *mut Value {
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

#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
/// Merges two maps into a new map allocation.
///
/// # Safety
///
/// Each argument may be null, which returns null; otherwise it must point to a
/// live map created by the runtime.
pub unsafe extern "C" fn mux_map_merge(a: *const Map, b: *const Map) -> *mut Map {
    if a.is_null() || b.is_null() {
        return std::ptr::null_mut();
    }

    let mut result = unsafe { (*a).0.clone() };
    result.extend(unsafe { (*b).0.clone() });
    Box::into_raw(Box::new(Map(result)))
}
