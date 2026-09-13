use crate::json::{json_to_value, value_to_json, Json};
use crate::object::{
    alloc_object, get_object_ptr, get_object_type_id, register_object_type_with_copy,
    register_shared_object_type,
};
use crate::refcount::{mux_rc_alloc, mux_rc_dec, mux_rc_inc};
use crate::sync_primitives::{cancellation_entry, cancellation_entry_is_cancelled};
use crate::{TypeId, Value};
use futures_util::TryStreamExt;
use mysql::prelude::Queryable;
use mysql::{
    consts::ColumnFlags, Conn as MySqlDriverConnection, Opts as MySqlOpts, Params as MySqlParams,
    Value as MySqlValue,
};
use postgres::types::{ToSql, Type as PgType};
use postgres::{Client as PostgresClient, NoTls};
use rusqlite::types::{Value as SqliteValue, ValueRef};
use rusqlite::{params_from_iter, Connection as SqliteConnection};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tiberius::{
    AuthMethod as TdsAuthMethod, Client as TdsClient, ColumnData as TdsColumnData,
    Config as TdsConfig, EncryptionLevel as TdsEncryptionLevel, QueryItem as TdsQueryItem,
    QueryStream as TdsQueryStream, Row as TdsRow, ToSql as TdsToSql,
};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use url::Url;

// THREAD AFFINITY INVARIANT:
// Handle IDs (Connection, Transaction, ResultSet) are globally unique via NEXT_HANDLE,
// but the backing stores are thread_local. A handle is only valid on the thread that
// created it. If a handle is passed to another thread (via sync blocks, closures, or
// spawned tasks), operations will silently fail with "invalid sql ... handle".
// This is a fundamental constraint of the current design and not enforced by the type
// system. Users must ensure handles remain on their creating thread.

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

thread_local! {
    static SQL_CONNECTIONS: RefCell<HashMap<i64, SqlConnection>> = RefCell::new(HashMap::new());
    static SQL_TRANSACTIONS: RefCell<HashMap<i64, SqlTransaction>> = RefCell::new(HashMap::new());
    static SQL_RESULTSETS: RefCell<HashMap<i64, SqlResultSet>> = RefCell::new(HashMap::new());
    static SQL_CONNECTION_LEASES: RefCell<HashMap<i64, usize>> = RefCell::new(HashMap::new());
    static SQL_TRANSACTION_LEASES: RefCell<HashMap<i64, usize>> = RefCell::new(HashMap::new());
    static SQL_CONNECTION_CLOSE_PENDING: RefCell<HashSet<i64>> = RefCell::new(HashSet::new());
    static SQL_TRANSACTION_CLOSE_PENDING: RefCell<HashSet<i64>> = RefCell::new(HashSet::new());
    static SQL_ROWS: RefCell<HashMap<i64, SqlRow>> = RefCell::new(HashMap::new());
    static SQL_PREPARED: RefCell<HashMap<i64, SqlPrepared>> = RefCell::new(HashMap::new());
    static SQL_MIGRATIONS: RefCell<HashMap<i64, MigrationEntry>> = RefCell::new(HashMap::new());
    static SQL_MIGRATORS: RefCell<HashMap<i64, MigratorEntry>> = RefCell::new(HashMap::new());
}

struct PoolInner {
    uri: String,
    max_connections: usize,
    acquire_timeout: Option<std::time::Duration>,
    idle: Vec<SqlConnection>,
    total: usize,
    in_use: usize,
    waiters: usize,
    closed: bool,
}

struct PoolState {
    inner: Mutex<PoolInner>,
    wake: Condvar,
}

/// A checked-out pool connection stays in this lease until its result set is
/// closed or reaches EOF. This keeps provider cursors and their packet/session
/// state exclusive to one result set instead of allowing the pool to reuse the
/// connection underneath an active iterator.
struct PoolLease {
    state: Arc<PoolState>,
    connection: Option<SqlConnection>,
    discard: bool,
}

impl PoolLease {
    fn connection_mut(&mut self) -> Result<&mut SqlConnection, SqlFailure> {
        self.connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("sql pool connection lease is closed"))
    }

    fn discard(&mut self) {
        self.discard = true;
    }

    fn release(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        let mut guard = self
            .state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.in_use = guard.in_use.saturating_sub(1);
        if guard.closed || self.discard || !pool_connection_is_reusable(&connection) {
            guard.total = guard.total.saturating_sub(1);
        } else {
            guard.idle.push(connection);
        }
        self.state.wake.notify_one();
    }
}

fn pool_connection_is_reusable(connection: &SqlConnection) -> bool {
    match connection {
        SqlConnection::SqlServer(connection) => !connection.poisoned,
        SqlConnection::Sqlite(_) | SqlConnection::Postgres(_) | SqlConnection::MySql(_) => true,
    }
}

fn close_pool_state(state: &PoolState) {
    let mut inner = state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    inner.closed = true;
    inner.idle.clear();
    // Active leases own the remaining connections. They decrement `total`
    // when they release, which lets close return without waiting on the same
    // synchronous thread that may still own a result set.
    inner.total = inner.in_use;
    state.wake.notify_all();
}

impl Drop for PoolLease {
    fn drop(&mut self) {
        self.release();
    }
}

struct PoolEntry {
    state: Arc<PoolState>,
    names: usize,
}

static SQL_POOLS: LazyLock<Mutex<HashMap<i64, PoolEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SQL_ERRORS: LazyLock<Mutex<HashMap<i64, SqlErrorEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static SQL_CONNECTION_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "Connection",
        std::mem::size_of::<i64>(),
        Some(drop_connection_handle as extern "C" fn(*mut c_void)),
    )
});
static SQL_TRANSACTION_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "Transaction",
        std::mem::size_of::<i64>(),
        Some(drop_transaction_handle as extern "C" fn(*mut c_void)),
    )
});
static SQL_RESULTSET_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "ResultSet",
        std::mem::size_of::<i64>(),
        Some(drop_resultset_handle as extern "C" fn(*mut c_void)),
    )
});
static SQL_ROW_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "Row",
        std::mem::size_of::<i64>(),
        Some(drop_row_handle as extern "C" fn(*mut c_void)),
    )
});
static SQL_PREPARED_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "PreparedStatement",
        std::mem::size_of::<i64>(),
        Some(drop_prepared_handle as extern "C" fn(*mut c_void)),
    )
});
static SQL_POOL_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Pool",
        std::mem::size_of::<i64>(),
        Some(drop_pool_handle),
        Some(copy_pool_handle),
    )
});
static SQL_MIGRATION_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Migration",
        std::mem::size_of::<i64>(),
        Some(drop_migration_handle),
        Some(copy_migration_handle),
    )
});
static SQL_MIGRATOR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_object_type_with_copy(
        "Migrator",
        std::mem::size_of::<i64>(),
        Some(drop_migrator_handle),
        Some(copy_migrator_handle),
    )
});
static SQL_ERROR_TYPE_ID: LazyLock<TypeId> = LazyLock::new(|| {
    register_shared_object_type(
        "SqlError",
        std::mem::size_of::<i64>(),
        Some(drop_sql_error as extern "C" fn(*mut c_void)),
    )
});

enum SqlConnection {
    Sqlite(SqliteConnection),
    // Keep the client at a stable address while a leased RowIter borrows its
    // protocol connection. The result-set lease prevents this connection
    // entry from being used or dropped until the iterator is finalized.
    Postgres(Box<PostgresClient>),
    MySql(Box<MySqlConnection>),
    SqlServer(Box<SqlServerConnection>),
}

/// A MySQL connection retains the options needed to open the separate control
/// connection used by `KILL QUERY`. The driver does not expose a same-session
/// cancellation primitive, but MySQL does expose a server-side connection ID
/// and accepts a kill request from another authenticated session.
struct MySqlConnection {
    driver: Box<MySqlDriverConnection>,
    opts: MySqlOpts,
}

impl MySqlConnection {
    fn new(opts: MySqlOpts) -> Result<Self, mysql::Error> {
        let driver = MySqlDriverConnection::new(opts.clone())?;
        Ok(Self {
            driver: Box::new(driver),
            opts,
        })
    }

    fn connection_id(&self) -> u32 {
        self.driver.connection_id()
    }
}

impl std::ops::Deref for MySqlConnection {
    type Target = MySqlDriverConnection;

    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}

impl std::ops::DerefMut for MySqlConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.driver
    }
}

type SqlServerClient = TdsClient<Compat<tokio::net::TcpStream>>;

struct SqlServerConnection {
    runtime: tokio::runtime::Runtime,
    client: Box<SqlServerClient>,
    poisoned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SqlServerUri {
    host: String,
    port: u16,
    database: String,
    username: Option<String>,
    password: Option<String>,
    encrypt_tls: bool,
    verify_tls: bool,
}

struct SqlTransaction {
    connection_handle: i64,
    provider: &'static str,
    connection: Option<SqlConnection>,
    active: bool,
    parent: Option<(i64, String)>,
    owner: *mut Value,
}

struct SqlResultSet {
    ordered_rows: Vec<Value>,
    columns: Vec<String>,
    next_ordered_index: usize,
    closed: bool,
    sqlite_cursor: Option<SqliteCursor>,
    postgres_cursor: Option<PostgresCursor>,
    mysql_cursor: Option<MySqlCursor>,
    sqlserver_cursor: Option<SqlServerCursor>,
    connection_lease: Option<i64>,
    transaction_lease: Option<i64>,
    pool_lease: Option<PoolLease>,
}

/// An owned SQLite statement cursor. The connection remains in its handle
/// store while the cursor is leased; the result-set lease prevents every
/// other operation from borrowing that connection until this statement is
/// finalized at EOF, close, or drop.
struct SqliteCursor {
    database: *mut rusqlite::ffi::sqlite3,
    statement: *mut rusqlite::ffi::sqlite3_stmt,
}

/// An owned PostgreSQL wire cursor. `postgres::RowIter` carries a borrow of
/// the client's internal connection, so the client is boxed in
/// `SqlConnection` and this lifetime is extended only after that address is
/// stable. Connection/result-set leases keep the pointer valid until the
/// iterator is dropped.
struct PostgresCursor {
    client: *mut PostgresClient,
    iter: postgres::RowIter<'static>,
    types: Vec<PgType>,
}

/// An owned MySQL result iterator. The MySQL driver ties `QueryResult` to the
/// mutable connection that owns its packet stream, so the connection is kept
/// in a stable Box and the result-set lease forbids every competing operation
/// until this iterator is dropped or exhausted.
struct MySqlCursor {
    connection: *mut MySqlConnection,
    result: mysql::QueryResult<'static, 'static, 'static, mysql::Binary>,
    binary_columns: Vec<bool>,
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum MySqlInterruptReason {
    Timeout = 1,
    Cancellation = 2,
}

/// Controls a running MySQL statement through the server's `KILL QUERY`
/// command. The synchronous MySQL driver has no same-session interrupt API, so
/// the controller owns a second authenticated connection and targets the
/// query connection by its server-assigned connection ID.
struct MySqlInterruptController {
    stop: Arc<std::sync::atomic::AtomicBool>,
    reason: Arc<std::sync::atomic::AtomicU8>,
    kill_error: Arc<Mutex<Option<SqlFailure>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl MySqlInterruptController {
    fn start(
        opts: &MySqlOpts,
        connection_id: u32,
        started: Instant,
        timeout: Option<Duration>,
        token: Option<Arc<crate::sync_primitives::CancellationEntry>>,
    ) -> Result<Self, SqlFailure> {
        let control = MySqlDriverConnection::new(opts.clone())
            .map_err(|error| SqlFailure::mysql("mysql interrupt connection failed", error))?;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reason = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let kill_error = Arc::new(Mutex::new(None));
        let stop_worker = Arc::clone(&stop);
        let reason_worker = Arc::clone(&reason);
        let kill_error_worker = Arc::clone(&kill_error);
        let worker = thread::Builder::new()
            .name("mux-mysql-interrupt".to_string())
            .spawn(move || {
                let mut control = control;
                loop {
                    if stop_worker.load(Ordering::Acquire) {
                        return;
                    }
                    if token.as_ref().is_some_and(cancellation_entry_is_cancelled) {
                        reason_worker
                            .store(MySqlInterruptReason::Cancellation as u8, Ordering::Release);
                        break;
                    }
                    if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                        reason_worker.store(MySqlInterruptReason::Timeout as u8, Ordering::Release);
                        break;
                    }
                    let remaining = timeout.map_or_else(
                        || Duration::from_millis(5),
                        |limit| limit.saturating_sub(started.elapsed()),
                    );
                    thread::sleep(remaining.min(Duration::from_millis(5)));
                }

                if let Err(error) = control.query_drop(format!("KILL QUERY {connection_id}")) {
                    let mut failure = kill_error_worker
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *failure = Some(SqlFailure::mysql("mysql query interrupt failed", error));
                }
            })
            .map_err(|error| {
                SqlFailure::plain(format!(
                    "could not start MySQL query interrupt worker: {error}"
                ))
            })?;

        Ok(Self {
            stop,
            reason,
            kill_error,
            worker: Some(worker),
        })
    }

    fn kill_failure(&self) -> Option<SqlFailure> {
        self.kill_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn reason(&self) -> Option<MySqlInterruptReason> {
        match self.reason.load(Ordering::Acquire) {
            1 => Some(MySqlInterruptReason::Timeout),
            2 => Some(MySqlInterruptReason::Cancellation),
            _ => None,
        }
    }

    fn finish(mut self) -> (Option<MySqlInterruptReason>, Option<SqlFailure>) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        (self.reason(), self.kill_failure())
    }
}

impl Drop for MySqlInterruptController {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct SqlServerCursor {
    connection: *mut SqlServerConnection,
    client: *mut SqlServerClient,
    runtime: *const tokio::runtime::Runtime,
    stream: TdsQueryStream<'static>,
    deadline: Option<Instant>,
    cancellation: Option<Arc<crate::sync_primitives::CancellationEntry>>,
    exhausted: bool,
}

impl Drop for SqliteCursor {
    fn drop(&mut self) {
        if !self.statement.is_null() {
            unsafe {
                let _ = rusqlite::ffi::sqlite3_finalize(self.statement);
            }
            self.statement = std::ptr::null_mut();
        }
    }
}

impl Drop for SqlServerCursor {
    fn drop(&mut self) {
        if !self.exhausted && !self.connection.is_null() {
            unsafe {
                (*self.connection).poisoned = true;
            }
        }
    }
}

struct SqlRow {
    provider: &'static str,
    columns: Vec<String>,
    values: Vec<Value>,
}

struct SqlPrepared {
    connection_handle: i64,
    sql: String,
}

#[derive(Clone)]
struct MigrationDefinition {
    version: i64,
    name: String,
    up: String,
    down: String,
    checksum: String,
}

struct MigrationEntry {
    definition: MigrationDefinition,
    names: usize,
}

struct MigratorEntry {
    connection_handle: i64,
    table: String,
    migrations: Vec<MigrationDefinition>,
    names: usize,
}

#[derive(Clone)]
struct SqlErrorEntry {
    kind: SqlErrorKind,
    detail: String,
    provider: String,
    code: String,
    constraint: String,
    operation: String,
}

/// Provider-native diagnostics carried alongside an operation failure.
///
/// The public `code` field uses the provider's canonical code. The additional
/// representations stay separate internally so adapters never discard SQLSTATE
/// or a numeric vendor code while crossing the common SQL error path.
#[derive(Clone, Debug, Default)]
struct ProviderDiagnostic {
    code: String,
    sqlstate: String,
    vendor_code: String,
    constraint: String,
}

impl ProviderDiagnostic {
    fn sqlite(error: &rusqlite::Error) -> Self {
        let vendor_code = error
            .sqlite_extended_error_code()
            .map_or_else(String::new, |code| code.to_string());
        Self {
            code: vendor_code.clone(),
            vendor_code,
            ..Self::default()
        }
    }

    fn postgres(error: &postgres::Error) -> Self {
        let sqlstate = error
            .code()
            .map_or_else(String::new, |code| code.code().to_string());
        let constraint = error
            .as_db_error()
            .and_then(|db_error| db_error.constraint())
            .unwrap_or_default()
            .to_string();
        Self {
            code: sqlstate.clone(),
            sqlstate,
            constraint,
            ..Self::default()
        }
    }

    fn mysql(error: &mysql::Error) -> Self {
        match error {
            mysql::Error::MySqlError(error) => Self {
                code: error.state.clone(),
                sqlstate: error.state.clone(),
                vendor_code: error.code.to_string(),
                ..Self::default()
            },
            _ => Self::default(),
        }
    }

    /// Map provider-native integrity diagnostics to the portable SQL error
    /// category.  Keep this at the diagnostic boundary so wrappers (prepared
    /// statements, transactions, pools, and migrations) do not need to
    /// inspect provider-specific display text.
    fn kind(&self) -> Option<SqlErrorKind> {
        // SQLSTATE class 23 is "integrity constraint violation".  This is
        // used by PostgreSQL and by MySQL's standard error states.
        if self.sqlstate.starts_with("23") {
            return Some(SqlErrorKind::Constraint);
        }

        // SQLite reports primary and extended result codes.  The low byte is
        // the primary code, so all SQLITE_CONSTRAINT_* variants classify as
        // the portable constraint category.
        if self
            .vendor_code
            .parse::<i32>()
            .ok()
            .is_some_and(|code| code & 0xff == 19)
        {
            return Some(SqlErrorKind::Constraint);
        }

        // A few MySQL constraint errors are emitted by older drivers with a
        // non-standard/empty SQLSTATE.  Preserve the portable category for
        // those well-known vendor codes as a fallback.
        if self.vendor_code.parse::<u16>().ok().is_some_and(|code| {
            matches!(
                code,
                1022 | 1048 | 1062 | 1169 | 1216 | 1217 | 1451 | 1452 | 1557 | 1577 | 1644 | 3819
            )
        }) {
            return Some(SqlErrorKind::Constraint);
        }

        // SQL Server reports constraint violations as vendor error numbers.
        if self
            .vendor_code
            .parse::<u32>()
            .ok()
            .is_some_and(|code| matches!(code, 2601 | 2627 | 547 | 515 | 544 | 8114 | 8115))
        {
            return Some(SqlErrorKind::Constraint);
        }

        None
    }
}

#[derive(Clone, Debug)]
struct SqlFailure {
    detail: String,
    // Keep the fallible result small enough for hot query paths. Provider
    // diagnostics carry several optional strings and belong behind a box;
    // callers still receive the same structured fields at the Mux boundary.
    diagnostic: Box<ProviderDiagnostic>,
    kind: Option<SqlErrorKind>,
}

impl SqlFailure {
    fn plain(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            diagnostic: Box::new(ProviderDiagnostic::default()),
            kind: None,
        }
    }

    fn sqlite(context: &str, error: rusqlite::Error) -> Self {
        Self {
            detail: format!("{context}: {error}"),
            diagnostic: Box::new(ProviderDiagnostic::sqlite(&error)),
            kind: None,
        }
    }

    fn postgres(context: &str, error: postgres::Error) -> Self {
        Self {
            detail: format!("{context}: {error}"),
            diagnostic: Box::new(ProviderDiagnostic::postgres(&error)),
            kind: None,
        }
    }

    fn mysql(context: &str, error: mysql::Error) -> Self {
        Self {
            detail: format!("{context}: {error}"),
            diagnostic: Box::new(ProviderDiagnostic::mysql(&error)),
            kind: None,
        }
    }

    fn sqlserver(context: &str, error: tiberius::error::Error) -> Self {
        let vendor_code = error
            .code()
            .map_or_else(String::new, |code| code.to_string());
        Self {
            detail: format!("{context}: {error}"),
            diagnostic: Box::new(ProviderDiagnostic {
                code: vendor_code.clone(),
                vendor_code,
                ..ProviderDiagnostic::default()
            }),
            kind: None,
        }
    }

    fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            diagnostic: Box::new(ProviderDiagnostic::default()),
            kind: Some(SqlErrorKind::Unsupported),
        }
    }

    fn invalid(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            diagnostic: Box::new(ProviderDiagnostic::default()),
            kind: Some(SqlErrorKind::Invalid),
        }
    }

    fn timeout(mut self) -> Self {
        self.kind = Some(SqlErrorKind::Timeout);
        self
    }

    fn cancelled(mut self) -> Self {
        self.kind = Some(SqlErrorKind::Cancelled);
        self
    }
}

impl From<String> for SqlFailure {
    fn from(detail: String) -> Self {
        Self::plain(detail)
    }
}

impl From<SqlFailure> for String {
    fn from(failure: SqlFailure) -> Self {
        failure.detail
    }
}

/// The stable, package-specific category exposed by `SqlError.kind`.
///
/// Keep this separate from the runtime's internal error taxonomy. SQL callers
/// need a small portable set of categories that can be compared as an enum;
/// provider diagnostics remain in `detail`, `code`, and `constraint`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
// Some categories are emitted by provider/operation-specific paths that are
// still being expanded. Keep the complete public enum in the runtime ABI now;
// generic paths intentionally use `Database` until they can supply a category
// without inspecting driver display text.
#[allow(dead_code)]
enum SqlErrorKind {
    Constraint = 0,
    Timeout = 1,
    Unsupported = 2,
    Invalid = 3,
    Database = 4,
    Cancelled = 5,
}

impl SqlErrorKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Constraint => "constraint",
            Self::Timeout => "timeout",
            Self::Unsupported => "unsupported",
            Self::Invalid => "invalid",
            Self::Database => "database",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Capabilities that affect whether a query can be interrupted after it has
/// been sent to the provider.
///
/// Keep this separate from `SqlErrorKind`: an unsupported operation is a
/// result at the call site, while these flags let a caller choose a supported
/// operation before sending a statement. MySQL uses a second authenticated
/// connection and the server's `KILL QUERY` command for both operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SqlInterruptCapabilities {
    timeout: bool,
    cancellation: bool,
}

fn sql_interrupt_capabilities(provider: &str) -> SqlInterruptCapabilities {
    match provider {
        "sqlite" | "postgres" | "mysql" | "sqlserver" => SqlInterruptCapabilities {
            timeout: true,
            cancellation: true,
        },
        _ => SqlInterruptCapabilities {
            timeout: false,
            cancellation: false,
        },
    }
}

/// Return the provider name and the supported query-interruption operations.
///
/// The map is intentionally made from ordinary Mux values so this ABI remains
/// usable by callers that cannot inspect Rust types. The keys are stable:
/// `provider`, `query_timeout`, and `query_cancellation`.
fn sql_capabilities_value(provider: &str) -> Value {
    let interrupt = sql_interrupt_capabilities(provider);
    let mut capabilities = crate::ordered::OrderedMap::new();
    capabilities.insert(
        Value::String("provider".to_string()),
        Value::String(provider.to_string()),
    );
    capabilities.insert(
        Value::String("query_timeout".to_string()),
        Value::Bool(interrupt.timeout),
    );
    capabilities.insert(
        Value::String("query_cancellation".to_string()),
        Value::Bool(interrupt.cancellation),
    );
    Value::Map(capabilities)
}

#[derive(Clone)]
enum SqlParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
}

impl TdsToSql for SqlParam {
    fn to_sql(&self) -> TdsColumnData<'_> {
        match self {
            SqlParam::Null => TdsColumnData::String(None),
            SqlParam::Bool(value) => TdsColumnData::Bit(Some(*value)),
            SqlParam::Int(value) => TdsColumnData::I64(Some(*value)),
            SqlParam::Float(value) => TdsColumnData::F64(Some(*value)),
            SqlParam::String(value) => TdsColumnData::String(Some(Cow::Borrowed(value.as_str()))),
            SqlParam::Bytes(value) => TdsColumnData::Binary(Some(Cow::Borrowed(value.as_slice()))),
        }
    }
}

// Float-to-int conversion is valid for the half-open interval
// `[-2^63, 2^63)`. These values are exactly representable as `f64`, unlike
// `i64::MAX as f64`, which rounds up to the exclusive upper bound.
const I64_MIN_AS_F64: f64 = -9_223_372_036_854_775_808.0;
const I64_MAX_EXCLUSIVE_AS_F64: f64 = 9_223_372_036_854_775_808.0;

fn next_handle() -> i64 {
    loop {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst);
        if handle > 0 {
            return handle;
        }
        let _ = NEXT_HANDLE.compare_exchange(handle, 1, Ordering::SeqCst, Ordering::SeqCst);
    }
}

fn sql_result_ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn sql_error_value(
    kind: SqlErrorKind,
    provider: &str,
    constraint: String,
    message: String,
) -> Result<Value, String> {
    sql_error_value_with_operation(kind, provider, constraint, String::new(), message)
}

fn sql_error_value_with_operation(
    kind: SqlErrorKind,
    provider: &str,
    constraint: String,
    operation: String,
    message: String,
) -> Result<Value, String> {
    sql_error_value_with_diagnostic(
        kind,
        provider,
        &operation,
        SqlFailure {
            detail: message,
            diagnostic: Box::new(ProviderDiagnostic {
                constraint,
                ..ProviderDiagnostic::default()
            }),
            kind: None,
        },
    )
}

fn sql_error_value_with_diagnostic(
    kind: SqlErrorKind,
    provider: &str,
    operation: &str,
    failure: SqlFailure,
) -> Result<Value, String> {
    let SqlFailure {
        detail, diagnostic, ..
    } = failure;
    let ProviderDiagnostic {
        code,
        sqlstate,
        vendor_code,
        constraint,
    } = *diagnostic;
    let code = if !code.is_empty() {
        code
    } else if !sqlstate.is_empty() {
        sqlstate
    } else {
        vendor_code
    };
    let handle = next_handle();
    SQL_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            SqlErrorEntry {
                kind,
                detail,
                provider: provider.to_string(),
                code,
                constraint,
                operation: operation.to_string(),
            },
        );
    create_handle_value(handle, *SQL_ERROR_TYPE_ID)
}

fn sql_result_err(msg: String) -> *mut Value {
    sql_result_err_kind(SqlErrorKind::Database, msg)
}

fn sql_result_err_kind(kind: SqlErrorKind, msg: String) -> *mut Value {
    let error =
        sql_error_value(kind, "unknown", String::new(), msg.clone()).unwrap_or(Value::String(msg));
    mux_rc_alloc(Value::Result(Err(Box::new(error))))
}

fn sql_result_err_context(
    kind: SqlErrorKind,
    provider: &str,
    operation: &str,
    message: String,
) -> *mut Value {
    let error = sql_error_value_with_operation(
        kind,
        provider,
        String::new(),
        operation.to_string(),
        message.clone(),
    )
    .unwrap_or(Value::String(message));
    mux_rc_alloc(Value::Result(Err(Box::new(error))))
}

fn sql_result_err_failure(
    kind: SqlErrorKind,
    provider: &str,
    operation: &str,
    failure: SqlFailure,
) -> *mut Value {
    let fallback = failure.detail.clone();
    let kind = failure
        .kind
        .or_else(|| failure.diagnostic.kind())
        .unwrap_or(kind);
    let error = sql_error_value_with_diagnostic(kind, provider, operation, failure)
        .unwrap_or(Value::String(fallback));
    mux_rc_alloc(Value::Result(Err(Box::new(error))))
}

fn connection_provider(handle: i64) -> &'static str {
    SQL_CONNECTIONS.with(|connections| match connections.borrow().get(&handle) {
        Some(SqlConnection::Sqlite(_)) => "sqlite",
        Some(SqlConnection::Postgres(_)) => "postgres",
        Some(SqlConnection::MySql(_)) => "mysql",
        Some(SqlConnection::SqlServer(_)) => "sqlserver",
        None => "unknown",
    })
}

fn transaction_provider(handle: i64) -> &'static str {
    SQL_TRANSACTIONS.with(|transactions| {
        transactions
            .borrow()
            .get(&handle)
            .map_or("unknown", |transaction| transaction.provider)
    })
}

fn migrator_provider(handle: i64) -> &'static str {
    SQL_MIGRATORS.with(|migrators| {
        migrators
            .borrow()
            .get(&handle)
            .map_or("unknown", |migrator| {
                connection_provider(migrator.connection_handle)
            })
    })
}

fn row_provider(handle: i64) -> &'static str {
    SQL_ROWS.with(|rows| {
        rows.borrow()
            .get(&handle)
            .map_or("unknown", |row| row.provider)
    })
}

fn provider_for_uri(uri: &str) -> &'static str {
    if uri.starts_with("sqlite:") {
        "sqlite"
    } else if uri.starts_with("postgres") {
        "postgres"
    } else if uri.starts_with("mysql:") || uri.starts_with("mariadb:") {
        "mysql"
    } else if is_sqlserver_uri(uri)
        || uri
            .get(..11)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sqlserver:"))
        || uri
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("mssql:"))
    {
        "sqlserver"
    } else {
        "unknown"
    }
}

fn pool_provider(handle: i64) -> &'static str {
    let Ok(state) = pool_state(handle) else {
        return "unknown";
    };
    let uri = state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .uri
        .clone();
    provider_for_uri(&uri)
}

fn resultset_provider(resultset: &SqlResultSet) -> &'static str {
    if resultset.sqlite_cursor.is_some() {
        return "sqlite";
    }
    if resultset.postgres_cursor.is_some() {
        return "postgres";
    }
    if resultset.mysql_cursor.is_some() {
        return "mysql";
    }
    if resultset.sqlserver_cursor.is_some() {
        return "sqlserver";
    }
    if let Some(connection) = resultset.connection_lease {
        return connection_provider(connection);
    }
    if let Some(transaction) = resultset.transaction_lease {
        return transaction_provider(transaction);
    }
    if let Some(lease) = resultset.pool_lease.as_ref() {
        let uri = lease
            .state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .uri
            .clone();
        return provider_for_uri(&uri);
    }
    "unknown"
}

fn prepared_provider(handle: i64) -> &'static str {
    with_prepared(handle, |statement| {
        Ok::<_, String>(connection_provider(statement.connection_handle))
    })
    .unwrap_or("unknown")
}

fn sql_result_invalid(msg: String) -> *mut Value {
    sql_result_err_kind(SqlErrorKind::Invalid, msg)
}

/// A typed accessor's answer: the value, or why it is not that kind.
///
/// The conversion helpers already produce a message; this used to call `.ok()`
/// on them and throw it away, so "not an int" reached the caller with no
/// indication of what the column actually held.
fn sql_accessor(converted: Result<Value, String>) -> *mut Value {
    match converted {
        Ok(value) => sql_result_ok(value),
        Err(message) => sql_result_invalid(message),
    }
}

fn sql_result_i64_failure_context(
    result: Result<i64, SqlFailure>,
    provider: &str,
    operation: &str,
) -> *mut Value {
    match result {
        Ok(value) => sql_result_ok(Value::Int(value)),
        Err(error) => sql_result_err_failure(SqlErrorKind::Database, provider, operation, error),
    }
}

fn sql_result_value_failure_context(
    result: Result<Value, SqlFailure>,
    provider: &str,
    operation: &str,
) -> *mut Value {
    match result {
        Ok(value) => sql_result_ok(value),
        Err(error) => sql_result_err_failure(SqlErrorKind::Database, provider, operation, error),
    }
}

fn sql_result_unit_failure_context(
    result: Result<(), SqlFailure>,
    provider: &str,
    operation: &str,
) -> *mut Value {
    match result {
        Ok(()) => sql_result_ok(Value::Unit),
        Err(error) => sql_result_err_failure(SqlErrorKind::Database, provider, operation, error),
    }
}

