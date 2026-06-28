//! Driver-level SQL coverage for Postgres and MySQL (feature-gated behind `sql`).
//!
//! These connect to live servers and are skipped unless the corresponding env
//! var is set, so `cargo test` stays green without databases:
//!
//! ```text
//! MUX_TEST_POSTGRES_URL  e.g. postgres://user:pass@localhost:5432/db
//! MUX_TEST_MYSQL_URL     e.g. mysql://user:pass@localhost:3306/db
//! ```
//!
//! CI sets both (via service containers) so the postgres_*/mysql_* code paths run.
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

fn ok_data(r: *mut Value) -> *mut Value {
    assert!(mux_result_is_ok(r), "expected Ok result");
    let data = mux_result_data(r);
    assert!(!data.is_null());
    assert!(mux_rc_dec(r));
    data
}

fn connect(uri: &str) -> *mut Value {
    let c = CString::new(uri).unwrap();
    ok_data(unsafe { mux_sql_connect(c.as_ptr()) })
}

fn exec(conn: *mut Value, sql: &str) {
    let s = sval(sql);
    assert_ok(mux_sql_connection_execute(conn, s));
    assert!(mux_rc_dec(s));
}

/// Run the full driver exercise. `ph` builds the placeholder for a 1-based index
/// ("$1" for Postgres, "?" for MySQL).
fn run_driver_suite(uri: &str, ph: impl Fn(usize) -> String) {
    let conn = connect(uri);

    exec(conn, "DROP TABLE IF EXISTS mux_cov_t");
    // BIGINT/DOUBLE PRECISION/BOOLEAN map cleanly to i64/f64/bool on both drivers.
    exec(
        conn,
        "CREATE TABLE mux_cov_t (id BIGINT, name TEXT, score DOUBLE PRECISION, flag BOOLEAN)",
    );

    // parameterized insert exercises the param binding + type mapping
    let insert_sql = format!(
        "INSERT INTO mux_cov_t (id, name, score, flag) VALUES ({}, {}, {}, {})",
        ph(1),
        ph(2),
        ph(3),
        ph(4)
    );
    let insert = sval(&insert_sql);
    let params = mux_rc_alloc(Value::List(vec![
        Value::Int(1),
        Value::String("alice".into()),
        Value::Float(ordered_float::OrderedFloat(1.5)),
        Value::Bool(true),
    ]));
    assert_ok(mux_sql_connection_execute_params(conn, insert, params));
    assert!(mux_rc_dec(insert));
    assert!(mux_rc_dec(params));

    // query exercises row/value conversion for INT/TEXT/FLOAT/BOOL columns
    let select = sval("SELECT id, name, score, flag FROM mux_cov_t");
    let rs = ok_data(mux_sql_connection_query(conn, select));
    assert!(mux_rc_dec(select));
    let cols = mux_sql_resultset_columns(rs);
    assert!(!cols.is_null());
    assert!(mux_rc_dec(cols));
    let rows = mux_sql_resultset_rows(rs);
    assert!(!rows.is_null());
    assert!(mux_rc_dec(rows));
    assert!(mux_rc_dec(rs));

    // parameterized query
    let qsql = format!("SELECT name FROM mux_cov_t WHERE id = {}", ph(1));
    let q = sval(&qsql);
    let qparams = mux_rc_alloc(Value::List(vec![Value::Int(1)]));
    let rs2 = ok_data(mux_sql_connection_query_params(conn, q, qparams));
    assert!(mux_rc_dec(q));
    assert!(mux_rc_dec(qparams));
    assert!(mux_rc_dec(rs2));

    // transactions: commit then rollback
    let tx = ok_data(mux_sql_connection_begin_transaction(conn));
    let ti = sval("INSERT INTO mux_cov_t (id, name, score, flag) VALUES (2, 'b', 2.5, false)");
    assert_ok(mux_sql_transaction_execute(tx, ti));
    assert!(mux_rc_dec(ti));
    assert_ok(mux_sql_transaction_commit(tx));
    assert!(mux_rc_dec(tx));

    let tx2 = ok_data(mux_sql_connection_begin_transaction(conn));
    let ti2 = sval("INSERT INTO mux_cov_t (id, name, score, flag) VALUES (3, 'c', 3.5, true)");
    assert_ok(mux_sql_transaction_execute(tx2, ti2));
    assert!(mux_rc_dec(ti2));
    assert_ok(mux_sql_transaction_rollback(tx2));
    assert!(mux_rc_dec(tx2));

    // invalid SQL surfaces as an error through the driver
    let bad = sval("THIS IS NOT VALID SQL");
    assert_err(mux_sql_connection_execute(conn, bad));
    assert!(mux_rc_dec(bad));

    exec(conn, "DROP TABLE IF EXISTS mux_cov_t");
    mux_sql_connection_close(conn);
    assert!(mux_rc_dec(conn));
}

#[test]
fn postgres_driver() {
    let Ok(uri) = std::env::var("MUX_TEST_POSTGRES_URL") else {
        eprintln!("skipping postgres_driver: MUX_TEST_POSTGRES_URL not set");
        return;
    };
    run_driver_suite(&uri, |i| format!("${}", i));
}

#[test]
fn mysql_driver() {
    let Ok(uri) = std::env::var("MUX_TEST_MYSQL_URL") else {
        eprintln!("skipping mysql_driver: MUX_TEST_MYSQL_URL not set");
        return;
    };
    run_driver_suite(&uri, |_| "?".to_string());
}

/// Postgres column types map through distinct branches of postgres_query_value
/// (INT2/INT4/FLOAT4/BYTEA + the NULL -> Unit path); use literal SQL so the
/// server parses each literal into its column type.
#[test]
fn postgres_column_types() {
    let Ok(uri) = std::env::var("MUX_TEST_POSTGRES_URL") else {
        eprintln!("skipping postgres_column_types: MUX_TEST_POSTGRES_URL not set");
        return;
    };
    let conn = connect(&uri);
    exec(conn, "DROP TABLE IF EXISTS mux_types");
    exec(
        conn,
        "CREATE TABLE mux_types (a SMALLINT, b INT, c REAL, d BYTEA, e TEXT)",
    );
    exec(
        conn,
        "INSERT INTO mux_types (a, b, c, d, e) VALUES (1, 2, 3.5, '\\x0102'::bytea, NULL)",
    );

    let select = sval("SELECT a, b, c, d, e FROM mux_types");
    let rs = ok_data(mux_sql_connection_query(conn, select));
    assert!(mux_rc_dec(select));
    let rows = mux_sql_resultset_rows(rs);
    assert!(!rows.is_null());
    assert!(mux_rc_dec(rows));
    assert!(mux_rc_dec(rs));

    exec(conn, "DROP TABLE IF EXISTS mux_types");
    mux_sql_connection_close(conn);
    assert!(mux_rc_dec(conn));
}

/// A connection to an unreachable server is an error (covers the connect-failure
/// branch of the driver router).
#[test]
fn postgres_connect_failure() {
    if std::env::var("MUX_TEST_POSTGRES_URL").is_err() {
        return;
    }
    let bad = CString::new("postgres://nouser:nopass@127.0.0.1:1/nodb").unwrap();
    assert_err(unsafe { mux_sql_connect(bad.as_ptr()) });
}

#[test]
fn unsupported_scheme_is_error() {
    let uri = CString::new("sqlserver://host/db").unwrap();
    assert_err(unsafe { mux_sql_connect(uri.as_ptr()) });
}
