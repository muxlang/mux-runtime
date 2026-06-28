//! Unit tests for the SQL layer against an in-memory SQLite database
//! (feature-gated behind `sql`). Postgres/MySQL paths need live servers and are
//! not exercised here.
#![cfg(feature = "sql")]

mod common;

use std::ffi::CString;

use common::{assert_err, assert_ok};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::sql::*;
use mux_runtime::Value;

fn sval(s: &str) -> *mut Value {
    mux_rc_alloc(Value::String(s.to_string()))
}

/// Assert Ok and return the inner value (caller frees it). Frees the result.
fn ok_data(r: *mut Value) -> *mut Value {
    assert!(mux_result_is_ok(r), "expected Ok result");
    let data = mux_result_data(r);
    assert!(!data.is_null());
    assert!(mux_rc_dec(r));
    data
}

fn connect_memory() -> *mut Value {
    let uri = CString::new("sqlite::memory:").unwrap();
    ok_data(unsafe { mux_sql_connect(uri.as_ptr()) })
}

#[test]
fn connect_execute_query_lifecycle() {
    let conn = connect_memory();

    let create = sval("CREATE TABLE t (id INTEGER, name TEXT)");
    assert_ok(mux_sql_connection_execute(conn, create));
    assert!(mux_rc_dec(create));

    // parameterized insert
    let insert = sval("INSERT INTO t (id, name) VALUES (?, ?)");
    let params = mux_rc_alloc(Value::List(vec![
        Value::Int(1),
        Value::String("alice".into()),
    ]));
    assert_ok(mux_sql_connection_execute_params(conn, insert, params));
    assert!(mux_rc_dec(insert));
    assert!(mux_rc_dec(params));

    // query and inspect the resultset
    let select = sval("SELECT id, name FROM t");
    let rs = ok_data(mux_sql_connection_query(conn, select));
    assert!(mux_rc_dec(select));

    // Resultset accessors return bare List/Optional values (not Result).
    let cols = mux_sql_resultset_columns(rs);
    assert!(!cols.is_null());
    assert!(mux_rc_dec(cols));
    let rows = mux_sql_resultset_rows(rs);
    assert!(!rows.is_null());
    assert!(mux_rc_dec(rows));
    let next = mux_sql_resultset_next(rs);
    assert!(!next.is_null());
    assert!(mux_rc_dec(next));
    assert!(mux_rc_dec(rs));

    mux_sql_connection_close(conn);
    assert!(mux_rc_dec(conn));
}

#[test]
fn transaction_commit_and_rollback() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE t (n INTEGER)");
    assert_ok(mux_sql_connection_execute(conn, create));
    assert!(mux_rc_dec(create));

    // commit path
    let tx = ok_data(mux_sql_connection_begin_transaction(conn));
    let ins = sval("INSERT INTO t (n) VALUES (1)");
    assert_ok(mux_sql_transaction_execute(tx, ins));
    assert!(mux_rc_dec(ins));
    assert_ok(mux_sql_transaction_commit(tx));
    assert!(mux_rc_dec(tx));

    // rollback path
    let tx2 = ok_data(mux_sql_connection_begin_transaction(conn));
    let ins2 = sval("INSERT INTO t (n) VALUES (2)");
    assert_ok(mux_sql_transaction_execute(tx2, ins2));
    assert!(mux_rc_dec(ins2));
    assert_ok(mux_sql_transaction_rollback(tx2));
    assert!(mux_rc_dec(tx2));

    mux_sql_connection_close(conn);
    assert!(mux_rc_dec(conn));
}

#[test]
fn sql_value_constructors_and_accessors() {
    let i = mux_sql_value_int(42);
    assert_ok(mux_sql_value_as_int(i));
    assert!(mux_rc_dec(i));

    let f = mux_sql_value_float(1.5);
    assert_ok(mux_sql_value_as_float(f));
    assert!(mux_rc_dec(f));

    let b = mux_sql_value_bool(true);
    assert_ok(mux_sql_value_as_bool(b));
    assert!(mux_rc_dec(b));

    let s = unsafe { mux_sql_value_string(CString::new("hi").unwrap().as_ptr()) };
    assert_ok(mux_sql_value_as_string(s));
    assert!(mux_rc_dec(s));

    let null = mux_sql_value_null();
    assert!(mux_sql_value_is_null(null));
    assert!(mux_rc_dec(null));

    let not_null = mux_sql_value_int(7);
    assert!(!mux_sql_value_is_null(not_null));
    assert!(mux_rc_dec(not_null));
}

#[test]
fn query_params_and_errors() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE t (id INTEGER, name TEXT)");
    assert_ok(mux_sql_connection_execute(conn, create));
    assert!(mux_rc_dec(create));

    let insert = sval("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')");
    assert_ok(mux_sql_connection_execute(conn, insert));
    assert!(mux_rc_dec(insert));

    // parameterized query
    let sel = sval("SELECT id, name FROM t WHERE id = ?");
    let params = mux_rc_alloc(Value::List(vec![Value::Int(1)]));
    let rs = ok_data(mux_sql_connection_query_params(conn, sel, params));
    assert!(mux_rc_dec(sel));
    assert!(mux_rc_dec(params));
    let rows = mux_sql_resultset_rows(rs);
    assert!(!rows.is_null());
    assert!(mux_rc_dec(rows));
    assert!(mux_rc_dec(rs));

    // invalid SQL surfaces as an error
    let bad_sql = sval("THIS IS NOT SQL");
    assert_err(mux_sql_connection_execute(conn, bad_sql));
    assert!(mux_rc_dec(bad_sql));
    let bad_q = sval("SELECT * FROM table_that_does_not_exist");
    assert_err(mux_sql_connection_query(conn, bad_q));
    assert!(mux_rc_dec(bad_q));

    mux_sql_connection_close(conn);
    assert!(mux_rc_dec(conn));
}

#[test]
fn sql_value_bytes_and_type_errors() {
    // bytes value round trip
    let list = mux_rc_alloc(Value::List(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(255),
    ]));
    let bytes = mux_sql_value_bytes(list);
    assert!(!bytes.is_null());
    assert_ok(mux_sql_value_as_bytes(bytes));
    assert!(mux_rc_dec(bytes));
    assert!(mux_rc_dec(list));

    // accessor type mismatch is an error
    let s = unsafe { mux_sql_value_string(CString::new("nope").unwrap().as_ptr()) };
    assert_err(mux_sql_value_as_int(s));
    assert!(mux_rc_dec(s));
}

#[test]
fn bad_uri_is_error() {
    let uri = CString::new("notarealscheme://x").unwrap();
    assert_err(unsafe { mux_sql_connect(uri.as_ptr()) });
}

#[test]
fn null_value_accessors_error() {
    assert_err(mux_sql_value_as_int(std::ptr::null()));
    assert!(mux_sql_value_is_null(std::ptr::null()));
}