fn create_handle_value(handle: i64, type_id: TypeId) -> Result<Value, String> {
    let obj_ptr = alloc_object(type_id);
    if obj_ptr.is_null() {
        return Err("could not allocate SQL handle".to_string());
    }
    let data_ptr = unsafe { get_object_ptr(obj_ptr) };
    if data_ptr.is_null() {
        unsafe { mux_rc_dec(obj_ptr) };
        return Err("could not initialize SQL handle".to_string());
    }
    unsafe { *data_ptr.cast::<i64>() = handle };
    let value = unsafe { (*obj_ptr).clone() };
    unsafe { mux_rc_dec(obj_ptr) };
    Ok(value)
}

fn sql_result_handle(handle: i64, type_id: TypeId) -> *mut Value {
    match create_handle_value(handle, type_id) {
        Ok(value) => sql_result_ok(value),
        Err(error) => sql_result_err(error),
    }
}

fn require_handle(value: *const Value, type_id: TypeId, label: &str) -> Result<i64, String> {
    if value.is_null() || unsafe { get_object_type_id(value) } != type_id {
        return Err(format!("invalid {label}"));
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return Err(format!("invalid {label}"));
    }
    let handle = unsafe { *(ptr as *const i64) };
    (handle != 0)
        .then_some(handle)
        .ok_or_else(|| format!("invalid {label}"))
}

fn connection_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *SQL_CONNECTION_TYPE_ID, "sql connection")
}

fn transaction_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *SQL_TRANSACTION_TYPE_ID, "sql transaction")
}

fn resultset_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *SQL_RESULTSET_TYPE_ID, "sql result set")
}

fn row_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *SQL_ROW_TYPE_ID, "sql row")
}

fn pool_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *SQL_POOL_TYPE_ID, "sql pool")
}

fn prepared_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *SQL_PREPARED_TYPE_ID, "sql prepared statement")
}

fn migration_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *SQL_MIGRATION_TYPE_ID, "sql migration")
}

fn migrator_handle(value: *const Value) -> Result<i64, String> {
    require_handle(value, *SQL_MIGRATOR_TYPE_ID, "sql migrator")
}

fn write_handle(value: *mut Value, handle: i64) {
    if value.is_null() {
        return;
    }
    let ptr = unsafe { get_object_ptr(value) };
    if ptr.is_null() {
        return;
    }
    unsafe { *ptr.cast::<i64>() = handle };
}

fn remove_connection(handle: i64) {
    SQL_CONNECTIONS.with(|connections| {
        connections.borrow_mut().remove(&handle);
    });
}

fn remove_prepared_for_connection(connection_handle: i64) {
    SQL_PREPARED.with(|prepared| {
        prepared
            .borrow_mut()
            .retain(|_, statement| statement.connection_handle != connection_handle);
    });
}

fn take_connection(handle: i64) -> Result<SqlConnection, String> {
    SQL_CONNECTIONS.with(|connections| {
        connections
            .borrow_mut()
            .remove(&handle)
            .ok_or_else(|| "connection handle not found".to_string())
    })
}

fn connection_has_active_transaction(handle: i64) -> bool {
    SQL_TRANSACTIONS.with(|transactions| {
        transactions
            .borrow()
            .values()
            .any(|tx| tx.active && tx.connection_handle == handle)
    })
}

fn return_connection(handle: i64, connection: SqlConnection) {
    SQL_CONNECTIONS.with(|connections| {
        connections.borrow_mut().insert(handle, connection);
    });
}

fn remove_transaction_for_connection(conn_handle: i64) {
    let leased = SQL_TRANSACTION_LEASES.with(|leases| {
        leases
            .borrow()
            .iter()
            .filter_map(|(handle, count)| (*count > 0).then_some(*handle))
            .collect::<HashSet<_>>()
    });
    let removed = SQL_TRANSACTIONS.with(|transactions| {
        let mut map = transactions.borrow_mut();
        let handles: Vec<_> = map
            .iter()
            .filter_map(|(handle, tx)| {
                (tx.connection_handle == conn_handle && leased.contains(handle)).then_some(*handle)
            })
            .collect::<Vec<_>>();
        SQL_TRANSACTION_CLOSE_PENDING.with(|pending| {
            pending.borrow_mut().extend(handles.iter().copied());
        });
        let removable: Vec<_> = map
            .iter()
            .filter_map(|(handle, tx)| {
                (tx.connection_handle == conn_handle && !leased.contains(handle)).then_some(*handle)
            })
            .collect();
        removable
            .into_iter()
            .filter_map(|handle| map.remove(&handle))
            .collect::<Vec<_>>()
    });
    for mut tx in removed {
        if let Some(mut connection) = tx.connection.take() {
            let _ = rollback_connection(&mut connection);
        }
        unsafe { mux_rc_dec(tx.owner) };
    }
}

fn finish_deferred_transaction(handle: i64) {
    let should_finish =
        SQL_TRANSACTION_CLOSE_PENDING.with(|pending| pending.borrow_mut().remove(&handle));
    if !should_finish {
        return;
    }
    let Some(mut tx) = take_transaction(handle) else {
        return;
    };
    if let Some(mut connection) = tx.connection.take() {
        if finish_transaction_backend(&mut connection, tx.parent.as_ref(), false).is_ok() {
            restore_transaction_connection(&tx, connection);
        } else {
            let _ = rollback_connection(&mut connection);
            remove_transaction_for_connection(tx.connection_handle);
        }
    }
    unsafe { mux_rc_dec(tx.owner) };
}

fn take_transaction(handle: i64) -> Option<SqlTransaction> {
    SQL_TRANSACTIONS.with(|transactions| transactions.borrow_mut().remove(&handle))
}

fn remove_resultset(handle: i64) {
    let (connection_lease, transaction_lease, pool_lease) = SQL_RESULTSETS.with(|resultsets| {
        resultsets
            .borrow_mut()
            .remove(&handle)
            .map_or((None, None, None), |mut resultset| {
                if resultset.postgres_cursor.is_some() || resultset.sqlserver_cursor.is_some() {
                    if let Some(lease) = resultset.pool_lease.as_mut() {
                        lease.discard();
                    }
                }
                (
                    resultset.connection_lease,
                    resultset.transaction_lease,
                    resultset.pool_lease,
                )
            })
    });
    if let Some(connection) = connection_lease {
        release_connection_lease(connection);
    }
    if let Some(transaction) = transaction_lease {
        release_transaction_lease(transaction);
    }
    drop(pool_lease);
}

fn release_connection_lease(connection: i64) {
    let released = SQL_CONNECTION_LEASES.with(|leases| {
        let mut leases = leases.borrow_mut();
        if let Some(count) = leases.get_mut(&connection) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                leases.remove(&connection);
                return true;
            }
        }
        false
    });
    if released {
        let pending =
            SQL_CONNECTION_CLOSE_PENDING.with(|pending| pending.borrow_mut().remove(&connection));
        if pending {
            remove_prepared_for_connection(connection);
            remove_connection(connection);
        }
    }
}

fn connection_has_resultset_lease(connection: i64) -> bool {
    SQL_CONNECTION_LEASES.with(|leases| leases.borrow().get(&connection).copied().unwrap_or(0) > 0)
}

fn release_transaction_lease(transaction: i64) {
    let released = SQL_TRANSACTION_LEASES.with(|leases| {
        let mut leases = leases.borrow_mut();
        if let Some(count) = leases.get_mut(&transaction) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                leases.remove(&transaction);
                return true;
            }
        }
        false
    });
    if released {
        finish_deferred_transaction(transaction);
    }
}

fn transaction_has_resultset_lease(transaction: i64) -> bool {
    SQL_TRANSACTION_LEASES
        .with(|leases| leases.borrow().get(&transaction).copied().unwrap_or(0) > 0)
}

fn release_resultset_leases(
    connection: Option<i64>,
    transaction: Option<i64>,
    pool: Option<PoolLease>,
) {
    if let Some(connection) = connection {
        release_connection_lease(connection);
    }
    if let Some(transaction) = transaction {
        release_transaction_lease(transaction);
    }
    drop(pool);
}

fn finish_resultset(resultset: &mut SqlResultSet, discard_pool: bool) -> ResultSetLeases {
    if discard_pool && (resultset.postgres_cursor.is_some() || resultset.sqlserver_cursor.is_some())
    {
        if let Some(lease) = resultset.pool_lease.as_mut() {
            lease.discard();
        }
    }
    let leases = (
        resultset.connection_lease.take(),
        resultset.transaction_lease.take(),
        resultset.pool_lease.take(),
    );
    resultset.closed = true;
    resultset.next_ordered_index = resultset.ordered_rows.len();
    resultset.sqlite_cursor.take();
    resultset.postgres_cursor.take();
    resultset.mysql_cursor.take();
    resultset.sqlserver_cursor.take();
    leases
}

fn remove_row(handle: i64) {
    SQL_ROWS.with(|rows| {
        rows.borrow_mut().remove(&handle);
    });
}

extern "C" fn drop_connection_handle(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        if connection_has_resultset_lease(handle) {
            SQL_CONNECTION_CLOSE_PENDING.with(|pending| {
                pending.borrow_mut().insert(handle);
            });
            return;
        }
        remove_prepared_for_connection(handle);
        remove_connection(handle);
    }
}

extern "C" fn drop_transaction_handle(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }

    let handle = unsafe { *ptr.cast::<i64>() };
    if handle == 0 {
        return;
    }

    if transaction_has_resultset_lease(handle) {
        SQL_TRANSACTION_CLOSE_PENDING.with(|pending| {
            pending.borrow_mut().insert(handle);
        });
        return;
    }

    finish_deferred_transaction(handle);
    if SQL_TRANSACTIONS.with(|transactions| transactions.borrow().contains_key(&handle)) {
        let Some(mut tx) = take_transaction(handle) else {
            return;
        };
        if let Some(mut connection) = tx.connection.take() {
            if finish_transaction_backend(&mut connection, tx.parent.as_ref(), false).is_ok() {
                restore_transaction_connection(&tx, connection);
            } else {
                let _ = rollback_connection(&mut connection);
                remove_transaction_for_connection(tx.connection_handle);
            }
        }
        unsafe { mux_rc_dec(tx.owner) };
    }
}

extern "C" fn drop_resultset_handle(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        remove_resultset(handle);
    }
}

extern "C" fn drop_row_handle(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        remove_row(handle);
    }
}

extern "C" fn copy_pool_handle(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let handle = unsafe { *source.cast::<i64>() };
    let mut pools = SQL_POOLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = pools.get_mut(&handle) {
        entry.names += 1;
        unsafe { *dest.cast::<i64>() = handle };
    } else {
        unsafe { *dest.cast::<i64>() = 0 };
    }
}

extern "C" fn drop_pool_handle(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    let mut pools = SQL_POOLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let remove = pools.get_mut(&handle).is_some_and(|entry| {
        entry.names = entry.names.saturating_sub(1);
        entry.names == 0
    });
    if let Some(entry) = remove.then(|| pools.remove(&handle)).flatten() {
        close_pool_state(&entry.state);
    }
}

extern "C" fn drop_prepared_handle(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        SQL_PREPARED.with(|prepared| {
            prepared.borrow_mut().remove(&handle);
        });
    }
}

extern "C" fn copy_migration_handle(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let handle = unsafe { *source.cast::<i64>() };
    SQL_MIGRATIONS.with(|migrations| {
        let mut migrations = migrations.borrow_mut();
        if let Some(entry) = migrations.get_mut(&handle) {
            entry.names += 1;
            unsafe { *dest.cast::<i64>() = handle };
        } else {
            unsafe { *dest.cast::<i64>() = 0 };
        }
    });
}

extern "C" fn drop_migration_handle(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle == 0 {
        return;
    }
    SQL_MIGRATIONS.with(|migrations| {
        let mut migrations = migrations.borrow_mut();
        let remove = migrations.get_mut(&handle).is_some_and(|entry| {
            entry.names = entry.names.saturating_sub(1);
            entry.names == 0
        });
        if remove {
            migrations.remove(&handle);
        }
    });
}

extern "C" fn copy_migrator_handle(source: *mut c_void, dest: *mut c_void) {
    if source.is_null() || dest.is_null() {
        return;
    }
    let handle = unsafe { *source.cast::<i64>() };
    SQL_MIGRATORS.with(|migrators| {
        let mut migrators = migrators.borrow_mut();
        if let Some(entry) = migrators.get_mut(&handle) {
            entry.names += 1;
            unsafe { *dest.cast::<i64>() = handle };
        } else {
            unsafe { *dest.cast::<i64>() = 0 };
        }
    });
}

extern "C" fn drop_migrator_handle(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle == 0 {
        return;
    }
    SQL_MIGRATORS.with(|migrators| {
        let mut migrators = migrators.borrow_mut();
        let remove = migrators.get_mut(&handle).is_some_and(|entry| {
            entry.names = entry.names.saturating_sub(1);
            entry.names == 0
        });
        if remove {
            migrators.remove(&handle);
        }
    });
}

extern "C" fn drop_sql_error(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let handle = unsafe { *ptr.cast::<i64>() };
    if handle != 0 {
        SQL_ERRORS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    }
}

fn store_connection(connection: SqlConnection) -> i64 {
    let handle = next_handle();
    SQL_CONNECTIONS.with(|connections| {
        connections.borrow_mut().insert(handle, connection);
    });
    handle
}

fn store_transaction(transaction: SqlTransaction) -> i64 {
    let handle = next_handle();
    SQL_TRANSACTIONS.with(|transactions| {
        transactions.borrow_mut().insert(handle, transaction);
    });
    handle
}

fn store_resultset(resultset: SqlResultSet) -> i64 {
    let handle = next_handle();
    SQL_RESULTSETS.with(|resultsets| {
        resultsets.borrow_mut().insert(handle, resultset);
    });
    handle
}

fn store_connection_resultset(mut resultset: SqlResultSet, connection: i64) -> i64 {
    resultset.connection_lease = Some(connection);
    let handle = store_resultset(resultset);
    SQL_CONNECTION_LEASES.with(|leases| {
        leases
            .borrow_mut()
            .entry(connection)
            .and_modify(|count| *count = count.saturating_add(1))
            .or_insert(1);
    });
    handle
}

fn store_transaction_resultset(mut resultset: SqlResultSet, transaction: i64) -> i64 {
    resultset.transaction_lease = Some(transaction);
    let handle = store_resultset(resultset);
    SQL_TRANSACTION_LEASES.with(|leases| {
        leases
            .borrow_mut()
            .entry(transaction)
            .and_modify(|count| *count = count.saturating_add(1))
            .or_insert(1);
    });
    handle
}

fn store_pool_resultset(mut resultset: SqlResultSet, lease: PoolLease) -> i64 {
    resultset.pool_lease = Some(lease);
    store_resultset(resultset)
}

fn create_resultset_value(handle: i64) -> Result<Value, SqlFailure> {
    match create_handle_value(handle, *SQL_RESULTSET_TYPE_ID) {
        Ok(value) => Ok(value),
        Err(error) => {
            // The cursor and its lease are owned by the registry. Remove them
            // if the public handle cannot be allocated, so an allocation
            // failure cannot strand a connection in the busy state.
            remove_resultset(handle);
            Err(SqlFailure::from(error))
        }
    }
}

fn store_row(row: SqlRow) -> Result<Value, String> {
    let handle = next_handle();
    SQL_ROWS.with(|rows| {
        rows.borrow_mut().insert(handle, row);
    });
    match create_handle_value(handle, *SQL_ROW_TYPE_ID) {
        Ok(value) => Ok(value),
        Err(error) => {
            remove_row(handle);
            Err(error)
        }
    }
}

fn materialize_row(
    provider: &'static str,
    columns: &[String],
    values: Vec<Value>,
) -> Result<Value, String> {
    if values.len() != columns.len() {
        return Err("SQL row value count does not match column count".to_string());
    }
    store_row(SqlRow {
        provider,
        columns: columns.to_vec(),
        values,
    })
}

fn store_pool(state: PoolState) -> Result<Value, String> {
    let handle = next_handle();
    SQL_POOLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handle,
            PoolEntry {
                state: Arc::new(state),
                names: 1,
            },
        );
    create_handle_value(handle, *SQL_POOL_TYPE_ID).inspect_err(|_| {
        SQL_POOLS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle);
    })
}

fn store_prepared(prepared: SqlPrepared) -> i64 {
    let handle = next_handle();
    SQL_PREPARED.with(|values| {
        values.borrow_mut().insert(handle, prepared);
    });
    handle
}

fn with_connection<R, E, F>(handle: i64, op: F) -> Result<R, E>
where
    F: FnOnce(&mut SqlConnection) -> Result<R, E>,
    E: From<String>,
{
    SQL_CONNECTIONS.with(|connections| {
        let mut map = connections.borrow_mut();
        if connection_has_resultset_lease(handle) {
            return Err(E::from(
                "sql connection is busy while a result set is open".to_string(),
            ));
        }
        let connection = map
            .get_mut(&handle)
            .ok_or_else(|| E::from("invalid sql connection handle".to_string()))?;
        op(connection)
    })
}

fn with_transaction<R, E, F>(handle: i64, op: F) -> Result<R, E>
where
    F: FnOnce(&mut SqlTransaction) -> Result<R, E>,
    E: From<String>,
{
    SQL_TRANSACTIONS.with(|transactions| {
        let mut map = transactions.borrow_mut();
        if transaction_has_resultset_lease(handle) {
            return Err(E::from(
                "sql transaction is busy while a result set is open".to_string(),
            ));
        }
        let transaction = map
            .get_mut(&handle)
            .ok_or_else(|| E::from("invalid sql transaction handle".to_string()))?;
        op(transaction)
    })
}

type ResultSetLeases = (Option<i64>, Option<i64>, Option<PoolLease>);

fn with_resultset<R, F>(handle: i64, op: F) -> Result<R, SqlFailure>
where
    F: FnOnce(&mut SqlResultSet) -> Result<R, SqlFailure>,
{
    SQL_RESULTSETS.with(|resultsets| {
        let mut map = resultsets.borrow_mut();
        let resultset = map
            .get_mut(&handle)
            .ok_or_else(|| SqlFailure::plain("invalid sql result set handle"))?;
        op(resultset)
    })
}

fn with_prepared<R, E, F>(handle: i64, op: F) -> Result<R, E>
where
    F: FnOnce(&SqlPrepared) -> Result<R, E>,
    E: From<String>,
{
    SQL_PREPARED.with(|prepared| {
        let map = prepared.borrow();
        let statement = map
            .get(&handle)
            .ok_or_else(|| E::from("invalid sql prepared statement handle".to_string()))?;
        op(statement)
    })
}

fn value_to_string(value: *mut Value) -> Result<String, String> {
    if value.is_null() {
        return Err("string pointer is null".to_string());
    }
    let val = unsafe { &*value };
    if let Value::String(s) = val {
        Ok(s.clone())
    } else {
        Err("expected string".to_string())
    }
}

const DEFAULT_MIGRATION_TABLE: &str = "mux_schema_migrations";
const MAX_MIGRATIONS: usize = 4096;
const MAX_SQL_BYTES: usize = 16 * 1024 * 1024;
const MAX_MIGRATION_SQL_BYTES: usize = 16 * 1024 * 1024;
const MAX_MIGRATION_TOTAL_BYTES: usize = 64 * 1024 * 1024;

fn validate_sql_size(sql: &str) -> Result<(), String> {
    if sql.len() > MAX_SQL_BYTES {
        return Err("SQL statement exceeds the 16 MiB limit".to_string());
    }
    Ok(())
}

