use crate::refcount::{mux_rc_alloc, mux_rc_count};
use crate::Value;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fmt;

#[derive(Clone, Debug)]
pub struct Set(pub BTreeSet<Value>);

/// Mutate the `BTreeSet` backing a `Value::Set` with copy-on-write semantics.
///
/// When the wrapping `Value` is uniquely owned (`mux_rc_count == 1`) the backing
/// store is mutated in place, so filling a set in a loop stays O(n log n) instead
/// of cloning the whole set on every add/remove (O(n^2)). When the `Value` is
/// shared, the previous clone-then-write-back behavior is preserved so aliased
/// sets keep value semantics. Returns the closure's result, or `None` when
/// `set_val` is null or does not hold a set.
///
/// # Safety
/// `set_val` must be null or a valid pointer to a ref-counted `Value`.
#[allow(clippy::mutable_key_type)]
#[inline]
unsafe fn with_set_mut<R>(
    set_val: *mut Value,
    f: impl FnOnce(&mut BTreeSet<Value>) -> R,
) -> Option<R> {
    if set_val.is_null() {
        return None;
    }
    unsafe {
        if mux_rc_count(set_val) == 1 {
            if let Value::Set(set_data) = &mut *set_val {
                return Some(f(set_data));
            }
        } else if let Value::Set(set_data) = &*set_val {
            let mut new_set = set_data.clone();
            let result = f(&mut new_set);
            *set_val = Value::Set(new_set);
            return Some(result);
        }
    }
    None
}

impl Set {
    pub fn add(&mut self, val: Value) {
        self.0.insert(val);
    }

    pub fn remove(&mut self, val: &Value) -> bool {
        self.0.remove(val)
    }

    pub fn contains(&self, val: &Value) -> bool {
        self.0.contains(val)
    }
}

impl fmt::Display for Set {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let strs: Vec<String> = self.0.iter().map(|v| v.to_string()).collect();
        write!(f, "{{{}}}", strs.join(", "))
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_set_value(set: *mut Set) -> *mut Value {
    let owned = unsafe { Box::from_raw(set) };
    mux_rc_alloc(Value::Set(owned.0))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_set_add(set: *mut Set, val: *mut Value) {
    let set = unsafe { &mut *set };
    let val = unsafe { (*val).clone() };
    set.add(val);
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_set_add_value(set_val: *mut Value, val: *mut Value) {
    let value = unsafe { (*val).clone() };
    unsafe {
        with_set_mut(set_val, |set_data| {
            set_data.insert(value);
        });
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_set_contains(set: *const Set, val: *const Value) -> bool {
    unsafe { (*set).contains(&*val) }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_set_remove(set: *mut Set, val: *mut Value) -> bool {
    let set = unsafe { &mut *set };
    let val = unsafe { (*val).clone() };
    set.remove(&val)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_set_remove_value(set_val: *mut Value, val: *mut Value) -> bool {
    let value = unsafe { (*val).clone() };
    unsafe { with_set_mut(set_val, |set_data| set_data.remove(&value)).unwrap_or(false) }
}

/// # Safety
/// `set` must be a valid, non-null pointer to a `Set` created by this runtime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_set_size(set: *const Set) -> i64 {
    unsafe { (*set).0.len() as i64 }
}

/// # Safety
/// `set` must be a valid, non-null pointer to a `Set` created by this runtime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_set_is_empty(set: *const Set) -> bool {
    unsafe { (*set).0.is_empty() }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_set_to_string(set: *const Set) -> *mut std::ffi::c_char {
    let set = unsafe { &*set };
    let s = set.to_string();
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Convert a set to a list containing all of its elements.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_set_to_list(set: *const Set) -> *mut Value {
    if set.is_null() {
        return mux_rc_alloc(Value::List(Vec::new()));
    }
    let items: Vec<Value> = unsafe { (*set).0.iter().cloned().collect() };
    mux_rc_alloc(Value::List(items))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::mutable_key_type)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_set_union(a: *const Set, b: *const Set) -> *mut Set {
    if a.is_null() || b.is_null() {
        return std::ptr::null_mut();
    }

    let mut result = unsafe { (*a).0.clone() };
    result.extend(unsafe { (*b).0.clone() });
    Box::into_raw(Box::new(Set(result)))
}
