use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_err, mux_result_is_ok};
use mux_runtime::std::{mux_io_error_kind, mux_io_error_operation};
use mux_runtime::stream::*;
use mux_runtime::Value;
use std::fs;
use std::path::PathBuf;

fn temp_stream_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mux-stream-{label}-{}", std::process::id()))
}

fn ok_value(value: *mut Value) -> Value {
    unsafe {
        let result = (&*value).clone();
        assert!(mux_rc_dec(value));
        match result {
            Value::Result(Ok(value)) => *value,
            Value::Result(Err(error)) => panic!("unexpected stream error: {error}"),
            other => panic!("expected result, got {other:?}"),
        }
    }
}

#[test]
fn reader_consumes_and_seeks_bytes() {
    let input = mux_rc_alloc(Value::Bytes(b"hello".to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    assert!(!reader.is_null());
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read(reader, 2) }),
        Value::Bytes(b"he".to_vec())
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_position(reader) }),
        Value::Int(2)
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_remaining(reader) }),
        Value::Int(3)
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_seek(reader, 1) }),
        Value::Int(1)
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read_exact(reader, 4) }),
        Value::Bytes(b"ello".to_vec())
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_remaining(reader) }),
        Value::Int(0)
    );
    unsafe {
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(input));
    }
}

#[test]
fn erased_streams_are_owned_and_report_missing_capabilities() {
    let input = mux_rc_alloc(Value::Bytes(b"hello".to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    let erased_reader = unsafe { mux_io_stream_from_reader(reader) };
    let erased_reader_value = mux_rc_alloc(ok_value(erased_reader));
    assert_eq!(
        ok_value(unsafe { mux_io_stream_read(erased_reader_value, 2) }),
        Value::Bytes(b"he".to_vec())
    );
    assert_eq!(
        ok_value(unsafe { mux_io_stream_seek(erased_reader_value, 0) }),
        Value::Int(0)
    );
    let bytes = mux_rc_alloc(Value::Bytes(b"x".to_vec()));
    let write_result = unsafe { mux_io_stream_write(erased_reader_value, bytes) };
    assert!(unsafe { mux_result_is_err(write_result) });
    unsafe {
        assert!(mux_rc_dec(write_result));
        mux_io_stream_close(erased_reader_value);
        assert!(mux_rc_dec(erased_reader_value));
        assert!(mux_rc_dec(bytes));
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(input));
    }

    let writer = mux_io_writer_new();
    let erased_writer = unsafe { mux_io_stream_from_writer(writer) };
    let erased_writer_value = mux_rc_alloc(ok_value(erased_writer));
    let output = mux_rc_alloc(Value::Bytes(b"ok".to_vec()));
    assert_eq!(
        ok_value(unsafe { mux_io_stream_write(erased_writer_value, output) }),
        Value::Int(2)
    );
    assert_eq!(
        ok_value(unsafe { mux_io_stream_flush(erased_writer_value) }),
        Value::Unit
    );
    let read_result = unsafe { mux_io_stream_read(erased_writer_value, 1) };
    assert!(unsafe { mux_result_is_err(read_result) });
    unsafe {
        assert!(mux_rc_dec(read_result));
        mux_io_stream_close(erased_writer_value);
        assert!(mux_rc_dec(erased_writer_value));
        assert!(mux_rc_dec(output));
        assert!(mux_rc_dec(writer));
    }
}

#[test]
fn erased_stream_alias_keeps_capability_alive_until_the_last_name_is_dropped() {
    let input = mux_rc_alloc(Value::Bytes(b"alias".to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    let erased = unsafe { mux_io_stream_from_reader(reader) };
    let erased_value = mux_rc_alloc(ok_value(erased));
    let alias = unsafe { mux_rc_alloc((*erased_value).clone()) };

    unsafe {
        assert!(mux_rc_dec(erased_value));
        assert_eq!(
            ok_value(mux_io_stream_read(alias, 5)),
            Value::Bytes(b"alias".to_vec())
        );
        assert!(mux_rc_dec(alias));
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(input));
    }
}

#[test]
fn stream_failures_use_structured_io_errors() {
    let result = unsafe { mux_io_reader_read(std::ptr::null(), 1) };
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let kind = unsafe { mux_io_error_kind(error) };
    let operation = unsafe { mux_io_error_operation(error) };
    assert!(
        matches!(unsafe { &*kind }, Value::Opaque(value) if value.as_ref() == 1_i32.to_ne_bytes())
    );
    assert!(matches!(unsafe { &*operation }, Value::String(value) if value.is_empty()));
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
    }
}

fn assert_file_open_not_found(result: *mut Value, operation: &str) {
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let kind = unsafe { mux_io_error_kind(error) };
    let operation_value = unsafe { mux_io_error_operation(error) };
    assert!(
        matches!(unsafe { &*kind }, Value::Opaque(value) if value.as_ref() == 2_i32.to_ne_bytes())
    );
    assert!(matches!(unsafe { &*operation_value }, Value::String(value) if value == operation));
    unsafe {
        assert!(mux_rc_dec(operation_value));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
    }
}

#[test]
fn file_stream_constructors_preserve_native_open_errors() {
    let missing_parent = temp_stream_path("missing-parent");
    let missing = missing_parent.join("file.bin");
    let path = mux_rc_alloc(Value::String(missing.to_string_lossy().into_owned()));

    assert_file_open_not_found(unsafe { mux_io_reader_from_file(path) }, "reader.from_file");
    assert_file_open_not_found(unsafe { mux_io_writer_to_file(path) }, "writer.to_file");
    assert_file_open_not_found(
        unsafe { mux_io_writer_append_file(path) },
        "writer.append_file",
    );

    unsafe {
        assert!(mux_rc_dec(path));
    }
}

#[test]
fn reader_read_line_normalizes_endings_and_reports_eof() {
    let input = mux_rc_alloc(Value::Bytes(b"one\r\ntwo\nfinal".to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read_line(reader) }),
        Value::Optional(Some(Box::new(Value::String("one".to_string()))))
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read_line(reader) }),
        Value::Optional(Some(Box::new(Value::String("two".to_string()))))
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read_line(reader) }),
        Value::Optional(Some(Box::new(Value::String("final".to_string()))))
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read_line(reader) }),
        Value::Optional(None)
    );
    unsafe {
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(input));
    }

    let invalid_input = mux_rc_alloc(Value::Bytes(vec![0xff]));
    let invalid_reader = unsafe { mux_io_reader_from_bytes(invalid_input) };
    let invalid = unsafe { mux_io_reader_read_line(invalid_reader) };
    assert!(!unsafe { mux_result_is_ok(invalid) });
    unsafe {
        assert!(mux_rc_dec(invalid));
        assert!(mux_rc_dec(invalid_reader));
        assert!(mux_rc_dec(invalid_input));
    }
}