fn valid_migration_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn migration_checksum(up: &str, down: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"mux-migration-v1\0");
    hasher.update(up.as_bytes());
    hasher.update([0]);
    hasher.update(down.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn migration_definition(
    version: i64,
    name: String,
    up: String,
    down: String,
) -> Result<MigrationDefinition, String> {
    if version <= 0 {
        return Err("migration version must be positive".to_string());
    }
    if !valid_migration_name(&name) {
        return Err(
            "migration name must be 1-128 ASCII letters, digits, underscores, or hyphens"
                .to_string(),
        );
    }
    if up.trim().is_empty() || down.trim().is_empty() {
        return Err("migration up and down SQL must not be empty".to_string());
    }
    if up.len() > MAX_MIGRATION_SQL_BYTES || down.len() > MAX_MIGRATION_SQL_BYTES {
        return Err("migration SQL exceeds the 16 MiB limit".to_string());
    }
    Ok(MigrationDefinition {
        version,
        name,
        checksum: migration_checksum(&up, &down),
        up,
        down,
    })
}

fn validate_migration_set(
    mut migrations: Vec<MigrationDefinition>,
) -> Result<Vec<MigrationDefinition>, String> {
    if migrations.is_empty() {
        return Err("at least one migration is required".to_string());
    }
    if migrations.len() > MAX_MIGRATIONS {
        return Err(format!("at most {MAX_MIGRATIONS} migrations are supported"));
    }
    let mut total_bytes = 0usize;
    for migration in &migrations {
        total_bytes = total_bytes
            .checked_add(migration.up.len())
            .and_then(|total| total.checked_add(migration.down.len()))
            .ok_or_else(|| "migration set size overflowed".to_string())?;
        if total_bytes > MAX_MIGRATION_TOTAL_BYTES {
            return Err("migration set exceeds the 64 MiB limit".to_string());
        }
    }
    migrations.sort_unstable_by_key(|migration| migration.version);
    let mut versions = HashSet::with_capacity(migrations.len());
    for migration in &migrations {
        if !versions.insert(migration.version) {
            return Err(format!("duplicate migration version {}", migration.version));
        }
    }
    Ok(migrations)
}

fn load_file_migrations(directory: &str) -> Result<Vec<MigrationDefinition>, String> {
    if directory.is_empty() || directory.as_bytes().contains(&0) {
        return Err("migration directory must be a non-empty path without NUL bytes".to_string());
    }
    let mut up_files: HashMap<i64, (String, std::path::PathBuf)> = HashMap::new();
    let mut down_files: HashMap<i64, std::path::PathBuf> = HashMap::new();
    let mut total_bytes = 0usize;
    let entries = fs::read_dir(Path::new(directory))
        .map_err(|error| format!("cannot read migration directory: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read migration entry: {error}"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            return Err("migration filenames must be valid UTF-8".to_string());
        };
        let Some(rest) = file_name.strip_prefix('V') else {
            continue;
        };
        let Some((version_text, suffix)) = rest.split_once("__") else {
            return Err(format!("invalid migration filename '{file_name}'"));
        };
        let (name, direction) = if let Some(name) = suffix.strip_suffix(".up.sql") {
            (name, true)
        } else if let Some(name) = suffix.strip_suffix(".down.sql") {
            (name, false)
        } else {
            return Err(format!(
                "migration filename '{file_name}' must end in .up.sql or .down.sql"
            ));
        };
        let version = version_text
            .parse::<i64>()
            .map_err(|_| format!("invalid migration version in '{file_name}'"))?;
        if version <= 0 || !valid_migration_name(name) {
            return Err(format!("invalid migration filename '{file_name}'"));
        }
        let metadata = entry
            .metadata()
            .map_err(|error| format!("cannot stat migration '{file_name}': {error}"))?;
        let size = usize::try_from(metadata.len())
            .map_err(|_| format!("migration '{file_name}' is too large"))?;
        if size > MAX_MIGRATION_SQL_BYTES {
            return Err(format!("migration '{file_name}' exceeds the 16 MiB limit"));
        }
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or_else(|| "migration directory size overflowed".to_string())?;
        if total_bytes > MAX_MIGRATION_TOTAL_BYTES {
            return Err("migration directory exceeds the 64 MiB limit".to_string());
        }
        if direction {
            if up_files
                .insert(version, (name.to_string(), path.clone()))
                .is_some()
            {
                return Err(format!("duplicate up migration version {version}"));
            }
        } else if down_files.insert(version, path).is_some() {
            return Err(format!("duplicate down migration version {version}"));
        }
    }
    if up_files.is_empty() {
        return Err("migration directory contains no up migrations".to_string());
    }
    let mut migrations = Vec::with_capacity(up_files.len());
    for (version, (name, up_path)) in up_files {
        let down_path = down_files
            .remove(&version)
            .ok_or_else(|| format!("migration {version} is missing its down file"))?;
        let up = fs::read_to_string(&up_path)
            .map_err(|error| format!("cannot read migration {version} up SQL: {error}"))?;
        let down = fs::read_to_string(&down_path)
            .map_err(|error| format!("cannot read migration {version} down SQL: {error}"))?;
        migrations.push(migration_definition(version, name, up, down)?);
    }
    if !down_files.is_empty() {
        return Err(
            "migration directory contains a down migration without an up migration".to_string(),
        );
    }
    validate_migration_set(migrations)
}

fn store_migration(definition: MigrationDefinition) -> i64 {
    let handle = next_handle();
    SQL_MIGRATIONS.with(|migrations| {
        migrations.borrow_mut().insert(
            handle,
            MigrationEntry {
                definition,
                names: 1,
            },
        );
    });
    handle
}

fn store_migrator(entry: MigratorEntry) -> i64 {
    let handle = next_handle();
    SQL_MIGRATORS.with(|migrators| {
        migrators.borrow_mut().insert(handle, entry);
    });
    handle
}

fn migrator_snapshot(handle: i64) -> Result<(i64, String, Vec<MigrationDefinition>), String> {
    SQL_MIGRATORS.with(|migrators| {
        let entries = migrators.borrow();
        let migrator = entries
            .get(&handle)
            .ok_or_else(|| "invalid sql migrator handle".to_string())?;
        Ok((
            migrator.connection_handle,
            migrator.table.clone(),
            migrator.migrations.clone(),
        ))
    })
}

fn migration_status(
    connection: &mut SqlConnection,
    table: &str,
    migrations: &[MigrationDefinition],
) -> Result<Vec<Value>, SqlFailure> {
    let applied = applied_migrations_if_present(connection, table)?;
    Ok(migrations
        .iter()
        .map(|migration| migration_status_value(migration, applied.get(&migration.version)))
        .collect())
}

fn migrate_to(
    connection: &mut SqlConnection,
    table: &str,
    migrations: &[MigrationDefinition],
    target: i64,
) -> Result<i64, SqlFailure> {
    ensure_migration_table(connection, table)?;
    begin_migration_transaction(connection, table)?;
    let result = (|| {
        let applied = applied_migrations(connection, table)?;
        validate_applied_migrations(&applied, migrations).map_err(SqlFailure::plain)?;
        let current = applied.keys().copied().max().unwrap_or(0);
        if target == current {
            return Ok(0);
        }
        let mut count = 0i64;
        if target > current {
            for migration in migrations
                .iter()
                .filter(|migration| migration.version > current && migration.version <= target)
            {
                execute_batch_on_connection(connection, &migration.up)?;
                let params = vec![
                    SqlParam::Int(migration.version),
                    SqlParam::String(migration.name.clone()),
                    SqlParam::String(migration.checksum.clone()),
                ];
                execute_on_connection(connection, &migration_insert_sql(table), &params)?;
                count += 1;
            }
        } else {
            let mut versions = applied.keys().copied().collect::<Vec<_>>();
            versions.sort_unstable_by(|left, right| right.cmp(left));
            for version in versions.into_iter().filter(|version| *version > target) {
                let migration = migrations
                    .iter()
                    .find(|migration| migration.version == version)
                    .ok_or_else(|| format!("migration {version} is absent locally"))?;
                execute_batch_on_connection(connection, &migration.down)?;
                execute_on_connection(
                    connection,
                    &migration_delete_sql(table),
                    &[SqlParam::Int(version)],
                )?;
                count += 1;
            }
        }
        Ok(count)
    })();
    match result {
        Ok(count) => {
            migration_commit_connection(connection)?;
            Ok(count)
        }
        Err(error) => {
            let _ = migration_rollback_connection(connection);
            Err(error)
        }
    }
}

fn migration_apply(handle: i64, target: i64) -> Result<i64, SqlFailure> {
    let (connection_handle, table, migrations) = migrator_snapshot(handle)?;
    if connection_has_active_transaction(connection_handle) {
        return Err(SqlFailure::plain(
            "cannot migrate while another transaction is active",
        ));
    }
    with_connection(connection_handle, |connection| {
        migrate_to(connection, &table, &migrations, target)
    })
}

fn migration_down_one(handle: i64) -> Result<i64, SqlFailure> {
    let (connection_handle, table, migrations) = migrator_snapshot(handle)?;
    if connection_has_active_transaction(connection_handle) {
        return Err(SqlFailure::plain(
            "cannot migrate while another transaction is active",
        ));
    }
    with_connection(connection_handle, |connection| {
        ensure_migration_table(connection, &table)?;
        let applied = applied_migrations_if_present(connection, &table)?;
        let current = applied.keys().copied().max().unwrap_or(0);
        let target = migrations
            .iter()
            .filter(|migration| {
                migration.version < current && applied.contains_key(&migration.version)
            })
            .map(|migration| migration.version)
            .max()
            .unwrap_or(0);
        migrate_to(connection, &table, &migrations, target)
    })
}

fn migration_status_for_handle(handle: i64) -> Result<Vec<Value>, SqlFailure> {
    let (connection_handle, table, migrations) = migrator_snapshot(handle)?;
    with_connection(connection_handle, |connection| {
        migration_status(connection, &table, &migrations)
    })
}

fn migration_validate_for_handle(handle: i64) -> Result<(), SqlFailure> {
    let (connection_handle, table, migrations) = migrator_snapshot(handle)?;
    with_connection(connection_handle, |connection| {
        let applied = applied_migrations_if_present(connection, &table)?;
        validate_applied_migrations(&applied, &migrations).map_err(SqlFailure::plain)
    })
}

fn migration_dry_run_for_handle(handle: i64) -> Result<Vec<Value>, SqlFailure> {
    let (connection_handle, table, migrations) = migrator_snapshot(handle)?;
    with_connection(connection_handle, |connection| {
        let applied = applied_migrations_if_present(connection, &table)?;
        validate_applied_migrations(&applied, &migrations).map_err(SqlFailure::plain)?;
        let current = applied.keys().copied().max().unwrap_or(0);
        Ok(migrations
            .iter()
            .filter(|migration| migration.version > current)
            .map(|migration| Value::String(migration.up.clone()))
            .collect())
    })
}

fn migration_definition_from_handle(value: &Value) -> Result<MigrationDefinition, String> {
    let handle = migration_handle(value as *const Value)?;
    SQL_MIGRATIONS.with(|migrations| {
        migrations
            .borrow()
            .get(&handle)
            .map(|entry| entry.definition.clone())
            .ok_or_else(|| "invalid sql migration handle".to_string())
    })
}

fn migrator_from_definitions(
    connection: *const Value,
    definitions: Vec<MigrationDefinition>,
) -> Result<Value, String> {
    let connection_handle = connection_handle(connection)?;
    let migrations = validate_migration_set(definitions)?;
    let handle = store_migrator(MigratorEntry {
        connection_handle,
        table: DEFAULT_MIGRATION_TABLE.to_string(),
        migrations,
        names: 1,
    });
    create_handle_value(handle, *SQL_MIGRATOR_TYPE_ID)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migration_from_config(
    version: i64,
    name: *mut Value,
    up: *mut Value,
    down: *mut Value,
) -> *mut Value {
    let result = (|| {
        let name = value_to_string(name)?;
        let up = value_to_string(up)?;
        let down = value_to_string(down)?;
        let definition = migration_definition(version, name, up, down)?;
        let handle = store_migration(definition);
        create_handle_value(handle, *SQL_MIGRATION_TYPE_ID)
    })();
    match result {
        Ok(value) => sql_result_ok(value),
        Err(error) => sql_result_err_context(
            SqlErrorKind::Invalid,
            "unknown",
            "migration_from_config",
            error,
        ),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migrator_from_migrations(
    connection: *mut Value,
    migrations: *mut Value,
) -> *mut Value {
    let result = (|| {
        if migrations.is_null() {
            return Err("sql migrations must be a list<Migration>".to_string());
        }
        let Value::List(values) = (unsafe { &*migrations }) else {
            return Err("sql migrations must be a list<Migration>".to_string());
        };
        let definitions = values
            .iter()
            .map(migration_definition_from_handle)
            .collect::<Result<Vec<_>, _>>()?;
        migrator_from_definitions(connection, definitions)
    })();
    match result {
        Ok(value) => sql_result_ok(value),
        Err(error) => sql_result_err_context(
            SqlErrorKind::Invalid,
            connection_handle(connection)
                .map(connection_provider)
                .unwrap_or("unknown"),
            "migrator_from_migrations",
            error,
        ),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migrator_from_directory(
    connection: *mut Value,
    directory: *mut Value,
) -> *mut Value {
    let result = (|| {
        let directory = value_to_string(directory)?;
        migrator_from_definitions(connection, load_file_migrations(&directory)?)
    })();
    match result {
        Ok(value) => sql_result_ok(value),
        Err(error) => sql_result_err_context(
            SqlErrorKind::Invalid,
            connection_handle(connection)
                .map(connection_provider)
                .unwrap_or("unknown"),
            "migrator_from_directory",
            error,
        ),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migrator_up(migrator: *mut Value) -> *mut Value {
    let handle = match migrator_handle(migrator) {
        Ok(handle) => handle,
        Err(error) => {
            return sql_result_err_context(SqlErrorKind::Invalid, "unknown", "migrator_up", error)
        }
    };
    sql_result_i64_failure_context(
        migration_apply(handle, i64::MAX),
        migrator_provider(handle),
        "up",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migrator_up_to(migrator: *mut Value, version: i64) -> *mut Value {
    let handle = match migrator_handle(migrator) {
        Ok(handle) => handle,
        Err(error) => {
            return sql_result_err_context(
                SqlErrorKind::Invalid,
                "unknown",
                "migrator_up_to",
                error,
            )
        }
    };
    if version < 0 {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            migrator_provider(handle),
            "up_to",
            "migration target version must be non-negative".to_string(),
        );
    }
    sql_result_i64_failure_context(
        migration_apply(handle, version),
        migrator_provider(handle),
        "up_to",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migrator_down(migrator: *mut Value) -> *mut Value {
    let handle = match migrator_handle(migrator) {
        Ok(handle) => handle,
        Err(error) => {
            return sql_result_err_context(SqlErrorKind::Invalid, "unknown", "migrator_down", error)
        }
    };
    sql_result_i64_failure_context(
        migration_down_one(handle),
        migrator_provider(handle),
        "down",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migrator_down_to(migrator: *mut Value, version: i64) -> *mut Value {
    let handle = match migrator_handle(migrator) {
        Ok(handle) => handle,
        Err(error) => {
            return sql_result_err_context(
                SqlErrorKind::Invalid,
                "unknown",
                "migrator_down_to",
                error,
            )
        }
    };
    if version < 0 {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            migrator_provider(handle),
            "down_to",
            "migration target version must be non-negative".to_string(),
        );
    }
    sql_result_i64_failure_context(
        migration_apply(handle, version),
        migrator_provider(handle),
        "down_to",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migrator_status(migrator: *mut Value) -> *mut Value {
    let handle = match migrator_handle(migrator) {
        Ok(handle) => handle,
        Err(error) => {
            return sql_result_err_context(SqlErrorKind::Invalid, "unknown", "status", error)
        }
    };
    sql_result_value_failure_context(
        migration_status_for_handle(handle).map(Value::List),
        migrator_provider(handle),
        "status",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migrator_validate(migrator: *mut Value) -> *mut Value {
    let handle = match migrator_handle(migrator) {
        Ok(handle) => handle,
        Err(error) => {
            return sql_result_err_context(SqlErrorKind::Invalid, "unknown", "validate", error)
        }
    };
    sql_result_unit_failure_context(
        migration_validate_for_handle(handle),
        migrator_provider(handle),
        "validate",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_migrator_dry_run(migrator: *mut Value) -> *mut Value {
    let handle = match migrator_handle(migrator) {
        Ok(handle) => handle,
        Err(error) => {
            return sql_result_err_context(SqlErrorKind::Invalid, "unknown", "dry_run", error)
        }
    };
    sql_result_value_failure_context(
        migration_dry_run_for_handle(handle).map(Value::List),
        migrator_provider(handle),
        "dry_run",
    )
}

fn value_list_to_sql_params(value: *mut Value) -> Result<Vec<SqlParam>, String> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let val = unsafe { &*value };
    let Value::List(values) = val else {
        return Err("sql params must be a list".to_string());
    };
    values
        .iter()
        .map(mux_value_to_sql_param)
        .collect::<Result<Vec<_>, _>>()
}

fn value_list_to_sql_param_rows(value: *mut Value) -> Result<Vec<Vec<SqlParam>>, String> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Value::List(rows) = (unsafe { &*value }) else {
        return Err("sql rows must be a list<list<SqlValue>>".to_string());
    };
    if rows.len() > 65_536 {
        return Err("sql execute_many accepts at most 65536 rows".to_string());
    }
    let mut total_params = 0usize;
    let mut converted = Vec::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        let Value::List(values) = row else {
            return Err(format!(
                "sql execute_many row {row_index} must be a list<SqlValue>"
            ));
        };
        total_params = total_params
            .checked_add(values.len())
            .ok_or_else(|| "sql execute_many parameter count overflowed".to_string())?;
        if total_params > 4_194_304 {
            return Err("sql execute_many accepts at most 4194304 total parameters".to_string());
        }
        converted.push(
            values
                .iter()
                .map(mux_value_to_sql_param)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(converted)
}

#[derive(Clone, Copy, Debug)]
enum SqlBackend {
    Sqlite,
    Postgres,
    MySql,
    SqlServer,
}

fn sql_placeholder(backend: SqlBackend, index: usize) -> String {
    match backend {
        SqlBackend::Postgres => format!("${}", index + 1),
        SqlBackend::SqlServer => format!("@P{}", index + 1),
        SqlBackend::Sqlite | SqlBackend::MySql => "?".to_string(),
    }
}

/// PostgreSQL escape strings (`E'...'`) and MySQL string literals use a
/// backslash to escape the next character. SQLite does not: a backslash is an
/// ordinary character there, so treating it as an escape would make valid
/// SQLite SQL look unterminated. Keep this provider detail in the scanner so
/// placeholder and batch validation agree with the provider's literal rules.
fn quote_uses_backslash_escape(backend: SqlBackend, sql: &str, index: usize, quote: u8) -> bool {
    match backend {
        SqlBackend::MySql => quote == b'\'' || quote == b'"',
        SqlBackend::Postgres => {
            quote == b'\''
                && index > 0
                && matches!(sql.as_bytes().get(index - 1), Some(b'e' | b'E'))
        }
        SqlBackend::Sqlite | SqlBackend::SqlServer => false,
    }
}

/// Copy the UTF-8 character beginning at `index` without reinterpreting its
/// individual bytes as Unicode scalar values.  The SQL placeholder scanners
/// operate on bytes so that punctuation inside literals/comments can be
/// recognized, but ordinary SQL text must remain byte-for-byte equivalent.
fn copy_sql_char(output: &mut String, sql: &str, index: usize) -> Result<usize, String> {
    let character = sql
        .get(index..)
        .and_then(|suffix| suffix.chars().next())
        .ok_or_else(|| "SQL scanner reached an invalid UTF-8 boundary".to_string())?;
    output.push(character);
    Ok(character.len_utf8())
}

/// Return the end of a PostgreSQL dollar-quoted string beginning at `index`.
/// Dollar-quoted bodies may contain quotes and placeholder-looking text, so
/// treating them as ordinary SQL would rewrite literal `?`, `$1`, or `:name`
/// fragments into parameters. `None` means that the `$` is ordinary SQL text.
fn dollar_quote_end(sql: &str, index: usize) -> Result<Option<usize>, String> {
    let bytes = sql.as_bytes();
    if bytes.get(index) != Some(&b'$') {
        return Ok(None);
    }
    let mut delimiter_end = index + 1;
    if bytes.get(delimiter_end) == Some(&b'$') {
        delimiter_end += 1;
    } else {
        let Some(first) = bytes.get(delimiter_end) else {
            return Ok(None);
        };
        if !first.is_ascii_alphabetic() && *first != b'_' {
            return Ok(None);
        }
        delimiter_end += 1;
        while let Some(byte) = bytes.get(delimiter_end) {
            if byte.is_ascii_alphanumeric() || *byte == b'_' {
                delimiter_end += 1;
            } else {
                break;
            }
        }
        if bytes.get(delimiter_end) != Some(&b'$') {
            return Ok(None);
        }
        delimiter_end += 1;
    }

    let delimiter = &sql[index..delimiter_end];
    let body_start = delimiter_end;
    let Some(relative_close) = sql[body_start..].find(delimiter) else {
        return Err("SQL contains an unterminated dollar-quoted string".to_string());
    };
    Ok(Some(body_start + relative_close + delimiter.len()))
}

/// Rewrite positional SQL placeholders for the selected driver. Strings and
/// comments are copied byte-for-byte, so a question mark in a literal is not
/// mistaken for a parameter. Existing callers may use either `?` (portable
/// form) or PostgreSQL-style `$1`, `$2`, ...; mixed forms and malformed
/// numbering are rejected before a driver sees the statement.
fn rewrite_positional_sql(
    sql: &str,
    backend: SqlBackend,
    expected: usize,
) -> Result<String, String> {
    validate_sql_size(sql)?;
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut index = 0usize;
    let mut saw_question = false;
    let mut saw_dollar = false;
    let mut i = 0usize;
    let mut quote = None;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some((delimiter, backslash_escape)) = quote {
            if backslash_escape && byte == b'\\' {
                let character_len = copy_sql_char(&mut out, sql, i)?;
                i += character_len;
                if i < bytes.len() {
                    i += copy_sql_char(&mut out, sql, i)?;
                }
                continue;
            }
            let character_len = copy_sql_char(&mut out, sql, i)?;
            if byte == delimiter {
                if bytes.get(i + 1) == Some(&delimiter) {
                    out.push(delimiter as char);
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += character_len;
            continue;
        }
        if byte == b'\'' || byte == b'"' || byte == b'`' {
            quote = Some((byte, quote_uses_backslash_escape(backend, sql, i, byte)));
            out.push(byte as char);
            i += 1;
            continue;
        }
        if byte == b'-' && bytes.get(i + 1) == Some(&b'-') {
            let start = i;
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push_str(&sql[start..i]);
            continue;
        }
        if byte == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 >= bytes.len() {
                return Err("SQL contains an unterminated block comment".to_string());
            }
            i += 2;
            out.push_str(&sql[start..i]);
            continue;
        }
        if byte == b'$' {
            if let Some(end) = dollar_quote_end(sql, i)? {
                out.push_str(&sql[i..end]);
                i = end;
                continue;
            }
        }
        if byte == b'?' {
            if saw_dollar {
                return Err("SQL mixes ? and $n placeholders".to_string());
            }
            saw_question = true;
            out.push_str(&sql_placeholder(backend, index));
            index += 1;
            i += 1;
            continue;
        }
        if byte == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            if saw_question {
                return Err("SQL mixes ? and $n placeholders".to_string());
            }
            saw_dollar = true;
            let start = i + 1;
            i = start;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let number = sql[start..i]
                .parse::<usize>()
                .map_err(|_| "SQL placeholder number is invalid".to_string())?;
            if number == 0 || number != index + 1 {
                return Err("SQL $n placeholders must be contiguous starting at $1".to_string());
            }
            out.push_str(&sql_placeholder(backend, index));
            index += 1;
            continue;
        }
        i += copy_sql_char(&mut out, sql, i)?;
    }
    if quote.is_some() {
        return Err("SQL contains an unterminated quoted string".to_string());
    }
    if index != expected {
        return Err(format!(
            "SQL expects {index} positional parameter(s), received {expected}"
        ));
    }
    if !saw_question && !saw_dollar && expected != 0 {
        return Err("SQL has no positional placeholders".to_string());
    }
    Ok(out)
}

fn value_map_to_named_params(value: *mut Value) -> Result<HashMap<String, SqlParam>, String> {
    if value.is_null() {
        return Ok(HashMap::new());
    }
    let Value::Map(map) = (unsafe { &*value }) else {
        return Err("sql named params must be a map<string, SqlValue>".to_string());
    };
    let mut params = HashMap::new();
    for (key, value) in map {
        let Value::String(key) = key else {
            return Err("sql named parameter keys must be strings".to_string());
        };
        if params
            .insert(key.clone(), mux_value_to_sql_param(value)?)
            .is_some()
        {
            return Err(format!("duplicate SQL named parameter: {key}"));
        }
    }
    Ok(params)
}

fn rewrite_named_sql(
    sql: &str,
    backend: SqlBackend,
    named: &HashMap<String, SqlParam>,
) -> Result<(String, Vec<SqlParam>), String> {
    validate_sql_size(sql)?;
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut params = Vec::new();
    let mut used_names = HashSet::new();
    let mut i = 0usize;
    let mut quote = None;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some((delimiter, backslash_escape)) = quote {
            if backslash_escape && byte == b'\\' {
                let character_len = copy_sql_char(&mut out, sql, i)?;
                i += character_len;
                if i < bytes.len() {
                    i += copy_sql_char(&mut out, sql, i)?;
                }
                continue;
            }
            let character_len = copy_sql_char(&mut out, sql, i)?;
            if byte == delimiter {
                if bytes.get(i + 1) == Some(&delimiter) {
                    out.push(delimiter as char);
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += character_len;
            continue;
        }
        if byte == b'\'' || byte == b'"' || byte == b'`' {
            quote = Some((byte, quote_uses_backslash_escape(backend, sql, i, byte)));
            out.push(byte as char);
            i += 1;
            continue;
        }
        if byte == b'-' && bytes.get(i + 1) == Some(&b'-') {
            let start = i;
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push_str(&sql[start..i]);
            continue;
        }
        if byte == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 >= bytes.len() {
                return Err("SQL contains an unterminated block comment".to_string());
            }
            i += 2;
            out.push_str(&sql[start..i]);
            continue;
        }
        if byte == b'$' {
            if let Some(end) = dollar_quote_end(sql, i)? {
                out.push_str(&sql[i..end]);
                i = end;
                continue;
            }
        }
        if byte == b'?' || (byte == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            return Err("SQL mixes named and positional placeholders".to_string());
        }
        if byte == b':'
            && bytes
                .get(i + 1)
                .is_some_and(|next| next.is_ascii_alphabetic() || *next == b'_')
            && (i == 0 || bytes[i - 1] != b':')
        {
            let start = i + 1;
            i = start + 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let name = &sql[start..i];
            let value = named
                .get(name)
                .cloned()
                .ok_or_else(|| format!("missing SQL named parameter: {name}"))?;
            used_names.insert(name);
            out.push_str(&sql_placeholder(backend, params.len()));
            params.push(value);
            continue;
        }
        i += copy_sql_char(&mut out, sql, i)?;
    }
    if quote.is_some() {
        return Err("SQL contains an unterminated quoted string".to_string());
    }
    if params.is_empty() && !named.is_empty() {
        return Err("SQL has no named placeholders".to_string());
    }
    let mut unused_names: Vec<&str> = named
        .keys()
        .filter_map(|name| (!used_names.contains(name.as_str())).then_some(name.as_str()))
        .collect();
    unused_names.sort_unstable();
    if !unused_names.is_empty() {
        return Err(format!(
            "SQL has unused named parameter(s): {}",
            unused_names.join(", ")
        ));
    }
    Ok((out, params))
}

fn mux_value_to_sql_param(value: &Value) -> Result<SqlParam, String> {
    match value {
        Value::Unit => Ok(SqlParam::Null),
        Value::Bool(b) => Ok(SqlParam::Bool(*b)),
        Value::Int(i) => Ok(SqlParam::Int(*i)),
        Value::Float(f) => Ok(SqlParam::Float(f.into_inner())),
        Value::String(s) => Ok(SqlParam::String(s.clone())),
        Value::Bytes(bytes) => Ok(SqlParam::Bytes(bytes.clone())),
        Value::Optional(opt) => match opt {
            None => Ok(SqlParam::Null),
            Some(inner) => mux_value_to_sql_param(inner),
        },
        _ => Err("unsupported sql parameter type".to_string()),
    }
}

fn sql_param_to_sqlite(param: &SqlParam) -> SqliteValue {
    match param {
        SqlParam::Null => SqliteValue::Null,
        SqlParam::Bool(b) => SqliteValue::Integer(i64::from(*b)),
        SqlParam::Int(i) => SqliteValue::Integer(*i),
        SqlParam::Float(f) => SqliteValue::Real(*f),
        SqlParam::String(s) => SqliteValue::Text(s.clone()),
        SqlParam::Bytes(b) => SqliteValue::Blob(b.clone()),
    }
}

fn sql_param_to_mysql(param: &SqlParam) -> MySqlValue {
    match param {
        SqlParam::Null => MySqlValue::NULL,
        SqlParam::Bool(b) => MySqlValue::Int(i64::from(*b)),
        SqlParam::Int(i) => MySqlValue::Int(*i),
        SqlParam::Float(f) => MySqlValue::Double(*f),
        SqlParam::String(s) => MySqlValue::Bytes(s.as_bytes().to_vec()),
        SqlParam::Bytes(b) => MySqlValue::Bytes(b.clone()),
    }
}

fn sql_param_to_postgres(param: &SqlParam) -> Box<dyn ToSql + Sync> {
    match param {
        SqlParam::Null => Box::new(Option::<String>::None),
        SqlParam::Bool(b) => Box::new(*b),
        SqlParam::Int(i) => Box::new(*i),
        SqlParam::Float(f) => Box::new(*f),
        SqlParam::String(s) => Box::new(s.clone()),
        SqlParam::Bytes(b) => Box::new(b.clone()),
    }
}

fn sql_value_from_ref(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Unit,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(ordered_float::OrderedFloat(f)),
        // SQLite exposes TEXT as raw bytes. Preserve invalid UTF-8 as `bytes`
        // instead of silently replacing it; callers can choose the appropriate
        // decoding explicitly at the Mux boundary.
        ValueRef::Text(text) => match String::from_utf8(text.to_vec()) {
            Ok(text) => Value::String(text),
            Err(error) => Value::Bytes(error.into_bytes()),
        },
        ValueRef::Blob(blob) => Value::Bytes(blob.to_vec()),
    }
}

fn sqlite_cursor_failure(database: *mut rusqlite::ffi::sqlite3, context: &str) -> SqlFailure {
    let code = unsafe { rusqlite::ffi::sqlite3_extended_errcode(database) };
    let detail = unsafe {
        let message = rusqlite::ffi::sqlite3_errmsg(database);
        if message.is_null() {
            context.to_string()
        } else {
            format!("{context}: {}", CStr::from_ptr(message).to_string_lossy())
        }
    };
    let code = code.to_string();
    SqlFailure {
        detail,
        diagnostic: Box::new(ProviderDiagnostic {
            code: code.clone(),
            vendor_code: code,
            ..ProviderDiagnostic::default()
        }),
        kind: None,
    }
}

fn sqlite_cursor_bind(
    database: *mut rusqlite::ffi::sqlite3,
    statement: *mut rusqlite::ffi::sqlite3_stmt,
    index: c_int,
    param: &SqlParam,
) -> Result<(), SqlFailure> {
    let result = unsafe {
        match param {
            SqlParam::Null => rusqlite::ffi::sqlite3_bind_null(statement, index),
            SqlParam::Bool(value) => {
                rusqlite::ffi::sqlite3_bind_int64(statement, index, i64::from(*value))
            }
            SqlParam::Int(value) => rusqlite::ffi::sqlite3_bind_int64(statement, index, *value),
            SqlParam::Float(value) => rusqlite::ffi::sqlite3_bind_double(statement, index, *value),
            SqlParam::String(value) => {
                let length = c_int::try_from(value.len())
                    .map_err(|_| SqlFailure::plain("SQLite text parameter is too large"))?;
                rusqlite::ffi::sqlite3_bind_text(
                    statement,
                    index,
                    value.as_ptr().cast::<c_char>(),
                    length,
                    rusqlite::ffi::SQLITE_TRANSIENT(),
                )
            }
            SqlParam::Bytes(value) => {
                let length = c_int::try_from(value.len())
                    .map_err(|_| SqlFailure::plain("SQLite blob parameter is too large"))?;
                rusqlite::ffi::sqlite3_bind_blob(
                    statement,
                    index,
                    value.as_ptr().cast::<c_void>(),
                    length,
                    rusqlite::ffi::SQLITE_TRANSIENT(),
                )
            }
        }
    };
    if result == rusqlite::ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(sqlite_cursor_failure(
            database,
            "SQLite parameter bind failed",
        ))
    }
}

fn sqlite_cursor_columns(
    statement: *mut rusqlite::ffi::sqlite3_stmt,
) -> Result<Vec<String>, SqlFailure> {
    let count = unsafe { rusqlite::ffi::sqlite3_column_count(statement) };
    let count =
        usize::try_from(count).map_err(|_| SqlFailure::plain("SQLite column count is invalid"))?;
    let mut columns = Vec::with_capacity(count);
    for index in 0..count {
        let name = unsafe {
            let pointer = rusqlite::ffi::sqlite3_column_name(statement, index as c_int);
            if pointer.is_null() {
                return Err(SqlFailure::plain("SQLite returned a null column name"));
            }
            CStr::from_ptr(pointer).to_string_lossy().into_owned()
        };
        columns.push(name);
    }
    Ok(columns)
}

fn sqlite_cursor_value(statement: *mut rusqlite::ffi::sqlite3_stmt, index: usize) -> Value {
    let index = index as c_int;
    let kind = unsafe { rusqlite::ffi::sqlite3_column_type(statement, index) };
    match kind {
        rusqlite::ffi::SQLITE_INTEGER => {
            Value::Int(unsafe { rusqlite::ffi::sqlite3_column_int64(statement, index) })
        }
        rusqlite::ffi::SQLITE_FLOAT => Value::Float(ordered_float::OrderedFloat(unsafe {
            rusqlite::ffi::sqlite3_column_double(statement, index)
        })),
        rusqlite::ffi::SQLITE_TEXT => {
            let length = unsafe { rusqlite::ffi::sqlite3_column_bytes(statement, index) };
            let pointer = unsafe { rusqlite::ffi::sqlite3_column_text(statement, index) };
            let bytes = if pointer.is_null() || length <= 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(pointer, length as usize).to_vec() }
            };
            match String::from_utf8(bytes.clone()) {
                Ok(text) => Value::String(text),
                Err(_) => Value::Bytes(bytes),
            }
        }
        rusqlite::ffi::SQLITE_BLOB => {
            let length = unsafe { rusqlite::ffi::sqlite3_column_bytes(statement, index) };
            let pointer = unsafe { rusqlite::ffi::sqlite3_column_blob(statement, index) };
            if pointer.is_null() || length <= 0 {
                Value::Bytes(Vec::new())
            } else {
                Value::Bytes(unsafe {
                    std::slice::from_raw_parts(pointer.cast::<u8>(), length as usize).to_vec()
                })
            }
        }
        _ => Value::Unit,
    }
}

fn sql_value_to_int(value: &Value) -> Result<i64, String> {
    match value {
        Value::Int(i) => Ok(*i),
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Float(f) => {
            let raw = f.into_inner();
            if !raw.is_finite() {
                return Err("cannot convert non-finite float to int".to_string());
            }
            if raw.fract() != 0.0 {
                return Err("cannot convert non-integer float to int".to_string());
            }
            // Rust's float-to-int cast saturates instead of reporting an
            // out-of-range value. Keep SQL conversion checked: `2^63` would
            // otherwise silently become `i64::MAX`, while values below
            // `i64::MIN` would silently become `i64::MIN`.
            if !(I64_MIN_AS_F64..I64_MAX_EXCLUSIVE_AS_F64).contains(&raw) {
                return Err("cannot convert out-of-range float to int".to_string());
            }
            Ok(raw as i64)
        }
        Value::String(s) => s
            .parse::<i64>()
            .map_err(|_| "cannot parse sql value as int".to_string()),
        Value::Unit => Err("cannot convert null sql value to int".to_string()),
        _ => Err("cannot convert sql value to int".to_string()),
    }
}

fn sql_value_to_bool(value: &Value) -> Result<bool, String> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Int(i) => Ok(*i != 0),
        Value::Float(f) => Ok(f.into_inner() != 0.0),
        Value::String(s) => {
            let normalized = s.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "true" | "t" | "1" | "yes" | "y" => Ok(true),
                "false" | "f" | "0" | "no" | "n" => Ok(false),
                _ => Err("cannot parse sql value as bool".to_string()),
            }
        }
        Value::Unit => Err("cannot convert null sql value to bool".to_string()),
        _ => Err("cannot convert sql value to bool".to_string()),
    }
}

fn sql_value_to_float(value: &Value) -> Result<f64, String> {
    match value {
        Value::Float(f) => Ok(f.into_inner()),
        Value::Int(i) => Ok(*i as f64),
        Value::String(s) => s
            .parse::<f64>()
            .map_err(|_| "cannot parse sql value as float".to_string()),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Unit => Err("cannot convert null sql value to float".to_string()),
        _ => Err("cannot convert sql value to float".to_string()),
    }
}

fn sql_value_to_bytes(value: &Value) -> Result<Vec<u8>, String> {
    match value {
        Value::Bytes(items) => Ok(items.clone()),
        Value::Unit => Err("cannot convert null sql value to bytes".to_string()),
        _ => Err("sql value is not bytes".to_string()),
    }
}

fn sql_value_to_strict_string(value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Unit => Err("cannot convert null sql value to string".to_string()),
        _ => Err("sql value is not a string".to_string()),
    }
}

fn sql_value_to_json(value: &Value) -> Result<Value, String> {
    let Value::String(text) = value else {
        return Err("sql value is not JSON text".to_string());
    };
    Json::parse(text)
        .map(|json| json_to_value(&json))
        .map_err(|error| format!("cannot parse sql value as JSON: {error}"))
}

fn sql_value_to_datetime(value: &Value) -> Result<Value, String> {
    let text = sql_value_to_strict_string(value)?;
    crate::datetime_types::sql_datetime_value(&text)
}

fn sql_value_to_uuid(value: &Value) -> Result<Value, String> {
    let text = sql_value_to_strict_string(value)?;
    crate::uuid::sql_uuid_value(&text)
}

fn decode_sqlserver_component(value: &str, field: &str) -> Result<String, SqlFailure> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let Some((&high, &low)) = bytes.get(index + 1).zip(bytes.get(index + 2)) else {
            return Err(SqlFailure::invalid(format!(
                "sqlserver {field} contains an incomplete percent escape"
            )));
        };
        let hex = |byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        let Some(high) = hex(high) else {
            return Err(SqlFailure::invalid(format!(
                "sqlserver {field} contains an invalid percent escape"
            )));
        };
        let Some(low) = hex(low) else {
            return Err(SqlFailure::invalid(format!(
                "sqlserver {field} contains an invalid percent escape"
            )));
        };
        decoded.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| {
        SqlFailure::invalid(format!(
            "sqlserver {field} percent-decoding produced invalid UTF-8"
        ))
    })
}

fn parse_sqlserver_bool(value: &str, field: &str) -> Result<bool, SqlFailure> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(SqlFailure::invalid(format!(
            "sqlserver {field} must be true or false"
        ))),
    }
}

fn parse_sqlserver_uri(uri: &str) -> Result<SqlServerUri, SqlFailure> {
    let parsed = Url::parse(uri)
        .map_err(|error| SqlFailure::invalid(format!("sqlserver URL parse failed: {error}")))?;
    if !matches!(parsed.scheme(), "sqlserver" | "mssql") {
        return Err(SqlFailure::invalid(
            "SQL Server URI must use sqlserver:// or mssql://",
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| SqlFailure::invalid("sqlserver URI must include a host"))?;
    if host.is_empty() {
        return Err(SqlFailure::invalid("sqlserver URI host must not be empty"));
    }
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let path = parsed.path().strip_prefix('/').unwrap_or(parsed.path());
    if path.is_empty() || path.contains('/') {
        return Err(SqlFailure::invalid(
            "sqlserver URI must contain exactly one database path segment",
        ));
    }
    let database = decode_sqlserver_component(path, "database")?;
    if database.is_empty() {
        return Err(SqlFailure::invalid(
            "sqlserver database name must not be empty",
        ));
    }
    let username = if parsed.username().is_empty() && parsed.password().is_none() {
        None
    } else {
        Some(decode_sqlserver_component(parsed.username(), "username")?)
    };
    let password = parsed
        .password()
        .map(|value| decode_sqlserver_component(value, "password"))
        .transpose()?;
    let mut verify_tls = true;
    let mut encrypt_tls = true;
    let mut saw_encrypt = false;
    let mut saw_trust_server_certificate = false;
    for (key, value) in parsed.query_pairs() {
        match key.to_ascii_lowercase().as_str() {
            "encrypt" => {
                if saw_encrypt {
                    return Err(SqlFailure::invalid(
                        "sqlserver URI must not repeat the encrypt option",
                    ));
                }
                saw_encrypt = true;
                encrypt_tls = parse_sqlserver_bool(&value, "encrypt")?;
            }
            "trustservercertificate" | "trust_server_certificate" => {
                if saw_trust_server_certificate {
                    return Err(SqlFailure::invalid(
                        "sqlserver URI must not repeat trustServerCertificate",
                    ));
                }
                saw_trust_server_certificate = true;
                if parse_sqlserver_bool(&value, "trustServerCertificate")? {
                    verify_tls = false;
                }
            }
            "insecure" => {
                if parse_sqlserver_bool(&value, "insecure")? {
                    verify_tls = false;
                }
            }
            _ => {}
        }
    }
    Ok(SqlServerUri {
        host: host.to_string(),
        port: parsed.port().unwrap_or(1433),
        database,
        username,
        password,
        encrypt_tls,
        verify_tls,
    })
}

enum SqlServerWait<T> {
    Complete(Result<T, tiberius::error::Error>),
    Timeout,
    Cancelled,
}

async fn wait_for_sqlserver<T, F>(
    operation: F,
    timeout: Option<Duration>,
    cancellation: Option<Arc<crate::sync_primitives::CancellationEntry>>,
) -> SqlServerWait<T>
where
    F: Future<Output = Result<T, tiberius::error::Error>>,
{
    let timeout_wait = async {
        match timeout {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending::<()>().await,
        }
    };
    let cancellation_wait = async {
        if let Some(cancellation) = cancellation {
            loop {
                if cancellation_entry_is_cancelled(&cancellation) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(operation);
    tokio::pin!(timeout_wait);
    tokio::pin!(cancellation_wait);
    tokio::select! {
        result = &mut operation => SqlServerWait::Complete(result),
        () = &mut timeout_wait => SqlServerWait::Timeout,
        () = &mut cancellation_wait => SqlServerWait::Cancelled,
    }
}

fn sqlserver_timeout(timeout_ms: i64) -> Result<Duration, SqlFailure> {
    let milliseconds = u64::try_from(timeout_ms)
        .map_err(|_| SqlFailure::plain("SQL query timeout must be non-negative"))?;
    Ok(Duration::from_millis(milliseconds))
}

fn sqlserver_check_usable(connection: &SqlServerConnection) -> Result<(), SqlFailure> {
    if connection.poisoned {
        return Err(SqlFailure::plain(
            "SQL Server connection was retired after a cancelled or timed out operation",
        ));
    }
    Ok(())
}

fn sqlserver_finish_wait<T>(
    connection: &mut SqlServerConnection,
    operation: &str,
    result: SqlServerWait<T>,
) -> Result<T, SqlFailure> {
    match result {
        SqlServerWait::Complete(Ok(value)) => Ok(value),
        SqlServerWait::Complete(Err(error)) => Err(SqlFailure::sqlserver(operation, error)),
        SqlServerWait::Timeout => {
            connection.poisoned = true;
            Err(SqlFailure::plain(format!("{operation} timed out")).timeout())
        }
        SqlServerWait::Cancelled => {
            connection.poisoned = true;
            Err(SqlFailure::plain(format!("{operation} was cancelled")).cancelled())
        }
    }
}

fn sqlserver_connect(configuration: SqlServerUri) -> Result<SqlConnection, SqlFailure> {
    let mut config = TdsConfig::new();
    config.host(&configuration.host);
    config.port(configuration.port);
    config.database(&configuration.database);
    config.authentication(TdsAuthMethod::sql_server(
        configuration.username.as_deref().unwrap_or_default(),
        configuration.password.as_deref().unwrap_or_default(),
    ));
    config.encryption(if configuration.encrypt_tls {
        TdsEncryptionLevel::Required
    } else {
        TdsEncryptionLevel::NotSupported
    });
    if !configuration.verify_tls {
        config.trust_cert();
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| SqlFailure::plain(format!("sqlserver runtime setup failed: {error}")))?;
    let address = config.get_addr();
    let client = runtime.block_on(async move {
        let tcp = tokio::net::TcpStream::connect(address).await?;
        tcp.set_nodelay(true)?;
        TdsClient::connect(config, tcp.compat_write()).await
    });
    client
        .map(|client| {
            SqlConnection::SqlServer(Box::new(SqlServerConnection {
                runtime,
                client: Box::new(client),
                poisoned: false,
            }))
        })
        .map_err(|error| SqlFailure::sqlserver("sqlserver connect failed", error))
}

fn sqlserver_execute(
    connection: &mut SqlServerConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<i64, SqlFailure> {
    sqlserver_check_usable(connection)?;
    let references: Vec<&dyn TdsToSql> =
        params.iter().map(|param| param as &dyn TdsToSql).collect();
    let result = connection.runtime.block_on(wait_for_sqlserver(
        connection.client.execute(sql.to_owned(), &references),
        None,
        None,
    ));
    let result = sqlserver_finish_wait(connection, "sqlserver execute failed", result)?;
    i64::try_from(result.total())
        .map_err(|_| SqlFailure::plain("sqlserver affected row count overflowed int"))
}

fn sqlserver_simple_query(
    connection: &mut SqlServerConnection,
    sql: &str,
) -> Result<(), SqlFailure> {
    sqlserver_check_usable(connection)?;
    let result = connection.runtime.block_on(wait_for_sqlserver(
        async {
            let mut stream = connection.client.simple_query(sql.to_owned()).await?;
            while stream.try_next().await?.is_some() {}
            Ok(())
        },
        None,
        None,
    ));
    sqlserver_finish_wait(connection, "sqlserver batch execute failed", result)
}

fn sqlserver_query_streaming(
    connection: &mut SqlServerConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<SqlResultSet, SqlFailure> {
    sqlserver_query_streaming_with_options(connection, sql, params, None, None)
}

fn sqlserver_query_streaming_with_options(
    connection: &mut SqlServerConnection,
    sql: &str,
    params: &[SqlParam],
    timeout_ms: Option<i64>,
    cancellation: Option<Arc<crate::sync_primitives::CancellationEntry>>,
) -> Result<SqlResultSet, SqlFailure> {
    sqlserver_check_usable(connection)?;
    let timeout = timeout_ms.map(sqlserver_timeout).transpose()?;
    if cancellation
        .as_ref()
        .is_some_and(cancellation_entry_is_cancelled)
    {
        return Err(SqlFailure::plain("sqlserver query was cancelled").cancelled());
    }
    let references: Vec<&dyn TdsToSql> =
        params.iter().map(|param| param as &dyn TdsToSql).collect();
    let client = connection.client.as_mut() as *mut SqlServerClient;
    let runtime = &connection.runtime as *const tokio::runtime::Runtime;
    let started = Instant::now();
    let deadline = timeout
        .map(|duration| {
            started
                .checked_add(duration)
                .ok_or_else(|| SqlFailure::plain("SQL query timeout is too large"))
        })
        .transpose()?;
    let mut stream = {
        let result = unsafe {
            (&*runtime).block_on(wait_for_sqlserver(
                (&mut *client).query(sql.to_owned(), &references),
                timeout,
                cancellation.clone(),
            ))
        };
        sqlserver_finish_wait(connection, "sqlserver query failed", result)?
    };
    let metadata_timeout =
        deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    let metadata = unsafe {
        (&*runtime).block_on(wait_for_sqlserver(
            stream.columns(),
            metadata_timeout,
            cancellation.clone(),
        ))
    };
    let metadata =
        match sqlserver_finish_wait(connection, "sqlserver query metadata failed", metadata) {
            Ok(metadata) => metadata,
            Err(error) => {
                connection.poisoned = true;
                return Err(error);
            }
        };
    let columns = metadata
        .map(|columns| {
            columns
                .iter()
                .map(|column| column.name().to_string())
                .collect()
        })
        .unwrap_or_default();
    let stream =
        unsafe { std::mem::transmute::<TdsQueryStream<'_>, TdsQueryStream<'static>>(stream) };
    Ok(SqlResultSet {
        ordered_rows: Vec::new(),
        columns,
        next_ordered_index: 0,
        closed: false,
        sqlite_cursor: None,
        postgres_cursor: None,
        mysql_cursor: None,
        sqlserver_cursor: Some(SqlServerCursor {
            connection,
            client,
            runtime,
            stream,
            deadline,
            cancellation,
            exhausted: false,
        }),
        connection_lease: None,
        transaction_lease: None,
        pool_lease: None,
    })
}

fn sqlserver_column_to_value(value: &TdsColumnData<'static>) -> Value {
    match value {
        TdsColumnData::U8(value) => value.map_or(Value::Unit, |value| Value::Int(i64::from(value))),
        TdsColumnData::I16(value) => {
            value.map_or(Value::Unit, |value| Value::Int(i64::from(value)))
        }
        TdsColumnData::I32(value) => {
            value.map_or(Value::Unit, |value| Value::Int(i64::from(value)))
        }
        TdsColumnData::I64(value) => value.map_or(Value::Unit, Value::Int),
        TdsColumnData::F32(value) => value.map_or(Value::Unit, |value| {
            Value::Float(ordered_float::OrderedFloat(f64::from(value)))
        }),
        TdsColumnData::F64(value) => value.map_or(Value::Unit, |value| {
            Value::Float(ordered_float::OrderedFloat(value))
        }),
        TdsColumnData::Bit(value) => value.map_or(Value::Unit, Value::Bool),
        TdsColumnData::String(value) => value
            .as_ref()
            .map_or(Value::Unit, |value| Value::String(value.to_string())),
        TdsColumnData::Guid(value) => value.map_or(Value::Unit, |value| {
            Value::Bytes(value.into_bytes().to_vec())
        }),
        TdsColumnData::Binary(value) => value
            .as_ref()
            .map_or(Value::Unit, |value| Value::Bytes(value.to_vec())),
        TdsColumnData::Numeric(value) => {
            value.map_or(Value::Unit, |value| Value::String(value.to_string()))
        }
        TdsColumnData::Xml(value) => value
            .as_ref()
            .map_or(Value::Unit, |value| Value::String(format!("{value:?}"))),
        TdsColumnData::DateTime(value) => value
            .as_ref()
            .map_or(Value::Unit, |value| Value::String(format!("{value:?}"))),
        TdsColumnData::SmallDateTime(value) => value
            .as_ref()
            .map_or(Value::Unit, |value| Value::String(format!("{value:?}"))),
        TdsColumnData::Time(value) => value
            .as_ref()
            .map_or(Value::Unit, |value| Value::String(format!("{value:?}"))),
        TdsColumnData::Date(value) => value
            .as_ref()
            .map_or(Value::Unit, |value| Value::String(format!("{value:?}"))),
        TdsColumnData::DateTime2(value) => value
            .as_ref()
            .map_or(Value::Unit, |value| Value::String(format!("{value:?}"))),
        TdsColumnData::DateTimeOffset(value) => value
            .as_ref()
            .map_or(Value::Unit, |value| Value::String(format!("{value:?}"))),
    }
}

fn sqlserver_query_materialized(
    connection: &mut SqlServerConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<SqlResultSet, SqlFailure> {
    let mut resultset = sqlserver_query_streaming(connection, sql, params)?;
    while let Some(row) = resultset_next_row(&mut resultset)? {
        resultset.ordered_rows.push(row);
    }
    resultset.sqlserver_cursor = None;
    Ok(resultset)
}

fn sqlserver_row_to_value(row: TdsRow, columns: &[String]) -> Value {
    let values = row
        .cells()
        .map(|(_, value)| sqlserver_column_to_value(value))
        .collect();
    materialize_row("sqlserver", columns, values).unwrap_or_else(Value::String)
}

fn sqlserver_cursor_next(
    cursor: &mut SqlServerCursor,
    columns: &mut Vec<String>,
) -> Result<Option<Value>, SqlFailure> {
    let _client = cursor.client;
    if cursor
        .cancellation
        .as_ref()
        .is_some_and(cancellation_entry_is_cancelled)
    {
        unsafe { (*cursor.connection).poisoned = true };
        return Err(SqlFailure::plain("sqlserver row read was cancelled").cancelled());
    }
    loop {
        let timeout = cursor
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let result = unsafe {
            (&*cursor.runtime).block_on(wait_for_sqlserver(
                cursor.stream.try_next(),
                timeout,
                cursor.cancellation.clone(),
            ))
        };
        let item = match result {
            SqlServerWait::Complete(Ok(item)) => item,
            SqlServerWait::Complete(Err(error)) => {
                unsafe { (*cursor.connection).poisoned = true };
                return Err(SqlFailure::sqlserver("sqlserver row read failed", error));
            }
            SqlServerWait::Timeout => {
                unsafe { (*cursor.connection).poisoned = true };
                return Err(SqlFailure::plain("sqlserver row read timed out").timeout());
            }
            SqlServerWait::Cancelled => {
                unsafe { (*cursor.connection).poisoned = true };
                return Err(SqlFailure::plain("sqlserver row read was cancelled").cancelled());
            }
        };
        let Some(item) = item else {
            cursor.exhausted = true;
            return Ok(None);
        };
        match item {
            TdsQueryItem::Metadata(metadata) => {
                *columns = metadata
                    .columns()
                    .iter()
                    .map(|column| column.name().to_string())
                    .collect();
            }
            TdsQueryItem::Row(row) => return Ok(Some(sqlserver_row_to_value(row, columns))),
        }
    }
}

fn route_connect_failure(uri: &str) -> Result<SqlConnection, SqlFailure> {
    if uri == "sqlite::memory:" || uri == "sqlite://:memory:" {
        return Err(SqlFailure::unsupported(
            "in-memory SQLite connections require sql.sqlite_memory()",
        ));
    }

    if let Some(path) = uri.strip_prefix("sqlite://") {
        return SqliteConnection::open(path)
            .map(SqlConnection::Sqlite)
            .map_err(|e| SqlFailure::sqlite("sqlite connect failed", e));
    }

    if uri.starts_with("postgres://") || uri.starts_with("postgresql://") {
        return PostgresClient::connect(uri, NoTls)
            .map(|client| SqlConnection::Postgres(Box::new(client)))
            .map_err(|e| SqlFailure::postgres("postgres connect failed", e));
    }
    if uri.starts_with("mysql://") || uri.starts_with("mariadb://") {
        let opts = MySqlOpts::from_url(uri)
            .map_err(|e| SqlFailure::invalid(format!("mysql url parse failed: {e}")))?;
        return MySqlConnection::new(opts)
            .map(|connection| SqlConnection::MySql(Box::new(connection)))
            .map_err(|e| SqlFailure::mysql("mysql connect failed", e));
    }
    if is_sqlserver_uri(uri) {
        return sqlserver_connect(parse_sqlserver_uri(uri)?);
    }

    Err(SqlFailure::unsupported(format!(
        "unsupported or unrecognised sql uri scheme: {uri}"
    )))
}

fn is_sqlserver_uri(uri: &str) -> bool {
    uri.get(..12)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sqlserver://"))
        || uri
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("mssql://"))
}

/// Classify a connection URI before attempting a provider operation. This is
/// based on the URI scheme supplied by the caller, never on driver display
/// text, so unsupported providers remain distinguishable from database I/O
/// failures.
fn sql_uri_error_kind(uri: &str) -> Option<SqlErrorKind> {
    if is_sqlserver_uri(uri) {
        return Some(match parse_sqlserver_uri(uri) {
            Ok(_) => SqlErrorKind::Unsupported,
            Err(_) => SqlErrorKind::Invalid,
        });
    }
    let supported = (uri.starts_with("sqlite://") && uri != "sqlite://:memory:")
        || uri.starts_with("postgres://")
        || uri.starts_with("postgresql://")
        || uri.starts_with("mysql://")
        || uri.starts_with("mariadb://");
    (!supported).then_some(SqlErrorKind::Unsupported)
}

fn split_sql_batch(sql: &str, backend: SqlBackend) -> Result<Vec<String>, String> {
    validate_sql_size(sql).map_err(|_| "SQL batch exceeds the 16 MiB limit".to_string())?;
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut quote = None;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some((delimiter, backslash_escape)) = quote {
            if backslash_escape && byte == b'\\' {
                i += 1;
                if i < bytes.len() {
                    i += 1;
                }
                continue;
            }
            if byte == delimiter && bytes.get(i + 1) == Some(&delimiter) {
                i += 2;
                continue;
            }
            if byte == delimiter {
                quote = None;
            }
            i += 1;
            continue;
        }
        if byte == b'\'' || byte == b'"' || byte == b'`' {
            quote = Some((byte, quote_uses_backslash_escape(backend, sql, i, byte)));
            i += 1;
            continue;
        }
        if byte == b'-' && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 >= bytes.len() {
                return Err("SQL batch contains an unterminated block comment".to_string());
            }
            i += 2;
            continue;
        }
        // PostgreSQL dollar-quoted function bodies can contain semicolons,
        // quotes, and comment-looking text. Treat the complete body as one
        // opaque region so execute_batch's statement count agrees with the
        // provider's parser instead of rejecting a valid one-statement
        // function definition.
        if byte == b'$' {
            if let Some(end) = dollar_quote_end(sql, i)? {
                i = end;
                continue;
            }
        }
        if byte == b';' {
            let statement = sql[start..i].trim();
            if sql_statement_has_code(statement) {
                statements.push(statement.to_string());
            }
            if statements.len() > 1024 {
                return Err("SQL batch contains more than 1024 statements".to_string());
            }
            start = i + 1;
        }
        i += 1;
    }
    if quote.is_some() {
        return Err("SQL batch contains an unterminated quoted string".to_string());
    }
    let statement = sql[start..].trim();
    if sql_statement_has_code(statement) {
        statements.push(statement.to_string());
    }
    if statements.len() > 1024 {
        return Err("SQL batch contains more than 1024 statements".to_string());
    }
    Ok(statements)
}

fn sql_statement_has_code(statement: &str) -> bool {
    let bytes = statement.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 >= bytes.len() {
                return false;
            }
            i += 2;
            continue;
        }
        return true;
    }
    false
}

fn execute_batch_on_connection(
    connection: &mut SqlConnection,
    sql: &str,
) -> Result<(), SqlFailure> {
    let backend = match connection {
        SqlConnection::Sqlite(_) => SqlBackend::Sqlite,
        SqlConnection::Postgres(_) => SqlBackend::Postgres,
        SqlConnection::MySql(_) => SqlBackend::MySql,
        SqlConnection::SqlServer(_) => SqlBackend::SqlServer,
    };
    let statements = split_sql_batch(sql, backend).map_err(SqlFailure::plain)?;
    if statements.is_empty() {
        return Ok(());
    }
    match connection {
        SqlConnection::Sqlite(conn) => {
            for statement in statements {
                sqlite_execute(conn, &statement, &[])?;
            }
            Ok(())
        }
        SqlConnection::Postgres(client) => client
            .batch_execute(sql)
            .map_err(|error| SqlFailure::postgres("postgres batch execute failed", error)),
        SqlConnection::MySql(conn) => {
            for statement in statements {
                conn.query_drop(statement)
                    .map_err(|error| SqlFailure::mysql("mysql batch execute failed", error))?;
            }
            Ok(())
        }
        SqlConnection::SqlServer(conn) => sqlserver_simple_query(conn, sql),
    }
}

fn sqlite_execute(
    connection: &mut SqliteConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<i64, SqlFailure> {
    let sqlite_params: Vec<SqliteValue> = params.iter().map(sql_param_to_sqlite).collect();
    let affected = connection
        .execute(sql, params_from_iter(sqlite_params.iter()))
        .map_err(|e| SqlFailure::sqlite("sqlite execute failed", e))?;
    i64::try_from(affected).map_err(|_| SqlFailure::plain("affected row count overflowed int"))
}

#[allow(clippy::mutable_key_type)]
fn sqlite_query(
    connection: &mut SqliteConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<SqlResultSet, SqlFailure> {
    let sqlite_params: Vec<SqliteValue> = params.iter().map(sql_param_to_sqlite).collect();
    let mut statement = connection
        .prepare(sql)
        .map_err(|e| SqlFailure::sqlite("sqlite query prepare failed", e))?;
    let columns: Vec<String> = statement
        .column_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let column_count = columns.len();
    let mut rows = statement
        .query(params_from_iter(sqlite_params.iter()))
        .map_err(|e| SqlFailure::sqlite("sqlite query failed", e))?;

    let mut ordered_rows = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| SqlFailure::sqlite("sqlite query failed", e))?
    {
        let mut values = Vec::with_capacity(column_count);
        for idx in 0..column_count {
            let value_ref = row
                .get_ref(idx)
                .map_err(|e| SqlFailure::sqlite("sqlite row read failed", e))?;
            values.push(sql_value_from_ref(value_ref));
        }
        ordered_rows.push(materialize_row("sqlite", &columns, values).map_err(SqlFailure::from)?);
    }

    Ok(SqlResultSet {
        ordered_rows,
        columns,
        next_ordered_index: 0,
        closed: false,
        sqlite_cursor: None,
        postgres_cursor: None,
        mysql_cursor: None,
        sqlserver_cursor: None,
        connection_lease: None,
        transaction_lease: None,
        pool_lease: None,
    })
}

/// Prepare a SQLite statement without advancing it. The raw statement is
/// owned by the result set and stepped only by `next`/`next_batch`, so large
/// result sets do not become a second materialized `Vec<Value>` before the
/// caller asks for rows.
fn sqlite_query_streaming(
    connection: &mut SqliteConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<SqlResultSet, SqlFailure> {
    let database = unsafe { connection.handle() };
    let query = std::ffi::CString::new(sql)
        .map_err(|_| SqlFailure::plain("SQLite query contains an embedded NUL"))?;
    let mut statement = std::ptr::null_mut();
    let mut tail = std::ptr::null();
    let status = unsafe {
        rusqlite::ffi::sqlite3_prepare_v2(database, query.as_ptr(), -1, &mut statement, &mut tail)
    };
    if status != rusqlite::ffi::SQLITE_OK || statement.is_null() {
        return Err(sqlite_cursor_failure(
            database,
            "SQLite query prepare failed",
        ));
    }
    let cleanup = |statement: *mut rusqlite::ffi::sqlite3_stmt| {
        if !statement.is_null() {
            unsafe {
                let _ = rusqlite::ffi::sqlite3_finalize(statement);
            }
        }
    };
    let trailing = unsafe { CStr::from_ptr(tail).to_bytes() };
    if !trailing.iter().all(u8::is_ascii_whitespace) {
        cleanup(statement);
        return Err(SqlFailure::plain(
            "SQLite queries must contain exactly one statement",
        ));
    }
    let parameter_count = unsafe { rusqlite::ffi::sqlite3_bind_parameter_count(statement) };
    let parameter_count = usize::try_from(parameter_count)
        .map_err(|_| SqlFailure::plain("SQLite parameter count is invalid"))?;
    if parameter_count != params.len() {
        cleanup(statement);
        return Err(SqlFailure::plain(format!(
            "SQLite query expected {parameter_count} parameters, received {}",
            params.len()
        )));
    }
    for (offset, param) in params.iter().enumerate() {
        let index = c_int::try_from(offset + 1)
            .map_err(|_| SqlFailure::plain("SQLite parameter index is too large"))?;
        if let Err(error) = sqlite_cursor_bind(database, statement, index, param) {
            cleanup(statement);
            return Err(error);
        }
    }
    let columns = match sqlite_cursor_columns(statement) {
        Ok(columns) => columns,
        Err(error) => {
            cleanup(statement);
            return Err(error);
        }
    };
    Ok(SqlResultSet {
        ordered_rows: Vec::new(),
        columns,
        next_ordered_index: 0,
        closed: false,
        sqlite_cursor: Some(SqliteCursor {
            database,
            statement,
        }),
        postgres_cursor: None,
        mysql_cursor: None,
        sqlserver_cursor: None,
        connection_lease: None,
        transaction_lease: None,
        pool_lease: None,
    })
}

fn sqlite_cursor_next(
    cursor: &mut SqliteCursor,
    columns: &[String],
) -> Result<Option<Value>, SqlFailure> {
    let status = unsafe { rusqlite::ffi::sqlite3_step(cursor.statement) };
    match status {
        rusqlite::ffi::SQLITE_ROW => {
            let values = (0..columns.len())
                .map(|index| sqlite_cursor_value(cursor.statement, index))
                .collect();
            Ok(Some(
                materialize_row("sqlite", columns, values).map_err(SqlFailure::from)?,
            ))
        }
        rusqlite::ffi::SQLITE_DONE => Ok(None),
        _ => Err(sqlite_cursor_failure(
            cursor.database,
            "SQLite row read failed",
        )),
    }
}

/// Run one SQLite query with a bounded execution deadline.
///
/// SQLite's progress hook is the only backend-independent way exposed by the
/// bundled driver to interrupt a CPU-bound statement. The hook is installed
/// for this operation only and is always removed before returning, so a
/// timeout cannot leak into the next statement on the connection.
fn sqlite_query_with_timeout(
    connection: &mut SqliteConnection,
    sql: &str,
    params: &[SqlParam],
    timeout_ms: i64,
) -> Result<SqlResultSet, SqlFailure> {
    let milliseconds = u64::try_from(timeout_ms)
        .map_err(|_| SqlFailure::plain("SQL query timeout must be non-negative"))?;
    let timeout = std::time::Duration::from_millis(milliseconds);
    // Compare elapsed time instead of constructing an absolute deadline.
    // `Instant::checked_add` can fail for a valid, very large duration on
    // platforms with a bounded monotonic clock; treating that failure as
    // `Instant::now()` would turn a long timeout into an immediate one.
    let started = std::time::Instant::now();
    connection
        .progress_handler(1000, Some(move || started.elapsed() >= timeout))
        .map_err(|error| SqlFailure::sqlite("sqlite timeout setup failed", error))?;
    let result = sqlite_query(connection, sql, params);
    let clear_result = connection.progress_handler(0, None::<fn() -> bool>);
    if let Err(error) = clear_result {
        return Err(SqlFailure::sqlite("sqlite timeout cleanup failed", error));
    }
    match result {
        Err(error) if error.diagnostic.code == "9" => Err(error.timeout()),
        other => other,
    }
}

/// Run one SQLite query until the caller's shared cancellation token is set.
///
/// SQLite invokes its progress callback periodically while executing a
/// statement. The callback captures the token's `Arc`, not a raw Mux value, so
/// it remains valid for the complete synchronous query and can be cancelled
/// by another Mux thread. The hook is removed on every return path.
fn sqlite_query_with_cancellation(
    connection: &mut SqliteConnection,
    sql: &str,
    params: &[SqlParam],
    token: &std::sync::Arc<crate::sync_primitives::CancellationEntry>,
) -> Result<SqlResultSet, SqlFailure> {
    if cancellation_entry_is_cancelled(token) {
        return Err(SqlFailure::plain("SQL query was cancelled").cancelled());
    }
    let callback_token = std::sync::Arc::clone(token);
    connection
        .progress_handler(
            1000,
            Some(move || cancellation_entry_is_cancelled(&callback_token)),
        )
        .map_err(|error| SqlFailure::sqlite("sqlite cancellation setup failed", error))?;
    let result = sqlite_query(connection, sql, params);
    let clear_result = connection.progress_handler(0, None::<fn() -> bool>);
    if let Err(error) = clear_result {
        return Err(SqlFailure::sqlite(
            "sqlite cancellation cleanup failed",
            error,
        ));
    }
    match result {
        Err(error) if error.diagnostic.code == "9" && cancellation_entry_is_cancelled(token) => {
            Err(error.cancelled())
        }
        other => other,
    }
}

fn postgres_query_value(
    row: &postgres::Row,
    idx: usize,
    pg_type: &PgType,
) -> Result<Value, SqlFailure> {
    macro_rules! get_col {
        ($row:expr, $idx:expr, $rust_type:ty, $map:expr) => {{
            let v: Option<$rust_type> = $row
                .try_get($idx)
                .map_err(|e| SqlFailure::postgres("postgres row read failed", e))?;
            Ok(v.map_or(Value::Unit, $map))
        }};
    }

    match *pg_type {
        PgType::BOOL => get_col!(row, idx, bool, Value::Bool),
        PgType::INT2 => get_col!(row, idx, i16, |v| Value::Int(i64::from(v))),
        PgType::INT4 => get_col!(row, idx, i32, |v| Value::Int(i64::from(v))),
        PgType::INT8 => get_col!(row, idx, i64, Value::Int),
        PgType::FLOAT4 => get_col!(row, idx, f32, |v| Value::Float(
            ordered_float::OrderedFloat(f64::from(v))
        )),
        PgType::FLOAT8 => get_col!(row, idx, f64, |v| Value::Float(
            ordered_float::OrderedFloat(v)
        )),
        PgType::BYTEA => {
            let value: Option<Vec<u8>> = row
                .try_get(idx)
                .map_err(|e| SqlFailure::postgres("postgres row read failed", e))?;
            Ok(value.map_or(Value::Unit, Value::Bytes))
        }
        _ => get_col!(row, idx, String, Value::String),
    }
}

#[allow(clippy::mutable_key_type)]
fn postgres_query(
    client: &mut PostgresClient,
    sql: &str,
    params: &[SqlParam],
) -> Result<SqlResultSet, SqlFailure> {
    let stmt = client
        .prepare(sql)
        .map_err(|e| SqlFailure::postgres("postgres query prepare failed", e))?;
    let param_storage: Vec<Box<dyn ToSql + Sync>> =
        params.iter().map(sql_param_to_postgres).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = param_storage
        .iter()
        .map(|p| p.as_ref() as &(dyn ToSql + Sync))
        .collect();
    let rows = client
        .query(&stmt, &refs)
        .map_err(|e| SqlFailure::postgres("postgres query failed", e))?;

    let columns: Vec<String> = stmt
        .columns()
        .iter()
        .map(|col| col.name().to_string())
        .collect();
    let mut ordered_rows = Vec::new();
    for row in rows {
        let mut values = Vec::with_capacity(stmt.columns().len());
        for (idx, col) in stmt.columns().iter().enumerate() {
            values.push(postgres_query_value(&row, idx, col.type_())?);
        }
        ordered_rows.push(materialize_row("postgres", &columns, values).map_err(SqlFailure::from)?);
    }

    Ok(SqlResultSet {
        ordered_rows,
        columns,
        next_ordered_index: 0,
        closed: false,
        sqlite_cursor: None,
        postgres_cursor: None,
        mysql_cursor: None,
        sqlserver_cursor: None,
        connection_lease: None,
        transaction_lease: None,
        pool_lease: None,
    })
}

/// Prepare a PostgreSQL query and leave its wire row stream owned by the
/// result set. `query_raw` has already sent the statement and parameters when
/// it returns; the returned iterator only borrows the client's live protocol
/// connection while it fetches subsequent rows.
fn postgres_query_streaming(
    client: &mut PostgresClient,
    sql: &str,
    params: &[SqlParam],
) -> Result<SqlResultSet, SqlFailure> {
    let statement = client
        .prepare(sql)
        .map_err(|error| SqlFailure::postgres("postgres query prepare failed", error))?;
    let columns: Vec<String> = statement
        .columns()
        .iter()
        .map(|column| column.name().to_string())
        .collect();
    let types: Vec<PgType> = statement
        .columns()
        .iter()
        .map(|column| column.type_().clone())
        .collect();
    let param_storage: Vec<Box<dyn ToSql + Sync>> =
        params.iter().map(sql_param_to_postgres).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = param_storage
        .iter()
        .map(|param| param.as_ref() as &(dyn ToSql + Sync))
        .collect();
    let client_ptr = client as *mut PostgresClient;
    let iterator = client
        .query_raw(&statement, refs.into_iter())
        .map_err(|error| SqlFailure::postgres("postgres query failed", error))?;
    // `SqlConnection::Postgres` stores the client in a Box. The connection
    // lease prevents the Box from being moved or dropped until this iterator
    // is gone, so extending the driver's borrow is valid for this thread-local
    // result-set ownership model.
    let iterator = unsafe {
        std::mem::transmute::<postgres::RowIter<'_>, postgres::RowIter<'static>>(iterator)
    };
    Ok(SqlResultSet {
        ordered_rows: Vec::new(),
        columns,
        next_ordered_index: 0,
        closed: false,
        sqlite_cursor: None,
        postgres_cursor: Some(PostgresCursor {
            client: client_ptr,
            iter: iterator,
            types,
        }),
        mysql_cursor: None,
        sqlserver_cursor: None,
        connection_lease: None,
        transaction_lease: None,
        pool_lease: None,
    })
}

fn postgres_cursor_next(
    cursor: &mut PostgresCursor,
    columns: &[String],
) -> Result<Option<Value>, SqlFailure> {
    // Reading the pointer documents and enforces that the cursor's client
    // anchor is part of its live state; the connection lease owns the actual
    // client and keeps this address valid while `iter` is active.
    let _client = cursor.client;
    let Some(row) = postgres::fallible_iterator::FallibleIterator::next(&mut cursor.iter)
        .map_err(|error| SqlFailure::postgres("postgres row read failed", error))?
    else {
        return Ok(None);
    };
    let mut values = Vec::with_capacity(cursor.types.len());
    for (index, pg_type) in cursor.types.iter().enumerate() {
        values.push(postgres_query_value(&row, index, pg_type)?);
    }
    Ok(Some(
        materialize_row("postgres", columns, values).map_err(SqlFailure::from)?,
    ))
}

fn resultset_next_row(resultset: &mut SqlResultSet) -> Result<Option<Value>, SqlFailure> {
    if let Some(cursor) = resultset.sqlite_cursor.as_mut() {
        return sqlite_cursor_next(cursor, &resultset.columns);
    }
    if let Some(cursor) = resultset.postgres_cursor.as_mut() {
        return postgres_cursor_next(cursor, &resultset.columns);
    }
    if let Some(cursor) = resultset.mysql_cursor.as_mut() {
        return mysql_cursor_next(cursor, &resultset.columns);
    }
    if let Some(cursor) = resultset.sqlserver_cursor.as_mut() {
        return sqlserver_cursor_next(cursor, &mut resultset.columns);
    }
    if resultset.next_ordered_index < resultset.ordered_rows.len() {
        let row = resultset.ordered_rows[resultset.next_ordered_index].clone();
        resultset.next_ordered_index += 1;
        return Ok(Some(row));
    }
    Ok(None)
}

fn resultset_has_active_cursor(resultset: &SqlResultSet) -> bool {
    resultset.sqlite_cursor.is_some()
        || resultset.postgres_cursor.is_some()
        || resultset.mysql_cursor.is_some()
        || resultset.sqlserver_cursor.is_some()
}

/// Run a PostgreSQL query with a cooperative cancellation token.
///
/// PostgreSQL's wire protocol has a dedicated cancel request.  The synchronous
/// `postgres` client exposes that request through `CancelToken`, which can be
/// sent from a helper thread while the owning client remains borrowed by the
/// query.  The helper is joined before this function returns so a late cancel
/// request can never outlive the operation and affect a subsequent query.
fn postgres_query_with_cancellation(
    client: &mut PostgresClient,
    sql: &str,
    params: &[SqlParam],
    token: &std::sync::Arc<crate::sync_primitives::CancellationEntry>,
) -> Result<SqlResultSet, SqlFailure> {
    if cancellation_entry_is_cancelled(token) {
        return Err(SqlFailure::plain("SQL query was cancelled").cancelled());
    }

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);
    let interrupted_worker = Arc::clone(&interrupted);
    let token_worker = Arc::clone(token);
    let cancel_token = client.cancel_token();
    let worker = thread::Builder::new()
        .name("mux-postgres-cancel".to_string())
        .spawn(move || {
            while !stop_worker.load(Ordering::Acquire) {
                if cancellation_entry_is_cancelled(&token_worker) {
                    interrupted_worker.store(true, Ordering::Release);
                    // A cancellation request is inherently racy.  The query
                    // result remains authoritative; this call only asks the
                    // server to interrupt the current operation.
                    let _ = cancel_token.cancel_query(NoTls);
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        })
        .map_err(|error| {
            SqlFailure::plain(format!("could not start SQL cancellation worker: {error}"))
        })?;

    let result = postgres_query(client, sql, params);
    stop.store(true, Ordering::Release);
    let _ = worker.join();

    match result {
        Err(error)
            if interrupted.load(Ordering::Acquire) && error.diagnostic.sqlstate == "57014" =>
        {
            Err(error.cancelled())
        }
        other => other,
    }
}

/// Run a PostgreSQL query until its deadline, using PostgreSQL's wire-level
/// cancel request to interrupt the server-side operation.
///
/// `postgres` exposes cancellation through a token that is independent of the
/// borrowed client, so a short-lived helper can issue the request while the
/// synchronous query is blocked. The helper is always joined before this
/// function returns. This matters for pooled and transaction connections: a
/// cancellation request that outlived the operation could otherwise race with
/// the next query on the same backend connection.
fn postgres_query_with_timeout(
    client: &mut PostgresClient,
    sql: &str,
    params: &[SqlParam],
    timeout_ms: i64,
) -> Result<SqlResultSet, SqlFailure> {
    let milliseconds = u64::try_from(timeout_ms)
        .map_err(|_| SqlFailure::plain("SQL query timeout must be non-negative"))?;
    let timeout = Duration::from_millis(milliseconds);
    // Keep large, valid timeout values large. Adding them to `Instant` can
    // overflow the platform's representable clock range, while elapsed-time
    // comparison remains well-defined for the complete `i64` API range.
    let started = std::time::Instant::now();

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);
    let interrupted_worker = Arc::clone(&interrupted);
    let cancel_token = client.cancel_token();
    let worker = thread::Builder::new()
        .name("mux-postgres-timeout".to_string())
        .spawn(move || {
            while !stop_worker.load(Ordering::Acquire) {
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    interrupted_worker.store(true, Ordering::Release);
                    // The query result remains authoritative. The flag only
                    // classifies SQLSTATE 57014 when this request interrupted
                    // the operation rather than a user-issued cancel.
                    let _ = cancel_token.cancel_query(NoTls);
                    return;
                }
                let remaining = timeout.saturating_sub(elapsed);
                thread::sleep(remaining.min(Duration::from_millis(5)));
            }
        })
        .map_err(|error| {
            SqlFailure::plain(format!("could not start SQL timeout worker: {error}"))
        })?;

    let result = postgres_query(client, sql, params);
    stop.store(true, Ordering::Release);
    let _ = worker.join();

    match result {
        Err(error)
            if interrupted.load(Ordering::Acquire) && error.diagnostic.sqlstate == "57014" =>
        {
            Err(error.timeout())
        }
        other => other,
    }
}

fn postgres_execute(
    client: &mut PostgresClient,
    sql: &str,
    params: &[SqlParam],
) -> Result<i64, SqlFailure> {
    let stmt = client
        .prepare(sql)
        .map_err(|e| SqlFailure::postgres("postgres execute prepare failed", e))?;
    let param_storage: Vec<Box<dyn ToSql + Sync>> =
        params.iter().map(sql_param_to_postgres).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = param_storage
        .iter()
        .map(|p| p.as_ref() as &(dyn ToSql + Sync))
        .collect();
    let affected = client
        .execute(&stmt, &refs)
        .map_err(|e| SqlFailure::postgres("postgres execute failed", e))?;
    i64::try_from(affected).map_err(|_| SqlFailure::plain("affected row count overflowed int"))
}

fn mysql_value_to_mux(value: MySqlValue, binary: bool) -> Value {
    match value {
        MySqlValue::NULL => Value::Unit,
        MySqlValue::Int(v) => Value::Int(v),
        MySqlValue::UInt(v) => i64::try_from(v)
            .map(Value::Int)
            .unwrap_or(Value::String(v.to_string())),
        MySqlValue::Float(v) => Value::Float(ordered_float::OrderedFloat(f64::from(v))),
        MySqlValue::Double(v) => Value::Float(ordered_float::OrderedFloat(v)),
        // MySQL represents both text and binary columns as `Bytes` on the
        // wire. Column metadata is authoritative: a binary column must stay
        // bytes even when its contents happen to be valid UTF-8 (for example,
        // a BLOB containing `b"hello"`). Text columns retain the existing
        // UTF-8 conversion, with invalid text preserved as bytes.
        MySqlValue::Bytes(bytes) if binary => Value::Bytes(bytes),
        MySqlValue::Bytes(bytes) => match String::from_utf8(bytes.clone()) {
            Ok(text) => Value::String(text),
            Err(_) => Value::Bytes(bytes),
        },
        MySqlValue::Date(year, month, day, hour, min, sec, micros) => Value::String(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}.{micros:06}"
        )),
        MySqlValue::Time(is_neg, days, hours, mins, secs, micros) => {
            let sign = if is_neg { "-" } else { "" };
            Value::String(format!(
                "{sign}{days} {hours:02}:{mins:02}:{secs:02}.{micros:06}"
            ))
        }
    }
}

fn mysql_column_is_binary(column: &mysql::Column) -> bool {
    // `BINARY_FLAG` is the normal signal for BINARY/VARBINARY/BLOB values.
    // The protocol also identifies the binary character set as 63; retain
    // that fallback for servers or proxies that omit the flag on a BLOB.
    column.flags().contains(ColumnFlags::BINARY_FLAG) || column.character_set() == 63
}

#[allow(clippy::mutable_key_type)]
fn mysql_query(
    conn: &mut MySqlConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<SqlResultSet, SqlFailure> {
    let mysql_params = MySqlParams::Positional(params.iter().map(sql_param_to_mysql).collect());
    let result = conn
        .exec_iter(sql, mysql_params)
        .map_err(|e| SqlFailure::mysql("mysql query failed", e))?;
    let columns: Vec<String> = result
        .columns()
        .as_ref()
        .iter()
        .map(|col| col.name_str().to_string())
        .collect();
    let binary_columns: Vec<bool> = result
        .columns()
        .as_ref()
        .iter()
        .map(mysql_column_is_binary)
        .collect();

    let mut ordered_rows = Vec::new();
    for row in result {
        let mut row = row.map_err(|e| SqlFailure::mysql("mysql row read failed", e))?;
        let raw_values: Vec<MySqlValue> = (0..columns.len())
            .map(|idx| row.take(idx).unwrap_or(MySqlValue::NULL))
            .collect();
        let mut values = Vec::with_capacity(columns.len());
        for (raw, binary) in raw_values.into_iter().zip(binary_columns.iter().copied()) {
            values.push(mysql_value_to_mux(raw, binary));
        }
        ordered_rows.push(materialize_row("mysql", &columns, values).map_err(SqlFailure::from)?);
    }

    Ok(SqlResultSet {
        ordered_rows,
        columns,
        next_ordered_index: 0,
        closed: false,
        sqlite_cursor: None,
        postgres_cursor: None,
        mysql_cursor: None,
        sqlserver_cursor: None,
        connection_lease: None,
        transaction_lease: None,
        pool_lease: None,
    })
}

/// Execute one MySQL result set without materializing all rows. MySQL's
/// `QueryResult` owns the packet cursor and drains any unread packets on drop,
/// so the leased connection remains unavailable until this cursor reaches EOF
/// or the result set is explicitly closed.
fn mysql_query_streaming(
    conn: &mut MySqlConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<SqlResultSet, SqlFailure> {
    let statements = split_sql_batch(sql, SqlBackend::MySql).map_err(SqlFailure::plain)?;
    if statements.len() != 1 {
        return Err(SqlFailure::plain(
            "MySQL queries must contain exactly one statement",
        ));
    }
    let mysql_params = MySqlParams::Positional(params.iter().map(sql_param_to_mysql).collect());
    let connection = conn as *mut MySqlConnection;
    let result = conn
        .exec_iter(sql, mysql_params)
        .map_err(|error| SqlFailure::mysql("mysql query failed", error))?;
    let columns: Vec<String> = result
        .columns()
        .as_ref()
        .iter()
        .map(|column| column.name_str().to_string())
        .collect();
    let binary_columns: Vec<bool> = result
        .columns()
        .as_ref()
        .iter()
        .map(mysql_column_is_binary)
        .collect();
    let result = unsafe {
        std::mem::transmute::<
            mysql::QueryResult<'_, '_, '_, mysql::Binary>,
            mysql::QueryResult<'static, 'static, 'static, mysql::Binary>,
        >(result)
    };
    Ok(SqlResultSet {
        ordered_rows: Vec::new(),
        columns,
        next_ordered_index: 0,
        closed: false,
        sqlite_cursor: None,
        postgres_cursor: None,
        mysql_cursor: Some(MySqlCursor {
            connection,
            result,
            binary_columns,
        }),
        sqlserver_cursor: None,
        connection_lease: None,
        transaction_lease: None,
        pool_lease: None,
    })
}

fn mysql_query_with_interrupt(
    conn: &mut MySqlConnection,
    sql: &str,
    params: &[SqlParam],
    timeout: Option<Duration>,
    token: Option<Arc<crate::sync_primitives::CancellationEntry>>,
) -> Result<SqlResultSet, SqlFailure> {
    if token.as_ref().is_some_and(cancellation_entry_is_cancelled) {
        return Err(SqlFailure::plain("MySQL query was cancelled").cancelled());
    }
    if timeout.is_some_and(|limit| limit.is_zero()) {
        return Err(SqlFailure::plain("MySQL query timed out").timeout());
    }

    let started = Instant::now();
    let controller =
        MySqlInterruptController::start(&conn.opts, conn.connection_id(), started, timeout, token)?;
    let result = mysql_query(conn, sql, params);
    let (reason, kill_error) = controller.finish();
    match result {
        Err(error) => {
            if let Some(kill_error) = kill_error {
                return Err(kill_error);
            }
            if mysql_interrupted_error(&error) {
                if let Some(reason) = reason {
                    return Err(match reason {
                        MySqlInterruptReason::Timeout => error.timeout(),
                        MySqlInterruptReason::Cancellation => error.cancelled(),
                    });
                }
            }
            Err(error)
        }
        Ok(resultset) => {
            if let Some(kill_error) = kill_error {
                return Err(kill_error);
            }
            if let Some(reason) = reason {
                return Err(match reason {
                    MySqlInterruptReason::Timeout => {
                        SqlFailure::plain("MySQL query timed out").timeout()
                    }
                    MySqlInterruptReason::Cancellation => {
                        SqlFailure::plain("MySQL query was cancelled").cancelled()
                    }
                });
            }
            Ok(resultset)
        }
    }
}

fn mysql_cursor_next(
    cursor: &mut MySqlCursor,
    columns: &[String],
) -> Result<Option<Value>, SqlFailure> {
    let _connection = cursor.connection;
    let Some(row) = cursor.result.next() else {
        return Ok(None);
    };
    let mut row = row.map_err(|error| SqlFailure::mysql("mysql row read failed", error))?;
    let raw_values: Vec<MySqlValue> = (0..cursor.binary_columns.len())
        .map(|index| row.take(index).unwrap_or(MySqlValue::NULL))
        .collect();
    let values = raw_values
        .into_iter()
        .zip(cursor.binary_columns.iter().copied())
        .map(|(value, binary)| mysql_value_to_mux(value, binary))
        .collect();
    Ok(Some(
        materialize_row("mysql", columns, values).map_err(SqlFailure::from)?,
    ))
}

fn mysql_interrupted_error(failure: &SqlFailure) -> bool {
    failure.diagnostic.vendor_code == "1317" || failure.diagnostic.sqlstate == "70100"
}

fn mysql_execute(
    conn: &mut MySqlConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<i64, SqlFailure> {
    let mysql_params = MySqlParams::Positional(params.iter().map(sql_param_to_mysql).collect());
    conn.exec_drop(sql, mysql_params)
        .map_err(|e| SqlFailure::mysql("mysql execute failed", e))?;
    i64::try_from(conn.affected_rows())
        .map_err(|_| SqlFailure::plain("affected row count overflowed int"))
}

fn normalize_isolation(value: &str) -> Result<&'static str, String> {
    match value {
        "default" => Ok("default"),
        "read_uncommitted" => Ok("read uncommitted"),
        "read_committed" => Ok("read committed"),
        "repeatable_read" => Ok("repeatable read"),
        "serializable" => Ok("serializable"),
        _ => Err(format!(
            "unsupported transaction isolation '{value}'; use default, read_uncommitted, read_committed, repeatable_read, or serializable"
        )),
    }
}

fn begin_transaction_on_connection_with_options(
    connection: &mut SqlConnection,
    isolation: &str,
    read_only: bool,
    deferrable: bool,
) -> Result<(), SqlFailure> {
    let isolation = normalize_isolation(isolation).map_err(SqlFailure::plain)?;
    match connection {
        SqlConnection::Sqlite(conn) => {
            if isolation != "default" || read_only || deferrable {
                return Err(SqlFailure::plain(
                    "SQLite transaction options support only default isolation and read-write, non-deferrable transactions",
                ));
            }
            conn.execute_batch("BEGIN TRANSACTION")
                .map_err(|e| SqlFailure::sqlite("begin transaction failed", e))
        }
        SqlConnection::Postgres(client) => {
            if deferrable && !(isolation == "serializable" && read_only) {
                return Err(SqlFailure::plain(
                    "PostgreSQL deferrable transactions must be serializable and read-only",
                ));
            }
            let mut sql = String::from("BEGIN");
            if isolation != "default" {
                sql.push_str(" ISOLATION LEVEL ");
                sql.push_str(isolation);
            }
            if read_only {
                sql.push_str(" READ ONLY");
            }
            if deferrable {
                sql.push_str(" DEFERRABLE");
            }
            client
                .batch_execute(&sql)
                .map_err(|e| SqlFailure::postgres("begin transaction failed", e))
        }
        SqlConnection::MySql(conn) => {
            if deferrable {
                return Err(SqlFailure::plain(
                    "MySQL does not support deferrable transactions",
                ));
            }
            if isolation != "default" {
                let sql = format!(
                    "SET TRANSACTION ISOLATION LEVEL {}",
                    isolation.to_uppercase()
                );
                conn.query_drop(sql)
                    .map_err(|e| SqlFailure::mysql("set transaction isolation failed", e))?;
            }
            let sql = if read_only {
                "START TRANSACTION READ ONLY"
            } else {
                "START TRANSACTION"
            };
            conn.query_drop(sql)
                .map_err(|e| SqlFailure::mysql("begin transaction failed", e))
        }
        SqlConnection::SqlServer(conn) => {
            if read_only || deferrable {
                return Err(SqlFailure::unsupported(
                    "SQL Server does not support read-only or deferrable transaction options",
                ));
            }
            let mut statement = String::new();
            if isolation != "default" {
                statement.push_str("SET TRANSACTION ISOLATION LEVEL ");
                statement.push_str(&isolation.to_uppercase());
                statement.push(';');
            }
            statement.push_str("BEGIN TRANSACTION");
            sqlserver_simple_query(conn, &statement)
        }
    }
}

fn begin_transaction_on_connection(connection: &mut SqlConnection) -> Result<(), SqlFailure> {
    begin_transaction_on_connection_with_options(connection, "default", false, false)
}

fn commit_connection(connection: &mut SqlConnection) -> Result<(), SqlFailure> {
    match connection {
        SqlConnection::Sqlite(conn) => conn
            .execute_batch("COMMIT")
            .map_err(|e| SqlFailure::sqlite("commit failed", e)),
        SqlConnection::Postgres(client) => client
            .batch_execute("COMMIT")
            .map_err(|e| SqlFailure::postgres("commit failed", e)),
        SqlConnection::MySql(conn) => conn
            .query_drop("COMMIT")
            .map_err(|e| SqlFailure::mysql("commit failed", e)),
        SqlConnection::SqlServer(conn) => sqlserver_simple_query(conn, "COMMIT TRANSACTION")
            .map_err(|error| SqlFailure {
                detail: format!("commit failed: {}", error.detail),
                ..error
            }),
    }
}

fn migration_commit_connection(connection: &mut SqlConnection) -> Result<(), SqlFailure> {
    match connection {
        SqlConnection::Sqlite(conn) => conn
            .execute_batch("COMMIT")
            .map_err(|error| SqlFailure::sqlite("migration commit failed", error)),
        SqlConnection::Postgres(client) => client
            .batch_execute("COMMIT")
            .map_err(|error| SqlFailure::postgres("migration commit failed", error)),
        SqlConnection::MySql(conn) => conn
            .query_drop("COMMIT")
            .map_err(|error| SqlFailure::mysql("migration commit failed", error)),
        SqlConnection::SqlServer(conn) => sqlserver_simple_query(conn, "COMMIT TRANSACTION")
            .map_err(|error| SqlFailure {
                detail: format!("migration commit failed: {}", error.detail),
                ..error
            }),
    }
}

fn rollback_connection(connection: &mut SqlConnection) -> Result<(), SqlFailure> {
    match connection {
        SqlConnection::Sqlite(conn) => conn
            .execute_batch("ROLLBACK")
            .map_err(|e| SqlFailure::sqlite("rollback failed", e)),
        SqlConnection::Postgres(client) => client
            .batch_execute("ROLLBACK")
            .map_err(|e| SqlFailure::postgres("rollback failed", e)),
        SqlConnection::MySql(conn) => conn
            .query_drop("ROLLBACK")
            .map_err(|e| SqlFailure::mysql("rollback failed", e)),
        SqlConnection::SqlServer(conn) => sqlserver_simple_query(conn, "ROLLBACK TRANSACTION")
            .map_err(|error| SqlFailure {
                detail: format!("rollback failed: {}", error.detail),
                ..error
            }),
    }
}

fn migration_rollback_connection(connection: &mut SqlConnection) -> Result<(), SqlFailure> {
    match connection {
        SqlConnection::Sqlite(conn) => conn
            .execute_batch("ROLLBACK")
            .map_err(|error| SqlFailure::sqlite("migration rollback failed", error)),
        SqlConnection::Postgres(client) => client
            .batch_execute("ROLLBACK")
            .map_err(|error| SqlFailure::postgres("migration rollback failed", error)),
        SqlConnection::MySql(conn) => conn
            .query_drop("ROLLBACK")
            .map_err(|error| SqlFailure::mysql("migration rollback failed", error)),
        SqlConnection::SqlServer(conn) => sqlserver_simple_query(conn, "ROLLBACK TRANSACTION")
            .map_err(|error| SqlFailure {
                detail: format!("migration rollback failed: {}", error.detail),
                ..error
            }),
    }
}

fn finish_transaction_backend(
    connection: &mut SqlConnection,
    parent: Option<&(i64, String)>,
    commit: bool,
) -> Result<(), SqlFailure> {
    if let Some((_, name)) = parent {
        if let SqlConnection::SqlServer(connection) = connection {
            if commit {
                return Ok(());
            }
            return sqlserver_simple_query(connection, &format!("ROLLBACK TRANSACTION {name}"));
        }
        if !commit {
            execute_batch_on_connection(connection, &format!("ROLLBACK TO SAVEPOINT {name}"))?;
        }
        execute_batch_on_connection(connection, &format!("RELEASE SAVEPOINT {name}"))
    } else if commit {
        commit_connection(connection)
    } else {
        rollback_connection(connection)
    }
}

fn restore_transaction_connection(tx: &SqlTransaction, connection: SqlConnection) {
    if let Some((parent, _)) = &tx.parent {
        SQL_TRANSACTIONS.with(|transactions| {
            if let Some(parent) = transactions.borrow_mut().get_mut(parent) {
                parent.connection = Some(connection);
            }
        });
    } else {
        return_connection(tx.connection_handle, connection);
    }
}

fn finish_transaction(handle: i64, commit: bool) -> Result<(), SqlFailure> {
    if transaction_has_resultset_lease(handle) {
        return Err(SqlFailure::plain(
            "sql transaction is busy while a result set is open",
        ));
    }
    let mut tx = take_transaction(handle)
        .ok_or_else(|| SqlFailure::plain("invalid sql transaction handle"))?;
    let Some(mut connection) = tx.connection.take() else {
        SQL_TRANSACTIONS.with(|transactions| transactions.borrow_mut().insert(handle, tx));
        return Err(SqlFailure::plain(
            "transaction has an active nested transaction",
        ));
    };
    if let Err(error) = finish_transaction_backend(&mut connection, tx.parent.as_ref(), commit) {
        tx.connection = Some(connection);
        SQL_TRANSACTIONS.with(|transactions| transactions.borrow_mut().insert(handle, tx));
        return Err(error);
    }
    restore_transaction_connection(&tx, connection);
    unsafe { mux_rc_dec(tx.owner) };
    Ok(())
}

/// Begins a savepoint-backed child transaction, suspending its parent until completion.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_begin_transaction(transaction: *mut Value) -> *mut Value {
    let provider = transaction_handle(transaction).map_or("unknown", transaction_provider);
    let result = transaction_handle(transaction).and_then(|parent| {
        with_transaction(parent, |tx| {
            let connection = tx
                .connection
                .as_mut()
                .ok_or_else(|| "transaction has an active nested transaction".to_string())?;
            let name = format!("mux_nested_{}", NEXT_HANDLE.fetch_add(1, Ordering::Relaxed));
            if let SqlConnection::SqlServer(connection) = connection {
                sqlserver_simple_query(connection, &format!("SAVE TRANSACTION {name}"))?;
            } else {
                execute_batch_on_connection(connection, &format!("SAVEPOINT {name}"))?;
            }
            unsafe { mux_rc_inc(transaction) };
            Ok(SqlTransaction {
                connection_handle: tx.connection_handle,
                provider,
                connection: tx.connection.take(),
                active: true,
                parent: Some((parent, name)),
                owner: transaction,
            })
        })
    });
    match result {
        Ok(tx) => sql_result_handle(store_transaction(tx), *SQL_TRANSACTION_TYPE_ID),
        Err(error) => {
            sql_result_err_context(SqlErrorKind::Database, provider, "begin_transaction", error)
        }
    }
}

fn connection_execute_with_params(
    handle: i64,
    sql: &str,
    params: &[SqlParam],
) -> Result<i64, SqlFailure> {
    with_connection(handle, |connection| {
        execute_on_connection(connection, sql, params)
    })
}

fn execute_on_connection(
    connection: &mut SqlConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<i64, SqlFailure> {
    match connection {
        SqlConnection::Sqlite(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Sqlite, params.len())?;
            sqlite_execute(conn, &sql, params)
        }
        SqlConnection::Postgres(client) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Postgres, params.len())?;
            postgres_execute(client, &sql, params)
        }
        SqlConnection::MySql(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::MySql, params.len())?;
            mysql_execute(conn, &sql, params)
        }
        SqlConnection::SqlServer(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::SqlServer, params.len())?;
            sqlserver_execute(conn, &sql, params)
        }
    }
}

fn execute_many_on_connection(
    connection: &mut SqlConnection,
    sql: &str,
    rows: &[Vec<SqlParam>],
) -> Result<i64, SqlFailure> {
    let backend = match connection {
        SqlConnection::Sqlite(_) => SqlBackend::Sqlite,
        SqlConnection::Postgres(_) => SqlBackend::Postgres,
        SqlConnection::MySql(_) => SqlBackend::MySql,
        SqlConnection::SqlServer(_) => SqlBackend::SqlServer,
    };
    let statements = split_sql_batch(sql, backend).map_err(SqlFailure::plain)?;
    if statements.len() != 1 {
        return Err(SqlFailure::plain(
            "SQL execute_many requires exactly one statement",
        ));
    }
    let statement = &statements[0];
    let expected_params = rows.first().map_or(0, Vec::len);
    for (row_index, params) in rows.iter().enumerate() {
        if params.len() != expected_params {
            return Err(SqlFailure::plain(format!(
                "SQL execute_many row {row_index} has {} parameter(s), expected {expected_params}",
                params.len()
            )));
        }
    }
    // Validate and rewrite the statement before executing any row. The
    // rewritten SQL must be the one sent to the provider: `?` is portable
    // input, but PostgreSQL requires `$n`, while SQLite/MySQL use `?` for the
    // normalized form. Previously this validation result was discarded and
    // execute_many sent the caller's spelling unchanged.
    let rewritten =
        rewrite_positional_sql(statement, backend, expected_params).map_err(SqlFailure::plain)?;
    match connection {
        SqlConnection::Sqlite(connection) => sqlite_execute_many(connection, &rewritten, rows),
        SqlConnection::Postgres(connection) => postgres_execute_many(connection, &rewritten, rows),
        SqlConnection::MySql(connection) => mysql_execute_many(connection, &rewritten, rows),
        SqlConnection::SqlServer(connection) => {
            let mut total = 0;
            for params in rows {
                add_affected_rows(
                    &mut total,
                    sqlserver_execute(connection, &rewritten, params)?,
                )?;
            }
            Ok(total)
        }
    }
}

fn add_affected_rows(total: &mut i64, affected: i64) -> Result<(), SqlFailure> {
    *total = total
        .checked_add(affected)
        .ok_or_else(|| SqlFailure::plain("SQL execute_many affected row count overflowed int"))?;
    Ok(())
}

fn sqlite_execute_many(
    connection: &mut SqliteConnection,
    sql: &str,
    rows: &[Vec<SqlParam>],
) -> Result<i64, SqlFailure> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| SqlFailure::sqlite("sqlite execute_many prepare failed", error))?;
    let mut total = 0i64;
    for params in rows {
        let sqlite_params: Vec<SqliteValue> = params.iter().map(sql_param_to_sqlite).collect();
        let affected = statement
            .execute(params_from_iter(sqlite_params.iter()))
            .map_err(|error| SqlFailure::sqlite("sqlite execute_many failed", error))?;
        add_affected_rows(
            &mut total,
            i64::try_from(affected)
                .map_err(|_| SqlFailure::plain("affected row count overflowed int"))?,
        )?;
    }
    Ok(total)
}

fn postgres_execute_many(
    connection: &mut PostgresClient,
    sql: &str,
    rows: &[Vec<SqlParam>],
) -> Result<i64, SqlFailure> {
    let statement = connection
        .prepare(sql)
        .map_err(|error| SqlFailure::postgres("postgres execute_many prepare failed", error))?;
    let mut total = 0i64;
    for params in rows {
        let storage: Vec<Box<dyn ToSql + Sync>> =
            params.iter().map(sql_param_to_postgres).collect();
        let refs: Vec<&(dyn ToSql + Sync)> = storage
            .iter()
            .map(|param| param.as_ref() as &(dyn ToSql + Sync))
            .collect();
        let affected = connection
            .execute(&statement, &refs)
            .map_err(|error| SqlFailure::postgres("postgres execute_many failed", error))?;
        add_affected_rows(
            &mut total,
            i64::try_from(affected)
                .map_err(|_| SqlFailure::plain("affected row count overflowed int"))?,
        )?;
    }
    Ok(total)
}

fn mysql_execute_many(
    connection: &mut MySqlConnection,
    sql: &str,
    rows: &[Vec<SqlParam>],
) -> Result<i64, SqlFailure> {
    let statement = connection
        .prep(sql)
        .map_err(|error| SqlFailure::mysql("mysql execute_many prepare failed", error))?;
    let mut total = 0i64;
    for params in rows {
        let mysql_params = MySqlParams::Positional(params.iter().map(sql_param_to_mysql).collect());
        connection
            .exec_drop(&statement, mysql_params)
            .map_err(|error| SqlFailure::mysql("mysql execute_many failed", error))?;
        add_affected_rows(
            &mut total,
            i64::try_from(connection.affected_rows())
                .map_err(|_| SqlFailure::plain("affected row count overflowed int"))?,
        )?;
    }
    Ok(total)
}

struct AppliedMigration {
    name: String,
    checksum: String,
}

fn query_on_connection(connection: &mut SqlConnection, sql: &str) -> Result<SqlResultSet, String> {
    query_on_connection_params(connection, sql, &[])
}

fn query_on_connection_params(
    connection: &mut SqlConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<SqlResultSet, String> {
    match connection {
        SqlConnection::Sqlite(connection) => {
            sqlite_query(connection, sql, params).map_err(String::from)
        }
        SqlConnection::Postgres(connection) => {
            postgres_query(connection, sql, params).map_err(String::from)
        }
        SqlConnection::MySql(connection) => {
            mysql_query(connection, sql, params).map_err(String::from)
        }
        SqlConnection::SqlServer(connection) => {
            sqlserver_query_materialized(connection, sql, params).map_err(String::from)
        }
    }
}

fn sql_row_values(value: &Value) -> Result<Vec<Value>, String> {
    let handle = row_handle(value as *const Value)?;
    SQL_ROWS.with(|rows| {
        rows.borrow()
            .get(&handle)
            .map(|row| row.values.clone())
            .ok_or_else(|| "migration status row is no longer available".to_string())
    })
}

fn applied_migrations(
    connection: &mut SqlConnection,
    table: &str,
) -> Result<HashMap<i64, AppliedMigration>, SqlFailure> {
    let sql = format!("SELECT version, name, checksum FROM {table} ORDER BY version");
    let resultset = query_on_connection(connection, &sql)?;
    let mut applied = HashMap::new();
    for row in &resultset.ordered_rows {
        let values = sql_row_values(row)?;
        if values.len() != 3 {
            return Err(SqlFailure::plain(
                "migration metadata table has an invalid row shape",
            ));
        }
        let Value::Int(version) = values[0] else {
            return Err(SqlFailure::plain(
                "migration metadata version is not an integer",
            ));
        };
        let Value::String(name) = &values[1] else {
            return Err(SqlFailure::plain("migration metadata name is not a string"));
        };
        let Value::String(checksum) = &values[2] else {
            return Err(SqlFailure::plain(
                "migration metadata checksum is not a string",
            ));
        };
        if version > 0
            && applied
                .insert(
                    version,
                    AppliedMigration {
                        name: name.clone(),
                        checksum: checksum.clone(),
                    },
                )
                .is_some()
        {
            return Err(SqlFailure::plain(format!(
                "duplicate applied migration version {version}"
            )));
        }
    }
    Ok(applied)
}

fn applied_migrations_if_present(
    connection: &mut SqlConnection,
    table: &str,
) -> Result<HashMap<i64, AppliedMigration>, SqlFailure> {
    if !migration_table_exists(connection, table)? {
        return Ok(HashMap::new());
    }
    applied_migrations(connection, table)
}

fn migration_table_exists(connection: &mut SqlConnection, table: &str) -> Result<bool, SqlFailure> {
    let (sql, params) = match connection {
        SqlConnection::Sqlite(_) => (
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
            vec![SqlParam::String(table.to_string())],
        ),
        SqlConnection::Postgres(_) => (
            "SELECT 1 FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = $1 LIMIT 1",
            vec![SqlParam::String(table.to_string())],
        ),
        SqlConnection::MySql(_) => (
            "SELECT 1 FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = ? LIMIT 1",
            vec![SqlParam::String(table.to_string())],
        ),
        SqlConnection::SqlServer(_) => (
            "SELECT 1 FROM sys.tables WHERE name = @P1",
            vec![SqlParam::String(table.to_string())],
        ),
    };
    Ok(!query_on_connection_params(connection, sql, &params)?
        .ordered_rows
        .is_empty())
}

fn migration_table_sql(table: &str, connection: &SqlConnection) -> String {
    match connection {
        SqlConnection::SqlServer(_) => format!(
            "IF OBJECT_ID(N'{table}', N'U') IS NULL BEGIN CREATE TABLE {table} (version BIGINT PRIMARY KEY, name NVARCHAR(128) NOT NULL, checksum NVARCHAR(64) NOT NULL) END"
        ),
        SqlConnection::Sqlite(_) => format!(
            "CREATE TABLE IF NOT EXISTS {table} (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL)"
        ),
        SqlConnection::Postgres(_) => format!(
            "CREATE TABLE IF NOT EXISTS {table} (version BIGINT PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL)"
        ),
        SqlConnection::MySql(_) => format!(
            "CREATE TABLE IF NOT EXISTS {table} (version BIGINT PRIMARY KEY, name VARCHAR(128) NOT NULL, checksum VARCHAR(64) NOT NULL)"
        ),
    }
}

fn ensure_migration_table(connection: &mut SqlConnection, table: &str) -> Result<(), SqlFailure> {
    let ddl = migration_table_sql(table, connection);
    execute_batch_on_connection(connection, &ddl)?;
    let lock_sql = match connection {
        SqlConnection::Sqlite(_) => {
            format!(
                "INSERT OR IGNORE INTO {table} (version, name, checksum) VALUES (0, '__lock__', 'lock')"
            )
        }
        SqlConnection::Postgres(_) => {
            format!(
                "INSERT INTO {table} (version, name, checksum) VALUES (0, '__lock__', 'lock') ON CONFLICT (version) DO NOTHING"
            )
        }
        SqlConnection::MySql(_) => {
            format!(
                "INSERT IGNORE INTO {table} (version, name, checksum) VALUES (0, '__lock__', 'lock')"
            )
        }
        SqlConnection::SqlServer(_) => format!(
            "IF NOT EXISTS (SELECT 1 FROM {table} WHERE version = 0) INSERT INTO {table} (version, name, checksum) VALUES (0, N'__lock__', N'lock')"
        ),
    };
    execute_batch_on_connection(connection, &lock_sql)
}

fn begin_migration_transaction(
    connection: &mut SqlConnection,
    table: &str,
) -> Result<(), SqlFailure> {
    match connection {
        SqlConnection::Sqlite(connection) => connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| SqlFailure::sqlite("begin migration transaction failed", error)),
        SqlConnection::Postgres(connection) => {
            connection.batch_execute("BEGIN").map_err(|error| {
                SqlFailure::postgres("begin migration transaction failed", error)
            })?;
            let lock = format!("SELECT version FROM {table} WHERE version = 0 FOR UPDATE");
            connection
                .batch_execute(&lock)
                .map_err(|error| SqlFailure::postgres("migration lock failed", error))
        }
        SqlConnection::MySql(connection) => {
            connection
                .query_drop("START TRANSACTION")
                .map_err(|error| SqlFailure::mysql("begin migration transaction failed", error))?;
            let lock = format!("SELECT version FROM {table} WHERE version = 0 FOR UPDATE");
            connection
                .query_drop(lock)
                .map_err(|error| SqlFailure::mysql("migration lock failed", error))
        }
        SqlConnection::SqlServer(connection) => {
            sqlserver_simple_query(connection, "BEGIN TRANSACTION")?;
            let lock =
                format!("SELECT version FROM {table} WITH (UPDLOCK, HOLDLOCK) WHERE version = 0");
            sqlserver_simple_query(connection, &lock).map_err(|error| SqlFailure {
                detail: format!("migration lock failed: {}", error.detail),
                ..error
            })
        }
    }
}

fn migration_insert_sql(table: &str) -> String {
    format!("INSERT INTO {table} (version, name, checksum) VALUES (?, ?, ?)")
}

fn migration_delete_sql(table: &str) -> String {
    format!("DELETE FROM {table} WHERE version = ?")
}

fn validate_applied_migrations(
    applied: &HashMap<i64, AppliedMigration>,
    migrations: &[MigrationDefinition],
) -> Result<(), String> {
    let local = migrations
        .iter()
        .map(|migration| (migration.version, migration))
        .collect::<HashMap<_, _>>();
    for (version, record) in applied {
        let Some(migration) = local.get(version) else {
            return Err(format!(
                "database contains migration {version}, but it is absent locally"
            ));
        };
        if record.name != migration.name || record.checksum != migration.checksum {
            return Err(format!("migration {version} changed after it was applied"));
        }
    }
    if let Some(&highest) = applied.keys().max() {
        for migration in migrations {
            if migration.version <= highest && !applied.contains_key(&migration.version) {
                return Err(format!(
                    "migration history is missing locally defined version {}",
                    migration.version
                ));
            }
        }
    }
    Ok(())
}

fn migration_status_value(
    migration: &MigrationDefinition,
    applied: Option<&AppliedMigration>,
) -> Value {
    let state = match applied {
        Some(record) if record.checksum == migration.checksum && record.name == migration.name => {
            "applied"
        }
        Some(_) => "modified",
        None => "pending",
    };
    let mut values = crate::ordered::OrderedMap::new();
    values.insert(
        Value::String("version".to_string()),
        Value::String(migration.version.to_string()),
    );
    values.insert(
        Value::String("name".to_string()),
        Value::String(migration.name.clone()),
    );
    values.insert(
        Value::String("state".to_string()),
        Value::String(state.to_string()),
    );
    values.insert(
        Value::String("checksum".to_string()),
        Value::String(migration.checksum.clone()),
    );
    Value::Map(values)
}

fn connection_execute_batch(handle: i64, sql: &str) -> Result<(), SqlFailure> {
    with_connection(handle, |connection| {
        execute_batch_on_connection(connection, sql)
    })
}

fn connection_execute_many(
    handle: i64,
    sql: &str,
    rows: &[Vec<SqlParam>],
) -> Result<i64, SqlFailure> {
    with_connection(handle, |connection| {
        execute_many_on_connection(connection, sql, rows)
    })
}

fn connection_query_with_params(
    handle: i64,
    sql: &str,
    params: &[SqlParam],
) -> Result<Value, SqlFailure> {
    let resultset = with_connection(handle, |connection| match connection {
        SqlConnection::Sqlite(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Sqlite, params.len())?;
            sqlite_query_streaming(conn, &sql, params)
        }
        SqlConnection::Postgres(client) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Postgres, params.len())?;
            postgres_query_streaming(client, &sql, params)
        }
        SqlConnection::MySql(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::MySql, params.len())?;
            mysql_query_streaming(conn, &sql, params)
        }
        SqlConnection::SqlServer(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::SqlServer, params.len())?;
            sqlserver_query_streaming(conn, &sql, params)
        }
    })?;
    let rs_handle = store_connection_resultset(resultset, handle);
    create_resultset_value(rs_handle)
}

