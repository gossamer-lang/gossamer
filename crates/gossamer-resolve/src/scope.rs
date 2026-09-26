//! Lexical-scope tree used by the resolver.
//! A [`ScopeStack`] is a LIFO stack of named bindings organised into two
//! namespaces (type and value). Items at module scope are registered up
//! front so that forward references work; nested block, function, and
//! pattern scopes shadow the module scope and each other.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use crate::def_id::{DefId, DefKind};
use crate::resolutions::{FloatWidth, IntWidth, PrimitiveTy, Resolution};
use gossamer_ast::NodeId;

/// Sentinel [`NodeId`] used for prelude-provided names that have no
/// corresponding `use` declaration in the source file.
pub(crate) const PRELUDE_SENTINEL: NodeId = NodeId::DUMMY;

/// True for bindings inserted by [`ScopeStack::with_prelude`].
/// Those are the only entries `insert_type` / `insert_value`
/// silently overwrite - a non-prelude collision stays an error.
fn is_prelude_binding(b: &Binding) -> bool {
    matches!(
        b.resolution,
        Resolution::Import { use_id } if use_id == PRELUDE_SENTINEL
    )
}

/// `true` for the primitive type names that are not part of the language's
/// written type vocabulary (`Unit`, `Never`): a program's own item of that
/// name takes the slot, the way it takes a prelude name's.
fn is_shadowable_primitive(b: &Binding) -> bool {
    matches!(
        b.resolution,
        Resolution::Primitive(PrimitiveTy::Unit | PrimitiveTy::Never)
    )
}

/// `true` when `b` came from a `use`. A definition collected afterwards
/// takes the slot: the import names that very item, so the definition is
/// what the name should resolve to.
fn is_import_binding(b: &Binding) -> bool {
    matches!(b.resolution, Resolution::Import { .. })
}

/// A single entry in the value or type namespace.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binding {
    /// Resolved target of this name.
    pub resolution: Resolution,
}

impl Binding {
    pub(crate) const fn def(def: DefId, kind: DefKind) -> Self {
        Self {
            resolution: Resolution::Def { def, kind },
        }
    }

    pub(crate) const fn local(node: NodeId) -> Self {
        Self {
            resolution: Resolution::Local(node),
        }
    }

    pub(crate) const fn primitive(prim: PrimitiveTy) -> Self {
        Self {
            resolution: Resolution::Primitive(prim),
        }
    }

    pub(crate) const fn import(use_id: NodeId) -> Self {
        Self {
            resolution: Resolution::Import { use_id },
        }
    }
}

/// One layer in the [`ScopeStack`].
#[derive(Debug, Default, Clone)]
pub(crate) struct Scope {
    /// Names live in the type namespace (struct/enum/trait/alias/module/
    /// type-parameter/primitive).
    types: HashMap<Box<str>, Binding>,
    /// Names live in the value namespace (fn/const/static/variant/local
    /// binding).
    values: HashMap<Box<str>, Binding>,
}

impl Scope {
    /// Returns `false` when the slot is already occupied by a
    /// non-prelude binding (a real duplicate). Prelude bindings -
    /// inserted by `with_prelude` with `PRELUDE_SENTINEL` - get
    /// silently overwritten so user definitions can shadow them
    /// (e.g. `fn clamp(...)` overriding the new prelude `clamp`).
    pub(crate) fn insert_type(&mut self, name: &str, binding: Binding) -> bool {
        if let Some(existing) = self.types.get(name) {
            if !is_prelude_binding(existing)
                && !is_import_binding(existing)
                && !is_shadowable_primitive(existing)
            {
                return false;
            }
        }
        self.types.insert(Box::from(name), binding);
        true
    }

    pub(crate) fn insert_value(&mut self, name: &str, binding: Binding) -> bool {
        if let Some(existing) = self.values.get(name) {
            if !is_prelude_binding(existing) && !is_import_binding(existing) {
                return false;
            }
        }
        self.values.insert(Box::from(name), binding);
        true
    }

    pub(crate) fn shadow_value(&mut self, name: &str, binding: Binding) {
        self.values.insert(Box::from(name), binding);
    }

    pub(crate) fn lookup_type(&self, name: &str) -> Option<Binding> {
        self.types.get(name).copied()
    }

    pub(crate) fn lookup_value(&self, name: &str) -> Option<Binding> {
        self.values.get(name).copied()
    }

    /// Names bound in this layer's value namespace.
    pub(crate) fn value_names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(AsRef::as_ref)
    }

    /// Names bound in this layer's type namespace.
    pub(crate) fn type_names(&self) -> impl Iterator<Item = &str> {
        self.types.keys().map(AsRef::as_ref)
    }
}

