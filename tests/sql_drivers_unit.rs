//! Driver-level SQL coverage for Postgres, `MySQL`, and SQL Server (feature-gated behind `sql`).
//!
//! These connect to live servers and are skipped unless the corresponding env
//! var is set, so `cargo test` stays green without databases:
//!
//! ```text
//! MUX_TEST_POSTGRES_URL  e.g. postgres://user:pass@localhost:5432/db
//! MUX_TEST_MYSQL_URL     e.g. mysql://user:pass@localhost:3306/db
//! MUX_TEST_SQLSERVER_URL e.g. sqlserver://user:pass@localhost:1433/db?trustServerCertificate=true
//! ```
//!
//! CI sets the Postgres and MySQL variables via service containers. SQL Server is
//! optional and runs when its variable points to a reachable TDS service.
#![cfg(feature = "sql")]

mod common;

use std::ffi::CString;
use std::thread;
use std::time::Duration;

use common::{assert_err, assert_ok, ok_int};
use mux_runtime::optional::{mux_optional_data, mux_optional_is_some};
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_err, mux_result_is_ok};
use mux_runtime::sql::*;
use mux_runtime::Value;

fn sval(s: &str) -> *mut Value {
    mux_rc_alloc(Value::String(s.to_string()))
}

fn ok_data(r: *mut Value) -> *mut Value {
    assert!(unsafe { mux_result_is_ok(r) }, "expected Ok result");
    let data = unsafe { mux_result_data(r) };
    assert!(!data.is_null());
    assert!(unsafe { mux_rc_dec(r) });
    data
}

fn resultset_rows(resultset: *mut Value) -> *mut Value {
    ok_data(mux_sql_resultset_rows(resultset))
}

fn resultset_next(resultset: *mut Value) -> *mut Value {
    ok_data(mux_sql_resultset_next(resultset))
}

fn resultset_next_batch(resultset: *mut Value, limit: i64) -> *mut Value {
    ok_data(mux_sql_resultset_next_batch(resultset, limit))
}

fn connect(uri: &str) -> *mut Value {
    let c = CString::new(uri).unwrap();
    ok_data(unsafe { mux_sql_connect(c.as_ptr()) })
}

fn named_value(name: &str, value: Value) -> *mut Value {
    let mut values = mux_runtime::ordered::OrderedMap::new();
    values.insert(Value::String(name.to_string()), value);
    mux_rc_alloc(Value::Map(values))
}

fn assert_single_int(result: *mut Value, expected: i64) {
    let resultset = ok_data(result);
    let rows = resultset_rows(resultset);
    let row = unsafe {
        let Value::List(items) = &*rows else {
            panic!("expected a row list");
        };
        assert_eq!(items.len(), 1);
        items[0].clone()
    };
    let row_handle = mux_rc_alloc(row);
    let value = ok_data(mux_sql_row_at(row_handle, 0));
    assert!(unsafe { matches!(&*value, Value::Int(actual) if *actual == expected) });
    unsafe {
        assert!(mux_rc_dec(row_handle));
        assert!(mux_rc_dec(value));
        assert!(mux_rc_dec(rows));
        assert!(mux_rc_dec(resultset));
    }
}

fn assert_constraint_error(result: *mut Value, provider: &str) {
    assert!(
        unsafe { mux_result_is_err(result) },
        "expected constraint error"
    );
    let error = unsafe { mux_result_data(result) };
    let (kind, provider_value, code, operation) = unsafe {
        (
            mux_sql_error_kind(error),
            mux_sql_error_provider(error),
            mux_sql_error_code(error),
            mux_sql_error_operation(error),
        )
    };
    assert!(unsafe {
        matches!(&*kind, Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes())
            && matches!(&*provider_value, Value::String(value) if value == provider)
            && matches!(&*code, Value::String(value) if !value.is_empty())
            && matches!(&*operation, Value::String(value) if value == "execute")
    });
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(code));
        assert!(mux_rc_dec(provider_value));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
    }
}

fn exec(conn: *mut Value, sql: &str) {
    let s = sval(sql);
    assert_ok(mux_sql_connection_execute(conn, s));
    assert!(unsafe { mux_rc_dec(s) });
}

fn assert_error_kind(result: *mut Value, expected: i32) {
    assert!(unsafe { mux_result_is_err(result) }, "expected SQL error");
    unsafe {
        let error = mux_result_data(result);
        let kind = mux_sql_error_kind(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == expected.to_ne_bytes()));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
    }
}

fn assert_timeout(result: *mut Value) {
    assert_error_kind(result, 1);
}

fn assert_cancelled(result: *mut Value) {
    assert_error_kind(result, 5);
}

unsafe fn assert_interrupt_capabilities(
    capabilities: *mut Value,
    provider: &str,
    timeout: bool,
    cancellation: bool,
) {
    let Value::Map(fields) = &*capabilities else {
        panic!("expected SQL capabilities map");
    };
    assert_eq!(
        fields.get(&Value::String("provider".to_string())),
        Some(&Value::String(provider.to_string()))
    );
    assert_eq!(
        fields.get(&Value::String("query_timeout".to_string())),
        Some(&Value::Bool(timeout))
    );
    assert_eq!(
        fields.get(&Value::String("query_cancellation".to_string())),
        Some(&Value::Bool(cancellation))
    );
    assert!(mux_rc_dec(capabilities));
}

