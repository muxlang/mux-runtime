//! UUID construction, formatting, and opaque-handle tests.
#![cfg(feature = "uuid")]

mod common;

use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::{mux_uuid_error_detail, mux_uuid_error_kind};
use mux_runtime::uuid::*;
use mux_runtime::Value;

fn string(value: &str) -> *mut Value {
    mux_rc_alloc(Value::String(value.to_string()))
}

fn bytes(value: &[u8]) -> *mut Value {
    mux_rc_alloc(Value::Bytes(value.to_vec()))
}

unsafe fn ok_data(result: *mut Value) -> *mut Value {
    assert!(mux_result_is_ok(result));
    let data = mux_result_data(result);
    assert!(mux_rc_dec(result));
    data
}

#[test]
fn parse_and_format_all_supported_representations() {
    unsafe {
        let input = string("{550e8400-e29b-41d4-a716-446655440000}");
        let uuid = ok_data(mux_uuid_parse(input));
        assert!(mux_rc_dec(input));
        assert_eq!(
            direct_string(mux_uuid_to_string(uuid)),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(
            direct_string(mux_uuid_to_compact(uuid)),
            "550e8400e29b41d4a716446655440000"
        );
        assert_eq!(
            direct_string(mux_uuid_to_braced(uuid)),
            "{550e8400-e29b-41d4-a716-446655440000}"
        );
        assert_eq!(
            direct_string(mux_uuid_to_urn(uuid)),
            "urn:uuid:550e8400-e29b-41d4-a716-446655440000"
        );
        let raw = mux_uuid_to_bytes(uuid);
        assert!(matches!(&*raw, Value::Bytes(bytes) if bytes.len() == 16 && bytes[0] == 0x55));
        assert!(mux_rc_dec(raw));
        assert_eq!(direct_string(mux_uuid_variant(uuid)), "rfc4122");
        assert!(mux_rc_dec(uuid));
    }
}

#[test]
fn versions_namespaces_and_monotonic_v7_work() {
    unsafe {
        let namespace = mux_uuid_nil();
        let name = string("mux");
        let v3 = ok_data(mux_uuid_v3(namespace, name));
        let v5 = ok_data(mux_uuid_v5(namespace, name));
        assert!(mux_rc_dec(name));
        assert_eq!(
            direct_string(mux_uuid_to_string(v3)),
            "e8f06526-9af9-36dd-a9af-d54889c34536"
        );
        assert_eq!(
            direct_string(mux_uuid_to_string(v5)),
            "322d04df-e1ef-5cbd-bf60-a54360d86c6e"
        );
        assert_eq!(version(mux_uuid_version(v3)), Some(3));
        assert_eq!(version(mux_uuid_version(v5)), Some(5));
        assert!(mux_rc_dec(v3));
        assert!(mux_rc_dec(v5));
        assert!(mux_rc_dec(namespace));

        let first = mux_uuid_v7();
        let second = mux_uuid_v7();
        assert_eq!(version(mux_uuid_version(first)), Some(7));
        assert_eq!(version(mux_uuid_version(second)), Some(7));
        assert!(mux_rc_dec(first));
        assert!(mux_rc_dec(second));
    }
}

#[test]
fn bytes_parts_and_invalid_inputs_are_checked() {
    unsafe {
        let raw = bytes(&[0; 16]);
        let uuid = ok_data(mux_uuid_from_bytes(raw));
        assert!(mux_rc_dec(raw));
        assert!(mux_uuid_is_nil(uuid));
        let parts = mux_uuid_to_parts(uuid);
        assert!(
            matches!(&*parts, Value::Tuple(tuple) if matches!(tuple.0, Value::Int(0)) && matches!(tuple.1, Value::Int(0)))
        );
        assert!(mux_rc_dec(parts));
        assert!(mux_rc_dec(uuid));

        let bad = bytes(&[1, 2]);
        let bad_bytes_result = mux_uuid_from_bytes(bad);
        assert!(!mux_result_is_ok(bad_bytes_result));
        let bad_bytes_error = mux_result_data(bad_bytes_result);
        let bad_bytes_kind = mux_uuid_error_kind(bad_bytes_error);
        assert!(matches!(
            &*bad_bytes_kind,
            Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes()
        ));
        assert!(mux_rc_dec(bad_bytes_kind));
        assert!(direct_string(mux_uuid_error_detail(bad_bytes_error)).contains("exactly 16"));
        assert!(mux_rc_dec(bad_bytes_error));
        assert!(mux_rc_dec(bad_bytes_result));
        assert!(mux_rc_dec(bad));
        let bad_text = string("not-a-uuid");
        let bad_text_result = mux_uuid_parse(bad_text);
        assert!(!mux_result_is_ok(bad_text_result));
        let bad_text_error = mux_result_data(bad_text_result);
        let bad_text_kind = mux_uuid_error_kind(bad_text_error);
        assert!(matches!(
            &*bad_text_kind,
            Value::Opaque(value) if value.as_ref() == 1_i32.to_ne_bytes()
        ));
        assert!(mux_rc_dec(bad_text_kind));
        assert!(direct_string(mux_uuid_error_detail(bad_text_error)).contains("invalid"));
        assert!(mux_rc_dec(bad_text_error));
        assert!(mux_rc_dec(bad_text_result));
        assert!(mux_rc_dec(bad_text));
    }
}

fn version(value: *mut Value) -> Option<i64> {
    unsafe {
        let result = match &*value {
            Value::Optional(Some(value)) => match value.as_ref() {
                Value::Int(value) => Some(*value),
                _ => None,
            },
            _ => None,
        };
        assert!(mux_rc_dec(value));
        result
    }
}

fn direct_string(value: *mut Value) -> String {
    unsafe {
        let output = match &*value {
            Value::String(value) => value.clone(),
            other => panic!("expected String, got {other:?}"),
        };
        assert!(mux_rc_dec(value));
        output
    }
}
