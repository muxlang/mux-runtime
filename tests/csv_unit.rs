//! Unit tests for the CSV layer (feature-gated behind `csv`).
#![cfg(feature = "csv")]
#![allow(clippy::mutable_key_type)]

use std::ffi::CString;

use mux_runtime::data::{
    mux_csv_parse, mux_csv_parse_with_headers, mux_csv_parse_with_options,
    mux_csv_reader_from_bytes, mux_csv_reader_from_reader, mux_csv_reader_headers,
    mux_csv_reader_read, mux_csv_to_string, mux_csv_to_string_with, mux_csv_writer_bytes,
    mux_csv_writer_from_config, mux_csv_writer_from_writer, mux_csv_writer_write,
};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_err, mux_result_is_ok};
use mux_runtime::std::mux_csv_error_message;
use mux_runtime::stream::{mux_io_reader_from_bytes, mux_io_writer_bytes, mux_io_writer_new};
use mux_runtime::Value;

/// The `ok` payload of a result, asserting it is one.
fn ok_data(result: *mut Value) -> *mut Value {
    let data = unsafe {
        assert!(mux_result_is_ok(result), "expected an ok result");
        mux_result_data(result)
    };
    assert!(!data.is_null());
    assert!(unsafe { mux_rc_dec(result) });
    data
}

#[test]
fn parse_plain_csv() {
    let input = CString::new("a,b\n1,2\n").unwrap();
    let res = unsafe { mux_csv_parse(input.as_ptr()) };
    assert!(unsafe { mux_result_is_ok(res) });
    assert!(unsafe { mux_rc_dec(res) });
}

#[test]
fn parse_with_headers() {
    let input = CString::new("name,age\nAlice,30\nBob,25\n").unwrap();
    let res = unsafe { mux_csv_parse_with_headers(input.as_ptr()) };
    assert!(unsafe { mux_result_is_ok(res) });
    assert!(unsafe { mux_rc_dec(res) });
}

#[test]
fn parse_and_render_with_explicit_dialect() {
    let input = CString::new(" name ; age \n Alice ; 30\n").unwrap();
    let parsed =
        unsafe { mux_csv_parse_with_options(input.as_ptr(), b';'.into(), b'"'.into(), 1, 1, 0) };
    let table = ok_data(parsed);
    let rendered = unsafe { mux_csv_to_string_with(table, b';'.into(), b'"'.into()) };
    assert!(unsafe { mux_result_is_ok(rendered) });
    assert!(unsafe { mux_rc_dec(rendered) });
    assert!(unsafe { mux_rc_dec(table) });
}

#[test]
fn dialect_rejects_invalid_control_bytes() {
    let input = CString::new("a,b\n1,2\n").unwrap();
    let parsed = unsafe { mux_csv_parse_with_options(input.as_ptr(), 0, b'"'.into(), 0, 0, 0) };
    assert!(unsafe { mux_result_is_err(parsed) });
    assert!(unsafe { mux_rc_dec(parsed) });

    for invalid in [0, b'\r', b'\n', 0x80] {
        let writer = unsafe { mux_csv_writer_from_config(invalid.into(), b'"'.into()) };
        assert!(unsafe { mux_result_is_err(writer) });
        assert!(unsafe { mux_rc_dec(writer) });

        let writer = unsafe { mux_csv_writer_from_config(b','.into(), invalid.into()) };
        assert!(unsafe { mux_result_is_err(writer) });
        assert!(unsafe { mux_rc_dec(writer) });
    }
}

#[test]
fn parse_null_is_error() {
    let res = unsafe { mux_csv_parse(std::ptr::null()) };
    assert!(unsafe { mux_result_is_err(res) });
    assert!(unsafe { mux_rc_dec(res) });
}

#[test]
fn invalid_utf8_and_oversized_input_are_errors() {
    let invalid = [0xff_u8, 0];
    let parsed = unsafe { mux_csv_parse(invalid.as_ptr().cast()) };
    assert!(unsafe { mux_result_is_err(parsed) });
    assert!(unsafe { mux_rc_dec(parsed) });

    let oversized = vec![b'a'; 16 * 1024 * 1024 + 1];
    let mut nul_terminated = oversized;
    nul_terminated.push(0);
    let parsed = unsafe { mux_csv_parse(nul_terminated.as_ptr().cast()) };
    assert!(unsafe { mux_result_is_err(parsed) });
    assert!(unsafe { mux_rc_dec(parsed) });
}