/// Run the full driver exercise. `ph` builds the placeholder for a 1-based index
/// ("$1" for Postgres, "?" for `MySQL`).
fn run_driver_suite(uri: &str, provider: &str, ph: impl Fn(usize) -> String) {
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
    assert!(unsafe { mux_rc_dec(insert) });
    assert!(unsafe { mux_rc_dec(params) });

    // query exercises row/value conversion for INT/TEXT/FLOAT/BOOL columns
    let select = sval("SELECT id, name, score, flag FROM mux_cov_t");
    let rs = ok_data(mux_sql_connection_query(conn, select));
    assert!(unsafe { mux_rc_dec(select) });
    let cols = mux_sql_resultset_columns(rs);
    assert!(!cols.is_null());
    assert!(unsafe { mux_rc_dec(cols) });
    let rows = resultset_rows(rs);
    assert!(!rows.is_null());
    assert!(unsafe { mux_rc_dec(rows) });
    assert!(unsafe { mux_rc_dec(rs) });

    // The ordinary query path must retain the provider cursor rather than
    // buffering every row. Exercise both bounded advancement and EOF before
    // releasing the connection lease.
    let streaming_sql = sval("SELECT id FROM mux_cov_t ORDER BY id");
    let streaming_rs = ok_data(mux_sql_connection_query(conn, streaming_sql));
    let first_batch = resultset_next_batch(streaming_rs, 1);
    let first_batch_len = unsafe {
        let Value::List(rows) = &*first_batch else {
            panic!("expected first streaming batch");
        };
        rows.len()
    };
    assert_eq!(first_batch_len, 1);
    let second_batch = resultset_next_batch(streaming_rs, 1);
    let second_batch_len = unsafe {
        let Value::List(rows) = &*second_batch else {
            panic!("expected second streaming batch");
        };
        rows.len()
    };
    assert_eq!(second_batch_len, 0);
    let end = resultset_next(streaming_rs);
    assert!(unsafe { matches!(&*end, Value::Optional(value) if value.is_none()) });
    unsafe {
        assert!(mux_rc_dec(end));
        assert!(mux_rc_dec(second_batch));
        assert!(mux_rc_dec(first_batch));
        assert!(mux_rc_dec(streaming_rs));
        assert!(mux_rc_dec(streaming_sql));
    }

    // parameterized query
    let qsql = format!("SELECT name FROM mux_cov_t WHERE id = {}", ph(1));
    let q = sval(&qsql);
    let qparams = mux_rc_alloc(Value::List(vec![Value::Int(1)]));
    let rs2 = ok_data(mux_sql_connection_query_params(conn, q, qparams));
    assert!(unsafe { mux_rc_dec(q) });
    assert!(unsafe { mux_rc_dec(qparams) });
    assert!(unsafe { mux_rc_dec(rs2) });

    // Named parameters can be repeated without requiring duplicate map keys.
    // The scanner rewrites the same logical value for each provider-specific
    // placeholder occurrence.
    let named_query = sval("SELECT :value AS first, :value AS second");
    let named_params = named_value("value", Value::Int(21));
    let named_result = ok_data(mux_sql_connection_query_named(
        conn,
        named_query,
        named_params,
    ));
    let named_rows = resultset_rows(named_result);
    let named_row = unsafe {
        let Value::List(items) = &*named_rows else {
            panic!("expected a row list");
        };
        assert_eq!(items.len(), 1);
        items[0].clone()
    };
    let named_row_handle = mux_rc_alloc(named_row);
    let first = ok_data(mux_sql_row_at(named_row_handle, 0));
    let second = ok_data(mux_sql_row_at(named_row_handle, 1));
    assert!(unsafe { matches!(&*first, Value::Int(value) if *value == 21) });
    assert!(unsafe { matches!(&*second, Value::Int(value) if *value == 21) });
    unsafe {
        assert!(mux_rc_dec(second));
        assert!(mux_rc_dec(first));
        assert!(mux_rc_dec(named_row_handle));
        assert!(mux_rc_dec(named_rows));
        assert!(mux_rc_dec(named_result));
        assert!(mux_rc_dec(named_params));
        assert!(mux_rc_dec(named_query));
    }

    // Batch execution and bulk execution share the same provider-neutral
    // statement boundary as the single-statement operations.
    let batch = sval(
        "INSERT INTO mux_cov_t (id, name, score, flag) VALUES (10, 'batch-a', 10.0, true); \
         INSERT INTO mux_cov_t (id, name, score, flag) VALUES (11, 'batch-b', 11.0, false)",
    );
    assert_ok(mux_sql_connection_execute_batch(conn, batch));
    unsafe {
        assert!(mux_rc_dec(batch));
    }

    let many_sql = sval(
        format!(
            "INSERT INTO mux_cov_t (id, name, score, flag) VALUES ({}, {}, {}, {})",
            ph(1),
            ph(2),
            ph(3),
            ph(4)
        )
        .as_str(),
    );
    let many_rows = mux_rc_alloc(Value::List(vec![
        Value::List(vec![
            Value::Int(12),
            Value::String("many-a".into()),
            Value::Float(ordered_float::OrderedFloat(12.0)),
            Value::Bool(true),
        ]),
        Value::List(vec![
            Value::Int(13),
            Value::String("many-b".into()),
            Value::Float(ordered_float::OrderedFloat(13.0)),
            Value::Bool(false),
        ]),
    ]));
    assert_eq!(
        ok_int(mux_sql_connection_execute_many(conn, many_sql, many_rows)),
        2
    );
    unsafe {
        assert!(mux_rc_dec(many_rows));
        assert!(mux_rc_dec(many_sql));
    }

    // transactions: commit then rollback
    let tx = ok_data(mux_sql_connection_begin_transaction(conn));
    let ti = sval("INSERT INTO mux_cov_t (id, name, score, flag) VALUES (2, 'b', 2.5, false)");
    assert_ok(mux_sql_transaction_execute(tx, ti));
    assert!(unsafe { mux_rc_dec(ti) });
    assert_ok(mux_sql_transaction_commit(tx));
    assert!(unsafe { mux_rc_dec(tx) });

    let tx2 = ok_data(mux_sql_connection_begin_transaction(conn));
    let ti2 = sval("INSERT INTO mux_cov_t (id, name, score, flag) VALUES (3, 'c', 3.5, true)");
    assert_ok(mux_sql_transaction_execute(tx2, ti2));
    assert!(unsafe { mux_rc_dec(ti2) });
    assert_ok(mux_sql_transaction_rollback(tx2));
    assert!(unsafe { mux_rc_dec(tx2) });

    // Savepoints make a partial transaction rollback observable without
    // discarding work that was completed before the savepoint.
    let tx3 = ok_data(mux_sql_connection_begin_transaction(conn));
    let before_savepoint = sval(
        "INSERT INTO mux_cov_t (id, name, score, flag) VALUES (20, 'before-savepoint', 20.0, true)",
    );
    assert_ok(mux_sql_transaction_execute(tx3, before_savepoint));
    unsafe {
        assert!(mux_rc_dec(before_savepoint));
    }
    let savepoint = sval("driver_savepoint");
    assert_ok(mux_sql_transaction_savepoint(tx3, savepoint));
    let after_savepoint = sval(
        "INSERT INTO mux_cov_t (id, name, score, flag) VALUES (21, 'after-savepoint', 21.0, true)",
    );
    assert_ok(mux_sql_transaction_execute(tx3, after_savepoint));
    assert_ok(mux_sql_transaction_rollback_to(tx3, savepoint));
    assert_ok(mux_sql_transaction_release_savepoint(tx3, savepoint));
    assert_ok(mux_sql_transaction_commit(tx3));
    unsafe {
        assert!(mux_rc_dec(after_savepoint));
        assert!(mux_rc_dec(savepoint));
        assert!(mux_rc_dec(tx3));
    }

    // Duplicate keys must use the same typed constraint category and retain
    // each provider's native diagnostic code.
    exec(conn, "DROP TABLE IF EXISTS mux_cov_constraints");
    exec(
        conn,
        "CREATE TABLE mux_cov_constraints (id BIGINT PRIMARY KEY)",
    );
    exec(conn, "INSERT INTO mux_cov_constraints (id) VALUES (1)");
    let duplicate = sval("INSERT INTO mux_cov_constraints (id) VALUES (1)");
    assert_constraint_error(mux_sql_connection_execute(conn, duplicate), provider);
    unsafe {
        assert!(mux_rc_dec(duplicate));
    }

    // Exercise the pool's batch, parameter, named, bulk, and query paths with
    // the same provider URI. A single connection keeps this fixture stable
    // while still covering pool acquisition and return.
    let pool_uri = sval(uri);
    let pool = ok_data(mux_sql_pool_from_config(pool_uri, 1, 1_000));
    unsafe {
        assert!(mux_rc_dec(pool_uri));
    }
    let pool_batch = sval(
        "DROP TABLE IF EXISTS mux_cov_pool; CREATE TABLE mux_cov_pool (id BIGINT PRIMARY KEY)",
    );
    assert_ok(mux_sql_pool_execute_batch(pool, pool_batch));
    unsafe {
        assert!(mux_rc_dec(pool_batch));
    }
    let pool_insert = sval("INSERT INTO mux_cov_pool (id) VALUES (?)");
    let pool_params = mux_rc_alloc(Value::List(vec![Value::Int(100)]));
    assert_eq!(
        ok_int(mux_sql_pool_execute_params(pool, pool_insert, pool_params)),
        1
    );
    unsafe {
        assert!(mux_rc_dec(pool_insert));
        assert!(mux_rc_dec(pool_params));
    }
    let pool_many_sql = sval("INSERT INTO mux_cov_pool (id) VALUES (?)");
    let pool_many_rows = mux_rc_alloc(Value::List(vec![
        Value::List(vec![Value::Int(101)]),
        Value::List(vec![Value::Int(102)]),
    ]));
    assert_eq!(
        ok_int(mux_sql_pool_execute_many(
            pool,
            pool_many_sql,
            pool_many_rows
        )),
        2
    );
    unsafe {
        assert!(mux_rc_dec(pool_many_sql));
        assert!(mux_rc_dec(pool_many_rows));
    }
    let pool_named_sql = sval("INSERT INTO mux_cov_pool (id) VALUES (:id)");
    let pool_named = named_value("id", Value::Int(103));
    assert_eq!(
        ok_int(mux_sql_pool_execute_named(pool, pool_named_sql, pool_named)),
        1
    );
    unsafe {
        assert!(mux_rc_dec(pool_named_sql));
        assert!(mux_rc_dec(pool_named));
    }
    let pool_query_sql = sval("SELECT COUNT(*) FROM mux_cov_pool WHERE id >= :minimum");
    let pool_query_params = named_value("minimum", Value::Int(100));
    assert_single_int(
        mux_sql_pool_query_named(pool, pool_query_sql, pool_query_params),
        4,
    );
    unsafe {
        assert!(mux_rc_dec(pool_query_sql));
        assert!(mux_rc_dec(pool_query_params));

        // Dropping an unfinished streaming result must not return a provider
        // cursor to the idle pool. MySQL drains QueryResult on drop; the
        // PostgreSQL path must retire the checked-out connection instead.
        let dropped_query = sval("SELECT id FROM mux_cov_pool ORDER BY id");
        let dropped_resultset = ok_data(mux_sql_pool_query(pool, dropped_query));
        assert!(mux_rc_dec(dropped_query));
        assert!(mux_rc_dec(dropped_resultset));
    }
    let metrics = ok_data(mux_sql_pool_metrics(pool));
    let expected_total = i64::from(provider != "postgres");
    assert!(unsafe {
        matches!(&*metrics, Value::Map(values)
            if values.get(&Value::String("in_use".to_string())) == Some(&Value::Int(0))
            && values.get(&Value::String("total".to_string())) == Some(&Value::Int(expected_total)))
    });
    unsafe {
        assert!(mux_rc_dec(metrics));
        assert_ok(mux_sql_pool_close(pool));
        assert!(mux_rc_dec(pool));
    }
    exec(conn, "DROP TABLE IF EXISTS mux_cov_pool");
    exec(conn, "DROP TABLE IF EXISTS mux_cov_constraints");

    // invalid SQL surfaces as an error through the driver
    let bad = sval("THIS IS NOT VALID SQL");
    assert_err(mux_sql_connection_execute(conn, bad));
    assert!(unsafe { mux_rc_dec(bad) });

    exec(conn, "DROP TABLE IF EXISTS mux_cov_t");
    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn postgres_driver() {
    let Ok(uri) = std::env::var("MUX_TEST_POSTGRES_URL") else {
        eprintln!("skipping postgres_driver: MUX_TEST_POSTGRES_URL not set");
        return;
    };
    run_driver_suite(&uri, "postgres", |i| format!("${i}"));
}

#[test]
fn mysql_driver() {
    let Ok(uri) = std::env::var("MUX_TEST_MYSQL_URL") else {
        eprintln!("skipping mysql_driver: MUX_TEST_MYSQL_URL not set");
        return;
    };
    run_driver_suite(&uri, "mysql", |_| "?".to_string());
}

/// Exercise the native TDS connection and the leased result-set lifecycle.
///
/// This test is skipped unless `MUX_TEST_SQLSERVER_URL` points to a live
/// server, so ordinary local test runs do not need SQL Server installed.
#[test]
fn sqlserver_live_driver() {
    let Ok(uri) = std::env::var("MUX_TEST_SQLSERVER_URL") else {
        eprintln!("skipping sqlserver_live_driver: MUX_TEST_SQLSERVER_URL not set");
        return;
    };

    let conn = connect(&uri);
    unsafe {
        assert_interrupt_capabilities(
            ok_data(mux_sql_connection_capabilities(conn)),
            "sqlserver",
            true,
            true,
        );
    }
    let query =
        sval("SELECT CAST(1 AS BIGINT) AS value UNION ALL SELECT CAST(2 AS BIGINT) AS value");
    let resultset = ok_data(mux_sql_connection_query(conn, query));

    // An open cursor owns the connection lease. A competing operation must
    // fail until the cursor reaches EOF or is explicitly closed.
    let busy_query = sval("SELECT CAST(9 AS BIGINT)");
    assert_err(mux_sql_connection_query(conn, busy_query));
    unsafe {
        assert!(mux_rc_dec(busy_query));
    }

    let first = resultset_next(resultset);
    assert!(unsafe { mux_runtime::optional::mux_optional_is_some(first) });
    let first_row = unsafe { mux_optional_data(first) };
    assert!(!first_row.is_null());
    let first_value = ok_data(mux_sql_row_at(first_row, 0));
    assert!(unsafe { matches!(&*first_value, Value::Int(value) if *value == 1) });
    unsafe {
        assert!(mux_rc_dec(first_value));
        assert!(mux_rc_dec(first_row));
        assert!(mux_rc_dec(first));
    }

    let second = resultset_next(resultset);
    let second_row = unsafe { mux_optional_data(second) };
    assert!(!second_row.is_null());
    let second_value = ok_data(mux_sql_row_at(second_row, 0));
    assert!(unsafe { matches!(&*second_value, Value::Int(value) if *value == 2) });
    unsafe {
        assert!(mux_rc_dec(second_value));
        assert!(mux_rc_dec(second_row));
        assert!(mux_rc_dec(second));
    }

    // EOF releases the lease. Explicit close remains safe and idempotent.
    let eof = resultset_next(resultset);
    assert!(unsafe { mux_runtime::optional::mux_optional_is_none(eof) });
    unsafe {
        assert!(mux_rc_dec(eof));
    }
    assert_ok(mux_sql_resultset_close(resultset));
    assert_ok(mux_sql_resultset_close(resultset));

    let reusable_query = sval("SELECT CAST(42 AS BIGINT)");
    let reusable = ok_data(mux_sql_connection_query(conn, reusable_query));
    assert_ok(mux_sql_resultset_close(reusable));
    assert_ok(mux_sql_resultset_close(reusable));
    unsafe {
        assert!(mux_rc_dec(reusable));
        assert!(mux_rc_dec(reusable_query));
        assert!(mux_rc_dec(resultset));
        assert!(mux_rc_dec(query));
        mux_sql_connection_close(conn);
        assert!(mux_rc_dec(conn));
    }
}

#[test]
fn sqlserver_streaming_row_failure_is_typed_and_retires_connection() {
    let Ok(uri) = std::env::var("MUX_TEST_SQLSERVER_URL") else {
        eprintln!(
            "skipping sqlserver_streaming_row_failure_is_typed_and_retires_connection: MUX_TEST_SQLSERVER_URL not set"
        );
        return;
    };

    let conn = connect(&uri);
    let query =
        sval("SELECT CAST(1 AS BIGINT) AS value; RAISERROR ('mux streaming failure', 16, 1)");
    let resultset = ok_data(mux_sql_connection_query(conn, query));
    let first = resultset_next(resultset);
    assert!(unsafe { mux_optional_is_some(first) });
    let first_row = unsafe { mux_optional_data(first) };
    assert!(!first_row.is_null());
    unsafe {
        assert!(mux_rc_dec(first_row));
        assert!(mux_rc_dec(first));
    }

    let failure = mux_sql_resultset_next(resultset);
    assert!(unsafe { mux_result_is_err(failure) });
    let error = unsafe { mux_result_data(failure) };
    let kind = unsafe { mux_sql_error_kind(error) };
    let detail = unsafe { mux_sql_error_detail(error) };
    let operation = unsafe { mux_sql_error_operation(error) };
    assert!(unsafe {
        matches!(&*kind, Value::Opaque(value) if value.as_ref() == 4_i32.to_ne_bytes())
            && matches!(&*detail, Value::String(value) if value.contains("mux streaming failure"))
            && matches!(&*operation, Value::String(value) if value == "resultset_next")
    });

    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(detail));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(failure));
    }

    let unusable_query = sval("SELECT CAST(42 AS BIGINT)");
    assert_err(mux_sql_connection_query(conn, unusable_query));
    unsafe {
        assert!(mux_rc_dec(unusable_query));
        assert!(mux_rc_dec(resultset));
        assert!(mux_rc_dec(query));
        mux_sql_connection_close(conn);
        assert!(mux_rc_dec(conn));
    }
}

