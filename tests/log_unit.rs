use mux_runtime::log::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::{mux_log_error_detail, mux_log_error_kind};
use mux_runtime::stream::mux_io_writer_bytes;
use mux_runtime::Value;

fn string(value: &str) -> *mut Value {
    mux_rc_alloc(Value::String(value.to_string()))
}

#[test]
fn logger_handles_levels_names_and_fields() {
    unsafe {
        let logger = mux_log_logger_new();
        assert!(!logger.is_null());
        let level = string("debug");
        let name = string("worker");
        let key = string("request_id");
        let value = string("abc");
        let message = string("started");

        for result in [
            mux_log_logger_set_level(logger, level),
            mux_log_logger_set_name(logger, name),
            mux_log_logger_field(logger, key, value),
            mux_log_logger_debug(logger, message),
        ] {
            assert!(mux_result_is_ok(result));
            assert!(mux_rc_dec(result));
        }

        for value in [level, name, key, value, message, logger] {
            assert!(mux_rc_dec(value));
        }
    }
}

#[test]
fn logger_rejects_unknown_levels_and_default_can_be_replaced() {
    unsafe {
        let logger = mux_log_logger_new();
        let invalid = string("verbose");
        let result = mux_log_logger_set_level(logger, invalid);
        assert!(!mux_result_is_ok(result));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(invalid));

        let set_default = mux_log_set_default(logger);
        assert!(mux_result_is_ok(set_default));
        assert!(mux_rc_dec(set_default));
        let default = mux_log_default();
        assert!(!default.is_null());
        assert!(mux_rc_dec(default));
        assert!(mux_rc_dec(logger));
    }
}

#[test]
fn logger_result_errors_have_structured_payloads() {
    unsafe {
        let logger = mux_log_logger_new();
        let bad_key = string("bad key");
        let value = string("value");
        let result = mux_log_logger_field(logger, bad_key, value);
        assert!(!mux_result_is_ok(result));
        let error = mux_result_data(result);
        assert_eq!(direct_i32(mux_log_error_kind(error)), 0);
        assert!(direct_string(mux_log_error_detail(error)).contains("field name"));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(bad_key));
        assert!(mux_rc_dec(value));
        assert!(mux_rc_dec(logger));
    }
}

#[test]
fn logger_operations_reject_other_opaque_handle_types() {
    unsafe {
        let logger = mux_log_logger_new();
        let writer = mux_runtime::stream::mux_io_writer_new();
        let level = string("debug");

        // Both handles contain an integer registry id, but that id is only
        // meaningful inside its owning runtime registry. Passing a Writer to
        // a Logger operation must fail before it can mutate a logger entry.
        let result = mux_log_logger_set_level(writer, level);
        assert!(!mux_result_is_ok(result));
        assert!(mux_rc_dec(result));

        assert!(mux_rc_dec(level));
        assert!(mux_rc_dec(writer));
        assert!(mux_rc_dec(logger));
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

fn direct_i32(value: *mut Value) -> i32 {
    unsafe {
        let output = match &*value {
            Value::Opaque(bytes) => i32::from_ne_bytes(bytes.as_ref().try_into().unwrap()),
            other => panic!("expected Opaque, got {other:?}"),
        };
        assert!(mux_rc_dec(value));
        output
    }
}

#[test]
fn logger_can_route_atomic_lines_to_a_writer() {
    unsafe {
        let logger = mux_log_logger_new();
        let writer = mux_runtime::stream::mux_io_writer_new();
        let routed = mux_log_logger_set_writer(logger, writer);
        assert!(mux_result_is_ok(routed));
        assert!(mux_rc_dec(routed));
        let message = string("to writer");
        let emitted = mux_log_logger_info(logger, message);
        assert!(mux_result_is_ok(emitted));
        assert!(mux_rc_dec(emitted));
        let bytes_result = mux_io_writer_bytes(writer);
        assert!(mux_result_is_ok(bytes_result));
        let bytes = mux_result_data(bytes_result);
        assert!(
            matches!(&*bytes, Value::Bytes(data) if String::from_utf8_lossy(data).contains("to writer"))
        );
        assert!(mux_rc_dec(bytes));
        assert!(mux_rc_dec(bytes_result));
        assert!(mux_rc_dec(message));
        assert!(mux_rc_dec(logger));
        assert!(mux_rc_dec(writer));
    }
}