#[test]
fn reader_read_line_accepts_exact_limit_but_rejects_overflow() {
    const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

    let exact_input = mux_rc_alloc(Value::Bytes(vec![b'a'; MAX_STREAM_BYTES]));
    let exact_reader = unsafe { mux_io_reader_from_bytes(exact_input) };
    let exact = ok_value(unsafe { mux_io_reader_read_line(exact_reader) });
    match exact {
        Value::Optional(Some(value)) => match *value {
            Value::String(text) => assert_eq!(text.len(), MAX_STREAM_BYTES),
            other => panic!("expected string line, got {other:?}"),
        },
        other => panic!("expected optional line, got {other:?}"),
    }
    unsafe {
        assert!(mux_rc_dec(exact_reader));
        assert!(mux_rc_dec(exact_input));
    }

    let mut newline_overflow = vec![b'a'; MAX_STREAM_BYTES];
    newline_overflow.push(b'\n');
    let newline_input = mux_rc_alloc(Value::Bytes(newline_overflow));
    let newline_reader = unsafe { mux_io_reader_from_bytes(newline_input) };
    let newline_result = unsafe { mux_io_reader_read_line(newline_reader) };
    assert!(unsafe { mux_result_is_err(newline_result) });
    unsafe {
        assert!(mux_rc_dec(newline_result));
        assert!(mux_rc_dec(newline_reader));
        assert!(mux_rc_dec(newline_input));
    }

    let oversized_input = mux_rc_alloc(Value::Bytes(vec![b'a'; MAX_STREAM_BYTES + 1]));
    let oversized_reader = unsafe { mux_io_reader_from_bytes(oversized_input) };
    let oversized = unsafe { mux_io_reader_read_line(oversized_reader) };
    assert!(unsafe { mux_result_is_err(oversized) });
    unsafe {
        assert!(mux_rc_dec(oversized));
        assert!(mux_rc_dec(oversized_reader));
        assert!(mux_rc_dec(oversized_input));
    }
}