/// Exercise SQL Server operations that use the connection ownership paths
/// beyond a direct query. This remains env-gated so local runs do not need a
/// TDS service, while CI can catch regressions in transaction restoration,
/// prepared-statement reuse, and pool lease return.
#[test]
fn sqlserver_live_transaction_prepared_and_pool_paths() {
    let Ok(uri) = std::env::var("MUX_TEST_SQLSERVER_URL") else {
        eprintln!(
            "skipping sqlserver_live_transaction_prepared_and_pool_paths: MUX_TEST_SQLSERVER_URL not set"
        );
        return;
    };

    let conn = connect(&uri);
    let table = format!("mux_sqlserver_acceptance_{}", std::process::id());
    exec(conn, &format!("DROP TABLE IF EXISTS {table}"));
    exec(
        conn,
        &format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY, label NVARCHAR(128) NOT NULL)"),
    );

    // A committed transaction must restore the connection for the next
    // operation, while a rolled-back insert must not become visible.
    let transaction = ok_data(mux_sql_connection_begin_transaction(conn));
    let insert_sql = sval(&format!("INSERT INTO {table} (id, label) VALUES (?, ?)"));
    let insert_params = mux_rc_alloc(Value::List(vec![
        Value::Int(1),
        Value::String("committed".to_string()),
    ]));
    assert_eq!(
        ok_int(mux_sql_transaction_execute_params(
            transaction,
            insert_sql,
            insert_params,
        )),
        1
    );
    unsafe {
        assert!(mux_rc_dec(insert_params));
        assert!(mux_rc_dec(insert_sql));
    }
    assert_ok(mux_sql_transaction_commit(transaction));
    unsafe {
        assert!(mux_rc_dec(transaction));
    }

    let rollback = ok_data(mux_sql_connection_begin_transaction(conn));
    let rollback_sql = sval(&format!("INSERT INTO {table} (id, label) VALUES (?, ?)"));
    let rollback_params = mux_rc_alloc(Value::List(vec![
        Value::Int(2),
        Value::String("rolled-back".to_string()),
    ]));
    assert_eq!(
        ok_int(mux_sql_transaction_execute_params(
            rollback,
            rollback_sql,
            rollback_params,
        )),
        1
    );
    unsafe {
        assert!(mux_rc_dec(rollback_params));
        assert!(mux_rc_dec(rollback_sql));
    }
    assert_ok(mux_sql_transaction_rollback(rollback));
    unsafe {
        assert!(mux_rc_dec(rollback));
    }

    // Prepared handles are reusable wrappers around provider-neutral SQL.
    // Running the same handle twice catches stale parameter state and verifies
    // that SQL Server's @P1/@P2 rewrite is applied on every execution.
    let prepared_sql = sval(&format!("INSERT INTO {table} (id, label) VALUES (?, ?)"));
    let prepared = ok_data(mux_sql_connection_prepare(conn, prepared_sql));
    unsafe {
        assert!(mux_rc_dec(prepared_sql));
    }
    for (id, label) in [(3_i64, "prepared-a"), (4_i64, "prepared-b")] {
        let params = mux_rc_alloc(Value::List(vec![
            Value::Int(id),
            Value::String(label.to_string()),
        ]));
        assert_eq!(ok_int(mux_sql_prepared_execute(prepared, params)), 1);
        unsafe {
            assert!(mux_rc_dec(params));
        }
    }

    let select_prepared_sql = sval(&format!("SELECT label FROM {table} WHERE id = ?"));
    let select_prepared = ok_data(mux_sql_connection_prepare(conn, select_prepared_sql));
    unsafe {
        assert!(mux_rc_dec(select_prepared_sql));
    }
    let select_params = mux_rc_alloc(Value::List(vec![Value::Int(3)]));
    let selected = ok_data(mux_sql_prepared_query(select_prepared, select_params));
    let selected_rows = resultset_rows(selected);
    let selected_row = unsafe {
        let Value::List(rows) = &*selected_rows else {
            panic!("expected SQL Server prepared rows");
        };
        assert_eq!(rows.len(), 1);
        mux_rc_alloc(rows[0].clone())
    };
    let selected_value = ok_data(mux_sql_row_at(selected_row, 0));
    assert!(unsafe { matches!(&*selected_value, Value::String(value) if value == "prepared-a") });
    unsafe {
        assert!(mux_rc_dec(selected_value));
        assert!(mux_rc_dec(selected_row));
        assert!(mux_rc_dec(selected_rows));
        assert!(mux_rc_dec(selected));
        assert!(mux_rc_dec(select_params));
    }
    mux_sql_prepared_close(select_prepared);
    unsafe {
        assert!(mux_rc_dec(select_prepared));
    }
    mux_sql_prepared_close(prepared);
    unsafe {
        assert!(mux_rc_dec(prepared));
    }

    // A pool query owns its checked-out connection until EOF. Metrics observe
    // that lease while the cursor is open and zero after EOF, which proves the
    // pool can safely reuse the connection instead of leaking it.
    let pool_uri = sval(&uri);
    let pool = ok_data(mux_sql_pool_from_config(pool_uri, 1, 1_000));
    unsafe {
        assert!(mux_rc_dec(pool_uri));
    }
    let pool_insert = sval(&format!("INSERT INTO {table} (id, label) VALUES (?, ?)"));
    let pool_params = mux_rc_alloc(Value::List(vec![
        Value::Int(5),
        Value::String("pooled".to_string()),
    ]));
    assert_eq!(
        ok_int(mux_sql_pool_execute_params(pool, pool_insert, pool_params)),
        1
    );
    unsafe {
        assert!(mux_rc_dec(pool_insert));
        assert!(mux_rc_dec(pool_params));
    }

    let pool_query = sval(&format!("SELECT id FROM {table} ORDER BY id"));
    let pool_resultset = ok_data(mux_sql_pool_query(pool, pool_query));
    let metrics = ok_data(mux_sql_pool_metrics(pool));
    assert!(unsafe {
        matches!(&*metrics, Value::Map(values) if values.get(&Value::String("in_use".to_string())) == Some(&Value::Int(1)))
    });
    unsafe {
        assert!(mux_rc_dec(metrics));
    }
    loop {
        let next = resultset_next(pool_resultset);
        let done = unsafe { mux_runtime::optional::mux_optional_is_none(next) };
        unsafe {
            assert!(mux_rc_dec(next));
        }
        if done {
            break;
        }
    }
    let metrics = ok_data(mux_sql_pool_metrics(pool));
    assert!(unsafe {
        matches!(&*metrics, Value::Map(values) if values.get(&Value::String("in_use".to_string())) == Some(&Value::Int(0)))
    });
    unsafe {
        assert!(mux_rc_dec(metrics));
        assert!(mux_rc_dec(pool_resultset));
        assert!(mux_rc_dec(pool_query));
    }

    // SQL Server QueryStream cannot drain itself on drop, so an unfinished
    // pooled cursor must retire the checked-out connection.
    let dropped_query = sval(&format!("SELECT id FROM {table} ORDER BY id"));
    let dropped_resultset = ok_data(mux_sql_pool_query(pool, dropped_query));
    unsafe {
        assert!(mux_rc_dec(dropped_query));
        assert!(mux_rc_dec(dropped_resultset));
    }
    let metrics = ok_data(mux_sql_pool_metrics(pool));
    assert!(unsafe {
        matches!(&*metrics, Value::Map(values)
            if values.get(&Value::String("in_use".to_string())) == Some(&Value::Int(0))
            && values.get(&Value::String("total".to_string())) == Some(&Value::Int(0)))
    });
    unsafe {
        assert!(mux_rc_dec(metrics));
    }
    assert_ok(mux_sql_pool_close(pool));
    unsafe {
        assert!(mux_rc_dec(pool));
    }

    exec(conn, &format!("DROP TABLE IF EXISTS {table}"));
    mux_sql_connection_close(conn);
    unsafe {
        assert!(mux_rc_dec(conn));
    }
}

