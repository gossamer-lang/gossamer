//! C-ABI shims for `std::database::sql`.
//!
//! Each `gos_rt_sql_*` symbol is callable directly from compiled
//! Gossamer code. The shims operate on handle registries
//! (Conn / Stmt / Rows / Row / Tx / Params) that store the trait-object
//! pointers behind opaque `i64` handles so the compiled tier
//! never sees a Rust trait fat-pointer.
//!
//! The shims share one process-global registry with `gos`:
//! the interpreter's `__gos_sql_*_raw` builtins call the same safe
//! core functions (`sql_open_handle`, `sql_conn_execute_params`, …)
//! that these shims marshal to, so handles round-trip across tier
//! boundaries and semantics are identical by construction.

#![allow(clippy::missing_safety_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::too_many_lines)]
#![allow(missing_docs)]

use std::collections::HashMap;
use std::os::raw::c_char;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;

use crate::sql::{
    ConnectionImpl, Driver, Error, IsolationLevel, Notification, RowsImpl, StatementImpl,
    TransactionImpl, Value,
};

// Exported under an unmangled name on purpose: a `gos build` binary
// with `[rust-bindings]` links TWO copies of gossamer-runtime (the
// runtime staticlib and the bindings staticlib) under
// `--allow-multiple-definition`. Identical code is harmless, but a
// crate-internal static would exist once per copy - the binding's
// `register()` would write one registry while `gos_rt_sql_open`
// reads the other. An unmangled symbol is deduplicated by the
// linker, so every copy shares this one storage location.
#[unsafe(no_mangle)]
static GOS_RT_SQL_DRIVER_REGISTRY: Mutex<Vec<Arc<dyn Driver>>> = Mutex::new(Vec::new());

/// The process-wide SQL driver registry shared across linked
/// gossamer-runtime copies.
pub(crate) fn driver_registry() -> &'static Mutex<Vec<Arc<dyn Driver>>> {
    &GOS_RT_SQL_DRIVER_REGISTRY
}

// --- helpers -------------------------------------------------------

/// # Safety
///
/// `p` is null or a live string body.
unsafe fn c_str_to_string(p: *const c_char) -> String {
    // SAFETY: callers pass a Gossamer `String`, read through its length
    // header; non-UTF-8 falls back to the empty string.
    unsafe { crate::c_abi::gos_str_arg_text(p) }.to_string()
}

fn alloc_cstring(bytes: &[u8]) -> *mut c_char {
    super::alloc_cstring(bytes)
}

fn empty_cstring() -> *mut c_char {
    alloc_cstring(b"")
}

// --- handle registries --------------------------------------------

static CONN_HANDLES: Mutex<Option<HashMap<i64, Box<dyn ConnectionImpl>>>> = Mutex::new(None);
static STMT_HANDLES: Mutex<Option<HashMap<i64, StmtEntry>>> = Mutex::new(None);
static ROWS_HANDLES: Mutex<Option<HashMap<i64, RowsEntry>>> = Mutex::new(None);
static ROW_HANDLES: Mutex<Option<HashMap<i64, Row>>> = Mutex::new(None);
static TX_HANDLES: Mutex<Option<HashMap<i64, TxEntry>>> = Mutex::new(None);
static PARAMS_HANDLES: Mutex<Option<HashMap<i64, Vec<Value>>>> = Mutex::new(None);
static POOL_HANDLES: Mutex<Option<HashMap<i64, crate::sql_pool::Pool>>> = Mutex::new(None);
/// Most recent notification delivered by `sql_conn_poll_notification`,
/// keyed by connection handle; the scalar getter shims read it.
static LAST_NOTIFICATION: Mutex<Option<HashMap<i64, Notification>>> = Mutex::new(None);
/// Bytes produced by the most recent `copy_out_run`, keyed by
/// connection handle, until `copy_out_take` claims them.
static COPY_OUT_SLOTS: Mutex<Option<HashMap<i64, Vec<u8>>>> = Mutex::new(None);

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

// Goroutines are not thread-pinned, so this is a process-global slot
// rather than a thread-local: the injected wrappers read it on the
// same goroutine immediately after a failing call. Concurrent
// failures on different goroutines may interleave messages; the
// failure itself is still reported.
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

/// Records the message returned by the next `sql_take_last_error`.
pub fn sql_set_last_error(msg: impl Into<String>) {
    *LAST_ERROR.lock() = msg.into();
}

/// Returns and clears the most recent SQL error message.
pub fn sql_take_last_error() -> String {
    std::mem::take(&mut *LAST_ERROR.lock())
}

fn fail(msg: impl Into<String>) -> i64 {
    sql_set_last_error(msg);
    -1
}

/// One row resolved from a `RowsImpl::next_row`. Carries the column
/// metadata so `Row::get(&str)` can look up by name.
struct Row {
    values: Vec<Value>,
    columns: Vec<String>,
}

/// A live result-set cursor. `conn` is the owning connection handle
/// (closing the connection sweeps its cursors); `current_row` is the
/// Row handle returned by the most recent advance (0 = none). Rows
/// follow cursor semantics: advancing frees the previous Row, so a
/// Row is valid only until the next `next_row` / `close`.
struct RowsEntry {
    rows: Box<dyn RowsImpl>,
    conn: i64,
    current_row: i64,
}

/// A prepared statement; `conn` is the owning connection handle
/// (closing the connection sweeps its statements).
struct StmtEntry {
    stmt: Box<dyn StatementImpl>,
    conn: i64,
}

/// A live transaction; `conn` is the owning connection handle -
/// result-set cursors opened inside the transaction register under
/// it, and closing the connection sweeps surviving transactions
/// (driver `Drop` rolls back).
struct TxEntry {
    tx: Box<dyn TransactionImpl>,
    conn: i64,
}

fn next_handle() -> i64 {
    NEXT_HANDLE.fetch_add(1, Ordering::AcqRel)
}

fn conn_register(c: Box<dyn ConnectionImpl>) -> i64 {
    let id = next_handle();
    let mut guard = CONN_HANDLES.lock();
    guard.get_or_insert_with(HashMap::new).insert(id, c);
    id
}

/// Removes a session from the registry while a driver call owns it.  Database
/// sessions are `Send` but deliberately not `Sync`, so taking the entry gives
/// one operation exclusive ownership without pinning the handle-registry lock
/// across a potentially unbounded driver call.
fn conn_take(handle: i64) -> Option<Box<dyn ConnectionImpl>> {
    let mut guard = CONN_HANDLES.lock();
    guard.as_mut()?.remove(&handle)
}

fn conn_reinsert(handle: i64, conn: Box<dyn ConnectionImpl>) {
    let mut guard = CONN_HANDLES.lock();
    guard.get_or_insert_with(HashMap::new).insert(handle, conn);
}

/// Runs one session operation on the blocking pool and restores the session
/// afterward.  While the operation is in flight, a competing use of the same
/// opaque handle observes it as unavailable rather than blocking on a global
/// registry mutex.  This matches the existing take/reinsert ownership model
/// for statements, rows, and transactions.
fn conn_run<R>(
    handle: i64,
    label: &'static str,
    f: impl FnOnce(&mut dyn ConnectionImpl) -> R + Send + 'static,
) -> Result<R, String>
where
    R: Send + 'static,
{
    let conn = conn_take(handle).ok_or_else(|| INVALID_CONN.to_string())?;
    let (conn, result) = crate::sched_global::run_blocking(label, move || {
        let mut conn = conn;
        let result = f(conn.as_mut());
        (conn, result)
    })?;
    conn_reinsert(handle, conn);
    Ok(result)
}

fn rows_register(r: Box<dyn RowsImpl>, conn: i64) -> i64 {
    let id = next_handle();
    let entry = RowsEntry {
        rows: r,
        conn,
        current_row: 0,
    };
    let mut guard = ROWS_HANDLES.lock();
    guard.get_or_insert_with(HashMap::new).insert(id, entry);
    id
}

fn rows_take(handle: i64) -> Option<RowsEntry> {
    let mut guard = ROWS_HANDLES.lock();
    guard.as_mut()?.remove(&handle)
}

fn rows_reinsert(handle: i64, entry: RowsEntry) {
    let mut guard = ROWS_HANDLES.lock();
    guard.get_or_insert_with(HashMap::new).insert(handle, entry);
}

fn row_unregister(handle: i64) {
    if handle <= 0 {
        return;
    }
    let mut guard = ROW_HANDLES.lock();
    if let Some(map) = guard.as_mut() {
        map.remove(&handle);
    }
}

fn row_register(r: Row) -> i64 {
    let id = next_handle();
    let mut guard = ROW_HANDLES.lock();
    guard.get_or_insert_with(HashMap::new).insert(id, r);
    id
}

fn row_with<R>(handle: i64, f: impl FnOnce(&Row) -> R) -> Option<R> {
    let guard = ROW_HANDLES.lock();
    let map = guard.as_ref()?;
    let r = map.get(&handle)?;
    Some(f(r))
}

fn tx_register(t: Box<dyn TransactionImpl>, conn: i64) -> i64 {
    let id = next_handle();
    let mut guard = TX_HANDLES.lock();
    guard
        .get_or_insert_with(HashMap::new)
        .insert(id, TxEntry { tx: t, conn });
    id
}

fn tx_take(handle: i64) -> Option<TxEntry> {
    let mut guard = TX_HANDLES.lock();
    guard.as_mut()?.remove(&handle)
}

fn tx_reinsert(handle: i64, t: TxEntry) {
    let mut guard = TX_HANDLES.lock();
    guard.get_or_insert_with(HashMap::new).insert(handle, t);
}

fn stmt_register(stmt: Box<dyn StatementImpl>, conn: i64) -> i64 {
    let id = next_handle();
    let mut guard = STMT_HANDLES.lock();
    guard
        .get_or_insert_with(HashMap::new)
        .insert(id, StmtEntry { stmt, conn });
    id
}

fn stmt_take(handle: i64) -> Option<StmtEntry> {
    let mut guard = STMT_HANDLES.lock();
    guard.as_mut()?.remove(&handle)
}

fn stmt_reinsert(handle: i64, entry: StmtEntry) {
    let mut guard = STMT_HANDLES.lock();
    guard.get_or_insert_with(HashMap::new).insert(handle, entry);
}

// --- value shims ---------------------------------------------------
//
// SQL `Value` is a wide enum. Compiled code holds opaque `i64`
// handles into a tagged-payload registry rather than trying to
// marshal the discriminator + payload into a single register.

static VALUE_HANDLES: Mutex<Option<HashMap<i64, Value>>> = Mutex::new(None);

fn value_register(v: Value) -> i64 {
    let id = next_handle();
    let mut guard = VALUE_HANDLES.lock();
    guard.get_or_insert_with(HashMap::new).insert(id, v);
    id
}

/// Removes the value behind `handle` and returns it. A missing
/// handle resolves to `Value::Null` so a driver passing a stale id
/// stores a NULL column rather than aborting.
fn value_take(handle: i64) -> Value {
    let mut guard = VALUE_HANDLES.lock();
    guard
        .as_mut()
        .and_then(|m| m.remove(&handle))
        .unwrap_or(Value::Null)
}

/// Clones the value behind `handle` without consuming it. A missing
/// handle resolves to `Value::Null`.
fn value_peek(handle: i64) -> Value {
    let guard = VALUE_HANDLES.lock();
    guard
        .as_ref()
        .and_then(|m| m.get(&handle).cloned())
        .unwrap_or(Value::Null)
}

// --- safe core -----------------------------------------------------
//
// One implementation shared by the C-ABI shims below and the
// interpreter's `__gos_sql_*_raw` builtins (gossamer-interp calls
// these directly). Sentinel conventions: handles are > 0; `-1`
// means error (`-2` for `open` means driver error vs `-1` unknown
// driver); `0` from `sql_rows_next_row` means end-of-set. Every
// error path records a message readable via `sql_take_last_error`.

const INVALID_CONN: &str = "sql: invalid connection handle";
const INVALID_ROWS: &str = "sql: invalid rows handle";
const INVALID_TX: &str = "sql: invalid transaction handle";

/// Opens a connection. Returns a Conn handle, -1 on unknown driver,
/// -2 on driver error.
pub fn sql_open_handle(name: &str, url: &str) -> i64 {
    let name = name.to_string();
    let url = url.to_string();
    match crate::sched_global::run_blocking("sql-open", move || crate::sql::open(&name, &url)) {
        Ok(Ok(conn)) => conn_register(conn),
        Ok(Err(e @ Error::UnknownDriver(_))) => {
            sql_set_last_error(e.to_string());
            -1
        }
        Ok(Err(e)) => {
            sql_set_last_error(e.to_string());
            -2
        }
        Err(e) => fail(e),
    }
}

