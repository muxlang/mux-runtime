//! Coverage for the remaining std-layer C-ABI: some/ok/err wrappers, container
//! extraction, list slicing, env access, enum boxing, and no-op frees.
#![allow(clippy::mutable_key_type)]

use std::ffi::CString;

use mux_runtime::io::mux_io_read_file;
use mux_runtime::optional::{mux_optional_get_value, mux_optional_is_none, mux_optional_is_some};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_is_err, mux_result_is_ok};
use mux_runtime::std::*;
use mux_runtime::Value;

#[test]
fn some_none_ok_err_wrappers() {
    unsafe {
        let i = mux_int_value(1);
        let some = mux_some(i);
        assert!(mux_optional_is_some(some));
        assert!(mux_rc_dec(some));

        let none = mux_none();
        assert!(mux_optional_is_none(none));
        assert!(mux_rc_dec(none));

        let ok = mux_ok(i);
        assert!(mux_result_is_ok(ok));
        assert!(mux_rc_dec(ok));

        let err = mux_err(CString::new("e").unwrap().as_ptr());
        assert!(mux_result_is_err(err));
        assert!(mux_rc_dec(err));

        assert!(mux_rc_dec(i));
    }
}

#[test]
fn data_error_categories_cross_abi_as_enums() {
    unsafe {
        let message = mux_rc_alloc(Value::String("invalid input".to_string()));
        let json = mux_json_error_from_message(message);
        let csv = mux_csv_error_from_message(message);
        let byte = mux_byte_error_from_message(message);
        let bytes = mux_bytes_error_from_message(message);

        let json_kind = mux_json_error_kind(json);
        let csv_kind = mux_csv_error_kind(csv);
        let byte_kind = mux_byte_error_kind(byte);
        let bytes_kind = mux_bytes_error_kind(bytes);
        for kind in [json_kind, csv_kind, byte_kind, bytes_kind] {
            assert!(
                matches!(&*kind, Value::Opaque(value) if value.as_ref() == 1_i32.to_ne_bytes())
            );
            assert!(mux_rc_dec(kind));
        }
        assert!(mux_rc_dec(json));
        assert!(mux_rc_dec(csv));
        assert!(mux_rc_dec(byte));
        assert!(mux_rc_dec(bytes));
        assert!(mux_rc_dec(message));
    }
}

#[test]
fn container_extraction() {
    unsafe {
        let mut m = mux_runtime::ordered::OrderedMap::new();
        m.insert(Value::String("k".into()), Value::Int(1));
        let map_val = mux_rc_alloc(Value::Map(m));
        let raw_map = mux_value_get_map(map_val);
        assert!(!raw_map.is_null());
        mux_free_map(raw_map);
        assert!(mux_rc_dec(map_val));

        let mut s = mux_runtime::ordered::OrderedSet::new();
        s.insert(Value::Int(7));
        let set_val = mux_rc_alloc(Value::Set(s));
        let raw_set = mux_value_get_set(set_val);
        assert!(!raw_set.is_null());
        mux_free_set(raw_set);
        assert!(mux_rc_dec(set_val));

        let list_val = mux_rc_alloc(Value::List(vec![Value::Int(1), Value::Int(2)]));
        let raw_list = mux_value_to_list(list_val);
        assert!(!raw_list.is_null());
        mux_free_list(raw_list);
        assert!(mux_rc_dec(list_val));

        // Non-matching value types extract to null.
        let not_a_map = mux_int_value(0);
        assert!(mux_value_get_map(not_a_map).is_null());
        assert!(mux_value_get_set(not_a_map).is_null());
        assert!(mux_value_to_list(not_a_map).is_null());
        assert!(mux_rc_dec(not_a_map));
    }
}

#[test]
fn value_map_get_value_reads_without_cloning_whole_map() {
    unsafe {
        let mut m = mux_runtime::ordered::OrderedMap::new();
        m.insert(Value::String("k".into()), Value::Int(42));
        let map_val = mux_rc_alloc(Value::Map(m));

        // Present key -> Some(value).
        let key = mux_rc_alloc(Value::String("k".into()));
        let hit = mux_value_map_get_value(map_val, key);
        assert!(mux_optional_is_some(hit));
        let inner = mux_optional_get_value(hit);
        assert_eq!(mux_value_get_int(inner), 42);
        assert!(mux_rc_dec(inner));
        assert!(mux_rc_dec(hit));
        assert!(mux_rc_dec(key));

        // Missing key -> None.
        let missing = mux_rc_alloc(Value::String("nope".into()));
        let miss = mux_value_map_get_value(map_val, missing);
        assert!(mux_optional_is_none(miss));
        assert!(mux_rc_dec(miss));
        assert!(mux_rc_dec(missing));

        // Non-map value -> None (defensive).
        let not_a_map = mux_int_value(0);
        let probe_key = mux_rc_alloc(Value::Int(1));
        let none = mux_value_map_get_value(not_a_map, probe_key);
        assert!(mux_optional_is_none(none));
        assert!(mux_rc_dec(none));
        assert!(mux_rc_dec(probe_key));
        assert!(mux_rc_dec(not_a_map));

        assert!(mux_rc_dec(map_val));
    }
}