/// MySQL interrupts a running statement through a second authenticated session
/// and the server's `KILL QUERY` command. Exercise every public query wrapper
/// so all paths preserve the typed timeout/cancellation result.
#[test]
fn mysql_query_interrupt_operations_use_server_kill() {
    let Ok(uri) = std::env::var("MUX_TEST_MYSQL_URL") else {
        eprintln!(
            "skipping mysql_query_interrupt_operations_use_server_kill: MUX_TEST_MYSQL_URL not set"
        );
        return;
    };

    let conn = connect(&uri);
    unsafe {
        assert_interrupt_capabilities(
            ok_data(mux_sql_connection_capabilities(conn)),
            "mysql",
            true,
            true,
        );
    }
    let query = sval("SELECT SLEEP(2)");
    assert_timeout(mux_sql_connection_query_with_timeout(conn, query, 50));
    unsafe {
        assert!(mux_rc_dec(query));
    }

    let token = mux_runtime::sync_primitives::mux_cancellation_new();
    let token_address = token as usize;
    let worker = thread::spawn(move || -> usize {
        thread::sleep(Duration::from_millis(50));
        unsafe {
            mux_runtime::sync_primitives::mux_cancellation_cancel(token_address as *const Value)
                as usize
        }
    });
    let query = sval("SELECT SLEEP(2)");
    let result = mux_sql_connection_query_with_cancellation(conn, query, token);
    let cancel_result = worker.join().expect("cancellation worker panicked") as *mut Value;
    assert_ok(cancel_result);
    assert_cancelled(result);
    unsafe {
        assert!(mux_rc_dec(query));
        assert!(mux_rc_dec(token));
    }

    let statement = sval("SELECT SLEEP(?)");
    let prepared = ok_data(mux_sql_connection_prepare(conn, statement));
    unsafe {
        assert!(mux_rc_dec(statement));
    }
    let params = mux_rc_alloc(Value::List(vec![Value::Int(2)]));
    assert_timeout(mux_sql_prepared_query_with_timeout(prepared, params, 50));
    unsafe {
        assert!(mux_rc_dec(params));
    }
    let token = mux_runtime::sync_primitives::mux_cancellation_new();
    let token_address = token as usize;
    let worker = thread::spawn(move || -> usize {
        thread::sleep(Duration::from_millis(50));
        unsafe {
            mux_runtime::sync_primitives::mux_cancellation_cancel(token_address as *const Value)
                as usize
        }
    });
    let params = mux_rc_alloc(Value::List(vec![Value::Int(2)]));
    let result = mux_sql_prepared_query_with_cancellation(prepared, params, token);
    let cancel_result = worker.join().expect("cancellation worker panicked") as *mut Value;
    assert_ok(cancel_result);
    assert_cancelled(result);
    unsafe {
        assert!(mux_rc_dec(params));
        assert!(mux_rc_dec(token));
        mux_sql_prepared_close(prepared);
        assert!(mux_rc_dec(prepared));
    }

    let transaction = ok_data(mux_sql_connection_begin_transaction(conn));
    let query = sval("SELECT SLEEP(2)");
    assert_timeout(mux_sql_transaction_query_with_timeout(
        transaction,
        query,
        50,
    ));
    unsafe {
        assert!(mux_rc_dec(query));
    }
    let token = mux_runtime::sync_primitives::mux_cancellation_new();
    let token_address = token as usize;
    let worker = thread::spawn(move || -> usize {
        thread::sleep(Duration::from_millis(50));
        unsafe {
            mux_runtime::sync_primitives::mux_cancellation_cancel(token_address as *const Value)
                as usize
        }
    });
    let query = sval("SELECT SLEEP(2)");
    let result = mux_sql_transaction_query_with_cancellation(transaction, query, token);
    let cancel_result = worker.join().expect("cancellation worker panicked") as *mut Value;
    assert_ok(cancel_result);
    assert_cancelled(result);
    unsafe {
        assert!(mux_rc_dec(query));
        assert!(mux_rc_dec(token));
    }
    assert_ok(mux_sql_transaction_commit(transaction));
    unsafe {
        assert!(mux_rc_dec(transaction));
    }

    let pool_uri = sval(&uri);
    let pool = ok_data(mux_sql_pool_from_config(pool_uri, 1, 1_000));
    unsafe {
        assert!(mux_rc_dec(pool_uri));
    }
    let query = sval("SELECT SLEEP(2)");
    assert_timeout(mux_sql_pool_query_with_timeout(pool, query, 50));
    unsafe {
        assert!(mux_rc_dec(query));
    }
    let token = mux_runtime::sync_primitives::mux_cancellation_new();
    let token_address = token as usize;
    let worker = thread::spawn(move || -> usize {
        thread::sleep(Duration::from_millis(50));
        unsafe {
            mux_runtime::sync_primitives::mux_cancellation_cancel(token_address as *const Value)
                as usize
        }
    });
    let query = sval("SELECT SLEEP(2)");
    let result = mux_sql_pool_query_with_cancellation(pool, query, token);
    let cancel_result = worker.join().expect("cancellation worker panicked") as *mut Value;
    assert_ok(cancel_result);
    assert_cancelled(result);
    unsafe {
        assert!(mux_rc_dec(query));
        assert!(mux_rc_dec(token));
    }

    let healthy_query = sval("SELECT 42");
    assert_single_int(mux_sql_connection_query(conn, healthy_query), 42);
    unsafe {
        assert!(mux_rc_dec(healthy_query));
        assert_ok(mux_sql_pool_close(pool));
        assert!(mux_rc_dec(pool));
        mux_sql_connection_close(conn);
        assert!(mux_rc_dec(conn));
    }
}