fn connection_query_with_timeout(
    handle: i64,
    sql: &str,
    params: &[SqlParam],
    timeout_ms: i64,
) -> Result<Value, SqlFailure> {
    let resultset = with_connection(handle, |connection| {
        query_on_connection_with_timeout(connection, sql, params, timeout_ms)
    })?;
    let rs_handle = store_connection_resultset(resultset, handle);
    create_resultset_value(rs_handle)
}

fn connection_query_with_cancellation(
    handle: i64,
    sql: &str,
    params: &[SqlParam],
    token: *const Value,
) -> Result<Value, SqlFailure> {
    let token = cancellation_entry(token).map_err(SqlFailure::plain)?;
    let resultset = with_connection(handle, |connection| {
        query_on_connection_with_cancellation(connection, sql, params, &token)
    })?;
    let rs_handle = store_connection_resultset(resultset, handle);
    create_resultset_value(rs_handle)
}

/// Execute a bounded query on an already borrowed provider connection.
///
/// Keeping this at the provider-connection boundary lets connections,
/// transactions, prepared statements, and pools share exactly the same
/// timeout semantics. Each provider uses its driver-supported interruption
/// mechanism and returns a typed error when that mechanism fails.
fn query_on_connection_with_timeout(
    connection: &mut SqlConnection,
    sql: &str,
    params: &[SqlParam],
    timeout_ms: i64,
) -> Result<SqlResultSet, SqlFailure> {
    if timeout_ms < 0 {
        return Err(SqlFailure::plain("SQL query timeout must be non-negative"));
    }
    match connection {
        SqlConnection::Sqlite(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Sqlite, params.len())?;
            sqlite_query_with_timeout(conn, &sql, params, timeout_ms)
        }
        SqlConnection::Postgres(client) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Postgres, params.len())?;
            postgres_query_with_timeout(client, &sql, params, timeout_ms)
        }
        SqlConnection::MySql(conn) => {
            let timeout = Duration::from_millis(
                u64::try_from(timeout_ms)
                    .map_err(|_| SqlFailure::plain("SQL query timeout must be non-negative"))?,
            );
            mysql_query_with_interrupt(conn, sql, params, Some(timeout), None)
        }
        SqlConnection::SqlServer(conn) => {
            let timeout = Some(timeout_ms);
            sqlserver_query_streaming_with_options(conn, sql, params, timeout, None)
        }
    }
}