#[test]
fn list_index_and_slice() {
    unsafe {
        let list_val = mux_rc_alloc(Value::List(vec![
            Value::Int(10),
            Value::Int(20),
            Value::Int(30),
        ]));
        assert_eq!(mux_value_list_length(list_val), 3);

        let elem = mux_value_list_get_value(list_val, 1);
        assert_eq!(mux_value_get_int(elem), 20);
        assert!(mux_rc_dec(elem));

        let slice = mux_value_list_slice(list_val, 0, 2);
        assert_eq!(mux_value_list_length(slice), 2);
        assert!(mux_rc_dec(slice));

        // A negative end is an empty range, not a wrapped usize index.
        let empty = mux_value_list_slice(list_val, 0, -1);
        assert_eq!(mux_value_list_length(empty), 0);
        assert!(mux_rc_dec(empty));

        // out-of-range index yields null
        assert!(mux_value_list_get_value(list_val, 99).is_null());

        assert!(mux_rc_dec(list_val));
    }
}

#[test]
fn env_access() {
    unsafe {
        // A variable we set is visible.
        std::env::set_var("MUX_TEST_ENV_VAR", "present");
        let got = mux_env_get(CString::new("MUX_TEST_ENV_VAR").unwrap().as_ptr());
        assert!(mux_result_is_ok(got));
        let got_value = mux_runtime::result::mux_result_data(got);
        assert!(mux_optional_is_some(got_value));
        assert!(mux_rc_dec(got_value));
        assert!(mux_rc_dec(got));

        let missing = mux_env_get(CString::new("MUX_DEFINITELY_UNSET_XYZ").unwrap().as_ptr());
        assert!(mux_result_is_ok(missing));
        let missing_value = mux_runtime::result::mux_result_data(missing);
        assert!(mux_optional_is_none(missing_value));
        assert!(mux_rc_dec(missing_value));
        assert!(mux_rc_dec(missing));

        let null_key = mux_env_get(std::ptr::null());
        assert!(mux_result_is_err(null_key));
        let error = mux_runtime::result::mux_result_data(null_key);
        assert!(matches!(&*error, Value::Object(_)));
        let kind = mux_env_error_kind(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes()));
        let detail = mux_env_error_message(error);
        assert!(matches!(&*detail, Value::String(value) if value.contains("must not be null")));
        assert!(mux_rc_dec(detail));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(null_key));
    }
}

#[test]
fn filesystem_errors_are_structured() {
    unsafe {
        let result = mux_io_read_file(std::ptr::null());
        assert!(mux_result_is_err(result));
        let error = mux_runtime::result::mux_result_data(result);
        assert!(matches!(&*error, Value::Object(_)));
        let kind = mux_fs_error_kind(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes()));
        let detail = mux_fs_error_message(error);
        assert!(matches!(&*detail, Value::String(value) if value.contains("path is null")));
        let path = mux_fs_error_path(error);
        assert!(matches!(&*path, Value::String(value) if value.is_empty()));
        assert!(mux_rc_dec(path));
        assert!(mux_rc_dec(detail));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));

        let missing_path = std::env::temp_dir().join(format!(
            "mux-definitely-missing-file-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&missing_path);
        let missing_path_text = missing_path.to_string_lossy().into_owned();
        let missing_path = CString::new(missing_path_text.clone()).unwrap();
        let missing = mux_io_read_file(missing_path.as_ptr());
        assert!(mux_result_is_err(missing));
        let missing_error = mux_runtime::result::mux_result_data(missing);
        let missing_kind = mux_fs_error_kind(missing_error);
        assert!(
            matches!(&*missing_kind, Value::Opaque(value) if value.as_ref() == 2_i32.to_ne_bytes())
        );
        let missing_path_value = mux_fs_error_path(missing_error);
        assert!(
            matches!(&*missing_path_value, Value::String(value) if value == &missing_path_text)
        );
        assert!(mux_rc_dec(missing_path_value));
        assert!(mux_rc_dec(missing_kind));
        assert!(mux_rc_dec(missing_error));
        assert!(mux_rc_dec(missing));

        let synthetic_detail = CString::new("failed at '/not/a/field' (quoted)").unwrap();
        let synthetic_detail_value = mux_rc_alloc(Value::String(
            synthetic_detail.to_string_lossy().into_owned(),
        ));
        let synthetic_error = mux_fs_error_from_message(synthetic_detail_value);
        assert!(mux_rc_dec(synthetic_detail_value));
        let synthetic_path = mux_fs_error_path(synthetic_error);
        assert!(matches!(&*synthetic_path, Value::String(value) if value.is_empty()));
        assert!(mux_rc_dec(synthetic_path));
        assert!(mux_rc_dec(synthetic_error));
    }
}