/// Comma-joined registered driver names.
pub fn sql_drivers_joined() -> String {
    crate::sql::drivers().join(",")
}

/// Allocates a fresh parameter list and returns its handle.
pub fn sql_params_new() -> i64 {
    let id = next_handle();
    let mut guard = PARAMS_HANDLES.lock();
    guard
        .get_or_insert_with(HashMap::new)
        .insert(id, Vec::new());
    id
}

/// Appends a value to the parameter list. Returns 0, or -1 on a bad
/// handle.
pub fn sql_params_push(handle: i64, v: Value) -> i64 {
    let mut guard = PARAMS_HANDLES.lock();
    match guard.as_mut().and_then(|m| m.get_mut(&handle)) {
        Some(list) => {
            list.push(v);
            0
        }
        None => fail("sql: invalid params handle"),
    }
}

fn params_take(handle: i64) -> Vec<Value> {
    let mut guard = PARAMS_HANDLES.lock();
    guard
        .as_mut()
        .and_then(|m| m.remove(&handle))
        .unwrap_or_default()
}

/// Takes (consumes) a bound parameter list by handle. Exposed so the
/// interpreter's native-driver path can hand the params to a
/// `native_facade_*` op (the Rust-driver shims consume the list
/// internally).
pub fn params_take_public(handle: i64) -> Vec<Value> {
    params_take(handle)
}

/// Prepares + executes `sql` with the bound parameter list (consumed).
/// Returns rows affected, or -1 on error.
pub fn sql_conn_execute_params(handle: i64, sql: &str, params_handle: i64) -> i64 {
    let params = params_take(params_handle);
    let sql = sql.to_string();
    match conn_run(handle, "sql-conn-execute", move |c| match c.prepare(&sql) {
        Ok(mut stmt) => match stmt.execute(&params) {
            Ok(n) => n as i64,
            Err(e) => fail(e.to_string()),
        },
        Err(e) => fail(e.to_string()),
    }) {
        Ok(n) => n,
        Err(e) => fail(e),
    }
}