/// Stack of lexical scopes visible at a given program point.
#[derive(Debug, Default, Clone)]
pub(crate) struct ScopeStack {
    layers: Vec<Scope>,
}

impl ScopeStack {
    /// Builds a stack seeded with a single module-level scope containing
    /// every primitive type name and the stdlib prelude entries that are
    /// always in scope (Result, Option, their variants, `str`, ...).
    pub(crate) fn with_prelude() -> Self {
        let mut root = Scope::default();
        for (name, prim) in PRIMITIVE_TYPES {
            root.insert_type(name, Binding::primitive(*prim));
        }
        for name in PRELUDE_TYPES {
            root.insert_type(name, Binding::import(PRELUDE_SENTINEL));
        }
        for name in PRELUDE_VALUES {
            root.insert_value(name, Binding::import(PRELUDE_SENTINEL));
        }
        Self { layers: vec![root] }
    }

    /// Pushes a fresh empty scope.
    pub(crate) fn push(&mut self) {
        self.layers.push(Scope::default());
    }

    /// Pushes an existing scope layer (an inline module's own item
    /// bindings, collected up front) so lookups inside the module's
    /// body see its items before the flat root scope.
    pub(crate) fn push_scope(&mut self, scope: Scope) {
        self.layers.push(scope);
    }

    /// Pops the top scope. Panics in debug builds if the stack is empty
    /// (callers must balance push/pop).
    pub(crate) fn pop(&mut self) {
        debug_assert!(self.layers.len() > 1, "cannot pop the module scope");
        self.layers.pop();
    }

    /// Returns a mutable handle to the top-of-stack scope for inserting
    /// new bindings.
    pub(crate) fn top_mut(&mut self) -> &mut Scope {
        let idx = self.layers.len() - 1;
        &mut self.layers[idx]
    }

    /// Returns a handle to the innermost module-level scope (the bottom
    /// of the stack). Used when registering top-level items up front.
    pub(crate) fn module_mut(&mut self) -> &mut Scope {
        &mut self.layers[0]
    }

    /// Read-only handle to the innermost module-level scope.
    pub(crate) fn module_ref(&self) -> &Scope {
        &self.layers[0]
    }

    /// Searches from innermost to outermost for a type-namespace binding.
    pub(crate) fn lookup_type(&self, name: &str) -> Option<Binding> {
        for scope in self.layers.iter().rev() {
            if let Some(binding) = scope.lookup_type(name) {
                return Some(binding);
            }
        }
        None
    }

    /// Searches from innermost to outermost for a value-namespace binding.
    pub(crate) fn lookup_value(&self, name: &str) -> Option<Binding> {
        for scope in self.layers.iter().rev() {
            if let Some(binding) = scope.lookup_value(name) {
                return Some(binding);
            }
        }
        None
    }

    /// Every name visible at this point, innermost scope first, across both
    /// namespaces. Locals, parameters, and closure bindings live only here,
    /// so this is the candidate set a "did you mean" must draw on to reach
    /// the names a user most often mistypes.
    pub(crate) fn visible_names(&self) -> impl Iterator<Item = &str> {
        self.layers
            .iter()
            .rev()
            .flat_map(|scope| scope.value_names().chain(scope.type_names()))
    }
}

pub(crate) const PRELUDE_TYPES: &[&str] = &[
    "str", "Result", "Option", "Vec", "Map", "Set", "BTreeSet", "BTreeMap", "Deque", "Queue",
    "Stack", "MaxHeap", "MinHeap", "Iterator", "Box", "Arc", "Rc", "Weak", "Range", "Sender",
    "Receiver",
    // Goroutine fan-out buffers: a heap-allocated `[i64]` and a
    // `[u8]` whose elements take lock-free cross-goroutine writes.
    // These are the prelude's own spelling - no module exports them,
    // so `I64Vec::new` / `U8Vec::new` is the whole surface. The
    // `std::sync` primitives are imported (`use std::sync::Mutex`)
    // rather than listed here, so a bare `Mutex` reports the import
    // it is missing instead of binding a name with no owning module.
    "I64Vec", "U8Vec",
    // The open dynamic value. No module exports it - a decoder builds one
    // with `DynValue::int(..)` and reads it back by kind and arm name - so
    // the prelude is where its name lives.
    "DynValue",
    // Fixed-width lane vectors and their `bool` masks, typed by the checker.
    "Simd", "Mask",
];

