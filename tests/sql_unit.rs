//! Unit tests for the SQL layer against an in-memory `SQLite` database
//! (feature-gated behind `sql`). Postgres/MySQL paths need live servers and are
//! not exercised here.
#![cfg(feature = "sql")]

mod common;

use std::ffi::CString;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{assert_err, assert_ok, ok_int};
use mux_runtime::datetime_types::mux_datetime_datetime_parse;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec, mux_value_deep_clone};
use mux_runtime::result::{mux_result_data, mux_result_is_err, mux_result_is_ok};
use mux_runtime::sql::*;
use mux_runtime::uuid::mux_uuid_parse;
use mux_runtime::Value;

fn sval(s: &str) -> *mut Value {
    mux_rc_alloc(Value::String(s.to_string()))
}

/// Assert Ok and return the inner value (caller frees it). Frees the result.
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

fn assert_provider_code(result: *mut Value, operation: &str) {
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let provider = unsafe { mux_sql_error_provider(error) };
    let code = unsafe { mux_sql_error_code(error) };
    let actual_operation = unsafe { mux_sql_error_operation(error) };
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "sqlite") });
    assert!(unsafe { matches!(&*code, Value::String(value) if value == "2067") });
    assert!(unsafe { matches!(&*actual_operation, Value::String(value) if value == operation) });
    unsafe {
        assert!(mux_rc_dec(actual_operation));
        assert!(mux_rc_dec(code));
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
    }
}

fn connect_memory() -> *mut Value {
    ok_data(mux_sql_sqlite_memory())
}

fn temporary_sqlite_pool_uri() -> (PathBuf, *mut Value) {
    let path = std::env::temp_dir().join(format!(
        "mux-sql-pool-{}-{}.db",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos()
    ));
    let uri = format!("sqlite://{}", path.to_string_lossy());
    (path, sval(&uri))
}

#[test]
fn copied_sql_handles_survive_source_drop_and_share_close_and_cursor_state() {
    unsafe {
        let original = ok_data(mux_sql_sqlite_memory());
        let conn = mux_value_deep_clone(original);
        assert!(!conn.is_null());
        mux_rc_dec(original);
        let query = sval("SELECT 1 AS value UNION ALL SELECT 2");
        let prepared = ok_data(mux_sql_connection_prepare(conn, query));
        let prepared_alias = mux_value_deep_clone(prepared);
        assert!(!prepared_alias.is_null());
        mux_rc_dec(prepared);
        let params = mux_rc_alloc(Value::List(vec![]));
        let rows = ok_data(mux_sql_prepared_query(prepared_alias, params));
        let rows_alias = mux_value_deep_clone(rows);
        assert!(!rows_alias.is_null());
        let first = resultset_next(rows);
        let first_row = mux_runtime::optional::mux_optional_data(first);
        let row_alias = mux_value_deep_clone(first_row);
        assert!(!row_alias.is_null());
        let second = resultset_next(rows_alias);
        let second_row = mux_runtime::optional::mux_optional_data(second);
        assert!(row_at_equals(&*row_alias, 0, &Value::Int(1)));
        assert!(row_at_equals(&*second_row, 0, &Value::Int(2)));
        for value in [rows, rows_alias, first, first_row, second, second_row] {
            mux_rc_dec(value);
        }
        assert!(row_at_equals(&*row_alias, 0, &Value::Int(1)));
        let closed_prepared = mux_value_deep_clone(prepared_alias);
        mux_sql_prepared_close(prepared_alias);
        assert_err(mux_sql_prepared_query(closed_prepared, params));
        let closed_conn = mux_value_deep_clone(conn);
        mux_sql_connection_close(conn);
        assert_err(mux_sql_connection_query(closed_conn, query));
        for value in [
            row_alias,
            closed_prepared,
            prepared_alias,
            params,
            closed_conn,
            conn,
            query,
        ] {
            mux_rc_dec(value);
        }
    }
}