/// Prepares + queries `sql` with the bound parameter list (consumed).
/// Returns a Rows handle, or -1 on error.
pub fn sql_conn_query_params(handle: i64, sql: &str, params_handle: i64) -> i64 {
    let params = params_take(params_handle);
    let sql = sql.to_string();
    let result = conn_run(handle, "sql-conn-query", move |c| match c.prepare(&sql) {
        Ok(mut stmt) => stmt.query(&params),
        Err(e) => Err(e),
    });
    match result {
        Ok(Ok(rows)) => rows_register(rows, handle),
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Begins a transaction. Returns a Tx handle, or -1 on error.
pub fn sql_conn_begin(handle: i64) -> i64 {
    match conn_run(handle, "sql-conn-begin", |c| c.begin()) {
        Ok(Ok(tx)) => tx_register(tx, handle),
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Begins a transaction at isolation level `iso` (0=Default,
/// 1=ReadUncommitted, 2=ReadCommitted, 3=RepeatableRead,
/// 4=Serializable). Returns a Tx handle, or -1 on error.
pub fn sql_conn_begin_with(handle: i64, iso: i64) -> i64 {
    let level = match iso {
        1 => IsolationLevel::ReadUncommitted,
        2 => IsolationLevel::ReadCommitted,
        3 => IsolationLevel::RepeatableRead,
        4 => IsolationLevel::Serializable,
        _ => IsolationLevel::Default,
    };
    match conn_run(handle, "sql-conn-begin", move |c| c.begin_with(level)) {
        Ok(Ok(tx)) => tx_register(tx, handle),
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Pings the connection. Returns 0 on success, -1 on error.
pub fn sql_conn_ping(handle: i64) -> i64 {
    match conn_run(handle, "sql-conn-ping", |c| c.ping()) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Sets the driver busy timeout. Returns 0 on success, -1 on error.
pub fn sql_conn_set_busy_timeout(handle: i64, ms: i64) -> i64 {
    match conn_run(handle, "sql-conn-set-busy-timeout", move |c| {
        c.set_busy_timeout(ms)
    }) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Cancels any in-flight statement on the connection. Returns 0, or
/// -1 on a bad handle.
pub fn sql_conn_interrupt(handle: i64) -> i64 {
    let mut guard = CONN_HANDLES.lock();
    let Some(conn) = guard.as_mut().and_then(|map| map.get_mut(&handle)) else {
        return fail(INVALID_CONN);
    };
    conn.interrupt();
    0
}

/// Closes the connection and releases its handle, sweeping any
/// cursors still open on it (so an abandoned iteration is bounded by
/// the connection's lifetime). Returns 0 on success, -1 on error.
pub fn sql_conn_close(handle: i64) -> i64 {
    let conn = conn_take(handle);
    let Some(conn) = conn else {
        return fail(INVALID_CONN);
    };
    sweep_conn_cursors(handle);
    sweep_conn_children(handle);
    match crate::sched_global::run_blocking("sql-conn-close", move || {
        let mut conn = conn;
        let result = conn.close();
        drop(conn);
        result
    }) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Releases every Rows cursor (and its current Row) opened on
/// `conn`. Driver-side drops run outside the registry lock - a
/// blocking driver Drop must not stall unrelated SQL calls.
/// Releases every prepared statement and live transaction opened on
/// `conn`, plus its pending-notification slot. Driver-side drops run
/// outside the registry locks (a transaction `Drop` typically issues
/// a ROLLBACK).
fn sweep_conn_children(conn: i64) {
    let stmts: Vec<StmtEntry> = {
        let mut guard = STMT_HANDLES.lock();
        match guard.as_mut() {
            Some(map) => {
                let keys: Vec<i64> = map
                    .iter()
                    .filter(|(_, e)| e.conn == conn)
                    .map(|(k, _)| *k)
                    .collect();
                keys.iter().filter_map(|k| map.remove(k)).collect()
            }
            None => Vec::new(),
        }
    };
    drop(stmts);
    let txs: Vec<TxEntry> = {
        let mut guard = TX_HANDLES.lock();
        match guard.as_mut() {
            Some(map) => {
                let keys: Vec<i64> = map
                    .iter()
                    .filter(|(_, e)| e.conn == conn)
                    .map(|(k, _)| *k)
                    .collect();
                keys.iter().filter_map(|k| map.remove(k)).collect()
            }
            None => Vec::new(),
        }
    };
    drop(txs);
    let mut guard = LAST_NOTIFICATION.lock();
    if let Some(map) = guard.as_mut() {
        map.remove(&conn);
    }
    drop(guard);
    let mut guard = COPY_OUT_SLOTS.lock();
    if let Some(map) = guard.as_mut() {
        map.remove(&conn);
    }
}

fn sweep_conn_cursors(conn: i64) {
    let swept: Vec<RowsEntry> = {
        let mut guard = ROWS_HANDLES.lock();
        match guard.as_mut() {
            Some(map) => {
                let keys: Vec<i64> = map
                    .iter()
                    .filter(|(_, e)| e.conn == conn)
                    .map(|(k, _)| *k)
                    .collect();
                keys.iter().filter_map(|k| map.remove(k)).collect()
            }
            None => Vec::new(),
        }
    };
    for entry in &swept {
        row_unregister(entry.current_row);
    }
}

/// Advances the result set. Returns a Row handle, 0 on end-of-set
/// (the Rows handle is released), or -1 on error. Cursor semantics:
/// advancing frees the previous Row handle, so at most one Row per
/// cursor is live at a time and a fully drained iteration leaks
/// nothing.
pub fn sql_rows_next_row(handle: i64) -> i64 {
    let Some(mut entry) = rows_take(handle) else {
        return fail(INVALID_ROWS);
    };
    let columns: Vec<String> = entry.rows.columns().to_vec();
    match entry.rows.next_row() {
        Ok(Some(values)) => {
            row_unregister(entry.current_row);
            let row = row_register(Row { values, columns });
            entry.current_row = row;
            rows_reinsert(handle, entry);
            row
        }
        // End-of-set: release the final Row and drop the entry (not
        // reinserted) - the natural completion path reclaims both.
        Ok(None) => {
            row_unregister(entry.current_row);
            0
        }
        // The advance failed; the cursor (and its current Row) stay
        // live so the caller can inspect or close.
        Err(e) => {
            rows_reinsert(handle, entry);
            fail(e.to_string())
        }
    }
}

/// Releases a Rows cursor and its current Row. Idempotent: closing
/// an already-released (or exhausted) handle is a no-op returning 0,
/// so `defer rows.close()` composes with the drain loop.
pub fn sql_rows_close(handle: i64) -> i64 {
    if let Some(entry) = rows_take(handle) {
        row_unregister(entry.current_row);
    }
    0
}

/// Comma-joined column names for the Rows handle.
pub fn sql_rows_columns_joined(handle: i64) -> String {
    let Some(entry) = rows_take(handle) else {
        return String::new();
    };
    let joined = entry.rows.columns().join(",");
    rows_reinsert(handle, entry);
    joined
}

/// Coarse kind of the named column: -1 missing, 0 Null, 1 Bool,
/// 2 Int, 3 Float, 4 Text, 5 Blob; -1 missing column, -2 stale Row
/// handle (the cursor advanced past it or was closed).
pub fn sql_row_kind(handle: i64, column: &str) -> i64 {
    row_with(handle, |row| match row_value_by_column(row, column) {
        None => -1,
        Some(Value::Null) => 0,
        Some(Value::Bool(_)) => 1,
        Some(Value::Int(_)) => 2,
        Some(Value::Float(_)) => 3,
        Some(Value::Text(_)) => 4,
        Some(Value::Blob(_)) => 5,
    })
    .unwrap_or(-2)
}

/// Int column value, or 0 when absent / not Int.
pub fn sql_row_get_i64(handle: i64, column: &str) -> i64 {
    row_with(handle, |row| match row_value_by_column(row, column) {
        Some(Value::Int(n)) => n,
        _ => 0,
    })
    .unwrap_or(0)
}

/// Float column value (Int coerces), or 0.0 when absent.
pub fn sql_row_get_f64(handle: i64, column: &str) -> f64 {
    row_with(handle, |row| match row_value_by_column(row, column) {
        Some(Value::Float(f)) => f,
        Some(Value::Int(n)) => n as f64,
        _ => 0.0,
    })
    .unwrap_or(0.0)
}

/// Bool column value as 0/1 (Int coerces), or 0 when absent.
pub fn sql_row_get_bool(handle: i64, column: &str) -> i64 {
    row_with(handle, |row| match row_value_by_column(row, column) {
        Some(Value::Bool(b)) => i64::from(b),
        Some(Value::Int(n)) => i64::from(n != 0),
        _ => 0,
    })
    .unwrap_or(0)
}

/// Text column value, or "" when absent / not Text.
pub fn sql_row_get_text(handle: i64, column: &str) -> String {
    row_with(handle, |row| match row_value_by_column(row, column) {
        Some(Value::Text(s)) => s,
        _ => String::new(),
    })
    .unwrap_or_default()
}

/// Blob column bytes, or empty when absent / not Blob.
pub fn sql_row_get_blob(handle: i64, column: &str) -> Vec<u8> {
    row_with(handle, |row| match row_value_by_column(row, column) {
        Some(Value::Blob(b)) => b,
        _ => Vec::new(),
    })
    .unwrap_or_default()
}

/// Number of columns in the row.
pub fn sql_row_width(handle: i64) -> i64 {
    row_with(handle, |row| row.values.len() as i64).unwrap_or(0)
}

/// Commits and releases the transaction. Returns 0, or -1 on error.
pub fn sql_tx_commit(handle: i64) -> i64 {
    let Some(mut tx) = tx_take(handle) else {
        return fail(INVALID_TX);
    };
    match tx.tx.commit() {
        Ok(()) => 0,
        Err(e) => fail(e.to_string()),
    }
}

/// Rolls back and releases the transaction. Returns 0, or -1 on
/// error.
pub fn sql_tx_rollback(handle: i64) -> i64 {
    let Some(mut tx) = tx_take(handle) else {
        return fail(INVALID_TX);
    };
    match tx.tx.rollback() {
        Ok(()) => 0,
        Err(e) => fail(e.to_string()),
    }
}

/// Executes raw SQL inside the transaction. Returns rows affected,
/// or -1 on error.
pub fn sql_tx_execute(handle: i64, sql: &str) -> i64 {
    let Some(mut tx) = tx_take(handle) else {
        return fail(INVALID_TX);
    };
    let n = match tx.tx.execute(sql) {
        Ok(n) => n as i64,
        Err(e) => fail(e.to_string()),
    };
    tx_reinsert(handle, tx);
    n
}

fn tx_savepoint_op(
    handle: i64,
    name: &str,
    f: impl FnOnce(&mut dyn TransactionImpl, &str) -> Result<(), Error>,
) -> i64 {
    let Some(mut tx) = tx_take(handle) else {
        return fail(INVALID_TX);
    };
    let r = match f(tx.tx.as_mut(), name) {
        Ok(()) => 0,
        Err(e) => fail(e.to_string()),
    };
    tx_reinsert(handle, tx);
    r
}

/// Creates a savepoint. Returns 0, or -1 on error.
pub fn sql_tx_savepoint(handle: i64, name: &str) -> i64 {
    tx_savepoint_op(handle, name, |tx, n| tx.savepoint(n))
}

/// Releases (commits) a savepoint. Returns 0, or -1 on error.
pub fn sql_tx_release_savepoint(handle: i64, name: &str) -> i64 {
    tx_savepoint_op(handle, name, |tx, n| tx.release_savepoint(n))
}

/// Rolls back to a savepoint. Returns 0, or -1 on error.
pub fn sql_tx_rollback_to_savepoint(handle: i64, name: &str) -> i64 {
    tx_savepoint_op(handle, name, |tx, n| tx.rollback_to_savepoint(n))
}

/// Executes a statement with bound parameters (consumed) inside the
/// transaction. Returns rows affected, or -1 on error.
pub fn sql_tx_execute_params(handle: i64, sql: &str, params_handle: i64) -> i64 {
    let params = params_take(params_handle);
    let Some(mut entry) = tx_take(handle) else {
        return fail(INVALID_TX);
    };
    let n = match entry.tx.execute_params(sql, &params) {
        Ok(n) => n as i64,
        Err(e) => fail(e.to_string()),
    };
    tx_reinsert(handle, entry);
    n
}

/// Runs a query with bound parameters (consumed) inside the
/// transaction. Returns a Rows handle (registered under the
/// transaction's connection), or -1 on error.
pub fn sql_tx_query_params(handle: i64, sql: &str, params_handle: i64) -> i64 {
    let params = params_take(params_handle);
    let Some(mut entry) = tx_take(handle) else {
        return fail(INVALID_TX);
    };
    let conn = entry.conn;
    let r = match entry.tx.query_params(sql, &params) {
        Ok(rows) => rows_register(rows, conn),
        Err(e) => fail(e.to_string()),
    };
    tx_reinsert(handle, entry);
    r
}

/// Prepares a statement for repeated execution. Returns a Stmt
/// handle (registered under the connection), or -1 on error.
pub fn sql_conn_prepare(handle: i64, sql: &str) -> i64 {
    let sql = sql.to_string();
    match conn_run(handle, "sql-conn-prepare", move |c| c.prepare(&sql)) {
        Ok(Ok(stmt)) => stmt_register(stmt, handle),
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Executes a prepared statement with bound parameters (consumed).
/// Returns rows affected, or -1 on error.
pub fn sql_stmt_execute(handle: i64, params_handle: i64) -> i64 {
    let params = params_take(params_handle);
    let Some(mut entry) = stmt_take(handle) else {
        return fail("sql: invalid statement handle");
    };
    let n = match entry.stmt.execute(&params) {
        Ok(n) => n as i64,
        Err(e) => fail(e.to_string()),
    };
    stmt_reinsert(handle, entry);
    n
}

/// Runs a prepared statement with bound parameters (consumed).
/// Returns a Rows handle, or -1 on error.
pub fn sql_stmt_query(handle: i64, params_handle: i64) -> i64 {
    let params = params_take(params_handle);
    let Some(mut entry) = stmt_take(handle) else {
        return fail("sql: invalid statement handle");
    };
    let conn = entry.conn;
    let r = match entry.stmt.query(&params) {
        Ok(rows) => rows_register(rows, conn),
        Err(e) => fail(e.to_string()),
    };
    stmt_reinsert(handle, entry);
    r
}

/// Releases a prepared statement. Idempotent.
pub fn sql_stmt_close(handle: i64) -> i64 {
    let _ = stmt_take(handle);
    0
}

/// Bulk-loads `data` through the dialect's copy mechanism. Returns
/// rows written, or -1 on error.
pub fn sql_conn_copy_in(handle: i64, sql: &str, data: &[u8]) -> i64 {
    let sql = sql.to_string();
    let data = data.to_vec();
    match conn_run(handle, "sql-conn-copy-in", move |c| c.copy_in(&sql, &data)) {
        Ok(Ok(n)) => n as i64,
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Bulk-extracts rows through the dialect's copy mechanism. `None`
/// means error (message via `sql_take_last_error`).
pub fn sql_conn_copy_out(handle: i64, sql: &str) -> Option<Vec<u8>> {
    let sql = sql.to_string();
    match conn_run(handle, "sql-conn-copy-out", move |c| c.copy_out(&sql)) {
        Ok(Ok(bytes)) => Some(bytes),
        Ok(Err(e)) => {
            sql_set_last_error(e.to_string());
            None
        }
        Err(e) => {
            sql_set_last_error(e);
            None
        }
    }
}

/// Subscribes the connection to `channel`. Returns 0, or -1 on error.
pub fn sql_conn_listen(handle: i64, channel: &str) -> i64 {
    let channel = channel.to_string();
    match conn_run(handle, "sql-conn-listen", move |c| c.listen(&channel)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Unsubscribes the connection from `channel`. Returns 0, or -1 on
/// error.
pub fn sql_conn_unlisten(handle: i64, channel: &str) -> i64 {
    let channel = channel.to_string();
    match conn_run(handle, "sql-conn-unlisten", move |c| c.unlisten(&channel)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Waits up to `timeout_ms` for a notification. Returns 1 when one
/// arrived (readable via the `sql_notification_*` getters), 0 on
/// timeout, -1 on error.
pub fn sql_conn_poll_notification(handle: i64, timeout_ms: i64) -> i64 {
    let polled = conn_run(handle, "sql-conn-poll-notification", move |c| {
        c.poll_notification(timeout_ms)
    });
    match polled {
        Ok(Ok(Some(n))) => {
            let mut guard = LAST_NOTIFICATION.lock();
            guard.get_or_insert_with(HashMap::new).insert(handle, n);
            1
        }
        Ok(Ok(None)) => 0,
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Stores copy-out bytes for the connection (interpreter-side
/// counterpart of `gos_rt_sql_conn_copy_out_run`).
pub fn sql_copy_out_store(conn: i64, bytes: Vec<u8>) {
    let mut guard = COPY_OUT_SLOTS.lock();
    guard.get_or_insert_with(HashMap::new).insert(conn, bytes);
}

/// Takes the connection's stored copy-out bytes (empty if none).
pub fn sql_copy_out_take(conn: i64) -> Vec<u8> {
    let mut guard = COPY_OUT_SLOTS.lock();
    guard
        .as_mut()
        .and_then(|m| m.remove(&conn))
        .unwrap_or_default()
}

fn last_notification<R>(conn: i64, f: impl FnOnce(&Notification) -> R) -> Option<R> {
    let guard = LAST_NOTIFICATION.lock();
    guard.as_ref()?.get(&conn).map(f)
}

/// Channel of the connection's most recently polled notification.
pub fn sql_notification_channel(conn: i64) -> String {
    last_notification(conn, |n| n.channel.clone()).unwrap_or_default()
}

/// Payload of the connection's most recently polled notification.
pub fn sql_notification_payload(conn: i64) -> String {
    last_notification(conn, |n| n.payload.clone()).unwrap_or_default()
}

/// Backend pid of the connection's most recently polled notification.
pub fn sql_notification_pid(conn: i64) -> i64 {
    last_notification(conn, |n| n.process_id).unwrap_or(0)
}

/// Builds a connection pool. Timeout/lifetime arguments are in
/// milliseconds; 0 disables idle/lifetime eviction. Returns a Pool
/// handle, or -1 on error.
pub fn sql_pool_new(
    driver: &str,
    url: &str,
    min: i64,
    max: i64,
    acquire_timeout_ms: i64,
    idle_timeout_ms: i64,
    max_lifetime_ms: i64,
) -> i64 {
    let mut config = crate::sql_pool::PoolConfig {
        min: min.max(0) as usize,
        max: max.max(0) as usize,
        idle_timeout: (idle_timeout_ms > 0)
            .then(|| std::time::Duration::from_millis(idle_timeout_ms as u64)),
        max_lifetime: (max_lifetime_ms > 0)
            .then(|| std::time::Duration::from_millis(max_lifetime_ms as u64)),
        ..Default::default()
    };
    if acquire_timeout_ms > 0 {
        config.acquire_timeout = std::time::Duration::from_millis(acquire_timeout_ms as u64);
    }
    let driver = driver.to_string();
    let url = url.to_string();
    match crate::sched_global::run_blocking("sql-pool-new", move || {
        crate::sql_pool::Pool::new(&driver, &url, config)
    }) {
        Ok(Ok(pool)) => {
            let id = next_handle();
            let mut guard = POOL_HANDLES.lock();
            guard.get_or_insert_with(HashMap::new).insert(id, pool);
            id
        }
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

fn pool_with<R>(handle: i64, f: impl FnOnce(&crate::sql_pool::Pool) -> R) -> Option<R> {
    let guard = POOL_HANDLES.lock();
    guard.as_ref()?.get(&handle).map(f)
}

fn pool_clone(handle: i64) -> Option<crate::sql_pool::Pool> {
    pool_with(handle, Clone::clone)
}

/// Checks a connection out of the pool. The result is an ordinary
/// Conn handle; closing it returns the connection to the pool.
pub fn sql_pool_get(handle: i64) -> i64 {
    let Some(pool) = pool_clone(handle) else {
        return fail("sql: invalid pool handle");
    };
    match crate::sched_global::run_blocking("sql-pool-get", move || pool.get()) {
        Ok(Ok(conn)) => conn_register(Box::new(conn)),
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

/// Live connections (idle + in-flight), or -1 on a bad handle.
pub fn sql_pool_live(handle: i64) -> i64 {
    pool_with(handle, |p| p.live() as i64).unwrap_or_else(|| fail("sql: invalid pool handle"))
}

/// Idle connections, or -1 on a bad handle.
pub fn sql_pool_idle(handle: i64) -> i64 {
    pool_with(handle, |p| p.idle() as i64).unwrap_or_else(|| fail("sql: invalid pool handle"))
}

/// Closes all idle pooled connections. Returns 0, or -1 on a bad
/// handle.
pub fn sql_pool_close_idle(handle: i64) -> i64 {
    pool_with(handle, |p| {
        p.close_idle();
        0
    })
    .unwrap_or_else(|| fail("sql: invalid pool handle"))
}

/// Applies pending migrations from `dir` on the connection. Returns
/// the number applied, or -1 on error.
pub fn sql_migrate_up(conn: i64, dir: &str) -> i64 {
    let dir = dir.to_string();
    let result = conn_run(conn, "sql-migrate-up", move |c| {
        crate::sql_migrate::up(c, dir)
    });
    match result {
        Ok(Ok(applied)) => applied.len() as i64,
        Ok(Err(e)) => fail(e.to_string()),
        Err(e) => fail(e),
    }
}

// Each SQL shim is `pub extern "C"` with `#[unsafe(no_mangle)]`, so the
// linker keeps every symbol on its own. No `#[used]` anchor needed.

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_value_null() -> i64 {
    value_register(Value::Null)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_value_bool(b: i32) -> i64 {
    value_register(Value::Bool(b != 0))
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_value_int(n: i64) -> i64 {
    value_register(Value::Int(n))
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_value_float(f: f64) -> i64 {
    value_register(Value::Float(f))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_value_text(s: *const c_char) -> i64 {
    // SAFETY: `s` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    value_register(Value::Text(unsafe { c_str_to_string(s) }))
}

// --- conn / open ---------------------------------------------------

/// Opens a connection. Returns -1 on unknown driver, -2 on driver
/// error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_open(name: *const c_char, url: *const c_char) -> i64 {
    // SAFETY: `name` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    // `url` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_open_handle(&unsafe { c_str_to_string(name) }, &unsafe {
        c_str_to_string(url)
    })
}

/// Returns the most recent SQL error message as a c-string (and
/// clears it). Caller frees via `gos_rt_free_cstring`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_last_error() -> *mut c_char {
    alloc_cstring(sql_take_last_error().as_bytes())
}

/// Returns a c-string of `,`-separated driver names. Caller frees
/// via `gos_rt_free_cstring`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_drivers() -> *mut c_char {
    alloc_cstring(sql_drivers_joined().as_bytes())
}

/// Allocates a parameter list; returns its handle.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_params_new() -> i64 {
    sql_params_new()
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_params_push_null(p: i64) -> i64 {
    sql_params_push(p, Value::Null)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_params_push_bool(p: i64, b: i64) -> i64 {
    sql_params_push(p, Value::Bool(b != 0))
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_params_push_int(p: i64, n: i64) -> i64 {
    sql_params_push(p, Value::Int(n))
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_params_push_float(p: i64, f: f64) -> i64 {
    sql_params_push(p, Value::Float(f))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_params_push_text(p: i64, s: *const c_char) -> i64 {
    // SAFETY: `s` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_params_push(p, Value::Text(unsafe { c_str_to_string(s) }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_params_push_blob(
    p: i64,
    bytes: *const super::vec::GosVec,
) -> i64 {
    // SAFETY: codegen passes a live GosVec pointer for a `[u8]` arg.
    let data = unsafe { crate::c_abi::vec::vec_bytes(bytes) };
    sql_params_push(p, Value::Blob(data))
}

/// Executes `sql` against the connection identified by `handle`.
/// Returns rows affected, or -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_conn_execute(handle: i64, sql: *const c_char) -> i64 {
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_conn_execute_params(handle, &unsafe { c_str_to_string(sql) }, 0)
}

/// Executes `sql` with bound parameters (the params handle is
/// consumed). Returns rows affected, or -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_conn_execute_params(
    handle: i64,
    sql: *const c_char,
    params: i64,
) -> i64 {
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_conn_execute_params(handle, &unsafe { c_str_to_string(sql) }, params)
}

/// Runs a query. Returns a Rows handle, or -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_conn_query(handle: i64, sql: *const c_char) -> i64 {
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_conn_query_params(handle, &unsafe { c_str_to_string(sql) }, 0)
}

/// Runs a query with bound parameters (the params handle is
/// consumed). Returns a Rows handle, or -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_conn_query_params(
    handle: i64,
    sql: *const c_char,
    params: i64,
) -> i64 {
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_conn_query_params(handle, &unsafe { c_str_to_string(sql) }, params)
}

/// Begins a transaction. Returns a Tx handle, or -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_conn_begin(handle: i64) -> i64 {
    sql_conn_begin(handle)
}

/// Begins a transaction at the requested isolation level (0=Default,
/// 1=ReadUncommitted, 2=ReadCommitted, 3=RepeatableRead,
/// 4=Serializable). Returns a Tx handle, or -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_conn_begin_with(handle: i64, iso: i64) -> i64 {
    sql_conn_begin_with(handle, iso)
}

/// Pings the connection. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_conn_ping(handle: i64) -> i64 {
    sql_conn_ping(handle)
}

/// Sets the driver-specific busy timeout in milliseconds. Returns 0
/// on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_conn_set_busy_timeout(handle: i64, ms: i64) -> i64 {
    sql_conn_set_busy_timeout(handle, ms)
}

/// Cancels any in-flight statement on the connection.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_conn_interrupt(handle: i64) -> i64 {
    sql_conn_interrupt(handle)
}

/// Closes the connection and releases its handle. Returns 0 on
/// success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_conn_close(handle: i64) -> i64 {
    sql_conn_close(handle)
}

// --- rows iteration ------------------------------------------------

/// Advances `rows` and returns a Row handle, 0 on end-of-set, -1 on
/// error.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_rows_next_row(handle: i64) -> i64 {
    sql_rows_next_row(handle)
}

/// Releases a Rows cursor and its current Row. Idempotent; always 0.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_rows_close(handle: i64) -> i64 {
    sql_rows_close(handle)
}

/// Returns a c-string of `,`-separated column names for `rows`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_rows_columns(handle: i64) -> *mut c_char {
    let joined = sql_rows_columns_joined(handle);
    if joined.is_empty() {
        return empty_cstring();
    }
    alloc_cstring(joined.as_bytes())
}

// --- row column readers --------------------------------------------

fn row_value_by_column(row: &Row, column: &str) -> Option<Value> {
    row.columns
        .iter()
        .position(|c| c == column)
        .and_then(|i| row.values.get(i).cloned())
}

/// Returns the coarse kind of the named column: -1 missing, 0 Null,
/// 1 Bool, 2 Int, 3 Float, 4 Text, 5 Blob.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_kind(handle: i64, column: *const c_char) -> i64 {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_row_kind(handle, &unsafe { c_str_to_string(column) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_i64(handle: i64, column: *const c_char) -> i64 {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_row_get_i64(handle, &unsafe { c_str_to_string(column) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_f64(handle: i64, column: *const c_char) -> f64 {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_row_get_f64(handle, &unsafe { c_str_to_string(column) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_bool(handle: i64, column: *const c_char) -> i32 {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_row_get_bool(handle, &unsafe { c_str_to_string(column) }) as i32
}

/// Like `gos_rt_sql_row_get_bool` with a uniform i64 return for the
/// injected-wrapper call path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_bool_i64(handle: i64, column: *const c_char) -> i64 {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_row_get_bool(handle, &unsafe { c_str_to_string(column) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_text(
    handle: i64,
    column: *const c_char,
) -> *mut c_char {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    let text = sql_row_get_text(handle, &unsafe { c_str_to_string(column) });
    alloc_cstring(text.as_bytes())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_blob(
    handle: i64,
    column: *const c_char,
) -> *mut c_char {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    let bytes = sql_row_get_blob(handle, &unsafe { c_str_to_string(column) });
    alloc_cstring(&bytes)
}

/// Blob column as a canonical packed `[u8]` GosVec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_blob_vec(
    handle: i64,
    column: *const c_char,
) -> *mut super::vec::GosVec {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    let bytes = sql_row_get_blob(handle, &unsafe { c_str_to_string(column) });
    super::encoding::bytes_to_gosvec(&bytes)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_opt_i64(
    handle: i64,
    column: *const c_char,
    out_present: *mut i32,
) -> i64 {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    let col = unsafe { c_str_to_string(column) };
    let result = row_with(handle, |row| match row_value_by_column(row, &col) {
        Some(Value::Null) | None => (0, 0_i32),
        Some(Value::Int(n)) => (n, 1),
        _ => (0, 0),
    })
    .unwrap_or((0, 0));
    if !out_present.is_null() {
        // SAFETY: `out_present` is non-null (checked above) and addresses the caller's flag word
        // (C-ABI contract).
        unsafe {
            *out_present = result.1;
        }
    }
    result.0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_opt_f64(
    handle: i64,
    column: *const c_char,
    out_present: *mut i32,
) -> f64 {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    let col = unsafe { c_str_to_string(column) };
    let result = row_with(handle, |row| match row_value_by_column(row, &col) {
        Some(Value::Null) | None => (0.0, 0_i32),
        Some(Value::Float(f)) => (f, 1),
        Some(Value::Int(n)) => (n as f64, 1),
        _ => (0.0, 0),
    })
    .unwrap_or((0.0, 0));
    if !out_present.is_null() {
        // SAFETY: `out_present` is non-null (checked above) and addresses the caller's flag word
        // (C-ABI contract).
        unsafe {
            *out_present = result.1;
        }
    }
    result.0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_opt_bool(
    handle: i64,
    column: *const c_char,
    out_present: *mut i32,
) -> i32 {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    let col = unsafe { c_str_to_string(column) };
    let result = row_with(handle, |row| match row_value_by_column(row, &col) {
        Some(Value::Null) | None => (0_i32, 0_i32),
        Some(Value::Bool(b)) => (i32::from(b), 1),
        _ => (0, 0),
    })
    .unwrap_or((0, 0));
    if !out_present.is_null() {
        // SAFETY: `out_present` is non-null (checked above) and addresses the caller's flag word
        // (C-ABI contract).
        unsafe {
            *out_present = result.1;
        }
    }
    result.0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_get_opt_text(
    handle: i64,
    column: *const c_char,
    out_present: *mut i32,
) -> *mut c_char {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    let col = unsafe { c_str_to_string(column) };
    let (text, present) = row_with(handle, |row| match row_value_by_column(row, &col) {
        Some(Value::Null) | None => (String::new(), 0_i32),
        Some(Value::Text(s)) => (s, 1),
        _ => (String::new(), 0),
    })
    .unwrap_or_default();
    if !out_present.is_null() {
        // SAFETY: `out_present` is non-null (checked above) and addresses the caller's flag word
        // (C-ABI contract).
        unsafe {
            *out_present = present;
        }
    }
    alloc_cstring(text.as_bytes())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_row_is_null(handle: i64, column: *const c_char) -> i32 {
    // SAFETY: `column` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    let col = unsafe { c_str_to_string(column) };
    row_with(handle, |row| {
        matches!(row_value_by_column(row, &col), Some(Value::Null) | None)
    })
    .map_or(0, i32::from)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_row_width(handle: i64) -> i64 {
    sql_row_width(handle)
}

// --- transaction ---------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_tx_commit(handle: i64) -> i64 {
    sql_tx_commit(handle)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_tx_rollback(handle: i64) -> i64 {
    sql_tx_rollback(handle)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_tx_execute(handle: i64, sql: *const c_char) -> i64 {
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_tx_execute(handle, &unsafe { c_str_to_string(sql) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_tx_savepoint(handle: i64, name: *const c_char) -> i64 {
    // SAFETY: `name` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_tx_savepoint(handle, &unsafe { c_str_to_string(name) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_tx_release_savepoint(handle: i64, name: *const c_char) -> i64 {
    // SAFETY: `name` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_tx_release_savepoint(handle, &unsafe { c_str_to_string(name) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_tx_rollback_to_savepoint(
    handle: i64,
    name: *const c_char,
) -> i64 {
    // SAFETY: `name` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_tx_rollback_to_savepoint(handle, &unsafe { c_str_to_string(name) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_conn_prepare(handle: i64, sql: *const c_char) -> i64 {
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_conn_prepare(handle, &unsafe { c_str_to_string(sql) })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_stmt_execute(handle: i64, params: i64) -> i64 {
    sql_stmt_execute(handle, params)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_stmt_query(handle: i64, params: i64) -> i64 {
    sql_stmt_query(handle, params)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_stmt_close(handle: i64) -> i64 {
    sql_stmt_close(handle)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_tx_execute_params(
    handle: i64,
    sql: *const c_char,
    params: i64,
) -> i64 {
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_tx_execute_params(handle, &unsafe { c_str_to_string(sql) }, params)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_tx_query_params(
    handle: i64,
    sql: *const c_char,
    params: i64,
) -> i64 {
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_tx_query_params(handle, &unsafe { c_str_to_string(sql) }, params)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_conn_copy_in(
    handle: i64,
    sql: *const c_char,
    data: *const super::vec::GosVec,
) -> i64 {
    // SAFETY: codegen passes a live GosVec pointer for a `[u8]` arg.
    let bytes = unsafe { crate::c_abi::vec::vec_bytes(data) };
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_conn_copy_in(handle, &unsafe { c_str_to_string(sql) }, &bytes)
}

/// Runs COPY TO and stores the bytes in the connection's copy-out
/// slot for `gos_rt_sql_conn_copy_out_take`. Returns the byte count,
/// or -1 on error. Two-step so the injected wrappers can branch on a
/// scalar status before materializing the bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_conn_copy_out_run(handle: i64, sql: *const c_char) -> i64 {
    // SAFETY: `sql` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    match sql_conn_copy_out(handle, &unsafe { c_str_to_string(sql) }) {
        Some(bytes) => {
            let n = bytes.len() as i64;
            let mut guard = COPY_OUT_SLOTS.lock();
            guard.get_or_insert_with(HashMap::new).insert(handle, bytes);
            n
        }
        None => -1,
    }
}

/// Takes the bytes stored by the most recent
/// `gos_rt_sql_conn_copy_out_run` on this connection as a `[u8]`
/// GosVec (empty if none).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_conn_copy_out_take(handle: i64) -> *mut super::vec::GosVec {
    let bytes = {
        let mut guard = COPY_OUT_SLOTS.lock();
        guard
            .as_mut()
            .and_then(|m| m.remove(&handle))
            .unwrap_or_default()
    };
    super::encoding::bytes_to_gosvec(&bytes)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_conn_listen(handle: i64, channel: *const c_char) -> i64 {
    // SAFETY: `channel` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_conn_listen(handle, &unsafe { c_str_to_string(channel) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_conn_unlisten(handle: i64, channel: *const c_char) -> i64 {
    // SAFETY: `channel` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_conn_unlisten(handle, &unsafe { c_str_to_string(channel) })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_conn_poll_notification(handle: i64, timeout_ms: i64) -> i64 {
    sql_conn_poll_notification(handle, timeout_ms)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_notification_channel(conn: i64) -> *mut c_char {
    alloc_cstring(sql_notification_channel(conn).as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_notification_payload(conn: i64) -> *mut c_char {
    alloc_cstring(sql_notification_payload(conn).as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_notification_pid(conn: i64) -> i64 {
    sql_notification_pid(conn)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_pool_new(
    driver: *const c_char,
    url: *const c_char,
    min: i64,
    max: i64,
    acquire_timeout_ms: i64,
    idle_timeout_ms: i64,
    max_lifetime_ms: i64,
) -> i64 {
    sql_pool_new(
        // SAFETY: `driver` is this shim's argument, as `c_str_to_string` requires (C-ABI
        // contract).
        &unsafe { c_str_to_string(driver) },
        // SAFETY: `url` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
        &unsafe { c_str_to_string(url) },
        min,
        max,
        acquire_timeout_ms,
        idle_timeout_ms,
        max_lifetime_ms,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_pool_get(handle: i64) -> i64 {
    sql_pool_get(handle)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_pool_live(handle: i64) -> i64 {
    sql_pool_live(handle)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_pool_idle(handle: i64) -> i64 {
    sql_pool_idle(handle)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_pool_close_idle(handle: i64) -> i64 {
    sql_pool_close_idle(handle)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_migrate_up(conn: i64, dir: *const c_char) -> i64 {
    // SAFETY: `dir` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    sql_migrate_up(conn, &unsafe { c_str_to_string(dir) })
}

// --- native (Gossamer-implemented) driver dispatch ------------------
//
// A `.gos` driver registers a stateless struct exposing one method,
// `fn dispatch(&self, op: i64, h: i64) -> i64`. Rust never marshals a
// complex value across the boundary: only the op code and the token
// `h` cross. Inputs and outputs flow through `SQL_NATIVE_SLOTS`, a
// per-token side-channel the driver reads/writes through the
// `native_*` helpers. The compiled tier dispatches through a Rust
// `GossamerDriver` registered in `crate::sql`; the interpreter
// dispatches through `NativeDispatch::call_fn` in its own sql
// builtins. Both share this one slot table and helper set.

/// Op codes shared with the `.gos` driver (mirror as a `const` block
/// in Gossamer source). A negative dispatch return is an error whose
/// message the driver set via `native_set_error`.
pub mod op {
    pub const OPEN: i64 = 0;
    pub const CLOSE: i64 = 1;
    pub const PREPARE: i64 = 2;
    pub const STMT_EXECUTE: i64 = 3;
    pub const STMT_QUERY: i64 = 4;
    pub const STMT_CLOSE: i64 = 5;
    pub const ROWS_NEXT: i64 = 6;
    pub const ROWS_CLOSE: i64 = 7;
    pub const BEGIN_WITH: i64 = 8;
    pub const COMMIT: i64 = 9;
    pub const ROLLBACK: i64 = 10;
    pub const TX_EXECUTE: i64 = 11;
    pub const TX_EXECUTE_PARAMS: i64 = 12;
    pub const TX_QUERY_PARAMS: i64 = 13;
    pub const PING: i64 = 14;
    pub const SET_BUSY_TIMEOUT: i64 = 15;
    pub const INTERRUPT: i64 = 16;
    pub const COPY_IN: i64 = 17;
    pub const COPY_OUT: i64 = 18;
    pub const LISTEN: i64 = 19;
    pub const UNLISTEN: i64 = 20;
    pub const POLL_NOTIFICATION: i64 = 21;
}

/// Per-token side-channel between the SQL façade (Rust) and the
/// `.gos` driver. Inputs are populated before a dispatch; outputs are
/// drained after it. The façade serializes per connection, so a given
/// token never sees concurrent dispatch; distinct tokens use distinct
/// slots and run concurrently.
#[derive(Default)]
struct Slot {
    // inputs (driver reads)
    url: String,
    sql: String,
    parent: i64,
    out_handle: i64,
    iso: i64,
    timeout: i64,
    channel: String,
    params: Vec<Value>,
    data: Vec<u8>,
    // outputs (driver writes)
    error: String,
    row: Vec<Value>,
    row_present: bool,
    columns: Vec<String>,
    out_bytes: Vec<u8>,
    notif_chan: String,
    notif_payload: String,
    notif_pid: i64,
    notif_present: bool,
}

static SQL_NATIVE_SLOTS: Mutex<Option<HashMap<i64, Slot>>> = Mutex::new(None);

fn slot_with<R>(token: i64, f: impl FnOnce(&mut Slot) -> R) -> R {
    let mut guard = SQL_NATIVE_SLOTS.lock();
    let map = guard.get_or_insert_with(HashMap::new);
    f(map.entry(token).or_default())
}

fn slot_remove(token: i64) {
    let mut guard = SQL_NATIVE_SLOTS.lock();
    if let Some(map) = guard.as_mut() {
        map.remove(&token);
    }
}

/// Allocates a fresh native-driver slot token. Exposed so the
/// interpreter and the compiled adapter share one id namespace with
/// the rest of the SQL handle registries.
pub fn native_alloc_token() -> i64 {
    next_handle()
}

// --- slot accessors used by both tiers' adapters -------------------

/// Records the per-op inputs for the next dispatch on `token`,
/// overwriting any previous output state.
#[derive(Default)]
struct SlotInputs<'a> {
    url: &'a str,
    sql: &'a str,
    parent: i64,
    out_handle: i64,
    iso: i64,
    timeout: i64,
    channel: &'a str,
    params: Vec<Value>,
    data: Vec<u8>,
}

fn slot_set_inputs(token: i64, inputs: SlotInputs<'_>) {
    slot_with(token, |s| {
        s.url = inputs.url.to_string();
        s.sql = inputs.sql.to_string();
        s.parent = inputs.parent;
        s.out_handle = inputs.out_handle;
        s.iso = inputs.iso;
        s.timeout = inputs.timeout;
        s.channel = inputs.channel.to_string();
        s.params = inputs.params;
        s.data = inputs.data;
        s.error.clear();
        s.row.clear();
        s.row_present = false;
        s.columns.clear();
        s.out_bytes.clear();
        s.notif_chan.clear();
        s.notif_payload.clear();
        s.notif_pid = 0;
        s.notif_present = false;
    });
}

/// Drains a slot's error message after a failing (`< 0`) dispatch.
fn slot_take_error(token: i64) -> String {
    slot_with(token, |s| std::mem::take(&mut s.error))
}

/// Drains a slot's emitted columns (after a query op set them under
/// the rows token).
fn slot_take_columns(token: i64) -> Vec<String> {
    slot_with(token, |s| std::mem::take(&mut s.columns))
}

/// Drains a slot's row if the driver marked one present.
fn slot_take_row(token: i64) -> Option<Vec<Value>> {
    slot_with(token, |s| {
        if s.row_present {
            s.row_present = false;
            Some(std::mem::take(&mut s.row))
        } else {
            None
        }
    })
}

/// Drains a slot's emitted bytes (copy-out).
fn slot_take_bytes(token: i64) -> Vec<u8> {
    slot_with(token, |s| std::mem::take(&mut s.out_bytes))
}

/// Drains a slot's notification if the driver set one present.
fn slot_take_notification(token: i64) -> Option<Notification> {
    slot_with(token, |s| {
        if s.notif_present {
            s.notif_present = false;
            Some(Notification {
                channel: std::mem::take(&mut s.notif_chan),
                payload: std::mem::take(&mut s.notif_payload),
                process_id: s.notif_pid,
            })
        } else {
            None
        }
    })
}

// --- native_* helpers the .gos driver calls (shared safe core) ------
//
// These read inputs from / write outputs to the slot keyed by `h`.
// Both tiers' C-ABI shims and interp builtins delegate here.

pub fn native_url(h: i64) -> String {
    slot_with(h, |s| s.url.clone())
}

pub fn native_sql(h: i64) -> String {
    slot_with(h, |s| s.sql.clone())
}

pub fn native_parent(h: i64) -> i64 {
    slot_with(h, |s| s.parent)
}

pub fn native_out_handle(h: i64) -> i64 {
    slot_with(h, |s| s.out_handle)
}

pub fn native_iso(h: i64) -> i64 {
    slot_with(h, |s| s.iso)
}

pub fn native_timeout(h: i64) -> i64 {
    slot_with(h, |s| s.timeout)
}

pub fn native_channel(h: i64) -> String {
    slot_with(h, |s| s.channel.clone())
}

pub fn native_param_count(h: i64) -> i64 {
    slot_with(h, |s| s.params.len() as i64)
}

/// Returns a fresh `sql::Value` handle for the i-th bound parameter
/// (cloned), readable through the `sql::value_*_of` accessors.
pub fn native_param(h: i64, i: i64) -> i64 {
    let v = slot_with(h, |s| {
        usize::try_from(i)
            .ok()
            .and_then(|idx| s.params.get(idx).cloned())
            .unwrap_or(Value::Null)
    });
    value_register(v)
}

pub fn native_data(h: i64) -> Vec<u8> {
    slot_with(h, |s| s.data.clone())
}

pub fn native_push_column(h: i64, name: &str) {
    slot_with(h, |s| s.columns.push(name.to_string()));
}

/// Pushes the value behind `value_handle` (consumed) onto the row the
/// driver is building under `h`.
pub fn native_push_value(h: i64, value_handle: i64) {
    let v = value_take(value_handle);
    slot_with(h, |s| s.row.push(v));
}

pub fn native_row_ready(h: i64) {
    slot_with(h, |s| s.row_present = true);
}

pub fn native_set_error(h: i64, msg: &str) {
    slot_with(h, |s| s.error = msg.to_string());
}

pub fn native_emit_bytes(h: i64, data: &[u8]) {
    slot_with(h, |s| s.out_bytes = data.to_vec());
}

pub fn native_set_notification(h: i64, chan: &str, payload: &str, pid: i64) {
    slot_with(h, |s| {
        s.notif_chan = chan.to_string();
        s.notif_payload = payload.to_string();
        s.notif_pid = pid;
        s.notif_present = true;
    });
}

// --- sql::Value handle constructors + accessors for the .gos driver -
//
// The driver turns a `__gos_sql_Value` enum into a handle with the
// `value_*` constructors and reads a param handle back with the
// `value_*_of` accessors. Both reuse the existing VALUE_HANDLES
// registry.

pub fn native_value_null() -> i64 {
    value_register(Value::Null)
}

pub fn native_value_bool(b: bool) -> i64 {
    value_register(Value::Bool(b))
}

pub fn native_value_int(n: i64) -> i64 {
    value_register(Value::Int(n))
}

pub fn native_value_float(f: f64) -> i64 {
    value_register(Value::Float(f))
}

pub fn native_value_text(s: &str) -> i64 {
    value_register(Value::Text(s.to_string()))
}

pub fn native_value_blob(data: &[u8]) -> i64 {
    value_register(Value::Blob(data.to_vec()))
}

/// Coarse kind of the value behind `handle`: 0 Null, 1 Bool, 2 Int,
/// 3 Float, 4 Text, 5 Blob. The handle is left in place.
pub fn native_value_kind(handle: i64) -> i64 {
    match value_peek(handle) {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) => 2,
        Value::Float(_) => 3,
        Value::Text(_) => 4,
        Value::Blob(_) => 5,
    }
}

pub fn native_value_int_of(handle: i64) -> i64 {
    match value_peek(handle) {
        Value::Int(n) => n,
        Value::Bool(b) => i64::from(b),
        _ => 0,
    }
}

pub fn native_value_float_of(handle: i64) -> f64 {
    match value_peek(handle) {
        Value::Float(f) => f,
        Value::Int(n) => n as f64,
        _ => 0.0,
    }
}

pub fn native_value_text_of(handle: i64) -> String {
    match value_peek(handle) {
        Value::Text(s) => s,
        _ => String::new(),
    }
}

pub fn native_value_blob_of(handle: i64) -> Vec<u8> {
    match value_peek(handle) {
        Value::Blob(b) => b,
        _ => Vec::new(),
    }
}

// --- connection-handle stash (the one retained Gossamer value) ------
//
// The goroutine-per-connection design needs one Gossamer value per
// token: the connection's command `Sender`, represented as its i64
// handle. The channel itself is kept alive by the connection
// goroutine, which owns the matching `Receiver` as a local for its
// whole lifetime; this stash holds the `Sender` handle so the
// stateless `dispatch` can route a command to the owning connection.
// The stash is cleared on the CLOSE op, after which the goroutine
// drains and exits and the channel is reclaimed.

static SQL_NATIVE_CONN_HANDLE: Mutex<Option<HashMap<i64, i64>>> = Mutex::new(None);

/// Stashes one Gossamer value (its i64 representation) under token `h`.
pub fn native_set_handle(h: i64, value: i64) {
    let mut guard = SQL_NATIVE_CONN_HANDLE.lock();
    guard.get_or_insert_with(HashMap::new).insert(h, value);
}

/// Returns the value stashed under `h` (0 if none). The driver
/// annotates the i64 back to its `Sender<Command>` at the call site.
pub fn native_handle(h: i64) -> i64 {
    let guard = SQL_NATIVE_CONN_HANDLE.lock();
    guard.as_ref().and_then(|m| m.get(&h).copied()).unwrap_or(0)
}

/// Forgets the stashed handle under `h`. Called from the CLOSE adapter
/// and slot teardown; the goroutine that observes the closed command
/// channel then exits.
fn native_drop_handle(h: i64) {
    let mut guard = SQL_NATIVE_CONN_HANDLE.lock();
    if let Some(map) = guard.as_mut() {
        map.remove(&h);
    }
}

// --- compiled-tier adapter -----------------------------------------

/// A transmuted `gos_fn_addr("<Type>::dispatch")` plus the driver's
/// env pointer. The driver is a ZST whose `dispatch` never reads
/// `self`, so the env pointer dangling after `register` returns is
/// harmless; `call` only crosses the op code and token.
#[derive(Clone, Copy)]
struct Dispatcher {
    env: usize,
    fn_addr: usize,
}

// SAFETY: `fn_addr` is a code address, and `env` is either a stateless
// driver's never-read address or a counted copy of the driver that the
// registry owns for the process, marked shared so its children count
// atomically; `dispatch` only reads it.
unsafe impl Send for Dispatcher {}
// SAFETY: as for `Send`: every thread only reads the driver, and its counts
// are atomic.
unsafe impl Sync for Dispatcher {}

type DispatchFn = unsafe extern "C" fn(env: *mut u8, op: i64, h: i64) -> i64;

impl Dispatcher {
    fn call(&self, op: i64, h: i64) -> i64 {
        // SAFETY: `fn_addr` came from `gos_fn_addr("<Type>::dispatch")`
        // at the user's `sql::register_native` call site; the signature
        // matches the compiled `dispatch(&self, op, h) -> i64` ABI.
        let f: DispatchFn = unsafe {
            std::mem::transmute::<*const (), DispatchFn>(crate::c_abi::code_address(self.fn_addr))
        };
        // SAFETY: `f` is the driver's compiled `dispatch`, and `env` the driver value the
        // registry keeps alive for the program.
        unsafe { f(self.env as *mut u8, op, h) }
    }
}

/// A `.gos`-implemented driver registered into `crate::sql`.
struct GossamerDriver {
    name: String,
    disp: Dispatcher,
}

impl Driver for GossamerDriver {
    fn name(&self) -> &str {
        &self.name
    }
    fn open(&self, url: &str) -> Result<Box<dyn ConnectionImpl>, Error> {
        let token = next_handle();
        slot_set_inputs(
            token,
            SlotInputs {
                url,
                ..Default::default()
            },
        );
        let rc = self.disp.call(op::OPEN, token);
        if rc < 0 {
            let msg = slot_take_error(token);
            slot_remove(token);
            return Err(Error::driver(self.name.clone(), msg));
        }
        Ok(Box::new(GossamerConnection {
            name: self.name.clone(),
            disp: self.disp,
            token,
        }))
    }
}

/// Maps a `< 0` dispatch return into an `Error::driver` carrying the
/// driver's slot message; `>= 0` returns are the success value.
fn dispatch_result(name: &str, disp: &Dispatcher, op: i64, token: i64) -> Result<i64, Error> {
    let rc = disp.call(op, token);
    if rc < 0 {
        Err(Error::driver(name.to_string(), slot_take_error(token)))
    } else {
        Ok(rc)
    }
}

struct GossamerConnection {
    name: String,
    disp: Dispatcher,
    token: i64,
}

impl ConnectionImpl for GossamerConnection {
    fn prepare(&mut self, sql: &str) -> Result<Box<dyn StatementImpl>, Error> {
        let stmt_token = next_handle();
        slot_set_inputs(
            stmt_token,
            SlotInputs {
                sql,
                parent: self.token,
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::PREPARE, stmt_token).inspect_err(|_| {
            slot_remove(stmt_token);
        })?;
        Ok(Box::new(GossamerStatement {
            name: self.name.clone(),
            disp: self.disp,
            token: stmt_token,
        }))
    }

    fn begin(&mut self) -> Result<Box<dyn TransactionImpl>, Error> {
        self.begin_with(IsolationLevel::Default)
    }

    fn begin_with(&mut self, iso: IsolationLevel) -> Result<Box<dyn TransactionImpl>, Error> {
        let tx_token = next_handle();
        slot_set_inputs(
            tx_token,
            SlotInputs {
                parent: self.token,
                iso: iso_code(iso),
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::BEGIN_WITH, tx_token).inspect_err(|_| {
            slot_remove(tx_token);
        })?;
        Ok(Box::new(GossamerTransaction {
            name: self.name.clone(),
            disp: self.disp,
            token: tx_token,
        }))
    }

    fn ping(&mut self) -> Result<(), Error> {
        slot_set_inputs(self.token, SlotInputs::default());
        dispatch_result(&self.name, &self.disp, op::PING, self.token).map(|_| ())
    }

    fn set_busy_timeout(&mut self, ms: i64) -> Result<(), Error> {
        slot_set_inputs(
            self.token,
            SlotInputs {
                timeout: ms,
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::SET_BUSY_TIMEOUT, self.token).map(|_| ())
    }

    fn interrupt(&self) {
        slot_set_inputs(self.token, SlotInputs::default());
        let _ = self.disp.call(op::INTERRUPT, self.token);
    }

    fn copy_in(&mut self, sql: &str, data: &[u8]) -> Result<u64, Error> {
        slot_set_inputs(
            self.token,
            SlotInputs {
                sql,
                data: data.to_vec(),
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::COPY_IN, self.token).map(|n| n as u64)
    }

    fn copy_out(&mut self, sql: &str) -> Result<Vec<u8>, Error> {
        slot_set_inputs(
            self.token,
            SlotInputs {
                sql,
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::COPY_OUT, self.token)?;
        Ok(slot_take_bytes(self.token))
    }

    fn listen(&mut self, channel: &str) -> Result<(), Error> {
        slot_set_inputs(
            self.token,
            SlotInputs {
                channel,
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::LISTEN, self.token).map(|_| ())
    }

    fn unlisten(&mut self, channel: &str) -> Result<(), Error> {
        slot_set_inputs(
            self.token,
            SlotInputs {
                channel,
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::UNLISTEN, self.token).map(|_| ())
    }

    fn poll_notification(&mut self, timeout_ms: i64) -> Result<Option<Notification>, Error> {
        slot_set_inputs(
            self.token,
            SlotInputs {
                timeout: timeout_ms,
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::POLL_NOTIFICATION, self.token)?;
        Ok(slot_take_notification(self.token))
    }

    fn close(&mut self) -> Result<(), Error> {
        slot_set_inputs(self.token, SlotInputs::default());
        let r = dispatch_result(&self.name, &self.disp, op::CLOSE, self.token).map(|_| ());
        native_drop_handle(self.token);
        slot_remove(self.token);
        r
    }
}

struct GossamerStatement {
    name: String,
    disp: Dispatcher,
    token: i64,
}

impl StatementImpl for GossamerStatement {
    fn execute(&mut self, params: &[Value]) -> Result<u64, Error> {
        slot_set_inputs(
            self.token,
            SlotInputs {
                params: params.to_vec(),
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::STMT_EXECUTE, self.token).map(|n| n as u64)
    }

    fn query(&mut self, params: &[Value]) -> Result<Box<dyn RowsImpl>, Error> {
        let rows_token = next_handle();
        slot_set_inputs(
            self.token,
            SlotInputs {
                params: params.to_vec(),
                out_handle: rows_token,
                ..Default::default()
            },
        );
        // The rows token's slot must exist so the driver can emit
        // columns under it during the query dispatch.
        slot_set_inputs(
            rows_token,
            SlotInputs {
                parent: self.token,
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::STMT_QUERY, self.token).inspect_err(|_| {
            slot_remove(rows_token);
        })?;
        let columns = slot_take_columns(rows_token);
        Ok(Box::new(GossamerRows {
            name: self.name.clone(),
            disp: self.disp,
            token: rows_token,
            columns,
        }))
    }
}

impl Drop for GossamerStatement {
    fn drop(&mut self) {
        slot_set_inputs(self.token, SlotInputs::default());
        let _ = self.disp.call(op::STMT_CLOSE, self.token);
        slot_remove(self.token);
    }
}

struct GossamerRows {
    name: String,
    disp: Dispatcher,
    token: i64,
    columns: Vec<String>,
}

impl RowsImpl for GossamerRows {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>, Error> {
        // Reset row state before the advance, per the protocol.
        slot_with(self.token, |s| {
            s.row.clear();
            s.row_present = false;
            s.error.clear();
        });
        dispatch_result(&self.name, &self.disp, op::ROWS_NEXT, self.token)?;
        Ok(slot_take_row(self.token))
    }
    fn columns(&self) -> &[String] {
        &self.columns
    }
}

impl Drop for GossamerRows {
    fn drop(&mut self) {
        slot_set_inputs(self.token, SlotInputs::default());
        let _ = self.disp.call(op::ROWS_CLOSE, self.token);
        slot_remove(self.token);
    }
}

struct GossamerTransaction {
    name: String,
    disp: Dispatcher,
    token: i64,
}

impl TransactionImpl for GossamerTransaction {
    fn commit(&mut self) -> Result<(), Error> {
        slot_set_inputs(self.token, SlotInputs::default());
        let r = dispatch_result(&self.name, &self.disp, op::COMMIT, self.token).map(|_| ());
        slot_remove(self.token);
        r
    }

    fn rollback(&mut self) -> Result<(), Error> {
        slot_set_inputs(self.token, SlotInputs::default());
        let r = dispatch_result(&self.name, &self.disp, op::ROLLBACK, self.token).map(|_| ());
        slot_remove(self.token);
        r
    }

    fn execute(&mut self, sql: &str) -> Result<u64, Error> {
        slot_set_inputs(
            self.token,
            SlotInputs {
                sql,
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::TX_EXECUTE, self.token).map(|n| n as u64)
    }

    fn execute_params(&mut self, sql: &str, params: &[Value]) -> Result<u64, Error> {
        slot_set_inputs(
            self.token,
            SlotInputs {
                sql,
                params: params.to_vec(),
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::TX_EXECUTE_PARAMS, self.token).map(|n| n as u64)
    }

    fn query_params(&mut self, sql: &str, params: &[Value]) -> Result<Box<dyn RowsImpl>, Error> {
        let rows_token = next_handle();
        slot_set_inputs(
            self.token,
            SlotInputs {
                sql,
                params: params.to_vec(),
                out_handle: rows_token,
                ..Default::default()
            },
        );
        slot_set_inputs(
            rows_token,
            SlotInputs {
                parent: self.token,
                ..Default::default()
            },
        );
        dispatch_result(&self.name, &self.disp, op::TX_QUERY_PARAMS, self.token).inspect_err(
            |_| {
                slot_remove(rows_token);
            },
        )?;
        let columns = slot_take_columns(rows_token);
        Ok(Box::new(GossamerRows {
            name: self.name.clone(),
            disp: self.disp,
            token: rows_token,
            columns,
        }))
    }
}

fn iso_code(iso: IsolationLevel) -> i64 {
    match iso {
        IsolationLevel::Default => 0,
        IsolationLevel::ReadUncommitted => 1,
        IsolationLevel::ReadCommitted => 2,
        IsolationLevel::RepeatableRead => 3,
        IsolationLevel::Serializable => 4,
    }
}

// --- interpreter-tier facade helpers -------------------------------
//
// The compiled tier dispatches through the `GossamerDriver` adapter
// above (fn-addr transmute). The interpreter cannot transmute a code
// address, so it dispatches through `NativeDispatch::call_fn` inside
// its own sql builtins and drives the same slots through these
// `pub` helpers, parameterized by a dispatch closure
// (`Fn(op, token) -> i64`). The slot orchestration is identical to the
// compiled adapter; only the call mechanism differs.

/// Tracks a native result-set cursor on the interpreter tier: the
/// emitted column names and the most recent Row handle (cursor
/// semantics - advancing frees the previous Row).
struct NativeRows {
    columns: Vec<String>,
    current_row: i64,
}

static NATIVE_ROWS: Mutex<Option<HashMap<i64, NativeRows>>> = Mutex::new(None);

fn native_record_error(token: i64) -> i64 {
    sql_set_last_error(slot_take_error(token));
    -1
}

/// OPEN op: returns a fresh connection token, or -1 (message set).
pub fn native_facade_open(url: &str, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    let token = next_handle();
    slot_set_inputs(
        token,
        SlotInputs {
            url,
            ..Default::default()
        },
    );
    if dispatch(op::OPEN, token) < 0 {
        let rc = native_record_error(token);
        slot_remove(token);
        return rc;
    }
    token
}

/// PREPARE op: returns a fresh statement token, or -1.
pub fn native_facade_prepare(conn: i64, sql: &str, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    let stmt = next_handle();
    slot_set_inputs(
        stmt,
        SlotInputs {
            sql,
            parent: conn,
            ..Default::default()
        },
    );
    if dispatch(op::PREPARE, stmt) < 0 {
        let rc = native_record_error(stmt);
        slot_remove(stmt);
        return rc;
    }
    stmt
}

/// BEGIN_WITH op: returns a fresh transaction token, or -1.
pub fn native_facade_begin(conn: i64, iso: i64, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    let tx = next_handle();
    slot_set_inputs(
        tx,
        SlotInputs {
            parent: conn,
            iso,
            ..Default::default()
        },
    );
    if dispatch(op::BEGIN_WITH, tx) < 0 {
        let rc = native_record_error(tx);
        slot_remove(tx);
        return rc;
    }
    tx
}

/// A no-input scalar op (PING / INTERRUPT) returning the dispatch
/// result, or -1 (message set).
pub fn native_facade_scalar(token: i64, op: i64, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    slot_set_inputs(token, SlotInputs::default());
    let rc = dispatch(op, token);
    if rc < 0 {
        native_record_error(token)
    } else {
        rc
    }
}

/// SET_BUSY_TIMEOUT op.
pub fn native_facade_set_busy_timeout(
    conn: i64,
    ms: i64,
    dispatch: impl Fn(i64, i64) -> i64,
) -> i64 {
    slot_set_inputs(
        conn,
        SlotInputs {
            timeout: ms,
            ..Default::default()
        },
    );
    let rc = dispatch(op::SET_BUSY_TIMEOUT, conn);
    if rc < 0 {
        native_record_error(conn)
    } else {
        rc
    }
}

/// STMT_EXECUTE / TX_EXECUTE op family for a statement-or-tx token
/// carrying bound params. Returns rows affected, or -1.
pub fn native_facade_execute(
    token: i64,
    op: i64,
    sql: &str,
    params: Vec<Value>,
    dispatch: impl Fn(i64, i64) -> i64,
) -> i64 {
    slot_set_inputs(
        token,
        SlotInputs {
            sql,
            params,
            ..Default::default()
        },
    );
    let rc = dispatch(op, token);
    if rc < 0 {
        native_record_error(token)
    } else {
        rc
    }
}

/// STMT_QUERY / TX_QUERY_PARAMS op: dispatches the query, captures the
/// emitted columns under a fresh rows token, and returns that token
/// (registered in the native-rows registry), or -1.
pub fn native_facade_query(
    token: i64,
    op: i64,
    sql: &str,
    params: Vec<Value>,
    dispatch: impl Fn(i64, i64) -> i64,
) -> i64 {
    let rows = next_handle();
    slot_set_inputs(
        token,
        SlotInputs {
            sql,
            params,
            out_handle: rows,
            ..Default::default()
        },
    );
    slot_set_inputs(
        rows,
        SlotInputs {
            parent: token,
            ..Default::default()
        },
    );
    if dispatch(op, token) < 0 {
        let rc = native_record_error(token);
        slot_remove(rows);
        return rc;
    }
    let columns = slot_take_columns(rows);
    NATIVE_ROWS.lock().get_or_insert_with(HashMap::new).insert(
        rows,
        NativeRows {
            columns,
            current_row: 0,
        },
    );
    rows
}

/// ROWS_NEXT op: advances the native cursor, registering the row in
/// the shared Row registry. Returns a Row handle, 0 on end-of-set, or
/// -1. Mirrors `sql_rows_next_row`'s cursor semantics.
pub fn native_facade_rows_next(rows: i64, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    let Some(columns) = NATIVE_ROWS
        .lock()
        .as_ref()
        .and_then(|m| m.get(&rows))
        .map(|nr| nr.columns.clone())
    else {
        sql_set_last_error(INVALID_ROWS);
        return -1;
    };
    slot_with(rows, |s| {
        s.row.clear();
        s.row_present = false;
        s.error.clear();
    });
    if dispatch(op::ROWS_NEXT, rows) < 0 {
        return native_record_error(rows);
    }
    let prev = NATIVE_ROWS
        .lock()
        .as_ref()
        .and_then(|m| m.get(&rows))
        .map_or(0, |nr| nr.current_row);
    let Some(values) = slot_take_row(rows) else {
        row_unregister(prev);
        native_rows_drop(rows, &dispatch);
        return 0;
    };
    row_unregister(prev);
    let row = row_register(Row { values, columns });
    if let Some(map) = NATIVE_ROWS.lock().as_mut()
        && let Some(nr) = map.get_mut(&rows)
    {
        nr.current_row = row;
    }
    row
}

/// Comma-joined column names for a native rows token.
pub fn native_facade_rows_columns(rows: i64) -> String {
    let guard = NATIVE_ROWS.lock();
    guard
        .as_ref()
        .and_then(|m| m.get(&rows))
        .map(|nr| nr.columns.join(","))
        .unwrap_or_default()
}

/// True when `token` names a live native result-set cursor.
pub fn native_is_rows(token: i64) -> bool {
    NATIVE_ROWS
        .lock()
        .as_ref()
        .is_some_and(|m| m.contains_key(&token))
}

fn native_rows_drop(rows: i64, dispatch: &impl Fn(i64, i64) -> i64) {
    let entry = NATIVE_ROWS.lock().as_mut().and_then(|m| m.remove(&rows));
    if let Some(nr) = entry {
        row_unregister(nr.current_row);
    }
    slot_set_inputs(rows, SlotInputs::default());
    let _ = dispatch(op::ROWS_CLOSE, rows);
    slot_remove(rows);
}

/// ROWS_CLOSE op: releases a native cursor (idempotent, returns 0).
pub fn native_facade_rows_close(rows: i64, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    if native_is_rows(rows) {
        native_rows_drop(rows, &dispatch);
    }
    0
}

/// STMT_CLOSE op: releases a native statement (idempotent, returns 0).
pub fn native_facade_stmt_close(stmt: i64, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    slot_set_inputs(stmt, SlotInputs::default());
    let _ = dispatch(op::STMT_CLOSE, stmt);
    slot_remove(stmt);
    0
}

/// COMMIT / ROLLBACK op: finalizes and releases a native transaction.
pub fn native_facade_tx_finish(tx: i64, op: i64, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    slot_set_inputs(tx, SlotInputs::default());
    let rc = dispatch(op, tx);
    let out = if rc < 0 { native_record_error(tx) } else { rc };
    slot_remove(tx);
    out
}

/// CLOSE op: closes a native connection, dropping its stashed handle
/// and slot. Returns 0, or -1.
pub fn native_facade_close(conn: i64, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    slot_set_inputs(conn, SlotInputs::default());
    let rc = dispatch(op::CLOSE, conn);
    let out = if rc < 0 {
        native_record_error(conn)
    } else {
        rc
    };
    native_drop_handle(conn);
    slot_remove(conn);
    out
}

/// COPY_IN op.
pub fn native_facade_copy_in(
    conn: i64,
    sql: &str,
    data: Vec<u8>,
    dispatch: impl Fn(i64, i64) -> i64,
) -> i64 {
    slot_set_inputs(
        conn,
        SlotInputs {
            sql,
            data,
            ..Default::default()
        },
    );
    let rc = dispatch(op::COPY_IN, conn);
    if rc < 0 {
        native_record_error(conn)
    } else {
        rc
    }
}

/// COPY_OUT op: runs the copy and stores the emitted bytes in the
/// connection's copy-out slot for `sql_copy_out_take`. Returns the
/// byte count, or -1.
pub fn native_facade_copy_out(conn: i64, sql: &str, dispatch: impl Fn(i64, i64) -> i64) -> i64 {
    slot_set_inputs(
        conn,
        SlotInputs {
            sql,
            ..Default::default()
        },
    );
    if dispatch(op::COPY_OUT, conn) < 0 {
        return native_record_error(conn);
    }
    let bytes = slot_take_bytes(conn);
    let n = bytes.len() as i64;
    sql_copy_out_store(conn, bytes);
    n
}

/// LISTEN / UNLISTEN op.
pub fn native_facade_listen(
    conn: i64,
    op: i64,
    channel: &str,
    dispatch: impl Fn(i64, i64) -> i64,
) -> i64 {
    slot_set_inputs(
        conn,
        SlotInputs {
            channel,
            ..Default::default()
        },
    );
    let rc = dispatch(op, conn);
    if rc < 0 {
        native_record_error(conn)
    } else {
        rc
    }
}

/// POLL_NOTIFICATION op: 1 when one arrived (stored for the
/// `sql_notification_*` getters), 0 on none, -1 on error.
pub fn native_facade_poll_notification(
    conn: i64,
    timeout_ms: i64,
    dispatch: impl Fn(i64, i64) -> i64,
) -> i64 {
    slot_set_inputs(
        conn,
        SlotInputs {
            timeout: timeout_ms,
            ..Default::default()
        },
    );
    if dispatch(op::POLL_NOTIFICATION, conn) < 0 {
        return native_record_error(conn);
    }
    match slot_take_notification(conn) {
        Some(n) => {
            let mut guard = LAST_NOTIFICATION.lock();
            guard.get_or_insert_with(HashMap::new).insert(conn, n);
            1
        }
        None => 0,
    }
}

/// Builds and registers a `.gos` driver (compiled tier). `name` is the
/// driver name for `sql::open`; `env`/`fn_addr` are the driver value's
/// env pointer and the address of its `dispatch` method.
///
/// # Safety
///
/// `env` is a driver value that stays live for the process, and `fn_addr` the
/// entry address of its compiled `dispatch` method.
pub unsafe fn register_native_driver(name: &str, env: usize, fn_addr: usize) {
    crate::sql::register(Arc::new(GossamerDriver {
        name: name.to_string(),
        disp: Dispatcher { env, fn_addr },
    }));
}

/// `sql::register_native(name, driver)` (compiled tier). The MIR
/// lowerer passes the driver value's env pointer and the address of
/// `<Type>::dispatch`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_register_native(
    name: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) {
    // SAFETY: this function's contract is the one the callee states.
    unsafe { register_native_driver(&c_str_to_string(name), env as usize, fn_addr as usize) };
}

// --- native_* C-ABI shims (compiled tier) --------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_url(h: i64) -> *mut c_char {
    alloc_cstring(native_url(h).as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_sql(h: i64) -> *mut c_char {
    alloc_cstring(native_sql(h).as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_parent(h: i64) -> i64 {
    native_parent(h)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_out_handle(h: i64) -> i64 {
    native_out_handle(h)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_iso(h: i64) -> i64 {
    native_iso(h)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_timeout(h: i64) -> i64 {
    native_timeout(h)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_channel(h: i64) -> *mut c_char {
    alloc_cstring(native_channel(h).as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_param_count(h: i64) -> i64 {
    native_param_count(h)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_param(h: i64, i: i64) -> i64 {
    native_param(h, i)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_data(h: i64) -> *mut super::vec::GosVec {
    bytes_to_gosvec(&native_data(h))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_native_push_column(h: i64, name: *const c_char) {
    // SAFETY: `name` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    native_push_column(h, &unsafe { c_str_to_string(name) });
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_push_value(h: i64, value_handle: i64) {
    native_push_value(h, value_handle);
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_row_ready(h: i64) {
    native_row_ready(h);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_native_set_error(h: i64, msg: *const c_char) {
    // SAFETY: `msg` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    native_set_error(h, &unsafe { c_str_to_string(msg) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_native_emit_bytes(h: i64, data: *const super::vec::GosVec) {
    // SAFETY: codegen passes a live GosVec pointer for a `[u8]` arg.
    let bytes = unsafe { crate::c_abi::vec::vec_bytes(data) };
    native_emit_bytes(h, &bytes);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_native_set_notification(
    h: i64,
    chan: *const c_char,
    payload: *const c_char,
    pid: i64,
) {
    // SAFETY: `chan` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    // `payload` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    native_set_notification(
        h,
        &unsafe { c_str_to_string(chan) },
        &unsafe { c_str_to_string(payload) },
        pid,
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_set_handle(h: i64, value: i64) {
    native_set_handle(h, value);
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_handle(h: i64) -> i64 {
    native_handle(h)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_value_null() -> i64 {
    native_value_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_value_bool(b: i64) -> i64 {
    native_value_bool(b != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_value_int(n: i64) -> i64 {
    native_value_int(n)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_value_float(f: f64) -> i64 {
    native_value_float(f)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_native_value_text(s: *const c_char) -> i64 {
    // SAFETY: `s` is this shim's argument, as `c_str_to_string` requires (C-ABI contract).
    native_value_text(&unsafe { c_str_to_string(s) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sql_native_value_blob(data: *const super::vec::GosVec) -> i64 {
    // SAFETY: codegen passes a live GosVec pointer for a `[u8]` arg.
    let bytes = unsafe { crate::c_abi::vec::vec_bytes(data) };
    native_value_blob(&bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_value_kind(handle: i64) -> i64 {
    native_value_kind(handle)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_value_int_of(handle: i64) -> i64 {
    native_value_int_of(handle)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_value_float_of(handle: i64) -> f64 {
    native_value_float_of(handle)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_value_text_of(handle: i64) -> *mut c_char {
    alloc_cstring(native_value_text_of(handle).as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sql_native_value_blob_of(handle: i64) -> *mut super::vec::GosVec {
    bytes_to_gosvec(&native_value_blob_of(handle))
}

/// Materializes `bytes` as a one-byte-per-slot `[u8]` GosVec (the
/// runtime's `[u8]` ABI), matching `gos_rt_sql_row_get_blob_vec`.
fn bytes_to_gosvec(bytes: &[u8]) -> *mut super::vec::GosVec {
    super::encoding::bytes_to_gosvec(bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use super::*;
    use crate::sql::{ConnectionImpl, Driver, RowsImpl, StatementImpl, TransactionImpl};

    /// Stub driver yielding `n` single-column Int rows per query,
    /// optionally erroring at row index `fail_at`.
    struct StubDriver;

    struct StubConn;

    struct StubStmt {
        n: usize,
        fail_at: Option<usize>,
    }

    struct StubRows {
        n: usize,
        fail_at: Option<usize>,
        idx: usize,
        cols: Vec<String>,
    }

    struct StubTx;

    struct BlockingPrepareDriver {
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
    }

    struct BlockingPrepareConn {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Driver for StubDriver {
        fn name(&self) -> &'static str {
            "stub-cursor-test"
        }
        fn open(&self, _url: &str) -> Result<Box<dyn ConnectionImpl>, Error> {
            Ok(Box::new(StubConn))
        }
    }

    impl Driver for BlockingPrepareDriver {
        fn name(&self) -> &'static str {
            "blocking-session-test"
        }

        fn open(&self, _url: &str) -> Result<Box<dyn ConnectionImpl>, Error> {
            let started = self
                .started
                .lock()
                .take()
                .ok_or_else(|| Error::driver("blocking", "connection already opened"))?;
            let release = self
                .release
                .lock()
                .take()
                .ok_or_else(|| Error::driver("blocking", "connection already opened"))?;
            Ok(Box::new(BlockingPrepareConn { started, release }))
        }
    }

    impl ConnectionImpl for StubConn {
        fn prepare(&mut self, sql: &str) -> Result<Box<dyn StatementImpl>, Error> {
            // "rows:N" yields N rows; "rows:N:fail:K" errors at index K.
            let mut parts = sql.split(':');
            let _ = parts.next();
            let n = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            let fail_at = match (parts.next(), parts.next()) {
                (Some("fail"), Some(k)) => k.parse().ok(),
                _ => None,
            };
            Ok(Box::new(StubStmt { n, fail_at }))
        }
        fn begin(&mut self) -> Result<Box<dyn TransactionImpl>, Error> {
            Ok(Box::new(StubTx))
        }
        fn close(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ConnectionImpl for BlockingPrepareConn {
        fn prepare(&mut self, _sql: &str) -> Result<Box<dyn StatementImpl>, Error> {
            self.started
                .send(())
                .map_err(|e| Error::driver("blocking", e.to_string()))?;
            self.release
                .recv()
                .map_err(|e| Error::driver("blocking", e.to_string()))?;
            Ok(Box::new(StubStmt {
                n: 0,
                fail_at: None,
            }))
        }

        fn begin(&mut self) -> Result<Box<dyn TransactionImpl>, Error> {
            Ok(Box::new(StubTx))
        }

        fn close(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl StatementImpl for StubStmt {
        fn execute(&mut self, _params: &[Value]) -> Result<u64, Error> {
            Ok(0)
        }
        fn query(&mut self, _params: &[Value]) -> Result<Box<dyn RowsImpl>, Error> {
            Ok(Box::new(StubRows {
                n: self.n,
                fail_at: self.fail_at,
                idx: 0,
                cols: vec!["c".into()],
            }))
        }
    }

    impl TransactionImpl for StubTx {
        fn commit(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn rollback(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn execute(&mut self, _sql: &str) -> Result<u64, Error> {
            Ok(0)
        }
    }

    impl RowsImpl for StubRows {
        fn next_row(&mut self) -> Result<Option<Vec<Value>>, Error> {
            if Some(self.idx) == self.fail_at {
                return Err(Error::driver("stub", "forced failure"));
            }
            if self.idx >= self.n {
                return Ok(None);
            }
            let v = self.idx as i64;
            self.idx += 1;
            Ok(Some(vec![Value::Int(v)]))
        }
        fn columns(&self) -> &[String] {
            &self.cols
        }
    }

    /// Serializes the stub-driver tests: they all read or drain the
    /// process-global `LAST_ERROR` slot (errno-style by design - see
    /// its doc), so parallel test threads race on take/overwrite.
    static ERROR_SLOT_LOCK: Mutex<()> = Mutex::new(());

    fn open_stub() -> i64 {
        static REGISTER: std::sync::Once = std::sync::Once::new();
        REGISTER.call_once(|| crate::sql::register(Arc::new(StubDriver)));
        let conn = sql_open_handle("stub-cursor-test", "mem");
        assert!(conn > 0, "stub open failed: {}", sql_take_last_error());
        conn
    }

    fn stale(row: i64) -> bool {
        sql_row_kind(row, "c") == -2
    }

    #[test]
    fn drained_iteration_releases_every_handle() {
        let _guard = ERROR_SLOT_LOCK.lock();
        let conn = open_stub();
        let rows = sql_conn_query_params(conn, "rows:3", 0);
        assert!(rows > 0);
        let mut returned = Vec::new();
        loop {
            let row = sql_rows_next_row(rows);
            assert!(row >= 0);
            if row == 0 {
                break;
            }
            assert_eq!(sql_row_kind(row, "c"), 2, "live row must read as Int");
            returned.push(row);
        }
        assert_eq!(returned.len(), 3);
        for row in returned {
            assert!(stale(row), "drained rows must release every Row handle");
        }
        assert_eq!(sql_rows_next_row(rows), -1, "exhausted cursor is released");
        let _ = sql_take_last_error();
        assert_eq!(sql_rows_close(rows), 0, "close after exhaustion is a no-op");
        assert_eq!(sql_conn_close(conn), 0);
    }

    #[test]
    fn advancing_frees_the_previous_row() {
        let _guard = ERROR_SLOT_LOCK.lock();
        let conn = open_stub();
        let rows = sql_conn_query_params(conn, "rows:2", 0);
        let first = sql_rows_next_row(rows);
        assert!(first > 0);
        let second = sql_rows_next_row(rows);
        assert!(second > 0);
        assert!(
            stale(first),
            "cursor semantics: previous row freed on advance"
        );
        assert_eq!(sql_row_kind(second, "c"), 2);
        assert_eq!(sql_rows_close(rows), 0);
        assert!(stale(second), "close frees the current row");
        assert_eq!(sql_conn_close(conn), 0);
    }

    #[test]
    fn rows_close_is_idempotent() {
        let _guard = ERROR_SLOT_LOCK.lock();
        let conn = open_stub();
        let rows = sql_conn_query_params(conn, "rows:2", 0);
        let first = sql_rows_next_row(rows);
        assert!(first > 0);
        assert_eq!(sql_rows_close(rows), 0);
        assert_eq!(sql_rows_close(rows), 0);
        assert_eq!(sql_rows_next_row(rows), -1, "closed cursor is invalid");
        let _ = sql_take_last_error();
        assert_eq!(sql_conn_close(conn), 0);
    }

    #[test]
    fn conn_close_sweeps_abandoned_cursors() {
        let _guard = ERROR_SLOT_LOCK.lock();
        let conn = open_stub();
        let rows_a = sql_conn_query_params(conn, "rows:5", 0);
        let rows_b = sql_conn_query_params(conn, "rows:5", 0);
        let row_a = sql_rows_next_row(rows_a);
        assert!(row_a > 0);
        assert_eq!(sql_conn_close(conn), 0);
        assert!(stale(row_a), "conn close frees swept cursors' rows");
        assert_eq!(sql_rows_next_row(rows_a), -1);
        let _ = sql_take_last_error();
        assert_eq!(sql_rows_next_row(rows_b), -1);
        let _ = sql_take_last_error();
    }

    #[test]
    fn failed_advance_keeps_cursor_and_current_row() {
        let _guard = ERROR_SLOT_LOCK.lock();
        let conn = open_stub();
        let rows = sql_conn_query_params(conn, "rows:3:fail:1", 0);
        let first = sql_rows_next_row(rows);
        assert!(first > 0);
        assert_eq!(sql_rows_next_row(rows), -1, "stub fails at index 1");
        assert!(
            sql_take_last_error().contains("forced failure"),
            "error message recorded"
        );
        assert_eq!(sql_row_kind(first, "c"), 2, "row survives a failed advance");
        assert_eq!(sql_rows_close(rows), 0, "cursor still closable after error");
        assert!(stale(first));
        assert_eq!(sql_conn_close(conn), 0);
    }

    #[test]
    fn connection_registry_lock_is_released_during_driver_io() {
        let _guard = ERROR_SLOT_LOCK.lock();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        crate::sql::register(Arc::new(BlockingPrepareDriver {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(Some(release_rx)),
        }));
        let conn = sql_open_handle("blocking-session-test", "mem");
        assert!(conn > 0, "open: {}", sql_take_last_error());

        let worker = std::thread::spawn(move || sql_conn_prepare(conn, "blocked"));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("driver prepare should start");
        assert!(
            CONN_HANDLES.try_lock().is_some(),
            "a blocked driver call must not retain the connection registry lock"
        );
        release_tx.send(()).expect("release driver prepare");
        let stmt = worker.join().expect("prepare worker panicked");
        assert!(stmt > 0, "prepare: {}", sql_take_last_error());
        assert_eq!(sql_stmt_close(stmt), 0);
        assert_eq!(sql_conn_close(conn), 0);
    }

    /// Drives the native-driver facade helpers with a closure standing
    /// in for a `.gos` driver's `dispatch`, exercising the shared slot
    /// orchestration both tiers run (the compiled adapter transmutes a
    /// fn-addr; the interp re-enters the VM; both land here). A trivial
    /// driver: STMT_QUERY emits one column under the rows token, the
    /// first ROWS_NEXT yields one Text row, the next ends the set.
    #[test]
    fn native_facade_drives_a_one_row_cursor() {
        let _guard = ERROR_SLOT_LOCK.lock();
        let pending = std::cell::Cell::new(false);
        let dispatch = |op: i64, h: i64| -> i64 {
            match op {
                op::OPEN | op::PREPARE | op::STMT_CLOSE | op::ROWS_CLOSE | op::CLOSE => 0,
                op::STMT_QUERY => {
                    let rows = native_out_handle(h);
                    native_push_column(rows, "greeting");
                    pending.set(true);
                    0
                }
                op::ROWS_NEXT => {
                    if pending.get() {
                        native_push_value(h, native_value_text("hello"));
                        native_row_ready(h);
                        pending.set(false);
                    }
                    0
                }
                _ => {
                    native_set_error(h, "unsupported op");
                    -1
                }
            }
        };

        let conn = native_facade_open("memory://x", dispatch);
        assert!(conn > 0, "open: {}", sql_take_last_error());
        let stmt = native_facade_prepare(conn, "SELECT greeting", dispatch);
        assert!(stmt > 0);
        let rows = native_facade_query(
            stmt,
            op::STMT_QUERY,
            "SELECT greeting",
            Vec::new(),
            dispatch,
        );
        assert!(rows > 0);
        assert_eq!(native_facade_rows_columns(rows), "greeting");

        let row = native_facade_rows_next(rows, dispatch);
        assert!(row > 0, "first advance yields a row");
        assert_eq!(sql_row_get_text(row, "greeting"), "hello");
        assert_eq!(sql_row_kind(row, "greeting"), 4, "Text column");

        assert_eq!(
            native_facade_rows_next(rows, dispatch),
            0,
            "second advance ends the set"
        );
        assert!(stale(row), "end-of-set releases the final row");

        assert_eq!(native_facade_stmt_close(stmt, dispatch), 0);
        assert_eq!(native_facade_close(conn, dispatch), 0);
    }

    /// A `< 0` dispatch return surfaces the driver's slot error message
    /// through `sql_take_last_error`, the sentinel convention both tiers
    /// share.
    #[test]
    fn native_facade_open_reports_driver_error() {
        let _guard = ERROR_SLOT_LOCK.lock();
        let dispatch = |op: i64, h: i64| -> i64 {
            if op == op::OPEN {
                native_set_error(h, "connection refused");
                return -1;
            }
            0
        };
        let conn = native_facade_open("memory://x", dispatch);
        assert_eq!(conn, -1);
        assert!(sql_take_last_error().contains("connection refused"));
    }
}
