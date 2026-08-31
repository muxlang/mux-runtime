use crate::refcount::mux_rc_alloc;
use crate::Tuple;
use crate::Value;
use std::ffi::CString;
use std::os::raw::c_char;

#[unsafe(no_mangle)]
/// Creates a tuple by cloning the two values.
///
/// # Safety
///
/// `left` and `right` must each be non-null, valid pointers to live [`Value`]
/// instances for the duration of this call. The pointed-to values are only
/// read and remain owned by the caller.
pub unsafe extern "C" fn mux_new_tuple(left: *mut Value, right: *mut Value) -> *mut Tuple {
    let left_val = unsafe { (*left).clone() };
    let right_val = unsafe { (*right).clone() };
    Box::into_raw(Box::new(Tuple(left_val, right_val)))
}

#[unsafe(no_mangle)]
/// Converts an owned tuple allocation into a reference-counted value.
///
/// # Safety
///
/// `tuple` must be a non-null pointer returned by a tuple constructor or
/// another function that transfers ownership of a `Box<Tuple>` to the caller.
/// The allocation is consumed by this call and must not be used or freed
/// afterward.
pub unsafe extern "C" fn mux_tuple_value(tuple: *mut Tuple) -> *mut Value {
    let owned = unsafe { Box::from_raw(tuple) };
    mux_rc_alloc(Value::Tuple(owned))
}

#[unsafe(no_mangle)]
/// Compares two tuples for equality.
///
/// # Safety
///
/// Each non-null argument must point to a live [`Tuple`] for the duration of
/// this call. A null argument is accepted and returns `false`.
pub unsafe extern "C" fn mux_tuple_eq(a: *mut Tuple, b: *mut Tuple) -> bool {
    if a.is_null() || b.is_null() {
        return false;
    }
    let a = unsafe { &*a };
    let b = unsafe { &*b };
    a == b
}

#[unsafe(no_mangle)]
/// Formats a tuple as a newly allocated C string.
///
/// # Safety
///
/// `tuple` must be a non-null pointer to a live [`Tuple`] for the duration of
/// this call. The returned string is owned by the caller and must be released
/// with the runtime string deallocator.
pub unsafe extern "C" fn mux_tuple_to_string(tuple: *const Tuple) -> *mut c_char {
    let tuple = unsafe { &*tuple };
    let s = tuple.to_string();
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
/// Clones and returns the left value from a tuple.
///
/// # Safety
///
/// `tuple` must be a non-null pointer to a live [`Tuple`] for the duration of
/// this call.
pub unsafe extern "C" fn mux_tuple_left(tuple: *mut Tuple) -> *mut Value {
    let tuple = unsafe { &*tuple };
    mux_rc_alloc(tuple.0.clone())
}

#[unsafe(no_mangle)]
/// Clones and returns the right value from a tuple.
///
/// # Safety
///
/// `tuple` must be a non-null pointer to a live [`Tuple`] for the duration of
/// this call.
pub unsafe extern "C" fn mux_tuple_right(tuple: *mut Tuple) -> *mut Value {
    let tuple = unsafe { &*tuple };
    mux_rc_alloc(tuple.1.clone())
}

#[unsafe(no_mangle)]
/// Returns a borrowed tuple inside a value, when the value contains one.
///
/// # Safety
///
/// If non-null, `value` must point to a live [`Value`] for the duration of
/// this call. The returned tuple pointer is borrowed from `value`; it remains
/// valid only while that value and its tuple are not moved or released, and it
/// must not be freed by the caller. A null `value` returns null.
pub unsafe extern "C" fn mux_value_get_tuple(value: *mut Value) -> *mut Tuple {
    if value.is_null() {
        return std::ptr::null_mut();
    }
    // The Value::Tuple variant contains a Box<Tuple>
    // We need to cast the Value pointer to access the inner Box<Tuple>
    // This is safe because we're just reinterpreting the pointer
    unsafe {
        let value_ref = &mut *value;
        if let Value::Tuple(tuple_box) = value_ref {
            let tuple_ptr: *mut Tuple = &raw mut **tuple_box;
            tuple_ptr
        } else {
            std::ptr::null_mut()
        }
    }
}