#[test]
fn closing_connection_invalidates_prepared_statements_bound_to_it() {
    unsafe {
        let connection = ok_data(mux_sql_sqlite_memory());
        let statement_sql = sval("SELECT 1");
        let prepared = ok_data(mux_sql_connection_prepare(connection, statement_sql));
        let retained_prepared = mux_value_deep_clone(prepared);
        assert!(!retained_prepared.is_null());

        mux_sql_connection_close(connection);

        let params = mux_rc_alloc(Value::List(vec![]));
        assert_err(mux_sql_prepared_query(retained_prepared, params));

        assert!(mux_rc_dec(params));
        assert!(mux_rc_dec(retained_prepared));
        assert!(mux_rc_dec(prepared));
        assert!(mux_rc_dec(statement_sql));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn resultset_next_batch_consumes_at_most_requested_rows() {
    unsafe {
        let conn = ok_data(mux_sql_sqlite_memory());
        let query = sval("SELECT 1 AS value UNION ALL SELECT 2 UNION ALL SELECT 3");
        let resultset = ok_data(mux_sql_connection_query(conn, query));
        assert!(mux_rc_dec(query));

        let first = resultset_next_batch(resultset, 2);
        assert!(matches!(&*first, Value::List(values) if values.len() == 2));
        if let Value::List(values) = &*first {
            assert!(row_at_equals(&values[0], 0, &Value::Int(1)));
            assert!(row_at_equals(&values[1], 0, &Value::Int(2)));
        }
        assert!(mux_rc_dec(first));

        let second = resultset_next_batch(resultset, 2);
        assert!(matches!(&*second, Value::List(values) if values.len() == 1));
        if let Value::List(values) = &*second {
            assert!(row_at_equals(&values[0], 0, &Value::Int(3)));
        }
        assert!(mux_rc_dec(second));

        let exhausted = resultset_next_batch(resultset, 2);
        assert!(matches!(&*exhausted, Value::List(values) if values.is_empty()));
        assert!(mux_rc_dec(exhausted));

        let non_positive = resultset_next_batch(resultset, -1);
        assert!(matches!(&*non_positive, Value::List(values) if values.is_empty()));
        assert!(mux_rc_dec(non_positive));
        assert!(mux_rc_dec(resultset));

        let large_limit_query = sval("SELECT 4 AS value");
        let large_limit_resultset = ok_data(mux_sql_connection_query(conn, large_limit_query));
        let large_limit_batch = resultset_next_batch(large_limit_resultset, i64::MAX);
        assert!(matches!(&*large_limit_batch, Value::List(values) if values.len() == 1));
        assert!(mux_rc_dec(large_limit_batch));
        assert!(mux_rc_dec(large_limit_resultset));
        assert!(mux_rc_dec(large_limit_query));
        assert!(mux_rc_dec(conn));
    }
}

#[test]
fn sqlite_cursor_keeps_lease_until_eof_then_releases_it() {
    unsafe {
        let conn = ok_data(mux_sql_sqlite_memory());
        let query = sval("SELECT 1 AS value UNION ALL SELECT 2 UNION ALL SELECT 3");
        let resultset = ok_data(mux_sql_connection_query(conn, query));
        assert!(mux_rc_dec(query));

        let first = resultset_next(resultset);
        let first_row = mux_runtime::optional::mux_optional_data(first);
        assert!(row_at_equals(&*first_row, 0, &Value::Int(1)));
        assert!(mux_rc_dec(first_row));
        assert!(mux_rc_dec(first));

        let blocked_query = sval("SELECT 99");
        assert_err(mux_sql_connection_query(conn, blocked_query));
        assert!(mux_rc_dec(blocked_query));

        let remaining = resultset_rows(resultset);
        assert!(matches!(&*remaining, Value::List(rows) if rows.len() == 2));
        assert!(mux_rc_dec(remaining));

        let after_eof = sval("SELECT 99");
        let available = ok_data(mux_sql_connection_query(conn, after_eof));
        assert!(mux_rc_dec(after_eof));
        assert!(mux_rc_dec(available));
        assert!(mux_rc_dec(resultset));
        assert!(mux_rc_dec(conn));
    }
}

#[test]
fn dropping_transaction_handle_during_sqlite_cursor_defers_backend_cleanup() {
    unsafe {
        let conn = ok_data(mux_sql_sqlite_memory());
        let tx = ok_data(mux_sql_connection_begin_transaction(conn));
        let query = sval("SELECT 1 AS value UNION ALL SELECT 2");
        let resultset = ok_data(mux_sql_transaction_query(tx, query));
        assert!(mux_rc_dec(query));

        assert!(mux_rc_dec(tx));
        let rows = resultset_rows(resultset);
        assert!(matches!(&*rows, Value::List(values) if values.len() == 2));
        assert!(mux_rc_dec(rows));
        assert!(mux_rc_dec(resultset));
        mux_sql_connection_close(conn);
        assert!(mux_rc_dec(conn));
    }
}

#[test]
fn open_connection_resultset_blocks_operations_until_close_and_close_is_idempotent() {
    unsafe {
        let conn = ok_data(mux_sql_sqlite_memory());
        let query = sval("SELECT 1");
        let resultset = ok_data(mux_sql_connection_query(conn, query));
        assert!(mux_rc_dec(query));

        let blocked = sval("SELECT 2");
        assert_err(mux_sql_connection_query(conn, blocked));
        assert!(mux_rc_dec(blocked));

        assert_ok(mux_sql_resultset_close(resultset));
        assert_ok(mux_sql_resultset_close(resultset));

        let available = sval("SELECT 3");
        let next = ok_data(mux_sql_connection_query(conn, available));
        assert!(mux_rc_dec(available));
        assert!(mux_rc_dec(next));
        assert!(mux_rc_dec(resultset));
        mux_sql_connection_close(conn);
        assert!(mux_rc_dec(conn));
    }
}

#[test]
fn pool_close_returns_with_an_open_resultset_and_releases_after_close() {
    let (pool_path, uri) = temporary_sqlite_pool_uri();
    let pool = ok_data(mux_sql_pool_from_config(uri, 1, 0));
    assert!(unsafe { mux_rc_dec(uri) });

    let query = sval("SELECT 1 AS value UNION ALL SELECT 2");
    let resultset = ok_data(mux_sql_pool_query(pool, query));
    assert!(unsafe { mux_rc_dec(query) });

    // Closing a pool cannot wait for a result set on this synchronous thread.
    // The active lease remains counted until the result set releases it.
    assert_ok(mux_sql_pool_close(pool));
    let metrics = ok_data(mux_sql_pool_metrics(pool));
    assert!(unsafe {
        matches!(&*metrics, Value::Map(values)
            if values.get(&Value::String("closed".into())) == Some(&Value::Int(1))
            && values.get(&Value::String("total".into())) == Some(&Value::Int(1))
            && values.get(&Value::String("idle".into())) == Some(&Value::Int(0))
            && values.get(&Value::String("in_use".into())) == Some(&Value::Int(1)))
    });
    assert!(unsafe { mux_rc_dec(metrics) });

    assert_ok(mux_sql_resultset_close(resultset));
    let metrics = ok_data(mux_sql_pool_metrics(pool));
    assert!(unsafe {
        matches!(&*metrics, Value::Map(values)
            if values.get(&Value::String("total".into())) == Some(&Value::Int(0))
            && values.get(&Value::String("idle".into())) == Some(&Value::Int(0))
            && values.get(&Value::String("in_use".into())) == Some(&Value::Int(0)))
    });
    assert!(unsafe { mux_rc_dec(metrics) });

    let rejected_query = sval("SELECT 3");
    assert_err(mux_sql_pool_query(pool, rejected_query));
    assert!(unsafe { mux_rc_dec(rejected_query) });
    assert_ok(mux_sql_pool_close(pool));
    assert!(unsafe { mux_rc_dec(resultset) });
    assert!(unsafe { mux_rc_dec(pool) });
    fs::remove_file(pool_path).expect("remove temporary SQLite pool database");
}

#[test]
fn dropping_open_pool_resultset_returns_the_connection_to_the_pool() {
    let (pool_path, uri) = temporary_sqlite_pool_uri();
    let pool = ok_data(mux_sql_pool_from_config(uri, 1, 0));
    assert!(unsafe { mux_rc_dec(uri) });

    let query = sval("SELECT 1 AS value UNION ALL SELECT 2");
    let resultset = ok_data(mux_sql_pool_query(pool, query));
    assert!(unsafe { mux_rc_dec(query) });

    assert!(unsafe { mux_rc_dec(resultset) });
    let metrics = ok_data(mux_sql_pool_metrics(pool));
    assert!(unsafe {
        matches!(&*metrics, Value::Map(values)
            if values.get(&Value::String("total".into())) == Some(&Value::Int(1))
            && values.get(&Value::String("idle".into())) == Some(&Value::Int(1))
            && values.get(&Value::String("in_use".into())) == Some(&Value::Int(0)))
    });
    assert!(unsafe { mux_rc_dec(metrics) });

    assert_ok(mux_sql_pool_close(pool));
    assert!(unsafe { mux_rc_dec(pool) });
    fs::remove_file(pool_path).expect("remove temporary SQLite pool database");
}

#[test]
fn resultset_leases_cover_prepared_transactions_and_pools() {
    unsafe {
        let connection = connect_memory();
        let query = sval("SELECT 1");
        let prepared = ok_data(mux_sql_connection_prepare(connection, query));
        let params = mux_rc_alloc(Value::List(vec![]));
        let resultset = ok_data(mux_sql_prepared_query(prepared, params));

        let blocked = mux_sql_prepared_execute(prepared, params);
        assert_err(blocked);
        assert_ok(mux_sql_resultset_close(resultset));
        let available_prepared_resultset = ok_data(mux_sql_prepared_query(prepared, params));
        assert!(mux_rc_dec(available_prepared_resultset));
        assert!(mux_rc_dec(resultset));
        mux_sql_prepared_close(prepared);
        assert!(mux_rc_dec(prepared));
        assert!(mux_rc_dec(params));
        assert!(mux_rc_dec(query));

        let transaction = ok_data(mux_sql_connection_begin_transaction(connection));
        let transaction_query = sval("SELECT 2");
        let transaction_resultset =
            ok_data(mux_sql_transaction_query(transaction, transaction_query));
        let blocked_transaction_sql = sval("SELECT 3");
        assert_err(mux_sql_transaction_execute(
            transaction,
            blocked_transaction_sql,
        ));
        assert!(mux_rc_dec(blocked_transaction_sql));
        assert_err(mux_sql_transaction_commit(transaction));
        assert_ok(mux_sql_resultset_close(transaction_resultset));
        assert_ok(mux_sql_transaction_commit(transaction));
        assert!(mux_rc_dec(transaction_resultset));
        assert!(mux_rc_dec(transaction_query));
        assert!(mux_rc_dec(transaction));

        let (pool_path, pool_uri) = temporary_sqlite_pool_uri();
        let pool = ok_data(mux_sql_pool_from_config(pool_uri, 1, 0));
        let create = sval("CREATE TABLE values_table (value INTEGER)");
        assert_ok(mux_sql_pool_execute(pool, create));
        assert!(mux_rc_dec(create));
        let pool_query = sval("SELECT 1 AS value");
        let pool_resultset = ok_data(mux_sql_pool_query(pool, pool_query));
        let pool_blocked = sval("SELECT 2");
        assert_err(mux_sql_pool_query(pool, pool_blocked));
        assert!(mux_rc_dec(pool_blocked));
        assert_ok(mux_sql_resultset_close(pool_resultset));
        let pool_available = sval("SELECT 3");
        assert_ok(mux_sql_pool_query(pool, pool_available));
        assert!(mux_rc_dec(pool_available));
        assert!(mux_rc_dec(pool_resultset));
        assert!(mux_rc_dec(pool_query));
        assert_ok(mux_sql_pool_close(pool));
        assert!(mux_rc_dec(pool));
        assert!(mux_rc_dec(pool_uri));
        fs::remove_file(pool_path).expect("remove temporary SQLite pool database");

        mux_sql_connection_close(connection);
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn sqlite_query_timeout_interrupts_long_running_statement() {
    let connection = connect_memory();
    let query = sval(
        "WITH RECURSIVE numbers(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 100000000) SELECT value FROM numbers",
    );
    let result = mux_sql_connection_query_with_timeout(connection, query, 1);
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let kind = unsafe { mux_sql_error_kind(error) };
    assert!(unsafe {
        matches!(&*kind, Value::Opaque(value) if value.as_ref() == 1_i32.to_ne_bytes())
    });
    unsafe {
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(query));
    }

    // The progress hook must be removed even after interruption; a later
    // statement on the same connection should run normally.
    let healthy_query = sval("SELECT 42");
    let healthy = mux_sql_connection_query(connection, healthy_query);
    assert!(unsafe { mux_result_is_ok(healthy) });
    unsafe {
        assert!(mux_rc_dec(healthy));
        assert!(mux_rc_dec(healthy_query));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn sqlite_query_timeout_keeps_large_valid_deadlines_non_immediate() {
    let connection = connect_memory();
    let query = sval("SELECT 42");
    // The public timeout is an i64 millisecond count. A valid, very large
    // value must not overflow an absolute Instant deadline and turn into an
    // immediate timeout.
    let resultset = ok_data(mux_sql_connection_query_with_timeout(
        connection,
        query,
        i64::MAX,
    ));
    unsafe {
        assert!(mux_rc_dec(resultset));
        assert!(mux_rc_dec(query));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn sqlite_query_timeout_applies_to_prepared_transaction_and_pool_queries() {
    let long_query = || {
        sval("WITH RECURSIVE numbers(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 100000000) SELECT value FROM numbers")
    };

    unsafe {
        let connection = ok_data(mux_sql_sqlite_memory());

        let prepared_sql = sval("SELECT value FROM (WITH RECURSIVE numbers(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 100000000) SELECT value FROM numbers)");
        let prepared = ok_data(mux_sql_connection_prepare(connection, prepared_sql));
        assert!(mux_rc_dec(prepared_sql));
        let empty_params = mux_rc_alloc(Value::List(vec![]));
        assert_timeout_result(mux_sql_prepared_query_with_timeout(
            prepared,
            empty_params,
            1,
        ));
        assert!(mux_rc_dec(empty_params));
        mux_sql_prepared_close(prepared);
        assert!(mux_rc_dec(prepared));

        let transaction = ok_data(mux_sql_connection_begin_transaction(connection));
        let transaction_query = long_query();
        assert_timeout_result(mux_sql_transaction_query_with_timeout(
            transaction,
            transaction_query,
            1,
        ));
        assert!(mux_rc_dec(transaction_query));
        assert_ok(mux_sql_transaction_rollback(transaction));
        assert!(mux_rc_dec(transaction));

        let (pool_path, pool_uri) = temporary_sqlite_pool_uri();
        let pool = ok_data(mux_sql_pool_from_config(pool_uri, 1, 0));
        assert!(mux_rc_dec(pool_uri));
        let pool_query = long_query();
        assert_timeout_result(mux_sql_pool_query_with_timeout(pool, pool_query, 1));
        assert!(mux_rc_dec(pool_query));
        assert_ok(mux_sql_pool_close(pool));
        assert!(mux_rc_dec(pool));
        fs::remove_file(pool_path).expect("remove temporary SQLite pool database");
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn sqlite_query_cancellation_interrupts_running_statement_and_cleans_up() {
    let connection = connect_memory();
    let token = mux_runtime::sync_primitives::mux_cancellation_new();
    let query = sval(
        "WITH RECURSIVE numbers(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000000000) SELECT sum(value) FROM numbers",
    );

    // SQL handles are thread-affine, so keep the connection and query on this
    // thread. Cancellation tokens are shared atomics and can be cancelled by
    // another Mux thread while SQLite is executing the progress callback.
    let token_address = token as usize;
    let worker = thread::spawn(move || unsafe {
        thread::sleep(Duration::from_millis(10));
        mux_runtime::sync_primitives::mux_cancellation_cancel(token_address as *const Value)
            as usize
    });
    let result = mux_sql_connection_query_with_cancellation(connection, query, token);
    let cancel_result = worker.join().expect("cancellation worker panicked") as *mut Value;
    assert_ok(cancel_result);

    unsafe {
        assert_cancelled_result(result);
        assert!(mux_rc_dec(query));
        assert!(mux_rc_dec(token));
    }

    // Cancellation hooks are scoped to one operation. A subsequent query on
    // the same connection remains usable after the interrupted statement.
    let healthy_query = sval("SELECT 42");
    let healthy = mux_sql_connection_query(connection, healthy_query);
    assert!(unsafe { mux_result_is_ok(healthy) });
    unsafe {
        assert!(mux_rc_dec(healthy));
        assert!(mux_rc_dec(healthy_query));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn sqlite_query_cancellation_applies_to_prepared_transaction_and_pool_queries() {
    unsafe {
        let connection = ok_data(mux_sql_sqlite_memory());
        let token = mux_runtime::sync_primitives::mux_cancellation_new();
        assert_ok(mux_runtime::sync_primitives::mux_cancellation_cancel(token));

        let prepared_sql = sval("SELECT 1");
        let prepared = ok_data(mux_sql_connection_prepare(connection, prepared_sql));
        let params = mux_rc_alloc(Value::List(vec![]));
        assert_cancelled_result(mux_sql_prepared_query_with_cancellation(
            prepared, params, token,
        ));
        assert!(mux_rc_dec(params));
        mux_sql_prepared_close(prepared);
        assert!(mux_rc_dec(prepared_sql));
        assert!(mux_rc_dec(prepared));

        let transaction = ok_data(mux_sql_connection_begin_transaction(connection));
        let transaction_query = sval("SELECT 1");
        assert_cancelled_result(mux_sql_transaction_query_with_cancellation(
            transaction,
            transaction_query,
            token,
        ));
        assert!(mux_rc_dec(transaction_query));
        assert_ok(mux_sql_transaction_rollback(transaction));
        assert!(mux_rc_dec(transaction));

        let (pool_path, pool_uri) = temporary_sqlite_pool_uri();
        let pool = ok_data(mux_sql_pool_from_config(pool_uri, 1, 0));
        assert!(mux_rc_dec(pool_uri));
        let pool_query = sval("SELECT 1");
        assert_cancelled_result(mux_sql_pool_query_with_cancellation(
            pool, pool_query, token,
        ));
        assert!(mux_rc_dec(pool_query));
        assert_ok(mux_sql_pool_close(pool));
        assert!(mux_rc_dec(pool));
        fs::remove_file(pool_path).expect("remove temporary SQLite pool database");
        assert!(mux_rc_dec(token));
        assert!(mux_rc_dec(connection));
    }
}

unsafe fn assert_timeout_result(result: *mut Value) {
    assert!(mux_result_is_err(result), "expected timeout error");
    let error = mux_result_data(result);
    let kind = mux_sql_error_kind(error);
    assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 1_i32.to_ne_bytes()));
    assert!(mux_rc_dec(kind));
    assert!(mux_rc_dec(error));
    assert!(mux_rc_dec(result));
}

unsafe fn assert_cancelled_result(result: *mut Value) {
    assert!(mux_result_is_err(result), "expected cancelled query");
    let error = mux_result_data(result);
    let kind = mux_sql_error_kind(error);
    assert!(matches!(&*kind, Value::Opaque(value) if value.as_ref() == 5_i32.to_ne_bytes()));
    assert!(mux_rc_dec(kind));
    assert!(mux_rc_dec(error));
    assert!(mux_rc_dec(result));
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

#[test]
fn sql_interrupt_capabilities_are_explicit_for_connections_and_pools() {
    unsafe {
        let connection = connect_memory();
        assert_interrupt_capabilities(
            ok_data(mux_sql_connection_capabilities(connection)),
            "sqlite",
            true,
            true,
        );

        let (pool_path, pool_uri) = temporary_sqlite_pool_uri();
        let pool = ok_data(mux_sql_pool_from_config(pool_uri, 1, 0));
        assert!(mux_rc_dec(pool_uri));
        assert_interrupt_capabilities(
            ok_data(mux_sql_pool_capabilities(pool)),
            "sqlite",
            true,
            true,
        );
        assert_ok(mux_sql_pool_close(pool));
        assert!(mux_rc_dec(pool));
        fs::remove_file(pool_path).expect("remove temporary SQLite pool database");
        mux_sql_connection_close(connection);
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn copied_transaction_rolls_back_only_when_its_last_alias_is_dropped() {
    unsafe {
        let conn = ok_data(mux_sql_sqlite_memory());
        let create = sval("CREATE TABLE shared_transaction (value INTEGER)");
        assert_ok(mux_sql_connection_execute(conn, create));
        let transaction = ok_data(mux_sql_connection_begin_transaction(conn));
        let alias = mux_value_deep_clone(transaction);
        assert!(!alias.is_null());
        mux_rc_dec(transaction);
        let insert = sval("INSERT INTO shared_transaction VALUES (1)");
        assert_ok(mux_sql_transaction_execute(alias, insert));
        mux_rc_dec(alias);
        let query = sval("SELECT count(*) FROM shared_transaction");
        let rows = ok_data(mux_sql_connection_query(conn, query));
        let next = resultset_next(rows);
        let row = mux_runtime::optional::mux_optional_data(next);
        assert!(row_at_equals(&*row, 0, &Value::Int(0)));
        for value in [row, next, rows, query, insert, create, conn] {
            mux_rc_dec(value);
        }
    }
}

#[test]
fn nested_transactions_commit_rollback_drop_and_suspend_parent() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE nested (value INTEGER)");
    assert_ok(mux_sql_connection_execute(conn, create));
    let outer = ok_data(mux_sql_connection_begin_transaction(conn));
    let insert = sval("INSERT INTO nested VALUES (1)");
    assert_ok(mux_sql_transaction_execute(outer, insert));
    let child = ok_data(mux_sql_transaction_begin_transaction(outer));
    let blocked = mux_sql_transaction_execute(outer, insert);
    assert!(unsafe { mux_result_is_err(blocked) });
    let blocked_error = unsafe { mux_result_data(blocked) };
    let provider = unsafe { mux_sql_error_provider(blocked_error) };
    let operation = unsafe { mux_sql_error_operation(blocked_error) };
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "sqlite") });
    assert!(unsafe { matches!(&*operation, Value::String(value) if value == "execute") });
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(blocked_error));
        assert!(mux_rc_dec(blocked));
    }
    assert_err(mux_sql_transaction_commit(outer));
    assert_err(mux_sql_transaction_begin_transaction(outer));
    assert_ok(mux_sql_transaction_execute(child, insert));
    let grandchild = ok_data(mux_sql_transaction_begin_transaction(child));
    assert_ok(mux_sql_transaction_execute(grandchild, insert));
    assert_ok(mux_sql_transaction_commit(grandchild));
    assert_ok(mux_sql_transaction_rollback(child));
    let dropped = ok_data(mux_sql_transaction_begin_transaction(outer));
    assert_ok(mux_sql_transaction_execute(dropped, insert));
    unsafe { mux_rc_dec(dropped) };
    let committed = ok_data(mux_sql_transaction_begin_transaction(outer));
    assert_ok(mux_sql_transaction_execute(committed, insert));
    assert_ok(mux_sql_transaction_commit(committed));
    assert_ok(mux_sql_transaction_commit(outer));
    let query = sval("SELECT count(*) FROM nested");
    let rows = ok_data(mux_sql_connection_query(conn, query));
    let values = resultset_rows(rows);
    assert!(unsafe {
        matches!(&*values, Value::List(items) if items.len() == 1 && row_at_equals(&items[0], 0, &Value::Int(2)))
    });
    unsafe {
        for value in [
            values, rows, query, committed, grandchild, child, outer, insert, create, conn,
        ] {
            mux_rc_dec(value);
        }
    }
}

#[test]
fn nested_transaction_retains_parents_and_root_drop_restores_connection() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE retained (value INTEGER)");
    assert_ok(mux_sql_connection_execute(conn, create));
    let outer = ok_data(mux_sql_connection_begin_transaction(conn));
    let child = ok_data(mux_sql_transaction_begin_transaction(outer));
    let insert = sval("INSERT INTO retained VALUES (1)");
    assert_ok(mux_sql_transaction_execute(child, insert));
    unsafe { mux_rc_dec(outer) };
    assert_ok(mux_sql_transaction_commit(child));
    let query = sval("SELECT count(*) FROM retained");
    let rows = ok_data(mux_sql_connection_query(conn, query));
    let values = resultset_rows(rows);
    assert!(unsafe {
        matches!(&*values, Value::List(items) if items.len() == 1 && row_at_equals(&items[0], 0, &Value::Int(0)))
    });
    unsafe {
        for value in [values, rows, query, child, insert, create, conn] {
            mux_rc_dec(value);
        }
    }
}

fn row_at_equals(row: &Value, index: i64, expected: &Value) -> bool {
    let row_ptr = mux_rc_alloc(row.clone());
    let value = ok_data(mux_sql_row_at(row_ptr, index));
    let equal = unsafe { &*value == expected };
    unsafe {
        assert!(mux_rc_dec(value));
        assert!(mux_rc_dec(row_ptr));
    }
    equal
}

#[test]
fn sqlite_memory_constructor_opens_connection() {
    let conn = ok_data(mux_sql_sqlite_memory());
    let query = sval("SELECT 1");
    let rows = ok_data(mux_sql_connection_query(conn, query));
    assert!(unsafe { mux_rc_dec(query) });
    assert!(unsafe { mux_rc_dec(rows) });
    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn connect_execute_query_lifecycle() {
    let conn = connect_memory();

    let create = sval("CREATE TABLE t (id INTEGER, name TEXT)");
    assert_ok(mux_sql_connection_execute(conn, create));
    assert!(unsafe { mux_rc_dec(create) });

    // parameterized insert
    let insert = sval("INSERT INTO t (id, name) VALUES (?, ?)");
    let params = mux_rc_alloc(Value::List(vec![
        Value::Int(1),
        Value::String("alice".into()),
    ]));
    assert_ok(mux_sql_connection_execute_params(conn, insert, params));
    assert!(unsafe { mux_rc_dec(insert) });
    assert!(unsafe { mux_rc_dec(params) });

    // query and inspect the resultset
    let select = sval("SELECT id, name FROM t");
    let rs = ok_data(mux_sql_connection_query(conn, select));
    assert!(unsafe { mux_rc_dec(select) });

    // Resultset accessors return bare List/Optional values (not Result).
    let cols = mux_sql_resultset_columns(rs);
    assert!(!cols.is_null());
    assert!(unsafe { mux_rc_dec(cols) });
    let rows = resultset_rows(rs);
    assert!(!rows.is_null());
    assert!(unsafe { mux_rc_dec(rows) });
    let next = resultset_next(rs);
    assert!(!next.is_null());
    assert!(unsafe { mux_rc_dec(next) });
    assert!(unsafe { mux_rc_dec(rs) });

    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn ordered_rows_preserve_duplicate_columns_and_report_ambiguity() {
    let conn = connect_memory();
    let query = sval("SELECT 1 AS value, 2 AS value");
    let rs = ok_data(mux_sql_connection_query(conn, query));
    assert!(unsafe { mux_rc_dec(query) });

    let rows = resultset_rows(rs);
    let row_value = unsafe {
        match &*rows {
            Value::List(values) if values.len() == 1 => values[0].clone(),
            other => panic!("expected one ordered row, got {other:?}"),
        }
    };
    let row = mux_rc_alloc(row_value);
    let columns = mux_sql_row_columns(row);
    assert!(unsafe {
        matches!(&*columns, Value::List(values) if values == &[Value::String("value".into()), Value::String("value".into())])
    });
    assert!(unsafe { mux_rc_dec(columns) });

    let name = sval("value");
    let ambiguous = mux_sql_row_get(row, name);
    assert!(unsafe { mux_result_is_err(ambiguous) });
    let ambiguous_error = unsafe { mux_result_data(ambiguous) };
    let provider = unsafe { mux_sql_error_provider(ambiguous_error) };
    let operation = unsafe { mux_sql_error_operation(ambiguous_error) };
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "sqlite") });
    assert!(unsafe { matches!(&*operation, Value::String(value) if value == "row_get") });
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(ambiguous_error));
        assert!(mux_rc_dec(ambiguous));
    }
    assert!(unsafe { mux_rc_dec(name) });

    let second = ok_data(mux_sql_row_at(row, 1));
    assert!(unsafe { matches!(&*second, Value::Int(2)) });
    assert!(unsafe { mux_rc_dec(second) });
    assert!(unsafe { mux_rc_dec(row) });
    assert!(unsafe { mux_rc_dec(rows) });
    assert!(unsafe { mux_rc_dec(rs) });
    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn sqlite_pool_reuses_connection_and_reports_metrics() {
    let (pool_path, uri) = temporary_sqlite_pool_uri();
    let pool = ok_data(mux_sql_pool_from_config(uri, 1, 0));
    assert!(unsafe { mux_rc_dec(uri) });
    let create = sval("CREATE TABLE items (value INTEGER)");
    assert_ok(mux_sql_pool_execute(pool, create));
    assert!(unsafe { mux_rc_dec(create) });
    let insert = sval("INSERT INTO items VALUES (?)");
    let params = mux_rc_alloc(Value::List(vec![Value::Int(7)]));
    assert_ok(mux_sql_pool_execute_params(pool, insert, params));
    assert!(unsafe { mux_rc_dec(insert) });
    assert!(unsafe { mux_rc_dec(params) });

    let query = sval("SELECT value FROM items");
    let resultset = ok_data(mux_sql_pool_query(pool, query));
    assert!(unsafe { mux_rc_dec(query) });
    let rows = resultset_rows(resultset);
    assert!(unsafe {
        matches!(&*rows, Value::List(values) if values.len() == 1
            && row_at_equals(&values[0], 0, &Value::Int(7)))
    });
    assert!(unsafe { mux_rc_dec(rows) });
    assert!(unsafe { mux_rc_dec(resultset) });

    let metrics = ok_data(mux_sql_pool_metrics(pool));
    assert!(unsafe {
        matches!(&*metrics, Value::Map(values)
            if values.get(&Value::String("capacity".into())) == Some(&Value::Int(1))
            && values.get(&Value::String("idle".into())) == Some(&Value::Int(1)))
    });
    assert!(unsafe { mux_rc_dec(metrics) });
    assert_ok(mux_sql_pool_close(pool));
    assert!(unsafe { mux_rc_dec(pool) });
    fs::remove_file(pool_path).expect("remove temporary SQLite pool database");
}

#[test]
fn transaction_commit_and_rollback() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE t (n INTEGER)");
    assert_ok(mux_sql_connection_execute(conn, create));
    assert!(unsafe { mux_rc_dec(create) });

    // commit path
    let tx = ok_data(mux_sql_connection_begin_transaction(conn));
    let ins = sval("INSERT INTO t (n) VALUES (1)");
    assert_ok(mux_sql_transaction_execute(tx, ins));
    assert!(unsafe { mux_rc_dec(ins) });
    assert_ok(mux_sql_transaction_commit(tx));
    assert!(unsafe { mux_rc_dec(tx) });

    // rollback path
    let tx2 = ok_data(mux_sql_connection_begin_transaction(conn));
    let ins2 = sval("INSERT INTO t (n) VALUES (2)");
    assert_ok(mux_sql_transaction_execute(tx2, ins2));
    assert!(unsafe { mux_rc_dec(ins2) });
    assert_ok(mux_sql_transaction_rollback(tx2));
    assert!(unsafe { mux_rc_dec(tx2) });

    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn transaction_options_reject_unsupported_sqlite_guarantees() {
    let connection = connect_memory();
    let isolation = sval("serializable");
    let read_only = mux_rc_alloc(Value::Bool(false));
    let deferrable = mux_rc_alloc(Value::Bool(false));
    let rejected = mux_sql_connection_begin_transaction_with_options(
        connection, isolation, read_only, deferrable,
    );
    assert_err(rejected);
    unsafe {
        assert!(mux_rc_dec(isolation));
        assert!(mux_rc_dec(read_only));
        assert!(mux_rc_dec(deferrable));
    }

    let isolation = sval("default");
    let read_only = mux_rc_alloc(Value::Bool(false));
    let deferrable = mux_rc_alloc(Value::Bool(false));
    let transaction = ok_data(mux_sql_connection_begin_transaction_with_options(
        connection, isolation, read_only, deferrable,
    ));
    assert_ok(mux_sql_transaction_rollback(transaction));
    unsafe {
        assert!(mux_rc_dec(isolation));
        assert!(mux_rc_dec(read_only));
        assert!(mux_rc_dec(deferrable));
        assert!(mux_rc_dec(transaction));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn transaction_savepoints_can_rollback_and_release() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE t (n INTEGER)");
    assert_ok(mux_sql_connection_execute(conn, create));
    assert!(unsafe { mux_rc_dec(create) });

    let tx = ok_data(mux_sql_connection_begin_transaction(conn));
    let first = sval("INSERT INTO t (n) VALUES (1)");
    assert_ok(mux_sql_transaction_execute(tx, first));
    assert!(unsafe { mux_rc_dec(first) });

    let name = sval("before_second");
    assert_ok(mux_sql_transaction_savepoint(tx, name));
    let second = sval("INSERT INTO t (n) VALUES (2)");
    assert_ok(mux_sql_transaction_execute(tx, second));
    assert!(unsafe { mux_rc_dec(second) });
    assert_ok(mux_sql_transaction_rollback_to(tx, name));
    assert_ok(mux_sql_transaction_release_savepoint(tx, name));
    assert!(unsafe { mux_rc_dec(name) });

    let third = sval("INSERT INTO t (n) VALUES (3)");
    assert_ok(mux_sql_transaction_execute(tx, third));
    assert!(unsafe { mux_rc_dec(third) });
    assert_ok(mux_sql_transaction_commit(tx));
    assert!(unsafe { mux_rc_dec(tx) });

    let query = sval("SELECT n FROM t ORDER BY n");
    let resultset = ok_data(mux_sql_connection_query(conn, query));
    assert!(unsafe { mux_rc_dec(query) });
    let rows = resultset_rows(resultset);
    assert!(unsafe {
        matches!(&*rows, Value::List(values) if values.len() == 2
            && row_at_equals(&values[0], 0, &Value::Int(1))
            && row_at_equals(&values[1], 0, &Value::Int(3)))
    });
    assert!(unsafe { mux_rc_dec(rows) });
    assert!(unsafe { mux_rc_dec(resultset) });

    let invalid_name = sval("bad-name");
    let tx2 = ok_data(mux_sql_connection_begin_transaction(conn));
    assert_err(mux_sql_transaction_savepoint(tx2, invalid_name));
    assert!(unsafe { mux_rc_dec(invalid_name) });
    assert_ok(mux_sql_transaction_rollback(tx2));
    assert!(unsafe { mux_rc_dec(tx2) });

    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn failed_commit_keeps_transaction_connection() {
    let conn = connect_memory();
    let tx = ok_data(mux_sql_connection_begin_transaction(conn));

    // End the backend transaction behind the wrapper's back. The wrapper's
    // subsequent COMMIT must fail, but it must retain the connection in the
    // transaction so another operation does not see "connection missing".
    let external_commit = sval("COMMIT");
    assert_ok(mux_sql_transaction_execute(tx, external_commit));
    assert!(unsafe { mux_rc_dec(external_commit) });

    assert_err(mux_sql_transaction_commit(tx));
    let query = sval("SELECT 1");
    let rs = ok_data(mux_sql_transaction_query(tx, query));
    assert!(unsafe { mux_rc_dec(query) });
    assert!(unsafe { mux_rc_dec(rs) });

    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(tx) });
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn failed_rollback_keeps_transaction_connection() {
    let conn = connect_memory();
    let tx = ok_data(mux_sql_connection_begin_transaction(conn));

    // End the backend transaction behind the wrapper's back so ROLLBACK
    // returns an error while the wrapper still owns its connection.
    let external_rollback = sval("ROLLBACK");
    assert_ok(mux_sql_transaction_execute(tx, external_rollback));
    assert!(unsafe { mux_rc_dec(external_rollback) });

    assert_err(mux_sql_transaction_rollback(tx));
    let query = sval("SELECT 1");
    let rs = ok_data(mux_sql_transaction_query(tx, query));
    assert!(unsafe { mux_rc_dec(query) });
    assert!(unsafe { mux_rc_dec(rs) });

    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(tx) });
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn sql_value_constructors_and_accessors() {
    let i = mux_sql_value_int(42);
    assert_ok(mux_sql_value_as_int(i));
    assert!(unsafe { mux_rc_dec(i) });

    let f = mux_sql_value_float(1.5);
    assert_ok(mux_sql_value_as_float(f));
    assert!(unsafe { mux_rc_dec(f) });

    let b = mux_sql_value_bool(true);
    assert_ok(mux_sql_value_as_bool(b));
    assert!(unsafe { mux_rc_dec(b) });

    let s = unsafe { mux_sql_value_string(CString::new("hi").unwrap().as_ptr()) };
    assert_ok(mux_sql_value_as_string(s));
    assert!(unsafe { mux_rc_dec(s) });

    let null = mux_sql_value_null();
    assert!(mux_sql_value_is_null(null));
    assert!(unsafe { mux_rc_dec(null) });

    let not_null = mux_sql_value_int(7);
    assert!(!mux_sql_value_is_null(not_null));
    assert!(unsafe { mux_rc_dec(not_null) });
}

#[test]
fn sql_json_constructor_and_accessor_round_trip_native_json() {
    let mut object = mux_runtime::ordered::OrderedMap::new();
    object.insert(Value::String("name".into()), Value::String("Mux".into()));
    object.insert(Value::String("count".into()), Value::Int(2));
    let json = mux_rc_alloc(Value::Map(object));

    let sql_value = ok_data(mux_sql_value_json(json));
    assert!(unsafe {
        matches!(&*sql_value, Value::String(text) if text == r#"{"name":"Mux","count":2}"#)
    });

    let decoded = ok_data(mux_sql_value_as_json(sql_value));
    let Value::Map(fields) = (unsafe { &*decoded }) else {
        panic!("expected native JSON object, got {decoded:?}");
    };
    assert_eq!(
        fields.get(&Value::String("name".into())),
        Some(&Value::String("Mux".into()))
    );
    assert_eq!(
        fields.get(&Value::String("count".into())),
        Some(&Value::Int(2))
    );

    unsafe {
        assert!(mux_rc_dec(decoded));
        assert!(mux_rc_dec(sql_value));
        assert!(mux_rc_dec(json));
    }
}

#[test]
fn sql_json_accessor_reports_invalid_json_text() {
    let value = mux_rc_alloc(Value::String("{not json".into()));
    assert_err(mux_sql_value_as_json(value));
    unsafe {
        assert!(mux_rc_dec(value));
    }
}

#[test]
fn sql_datetime_and_uuid_round_trip_as_typed_values() {
    let datetime_text = sval("2026-09-08T12:34:56.123456Z");
    let datetime = ok_data(unsafe { mux_datetime_datetime_parse(datetime_text) });
    let sql_datetime = ok_data(mux_sql_value_datetime(datetime));
    assert!(unsafe {
        matches!(&*sql_datetime, Value::String(text) if text == "2026-09-08T12:34:56.123456Z")
    });
    let decoded_datetime = ok_data(mux_sql_value_as_datetime(sql_datetime));
    let rendered =
        unsafe { mux_runtime::datetime_types::mux_datetime_datetime_to_string(decoded_datetime) };
    assert!(
        matches!(unsafe { &*rendered }, Value::String(text) if text == "2026-09-08T12:34:56.123456Z")
    );

    let uuid_text = sval("550e8400-e29b-41d4-a716-446655440000");
    let uuid = ok_data(unsafe { mux_uuid_parse(uuid_text) });
    let sql_uuid = ok_data(mux_sql_value_uuid(uuid));
    assert!(unsafe {
        matches!(&*sql_uuid, Value::String(text) if text == "550e8400-e29b-41d4-a716-446655440000")
    });
    let decoded_uuid = ok_data(mux_sql_value_as_uuid(sql_uuid));
    let uuid_rendered = unsafe { mux_runtime::uuid::mux_uuid_to_string(decoded_uuid) };
    assert!(
        matches!(unsafe { &*uuid_rendered }, Value::String(text) if text == "550e8400-e29b-41d4-a716-446655440000")
    );

    unsafe {
        for value in [
            datetime_text,
            datetime,
            sql_datetime,
            decoded_datetime,
            rendered,
            uuid_text,
            uuid,
            sql_uuid,
            decoded_uuid,
            uuid_rendered,
        ] {
            assert!(mux_rc_dec(value));
        }
    }
}

#[test]
fn sql_datetime_and_uuid_accessors_report_invalid_text() {
    let invalid_datetime = sval("not-a-datetime");
    let invalid_uuid = sval("not-a-uuid");
    assert_err(mux_sql_value_as_datetime(invalid_datetime));
    assert_err(mux_sql_value_as_uuid(invalid_uuid));
    unsafe {
        assert!(mux_rc_dec(invalid_datetime));
        assert!(mux_rc_dec(invalid_uuid));
    }
}

#[test]
fn query_params_and_errors() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE t (id INTEGER, name TEXT)");
    assert_ok(mux_sql_connection_execute(conn, create));
    assert!(unsafe { mux_rc_dec(create) });

    let insert = sval("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')");
    assert_ok(mux_sql_connection_execute(conn, insert));
    assert!(unsafe { mux_rc_dec(insert) });

    // parameterized query
    let sel = sval("SELECT id, name FROM t WHERE id = ?");
    let params = mux_rc_alloc(Value::List(vec![Value::Int(1)]));
    let rs = ok_data(mux_sql_connection_query_params(conn, sel, params));
    assert!(unsafe { mux_rc_dec(sel) });
    assert!(unsafe { mux_rc_dec(params) });
    let rows = resultset_rows(rs);
    assert!(!rows.is_null());
    assert!(unsafe { mux_rc_dec(rows) });
    assert!(unsafe { mux_rc_dec(rs) });

    // invalid SQL surfaces as an error
    let bad_sql = sval("THIS IS NOT SQL");
    let error_result = mux_sql_connection_execute(conn, bad_sql);
    assert!(unsafe { mux_result_is_err(error_result) });
    let error = unsafe { mux_result_data(error_result) };
    let provider = unsafe { mux_sql_error_provider(error) };
    let operation = unsafe { mux_sql_error_operation(error) };
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "sqlite") });
    assert!(unsafe { matches!(&*operation, Value::String(value) if value == "execute") });
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(error_result));
    }
    assert!(unsafe { mux_rc_dec(bad_sql) });
    let bad_q = sval("SELECT * FROM table_that_does_not_exist");
    assert_err(mux_sql_connection_query(conn, bad_q));
    assert!(unsafe { mux_rc_dec(bad_q) });

    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn placeholder_rewriting_preserves_utf8_sql_text() {
    let conn = connect_memory();

    let positional_sql = sval("SELECT 'café ☕' AS label, ? AS value");
    let positional_params = mux_rc_alloc(Value::List(vec![Value::Int(42)]));
    let positional_result = ok_data(mux_sql_connection_query_params(
        conn,
        positional_sql,
        positional_params,
    ));
    let positional_rows = resultset_rows(positional_result);
    assert!(unsafe {
        matches!(&*positional_rows, Value::List(rows) if rows.len() == 1
            && row_at_equals(&rows[0], 0, &Value::String("café ☕".into()))
            && row_at_equals(&rows[0], 1, &Value::Int(42)))
    });
    unsafe {
        assert!(mux_rc_dec(positional_rows));
        assert!(mux_rc_dec(positional_result));
        assert!(mux_rc_dec(positional_params));
        assert!(mux_rc_dec(positional_sql));
    }

    let named_sql = sval("SELECT '東京' AS label, :value");
    let mut named_values = mux_runtime::ordered::OrderedMap::new();
    named_values.insert(Value::String("value".into()), Value::Int(7));
    let named_params = mux_rc_alloc(Value::Map(named_values));
    let named_result = ok_data(mux_sql_connection_query_named(
        conn,
        named_sql,
        named_params,
    ));
    let named_rows = resultset_rows(named_result);
    assert!(unsafe {
        matches!(&*named_rows, Value::List(rows) if rows.len() == 1
            && row_at_equals(&rows[0], 0, &Value::String("東京".into()))
            && row_at_equals(&rows[0], 1, &Value::Int(7)))
    });
    unsafe {
        assert!(mux_rc_dec(named_rows));
        assert!(mux_rc_dec(named_result));
        assert!(mux_rc_dec(named_params));
        assert!(mux_rc_dec(named_sql));
        mux_sql_connection_close(conn);
        assert!(mux_rc_dec(conn));
    }
}

