//! SQL trait surface and driver registry, shared by the bytecode VM,
//! Cranelift JIT, and LLVM AOT through [`crate::c_abi::sql`].
//!
//! Drivers are third-party Rust crates that implement [`Driver`] and
//! call [`register`] at startup. No driver auto-registers; callers
//! that want `SQLite` invoke
//! `gossamer_std::database::sql::sqlite::register()` from their
//! Rust startup code (the reference driver lives in `gossamer-std`).
//! `Postgres` / `MySQL` drivers plug in the same way.
//!
//! User code goes through [`open`] to get a [`Box<dyn ConnectionImpl>`].
//! The high-level wrappers in `gossamer-std::database::sql::{Conn,
//! Stmt, Tx, Rows, Row}` are convenience layers on top of those
//! trait objects; the C-ABI shims in [`crate::c_abi::sql`] operate on
//! the trait objects directly through handle registries so compiled
//! Gossamer code sees the same surface as `gos`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use parking_lot::Mutex;
use thiserror::Error;

/// Typed value passed to / returned from the database.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `NULL` literal.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit floating-point.
    Float(f64),
    /// UTF-8 text.
    Text(String),
    /// Binary blob.
    Blob(Vec<u8>),
}

/// Coarse category of a [`Value`] - useful when a driver returns a
/// dynamically-typed column and the caller wants the kind without
/// matching the full enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `NULL`.
    Null,
    /// Boolean.
    Bool,
    /// 64-bit signed integer.
    Int,
    /// 64-bit floating-point.
    Float,
    /// UTF-8 text.
    Text,
    /// Binary blob.
    Blob,
}

impl Value {
    /// Returns the coarse [`Kind`] of this value.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Value::Null => Kind::Null,
            Value::Bool(_) => Kind::Bool,
            Value::Int(_) => Kind::Int,
            Value::Float(_) => Kind::Float,
            Value::Text(_) => Kind::Text,
            Value::Blob(_) => Kind::Blob,
        }
    }
}

/// Transaction isolation level. Drivers map to their dialect's
/// equivalent. `Default` defers to the driver's native default
/// (usually `READ COMMITTED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// Driver-default isolation.
    Default,
    /// Dirty reads allowed.
    ReadUncommitted,
    /// Dirty reads forbidden.
    ReadCommitted,
    /// Repeatable reads guaranteed within the transaction.
    RepeatableRead,
    /// Strict serializability.
    Serializable,
}

/// Driver-specific error payload. Carries a stable [`DriverErrorKind`]
/// for caller-side classification plus a free-form message.
#[derive(Debug, Clone, PartialEq)]
pub struct DriverError {
    /// Driver identifier (e.g. `"sqlite"`).
    pub driver: String,
    /// Coarse error classification.
    pub kind: DriverErrorKind,
    /// Lower-level message.
    pub message: String,
}

/// Coarse classification of a driver error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverErrorKind {
    /// Generic driver-side failure.
    Other,
    /// Unique-constraint violation.
    UniqueViolation,
    /// Foreign-key violation.
    ForeignKeyViolation,
    /// Connection refused / lost / closed by peer.
    Connection,
    /// Statement timed out.
    Timeout,
    /// Statement was cancelled.
    Cancelled,
}

/// Errors raised by drivers and the façade.
#[derive(Debug, Clone, Error)]
pub enum Error {
    /// Driver name was not registered.
    #[error("sql: no driver registered as {0:?}")]
    UnknownDriver(String),
    /// Driver-specific failure.
    #[error("sql: driver {driver}: {message}")]
    Driver {
        /// Driver identifier (e.g. `"sqlite"`).
        driver: String,
        /// Lower-level message.
        message: String,
    },
    /// Caller asked for the wrong column type.
    #[error("sql: column type mismatch: {0}")]
    Type(String),
    /// Connection has been closed.
    #[error("sql: connection closed")]
    Closed,
    /// Connection pool reached its capacity and the wait deadline
    /// elapsed before a slot became available.
    #[error("sql: connection pool exhausted (capacity={capacity})")]
    PoolExhausted {
        /// Configured pool capacity.
        capacity: usize,
    },
    /// Statement was cancelled (context deadline / cancel hit).
    #[error("sql: cancelled")]
    Cancelled,
}

impl Error {
    /// Builds a [`Error::Driver`] with the given driver name and
    /// message.
    pub fn driver(driver: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Driver {
            driver: driver.into(),
            message: message.into(),
        }
    }
}

/// An asynchronous notification (`LISTEN` / `NOTIFY`) delivered to a
/// connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Channel the notification was sent on.
    pub channel: String,
    /// Payload string (may be empty).
    pub payload: String,
    /// Backend process id of the notifying session, or 0 when the
    /// driver does not report one.
    pub process_id: i64,
}

fn unsupported(op: &str) -> Error {
    Error::driver("sql", format!("{op} is not supported by this driver"))
}