#[test]
fn writer_accumulates_and_returns_a_copy() {
    let writer = mux_io_writer_new();
    let input = mux_rc_alloc(Value::Bytes(b"mux".to_vec()));
    assert_eq!(
        ok_value(unsafe { mux_io_writer_write(writer, input) }),
        Value::Int(3)
    );
    let input2 = mux_rc_alloc(Value::Bytes(b"lang".to_vec()));
    assert_eq!(
        ok_value(unsafe { mux_io_writer_write_all(writer, input2) }),
        Value::Unit
    );
    assert_eq!(
        ok_value(unsafe { mux_io_writer_bytes(writer) }),
        Value::Bytes(b"muxlang".to_vec())
    );
    let flushed = unsafe { mux_io_writer_flush(writer) };
    assert!(matches!(unsafe { &*flushed }, Value::Result(Ok(_))));
    unsafe {
        assert!(mux_rc_dec(flushed));
        assert!(mux_rc_dec(input));
        assert!(mux_rc_dec(input2));
        assert!(mux_rc_dec(writer));
    }
}

#[test]
fn seekable_writer_overwrites_at_bounded_cursor() {
    let writer = mux_io_writer_new();
    let initial = mux_rc_alloc(Value::Bytes(b"abcd".to_vec()));
    assert_eq!(
        ok_value(unsafe { mux_io_writer_write_all(writer, initial) }),
        Value::Unit
    );
    assert_eq!(
        ok_value(unsafe { mux_io_writer_position(writer) }),
        Value::Int(4)
    );
    assert_eq!(
        ok_value(unsafe { mux_io_writer_seek(writer, 1) }),
        Value::Int(1)
    );
    let replacement = mux_rc_alloc(Value::Bytes(b"XY".to_vec()));
    assert_eq!(
        ok_value(unsafe { mux_io_writer_write_all(writer, replacement) }),
        Value::Unit
    );
    assert_eq!(
        ok_value(unsafe { mux_io_writer_bytes(writer) }),
        Value::Bytes(b"aXYd".to_vec())
    );
    unsafe {
        assert!(mux_rc_dec(initial));
        assert!(mux_rc_dec(replacement));
        assert!(mux_rc_dec(writer));
    }
}

#[test]
fn standard_stream_handles_are_non_buffered_and_close_locally() {
    let stdin = mux_io_stdin();
    assert!(!stdin.is_null());
    let remaining = unsafe { mux_io_reader_remaining(stdin) };
    assert!(!unsafe { mux_result_is_ok(remaining) });
    unsafe {
        assert!(mux_rc_dec(remaining));
        mux_io_reader_close(stdin);
        assert!(mux_rc_dec(stdin));
    }

    let stdout = mux_io_stdout();
    let stderr = mux_io_stderr();
    assert!(!stdout.is_null());
    assert!(!stderr.is_null());
    assert_eq!(
        ok_value(unsafe { mux_io_writer_flush(stdout) }),
        Value::Unit
    );
    assert_eq!(
        ok_value(unsafe { mux_io_writer_flush(stderr) }),
        Value::Unit
    );
    let bytes = unsafe { mux_io_writer_bytes(stdout) };
    assert!(!unsafe { mux_result_is_ok(bytes) });
    unsafe {
        assert!(mux_rc_dec(bytes));
        mux_io_writer_close(stdout);
        mux_io_writer_close(stderr);
        assert!(mux_rc_dec(stdout));
        assert!(mux_rc_dec(stderr));
    }
}

