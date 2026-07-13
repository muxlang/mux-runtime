use crate::refcount::mux_rc_alloc;
use crate::Value;
use std::ffi::CString;
use std::fmt;

#[derive(Clone, Debug)]
pub struct List(pub Vec<Value>);

/// Mutate the `Vec` backing a `Value::List` in place.
///
/// The `*_value` list mutators are in-place operators by ABI: they return
/// nothing and mutate whatever `list_val` points at, so the change is always
/// observed through that pointer. Mux collections are value types (assignment
/// deep-copies rather than sharing the `Value` allocation), so a mutation site
/// owns its list uniquely; mutating the backing store directly keeps
/// loop-building O(n) instead of cloning the whole vector on every call.
/// Returns the closure's result, or `None` when `list_val` is null or does not
/// hold a list.
///
/// # Safety
/// `list_val` must be null or a valid pointer to a ref-counted `Value`.
#[inline]
unsafe fn with_list_mut<R>(
    list_val: *mut Value,
    f: impl FnOnce(&mut Vec<Value>) -> R,
) -> Option<R> {
    if list_val.is_null() {
        return None;
    }
    unsafe {
        if let Value::List(list_data) = &mut *list_val {
            Some(f(list_data))
        } else {
            None
        }
    }
}

impl List {
    pub fn push_back(&mut self, val: Value) {
        self.0.push(val);
    }

    pub fn pop_back(&mut self) -> Option<Value> {
        self.0.pop()
    }

    pub fn length(&self) -> i64 {
        self.0.len() as i64
    }
}

impl fmt::Display for List {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let strs: Vec<String> = self.0.iter().map(|v| v.to_string()).collect();
        write!(f, "[{}]", strs.join(", "))
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_push_back(list: *mut List, val: *mut Value) {
    let value = unsafe { (*val).clone() };
    unsafe { (*list).push_back(value) }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_pop_back(list: *mut List) -> *mut Value {
    let opt = unsafe { (*list).pop_back() };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_push(list: *mut List, val: *mut Value) {
    let value = unsafe { (*val).clone() };
    unsafe { (*list).push_back(value) }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_pop(list: *mut List) -> *mut Value {
    let val = if unsafe { (*list).0.is_empty() } {
        None
    } else {
        Some(unsafe { (*list).0.remove(0) })
    };
    match val {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

/// # Safety
/// `list` must be a valid, non-null pointer to a `List` created by this runtime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_list_is_empty(list: *const List) -> bool {
    if list.is_null() {
        return false;
    }
    unsafe { (*list).length() == 0 }
}

/// # Safety
/// `list` must be a valid, non-null pointer to a `List` created by this runtime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_list_length(list: *const List) -> i64 {
    unsafe { (*list).length() }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_push_back_value(list_val: *mut Value, val: *mut Value) {
    let value = unsafe { (*val).clone() };
    unsafe {
        with_list_mut(list_val, |list_data| list_data.push(value));
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_push_value(list_val: *mut Value, val: *mut Value) {
    let value = unsafe { (*val).clone() };
    unsafe {
        with_list_mut(list_val, |list_data| list_data.insert(0, value)); // Add to front
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_pop_back_value(list_val: *mut Value) -> *mut Value {
    let opt = unsafe { with_list_mut(list_val, |list_data| list_data.pop()).flatten() };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_pop_value(list_val: *mut Value) -> *mut Value {
    let opt = unsafe {
        with_list_mut(list_val, |list_data| {
            if list_data.is_empty() {
                None
            } else {
                Some(list_data.remove(0))
            }
        })
        .flatten()
    };
    match opt {
        Some(v) => mux_rc_alloc(Value::Optional(Some(Box::new(v)))),
        None => mux_rc_alloc(Value::Optional(None)),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_get(list: *const List, index: i64) -> *mut Value {
    let len = unsafe { (*list).length() };
    if index < 0 || index >= len {
        mux_rc_alloc(Value::Optional(None))
    } else {
        let val = unsafe { (&(*list).0)[index as usize].clone() };
        mux_rc_alloc(Value::Optional(Some(Box::new(val))))
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_get_value(list: *const List, index: i64) -> *mut Value {
    if list.is_null() {
        return std::ptr::null_mut();
    }
    let len = unsafe { (*list).length() };
    if index < 0 || index >= len {
        std::ptr::null_mut()
    } else {
        let val = unsafe { (&(*list).0)[index as usize].clone() };
        mux_rc_alloc(val)
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_set(list: *mut List, index: i64, val: *mut Value) {
    let value = unsafe { (*val).clone() };
    let len = unsafe { (*list).length() };
    if index >= 0 && index < len {
        unsafe {
            (&mut (*list).0)[index as usize] = value;
        }
    }
    // else do nothing
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_set_value(list_val: *mut Value, index: i64, val: *mut Value) {
    if list_val.is_null() || val.is_null() {
        return;
    }

    let value = unsafe { (*val).clone() };
    unsafe {
        with_list_mut(list_val, |list_data| {
            let Some(actual_index) = normalized_index(index, list_data.len()) else {
                return;
            };

            if actual_index >= list_data.len() {
                extend_to_index(list_data, actual_index);
            }

            list_data[actual_index] = value;
        });
    }
}

fn normalized_index(index: i64, len: usize) -> Option<usize> {
    let len_i64 = len as i64;
    let wrapped = if index < 0 { len_i64 + index } else { index };
    (wrapped >= 0).then_some(wrapped as usize)
}

fn extend_to_index(list: &mut Vec<Value>, target_index: usize) {
    let default_value = default_fill_value(list);
    while list.len() <= target_index {
        list.push(default_value.clone());
    }
}

fn default_fill_value(list: &[Value]) -> Value {
    if list.is_empty() {
        return Value::Int(0);
    }

    match &list[0] {
        Value::Int(_) => Value::Int(0),
        Value::Float(_) => Value::Float(0.0.into()),
        Value::String(_) => Value::String(String::new()),
        Value::Bool(_) => Value::Bool(false),
        _ => Value::Int(0),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_insert(list: *mut List, index: i64, val: *mut Value) {
    let value = unsafe { (*val).clone() };
    let len = unsafe { (*list).length() as usize };
    let idx = if index < 0 {
        0
    } else if index as usize > len {
        len
    } else {
        index as usize
    };
    unsafe {
        (*list).0.insert(idx, value);
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_to_string(list: *const List) -> *mut std::ffi::c_char {
    let list = unsafe { &*list };
    let s = list.to_string();
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_concat(a: *const List, b: *const List) -> *mut List {
    if a.is_null() || b.is_null() {
        return std::ptr::null_mut();
    }

    let mut result = unsafe { (*a).0.clone() };
    result.extend(unsafe { (*b).0.clone() });
    Box::into_raw(Box::new(List(result)))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_list_contains(list: *const List, val: *const Value) -> bool {
    unsafe { (*list).0.iter().any(|item| item == &*val) }
}