#[test]
fn streaming_reader_and_writer_round_trip() {
    let input = mux_rc_alloc(Value::Bytes(b"a,b\n1,2\n".to_vec()));
    let reader_result = unsafe { mux_csv_reader_from_bytes(input, 1) };
    assert!(unsafe { mux_result_is_ok(reader_result) });
    let reader = ok_data(reader_result);
    let headers = unsafe { mux_csv_reader_headers(reader) };
    assert!(unsafe { mux_result_is_ok(headers) });
    assert!(unsafe { mux_rc_dec(headers) });
    let row = unsafe { mux_csv_reader_read(reader) };
    assert!(unsafe { mux_result_is_ok(row) });
    assert!(unsafe { mux_rc_dec(row) });
    assert!(unsafe { mux_rc_dec(reader) });
    assert!(unsafe { mux_rc_dec(input) });

    let writer_result = unsafe { mux_csv_writer_from_config(b','.into(), b'"'.into()) };
    let writer = ok_data(writer_result);
    let fields = mux_rc_alloc(Value::List(vec![
        Value::String("x".to_string()),
        Value::String("y".to_string()),
    ]));
    let wrote = unsafe { mux_csv_writer_write(writer, fields) };
    assert!(unsafe { mux_result_is_ok(wrote) });
    assert!(unsafe { mux_rc_dec(wrote) });
    assert!(unsafe { mux_rc_dec(fields) });
    let output = unsafe { mux_csv_writer_bytes(writer) };
    assert!(unsafe { mux_result_is_ok(output) });
    assert!(unsafe { mux_rc_dec(output) });
    assert!(unsafe { mux_rc_dec(writer) });
}

#[test]
fn streaming_reader_and_writer_use_io_handles_incrementally() {
    let input = mux_rc_alloc(Value::Bytes(b"name,age\nAda,36\n".to_vec()));
    let source = unsafe { mux_io_reader_from_bytes(input) };
    assert!(!source.is_null());
    let reader_result = unsafe { mux_csv_reader_from_reader(source, 1) };
    assert!(unsafe { mux_result_is_ok(reader_result) });
    let reader = ok_data(reader_result);
    let headers = unsafe { mux_csv_reader_headers(reader) };
    assert!(unsafe { mux_result_is_ok(headers) });
    assert!(unsafe { mux_rc_dec(headers) });
    let row = unsafe { mux_csv_reader_read(reader) };
    assert!(unsafe { mux_result_is_ok(row) });
    assert!(unsafe { mux_rc_dec(row) });
    assert!(unsafe { mux_rc_dec(reader) });
    assert!(unsafe { mux_rc_dec(source) });
    assert!(unsafe { mux_rc_dec(input) });

    let destination = mux_io_writer_new();
    assert!(!destination.is_null());
    let writer_result =
        unsafe { mux_csv_writer_from_writer(destination, b','.into(), b'"'.into()) };
    assert!(unsafe { mux_result_is_ok(writer_result) });
    let writer = ok_data(writer_result);
    let fields = mux_rc_alloc(Value::List(vec![
        Value::String("x".to_string()),
        Value::String("y".to_string()),
    ]));
    let wrote = unsafe { mux_csv_writer_write(writer, fields) };
    assert!(unsafe { mux_result_is_ok(wrote) });
    assert!(unsafe { mux_rc_dec(wrote) });
    assert!(unsafe { mux_rc_dec(fields) });
    let flushed = unsafe { mux_runtime::data::mux_csv_writer_flush(writer) };
    assert!(unsafe { mux_result_is_ok(flushed) });
    assert!(unsafe { mux_rc_dec(flushed) });
    let output = unsafe { mux_io_writer_bytes(destination) };
    assert!(unsafe { mux_result_is_ok(output) });
    let bytes = unsafe { mux_result_data(output) };
    assert_eq!(unsafe { &*bytes }, &Value::Bytes(b"x,y\n".to_vec()));
    assert!(unsafe { mux_rc_dec(bytes) });
    assert!(unsafe { mux_rc_dec(output) });
    assert!(unsafe { mux_rc_dec(writer) });
    assert!(unsafe { mux_rc_dec(destination) });
}

#[test]
fn to_string_roundtrip() {
    // Build the {headers, rows} map shape the writer expects.
    let mut map = mux_runtime::ordered::OrderedMap::new();
    map.insert(
        Value::String("headers".to_string()),
        Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]),
    );
    map.insert(
        Value::String("rows".to_string()),
        Value::List(vec![Value::List(vec![
            Value::String("1".to_string()),
            Value::String("2".to_string()),
        ])]),
    );
    let csv_val = mux_rc_alloc(Value::Map(map));

    let res = unsafe { mux_csv_to_string(csv_val) };
    assert!(unsafe { mux_result_is_ok(res) });

    assert!(unsafe { mux_rc_dec(res) });
    assert!(unsafe { mux_rc_dec(csv_val) });
}

