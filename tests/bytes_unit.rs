//! Unit coverage for the contiguous built-in bytes value.

use mux_runtime::bytes::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::mux_int_value;
use mux_runtime::Value;

#[test]
fn bytes_mutation_and_indexing_are_contiguous_value_operations() {
    let bytes = mux_bytes_new();
    let zero = mux_int_value(0);
    let max = mux_int_value(255);
    unsafe {
        mux_bytes_push_back(bytes, zero);
        mux_bytes_push_back(bytes, max);
    }
    assert_eq!(unsafe { mux_bytes_length(bytes) }, 2);
    assert!(unsafe { mux_bytes_contains(bytes, max) });

    let last = unsafe { mux_bytes_get(bytes, -1) };
    assert!(
        matches!(unsafe { &*last }, Value::Optional(Some(inner)) if **inner == Value::Int(255))
    );

    unsafe {
        assert!(mux_rc_dec(last));
        assert!(mux_rc_dec(zero));
        assert!(mux_rc_dec(max));
        assert!(mux_rc_dec(bytes));
    }
}

#[test]
fn bytes_conversion_checks_utf8_and_copies_lists() {
    let list =
        mux_runtime::refcount::mux_rc_alloc(Value::List(vec![Value::Int(0x68), Value::Int(0x69)]));
    let converted = unsafe { mux_bytes_from_list(list) };
    assert!(
        matches!(unsafe { &*converted }, Value::Result(Ok(inner)) if **inner == Value::Bytes(vec![0x68, 0x69]))
    );
    unsafe {
        assert!(mux_rc_dec(converted));
        assert!(mux_rc_dec(list));
    }

    let text = mux_runtime::refcount::mux_rc_alloc(Value::Bytes(vec![0xc3, 0xa9]));
    let utf8 = unsafe { mux_bytes_to_utf8(text) };
    assert!(
        matches!(unsafe { &*utf8 }, Value::Result(Ok(inner)) if **inner == Value::String("é".to_string()))
    );
    unsafe {
        assert!(mux_rc_dec(utf8));
        assert!(mux_rc_dec(text));
    }
}

#[test]
fn bytes_mutators_and_binary_codecs_validate_bounds() {
    let bytes = mux_bytes_new();
    let one = mux_int_value(1);
    let two = mux_int_value(2);
    let nine = mux_int_value(9);
    unsafe {
        mux_bytes_push_back(bytes, one);
        mux_bytes_push_back(bytes, two);
        mux_bytes_insert(bytes, 1, nine);
    }
    assert_eq!(unsafe { mux_bytes_length(bytes) }, 3);
    let removed = unsafe { mux_bytes_remove(bytes, -1) };
    assert!(
        matches!(unsafe { &*removed }, Value::Optional(Some(inner)) if **inner == Value::Int(2))
    );

    let wire = mux_runtime::refcount::mux_rc_alloc(Value::Bytes(vec![0; 8]));
    let encoded = unsafe { mux_bytes_write_uint(wire, 0, 0x1234, 2, true) };
    assert!(matches!(unsafe { &*encoded }, Value::Result(Ok(inner)) if **inner == Value::Unit));
    let decoded = unsafe { mux_bytes_read_uint(wire, 0, 2, true) };
    assert!(
        matches!(unsafe { &*decoded }, Value::Result(Ok(inner)) if **inner == Value::Int(0x1234))
    );

    unsafe {
        assert!(mux_rc_dec(removed));
        assert!(mux_rc_dec(encoded));
        assert!(mux_rc_dec(decoded));
        assert!(mux_rc_dec(one));
        assert!(mux_rc_dec(two));
        assert!(mux_rc_dec(nine));
        assert!(mux_rc_dec(wire));
        assert!(mux_rc_dec(bytes));
    }
}