const PRELUDE_VALUES: &[&str] = &[
    "Ok",
    "Err",
    "Some",
    "None",
    "print",
    "println",
    "eprint",
    "eprintln",
    "format",
    "panic",
    "assert",
    "assert_eq",
    "todo",
    // 0.7.0 scalar QoL helpers. Routed by `builtin_min_dispatch`
    // / `builtin_max_dispatch` / `builtin_clamp` in the interp;
    // compiled tier resolves them through the same bare-name
    // path. Single-Vec callers still get the iter::min / iter::max
    // fall-through inside the dispatcher.
    "min",
    "max",
    "clamp",
    // Goroutine join handle: `spawn(f)` runs `f` on a goroutine and
    // returns a handle whose `.join()` blocks for the outcome. It is the
    // one way to start one, so a declaration under the name reports
    // GR0020 rather than shadowing it.
    "spawn",
    // `channel()` - typed goroutine channel constructor. Prelude so the
    // injected `time::after` / `time::tick` timer wrappers can build a
    // channel without the user importing `std::sync::channel`.
    "channel",
    // Compile-time intrinsics referenced by macro expansion
    // (`println!` → `println(__concat(…))`) and struct-literal
    // lowering (`Path { f: v }` → `__struct("Path", "f", v)`).
    // Both are resolved in the interpreter/codegen, not by user
    // code, but the resolver still traverses the expanded form.
    "__concat",
    "__debug",
    "__fmt_prec",
    // Format-spec intrinsics emitted by `{:spec}` macro expansion:
    // `__fmt_pad` (width/align/fill), `__fmt_radix` (`{:x}`/`{:b}`/`{:o}`),
    // `__fmt_upper` (`{:X}`). Lowered through the stdlib free-call table.
    "__fmt_pad",
    "__fmt_radix",
    "__fmt_upper",
    "__repl_discard",
    "__gos_debug_quote",
    "__gos_f32_display",
    "__gos_f32_debug",
    "__gos_dyn_display",
    "__gos_dyn_debug",
    "__struct",
    // LCG jump-ahead: routes to `gos_rt_lcg_jump`. Callable
    // from user code as `lcg_jump(state, ia, ic, im, n)`.
    // Used by multi-threaded fasta to seed each worker.
    "lcg_jump",
    "gos_rt_lcg_jump",
    // Leaf intrinsics for the injected `encoding::pem` real-struct
    // wrappers (gossamer-parse autoderive). Resolved here so the
    // synthesized wrapper bodies type-check; user code never names
    // them directly.
    "__gos_pem_decode_raw",
    "__gos_pem_decode_all_raw",
    "__gos_pem_encode_raw",
    "__gos_x509_parse_pem_raw",
    "__gos_fs_metadata_raw",
    "__gos_fs_read_dir_raw",
    "__gos_fs_walk_dir_raw",
    "__gos_process_run_raw",
    "__gos_process_run_in_raw",
    "__gos_process_pipeline_run_raw",
    "__gos_time_location_raw",
    "__gos_time_fixed_location_raw",
    "__gos_time_civil_raw",
    "__gos_time_resolve_raw",
    "__gos_time_format_in_raw",
    "__gos_time_add_date_raw",
    "__gos_tar_read_raw",
    "__gos_zip_read_raw",
    // Leaf intrinsics for the injected `database::sql` real-struct
    // wrappers (gossamer-parse autoderive).
    "__gos_sql_open_raw",
    "__gos_sql_last_error_raw",
    "__gos_sql_drivers_raw",
    "__gos_sql_params_new_raw",
    "__gos_sql_params_push_null_raw",
    "__gos_sql_params_push_bool_raw",
    "__gos_sql_params_push_int_raw",
    "__gos_sql_params_push_float_raw",
    "__gos_sql_params_push_text_raw",
    "__gos_sql_params_push_blob_raw",
    "__gos_sql_conn_execute_raw",
    "__gos_sql_conn_query_raw",
    "__gos_sql_conn_begin_raw",
    "__gos_sql_conn_begin_with_raw",
    "__gos_sql_conn_ping_raw",
    "__gos_sql_conn_set_busy_timeout_raw",
    "__gos_sql_conn_interrupt_raw",
    "__gos_sql_conn_close_raw",
    "__gos_sql_rows_next_row_raw",
    "__gos_sql_rows_close_raw",
    "__gos_sql_rows_columns_raw",
    "__gos_sql_row_kind_raw",
    "__gos_sql_row_get_i64_raw",
    "__gos_sql_row_get_f64_raw",
    "__gos_sql_row_get_bool_raw",
    "__gos_sql_row_get_text_raw",
    "__gos_sql_row_get_blob_raw",
    "__gos_sql_row_width_raw",
    "__gos_sql_tx_commit_raw",
    "__gos_sql_tx_rollback_raw",
    "__gos_sql_tx_execute_raw",
    "__gos_sql_tx_savepoint_raw",
    "__gos_sql_tx_release_savepoint_raw",
    "__gos_sql_tx_rollback_to_savepoint_raw",
    "__gos_sql_tx_execute_params_raw",
    "__gos_sql_tx_query_params_raw",
    "__gos_sql_conn_prepare_raw",
    "__gos_sql_stmt_execute_raw",
    "__gos_sql_stmt_query_raw",
    "__gos_sql_stmt_close_raw",
    "__gos_sql_conn_copy_in_raw",
    "__gos_sql_conn_copy_out_run_raw",
    "__gos_sql_conn_copy_out_take_raw",
    "__gos_sql_conn_listen_raw",
    "__gos_sql_conn_unlisten_raw",
    "__gos_sql_conn_poll_notification_raw",
    "__gos_sql_notification_channel_raw",
    "__gos_sql_notification_payload_raw",
    "__gos_sql_notification_pid_raw",
    "__gos_sql_pool_new_raw",
    "__gos_sql_pool_get_raw",
    "__gos_sql_pool_live_raw",
    "__gos_sql_pool_idle_raw",
    "__gos_sql_pool_close_idle_raw",
    "__gos_sql_migrate_up_raw",
    // Gossamer-native driver dispatch: the `register_native` leaf is
    // custom-lowered (captures the driver env + dispatch fn-address);
    // the rest are the side-channel helpers a `.gos` driver calls.
    "__gos_sql_register_native",
    "__gos_sql_native_url",
    "__gos_sql_native_sql",
    "__gos_sql_native_parent",
    "__gos_sql_native_out_handle",
    "__gos_sql_native_iso",
    "__gos_sql_native_timeout",
    "__gos_sql_native_channel",
    "__gos_sql_native_param_count",
    "__gos_sql_native_param",
    "__gos_sql_native_data",
    "__gos_sql_native_push_column",
    "__gos_sql_native_push_value",
    "__gos_sql_native_row_ready",
    "__gos_sql_native_set_error",
    "__gos_sql_native_emit_bytes",
    "__gos_sql_native_set_notification",
    "__gos_sql_native_set_handle",
    "__gos_sql_native_handle",
    "__gos_sql_native_value_null",
    "__gos_sql_native_value_bool",
    "__gos_sql_native_value_int",
    "__gos_sql_native_value_float",
    "__gos_sql_native_value_text",
    "__gos_sql_native_value_blob",
    "__gos_sql_native_value_kind",
    "__gos_sql_native_value_int_of",
    "__gos_sql_native_value_float_of",
    "__gos_sql_native_value_text_of",
    "__gos_sql_native_value_blob_of",
];