#[test]
fn explicit_close_invalidates_stream_handles() {
    let input = mux_rc_alloc(Value::Bytes(b"closed".to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    unsafe { mux_io_reader_close(reader) };
    let read = unsafe { mux_io_reader_read(reader, 1) };
    assert!(unsafe { !mux_result_is_ok(read) });
    unsafe {
        assert!(mux_rc_dec(read));
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(input));
    }

    let writer = mux_io_writer_new();
    unsafe { mux_io_writer_close(writer) };
    let bytes = mux_rc_alloc(Value::Bytes(b"closed".to_vec()));
    let write = unsafe { mux_io_writer_write(writer, bytes) };
    assert!(unsafe { !mux_result_is_ok(write) });
    unsafe {
        assert!(mux_rc_dec(write));
        assert!(mux_rc_dec(bytes));
        assert!(mux_rc_dec(writer));
    }
}

#[test]
fn reader_read_to_end_respects_limit() {
    let input = mux_rc_alloc(Value::Bytes(b"abcdef".to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read_to_end(reader, 4) }),
        Value::Bytes(b"abcd".to_vec())
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read_to_end(reader, 4) }),
        Value::Bytes(b"ef".to_vec())
    );
    unsafe {
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(input));
    }
}

#[test]
fn reader_read_exact_rejects_oversized_requests_before_allocating() {
    let input = mux_rc_alloc(Value::Bytes(b"small".to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    let result = unsafe { mux_io_reader_read_exact(reader, i64::MAX) };
    assert!(!unsafe { mux_result_is_ok(result) });
    unsafe {
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(input));
    }
}

#[test]
fn reader_copy_to_advances_and_respects_limit() {
    let input = mux_rc_alloc(Value::Bytes(b"abcdef".to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    let writer = mux_io_writer_new();
    assert_eq!(
        ok_value(unsafe { mux_io_reader_copy_to(reader, writer, 4) }),
        Value::Int(4)
    );
    assert_eq!(
        ok_value(unsafe { mux_io_writer_bytes(writer) }),
        Value::Bytes(b"abcd".to_vec())
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_remaining(reader) }),
        Value::Int(2)
    );
    unsafe {
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(writer));
        assert!(mux_rc_dec(input));
    }
}

#[test]
fn reader_limit_and_tee_are_explicit_and_composable() {
    let input = mux_rc_alloc(Value::Bytes(b"abcdef".to_vec()));
    let reader = unsafe { mux_io_reader_from_bytes(input) };
    assert_eq!(
        ok_value(unsafe { mux_io_reader_limit(reader, 3) }),
        Value::Unit
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read(reader, 99) }),
        Value::Bytes(b"abc".to_vec())
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read(reader, 1) }),
        Value::Bytes(Vec::new())
    );

    let tee_input = mux_rc_alloc(Value::Bytes(b"hello".to_vec()));
    let tee_reader = unsafe { mux_io_reader_from_bytes(tee_input) };
    let tee_writer = mux_io_writer_new();
    assert_eq!(
        ok_value(unsafe { mux_io_reader_tee(tee_reader, tee_writer) }),
        Value::Unit
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read(tee_reader, 2) }),
        Value::Bytes(b"he".to_vec())
    );
    assert_eq!(
        ok_value(unsafe { mux_io_writer_bytes(tee_writer) }),
        Value::Bytes(b"he".to_vec())
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_untee(tee_reader) }),
        Value::Unit
    );
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read(tee_reader, 8) }),
        Value::Bytes(b"llo".to_vec())
    );
    assert_eq!(
        ok_value(unsafe { mux_io_writer_bytes(tee_writer) }),
        Value::Bytes(b"he".to_vec())
    );

    unsafe {
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(input));
        assert!(mux_rc_dec(tee_reader));
        assert!(mux_rc_dec(tee_writer));
        assert!(mux_rc_dec(tee_input));
    }
}

