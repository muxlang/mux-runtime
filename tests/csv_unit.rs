//! Unit tests for the CSV layer (feature-gated behind `csv`).
#![cfg(feature = "csv")]
#![allow(clippy::mutable_key_type)]

use std::ffi::CString;

use mux_runtime::data::{mux_csv_parse, mux_csv_parse_with_headers, mux_csv_to_string};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_err, mux_result_is_ok};
use mux_runtime::Value;

/// The `ok` payload of a result, asserting it is one.
fn ok_data(result: *mut Value) -> *mut Value {
    assert!(mux_result_is_ok(result), "expected an ok result");
    let data = mux_result_data(result);
    assert!(!data.is_null());
    assert!(unsafe { mux_rc_dec(result) });
    data
}

#[test]
fn parse_plain_csv() {
    let input = CString::new("a,b\n1,2\n").unwrap();
    let res = mux_csv_parse(input.as_ptr());
    assert!(mux_result_is_ok(res));
    assert!(unsafe { mux_rc_dec(res) });
}

#[test]
fn parse_with_headers() {
    let input = CString::new("name,age\nAlice,30\nBob,25\n").unwrap();
    let res = mux_csv_parse_with_headers(input.as_ptr());
    assert!(mux_result_is_ok(res));
    assert!(unsafe { mux_rc_dec(res) });
}

#[test]
fn parse_null_is_error() {
    let res = mux_csv_parse(std::ptr::null());
    assert!(mux_result_is_err(res));
    assert!(unsafe { mux_rc_dec(res) });
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

    let res = mux_csv_to_string(csv_val);
    assert!(mux_result_is_ok(res));

    assert!(unsafe { mux_rc_dec(res) });
    assert!(unsafe { mux_rc_dec(csv_val) });
}

#[test]
fn to_string_rejects_non_map() {
    let bad = mux_rc_alloc(Value::Int(1));
    let res = mux_csv_to_string(bad);
    assert!(mux_result_is_err(res));
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
    let parsed = mux_csv_parse_with_headers(text.as_ptr());
    let table = ok_data(parsed);

    let got = mux_csv_rows_as_maps(table);
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
    let parsed = mux_csv_parse_with_headers(text.as_ptr());
    assert!(
        mux_result_is_err(parsed),
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
    let table = ok_data(mux_csv_parse_with_headers(text.as_ptr()));
    let got = mux_csv_rows_as_maps(table);

    assert!(
        mux_result_is_err(got),
        "a repeated header must not pair silently"
    );
    let detail = mux_result_data(got);
    match unsafe { &*detail } {
        Value::String(message) => assert!(
            message.contains("duplicate column 'sku'"),
            "the message must name the column, got {message}"
        ),
        other => panic!("expected a message, got {other:?}"),
    }

    assert!(unsafe { mux_rc_dec(detail) });
    assert!(unsafe { mux_rc_dec(got) });
    assert!(unsafe { mux_rc_dec(table) });
}

/// Anything that is not a parsed CSV table is an error, not a panic.
#[test]
fn rows_as_maps_rejects_other_shapes() {
    use mux_runtime::data::mux_csv_rows_as_maps;

    for input in [Value::Int(1), Value::String("csv".into())] {
        let got = mux_csv_rows_as_maps(&raw const input);
        assert!(mux_result_is_err(got));
        assert!(unsafe { mux_rc_dec(got) });
    }

    let got = mux_csv_rows_as_maps(std::ptr::null());
    assert!(mux_result_is_err(got));
    assert!(unsafe { mux_rc_dec(got) });
}