/// Driver trait - concrete drivers implement [`open`] and return a
/// [`Box<dyn ConnectionImpl>`] backed by their own state.
pub trait Driver: Send + Sync {
    /// Driver name (for [`open`]).
    fn name(&self) -> &str;
    /// Opens a connection to the database identified by `url`.
    fn open(&self, url: &str) -> Result<Box<dyn ConnectionImpl>, Error>;
}

/// Connection trait. The wrapped trait object is what user code
/// drives.
pub trait ConnectionImpl: Send {
    /// Prepares a statement for repeated execution.
    fn prepare(&mut self, sql: &str) -> Result<Box<dyn StatementImpl>, Error>;
    /// Begins a transaction.
    fn begin(&mut self) -> Result<Box<dyn TransactionImpl>, Error>;
    /// Begins a transaction at the supplied isolation level. Default
    /// implementation falls through to [`Self::begin`].
    fn begin_with(&mut self, _iso: IsolationLevel) -> Result<Box<dyn TransactionImpl>, Error> {
        self.begin()
    }
    /// Round-trips a `SELECT 1`-equivalent. Default implementation
    /// uses `prepare + execute("SELECT 1")`.
    fn ping(&mut self) -> Result<(), Error> {
        let mut stmt = self.prepare("SELECT 1")?;
        stmt.execute(&[])?;
        Ok(())
    }
    /// Sets the driver-specific busy timeout in milliseconds. The
    /// default is a no-op; `SQLite` overrides this with
    /// `sqlite3_busy_timeout`.
    fn set_busy_timeout(&mut self, _ms: i64) -> Result<(), Error> {
        Ok(())
    }
    /// Signals to the driver that any in-flight statement on this
    /// connection should be cancelled. `SQLite` calls
    /// `sqlite3_interrupt`; default is a no-op.
    fn interrupt(&self) {}
    /// Bulk-loads `data` through the dialect's copy mechanism
    /// (`COPY … FROM STDIN` on `PostgreSQL`); returns rows written.
    /// Capability-gated: the default reports the operation as
    /// unsupported.
    fn copy_in(&mut self, _sql: &str, _data: &[u8]) -> Result<u64, Error> {
        Err(unsupported("copy_in"))
    }
    /// Bulk-extracts rows through the dialect's copy mechanism
    /// (`COPY … TO STDOUT` on `PostgreSQL`); returns the raw bytes.
    /// Capability-gated: the default reports the operation as
    /// unsupported.
    fn copy_out(&mut self, _sql: &str) -> Result<Vec<u8>, Error> {
        Err(unsupported("copy_out"))
    }
    /// Subscribes this connection to notifications on `channel`.
    /// Capability-gated: the default reports the operation as
    /// unsupported.
    fn listen(&mut self, _channel: &str) -> Result<(), Error> {
        Err(unsupported("listen"))
    }
    /// Unsubscribes this connection from `channel`.
    fn unlisten(&mut self, _channel: &str) -> Result<(), Error> {
        Err(unsupported("unlisten"))
    }
    /// Returns the next pending notification, waiting up to
    /// `timeout_ms` (0 = poll without waiting). `Ok(None)` means no
    /// notification arrived within the window.
    fn poll_notification(&mut self, _timeout_ms: i64) -> Result<Option<Notification>, Error> {
        Err(unsupported("poll_notification"))
    }
    /// Closes the connection. Subsequent calls return [`Error::Closed`].
    fn close(&mut self) -> Result<(), Error>;
}

/// Prepared statement trait.
pub trait StatementImpl: Send {
    /// Executes the statement with positional bindings; returns the
    /// number of rows affected.
    fn execute(&mut self, params: &[Value]) -> Result<u64, Error>;
    /// Runs the statement and returns rows.
    fn query(&mut self, params: &[Value]) -> Result<Box<dyn RowsImpl>, Error>;
}

/// Transaction trait.
pub trait TransactionImpl: Send {
    /// Commits the transaction.
    fn commit(&mut self) -> Result<(), Error>;
    /// Rolls back.
    fn rollback(&mut self) -> Result<(), Error>;
    /// Executes raw SQL inside the transaction (no parameters).
    fn execute(&mut self, sql: &str) -> Result<u64, Error>;
    /// Executes a statement with positional bindings inside the
    /// transaction; returns rows affected. Capability-gated: the
    /// default reports the operation as unsupported.
    fn execute_params(&mut self, _sql: &str, _params: &[Value]) -> Result<u64, Error> {
        Err(unsupported("execute_params in a transaction"))
    }
    /// Runs a query with positional bindings inside the transaction.
    /// Capability-gated: the default reports the operation as
    /// unsupported.
    fn query_params(&mut self, _sql: &str, _params: &[Value]) -> Result<Box<dyn RowsImpl>, Error> {
        Err(unsupported("query_params in a transaction"))
    }
    /// Creates a savepoint named `name` inside this transaction.
    /// Default implementation runs `SAVEPOINT name` as raw SQL.
    fn savepoint(&mut self, name: &str) -> Result<(), Error> {
        self.execute(&format!("SAVEPOINT {}", valid_savepoint_name(name)?))?;
        Ok(())
    }
    /// Releases (commits) a savepoint named `name`.
    fn release_savepoint(&mut self, name: &str) -> Result<(), Error> {
        self.execute(&format!(
            "RELEASE SAVEPOINT {}",
            valid_savepoint_name(name)?
        ))?;
        Ok(())
    }
    /// Rolls back to a savepoint named `name`.
    fn rollback_to_savepoint(&mut self, name: &str) -> Result<(), Error> {
        self.execute(&format!(
            "ROLLBACK TO SAVEPOINT {}",
            valid_savepoint_name(name)?
        ))?;
        Ok(())
    }
}

