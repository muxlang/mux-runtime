use crate::{list::List, map::Map, refcount::mux_rc_alloc, set::Set, Value};
use std::env as sys_env;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

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
        let len = vec.len() as i64;
        let s = start.max(0) as usize;
        let e = end.min(len) as usize;
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
/// Read an environment variable and return an owned optional string value.
///
/// # Safety
/// A null `key` is accepted and returns `None`. Otherwise `key` must point to
/// a valid NUL-terminated C string readable for the duration of this call.
pub unsafe extern "C" fn mux_env_get(key: *const c_char) -> *mut Value {
    if key.is_null() {
        return crate::optional::mux_optional_none();
    }
    // Convert C string to Rust String
    let k = unsafe { CStr::from_ptr(key) }
        .to_string_lossy()
        .into_owned();
    match sys_env::var(&k) {
        Ok(val) => {
            // If value contains interior NULs, CString::new will fail. Treat as None.
            if let Ok(cstr) = CString::new(val) {
                // mux_string_value clones the string, so passing as_ptr is safe while cstr is alive
                let vptr = unsafe { mux_string_value(cstr.as_ptr()) };
                // mux_optional_some_value clones vptr's inner value without
                // consuming vptr, so release the intermediate to avoid a leak.
                let some = unsafe { crate::optional::mux_optional_some_value(vptr) };
                unsafe { crate::refcount::mux_rc_dec(vptr) };
                some
            } else {
                crate::optional::mux_optional_none()
            }
        }
        Err(_) => crate::optional::mux_optional_none(),
    }
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