#[test]
fn to_string_rejects_non_map() {
    let bad = mux_rc_alloc(Value::Int(1));
    let res = unsafe { mux_csv_to_string(bad) };
    assert!(unsafe { mux_result_is_err(res) });
    assert!(unsafe { mux_rc_dec(res) });
    assert!(unsafe { mux_rc_dec(bad) });
}

/// Rows pair with headers by position, and every cell stays a string.
///
/// The parsed form keeps headers and rows apart, so a typed reader would have
/// to find each column's index per row. This does the pairing once. CSV has no
/// types, so deciding a column is a number is the reader's job.
#[test]
fn rows_as_maps_pairs_headers_with_cells() {
    use mux_runtime::data::mux_csv_rows_as_maps;
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::Value;
    use std::ffi::CString;

    let text = CString::new("sku,qty\nwidget,3\ngadget,7\n").expect("no interior nul");
    let parsed = unsafe { mux_csv_parse_with_headers(text.as_ptr()) };
    let table = ok_data(parsed);

    let got = unsafe { mux_csv_rows_as_maps(table) };
    let rows = match unsafe { &*got } {
        Value::Result(Ok(inner)) => match inner.as_ref() {
            Value::List(rows) => rows.clone(),
            other => panic!("expected a list, got {other:?}"),
        },
        other => panic!("expected ok(list), got {other:?}"),
    };
    assert_eq!(rows.len(), 2);

    let first = match &rows[0] {
        Value::Map(m) => m,
        other => panic!("expected a map, got {other:?}"),
    };
    assert_eq!(
        first.get(&Value::String("sku".into())),
        Some(&Value::String("widget".into()))
    );
    // A number stays text - CSV has no types.
    assert_eq!(
        first.get(&Value::String("qty".into())),
        Some(&Value::String("3".into()))
    );

    assert!(unsafe { mux_rc_dec(got) });
    assert!(unsafe { mux_rc_dec(table) });
}

/// A ragged row never reaches this function: the parser rejects a row whose
/// length does not match the header, so pairing never sees a short one.
///
/// Worth pinning, because the obvious worry about zipping headers with cells is
/// what happens when they disagree - and the answer is that they cannot.
#[test]
fn a_ragged_row_is_rejected_by_the_parser() {
    use std::ffi::CString;

    let text = CString::new("sku,qty\nwidget\n").expect("no interior nul");
    let parsed = unsafe { mux_csv_parse_with_headers(text.as_ptr()) };
    assert!(
        unsafe { mux_result_is_err(parsed) },
        "a row shorter than the header must not parse"
    );
    assert!(unsafe { mux_rc_dec(parsed) });
}
/// A repeated header is REJECTED, naming the column.
///
/// Keying by name cannot represent two columns called the same thing, so one
/// would have to be dropped - and dropping a source column without saying so is
/// the one answer a reader cannot recover from. Which one survived would also
/// be an arbitrary rule to remember.
#[test]
fn a_repeated_header_is_rejected() {
    use mux_runtime::data::mux_csv_rows_as_maps;
    use std::ffi::CString;

    let text = CString::new("sku,qty,sku\nwidget,3,gadget\n").expect("no interior nul");
    let table = ok_data(unsafe { mux_csv_parse_with_headers(text.as_ptr()) });
    let got = unsafe { mux_csv_rows_as_maps(table) };

    assert!(
        unsafe { mux_result_is_err(got) },
        "a repeated header must not pair silently"
    );
    let detail = unsafe { mux_result_data(got) };
    let message = unsafe { mux_csv_error_message(detail) };
    let message_value = unsafe { &*message };
    match message_value {
        Value::String(message) => assert!(
            message.contains("duplicate column 'sku'"),
            "the message must name the column, got {message}"
        ),
        other => panic!("expected a message, got {other:?}"),
    }

    assert!(unsafe { mux_rc_dec(message) });
    assert!(unsafe { mux_rc_dec(detail) });
    assert!(unsafe { mux_rc_dec(got) });
    assert!(unsafe { mux_rc_dec(table) });
}

/// Anything that is not a parsed CSV table is an error, not a panic.
#[test]
fn rows_as_maps_rejects_other_shapes() {
    use mux_runtime::data::mux_csv_rows_as_maps;

    for input in [Value::Int(1), Value::String("csv".into())] {
        let got = unsafe { mux_csv_rows_as_maps(&raw const input) };
        assert!(unsafe { mux_result_is_err(got) });
        assert!(unsafe { mux_rc_dec(got) });
    }

    let got = unsafe { mux_csv_rows_as_maps(std::ptr::null()) };
    assert!(unsafe { mux_result_is_err(got) });
    assert!(unsafe { mux_rc_dec(got) });
}