/// The named-parameter path has already rewritten its statement before it
/// reaches the timeout helper. Keep that boundary explicit so placeholders
/// are never rewritten twice.
fn query_on_rewritten_connection_with_timeout(
    connection: &mut SqlConnection,
    sql: &str,
    params: &[SqlParam],
    timeout_ms: i64,
) -> Result<SqlResultSet, SqlFailure> {
    if timeout_ms < 0 {
        return Err(SqlFailure::plain("SQL query timeout must be non-negative"));
    }
    match connection {
        SqlConnection::Sqlite(conn) => sqlite_query_with_timeout(conn, sql, params, timeout_ms),
        SqlConnection::Postgres(client) => {
            postgres_query_with_timeout(client, sql, params, timeout_ms)
        }
        SqlConnection::MySql(conn) => {
            let timeout = Duration::from_millis(
                u64::try_from(timeout_ms)
                    .map_err(|_| SqlFailure::plain("SQL query timeout must be non-negative"))?,
            );
            mysql_query_with_interrupt(conn, sql, params, Some(timeout), None)
        }
        SqlConnection::SqlServer(conn) => {
            sqlserver_query_streaming_with_options(conn, sql, params, Some(timeout_ms), None)
        }
    }
}

/// Execute a query with a cooperative cancellation token at the provider
/// boundary. Each provider uses its driver-supported interruption mechanism.
fn query_on_connection_with_cancellation(
    connection: &mut SqlConnection,
    sql: &str,
    params: &[SqlParam],
    token: &std::sync::Arc<crate::sync_primitives::CancellationEntry>,
) -> Result<SqlResultSet, SqlFailure> {
    match connection {
        SqlConnection::Sqlite(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Sqlite, params.len())?;
            sqlite_query_with_cancellation(conn, &sql, params, token)
        }
        SqlConnection::Postgres(client) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Postgres, params.len())?;
            postgres_query_with_cancellation(client, &sql, params, token)
        }
        SqlConnection::MySql(conn) => {
            mysql_query_with_interrupt(conn, sql, params, None, Some(Arc::clone(token)))
        }
        SqlConnection::SqlServer(conn) => {
            sqlserver_query_streaming_with_options(conn, sql, params, None, Some(Arc::clone(token)))
        }
    }
}