#[test]
fn bytes_format_rejects_unbounded_widths() {
    let value = mux_rc_alloc(Value::Bytes(vec![0x2a]));
    let result = unsafe { mux_bytes_format(value, 16, i64::MAX) };
    assert!(!unsafe { mux_result_is_ok(result) });
    unsafe {
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(value));
    }
}

#[test]
fn bytes_growth_operations_ignore_unrepresentable_sizes() {
    let bytes = mux_bytes_new();
    let fill = mux_int_value(0xaa);
    unsafe {
        mux_bytes_reserve(bytes, i64::MAX);
        mux_bytes_resize(bytes, i64::MAX, fill);
    }
    assert_eq!(unsafe { mux_bytes_length(bytes) }, 0);
    unsafe {
        assert!(mux_rc_dec(fill));
        assert!(mux_rc_dec(bytes));
    }
}

#[test]
fn bytes_varint_rejects_overflowing_tenth_byte() {
    let input = mux_rc_alloc(Value::Bytes(vec![
        0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02,
    ]));
    let result = unsafe { mux_bytes_read_varint(input, 0) };
    assert!(matches!(unsafe { &*result }, Value::Result(Err(_))));
    unsafe {
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(input));
    }
}

#[test]
fn bytes_cursor_reads_sequentially_and_returns_written_buffer() {
    let source = mux_runtime::refcount::mux_rc_alloc(Value::Bytes(vec![0x34, 0x12, 0xaa]));
    let cursor_result = unsafe { mux_bytes_cursor_new(source) };
    assert!(unsafe { mux_result_is_ok(cursor_result) });
    let cursor = unsafe { mux_result_data(cursor_result) };
    unsafe {
        assert!(mux_rc_dec(cursor_result));
    }

    let first = unsafe { mux_bytes_cursor_read_uint(cursor, 2, true) };
    assert!(
        matches!(unsafe { &*first }, Value::Result(Ok(inner)) if **inner == Value::Int(0x1234))
    );
    let position = unsafe { mux_bytes_cursor_position(cursor) };
    assert!(matches!(unsafe { &*position }, Value::Result(Ok(inner)) if **inner == Value::Int(2)));
    let rest = unsafe { mux_bytes_cursor_read_bytes(cursor, 1) };
    assert!(
        matches!(unsafe { &*rest }, Value::Result(Ok(inner)) if **inner == Value::Bytes(vec![0xaa]))
    );

    let output = unsafe { mux_bytes_cursor_into_bytes(cursor) };
    assert!(
        matches!(unsafe { &*output }, Value::Result(Ok(inner)) if **inner == Value::Bytes(vec![0x34, 0x12, 0xaa]))
    );

    unsafe {
        assert!(mux_rc_dec(first));
        assert!(mux_rc_dec(position));
        assert!(mux_rc_dec(rest));
        assert!(mux_rc_dec(output));
        assert!(mux_rc_dec(cursor));
        assert!(mux_rc_dec(source));
    }
}

#[test]
fn bytes_cursor_writes_and_grows() {
    let source = mux_runtime::refcount::mux_rc_alloc(Value::Bytes(Vec::new()));
    let cursor_result = unsafe { mux_bytes_cursor_new(source) };
    let cursor = unsafe { mux_result_data(cursor_result) };
    unsafe {
        assert!(mux_rc_dec(cursor_result));
    }
    let wrote = unsafe { mux_bytes_cursor_write_uint(cursor, 0xabcd, 2, false) };
    assert!(matches!(unsafe { &*wrote }, Value::Result(Ok(inner)) if **inner == Value::Unit));
    let bytes = unsafe { mux_bytes_cursor_into_bytes(cursor) };
    assert!(
        matches!(unsafe { &*bytes }, Value::Result(Ok(inner)) if **inner == Value::Bytes(vec![0xab, 0xcd]))
    );
    unsafe {
        assert!(mux_rc_dec(wrote));
        assert!(mux_rc_dec(bytes));
        assert!(mux_rc_dec(cursor));
        assert!(mux_rc_dec(source));
    }
}
