#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Static manifest of every registered stdlib module.
//! Each stdlib milestone extends this table with
//! the modules it adds. Entries are listed in phase-introduction order
//! so a `gos doc` walk renders modules in the same sequence as the
//! implementation plan.

#![forbid(unsafe_code)]
use crate::registry::{StdItem, StdItemKind, StdModule};

use super::*;

pub const DATABASE_SQL: StdModule = StdModule {
    path: "std::database::sql",
    summary: "Driver-pluggable SQL database access. No driver ships in the box; bring your own (Postgres, MySQL, SQLite, ...) by registering one at startup.",
    items: &[
        StdItem {
            name: "Driver",
            kind: StdItemKind::Trait,
            doc: "Host-side database driver contract. Rust bindings register implementations before the Gossamer program starts.",
        },
        StdItem {
            name: "register_native",
            kind: StdItemKind::Function,
            doc: "Registers a Gossamer-native driver under its canonical name. The driver must provide the SQL dispatch surface used by the runtime.",
        },
        StdItem {
            name: "drivers",
            kind: StdItemKind::Function,
            doc: "Lists every currently-registered driver name.",
        },
        StdItem {
            name: "open",
            kind: StdItemKind::Function,
            doc: "Opens a database connection by driver name + URL.",
        },
        StdItem {
            name: "Conn",
            kind: StdItemKind::Type,
            doc: "Open database connection. `prepare`, `execute`, `query`, `query_each`, `begin`, `begin_with`, `ping`, `execute_many`, `execute_ctx`, `query_ctx`, `interrupt`, `close` (closing sweeps any cursors still open on the connection).",
        },
        StdItem {
            name: "Tx",
            kind: StdItemKind::Type,
            doc: "Active transaction. `commit`, `rollback`, `savepoint`, `release_savepoint`, `rollback_to_savepoint`, `execute`.",
        },
        StdItem {
            name: "Stmt",
            kind: StdItemKind::Type,
            doc: "Prepared statement.",
        },
        StdItem {
            name: "Rows",
            kind: StdItemKind::Type,
            doc: "Result-set cursor. `next_row`, `columns`, `close` (idempotent). Advancing frees the previous Row; a full drain reclaims everything. For early exits, `defer rows.close()`.",
        },
        StdItem {
            name: "Row",
            kind: StdItemKind::Type,
            doc: "Current row inside a `Rows` walk; valid until the cursor advances or closes. Typed `get_i64`, `get_f64`, `get_bool`, `get_text`, `get_blob` plus `get_opt_*` and `is_null`.",
        },
        StdItem {
            name: "Value",
            kind: StdItemKind::Type,
            doc: "Bound or fetched value. Null / Bool / Int / Float / Text / Blob.",
        },
        StdItem {
            name: "IsolationLevel",
            kind: StdItemKind::Type,
            doc: "Default / ReadUncommitted / ReadCommitted / RepeatableRead / Serializable. Passed to `Conn::begin_with`.",
        },
        StdItem {
            name: "Error",
            kind: StdItemKind::Type,
            doc: "Driver error. `Error::driver(driver, msg)` builds one; `Error::PoolExhausted` and `Error::Cancelled` are variants.",
        },
        StdItem {
            name: "Pool",
            kind: StdItemKind::Type,
            doc: "Connection pool. `new`, `fill`, `get` (blocks up to `acquire_timeout`), `len`. Cheap to clone.",
        },
        StdItem {
            name: "PoolConfig",
            kind: StdItemKind::Type,
            doc: "Pool tuning: `min`, `max`, `idle_timeout`, `max_lifetime`, `acquire_timeout`, `statement_cache`. Fluent `with_*` builders.",
        },
        StdItem {
            name: "PooledConn",
            kind: StdItemKind::Type,
            doc: "Connection checked out from a `Pool`; returned on drop.",
        },
        StdItem {
            name: "Select",
            kind: StdItemKind::Type,
            doc: "Fluent SELECT builder. `Select::new(table).columns([..]).where_eq(col, sql::Value::Int(...))...render() -> String`; `.params()` returns the bound parameters. Emits Postgres-style `$N` placeholders.",
        },
        StdItem {
            name: "migrate_up",
            kind: StdItemKind::Function,
            doc: "Applies pending forward-only schema migrations from a directory of `<version>_<slug>.sql` files. `migrate::up(&mut conn, dir)` is an equivalent namespaced spelling.",
        },
    ],
};

pub const COLLECTIONS: StdModule = StdModule {
    path: "std::collections",
    summary: "Built-in container types.",
    items: &[
        StdItem {
            name: "Vec",
            kind: StdItemKind::Type,
            doc: "Growable contiguous sequence. Vec literals use `[a, b]` and empty Vec literals use `[]`.",
        },
        StdItem {
            name: "Deque",
            kind: StdItemKind::Type,
            doc: "Double-ended queue with explicit front/back methods; use `Deque::new()` or `Deque::from([a, b])`.",
        },
        StdItem {
            name: "Queue",
            kind: StdItemKind::Type,
            doc: "FIFO-only queue; use `Queue::new()` or `Queue::from([a, b])`.",
        },
        StdItem {
            name: "Stack",
            kind: StdItemKind::Type,
            doc: "LIFO-only stack; use `Stack::new()` or `Stack::from([a, b])`.",
        },
        StdItem {
            name: "MaxHeap",
            kind: StdItemKind::Type,
            doc: "Max-priority heap; use `MaxHeap::new()` or `MaxHeap::from([a, b])`.",
        },
        StdItem {
            name: "MinHeap",
            kind: StdItemKind::Type,
            doc: "Min-priority heap; use `MinHeap::new()` or `MinHeap::from([a, b])`.",
        },
        StdItem {
            name: "Map",
            kind: StdItemKind::Type,
            doc: "Key-value map backed by the swiss-table layout; literals use `{key: value}` and empty map literals use `{}`.",
        },
        StdItem {
            name: "BTreeMap",
            kind: StdItemKind::Type,
            doc: "Ordered key-value map backed by BTreeMap.",
        },
        StdItem {
            name: "Set",
            kind: StdItemKind::Type,
            doc: "Unique-value set backed by a hash table; literals use `#{a, b}` and empty set literals use `#{}`.",
        },
        StdItem {
            name: "BTreeSet",
            kind: StdItemKind::Type,
            doc: "Ordered unique-value set backed by BTreeSet.",
        },
        StdItem {
            name: "IndexMap",
            kind: StdItemKind::Type,
            doc: "Key-value map that keeps insertion order: `new`, `insert` (answers the replaced value), `get`, `contains_key`, `remove`, `len`, `is_empty`, and `keys` / `values` / `entries` in the order keys were first inserted.",
        },
        StdItem {
            name: "BitSet",
            kind: StdItemKind::Type,
            doc: "Set of non-negative integers stored one bit each: `new`, `insert` / `remove` (answer whether the set changed), `contains`, `len`, `is_empty`, `clear`, and `to_vec` in ascending order.",
        },
    ],
};
