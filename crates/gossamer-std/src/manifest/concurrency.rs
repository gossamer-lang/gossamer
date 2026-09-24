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

pub const CONTEXT: StdModule = StdModule {
    path: "std::context",
    summary: "Request-scoped cancellation, deadlines, and timeouts.",
    items: &[StdItem {
        name: "Context",
        kind: StdItemKind::Type,
        doc: "Cancellation-aware context handle.",
    }],
};

pub const THREAD: StdModule = StdModule {
    path: "std::thread",
    summary: "OS-thread scheduling hints and CPU introspection; user concurrency uses goroutines, not thread spawning.",
    items: &[
        StdItem {
            name: "yield_now",
            kind: StdItemKind::Function,
            doc: "Hints to the scheduler to switch to another runnable thread.",
        },
        StdItem {
            name: "num_cpus",
            kind: StdItemKind::Function,
            doc: "Returns the number of logical CPUs available.",
        },
    ],
};

pub const SYNC: StdModule = StdModule {
    path: "std::sync",
    summary: "Synchronisation primitives beyond channels.",
    items: &[
        StdItem {
            name: "Channel",
            kind: StdItemKind::Type,
            doc: "Bidirectional channel handle.",
        },
        StdItem {
            name: "Sender",
            kind: StdItemKind::Type,
            doc: "The sending end of a channel: `send(v)`, `close()`.",
        },
        StdItem {
            name: "Receiver",
            kind: StdItemKind::Type,
            doc: "The receiving end of a channel: `recv() -> Option<T>`, `None` once it is closed and drained.",
        },
        StdItem {
            name: "Mutex",
            kind: StdItemKind::Type,
            doc: "Mutual-exclusion lock: `lock()` / `unlock()` around the code it guards.",
        },
        StdItem {
            name: "RwLock",
            kind: StdItemKind::Type,
            doc: "Reader-writer lock guarding an `i64`: `read`, `write`, `with_read`, `with_write`.",
        },
        StdItem {
            name: "Shared",
            kind: StdItemKind::Type,
            doc: "A value several goroutines reach, every read and write taken under one lock. Build with `Shared::new(value)`; read with `get` or `with`, write with `set`, and read-modify-write with `update`, which holds the lock across the callback.",
        },
        StdItem {
            name: "Once",
            kind: StdItemKind::Type,
            doc: "One-shot initialisation latch.",
        },
        StdItem {
            name: "WaitGroup",
            kind: StdItemKind::Type,
            doc: "Counts goroutines and waits for them to finish.",
        },
        StdItem {
            name: "Barrier",
            kind: StdItemKind::Type,
            doc: "Synchronisation barrier across goroutines.",
        },
        StdItem {
            name: "AtomicI64",
            kind: StdItemKind::Type,
            doc: "Atomic 64-bit signed integer.",
        },
        StdItem {
            name: "AtomicI32",
            kind: StdItemKind::Type,
            doc: "Atomic 32-bit signed integer.",
        },
        StdItem {
            name: "AtomicU64",
            kind: StdItemKind::Type,
            doc: "Atomic 64-bit unsigned integer.",
        },
        StdItem {
            name: "AtomicBool",
            kind: StdItemKind::Type,
            doc: "Atomic boolean.",
        },
        StdItem {
            name: "Map",
            kind: StdItemKind::Type,
            doc: "Concurrent key/value map.",
        },
        StdItem {
            name: "channel",
            kind: StdItemKind::Function,
            doc: "Creates a typed channel, returning (Sender, Receiver).",
        },
        StdItem {
            name: "channel_unbounded",
            kind: StdItemKind::Function,
            doc: "Creates an explicit unbounded typed channel, returning (Sender, Receiver).",
        },
        StdItem {
            name: "shield",
            kind: StdItemKind::Function,
            doc: "Runs a callable in a cohort exempt from cancellation and answers its value: a cancel landing on an enclosing cohort does not reach the work inside. The shielded region is still counted and still drained.",
        },
        StdItem {
            name: "with_timeout",
            kind: StdItemKind::Function,
            doc: "Runs a callable on a child of a cohort bounded by a millisecond deadline, answering `Ok(value)` when it finished inside the bound and `Err` naming the bound when it did not. Cancellation is cooperative: a child that reaches no cancellation point runs on and is named in the cohort's drain report.",
        },
    ],
};