/// Validates a savepoint identifier (which cannot be a bound parameter)
/// against `[A-Za-z_][A-Za-z0-9_]*`, returning it unchanged or an error.
/// Rejects a user-derived name that would otherwise inject SQL into the
/// interpolated `SAVEPOINT <name>` statement.
fn valid_savepoint_name(name: &str) -> Result<&str, Error> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(name)
    } else {
        Err(Error::driver(
            "sql",
            format!("invalid savepoint name `{name}`: must be a simple identifier"),
        ))
    }
}

/// Rows trait - iterate result sets.
pub trait RowsImpl: Send {
    /// Pulls the next row, or `None` on end-of-set.
    fn next_row(&mut self) -> Result<Option<Vec<Value>>, Error>;
    /// Column names in the result set.
    fn columns(&self) -> &[String];
}

// --- registry ------------------------------------------------------

// The storage lives in `c_abi::sql` behind an unmangled symbol so a
// `gos build` binary that links two gossamer-runtime copies (runtime
// staticlib + rust-bindings staticlib, `--allow-multiple-definition`)
// still shares ONE registry; see `c_abi::sql::driver_registry`.
fn registry() -> &'static Mutex<Vec<Arc<dyn Driver>>> {
    crate::c_abi::sql::driver_registry()
}

/// Registers a driver so [`open`] can find it. Idempotent on driver
/// name - re-registering replaces the previous handle.
pub fn register(driver: Arc<dyn Driver>) {
    let mut reg = registry().lock();
    let name = driver.name().to_string();
    reg.retain(|d| d.name() != name);
    reg.push(driver);
}

/// Looks up a driver and opens a connection. Returns
/// [`Error::UnknownDriver`] if no driver is registered under that
/// name.
pub fn open(name: &str, url: &str) -> Result<Box<dyn ConnectionImpl>, Error> {
    let reg = registry().lock();
    for driver in reg.iter() {
        if driver.name() == name {
            let driver = Arc::clone(driver);
            drop(reg);
            return driver.open(url);
        }
    }
    Err(Error::UnknownDriver(name.to_string()))
}

/// Returns the names of every registered driver in registration
/// order.
#[must_use]
pub fn drivers() -> Vec<String> {
    registry()
        .lock()
        .iter()
        .map(|d| d.name().to_string())
        .collect()
}

/// Reconstructs a `&dyn ConnectionImpl` from the exposed address and
/// calls its `interrupt()` method. The caller (typically the SQL
/// context-cancellation watchdog in gossamer-std) is responsible for
/// ensuring the original `&mut Box<dyn ConnectionImpl>` outlives this
/// call. `ConnectionImpl::interrupt` is documented as thread-safe
/// w.r.t. the connection so a concurrent statement may be in flight.
pub fn interrupt_connection_by_addr(addr: usize) {
    if addr == 0 {
        return;
    }
    // the watchdog joins before the original &mut goes out
    // of scope, so the trait object is still live. Trait-object
    // pointers are 2 words (data + vtable); we can't reconstruct
    // the dyn pointer from a single address. Instead, we round-trip
    // through a small registry keyed by address.
    let map = INTERRUPT_REGISTRY.lock();
    if let Some(callback) = map.get(&addr) {
        callback();
    }
}

static INTERRUPT_REGISTRY: Mutex<std::collections::BTreeMap<usize, Box<dyn Fn() + Send + Sync>>> =
    Mutex::new(std::collections::BTreeMap::new());

/// Registers a `Fn()` callback under `addr` that
/// [`interrupt_connection_by_addr`] will invoke when the matching
/// context cancels.
pub fn register_interrupt_callback(addr: usize, callback: Box<dyn Fn() + Send + Sync>) {
    INTERRUPT_REGISTRY.lock().insert(addr, callback);
}

/// Removes the interrupt callback under `addr`.
pub fn unregister_interrupt_callback(addr: usize) {
    INTERRUPT_REGISTRY.lock().remove(&addr);
}

#[cfg(test)]
mod tests {
    use super::valid_savepoint_name;

    #[test]
    fn savepoint_name_accepts_simple_identifiers() {
        assert_eq!(valid_savepoint_name("sp1").unwrap(), "sp1");
        assert_eq!(valid_savepoint_name("_outer_2").unwrap(), "_outer_2");
    }

    #[test]
    fn savepoint_name_rejects_injection() {
        assert!(valid_savepoint_name("sp1; DROP TABLE audit--").is_err());
        assert!(valid_savepoint_name("1bad").is_err());
        assert!(valid_savepoint_name("").is_err());
        assert!(valid_savepoint_name("a b").is_err());
    }
}
