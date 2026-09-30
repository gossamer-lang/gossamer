# `std::database::sql`

Status: experimental

Driver-pluggable SQL database access. No driver ships in the box; bring your own (Postgres, MySQL, SQLite, ...) by registering one at startup.

## Items

| Item | Signature | Description |
|---|---|---|
| `Driver` | `trait Driver` | Host-side database driver contract. Rust bindings register implementations before the Gossamer program starts. |
| `register_native` | `fn register_native(name: String, driver: database::sql::Driver) -> ()` | Registers a Gossamer-native driver under its canonical name. The driver must provide the SQL dispatch surface used by the runtime. |
| `drivers` | `fn drivers() -> Vec<String>` | Lists every currently-registered driver name. |
| `open` | `fn open(driver: String, url: String) -> Result<database::sql::Conn, errors::Error>` | Opens a database connection by driver name + URL. |
| `Conn` | `type Conn` | Open database connection. `prepare`, `execute`, `query`, `query_each`, `begin`, `begin_with`, `ping`, `execute_many`, `execute_ctx`, `query_ctx`, `interrupt`, `close` (closing sweeps any cursors still open on the connection). |
| `Tx` | `type Tx` | Active transaction. `commit`, `rollback`, `savepoint`, `release_savepoint`, `rollback_to_savepoint`, `execute`. |
| `Stmt` | `type Stmt` | Prepared statement. |
| `Rows` | `type Rows` | Result-set cursor. `next_row`, `columns`, `close` (idempotent). Advancing frees the previous Row; a full drain reclaims everything. For early exits, `defer rows.close()`. |
| `Row` | `type Row` | Current row inside a `Rows` walk; valid until the cursor advances or closes. Typed `get_i64`, `get_f64`, `get_bool`, `get_text`, `get_blob` plus `get_opt_*` and `is_null`. |
| `Value` | `type Value` | Bound or fetched value. Null / Bool / Int / Float / Text / Blob. |
| `IsolationLevel` | `type IsolationLevel` | Default / ReadUncommitted / ReadCommitted / RepeatableRead / Serializable. Passed to `Conn::begin_with`. |
| `Error` | `type Error` | Driver error. `Error::driver(driver, msg)` builds one; `Error::PoolExhausted` and `Error::Cancelled` are variants. |
| `Pool` | `type Pool` | Connection pool. `new`, `fill`, `get` (blocks up to `acquire_timeout`), `len`. Cheap to clone. |
| `PoolConfig` | `type PoolConfig` | Pool tuning: `min`, `max`, `idle_timeout`, `max_lifetime`, `acquire_timeout`, `statement_cache`. Fluent `with_*` builders. |
| `PooledConn` | `type PooledConn` | Connection checked out from a `Pool`; returned on drop. |
| `Select` | `type Select` | Fluent SELECT builder. `Select::new(table).columns([..]).where_eq(col, sql::Value::Int(...))...render() -> String`; `.params()` returns the bound parameters. Emits Postgres-style `$N` placeholders. |
| `migrate_up` | `fn migrate_up(conn: database::sql::Conn, dir: String) -> Result<i64, errors::Error>` | Applies pending forward-only schema migrations from a directory of `<version>_<slug>.sql` files. `migrate::up(&mut conn, dir)` is an equivalent namespaced spelling. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