/// MySQL sends text and binary columns as the same wire-level `Bytes` value.
/// The runtime must use column metadata so a valid UTF-8 BLOB is not silently
/// converted to a Mux string.
#[test]
fn mysql_binary_columns_preserve_valid_utf8_bytes() {
    let Ok(uri) = std::env::var("MUX_TEST_MYSQL_URL") else {
        eprintln!(
            "skipping mysql_binary_columns_preserve_valid_utf8_bytes: MUX_TEST_MYSQL_URL not set"
        );
        return;
    };

    let conn = connect(&uri);
    exec(conn, "DROP TABLE IF EXISTS mux_binary_types");
    exec(conn, "CREATE TABLE mux_binary_types (payload BLOB)");

    let insert = sval("INSERT INTO mux_binary_types (payload) VALUES (?)");
    let params = mux_rc_alloc(Value::List(vec![Value::Bytes(b"hello".to_vec())]));
    assert_ok(mux_sql_connection_execute_params(conn, insert, params));
    unsafe {
        assert!(mux_rc_dec(insert));
        assert!(mux_rc_dec(params));
    }

    let query = sval("SELECT payload FROM mux_binary_types");
    let resultset = ok_data(mux_sql_connection_query(conn, query));
    let next = resultset_next(resultset);
    let row = unsafe { mux_optional_data(next) };
    assert!(!row.is_null());
    let values = mux_sql_row_values(row);
    assert!(unsafe {
        matches!(&*values, Value::List(items) if matches!(items.as_slice(), [Value::Bytes(value)] if value == b"hello"))
    });
    unsafe {
        assert!(mux_rc_dec(values));
        assert!(mux_rc_dec(next));
        assert!(mux_rc_dec(resultset));
        assert!(mux_rc_dec(query));
    }

    exec(conn, "DROP TABLE IF EXISTS mux_binary_types");
    mux_sql_connection_close(conn);
    unsafe {
        assert!(mux_rc_dec(conn));
    }
}