#[test]
fn sqlite_provider_error_exposes_canonical_vendor_code() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE codes (id INTEGER UNIQUE)");
    assert_ok(mux_sql_connection_execute(conn, create));
    unsafe {
        assert!(mux_rc_dec(create));
    }

    let insert = sval("INSERT INTO codes (id) VALUES (1)");
    assert_ok(mux_sql_connection_execute(conn, insert));
    unsafe {
        assert!(mux_rc_dec(insert));
    }

    let duplicate = sval("INSERT INTO codes (id) VALUES (1)");
    let result = mux_sql_connection_execute(conn, duplicate);
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let provider = unsafe { mux_sql_error_provider(error) };
    let code = unsafe { mux_sql_error_code(error) };
    let kind = unsafe { mux_sql_error_kind(error) };
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "sqlite") });
    assert!(unsafe { matches!(&*code, Value::String(value) if !value.is_empty()) });
    assert!(unsafe {
        matches!(&*kind, Value::Opaque(value) if value.as_ref() == 0_i32.to_ne_bytes())
    });

    unsafe {
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(code));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(duplicate));
        mux_sql_connection_close(conn);
        assert!(mux_rc_dec(conn));
    }
}

#[test]
fn sqlite_provider_code_survives_prepared_transaction_and_pool_wrappers() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE codes (id INTEGER UNIQUE)");
    assert_ok(mux_sql_connection_execute(conn, create));
    unsafe { mux_rc_dec(create) };
    let insert = sval("INSERT INTO codes (id) VALUES (1)");
    assert_ok(mux_sql_connection_execute(conn, insert));
    unsafe { mux_rc_dec(insert) };

    let duplicate = sval("INSERT INTO codes (id) VALUES (1)");
    let prepared = ok_data(mux_sql_connection_prepare(conn, duplicate));
    let empty_params = mux_rc_alloc(Value::List(vec![]));
    assert_provider_code(
        mux_sql_prepared_execute(prepared, empty_params),
        "prepared_execute",
    );
    unsafe {
        mux_rc_dec(empty_params);
        mux_sql_prepared_close(prepared);
        mux_rc_dec(prepared);
    }

    let transaction = ok_data(mux_sql_connection_begin_transaction(conn));
    assert_provider_code(
        mux_sql_transaction_execute(transaction, duplicate),
        "execute",
    );
    assert_ok(mux_sql_transaction_rollback(transaction));
    unsafe { mux_rc_dec(transaction) };
    unsafe { mux_rc_dec(duplicate) };
    mux_sql_connection_close(conn);
    unsafe { mux_rc_dec(conn) };

    let (pool_path, pool_uri) = temporary_sqlite_pool_uri();
    let pool = ok_data(mux_sql_pool_from_config(pool_uri, 1, 0));
    unsafe { mux_rc_dec(pool_uri) };
    let create = sval("CREATE TABLE codes (id INTEGER UNIQUE)");
    assert_ok(mux_sql_pool_execute(pool, create));
    unsafe { mux_rc_dec(create) };
    let insert = sval("INSERT INTO codes (id) VALUES (1)");
    assert_ok(mux_sql_pool_execute(pool, insert));
    unsafe { mux_rc_dec(insert) };
    let duplicate = sval("INSERT INTO codes (id) VALUES (1)");
    assert_provider_code(mux_sql_pool_execute(pool, duplicate), "execute");
    unsafe {
        mux_rc_dec(duplicate);
        assert_ok(mux_sql_pool_close(pool));
        mux_rc_dec(pool);
    }
    fs::remove_file(pool_path).expect("remove temporary SQLite pool database");
}