fn query_on_rewritten_connection_with_cancellation(
    connection: &mut SqlConnection,
    sql: &str,
    params: &[SqlParam],
    token: &std::sync::Arc<crate::sync_primitives::CancellationEntry>,
) -> Result<SqlResultSet, SqlFailure> {
    match connection {
        SqlConnection::Sqlite(conn) => sqlite_query_with_cancellation(conn, sql, params, token),
        SqlConnection::Postgres(client) => {
            postgres_query_with_cancellation(client, sql, params, token)
        }
        SqlConnection::MySql(conn) => {
            mysql_query_with_interrupt(conn, sql, params, None, Some(Arc::clone(token)))
        }
        SqlConnection::SqlServer(conn) => {
            sqlserver_query_streaming_with_options(conn, sql, params, None, Some(Arc::clone(token)))
        }
    }
}

fn connection_execute_named(
    handle: i64,
    sql: &str,
    named: &HashMap<String, SqlParam>,
) -> Result<i64, SqlFailure> {
    with_connection(handle, |connection| match connection {
        SqlConnection::Sqlite(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::Sqlite, named)?;
            sqlite_execute(conn, &sql, &params)
        }
        SqlConnection::Postgres(client) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::Postgres, named)?;
            postgres_execute(client, &sql, &params)
        }
        SqlConnection::MySql(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::MySql, named)?;
            mysql_execute(conn, &sql, &params)
        }
        SqlConnection::SqlServer(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::SqlServer, named)?;
            sqlserver_execute(conn, &sql, &params)
        }
    })
}

fn connection_query_named(
    handle: i64,
    sql: &str,
    named: &HashMap<String, SqlParam>,
) -> Result<Value, SqlFailure> {
    let resultset = with_connection(handle, |connection| match connection {
        SqlConnection::Sqlite(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::Sqlite, named)?;
            sqlite_query_streaming(conn, &sql, &params)
        }
        SqlConnection::Postgres(client) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::Postgres, named)?;
            postgres_query_streaming(client, &sql, &params)
        }
        SqlConnection::MySql(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::MySql, named)?;
            mysql_query_streaming(conn, &sql, &params)
        }
        SqlConnection::SqlServer(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::SqlServer, named)?;
            sqlserver_query_streaming(conn, &sql, &params)
        }
    })?;
    let rs_handle = store_connection_resultset(resultset, handle);
    create_resultset_value(rs_handle)
}

fn connection_query_named_with_cancellation(
    handle: i64,
    sql: &str,
    named: &HashMap<String, SqlParam>,
    token: *const Value,
) -> Result<Value, SqlFailure> {
    let token = cancellation_entry(token).map_err(SqlFailure::plain)?;
    let resultset = with_connection(handle, |connection| {
        let backend = match connection {
            SqlConnection::Sqlite(_) => SqlBackend::Sqlite,
            SqlConnection::Postgres(_) => SqlBackend::Postgres,
            SqlConnection::MySql(_) => SqlBackend::MySql,
            SqlConnection::SqlServer(_) => SqlBackend::SqlServer,
        };
        let (rewritten, values) = rewrite_named_sql(sql, backend, named)?;
        query_on_rewritten_connection_with_cancellation(connection, &rewritten, &values, &token)
    })?;
    let rs_handle = store_connection_resultset(resultset, handle);
    create_resultset_value(rs_handle)
}

fn pool_state(handle: i64) -> Result<Arc<PoolState>, SqlFailure> {
    SQL_POOLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&handle)
        .map(|entry| Arc::clone(&entry.state))
        .ok_or_else(|| SqlFailure::plain("invalid sql pool handle"))
}

fn acquire_pool_connection(handle: i64) -> Result<PoolLease, SqlFailure> {
    let state = pool_state(handle)?;
    let started = std::time::Instant::now();
    let mut guard = state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if guard.closed {
            return Err(SqlFailure::plain("sql pool is closed"));
        }
        let connection = if let Some(connection) = guard.idle.pop() {
            guard.in_use += 1;
            Some(connection)
        } else if guard.total < guard.max_connections {
            guard.total += 1;
            guard.in_use += 1;
            let uri = guard.uri.clone();
            drop(guard);
            let connection = route_connect_failure(&uri);
            guard = state
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match connection {
                Ok(connection) if !guard.closed => Some(connection),
                Ok(_connection) => {
                    guard.total = guard.total.saturating_sub(1);
                    guard.in_use = guard.in_use.saturating_sub(1);
                    state.wake.notify_all();
                    return Err(SqlFailure::plain("sql pool is closed"));
                }
                Err(error) => {
                    guard.total = guard.total.saturating_sub(1);
                    guard.in_use = guard.in_use.saturating_sub(1);
                    state.wake.notify_one();
                    return Err(error);
                }
            }
        } else {
            let Some(timeout) = guard.acquire_timeout else {
                guard.waiters += 1;
                guard = state
                    .wake
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.waiters = guard.waiters.saturating_sub(1);
                continue;
            };
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Err(SqlFailure::plain("sql pool acquisition timed out"));
            }
            let remaining = timeout.saturating_sub(elapsed);
            guard.waiters += 1;
            let (next_guard, timed_out) = match state.wake.wait_timeout(guard, remaining) {
                Ok((next_guard, wait_result)) => (next_guard, wait_result.timed_out()),
                Err(poisoned) => {
                    let (next_guard, wait_result) = poisoned.into_inner();
                    (next_guard, wait_result.timed_out())
                }
            };
            guard = next_guard;
            guard.waiters = guard.waiters.saturating_sub(1);
            if timed_out {
                return Err(SqlFailure::plain("sql pool acquisition timed out"));
            }
            continue;
        };

        let connection =
            connection.ok_or_else(|| SqlFailure::plain("sql pool connection unavailable"))?;
        drop(guard);
        return Ok(PoolLease {
            state,
            connection: Some(connection),
            discard: false,
        });
    }
}

fn with_pool_connection<R, F>(handle: i64, operation: F) -> Result<R, SqlFailure>
where
    F: FnOnce(&mut SqlConnection) -> Result<R, SqlFailure>,
{
    let mut lease = acquire_pool_connection(handle)?;
    operation(lease.connection_mut()?)
}

fn pool_execute_with_params(
    handle: i64,
    sql: &str,
    params: &[SqlParam],
) -> Result<i64, SqlFailure> {
    with_pool_connection(handle, |connection| {
        execute_on_connection(connection, sql, params)
    })
}

fn pool_execute_batch(handle: i64, sql: &str) -> Result<(), SqlFailure> {
    with_pool_connection(handle, |connection| {
        execute_batch_on_connection(connection, sql)
    })
}

fn pool_execute_many(handle: i64, sql: &str, rows: &[Vec<SqlParam>]) -> Result<i64, SqlFailure> {
    with_pool_connection(handle, |connection| {
        execute_many_on_connection(connection, sql, rows)
    })
}

fn store_pool_query_result<F>(handle: i64, operation: F) -> Result<Value, SqlFailure>
where
    F: FnOnce(&mut SqlConnection) -> Result<SqlResultSet, SqlFailure>,
{
    let mut lease = acquire_pool_connection(handle)?;
    let resultset = match operation(lease.connection_mut()?) {
        Ok(resultset) => resultset,
        Err(error) => {
            // A failed streaming setup may leave provider protocol state
            // unread. Retire that connection instead of returning it to the
            // idle pool.
            lease.discard();
            return Err(error);
        }
    };
    let rs_handle = store_pool_resultset(resultset, lease);
    create_resultset_value(rs_handle)
}

fn pool_query_with_params(
    handle: i64,
    sql: &str,
    params: &[SqlParam],
) -> Result<Value, SqlFailure> {
    store_pool_query_result(handle, |connection| match connection {
        SqlConnection::Sqlite(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Sqlite, params.len())?;
            Ok::<_, SqlFailure>(sqlite_query_streaming(conn, &sql, params)?)
        }
        SqlConnection::Postgres(client) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::Postgres, params.len())?;
            Ok::<_, SqlFailure>(postgres_query_streaming(client, &sql, params)?)
        }
        SqlConnection::MySql(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::MySql, params.len())?;
            Ok::<_, SqlFailure>(mysql_query_streaming(conn, &sql, params)?)
        }
        SqlConnection::SqlServer(conn) => {
            let sql = rewrite_positional_sql(sql, SqlBackend::SqlServer, params.len())?;
            Ok::<_, SqlFailure>(sqlserver_query_streaming(conn, &sql, params)?)
        }
    })
}

fn pool_query_with_timeout(
    handle: i64,
    sql: &str,
    params: &[SqlParam],
    timeout_ms: i64,
) -> Result<Value, SqlFailure> {
    store_pool_query_result(handle, |connection| {
        query_on_connection_with_timeout(connection, sql, params, timeout_ms)
    })
}

fn pool_query_named_with_timeout(
    handle: i64,
    sql: &str,
    named: &HashMap<String, SqlParam>,
    timeout_ms: i64,
) -> Result<Value, SqlFailure> {
    store_pool_query_result(handle, |connection| {
        let backend = match connection {
            SqlConnection::Sqlite(_) => SqlBackend::Sqlite,
            SqlConnection::Postgres(_) => SqlBackend::Postgres,
            SqlConnection::MySql(_) => SqlBackend::MySql,
            SqlConnection::SqlServer(_) => SqlBackend::SqlServer,
        };
        let (rewritten, values) = rewrite_named_sql(sql, backend, named)?;
        query_on_rewritten_connection_with_timeout(connection, &rewritten, &values, timeout_ms)
    })
}

fn pool_query_with_cancellation(
    handle: i64,
    sql: &str,
    params: &[SqlParam],
    token: *const Value,
) -> Result<Value, SqlFailure> {
    let token = cancellation_entry(token).map_err(SqlFailure::plain)?;
    store_pool_query_result(handle, |connection| {
        query_on_connection_with_cancellation(connection, sql, params, &token)
    })
}

fn pool_query_named_with_cancellation(
    handle: i64,
    sql: &str,
    named: &HashMap<String, SqlParam>,
    token: *const Value,
) -> Result<Value, SqlFailure> {
    let token = cancellation_entry(token).map_err(SqlFailure::plain)?;
    store_pool_query_result(handle, |connection| {
        let backend = match connection {
            SqlConnection::Sqlite(_) => SqlBackend::Sqlite,
            SqlConnection::Postgres(_) => SqlBackend::Postgres,
            SqlConnection::MySql(_) => SqlBackend::MySql,
            SqlConnection::SqlServer(_) => SqlBackend::SqlServer,
        };
        let (rewritten, values) = rewrite_named_sql(sql, backend, named)?;
        query_on_rewritten_connection_with_cancellation(connection, &rewritten, &values, &token)
    })
}

fn pool_execute_named(
    handle: i64,
    sql: &str,
    named: &HashMap<String, SqlParam>,
) -> Result<i64, SqlFailure> {
    with_pool_connection(handle, |connection| match connection {
        SqlConnection::Sqlite(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::Sqlite, named)?;
            Ok(sqlite_execute(conn, &sql, &params)?)
        }
        SqlConnection::Postgres(client) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::Postgres, named)?;
            Ok(postgres_execute(client, &sql, &params)?)
        }
        SqlConnection::MySql(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::MySql, named)?;
            Ok(mysql_execute(conn, &sql, &params)?)
        }
        SqlConnection::SqlServer(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::SqlServer, named)?;
            Ok(sqlserver_execute(conn, &sql, &params)?)
        }
    })
}

fn pool_query_named(
    handle: i64,
    sql: &str,
    named: &HashMap<String, SqlParam>,
) -> Result<Value, SqlFailure> {
    store_pool_query_result(handle, |connection| match connection {
        SqlConnection::Sqlite(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::Sqlite, named)?;
            Ok::<_, SqlFailure>(sqlite_query_streaming(conn, &sql, &params)?)
        }
        SqlConnection::Postgres(client) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::Postgres, named)?;
            Ok::<_, SqlFailure>(postgres_query_streaming(client, &sql, &params)?)
        }
        SqlConnection::MySql(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::MySql, named)?;
            Ok::<_, SqlFailure>(mysql_query_streaming(conn, &sql, &params)?)
        }
        SqlConnection::SqlServer(conn) => {
            let (sql, params) = rewrite_named_sql(sql, SqlBackend::SqlServer, named)?;
            Ok::<_, SqlFailure>(sqlserver_query_streaming(conn, &sql, &params)?)
        }
    })
}