#[test]
fn env_mutation_and_contains_report_results() {
    unsafe {
        let key = CString::new("MUX_TEST_ENV_MUTATION").unwrap();
        let value = CString::new("set-value").unwrap();
        let set = mux_env_set(key.as_ptr(), value.as_ptr());
        assert!(mux_result_is_ok(set));
        assert!(mux_rc_dec(set));

        let contains = mux_env_contains(key.as_ptr());
        assert!(mux_result_is_ok(contains));
        assert!(mux_rc_dec(contains));

        let remove = mux_env_remove(key.as_ptr());
        assert!(mux_result_is_ok(remove));
        assert!(mux_rc_dec(remove));

        let invalid = CString::new("BAD=KEY").unwrap();
        let error = mux_env_set(invalid.as_ptr(), value.as_ptr());
        assert!(mux_result_is_err(error));
        let error_value = mux_runtime::result::mux_result_data(error);
        let key = mux_env_error_key(error_value);
        assert!(matches!(&*key, Value::String(value) if value == "BAD=KEY"));
        assert!(mux_rc_dec(key));
        assert!(mux_rc_dec(error_value));
        assert!(mux_rc_dec(error));
    }
}

#[test]
fn env_entries_are_a_sorted_result() {
    unsafe {
        let entries = mux_env_entries();
        assert!(mux_result_is_ok(entries));
        let value = mux_runtime::result::mux_result_data(entries);
        match &*value {
            mux_runtime::Value::List(items) => {
                for pair in items.windows(2) {
                    let mux_runtime::Value::Tuple(left) = &pair[0] else {
                        panic!("expected key/value tuple")
                    };
                    let mux_runtime::Value::Tuple(right) = &pair[1] else {
                        panic!("expected key/value tuple")
                    };
                    let mux_runtime::Value::String(left_key) = &left.0 else {
                        panic!("expected string key")
                    };
                    let mux_runtime::Value::String(right_key) = &right.0 else {
                        panic!("expected string key")
                    };
                    assert!(left_key <= right_key);
                }
            }
            other => panic!("expected list, got {other:?}"),
        }
        assert!(mux_runtime::refcount::mux_rc_dec(value));
        assert!(mux_rc_dec(entries));
    }
}

#[test]
fn nullable_value_reads_preserve_their_sentinel_results() {
    unsafe {
        assert!(mux_value_get_list(std::ptr::null_mut()).is_null());
        assert!(mux_value_get_map(std::ptr::null_mut()).is_null());
        assert!(mux_value_get_set(std::ptr::null_mut()).is_null());
        assert!(mux_value_to_list(std::ptr::null_mut()).is_null());

        assert_eq!(mux_value_get_int(std::ptr::null()), 0);
        assert!(mux_value_get_float(std::ptr::null()).abs() < f64::EPSILON);
        assert_eq!(mux_value_get_bool(std::ptr::null()), 0);
        assert_eq!(mux_value_get_type_tag(std::ptr::null()), -1);

        let none = mux_value_map_get_value(std::ptr::null(), std::ptr::null());
        assert!(mux_optional_is_none(none));
        assert!(mux_rc_dec(none));

        assert_eq!(mux_value_equal(std::ptr::null(), std::ptr::null()), 1);
        assert_eq!(mux_value_not_equal(std::ptr::null(), std::ptr::null()), 0);
        assert_eq!(mux_value_compare(std::ptr::null(), std::ptr::null()), 0);
        assert_eq!(mux_value_hash(std::ptr::null()), 0);
    }
}

#[test]
fn box_enum_and_noop_frees() {
    unsafe {
        let mut bytes = [1u8, 2, 3, 4];
        let boxed = mux_box_enum(bytes.as_mut_ptr(), bytes.len());
        assert_eq!(mux_value_get_type_tag(boxed), 12); // Opaque
        assert!(mux_rc_dec(boxed));

        // no-op frees must be safe to call
        mux_free_optional(std::ptr::null_mut());
        mux_free_result(std::ptr::null_mut());
    }
}

#[test]
fn unbox_enum_roundtrips_payload() {
    unsafe {
        let mut bytes = [7u8, 0, 0, 42];
        let boxed = mux_box_enum(bytes.as_mut_ptr(), bytes.len());
        let payload = mux_value_unbox_enum(boxed);
        assert!(!payload.is_null());
        let view = std::slice::from_raw_parts(payload, bytes.len());
        assert_eq!(view, &bytes);
        assert!(mux_rc_dec(boxed));
    }
}

#[test]
fn unbox_enum_rejects_null_and_non_opaque() {
    unsafe {
        assert!(mux_value_unbox_enum(std::ptr::null_mut()).is_null());

        let int = mux_int_value(5);
        assert!(mux_value_unbox_enum(int).is_null());
        assert!(mux_rc_dec(int));
    }
}