/// Postgres column types map through distinct branches of `postgres_query_value`
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
    assert!(unsafe { mux_rc_dec(select) });
    let rows = resultset_rows(rs);
    assert!(!rows.is_null());
    assert!(unsafe { mux_rc_dec(rows) });
    assert!(unsafe { mux_rc_dec(rs) });

    exec(conn, "DROP TABLE IF EXISTS mux_types");
    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

/// PostgreSQL has a wire-level cancel request, so the existing cooperative
/// cancellation API should interrupt a server-side sleep and leave the
/// connection usable for the next query.
#[test]
fn postgres_query_cancellation_interrupts_and_reuses_connection() {
    let Ok(uri) = std::env::var("MUX_TEST_POSTGRES_URL") else {
        eprintln!(
            "skipping postgres_query_cancellation_interrupts_and_reuses_connection: MUX_TEST_POSTGRES_URL not set"
        );
        return;
    };

    let conn = connect(&uri);
    let token = mux_runtime::sync_primitives::mux_cancellation_new();
    let token_address = token as usize;
    let worker = thread::spawn(move || -> usize {
        thread::sleep(Duration::from_millis(50));
        unsafe {
            mux_runtime::sync_primitives::mux_cancellation_cancel(token_address as *const Value)
                as usize
        }
    });

    let query = sval("SELECT pg_sleep(10)");
    let result = mux_sql_connection_query_with_cancellation(conn, query, token);
    let cancel_result = worker.join().expect("cancellation worker panicked") as *mut Value;
    assert_ok(cancel_result);

    unsafe {
        assert!(mux_result_is_err(result));
        let error = mux_result_data(result);
        let kind = mux_sql_error_kind(error);
        assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 5_i32.to_ne_bytes()));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(query));
        assert!(mux_rc_dec(token));
    }

    // The cancel request must not leave the checked-out client unusable.
    let healthy_query = sval("SELECT 42");
    let healthy = mux_sql_connection_query(conn, healthy_query);
    assert!(unsafe { mux_result_is_ok(healthy) });
    unsafe {
        assert!(mux_rc_dec(healthy));
        assert!(mux_rc_dec(healthy_query));
        mux_sql_connection_close(conn);
        assert!(mux_rc_dec(conn));
    }
}