fn savepoint_name(value: *mut Value) -> Result<String, String> {
    let name = value_to_string(value)?;
    if name.is_empty() || name.len() > 128 {
        return Err("savepoint name must be between 1 and 128 bytes".to_string());
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("savepoint name must not be empty".to_string());
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || chars.any(|character| !(character.is_ascii_alphanumeric() || character == '_'))
    {
        return Err(
            "savepoint name must contain only ASCII letters, digits, and underscores".to_string(),
        );
    }
    Ok(name)
}

fn transaction_savepoint_command(
    transaction: *mut Value,
    name: *mut Value,
    command: &str,
    operation: &str,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let name = match savepoint_name(name) {
        Ok(name) => name,
        Err(error) => return sql_result_err(error),
    };
    let result = with_transaction(handle, |tx| {
        if !tx.active {
            return Err(SqlFailure::plain("transaction is no longer active"));
        }
        let connection = tx
            .connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
        let statement = format!("{command} {name}");
        match connection {
            SqlConnection::Sqlite(connection) => connection
                .execute_batch(&statement)
                .map_err(|error| SqlFailure::sqlite("SQL savepoint failed", error)),
            SqlConnection::Postgres(connection) => connection
                .batch_execute(&statement)
                .map_err(|error| SqlFailure::postgres("SQL savepoint failed", error)),
            SqlConnection::MySql(connection) => connection
                .query_drop(&statement)
                .map_err(|error| SqlFailure::mysql("SQL savepoint failed", error)),
            SqlConnection::SqlServer(connection) => {
                let statement = match operation {
                    "savepoint" => format!("SAVE TRANSACTION {name}"),
                    "rollback_to" => format!("ROLLBACK TRANSACTION {name}"),
                    "release_savepoint" => {
                        return Err(SqlFailure::unsupported(
                            "SQL Server does not support releasing a savepoint",
                        ));
                    }
                    _ => statement,
                };
                sqlserver_simple_query(connection, &statement)
            }
        }
    });
    sql_result_unit_failure_context(result, transaction_provider(handle), operation)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_savepoint(
    transaction: *mut Value,
    name: *mut Value,
) -> *mut Value {
    transaction_savepoint_command(transaction, name, "SAVEPOINT", "savepoint")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_rollback_to(
    transaction: *mut Value,
    name: *mut Value,
) -> *mut Value {
    transaction_savepoint_command(transaction, name, "ROLLBACK TO SAVEPOINT", "rollback_to")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_release_savepoint(
    transaction: *mut Value,
    name: *mut Value,
) -> *mut Value {
    transaction_savepoint_command(transaction, name, "RELEASE SAVEPOINT", "release_savepoint")
}

fn transaction_execute_with_params(
    tx_handle: i64,
    sql: &str,
    params: &[SqlParam],
) -> Result<i64, SqlFailure> {
    with_transaction(tx_handle, |tx| {
        if !tx.active {
            return Err(SqlFailure::plain("transaction is no longer active"));
        }
        let connection = tx
            .connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
        execute_on_connection(connection, sql, params)
    })
}

fn transaction_execute_batch(tx_handle: i64, sql: &str) -> Result<(), SqlFailure> {
    with_transaction(tx_handle, |tx| {
        if !tx.active {
            return Err(SqlFailure::plain("transaction is no longer active"));
        }
        let connection = tx
            .connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
        execute_batch_on_connection(connection, sql)
    })
}

fn transaction_execute_many(
    tx_handle: i64,
    sql: &str,
    rows: &[Vec<SqlParam>],
) -> Result<i64, SqlFailure> {
    with_transaction(tx_handle, |tx| {
        if !tx.active {
            return Err(SqlFailure::plain("transaction is no longer active"));
        }
        let connection = tx
            .connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
        execute_many_on_connection(connection, sql, rows)
    })
}

fn transaction_query_with_params(
    tx_handle: i64,
    sql: &str,
    params: &[SqlParam],
) -> Result<Value, SqlFailure> {
    with_transaction(tx_handle, |tx| {
        if !tx.active {
            return Err(SqlFailure::plain("transaction is no longer active"));
        }
        let connection = tx
            .connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
        let resultset = match connection {
            SqlConnection::Sqlite(conn) => {
                let sql = rewrite_positional_sql(sql, SqlBackend::Sqlite, params.len())?;
                sqlite_query_streaming(conn, &sql, params)
            }
            SqlConnection::Postgres(client) => {
                let sql = rewrite_positional_sql(sql, SqlBackend::Postgres, params.len())?;
                postgres_query_streaming(client, &sql, params)
            }
            SqlConnection::MySql(conn) => {
                let sql = rewrite_positional_sql(sql, SqlBackend::MySql, params.len())?;
                mysql_query_streaming(conn, &sql, params)
            }
            SqlConnection::SqlServer(conn) => {
                let sql = rewrite_positional_sql(sql, SqlBackend::SqlServer, params.len())?;
                sqlserver_query_streaming(conn, &sql, params)
            }
        }?;
        let rs_handle = store_transaction_resultset(resultset, tx_handle);
        create_resultset_value(rs_handle)
    })
}

fn transaction_query_with_timeout(
    tx_handle: i64,
    sql: &str,
    params: &[SqlParam],
    timeout_ms: i64,
) -> Result<Value, SqlFailure> {
    with_transaction(tx_handle, |tx| {
        if !tx.active {
            return Err(SqlFailure::plain("transaction is no longer active"));
        }
        let connection = tx
            .connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
        let resultset = query_on_connection_with_timeout(connection, sql, params, timeout_ms)?;
        let rs_handle = store_transaction_resultset(resultset, tx_handle);
        create_resultset_value(rs_handle)
    })
}

fn transaction_query_named_with_timeout(
    tx_handle: i64,
    sql: &str,
    named: &HashMap<String, SqlParam>,
    timeout_ms: i64,
) -> Result<Value, SqlFailure> {
    with_transaction(tx_handle, |tx| {
        if !tx.active {
            return Err(SqlFailure::plain("transaction is no longer active"));
        }
        let connection = tx
            .connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
        let backend = match connection {
            SqlConnection::Sqlite(_) => SqlBackend::Sqlite,
            SqlConnection::Postgres(_) => SqlBackend::Postgres,
            SqlConnection::MySql(_) => SqlBackend::MySql,
            SqlConnection::SqlServer(_) => SqlBackend::SqlServer,
        };
        let (rewritten, values) = rewrite_named_sql(sql, backend, named)?;
        let resultset = query_on_rewritten_connection_with_timeout(
            connection, &rewritten, &values, timeout_ms,
        )?;
        let rs_handle = store_transaction_resultset(resultset, tx_handle);
        create_resultset_value(rs_handle)
    })
}

fn transaction_query_with_cancellation(
    tx_handle: i64,
    sql: &str,
    params: &[SqlParam],
    token: *const Value,
) -> Result<Value, SqlFailure> {
    let token = cancellation_entry(token).map_err(SqlFailure::plain)?;
    with_transaction(tx_handle, |tx| {
        if !tx.active {
            return Err(SqlFailure::plain("transaction is no longer active"));
        }
        let connection = tx
            .connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
        let resultset = query_on_connection_with_cancellation(connection, sql, params, &token)?;
        let rs_handle = store_transaction_resultset(resultset, tx_handle);
        create_resultset_value(rs_handle)
    })
}

fn transaction_query_named_with_cancellation(
    tx_handle: i64,
    sql: &str,
    named: &HashMap<String, SqlParam>,
    token: *const Value,
) -> Result<Value, SqlFailure> {
    let token = cancellation_entry(token).map_err(SqlFailure::plain)?;
    with_transaction(tx_handle, |tx| {
        if !tx.active {
            return Err(SqlFailure::plain("transaction is no longer active"));
        }
        let connection = tx
            .connection
            .as_mut()
            .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
        let backend = match connection {
            SqlConnection::Sqlite(_) => SqlBackend::Sqlite,
            SqlConnection::Postgres(_) => SqlBackend::Postgres,
            SqlConnection::MySql(_) => SqlBackend::MySql,
            SqlConnection::SqlServer(_) => SqlBackend::SqlServer,
        };
        let (rewritten, values) = rewrite_named_sql(sql, backend, named)?;
        let resultset = query_on_rewritten_connection_with_cancellation(
            connection, &rewritten, &values, &token,
        )?;
        let rs_handle = store_transaction_resultset(resultset, tx_handle);
        create_resultset_value(rs_handle)
    })
}

#[unsafe(no_mangle)]
/// # Safety
/// The `uri` pointer must point to a valid, null-terminated C string for the
/// duration of this call.
pub unsafe extern "C" fn mux_sql_connect(uri: *const c_char) -> *mut Value {
    if uri.is_null() {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            "unknown",
            "connect",
            "sql uri pointer is null".to_string(),
        );
    }
    let uri_text = unsafe { CStr::from_ptr(uri) }
        .to_string_lossy()
        .into_owned();
    if let Some(kind) = sql_uri_error_kind(&uri_text) {
        let detail = if kind == SqlErrorKind::Invalid && is_sqlserver_uri(&uri_text) {
            parse_sqlserver_uri(&uri_text)
                .expect_err("invalid SQL Server URI classification must include a parse error")
                .detail
        } else {
            format!("unsupported or unrecognised sql URI scheme: {uri_text}")
        };
        return sql_result_err_context(kind, provider_for_uri(&uri_text), "connect", detail);
    }
    match route_connect_failure(&uri_text) {
        Ok(connection) => {
            let handle = store_connection(connection);
            sql_result_handle(handle, *SQL_CONNECTION_TYPE_ID)
        }
        Err(failure) => sql_result_err_failure(
            SqlErrorKind::Database,
            provider_for_uri(&uri_text),
            "connect",
            failure,
        ),
    }
}

/// Report the query-interruption capabilities of a live connection.
///
/// The returned `result<map<string, bool|string>, SqlError>` contains the
/// stable keys `provider`, `query_timeout`, and `query_cancellation`. A false
/// flag means the corresponding operation will return
/// `SqlErrorKind.Unsupported` without sending the statement.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_capabilities(connection: *mut Value) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err_kind(SqlErrorKind::Invalid, error),
    };
    sql_result_ok(sql_capabilities_value(connection_provider(handle)))
}

/// Report the query-interruption capabilities of a live connection pool.
///
/// Pool capabilities are determined from the configured provider and do not
/// acquire a connection. The result uses the same stable keys as
/// `mux_sql_connection_capabilities`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_capabilities(pool: *mut Value) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err_kind(SqlErrorKind::Invalid, error),
    };
    sql_result_ok(sql_capabilities_value(pool_provider(handle)))
}

/// Open an isolated in-memory SQLite database. This is the friendly default
/// for examples and tests; callers needing another provider can use `sql.connect`.
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_sqlite_memory() -> *mut Value {
    match SqliteConnection::open_in_memory().map(SqlConnection::Sqlite) {
        Ok(connection) => {
            let handle = store_connection(connection);
            sql_result_handle(handle, *SQL_CONNECTION_TYPE_ID)
        }
        Err(err) => sql_result_err_failure(
            SqlErrorKind::Database,
            "sqlite",
            "connect",
            SqlFailure::sqlite("sqlite memory connect failed", err),
        ),
    }
}