#[test]
fn named_params_are_rewritten_portably_and_ignore_literals() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE t (id INTEGER, name TEXT)");
    assert_ok(mux_sql_connection_execute(conn, create));
    assert!(unsafe { mux_rc_dec(create) });

    let insert = sval("INSERT INTO t (id, name) VALUES (:id, :name)");
    let mut values = mux_runtime::ordered::OrderedMap::new();
    values.insert(Value::String("id".into()), Value::Int(7));
    values.insert(Value::String("name".into()), Value::String("seven".into()));
    let params = mux_rc_alloc(Value::Map(values));
    assert_ok(mux_sql_connection_execute_named(conn, insert, params));
    assert!(unsafe { mux_rc_dec(insert) });
    assert!(unsafe { mux_rc_dec(params) });

    let query = sval("SELECT id, ':not_a_param' AS marker FROM t WHERE id = :id OR id = :id");
    let mut query_values = mux_runtime::ordered::OrderedMap::new();
    query_values.insert(Value::String("id".into()), Value::Int(7));
    let query_params = mux_rc_alloc(Value::Map(query_values));
    let resultset = ok_data(mux_sql_connection_query_named(conn, query, query_params));
    assert!(unsafe { mux_rc_dec(query) });
    assert!(unsafe { mux_rc_dec(query_params) });
    let rows = resultset_rows(resultset);
    assert!(unsafe {
        matches!(&*rows, Value::List(values) if values.len() == 1
            && row_at_equals(&values[0], 0, &Value::Int(7)))
    });
    assert!(unsafe { mux_rc_dec(rows) });
    assert!(unsafe { mux_rc_dec(resultset) });

    let missing = sval("SELECT :missing");
    let empty = mux_rc_alloc(Value::Map(mux_runtime::ordered::OrderedMap::new()));
    assert_err(mux_sql_connection_query_named(conn, missing, empty));
    assert!(unsafe { mux_rc_dec(missing) });
    assert!(unsafe { mux_rc_dec(empty) });

    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn named_params_reject_unused_values_before_driver_execution() {
    let conn = connect_memory();
    let query = sval("SELECT :id");
    let mut values = mux_runtime::ordered::OrderedMap::new();
    values.insert(Value::String("id".into()), Value::Int(7));
    values.insert(Value::String("typo".into()), Value::Int(8));
    let params = mux_rc_alloc(Value::Map(values));

    let result = mux_sql_connection_query_named(conn, query, params);
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let message = unsafe { mux_sql_error_message(error) };
    assert!(unsafe {
        matches!(&*message, Value::String(value) if value.contains("unused named parameter") && value.contains("typo"))
    });

    unsafe {
        assert!(mux_rc_dec(message));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(params));
        assert!(mux_rc_dec(query));
    }
    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn placeholder_rewriting_rejects_unterminated_block_comments() {
    let connection = connect_memory();

    let positional_sql = sval("SELECT ? /* unterminated");
    let positional_params = mux_rc_alloc(Value::List(vec![Value::Int(1)]));
    let positional_result =
        mux_sql_connection_query_params(connection, positional_sql, positional_params);
    assert!(unsafe { mux_result_is_err(positional_result) });
    let positional_error = unsafe { mux_result_data(positional_result) };
    let positional_message = unsafe { mux_sql_error_message(positional_error) };
    assert!(unsafe {
        matches!(&*positional_message, Value::String(message) if message.contains("unterminated block comment"))
    });

    let named_sql = sval("SELECT :value /* unterminated");
    let mut named_values = mux_runtime::ordered::OrderedMap::new();
    named_values.insert(Value::String("value".into()), Value::Int(1));
    let named_params = mux_rc_alloc(Value::Map(named_values));
    let named_result = mux_sql_connection_query_named(connection, named_sql, named_params);
    assert!(unsafe { mux_result_is_err(named_result) });
    let named_error = unsafe { mux_result_data(named_result) };
    let named_message = unsafe { mux_sql_error_message(named_error) };
    assert!(unsafe {
        matches!(&*named_message, Value::String(message) if message.contains("unterminated block comment"))
    });

    unsafe {
        assert!(mux_rc_dec(named_message));
        assert!(mux_rc_dec(named_error));
        assert!(mux_rc_dec(named_result));
        assert!(mux_rc_dec(named_params));
        assert!(mux_rc_dec(named_sql));
        assert!(mux_rc_dec(positional_message));
        assert!(mux_rc_dec(positional_error));
        assert!(mux_rc_dec(positional_result));
        assert!(mux_rc_dec(positional_params));
        assert!(mux_rc_dec(positional_sql));
        mux_sql_connection_close(connection);
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn prepared_statements_can_be_reused_and_closed() {
    let conn = connect_memory();
    let create = sval("CREATE TABLE t (id INTEGER, name TEXT)");
    assert_ok(mux_sql_connection_execute(conn, create));
    assert!(unsafe { mux_rc_dec(create) });

    let sql = sval("INSERT INTO t (id, name) VALUES (?, ?)");
    let prepared = ok_data(mux_sql_connection_prepare(conn, sql));
    assert!(unsafe { mux_rc_dec(sql) });

    for (id, name) in [(1, "one"), (2, "two")] {
        let params = mux_rc_alloc(Value::List(vec![
            Value::Int(id),
            Value::String(name.into()),
        ]));
        assert_ok(mux_sql_prepared_execute(prepared, params));
        assert!(unsafe { mux_rc_dec(params) });
    }

    let query_sql = sval("SELECT name FROM t WHERE id = :id");
    let named = ok_data(mux_sql_connection_prepare(conn, query_sql));
    assert!(unsafe { mux_rc_dec(query_sql) });
    let mut map = mux_runtime::ordered::OrderedMap::new();
    map.insert(Value::String("id".into()), Value::Int(2));
    let params = mux_rc_alloc(Value::Map(map));
    let rs = ok_data(mux_sql_prepared_query_named(named, params));
    assert!(unsafe { mux_rc_dec(params) });
    mux_sql_prepared_close(named);
    assert!(unsafe { mux_rc_dec(named) });
    let rows = resultset_rows(rs);
    assert!(unsafe {
        matches!(&*rows, Value::List(values) if values.len() == 1
            && row_at_equals(&values[0], 0, &Value::String("two".into())))
    });
    assert!(unsafe { mux_rc_dec(rows) });
    assert!(unsafe { mux_rc_dec(rs) });

    mux_sql_prepared_close(prepared);
    assert!(unsafe { mux_rc_dec(prepared) });
    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn sqlite_rejects_multiple_statements_explicitly() {
    let conn = connect_memory();

    // rusqlite 0.40 rejects a second statement instead of silently executing
    // only the first one. Keep that safer behavior explicit at the runtime
    // boundary so callers do not depend on version-specific SQLite semantics.
    let execute = sval("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1)");
    assert_err(mux_sql_connection_execute(conn, execute));
    assert!(unsafe { mux_rc_dec(execute) });

    let query = sval("SELECT 1; SELECT 2");
    assert_err(mux_sql_connection_query(conn, query));
    assert!(unsafe { mux_rc_dec(query) });

    mux_sql_connection_close(conn);
    assert!(unsafe { mux_rc_dec(conn) });
}

#[test]
fn execute_batch_runs_bounded_quote_aware_scripts() {
    let connection = connect_memory();
    let script =
        sval("CREATE TABLE items (value TEXT); INSERT INTO items VALUES ('a;b'); -- ; ignored\n");
    let batch = mux_sql_connection_execute_batch(connection, script);
    assert_ok(batch);
    unsafe {
        assert!(mux_rc_dec(script));
    }
    let query = sval("SELECT value FROM items");
    let resultset = ok_data(mux_sql_connection_query(connection, query));
    let rows = resultset_rows(resultset);
    assert!(unsafe {
        matches!(&*rows, Value::List(values) if values.len() == 1
            && row_at_equals(&values[0], 0, &Value::String("a;b".to_string())))
    });
    unsafe {
        assert!(mux_rc_dec(rows));
        assert!(mux_rc_dec(resultset));
        assert!(mux_rc_dec(query));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn execute_batch_preserves_provider_diagnostics() {
    let connection = connect_memory();
    let setup = sval("CREATE TABLE items (id INTEGER UNIQUE)");
    assert_ok(mux_sql_connection_execute(connection, setup));
    unsafe {
        assert!(mux_rc_dec(setup));
    }

    let script = sval("INSERT INTO items VALUES (1); INSERT INTO items VALUES (1)");
    let result = mux_sql_connection_execute_batch(connection, script);
    assert_provider_code(result, "execute_batch");
    unsafe {
        assert!(mux_rc_dec(script));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn execute_many_reuses_one_statement_and_validates_rows_before_execution() {
    let connection = connect_memory();
    let create = sval("CREATE TABLE items (id INTEGER, value TEXT)");
    assert_ok(mux_sql_connection_execute(connection, create));
    unsafe {
        assert!(mux_rc_dec(create));
    }

    let sql = sval("INSERT INTO items (id, value) VALUES (?, ?)");
    let rows = mux_rc_alloc(Value::List(vec![
        Value::List(vec![Value::Int(1), Value::String("one".to_string())]),
        Value::List(vec![Value::Int(2), Value::String("two".to_string())]),
    ]));
    assert_eq!(
        ok_int(mux_sql_connection_execute_many(connection, sql, rows)),
        2
    );
    unsafe {
        assert!(mux_rc_dec(rows));
        assert!(mux_rc_dec(sql));
    }

    let mismatch_sql = sval("INSERT INTO items (id, value) VALUES (?, ?)");
    let mismatch_rows = mux_rc_alloc(Value::List(vec![
        Value::List(vec![Value::Int(3), Value::String("three".to_string())]),
        Value::List(vec![Value::Int(4)]),
    ]));
    assert_err(mux_sql_connection_execute_many(
        connection,
        mismatch_sql,
        mismatch_rows,
    ));
    unsafe {
        assert!(mux_rc_dec(mismatch_rows));
        assert!(mux_rc_dec(mismatch_sql));
    }

    let empty_sql = sval("INSERT INTO items (id, value) VALUES (?, ?)");
    let empty_rows = mux_rc_alloc(Value::List(Vec::new()));
    assert_err(mux_sql_connection_execute_many(
        connection, empty_sql, empty_rows,
    ));
    unsafe {
        assert!(mux_rc_dec(empty_rows));
        assert!(mux_rc_dec(empty_sql));
    }

    let multi_sql = sval("INSERT INTO items (id, value) VALUES (?, ?); DELETE FROM items");
    let multi_rows = mux_rc_alloc(Value::List(vec![Value::List(vec![
        Value::Int(9),
        Value::String("nine".to_string()),
    ])]));
    assert_err(mux_sql_connection_execute_many(
        connection, multi_sql, multi_rows,
    ));
    unsafe {
        assert!(mux_rc_dec(multi_rows));
        assert!(mux_rc_dec(multi_sql));
    }

    let query = sval("SELECT id, value FROM items ORDER BY id");
    let resultset = ok_data(mux_sql_connection_query(connection, query));
    let rows = resultset_rows(resultset);
    assert!(unsafe { matches!(&*rows, Value::List(values) if values.len() == 2) });
    unsafe {
        assert!(mux_rc_dec(rows));
        assert!(mux_rc_dec(resultset));
        assert!(mux_rc_dec(query));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn execute_many_accepts_postgres_style_positional_placeholders() {
    let connection = connect_memory();
    let create = sval("CREATE TABLE items (id INTEGER, value TEXT)");
    assert_ok(mux_sql_connection_execute(connection, create));
    unsafe {
        assert!(mux_rc_dec(create));
    }

    // `$n` is accepted as portable input and normalized for the selected
    // provider before the prepared statement is executed.
    let sql = sval("INSERT INTO items (id, value) VALUES ($1, $2)");
    let rows = mux_rc_alloc(Value::List(vec![
        Value::List(vec![Value::Int(1), Value::String("one".to_string())]),
        Value::List(vec![Value::Int(2), Value::String("two".to_string())]),
    ]));
    assert_eq!(
        ok_int(mux_sql_connection_execute_many(connection, sql, rows)),
        2
    );

    unsafe {
        assert!(mux_rc_dec(rows));
        assert!(mux_rc_dec(sql));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn migrations_apply_validate_status_and_roll_back_transactionally() {
    let connection = connect_memory();
    let name_one = sval("create_items");
    let up_one = sval("CREATE TABLE items (id INTEGER)");
    let down_one = sval("DROP TABLE items");
    let migration_one = ok_data(mux_sql_migration_from_config(1, name_one, up_one, down_one));
    unsafe {
        assert!(mux_rc_dec(name_one));
        assert!(mux_rc_dec(up_one));
        assert!(mux_rc_dec(down_one));
    }
    let name_two = sval("seed_items");
    let up_two = sval("INSERT INTO items (id) VALUES (1)");
    let down_two = sval("DELETE FROM items WHERE id = 1");
    let migration_two = ok_data(mux_sql_migration_from_config(2, name_two, up_two, down_two));
    unsafe {
        assert!(mux_rc_dec(name_two));
        assert!(mux_rc_dec(up_two));
        assert!(mux_rc_dec(down_two));
    }
    let migrations = mux_rc_alloc(Value::List(vec![
        unsafe { (*migration_one).clone() },
        unsafe { (*migration_two).clone() },
    ]));
    let migrator = ok_data(mux_sql_migrator_from_migrations(connection, migrations));
    unsafe {
        assert!(mux_rc_dec(migrations));
        assert!(mux_rc_dec(migration_one));
        assert!(mux_rc_dec(migration_two));
    }

    let initial_status = ok_data(mux_sql_migrator_status(migrator));
    assert!(unsafe { matches!(&*initial_status, Value::List(values) if values.len() == 2) });
    unsafe {
        assert!(mux_rc_dec(initial_status));
    }
    let dry_run = ok_data(mux_sql_migrator_dry_run(migrator));
    assert!(unsafe { matches!(&*dry_run, Value::List(values) if values.len() == 2) });
    unsafe {
        assert!(mux_rc_dec(dry_run));
    }
    let metadata_check = sval(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'mux_schema_migrations'",
    );
    let metadata_result = ok_data(mux_sql_connection_query(connection, metadata_check));
    let metadata_rows = resultset_rows(metadata_result);
    assert!(unsafe { matches!(&*metadata_rows, Value::List(values) if values.is_empty()) });
    unsafe {
        assert!(mux_rc_dec(metadata_rows));
        assert!(mux_rc_dec(metadata_result));
        assert!(mux_rc_dec(metadata_check));
    }

    assert_eq!(ok_int(mux_sql_migrator_up(migrator)), 2);
    assert_ok(mux_sql_migrator_validate(migrator));
    let query = sval("SELECT COUNT(*) FROM items");
    let resultset = ok_data(mux_sql_connection_query(connection, query));
    let rows = resultset_rows(resultset);
    assert!(unsafe {
        matches!(&*rows, Value::List(values) if values.len() == 1
            && row_at_equals(&values[0], 0, &Value::Int(1)))
    });
    unsafe {
        assert!(mux_rc_dec(rows));
        assert!(mux_rc_dec(resultset));
        assert!(mux_rc_dec(query));
    }

    assert_eq!(ok_int(mux_sql_migrator_down(migrator)), 1);
    assert_eq!(ok_int(mux_sql_migrator_down_to(migrator, 0)), 1);
    assert_ok(mux_sql_migrator_validate(migrator));
    let missing = sval("SELECT COUNT(*) FROM items");
    assert_err(mux_sql_connection_query(connection, missing));
    unsafe {
        assert!(mux_rc_dec(missing));
        assert!(mux_rc_dec(migrator));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn migration_failures_preserve_provider_and_operation_context() {
    let connection = connect_memory();
    let name = sval("create_items");
    let up = sval("CREATE TABLE migration_context (id INTEGER)");
    let down = sval("DROP TABLE migration_context");
    let migration = ok_data(mux_sql_migration_from_config(1, name, up, down));
    unsafe {
        assert!(mux_rc_dec(name));
        assert!(mux_rc_dec(up));
        assert!(mux_rc_dec(down));
    }
    let migrations = mux_rc_alloc(Value::List(vec![unsafe { (*migration).clone() }]));
    let migrator = ok_data(mux_sql_migrator_from_migrations(connection, migrations));
    unsafe {
        assert!(mux_rc_dec(migrations));
        assert!(mux_rc_dec(migration));
    }

    let error = mux_sql_migrator_up_to(migrator, -1);
    assert!(unsafe { mux_result_is_err(error) });
    let error_value = unsafe { mux_result_data(error) };
    let kind = unsafe { mux_sql_error_kind(error_value) };
    let provider = unsafe { mux_sql_error_provider(error_value) };
    let operation = unsafe { mux_sql_error_operation(error_value) };
    assert!(unsafe {
        matches!(&*kind, Value::Opaque(value) if value.as_ref() == 3_i32.to_ne_bytes())
    });
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "sqlite") });
    assert!(unsafe { matches!(&*operation, Value::String(value) if value == "up_to") });
    unsafe {
        for value in [
            kind,
            provider,
            operation,
            error_value,
            error,
            migrator,
            connection,
        ] {
            assert!(mux_rc_dec(value));
        }
    }
}

#[test]
fn migration_provider_failures_preserve_native_diagnostics() {
    let connection = connect_memory();
    let name = sval("broken_sql");
    let up = sval("INSERT INTO missing_migration_table VALUES (1)");
    let down = sval("DELETE FROM missing_migration_table");
    let migration = ok_data(mux_sql_migration_from_config(1, name, up, down));
    unsafe {
        assert!(mux_rc_dec(name));
        assert!(mux_rc_dec(up));
        assert!(mux_rc_dec(down));
    }
    let migrations = mux_rc_alloc(Value::List(vec![unsafe { (*migration).clone() }]));
    let migrator = ok_data(mux_sql_migrator_from_migrations(connection, migrations));
    unsafe {
        assert!(mux_rc_dec(migrations));
        assert!(mux_rc_dec(migration));
    }

    let result = mux_sql_migrator_up(migrator);
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let provider = unsafe { mux_sql_error_provider(error) };
    let code = unsafe { mux_sql_error_code(error) };
    let operation = unsafe { mux_sql_error_operation(error) };
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "sqlite") });
    assert!(unsafe { matches!(&*code, Value::String(value) if !value.is_empty()) });
    assert!(unsafe { matches!(&*operation, Value::String(value) if value == "up") });
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(code));
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
        assert!(mux_rc_dec(migrator));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn migrations_load_from_directory_with_paired_files() {
    let directory = std::env::temp_dir().join(format!(
        "mux-migrations-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos()
    ));
    fs::create_dir(&directory).expect("create migration directory");
    fs::write(
        directory.join("V001__create_items.up.sql"),
        "CREATE TABLE items (id INTEGER)",
    )
    .expect("write up migration");
    fs::write(
        directory.join("V001__create_items.down.sql"),
        "DROP TABLE items",
    )
    .expect("write down migration");

    let connection = connect_memory();
    let path = sval(directory.to_str().expect("temporary path is utf-8"));
    let migrator = ok_data(mux_sql_migrator_from_directory(connection, path));
    unsafe {
        assert!(mux_rc_dec(path));
    }
    assert_eq!(ok_int(mux_sql_migrator_up(migrator)), 1);
    assert_eq!(ok_int(mux_sql_migrator_down(migrator)), 1);
    unsafe {
        assert!(mux_rc_dec(migrator));
        assert!(mux_rc_dec(connection));
    }
    fs::remove_dir_all(directory).expect("remove migration directory");
}

#[test]
fn sql_value_bytes_and_type_errors() {
    // bytes value round trip
    let input = mux_rc_alloc(Value::Bytes(vec![1, 2, 255]));
    let bytes = mux_sql_value_bytes(input);
    assert!(!bytes.is_null());
    assert!(unsafe { matches!(&*bytes, Value::Bytes(value) if value == &[1, 2, 255]) });
    assert_ok(mux_sql_value_as_bytes(bytes));
    assert!(unsafe { mux_rc_dec(bytes) });
    assert!(unsafe { mux_rc_dec(input) });

    // accessor type mismatch is an error
    let s = unsafe { mux_sql_value_string(CString::new("nope").unwrap().as_ptr()) };
    assert_err(mux_sql_value_as_int(s));
    assert!(unsafe { mux_rc_dec(s) });
}

#[test]
fn sql_float_to_int_rejects_out_of_range_values() {
    let too_large = mux_rc_alloc(Value::Float(ordered_float::OrderedFloat(
        9.223_372_036_854_776e18,
    )));
    let too_small = mux_rc_alloc(Value::Float(ordered_float::OrderedFloat(
        -9.223_372_036_854_778e18,
    )));

    assert_err(mux_sql_value_as_int(too_large));
    assert_err(mux_sql_value_as_int(too_small));

    unsafe {
        assert!(mux_rc_dec(too_large));
        assert!(mux_rc_dec(too_small));
    }
}

#[test]
fn sqlite_text_with_invalid_utf8_is_preserved_as_bytes() {
    let connection = connect_memory();
    let query = sval("SELECT CAST(x'80FF' AS TEXT)");
    let resultset = ok_data(mux_sql_connection_query(connection, query));
    let next = resultset_next(resultset);
    let row = unsafe { mux_runtime::optional::mux_optional_data(next) };
    assert!(!row.is_null());
    let value = ok_data(mux_sql_row_at(row, 0));
    assert!(unsafe { matches!(&*value, Value::Bytes(bytes) if bytes == &[0x80, 0xff]) });
    unsafe {
        assert!(mux_rc_dec(value));
        assert!(mux_rc_dec(row));
        assert!(mux_rc_dec(next));
        assert!(mux_rc_dec(resultset));
        assert!(mux_rc_dec(query));
        assert!(mux_rc_dec(connection));
    }
}

#[test]
fn sql_handles_reject_values_of_other_runtime_types() {
    // A scalar is not a connection handle. This check must happen before the
    // SQL registry lookup so an unrelated object can never alias a connection
    // id by accident.
    let not_connection = mux_sql_value_int(1);
    let statement = sval("SELECT 1");
    assert_err(mux_sql_connection_execute(not_connection, statement));
    assert!(unsafe { mux_rc_dec(statement) });
    assert!(unsafe { mux_rc_dec(not_connection) });
}

#[test]
fn bad_uri_is_error() {
    let uri = CString::new("notarealscheme://x").unwrap();
    assert_err(unsafe { mux_sql_connect(uri.as_ptr()) });
}

#[test]
fn malformed_mysql_uri_is_invalid_before_driver_connect() {
    // The MySQL driver validates the URL before opening a socket. Preserve
    // that distinction as Invalid instead of misclassifying configuration as
    // a provider/database failure.
    let uri = CString::new("mysql://").unwrap();
    let result = unsafe { mux_sql_connect(uri.as_ptr()) };
    assert!(unsafe { mux_result_is_err(result) });
    let error = unsafe { mux_result_data(result) };
    let kind = unsafe { mux_sql_error_kind(error) };
    let provider = unsafe { mux_sql_error_provider(error) };
    let operation = unsafe { mux_sql_error_operation(error) };
    assert!(unsafe {
        matches!(&*kind, Value::Opaque(value) if value.as_ref() == 3_i32.to_ne_bytes())
    });
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "mysql") });
    assert!(unsafe { matches!(&*operation, Value::String(value) if value == "connect") });
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(kind));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(result));
    }
}

#[test]
fn setup_errors_preserve_provider_and_operation_context() {
    let malformed = CString::new("sqlserver://host").unwrap();
    let malformed_result = unsafe { mux_sql_connect(malformed.as_ptr()) };
    assert!(unsafe { mux_result_is_err(malformed_result) });
    let malformed_error = unsafe { mux_result_data(malformed_result) };
    let malformed_kind = unsafe { mux_sql_error_kind(malformed_error) };
    assert!(unsafe {
        matches!(&*malformed_kind, Value::Opaque(value) if value.as_ref() == 3_i32.to_ne_bytes())
    });
    unsafe {
        assert!(mux_rc_dec(malformed_kind));
        assert!(mux_rc_dec(malformed_error));
        assert!(mux_rc_dec(malformed_result));
    }

    let uri = CString::new("sqlserver://localhost/example").unwrap();
    let connect = unsafe { mux_sql_connect(uri.as_ptr()) };
    assert!(unsafe { mux_result_is_err(connect) });
    let connect_error = unsafe { mux_result_data(connect) };
    let provider = unsafe { mux_sql_error_provider(connect_error) };
    let operation = unsafe { mux_sql_error_operation(connect_error) };
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "sqlserver") });
    assert!(unsafe { matches!(&*operation, Value::String(value) if value == "connect") });
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(connect_error));
        assert!(mux_rc_dec(connect));
    }

    let pool_uri = sval("sqlite:invalid");
    let pool = mux_sql_pool_from_config(pool_uri, 2, 0);
    assert!(unsafe { mux_result_is_err(pool) });
    let pool_error = unsafe { mux_result_data(pool) };
    let provider = unsafe { mux_sql_error_provider(pool_error) };
    let operation = unsafe { mux_sql_error_operation(pool_error) };
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "sqlite") });
    assert!(unsafe { matches!(&*operation, Value::String(value) if value == "pool_from_config") });
    unsafe {
        assert!(mux_rc_dec(operation));
        assert!(mux_rc_dec(provider));
        assert!(mux_rc_dec(pool_error));
        assert!(mux_rc_dec(pool));
        assert!(mux_rc_dec(pool_uri));
    }
}