#[test]
fn file_stream_adapters_are_incremental_and_flushable() {
    let path = temp_stream_path("adapter");
    fs::write(&path, b"file input").expect("seed file");
    let path_value = mux_rc_alloc(Value::String(path.to_string_lossy().into_owned()));
    let reader_result = unsafe { mux_io_reader_from_file(path_value) };
    assert!(unsafe { mux_result_is_ok(reader_result) });
    let reader = unsafe { mux_result_data(reader_result) };
    assert_eq!(
        ok_value(unsafe { mux_io_reader_read_to_end(reader, 64) }),
        Value::Bytes(b"file input".to_vec())
    );
    unsafe {
        assert!(mux_rc_dec(reader_result));
        assert!(mux_rc_dec(reader));
        assert!(mux_rc_dec(path_value));
    }
    let writer_path = temp_stream_path("writer");
    let writer_path_value = mux_rc_alloc(Value::String(writer_path.to_string_lossy().into_owned()));
    let writer_result = unsafe { mux_io_writer_to_file(writer_path_value) };
    assert!(unsafe { mux_result_is_ok(writer_result) });
    let writer = unsafe { mux_result_data(writer_result) };
    let payload = mux_rc_alloc(Value::Bytes(b"written".to_vec()));
    let write = unsafe { mux_io_writer_write_all(writer, payload) };
    assert_eq!(ok_value(write), Value::Unit);
    let flush = unsafe { mux_io_writer_flush(writer) };
    assert_eq!(ok_value(flush), Value::Unit);
    assert_eq!(fs::read(&writer_path).expect("written file"), b"written");
    unsafe {
        assert!(mux_rc_dec(payload));
        assert!(mux_rc_dec(writer));
        assert!(mux_rc_dec(writer_result));
        assert!(mux_rc_dec(writer_path_value));
    }
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(writer_path);
}

#[test]
fn append_file_stream_preserves_existing_contents() {
    let path = temp_stream_path("append");
    fs::write(&path, b"before").expect("seed file");
    let path_value = mux_rc_alloc(Value::String(path.to_string_lossy().into_owned()));
    let writer_result = unsafe { mux_io_writer_append_file(path_value) };
    assert!(unsafe { mux_result_is_ok(writer_result) });
    let writer = unsafe { mux_result_data(writer_result) };
    let position = unsafe { mux_io_writer_position(writer) };
    assert!(!unsafe { mux_result_is_ok(position) });
    unsafe {
        assert!(mux_rc_dec(position));
    }
    let seek = unsafe { mux_io_writer_seek(writer, 0) };
    assert!(!unsafe { mux_result_is_ok(seek) });
    unsafe {
        assert!(mux_rc_dec(seek));
    }
    let payload = mux_rc_alloc(Value::Bytes(b"-after".to_vec()));
    assert_eq!(
        ok_value(unsafe { mux_io_writer_write_all(writer, payload) }),
        Value::Unit
    );
    assert_eq!(
        ok_value(unsafe { mux_io_writer_flush(writer) }),
        Value::Unit
    );
    assert_eq!(fs::read(&path).expect("appended file"), b"before-after");
    unsafe {
        assert!(mux_rc_dec(payload));
        assert!(mux_rc_dec(writer));
        assert!(mux_rc_dec(writer_result));
        assert!(mux_rc_dec(path_value));
    }
    let _ = fs::remove_file(path);
}