/// Create a bounded connection pool. Connections are opened lazily up to
/// `max_connections`; `acquire_timeout_ms` controls how long an operation may
/// wait for an idle connection (`-1` waits indefinitely, `0` never waits).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_from_config(
    uri: *mut Value,
    max_connections: i64,
    acquire_timeout_ms: i64,
) -> *mut Value {
    let uri = match value_to_string(uri) {
        Ok(uri) if !uri.is_empty() => uri,
        Ok(_) => {
            return sql_result_err_context(
                SqlErrorKind::Invalid,
                "unknown",
                "pool_from_config",
                "sql pool URI must not be empty".to_string(),
            );
        }
        Err(error) => {
            return sql_result_err_context(
                SqlErrorKind::Invalid,
                "unknown",
                "pool_from_config",
                error,
            );
        }
    };
    if let Some(kind) = sql_uri_error_kind(&uri) {
        let detail = if kind == SqlErrorKind::Invalid && is_sqlserver_uri(&uri) {
            parse_sqlserver_uri(&uri)
                .expect_err("invalid SQL Server URI classification must include a parse error")
                .detail
        } else {
            format!("unsupported or unrecognised sql URI scheme: {uri}")
        };
        return sql_result_err_context(kind, provider_for_uri(&uri), "pool_from_config", detail);
    }
    let Ok(max_connections) = usize::try_from(max_connections) else {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            provider_for_uri(&uri),
            "pool_from_config",
            "sql pool max_connections must be positive".to_string(),
        );
    };
    if !(1..=64).contains(&max_connections) {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            provider_for_uri(&uri),
            "pool_from_config",
            "sql pool max_connections must be between 1 and 64".to_string(),
        );
    }
    let acquire_timeout = if acquire_timeout_ms < -1 {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            provider_for_uri(&uri),
            "pool_from_config",
            "sql pool acquire timeout must be -1 or non-negative".to_string(),
        );
    } else if acquire_timeout_ms == -1 {
        None
    } else {
        Some(std::time::Duration::from_millis(acquire_timeout_ms as u64))
    };
    let provider = provider_for_uri(&uri);
    let initial = match route_connect_failure(&uri) {
        Ok(connection) => connection,
        Err(error) => {
            return sql_result_err_failure(
                SqlErrorKind::Database,
                provider,
                "pool_from_config",
                error,
            );
        }
    };
    let state = PoolState {
        inner: Mutex::new(PoolInner {
            uri,
            max_connections,
            acquire_timeout,
            idle: vec![initial],
            total: 1,
            in_use: 0,
            waiters: 0,
            closed: false,
        }),
        wake: Condvar::new(),
    };
    match store_pool(state) {
        Ok(value) => sql_result_ok(value),
        Err(error) => {
            sql_result_err_context(SqlErrorKind::Database, provider, "pool_from_config", error)
        }
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_close(pool: *mut Value) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let state = match pool_state(handle) {
        Ok(state) => state,
        Err(error) => {
            return sql_result_err_failure(SqlErrorKind::Invalid, "unknown", "pool_close", error)
        }
    };
    close_pool_state(&state);
    sql_result_ok(Value::Unit)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_metrics(pool: *mut Value) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let state = match pool_state(handle) {
        Ok(state) => state,
        Err(error) => {
            return sql_result_err_failure(SqlErrorKind::Invalid, "unknown", "pool_metrics", error)
        }
    };
    let inner = state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut metrics = crate::ordered::OrderedMap::new();
    metrics.insert(
        Value::String("capacity".to_string()),
        Value::Int(inner.max_connections as i64),
    );
    metrics.insert(
        Value::String("total".to_string()),
        Value::Int(inner.total as i64),
    );
    metrics.insert(
        Value::String("idle".to_string()),
        Value::Int(inner.idle.len() as i64),
    );
    metrics.insert(
        Value::String("in_use".to_string()),
        Value::Int(inner.in_use as i64),
    );
    metrics.insert(
        Value::String("waiters".to_string()),
        Value::Int(inner.waiters as i64),
    );
    metrics.insert(
        Value::String("closed".to_string()),
        Value::Int(i64::from(inner.closed)),
    );
    sql_result_ok(Value::Map(metrics))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_execute(pool: *mut Value, sql: *mut Value) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let result = pool_execute_with_params(handle, &sql, &[]);
    sql_result_i64_failure_context(result, pool_provider(handle), "execute")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_execute_batch(pool: *mut Value, sql: *mut Value) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    sql_result_unit_failure_context(
        pool_execute_batch(handle, &sql),
        pool_provider(handle),
        "execute_batch",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_execute_many(
    pool: *mut Value,
    sql: *mut Value,
    rows: *mut Value,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let rows = match value_list_to_sql_param_rows(rows) {
        Ok(rows) => rows,
        Err(error) => return sql_result_err(error),
    };
    sql_result_i64_failure_context(
        pool_execute_many(handle, &sql, &rows),
        pool_provider(handle),
        "execute_many",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_execute_params(
    pool: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let params = match value_list_to_sql_params(params) {
        Ok(params) => params,
        Err(error) => return sql_result_err(error),
    };
    sql_result_i64_failure_context(
        pool_execute_with_params(handle, &sql, &params),
        pool_provider(handle),
        "execute_params",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_execute_named(
    pool: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let params = match value_map_to_named_params(params) {
        Ok(params) => params,
        Err(error) => return sql_result_err(error),
    };
    sql_result_i64_failure_context(
        pool_execute_named(handle, &sql, &params),
        pool_provider(handle),
        "execute_named",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_query(pool: *mut Value, sql: *mut Value) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    sql_result_value_failure_context(
        pool_query_with_params(handle, &sql, &[]),
        pool_provider(handle),
        "query",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_query_params(
    pool: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let params = match value_list_to_sql_params(params) {
        Ok(params) => params,
        Err(error) => return sql_result_err(error),
    };
    sql_result_value_failure_context(
        pool_query_with_params(handle, &sql, &params),
        pool_provider(handle),
        "query_params",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_query_named(
    pool: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let params = match value_map_to_named_params(params) {
        Ok(params) => params,
        Err(error) => return sql_result_err(error),
    };
    sql_result_value_failure_context(
        pool_query_named(handle, &sql, &params),
        pool_provider(handle),
        "query_named",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_query_with_timeout(
    pool: *mut Value,
    sql: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    sql_result_value_failure_context(
        pool_query_with_timeout(handle, &sql, &[], timeout_ms),
        pool_provider(handle),
        "query_with_timeout",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_query_params_with_timeout(
    pool: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let params = match value_list_to_sql_params(params) {
        Ok(params) => params,
        Err(error) => return sql_result_err(error),
    };
    sql_result_value_failure_context(
        pool_query_with_timeout(handle, &sql, &params, timeout_ms),
        pool_provider(handle),
        "query_params_with_timeout",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_query_named_with_timeout(
    pool: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let params = match value_map_to_named_params(params) {
        Ok(params) => params,
        Err(error) => return sql_result_err(error),
    };
    sql_result_value_failure_context(
        pool_query_named_with_timeout(handle, &sql, &params, timeout_ms),
        pool_provider(handle),
        "query_named_with_timeout",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_query_with_cancellation(
    pool: *mut Value,
    sql: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    sql_result_value_failure_context(
        pool_query_with_cancellation(handle, &sql, &[], token),
        pool_provider(handle),
        "query_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_query_params_with_cancellation(
    pool: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let params = match value_list_to_sql_params(params) {
        Ok(params) => params,
        Err(error) => return sql_result_err(error),
    };
    sql_result_value_failure_context(
        pool_query_with_cancellation(handle, &sql, &params, token),
        pool_provider(handle),
        "query_params_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_pool_query_named_with_cancellation(
    pool: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match pool_handle(pool) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let sql = match value_to_string(sql) {
        Ok(sql) => sql,
        Err(error) => return sql_result_err(error),
    };
    let params = match value_map_to_named_params(params) {
        Ok(params) => params,
        Err(error) => return sql_result_err(error),
    };
    sql_result_value_failure_context(
        pool_query_named_with_cancellation(handle, &sql, &params, token),
        pool_provider(handle),
        "query_named_with_cancellation",
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_int(value: i64) -> *mut Value {
    crate::refcount::mux_rc_alloc(Value::Int(value))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_float(value: f64) -> *mut Value {
    crate::refcount::mux_rc_alloc(Value::Float(ordered_float::OrderedFloat(value)))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_bool(value: bool) -> *mut Value {
    crate::refcount::mux_rc_alloc(Value::Bool(value))
}

/// # Safety
/// The `value` pointer must point to a valid, null-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_sql_value_string(value: *const c_char) -> *mut Value {
    if value.is_null() {
        return crate::refcount::mux_rc_alloc(Value::String(String::new()));
    }
    let raw = unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned();
    crate::refcount::mux_rc_alloc(Value::String(raw))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_bytes(value: *const Value) -> *mut Value {
    if value.is_null() {
        return crate::refcount::mux_rc_alloc(Value::Bytes(Vec::new()));
    }
    let list = unsafe { &*value };
    match list {
        Value::Bytes(items) => crate::refcount::mux_rc_alloc(Value::Bytes(items.clone())),
        _ => crate::refcount::mux_rc_alloc(Value::Bytes(Vec::new())),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_null() -> *mut Value {
    crate::refcount::mux_rc_alloc(Value::Unit)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_is_null(value: *const Value) -> bool {
    if value.is_null() {
        return true;
    }
    matches!(unsafe { &*value }, Value::Unit)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_as_int(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_invalid("expected an int, found nothing".to_string());
    }
    sql_accessor(sql_value_to_int(unsafe { &*value }).map(Value::Int))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_as_bool(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_invalid("expected a bool, found nothing".to_string());
    }
    sql_accessor(sql_value_to_bool(unsafe { &*value }).map(Value::Bool))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_as_float(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_invalid("expected a float, found nothing".to_string());
    }
    sql_accessor(
        sql_value_to_float(unsafe { &*value })
            .map(|v| Value::Float(ordered_float::OrderedFloat(v))),
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_as_string(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_invalid("expected a string, found nothing".to_string());
    }
    sql_accessor(sql_value_to_strict_string(unsafe { &*value }).map(Value::String))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_as_bytes(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_invalid("expected bytes, found nothing".to_string());
    }
    sql_accessor(sql_value_to_bytes(unsafe { &*value }).map(Value::Bytes))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_as_json(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_invalid("expected JSON, found nothing".to_string());
    }
    sql_accessor(sql_value_to_json(unsafe { &*value }))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_as_datetime(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_invalid("expected DateTime, found nothing".to_string());
    }
    sql_accessor(sql_value_to_datetime(unsafe { &*value }))
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_as_uuid(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_invalid("expected Uuid, found nothing".to_string());
    }
    sql_accessor(sql_value_to_uuid(unsafe { &*value }))
}

/// Serialize a native JSON-shaped Mux value as a SQL JSON text parameter.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_json(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_invalid("expected JSON value, found nothing".to_string());
    }
    let result = value_to_json(unsafe { &*value }).map(|json| Value::String(json.stringify(None)));
    match result {
        Ok(value) => sql_result_ok(value),
        Err(error) => sql_result_err(error),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_datetime(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_err("expected DateTime value, found nothing".to_string());
    }
    match unsafe { crate::datetime_types::sql_datetime_string(value) } {
        Ok(text) => sql_result_ok(Value::String(text)),
        Err(error) => sql_result_err(error),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_value_uuid(value: *const Value) -> *mut Value {
    if value.is_null() {
        return sql_result_err("expected Uuid value, found nothing".to_string());
    }
    match unsafe { crate::uuid::sql_uuid_string(value) } {
        Ok(text) => sql_result_ok(Value::String(text)),
        Err(error) => sql_result_err(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_close(connection: *mut Value) {
    if let Ok(handle) = connection_handle(connection) {
        if connection_has_resultset_lease(handle) {
            SQL_CONNECTION_CLOSE_PENDING.with(|pending| {
                pending.borrow_mut().insert(handle);
            });
            write_handle(connection, 0);
            return;
        }
        if connection_has_active_transaction(handle) {
            remove_transaction_for_connection(handle);
        }
        remove_prepared_for_connection(handle);
        remove_connection(handle);
        write_handle(connection, 0);
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_prepare(
    connection: *mut Value,
    sql: *mut Value,
) -> *mut Value {
    let connection_handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        if connection_has_resultset_lease(connection_handle) {
            return Err("sql connection is busy while a result set is open".to_string());
        }
        let statement = value_to_string(sql)?;
        if statement.trim().is_empty() {
            return Err("SQL statement must not be empty".to_string());
        }
        validate_sql_size(&statement)?;
        let handle = store_prepared(SqlPrepared {
            connection_handle,
            sql: statement,
        });
        create_handle_value(handle, *SQL_PREPARED_TYPE_ID)
    })();
    match result {
        Ok(value) => sql_result_ok(value),
        Err(error) => sql_result_err_context(
            SqlErrorKind::Invalid,
            connection_provider(connection_handle),
            "prepare",
            error,
        ),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_execute(
    connection: *mut Value,
    sql: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        connection_execute_with_params(handle, &statement, &[])
    })();
    sql_result_i64_failure_context(result, connection_provider(handle), "execute")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_execute_batch(
    connection: *mut Value,
    sql: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = value_to_string(sql)
        .map_err(SqlFailure::from)
        .and_then(|statement| connection_execute_batch(handle, &statement));
    sql_result_unit_failure_context(result, connection_provider(handle), "execute_batch")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_execute_many(
    connection: *mut Value,
    sql: *mut Value,
    rows: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql).map_err(SqlFailure::from)?;
        let rows = value_list_to_sql_param_rows(rows).map_err(SqlFailure::from)?;
        connection_execute_many(handle, &statement, &rows)
    })();
    sql_result_i64_failure_context(result, connection_provider(handle), "execute_many")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_execute_params(
    connection: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let sql_params = value_list_to_sql_params(params)?;
        connection_execute_with_params(handle, &statement, &sql_params)
    })();
    sql_result_i64_failure_context(result, connection_provider(handle), "execute_params")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_execute_named(
    connection: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let named = value_map_to_named_params(params)?;
        connection_execute_named(handle, &statement, &named)
    })();
    sql_result_i64_failure_context(result, connection_provider(handle), "execute_named")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_query(connection: *mut Value, sql: *mut Value) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        connection_query_with_params(handle, &statement, &[])
    })();
    sql_result_value_failure_context(result, connection_provider(handle), "query")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_query_params(
    connection: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let sql_params = value_list_to_sql_params(params)?;
        connection_query_with_params(handle, &statement, &sql_params)
    })();
    sql_result_value_failure_context(result, connection_provider(handle), "query_params")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_query_named(
    connection: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let named = value_map_to_named_params(params)?;
        connection_query_named(handle, &statement, &named)
    })();
    sql_result_value_failure_context(result, connection_provider(handle), "query_named")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_query_with_timeout(
    connection: *mut Value,
    sql: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        connection_query_with_timeout(handle, &statement, &[], timeout_ms)
    })();
    sql_result_value_failure_context(result, connection_provider(handle), "query_with_timeout")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_query_params_with_timeout(
    connection: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let sql_params = value_list_to_sql_params(params)?;
        connection_query_with_timeout(handle, &statement, &sql_params, timeout_ms)
    })();
    sql_result_value_failure_context(
        result,
        connection_provider(handle),
        "query_params_with_timeout",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_query_named_with_timeout(
    connection: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let named = value_map_to_named_params(params)?;
        let (rewritten, values) = with_connection(handle, |connection| match connection {
            SqlConnection::Sqlite(_) => rewrite_named_sql(&statement, SqlBackend::Sqlite, &named),
            SqlConnection::Postgres(_) => {
                rewrite_named_sql(&statement, SqlBackend::Postgres, &named)
            }
            SqlConnection::MySql(_) => rewrite_named_sql(&statement, SqlBackend::MySql, &named),
            SqlConnection::SqlServer(_) => {
                rewrite_named_sql(&statement, SqlBackend::SqlServer, &named)
            }
        })?;
        connection_query_with_timeout(handle, &rewritten, &values, timeout_ms)
    })();
    sql_result_value_failure_context(
        result,
        connection_provider(handle),
        "query_named_with_timeout",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_query_with_cancellation(
    connection: *mut Value,
    sql: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        connection_query_with_cancellation(handle, &statement, &[], token)
    })();
    sql_result_value_failure_context(
        result,
        connection_provider(handle),
        "query_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_query_params_with_cancellation(
    connection: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let sql_params = value_list_to_sql_params(params)?;
        connection_query_with_cancellation(handle, &statement, &sql_params, token)
    })();
    sql_result_value_failure_context(
        result,
        connection_provider(handle),
        "query_params_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_query_named_with_cancellation(
    connection: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let named = value_map_to_named_params(params)?;
        connection_query_named_with_cancellation(handle, &statement, &named, token)
    })();
    sql_result_value_failure_context(
        result,
        connection_provider(handle),
        "query_named_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_prepared_close(prepared: *mut Value) {
    if let Ok(handle) = prepared_handle(prepared) {
        SQL_PREPARED.with(|values| {
            values.borrow_mut().remove(&handle);
        });
        write_handle(prepared, 0);
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_prepared_execute(prepared: *mut Value, params: *mut Value) -> *mut Value {
    let handle = match prepared_handle(prepared) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let params = value_list_to_sql_params(params)?;
        with_prepared(handle, |statement| {
            connection_execute_with_params(statement.connection_handle, &statement.sql, &params)
        })
    })();
    sql_result_i64_failure_context(result, prepared_provider(handle), "prepared_execute")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_prepared_execute_named(
    prepared: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match prepared_handle(prepared) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let params = value_map_to_named_params(params)?;
        with_prepared(handle, |statement| {
            connection_execute_named(statement.connection_handle, &statement.sql, &params)
        })
    })();
    sql_result_i64_failure_context(result, prepared_provider(handle), "prepared_execute_named")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_prepared_query(prepared: *mut Value, params: *mut Value) -> *mut Value {
    let handle = match prepared_handle(prepared) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let params = value_list_to_sql_params(params)?;
        with_prepared(handle, |statement| {
            connection_query_with_params(statement.connection_handle, &statement.sql, &params)
        })
    })();
    sql_result_value_failure_context(result, prepared_provider(handle), "prepared_query")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_prepared_query_named(
    prepared: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match prepared_handle(prepared) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let params = value_map_to_named_params(params)?;
        with_prepared(handle, |statement| {
            connection_query_named(statement.connection_handle, &statement.sql, &params)
        })
    })();
    sql_result_value_failure_context(result, prepared_provider(handle), "prepared_query_named")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_prepared_query_with_timeout(
    prepared: *mut Value,
    params: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match prepared_handle(prepared) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let params = value_list_to_sql_params(params)?;
        with_prepared(handle, |statement| {
            connection_query_with_timeout(
                statement.connection_handle,
                &statement.sql,
                &params,
                timeout_ms,
            )
        })
    })();
    sql_result_value_failure_context(
        result,
        prepared_provider(handle),
        "prepared_query_with_timeout",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_prepared_query_named_with_timeout(
    prepared: *mut Value,
    params: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match prepared_handle(prepared) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let named = value_map_to_named_params(params)?;
        with_prepared(handle, |statement| {
            let (rewritten, values) = with_connection(statement.connection_handle, |connection| {
                let backend = match connection {
                    SqlConnection::Sqlite(_) => SqlBackend::Sqlite,
                    SqlConnection::Postgres(_) => SqlBackend::Postgres,
                    SqlConnection::MySql(_) => SqlBackend::MySql,
                    SqlConnection::SqlServer(_) => SqlBackend::SqlServer,
                };
                rewrite_named_sql(&statement.sql, backend, &named)
            })?;
            with_connection(statement.connection_handle, |connection| {
                let resultset = query_on_rewritten_connection_with_timeout(
                    connection, &rewritten, &values, timeout_ms,
                )?;
                let rs_handle = store_connection_resultset(resultset, statement.connection_handle);
                create_resultset_value(rs_handle)
            })
        })
    })();
    sql_result_value_failure_context(
        result,
        prepared_provider(handle),
        "prepared_query_named_with_timeout",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_prepared_query_with_cancellation(
    prepared: *mut Value,
    params: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match prepared_handle(prepared) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let params = value_list_to_sql_params(params)?;
        with_prepared(handle, |statement| {
            connection_query_with_cancellation(
                statement.connection_handle,
                &statement.sql,
                &params,
                token,
            )
        })
    })();
    sql_result_value_failure_context(
        result,
        prepared_provider(handle),
        "prepared_query_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_prepared_query_named_with_cancellation(
    prepared: *mut Value,
    params: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match prepared_handle(prepared) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let named = value_map_to_named_params(params)?;
        with_prepared(handle, |statement| {
            connection_query_named_with_cancellation(
                statement.connection_handle,
                &statement.sql,
                &named,
                token,
            )
        })
    })();
    sql_result_value_failure_context(
        result,
        prepared_provider(handle),
        "prepared_query_named_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_begin_transaction(connection: *mut Value) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(h) => h,
        Err(err) => return sql_result_err(err),
    };
    let provider = connection_provider(handle);
    if connection_has_active_transaction(handle) {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            provider,
            "begin_transaction",
            "connection already has an active transaction".to_string(),
        );
    }
    let Ok(conn) = take_connection(handle) else {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            provider,
            "begin_transaction",
            "invalid sql connection handle".to_string(),
        );
    };
    let mut conn = conn;
    if let Err(err) = begin_transaction_on_connection(&mut conn) {
        return_connection(handle, conn);
        return sql_result_err_failure(SqlErrorKind::Database, provider, "begin_transaction", err);
    }
    let tx_handle = store_transaction(SqlTransaction {
        connection_handle: handle,
        provider,
        connection: Some(conn),
        active: true,
        parent: None,
        owner: connection,
    });
    unsafe { mux_rc_inc(connection) };
    sql_result_handle(tx_handle, *SQL_TRANSACTION_TYPE_ID)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_connection_begin_transaction_with_options(
    connection: *mut Value,
    isolation: *mut Value,
    read_only: *mut Value,
    deferrable: *mut Value,
) -> *mut Value {
    let handle = match connection_handle(connection) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let provider = connection_provider(handle);
    if connection_has_active_transaction(handle) {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            provider,
            "begin_transaction_with_options",
            "connection already has an active transaction".to_string(),
        );
    }
    let isolation = match value_to_string(isolation) {
        Ok(value) => value,
        Err(error) => {
            return sql_result_err_context(
                SqlErrorKind::Invalid,
                provider,
                "begin_transaction_with_options",
                error,
            );
        }
    };
    let read_only = match unsafe { read_only.as_ref() } {
        Some(Value::Bool(value)) => *value,
        _ => {
            return sql_result_err_context(
                SqlErrorKind::Invalid,
                provider,
                "begin_transaction_with_options",
                "read_only must be a bool".to_string(),
            );
        }
    };
    let deferrable = match unsafe { deferrable.as_ref() } {
        Some(Value::Bool(value)) => *value,
        _ => {
            return sql_result_err_context(
                SqlErrorKind::Invalid,
                provider,
                "begin_transaction_with_options",
                "deferrable must be a bool".to_string(),
            );
        }
    };
    let Ok(conn) = take_connection(handle) else {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            provider,
            "begin_transaction_with_options",
            "invalid sql connection handle".to_string(),
        );
    };
    let mut conn = conn;
    if let Err(error) =
        begin_transaction_on_connection_with_options(&mut conn, &isolation, read_only, deferrable)
    {
        return_connection(handle, conn);
        return sql_result_err_failure(
            SqlErrorKind::Database,
            provider,
            "begin_transaction_with_options",
            error,
        );
    }
    let tx_handle = store_transaction(SqlTransaction {
        connection_handle: handle,
        provider,
        connection: Some(conn),
        active: true,
        parent: None,
        owner: connection,
    });
    unsafe { mux_rc_inc(connection) };
    sql_result_handle(tx_handle, *SQL_TRANSACTION_TYPE_ID)
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_commit(transaction: *mut Value) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(h) => h,
        Err(err) => return sql_result_err(err),
    };
    sql_result_unit_failure_context(
        finish_transaction(handle, true),
        transaction_provider(handle),
        "commit",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_rollback(transaction: *mut Value) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(h) => h,
        Err(err) => return sql_result_err(err),
    };
    sql_result_unit_failure_context(
        finish_transaction(handle, false),
        transaction_provider(handle),
        "rollback",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_execute(
    transaction: *mut Value,
    sql: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        transaction_execute_with_params(handle, &statement, &[])
    })();
    sql_result_i64_failure_context(result, transaction_provider(handle), "execute")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_execute_batch(
    transaction: *mut Value,
    sql: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = value_to_string(sql)
        .map_err(SqlFailure::from)
        .and_then(|statement| transaction_execute_batch(handle, &statement));
    sql_result_unit_failure_context(result, transaction_provider(handle), "execute_batch")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_execute_many(
    transaction: *mut Value,
    sql: *mut Value,
    rows: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql).map_err(SqlFailure::from)?;
        let rows = value_list_to_sql_param_rows(rows).map_err(SqlFailure::from)?;
        transaction_execute_many(handle, &statement, &rows)
    })();
    sql_result_i64_failure_context(result, transaction_provider(handle), "execute_many")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_query(
    transaction: *mut Value,
    sql: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        transaction_query_with_params(handle, &statement, &[])
    })();
    sql_result_value_failure_context(result, transaction_provider(handle), "query")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_execute_params(
    transaction: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let sql_params = value_list_to_sql_params(params)?;
        transaction_execute_with_params(handle, &statement, &sql_params)
    })();
    sql_result_i64_failure_context(result, transaction_provider(handle), "execute_params")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_execute_named(
    transaction: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let named = value_map_to_named_params(params)?;
        with_transaction(handle, |tx| {
            if !tx.active {
                return Err(SqlFailure::plain("transaction is no longer active"));
            }
            let connection = tx
                .connection
                .as_mut()
                .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
            match connection {
                SqlConnection::Sqlite(conn) => {
                    let (sql, params) = rewrite_named_sql(&statement, SqlBackend::Sqlite, &named)?;
                    Ok::<_, SqlFailure>(sqlite_execute(conn, &sql, &params)?)
                }
                SqlConnection::Postgres(client) => {
                    let (sql, params) =
                        rewrite_named_sql(&statement, SqlBackend::Postgres, &named)?;
                    Ok::<_, SqlFailure>(postgres_execute(client, &sql, &params)?)
                }
                SqlConnection::MySql(conn) => {
                    let (sql, params) = rewrite_named_sql(&statement, SqlBackend::MySql, &named)?;
                    Ok::<_, SqlFailure>(mysql_execute(conn, &sql, &params)?)
                }
                SqlConnection::SqlServer(conn) => {
                    let (sql, params) =
                        rewrite_named_sql(&statement, SqlBackend::SqlServer, &named)?;
                    Ok::<_, SqlFailure>(sqlserver_execute(conn, &sql, &params)?)
                }
            }
        })
    })();
    sql_result_i64_failure_context(result, transaction_provider(handle), "execute_named")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_query_params(
    transaction: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let sql_params = value_list_to_sql_params(params)?;
        transaction_query_with_params(handle, &statement, &sql_params)
    })();
    sql_result_value_failure_context(result, transaction_provider(handle), "query_params")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_query_named(
    transaction: *mut Value,
    sql: *mut Value,
    params: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let named = value_map_to_named_params(params)?;
        with_transaction(handle, |tx| {
            if !tx.active {
                return Err(SqlFailure::plain("transaction is no longer active"));
            }
            let connection = tx
                .connection
                .as_mut()
                .ok_or_else(|| SqlFailure::plain("transaction connection missing"))?;
            let resultset = match connection {
                SqlConnection::Sqlite(conn) => {
                    let (sql, params) = rewrite_named_sql(&statement, SqlBackend::Sqlite, &named)?;
                    sqlite_query_streaming(conn, &sql, &params)
                }
                SqlConnection::Postgres(client) => {
                    let (sql, params) =
                        rewrite_named_sql(&statement, SqlBackend::Postgres, &named)?;
                    postgres_query_streaming(client, &sql, &params)
                }
                SqlConnection::MySql(conn) => {
                    let (sql, params) = rewrite_named_sql(&statement, SqlBackend::MySql, &named)?;
                    mysql_query_streaming(conn, &sql, &params)
                }
                SqlConnection::SqlServer(conn) => {
                    let (sql, params) =
                        rewrite_named_sql(&statement, SqlBackend::SqlServer, &named)?;
                    sqlserver_query_streaming(conn, &sql, &params)
                }
            }?;
            let rs_handle = store_transaction_resultset(resultset, handle);
            create_resultset_value(rs_handle)
        })
    })();
    sql_result_value_failure_context(result, transaction_provider(handle), "query_named")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_query_with_timeout(
    transaction: *mut Value,
    sql: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        transaction_query_with_timeout(handle, &statement, &[], timeout_ms)
    })();
    sql_result_value_failure_context(result, transaction_provider(handle), "query_with_timeout")
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_query_params_with_timeout(
    transaction: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let sql_params = value_list_to_sql_params(params)?;
        transaction_query_with_timeout(handle, &statement, &sql_params, timeout_ms)
    })();
    sql_result_value_failure_context(
        result,
        transaction_provider(handle),
        "query_params_with_timeout",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_query_named_with_timeout(
    transaction: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    timeout_ms: i64,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let named = value_map_to_named_params(params)?;
        transaction_query_named_with_timeout(handle, &statement, &named, timeout_ms)
    })();
    sql_result_value_failure_context(
        result,
        transaction_provider(handle),
        "query_named_with_timeout",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_query_with_cancellation(
    transaction: *mut Value,
    sql: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        transaction_query_with_cancellation(handle, &statement, &[], token)
    })();
    sql_result_value_failure_context(
        result,
        transaction_provider(handle),
        "query_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_query_params_with_cancellation(
    transaction: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let sql_params = value_list_to_sql_params(params)?;
        transaction_query_with_cancellation(handle, &statement, &sql_params, token)
    })();
    sql_result_value_failure_context(
        result,
        transaction_provider(handle),
        "query_params_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_transaction_query_named_with_cancellation(
    transaction: *mut Value,
    sql: *mut Value,
    params: *mut Value,
    token: *mut Value,
) -> *mut Value {
    let handle = match transaction_handle(transaction) {
        Ok(handle) => handle,
        Err(error) => return sql_result_err(error),
    };
    let result = (|| {
        let statement = value_to_string(sql)?;
        let named = value_map_to_named_params(params)?;
        transaction_query_named_with_cancellation(handle, &statement, &named, token)
    })();
    sql_result_value_failure_context(
        result,
        transaction_provider(handle),
        "query_named_with_cancellation",
    )
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_resultset_close(resultset: *mut Value) -> *mut Value {
    let result = resultset_handle(resultset)
        .map_err(SqlFailure::plain)
        .and_then(|handle| with_resultset(handle, |rs| Ok(finish_resultset(rs, true))));
    match result {
        Ok(leases) => {
            release_resultset_leases(leases.0, leases.1, leases.2);
            sql_result_unit_failure_context(Ok(()), "unknown", "resultset_close")
        }
        Err(error) => sql_result_unit_failure_context(Err(error), "unknown", "resultset_close"),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_resultset_rows(resultset: *mut Value) -> *mut Value {
    let result = resultset_handle(resultset)
        .map_err(SqlFailure::plain)
        .and_then(|handle| {
            with_resultset(handle, |rs| {
                if rs.closed {
                    return Ok(Ok((Value::List(Vec::new()), (None, None, None))));
                }
                let mut values = Vec::new();
                loop {
                    match resultset_next_row(rs) {
                        Ok(Some(row)) => values.push(row),
                        Ok(None) => break,
                        Err(error) => {
                            let provider = resultset_provider(rs);
                            let leases = finish_resultset(rs, true);
                            return Ok(Err((error, provider, leases)));
                        }
                    }
                }
                let leases = finish_resultset(rs, false);
                Ok(Ok((Value::List(values), leases)))
            })
        });
    match result {
        Ok(Ok((value, leases))) => {
            release_resultset_leases(leases.0, leases.1, leases.2);
            sql_result_ok(value)
        }
        Ok(Err((error, provider, leases))) => {
            release_resultset_leases(leases.0, leases.1, leases.2);
            sql_result_err_failure(SqlErrorKind::Database, provider, "resultset_rows", error)
        }
        Err(error) => {
            sql_result_err_failure(SqlErrorKind::Invalid, "unknown", "resultset_rows", error)
        }
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_resultset_next(resultset: *mut Value) -> *mut Value {
    let result = resultset_handle(resultset)
        .map_err(SqlFailure::plain)
        .and_then(|handle| {
            with_resultset(handle, |rs| {
                if rs.closed {
                    return Ok(Ok((Value::Optional(None), (None, None, None))));
                }
                let value = match resultset_next_row(rs) {
                    Ok(Some(row)) => Value::Optional(Some(Box::new(row))),
                    Ok(None) => {
                        rs.sqlite_cursor.take();
                        rs.postgres_cursor.take();
                        rs.mysql_cursor.take();
                        rs.sqlserver_cursor.take();
                        Value::Optional(None)
                    }
                    Err(error) => {
                        let provider = resultset_provider(rs);
                        let leases = finish_resultset(rs, true);
                        return Ok(Err((error, provider, leases)));
                    }
                };
                let leases = if !resultset_has_active_cursor(rs)
                    && rs.next_ordered_index == rs.ordered_rows.len()
                {
                    finish_resultset(rs, false)
                } else {
                    (None, None, None)
                };
                Ok(Ok((value, leases)))
            })
        });
    match result {
        Ok(Ok((value, leases))) => {
            release_resultset_leases(leases.0, leases.1, leases.2);
            sql_result_ok(value)
        }
        Ok(Err((error, provider, leases))) => {
            release_resultset_leases(leases.0, leases.1, leases.2);
            sql_result_err_failure(SqlErrorKind::Database, provider, "resultset_next", error)
        }
        Err(error) => {
            sql_result_err_failure(SqlErrorKind::Invalid, "unknown", "resultset_next", error)
        }
    }
}

/// Consume at most `limit` rows from the result set's cursor.
///
/// A non-positive limit consumes no rows. Invalid result-set handles return a
/// typed SQL error.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_resultset_next_batch(resultset: *mut Value, limit: i64) -> *mut Value {
    let result = resultset_handle(resultset)
        .map_err(SqlFailure::plain)
        .and_then(|handle| {
            with_resultset(handle, |rs| {
                if rs.closed {
                    return Ok(Ok((Value::List(Vec::new()), (None, None, None))));
                }
                let Some(requested) = usize::try_from(limit).ok() else {
                    return Ok(Ok((Value::List(Vec::new()), (None, None, None))));
                };
                if requested == 0 {
                    return Ok(Ok((Value::List(Vec::new()), (None, None, None))));
                }
                let mut values = Vec::new();
                let mut at_eof = false;
                while values.len() < requested {
                    match resultset_next_row(rs) {
                        Ok(Some(row)) => values.push(row),
                        Ok(None) => {
                            at_eof = true;
                            break;
                        }
                        Err(error) => {
                            let provider = resultset_provider(rs);
                            let leases = finish_resultset(rs, true);
                            return Ok(Err((error, provider, leases)));
                        }
                    }
                }
                let leases = if at_eof {
                    finish_resultset(rs, false)
                } else {
                    (None, None, None)
                };
                Ok(Ok((Value::List(values), leases)))
            })
        });
    match result {
        Ok(Ok((value, leases))) => {
            release_resultset_leases(leases.0, leases.1, leases.2);
            sql_result_ok(value)
        }
        Ok(Err((error, provider, leases))) => {
            release_resultset_leases(leases.0, leases.1, leases.2);
            sql_result_err_failure(
                SqlErrorKind::Database,
                provider,
                "resultset_next_batch",
                error,
            )
        }
        Err(error) => sql_result_err_failure(
            SqlErrorKind::Invalid,
            "unknown",
            "resultset_next_batch",
            error,
        ),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_row_columns(row: *mut Value) -> *mut Value {
    let result = row_handle(row).and_then(|handle| {
        SQL_ROWS.with(|rows| {
            let rows = rows.borrow();
            let value = rows
                .get(&handle)
                .ok_or_else(|| "invalid sql row handle".to_string())?;
            Ok(Value::List(
                value.columns.iter().cloned().map(Value::String).collect(),
            ))
        })
    });
    match result {
        Ok(value) => crate::refcount::mux_rc_alloc(value),
        Err(_) => crate::refcount::mux_rc_alloc(Value::List(vec![])),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_row_values(row: *mut Value) -> *mut Value {
    let result = row_handle(row).and_then(|handle| {
        SQL_ROWS.with(|rows| {
            let rows = rows.borrow();
            rows.get(&handle)
                .map(|value| Value::List(value.values.clone()))
                .ok_or_else(|| "invalid sql row handle".to_string())
        })
    });
    match result {
        Ok(value) => crate::refcount::mux_rc_alloc(value),
        Err(_) => crate::refcount::mux_rc_alloc(Value::List(vec![])),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_row_at(row: *mut Value, index: i64) -> *mut Value {
    let provider = row_handle(row).map_or("unknown", row_provider);
    let result = row_handle(row).and_then(|handle| {
        let index =
            usize::try_from(index).map_err(|_| "row index must be non-negative".to_string())?;
        SQL_ROWS.with(|rows| {
            let rows = rows.borrow();
            let value = rows
                .get(&handle)
                .ok_or_else(|| "invalid sql row handle".to_string())?;
            value
                .values
                .get(index)
                .cloned()
                .ok_or_else(|| "row index is out of bounds".to_string())
        })
    });
    match result {
        Ok(value) => sql_result_ok(value),
        Err(error) => sql_result_err_context(SqlErrorKind::Invalid, provider, "row_at", error),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_row_get(row: *mut Value, name: *mut Value) -> *mut Value {
    let provider = row_handle(row).map_or("unknown", row_provider);
    let Some(Value::String(name)) = (unsafe { name.as_ref() }) else {
        return sql_result_err_context(
            SqlErrorKind::Invalid,
            provider,
            "row_get",
            "row column name must be a string".to_string(),
        );
    };
    let result = row_handle(row).and_then(|handle| {
        SQL_ROWS.with(|rows| {
            let rows = rows.borrow();
            let value = rows
                .get(&handle)
                .ok_or_else(|| "invalid sql row handle".to_string())?;
            let mut matches = value
                .columns
                .iter()
                .zip(value.values.iter())
                .filter(|(column, _)| column.as_str() == name)
                .map(|(_, value)| value);
            let Some(first) = matches.next() else {
                return Err(format!("unknown SQL column: {name}"));
            };
            if matches.next().is_some() {
                return Err(format!("ambiguous SQL column name: {name}"));
            }
            Ok(first.clone())
        })
    });
    match result {
        Ok(value) => sql_result_ok(value),
        Err(error) => sql_result_err_context(SqlErrorKind::Invalid, provider, "row_get", error),
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mux_sql_resultset_columns(resultset: *mut Value) -> *mut Value {
    let result = resultset_handle(resultset)
        .map_err(SqlFailure::plain)
        .and_then(|handle| {
            with_resultset(handle, |rs| {
                let columns = rs
                    .columns
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<_>>();
                Ok(Value::List(columns))
            })
        });
    match result {
        Ok(value) => crate::refcount::mux_rc_alloc(value),
        Err(_err) => crate::refcount::mux_rc_alloc(Value::List(vec![])),
    }
}

fn sql_error_field_value(
    error: *const Value,
    get: fn(&SqlErrorEntry) -> String,
) -> Result<String, String> {
    let handle = require_handle(error, *SQL_ERROR_TYPE_ID, "sql error")?;
    let errors = SQL_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = errors
        .get(&handle)
        .ok_or_else(|| "invalid sql error handle".to_string())?;
    Ok(get(entry))
}

fn sql_error_text(error: *const Value, decorated: bool) -> String {
    let handle = require_handle(error, *SQL_ERROR_TYPE_ID, "sql error");
    let errors = SQL_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = handle.ok().and_then(|handle| errors.get(&handle)) else {
        return "invalid sql error handle".to_string();
    };
    if decorated {
        format!("{}: {}", entry.kind.as_str(), entry.detail)
    } else {
        entry.detail.clone()
    }
}

#[unsafe(no_mangle)]
/// Creates a typed SQL error from a displayable message.
///
/// # Safety
/// `message` must be a valid pointer to a live Mux `Value` for the duration
/// of the call, or null when the caller wants the fallback detail.
pub unsafe extern "C" fn mux_sql_error_from_message(message: *const Value) -> *mut Value {
    let detail = value_to_string(message as *mut Value)
        .unwrap_or_else(|_| "invalid SQL error detail".to_string());
    let value = sql_error_value(SqlErrorKind::Database, "unknown", String::new(), detail)
        .unwrap_or_else(|_| Value::String("could not allocate SQL error".to_string()));
    mux_rc_alloc(value)
}

macro_rules! sql_error_string_getter {
    ($name:ident, $get:expr) => {
        #[unsafe(no_mangle)]
        /// Reads one string field from a typed SQL error.
        ///
        /// # Safety
        /// `error` must be a valid pointer to a live Mux `Value` for the
        /// duration of the call, or null for an invalid-handle response.
        pub unsafe extern "C" fn $name(error: *const Value) -> *mut Value {
            let value = sql_error_field_value(error, $get)
                .map(Value::String)
                .unwrap_or_else(|message| Value::String(message));
            mux_rc_alloc(value)
        }
    };
}

/// Box an SQL category using the same raw `{ i32 discriminant }` layout that
/// codegen uses for the payload-less `SqlErrorKind` enum. Compiler codegen
/// immediately unboxes this value on `error.kind` access.
fn sql_error_kind_value(kind: SqlErrorKind) -> Value {
    Value::Opaque((kind as i32).to_ne_bytes().to_vec().into_boxed_slice())
}

/// Reads the typed category from a SQL error.
///
/// # Safety
/// `error` must be a valid pointer to a live Mux `Value` for the duration of
/// the call, or null for an invalid-handle response.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_sql_error_kind(error: *const Value) -> *mut Value {
    let handle = require_handle(error, *SQL_ERROR_TYPE_ID, "sql error");
    let errors = SQL_ERRORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let value = handle
        .ok()
        .and_then(|handle| errors.get(&handle))
        .map_or_else(
            || sql_error_kind_value(SqlErrorKind::Invalid),
            |entry| sql_error_kind_value(entry.kind),
        );
    mux_rc_alloc(value)
}

sql_error_string_getter!(mux_sql_error_detail, |entry| entry.detail.clone());
sql_error_string_getter!(mux_sql_error_provider, |entry| entry.provider.clone());
sql_error_string_getter!(mux_sql_error_code, |entry| entry.code.clone());
sql_error_string_getter!(mux_sql_error_constraint, |entry| entry.constraint.clone());
sql_error_string_getter!(mux_sql_error_operation, |entry| entry.operation.clone());

#[unsafe(no_mangle)]
/// Returns the detail message from a typed SQL error.
///
/// # Safety
/// `error` must be a valid pointer to a live Mux `Value` for the duration of
/// the call, or null for an invalid-handle response.
pub unsafe extern "C" fn mux_sql_error_message(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(sql_error_text(error, false)))
}

#[unsafe(no_mangle)]
/// Returns a decorated string representation of a typed SQL error.
///
/// # Safety
/// `error` must be a valid pointer to a live Mux `Value` for the duration of
/// the call, or null for an invalid-handle response.
pub unsafe extern "C" fn mux_sql_error_to_string(error: *const Value) -> *mut Value {
    mux_rc_alloc(Value::String(sql_error_text(error, true)))
}

#[cfg(test)]
mod tests {
    use super::{
        create_resultset_value, mux_sql_error_code, mux_sql_error_operation,
        mux_sql_error_provider, mux_sql_resultset_next, mysql_interrupted_error,
        mysql_value_to_mux, parse_sqlserver_uri, rewrite_named_sql, rewrite_positional_sql,
        split_sql_batch, sql_interrupt_capabilities, sqlite_cursor_columns, store_resultset,
        validate_migration_set, MigrationDefinition, MySqlValue, SqlBackend, SqlErrorKind,
        SqlFailure, SqlParam, SqlResultSet, SqliteConnection, SqliteCursor,
        MAX_MIGRATION_TOTAL_BYTES, MAX_SQL_BYTES,
    };
    use crate::optional::{mux_optional_data, mux_optional_is_some};
    use crate::refcount::{mux_rc_alloc, mux_rc_dec};
    use crate::result::{mux_result_data, mux_result_is_err, mux_result_is_ok};
    use crate::Value;
    use std::collections::HashMap;
    use std::ffi::CString;

    #[test]
    fn mysql_interrupt_capability_and_diagnostic_mapping_are_typed() {
        let capabilities = sql_interrupt_capabilities("mysql");
        assert!(capabilities.timeout);
        assert!(capabilities.cancellation);

        let mut interrupted = SqlFailure::plain("query interrupted");
        interrupted.diagnostic.vendor_code = "1317".to_string();
        assert!(mysql_interrupted_error(&interrupted));
        assert_eq!(
            interrupted.clone().timeout().kind,
            Some(SqlErrorKind::Timeout)
        );
        assert_eq!(interrupted.cancelled().kind, Some(SqlErrorKind::Cancelled));

        let mut unrelated = SqlFailure::plain("syntax error");
        unrelated.diagnostic.vendor_code = "1064".to_string();
        assert!(!mysql_interrupted_error(&unrelated));

        let mut sqlstate = SqlFailure::plain("query interrupted");
        sqlstate.diagnostic.sqlstate = "70100".to_string();
        assert!(mysql_interrupted_error(&sqlstate));
    }

    #[test]
    fn sqlserver_uri_defaults_to_verified_tls_and_decodes_components() {
        let parsed =
            parse_sqlserver_uri("sqlserver://user:p%40ss@example.test/analytics%20db?encrypt=true")
                .expect("valid SQL Server URI should parse");
        assert_eq!(parsed.host, "example.test");
        assert_eq!(parsed.port, 1433);
        assert_eq!(parsed.database, "analytics db");
        assert_eq!(parsed.username.as_deref(), Some("user"));
        assert_eq!(parsed.password.as_deref(), Some("p@ss"));
        assert!(parsed.verify_tls);
    }

    #[test]
    fn sqlserver_uri_requires_explicit_insecure_opt_out() {
        let parsed =
            parse_sqlserver_uri("mssql://user:pass@[::1]:1444/db?trustServerCertificate=yes")
                .expect("valid SQL Server URI should parse");
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.port, 1444);
        assert!(!parsed.verify_tls);

        let parsed = parse_sqlserver_uri("sqlserver://host/db?encrypt=maybe")
            .expect_err("invalid encryption option should fail");
        assert_eq!(parsed.kind, Some(super::SqlErrorKind::Invalid));
    }

    #[test]
    fn sqlserver_placeholders_use_named_tds_parameters() {
        let rewritten = rewrite_positional_sql(
            "SELECT * FROM users WHERE id = ? AND name = ?",
            SqlBackend::SqlServer,
            2,
        )
        .expect("SQL Server placeholders should be rewritten");
        assert_eq!(
            rewritten,
            "SELECT * FROM users WHERE id = @P1 AND name = @P2"
        );

        let mut named = HashMap::new();
        named.insert("id".to_string(), SqlParam::Int(7));
        let (rewritten, params) = rewrite_named_sql(
            "SELECT * FROM users WHERE id = :id",
            SqlBackend::SqlServer,
            &named,
        )
        .expect("SQL Server named parameters should be rewritten");
        assert_eq!(rewritten, "SELECT * FROM users WHERE id = @P1");
        assert!(matches!(params.as_slice(), [SqlParam::Int(7)]));
    }

    #[test]
    fn positional_rewrite_preserves_dollar_quoted_bodies() {
        let sql = "SELECT $$? $1 :name - literal$$, ?";
        let rewritten = rewrite_positional_sql(sql, SqlBackend::Postgres, 1)
            .expect("dollar-quoted SQL should be rewritten");
        assert_eq!(rewritten, "SELECT $$? $1 :name - literal$$, $1");
    }

    #[test]
    fn named_rewrite_preserves_tagged_dollar_quoted_bodies() {
        let sql = "SELECT $body$? :name $1$body$, :name";
        let mut named = HashMap::new();
        named.insert("name".to_string(), SqlParam::Int(42));
        let (rewritten, params) = rewrite_named_sql(sql, SqlBackend::Postgres, &named)
            .expect("dollar-quoted SQL should be rewritten");
        assert_eq!(rewritten, "SELECT $body$? :name $1$body$, $1");
        assert!(matches!(params.as_slice(), [SqlParam::Int(42)]));
    }

    #[test]
    fn unterminated_dollar_quoted_body_is_rejected() {
        let error = rewrite_positional_sql("SELECT $$?", SqlBackend::Postgres, 0)
            .expect_err("unterminated dollar quote must fail");
        assert_eq!(error, "SQL contains an unterminated dollar-quoted string");
    }

    #[test]
    fn batch_split_preserves_semicolons_in_dollar_quoted_bodies() {
        let sql = concat!(
            "CREATE FUNCTION notify_items() RETURNS void AS $$\n",
            "BEGIN\n",
            "  PERFORM pg_notify('items', 'created;');\n",
            "END\n",
            "$$ LANGUAGE plpgsql;"
        );
        let statements = split_sql_batch(sql, SqlBackend::Postgres)
            .expect("dollar-quoted body should be opaque");
        assert_eq!(statements, vec![sql.trim_end_matches(';').to_string()]);
    }

    #[test]
    fn batch_split_rejects_unterminated_dollar_quoted_bodies() {
        let error = split_sql_batch(
            "CREATE FUNCTION f() RETURNS void AS $$BEGIN;",
            SqlBackend::Postgres,
        )
        .expect_err("unterminated dollar quote must fail");
        assert_eq!(error, "SQL contains an unterminated dollar-quoted string");
    }

    #[test]
    fn postgres_escape_strings_keep_placeholders_inside_literals() {
        let sql = r"SELECT E'quoted \'? :name', ?";
        let rewritten = rewrite_positional_sql(sql, SqlBackend::Postgres, 1)
            .expect("placeholder outside an escape string should be rewritten");
        assert_eq!(rewritten, r"SELECT E'quoted \'? :name', $1");

        let mut named = HashMap::new();
        named.insert("value".to_string(), SqlParam::Int(7));
        let (rewritten, params) = rewrite_named_sql(
            r"SELECT E'quoted \'? :name', :value",
            SqlBackend::Postgres,
            &named,
        )
        .expect("named placeholder outside an escape string should be rewritten");
        assert_eq!(rewritten, r"SELECT E'quoted \'? :name', $1");
        assert!(matches!(params.as_slice(), [SqlParam::Int(7)]));
    }

    #[test]
    fn mysql_backslash_escapes_keep_batch_delimiters_inside_literals() {
        let sql = r"INSERT INTO items (value) VALUES ('escaped \' quote; still text'); SELECT 1;";
        let statements = split_sql_batch(sql, SqlBackend::MySql)
            .expect("escaped MySQL quote should not end the literal");
        assert_eq!(statements.len(), 2);
        assert_eq!(
            statements[0],
            "INSERT INTO items (value) VALUES ('escaped \\' quote; still text')"
        );
        assert_eq!(statements[1], "SELECT 1");
    }

    #[test]
    fn sqlite_backslashes_remain_ordinary_literal_characters() {
        let sql = r"SELECT '\\', ?";
        let rewritten = rewrite_positional_sql(sql, SqlBackend::Sqlite, 1)
            .expect("SQLite backslashes are not escape syntax");
        assert_eq!(rewritten, "SELECT '\\\\', ?");
    }

    #[test]
    fn sql_scanners_reject_oversized_statements_before_processing() {
        let sql = "x".repeat(MAX_SQL_BYTES + 1);
        let positional = rewrite_positional_sql(&sql, SqlBackend::Sqlite, 0)
            .expect_err("oversized positional SQL must be rejected");
        assert_eq!(positional, "SQL statement exceeds the 16 MiB limit");

        let Err(named) = rewrite_named_sql(&sql, SqlBackend::Sqlite, &HashMap::new()) else {
            panic!("oversized named SQL must be rejected")
        };
        assert_eq!(named, "SQL statement exceeds the 16 MiB limit");

        let batch = split_sql_batch(&sql, SqlBackend::Sqlite)
            .expect_err("oversized batch SQL must be rejected");
        assert_eq!(batch, "SQL batch exceeds the 16 MiB limit");
    }

    #[test]
    fn mysql_binary_columns_preserve_valid_utf8_as_bytes() {
        let binary = mysql_value_to_mux(MySqlValue::Bytes(b"hello".to_vec()), true);
        assert!(matches!(binary, Value::Bytes(value) if value == b"hello"));

        let text = mysql_value_to_mux(MySqlValue::Bytes(b"hello".to_vec()), false);
        assert!(matches!(text, Value::String(value) if value == "hello"));
    }

    #[test]
    fn sqlite_streaming_failure_preserves_provider_after_lease_cleanup() {
        let connection = SqliteConnection::open_in_memory().expect("open SQLite database");
        let database = unsafe { connection.handle() };
        let query = CString::new("SELECT 1 AS value UNION ALL SELECT 2")
            .expect("query should not contain NUL");
        let mut statement = std::ptr::null_mut();
        let mut tail = std::ptr::null();
        let status = unsafe {
            rusqlite::ffi::sqlite3_prepare_v2(
                database,
                query.as_ptr(),
                -1,
                &mut statement,
                &mut tail,
            )
        };
        assert_eq!(status, rusqlite::ffi::SQLITE_OK);
        let columns = sqlite_cursor_columns(statement).expect("read SQLite columns");
        let handle = store_resultset(SqlResultSet {
            ordered_rows: Vec::new(),
            columns,
            next_ordered_index: 0,
            closed: false,
            sqlite_cursor: Some(SqliteCursor {
                database,
                statement,
            }),
            postgres_cursor: None,
            mysql_cursor: None,
            sqlserver_cursor: None,
            connection_lease: None,
            transaction_lease: None,
            pool_lease: None,
        });
        let resultset = mux_rc_alloc(create_resultset_value(handle).expect("allocate resultset"));

        let first = mux_sql_resultset_next(resultset);
        assert!(unsafe { mux_result_is_ok(first) });
        let first_data = unsafe { mux_result_data(first) };
        assert!(unsafe { mux_optional_is_some(first_data) });
        let first_row = unsafe { mux_optional_data(first_data) };
        assert!(!first_row.is_null());
        unsafe {
            assert!(mux_rc_dec(first_row));
            assert!(mux_rc_dec(first_data));
            assert!(mux_rc_dec(first));
            rusqlite::ffi::sqlite3_interrupt(database);
        }

        let failure = mux_sql_resultset_next(resultset);
        assert!(unsafe { mux_result_is_err(failure) });
        let error = unsafe { mux_result_data(failure) };
        let provider = unsafe { mux_sql_error_provider(error) };
        let code = unsafe { mux_sql_error_code(error) };
        let operation = unsafe { mux_sql_error_operation(error) };
        assert!(unsafe {
            matches!(&*provider, Value::String(value) if value == "sqlite")
                && matches!(&*code, Value::String(value) if value == "9")
                && matches!(&*operation, Value::String(value) if value == "resultset_next")
        });
        unsafe {
            assert!(mux_rc_dec(operation));
            assert!(mux_rc_dec(code));
            assert!(mux_rc_dec(provider));
            assert!(mux_rc_dec(error));
            assert!(mux_rc_dec(failure));
            assert!(mux_rc_dec(resultset));
        }
    }

    #[test]
    fn in_memory_migration_sets_enforce_aggregate_sql_limit() {
        let chunk = "x".repeat(MAX_MIGRATION_TOTAL_BYTES / 8);
        let mut definitions = Vec::new();
        for version in 1..=4 {
            definitions.push(MigrationDefinition {
                version,
                name: format!("migration-{version}"),
                checksum: String::new(),
                up: chunk.clone(),
                down: chunk.clone(),
            });
        }
        definitions.push(MigrationDefinition {
            version: 5,
            name: "one-byte-over".to_string(),
            checksum: String::new(),
            up: "x".to_string(),
            down: "x".to_string(),
        });

        let Err(error) = validate_migration_set(definitions) else {
            panic!("in-memory migrations must share the aggregate SQL limit");
        };
        assert_eq!(error, "migration set exceeds the 64 MiB limit");
    }
}