#[test]
fn legacy_sqlite_memory_uris_are_rejected() {
    for legacy_uri in ["sqlite::memory:", "sqlite://:memory:"] {
        let direct_uri = CString::new(legacy_uri).expect("legacy URI has no NUL byte");
        assert_err(unsafe { mux_sql_connect(direct_uri.as_ptr()) });

        let pool_uri = sval(legacy_uri);
        assert_err(mux_sql_pool_from_config(pool_uri, 1, 0));
        assert!(unsafe { mux_rc_dec(pool_uri) });
    }
}

#[test]
fn null_value_accessors_error() {
    assert_err(mux_sql_value_as_int(std::ptr::null()));
    assert!(mux_sql_value_is_null(std::ptr::null()));
}

#[test]
fn structured_sql_error_preserves_category_context_and_display() {
    let detail = sval("UNIQUE constraint failed: users.id");
    let error = unsafe { mux_sql_error_from_message(detail) };
    assert!(unsafe { matches!(&*error, Value::Object(_)) });

    let kind = unsafe { mux_sql_error_kind(error) };
    let constraint = unsafe { mux_sql_error_constraint(error) };
    let provider = unsafe { mux_sql_error_provider(error) };
    let message = unsafe { mux_sql_error_message(error) };
    let display = unsafe { mux_sql_error_to_string(error) };
    assert!(unsafe {
        matches!(&*kind, Value::Opaque(value) if value.as_ref() == 4_i32.to_ne_bytes())
    });
    assert!(unsafe { matches!(&*constraint, Value::String(value) if value.is_empty()) });
    assert!(unsafe { matches!(&*provider, Value::String(value) if value == "unknown") });
    assert!(unsafe {
        matches!(&*message, Value::String(value) if value == "UNIQUE constraint failed: users.id")
    });
    assert!(unsafe {
        matches!(&*display, Value::String(value) if value == "database: UNIQUE constraint failed: users.id")
    });

    for value in [detail, error, kind, constraint, provider, message, display] {
        assert!(unsafe { mux_rc_dec(value) });
    }
}