/// Whether `name` is a prelude value: a callable or constructor every
/// program can reach without an import.
#[must_use]
pub fn is_prelude_value(name: &str) -> bool {
    PRELUDE_VALUES.contains(&name)
}

pub(crate) fn prelude_suggestion_names() -> impl Iterator<Item = &'static str> {
    PRIMITIVE_TYPES
        .iter()
        .map(|(name, _)| *name)
        .chain(PRELUDE_TYPES.iter().copied())
        .chain(PRELUDE_VALUES.iter().copied())
}

const PRIMITIVE_TYPES: &[(&str, PrimitiveTy)] = &[
    ("bool", PrimitiveTy::Bool),
    ("char", PrimitiveTy::Char),
    ("String", PrimitiveTy::String),
    ("i8", PrimitiveTy::Int(IntWidth::W8)),
    ("i16", PrimitiveTy::Int(IntWidth::W16)),
    ("i32", PrimitiveTy::Int(IntWidth::W32)),
    ("i64", PrimitiveTy::Int(IntWidth::W64)),
    ("i128", PrimitiveTy::Int(IntWidth::W128)),
    ("isize", PrimitiveTy::Int(IntWidth::Size)),
    ("u8", PrimitiveTy::UInt(IntWidth::W8)),
    ("u16", PrimitiveTy::UInt(IntWidth::W16)),
    ("u32", PrimitiveTy::UInt(IntWidth::W32)),
    ("u64", PrimitiveTy::UInt(IntWidth::W64)),
    ("u128", PrimitiveTy::UInt(IntWidth::W128)),
    ("usize", PrimitiveTy::UInt(IntWidth::Size)),
    ("f32", PrimitiveTy::Float(FloatWidth::W32)),
    ("f64", PrimitiveTy::Float(FloatWidth::W64)),
    ("Never", PrimitiveTy::Never),
    ("Unit", PrimitiveTy::Unit),
];