/// PostgreSQL query deadlines use the same wire-level cancel request as
/// cooperative cancellation. Exercise every public query wrapper that borrows
/// a PostgreSQL client and verify a cancelled operation leaves the client (or
/// pool's returned client) usable.
#[test]
fn postgres_query_timeout_interrupts_and_reuses_connection() {
    let Ok(uri) = std::env::var("MUX_TEST_POSTGRES_URL") else {
        eprintln!(
            "skipping postgres_query_timeout_interrupts_and_reuses_connection: MUX_TEST_POSTGRES_URL not set"
        );
        return;
    };

    let conn = connect(&uri);

    let direct_query = sval("SELECT pg_sleep(10)");
    assert_timeout(mux_sql_connection_query_with_timeout(
        conn,
        direct_query,
        50,
    ));
    unsafe {
        assert!(mux_rc_dec(direct_query));
    }

    let prepared_sql = sval("SELECT pg_sleep(10)");
    let prepared = ok_data(mux_sql_connection_prepare(conn, prepared_sql));
    unsafe {
        assert!(mux_rc_dec(prepared_sql));
    }
    let empty_params = mux_rc_alloc(Value::List(vec![]));
    assert_timeout(mux_sql_prepared_query_with_timeout(
        prepared,
        empty_params,
        50,
    ));
    unsafe {
        assert!(mux_rc_dec(empty_params));
        mux_sql_prepared_close(prepared);
        assert!(mux_rc_dec(prepared));
    }

    let transaction = ok_data(mux_sql_connection_begin_transaction(conn));
    let transaction_query = sval("SELECT pg_sleep(10)");
    assert_timeout(mux_sql_transaction_query_with_timeout(
        transaction,
        transaction_query,
        50,
    ));
    unsafe {
        assert!(mux_rc_dec(transaction_query));
    }
    // A cancelled statement aborts the PostgreSQL transaction; rollback also
    // proves the transaction's client reached the protocol-ready state.
    assert_ok(mux_sql_transaction_rollback(transaction));
    unsafe {
        assert!(mux_rc_dec(transaction));
    }

    let pool_uri = sval(&uri);
    let pool = ok_data(mux_sql_pool_from_config(pool_uri, 1, 1_000));
    unsafe {
        assert!(mux_rc_dec(pool_uri));
    }
    let pool_query = sval("SELECT pg_sleep(10)");
    assert_timeout(mux_sql_pool_query_with_timeout(pool, pool_query, 50));
    unsafe {
        assert!(mux_rc_dec(pool_query));
    }
    let healthy_pool_query = sval("SELECT 42");
    let healthy_pool = mux_sql_pool_query(pool, healthy_pool_query);
    assert!(unsafe { mux_result_is_ok(healthy_pool) });
    unsafe {
        assert!(mux_rc_dec(healthy_pool));
        assert!(mux_rc_dec(healthy_pool_query));
        assert_ok(mux_sql_pool_close(pool));
        assert!(mux_rc_dec(pool));
    }

    let healthy_query = sval("SELECT 42");
    let healthy = mux_sql_connection_query(conn, healthy_query);
    assert!(unsafe { mux_result_is_ok(healthy) });
    unsafe {
        assert!(mux_rc_dec(healthy));
        assert!(mux_rc_dec(healthy_query));
        mux_sql_connection_close(conn);
        assert!(mux_rc_dec(conn));
    }
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
fn sqlserver_malformed_uri_is_typed_error() {
    let uri = CString::new("sqlserver://host").unwrap();
    let result = unsafe { mux_sql_connect(uri.as_ptr()) };
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let kind = unsafe { mux_sql_error_kind(error) };
    let provider = unsafe { mux_sql_error_provider(error) };
    let operation = unsafe { mux_sql_error_operation(error) };
    assert!(unsafe {
        matches!(&*kind, Value::Opaque(value) if value.as_ref() == 3_i32.to_ne_bytes())
            && matches!(&*provider, Value::String(value) if value == "sqlserver")
            && matches!(&*operation, Value::String(value) if value == "connect")
    });
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
    }
}
