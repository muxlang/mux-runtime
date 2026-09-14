use mux_runtime::object::{
    alloc_object, get_object_ptr, register_object_type_with_copy, register_shared_object_type,
};
use mux_runtime::optional::{mux_optional_data, mux_optional_some_value};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec, mux_value_deep_clone};
use mux_runtime::result::{mux_result_data, mux_result_err_value, mux_result_ok_value};
use mux_runtime::Value;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

extern "C" fn copy_number(source: *mut c_void, destination: *mut c_void) {
    unsafe { *destination.cast::<i64>() = *source.cast::<i64>() };
}

fn number(value: i64) -> *mut Value {
    let type_id = register_object_type_with_copy("PayloadNumber", 8, None, Some(copy_number));
    let object = alloc_object(type_id);
    assert!(!object.is_null());
    unsafe { *get_object_ptr(object).cast::<i64>() = value };
    object
}

fn read_number(object: *mut Value) -> i64 {
    assert!(!object.is_null());
    unsafe { *get_object_ptr(object).cast::<i64>() }
}

#[test]
fn wrapping_and_extracting_class_payloads_copies_the_value() {
    unsafe {
        let source = number(7);
        let success = mux_result_ok_value(source);
        let failure = mux_result_err_value(source);
        let present = mux_optional_some_value(source);
        *get_object_ptr(source).cast::<i64>() = 99;

        for (wrapper, optional) in [(success, false), (failure, false), (present, true)] {
            let first = if optional {
                mux_optional_data(wrapper)
            } else {
                mux_result_data(wrapper)
            };
            assert_eq!(read_number(first), 7);
            *get_object_ptr(first).cast::<i64>() = 42;
            let second = if optional {
                mux_optional_data(wrapper)
            } else {
                mux_result_data(wrapper)
            };
            assert_eq!(read_number(second), 7);
            mux_rc_dec(wrapper);
            assert_eq!(read_number(first), 42);
            assert_eq!(read_number(second), 7);
            mux_rc_dec(first);
            mux_rc_dec(second);
        }
        assert_eq!(read_number(source), 99);
        mux_rc_dec(source);
    }
}

#[test]
fn nested_optional_result_list_payloads_copy_class_values() {
    unsafe {
        let source = number(12);
        let list = mux_rc_alloc(Value::List(vec![(*source).clone()]));
        let optional = mux_optional_some_value(list);
        let result = mux_result_ok_value(optional);
        let copied = mux_value_deep_clone(result);
        *get_object_ptr(source).cast::<i64>() = 100;
        for value in [source, list, optional, result] {
            mux_rc_dec(value);
        }
        let extracted_optional = mux_result_data(copied);
        let extracted_list = mux_optional_data(extracted_optional);
        let Value::List(items) = &*extracted_list else {
            panic!("expected list payload");
        };
        let item = mux_rc_alloc(items[0].clone());
        assert_eq!(read_number(item), 12);
        for value in [copied, extracted_optional, extracted_list, item] {
            mux_rc_dec(value);
        }
    }
}

static SHARED_DROPS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn drop_shared(_data: *mut c_void) {
    SHARED_DROPS.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn shared_resource_payloads_release_the_resource_after_the_last_alias() {
    unsafe {
        let type_id = register_shared_object_type("PayloadResource", 8, Some(drop_shared));
        let resource = alloc_object(type_id);
        assert!(!resource.is_null());
        *get_object_ptr(resource).cast::<i64>() = 17;
        let copied = mux_value_deep_clone(resource);
        assert!(!copied.is_null());
        let optional = mux_optional_some_value(copied);
        let result = mux_result_ok_value(optional);
        let extracted_optional = mux_result_data(result);
        let extracted = mux_optional_data(extracted_optional);
        assert_eq!(get_object_ptr(resource), get_object_ptr(extracted));
        *get_object_ptr(extracted).cast::<i64>() = 23;
        assert_eq!(read_number(resource), 23);
        for value in [resource, copied, optional, result, extracted_optional] {
            mux_rc_dec(value);
        }
        assert_eq!(SHARED_DROPS.load(Ordering::Relaxed), 0);
        assert_eq!(read_number(extracted), 23);
        mux_rc_dec(extracted);
        assert_eq!(SHARED_DROPS.load(Ordering::Relaxed), 1);
    }
}
