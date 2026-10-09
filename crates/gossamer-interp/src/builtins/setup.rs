// Built-in callables exposed to interpreted programs.


use std::cell::RefCell;
use std::fmt::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gossamer_ast::Ident;

use gossamer_std::env as env_std;
use gossamer_std::exec as exec_std;
use gossamer_std::fs as fs_std;
use gossamer_std::http as http_std;
use gossamer_std::json as json_std;
use gossamer_std::os as os_std;
use gossamer_std::slog as slog_std;
use gossamer_std::time as time_std;

use crate::value::{
    DenseMap, JsonInner, MapKey, NativeDispatch, RuntimeError, RuntimeResult, SmolStr, Value,
    dense_map_with_capacity,
};

/// The arguments the process was started with, and its program name.
///
/// A goroutine runs on whichever worker thread the scheduler hands it, and a
/// process has one argument list on every one of them, so these belong to the
/// process rather than to a thread. The compiled tiers read the same facts
/// from a process-global in the runtime's `c_abi`, which is what keeps
/// `env::args()` answering the same list on all three.
static PROGRAM_ARGS: parking_lot::RwLock<Vec<String>> = parking_lot::RwLock::new(Vec::new());
static PROGRAM_NAME: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);

/// The arguments `env::args()` answers with.
pub(crate) fn program_args() -> Vec<String> {
    PROGRAM_ARGS.read().clone()
}

/// The name `os::program_name()` answers with.
pub(crate) fn program_name() -> String {
    PROGRAM_NAME
        .read()
        .clone()
        .unwrap_or_else(|| String::from("gos"))
}

/// Overwrites the program-level argument list that `env::args()`
/// returns. Called by the CLI entrypoint before invoking `main`.
///
/// Wires both execution paths:
/// - The bytecode VM's `env::args()` builtin reads from the
///   `PROGRAM_ARGS` thread-local cell below.
/// - JIT-compiled `main` calls into the runtime's
///   `gos_rt_os_args`, which reads from a *different* static
///   inside `gossamer-runtime::c_abi`. Without this second wire,
///   benchmarks like `fasta` and `nbody` see an empty arg list
///   when their `main` JIT-compiles, fall back to the default N
///   (typically 1000), and silently produce undersized output.
///
/// The runtime side is wired by `crate::set_runtime_args` in
/// `lib.rs`, which is allowed to call into the FFI; this module
/// keeps `forbid(unsafe_code)`.
pub fn set_program_args(args: &[String]) {
    {
        let mut v = PROGRAM_ARGS.write();
        v.clear();
        v.extend_from_slice(args);
    }
    crate::set_runtime_args(args);
}

/// Sets the program name returned by `os::program_name()`. The CLI
/// calls this with the script path before invoking `main`.
pub fn set_program_name(name: &str) {
    *PROGRAM_NAME.write() = Some(String::from(name));
    crate::set_runtime_program_name(name);
}

// ------------------------------------------------------------------
// Mutable cell backing for `flag::Set` API.
//
// `Set::string` / `int` / `uint` / `bool` return a `__Cell` struct
// that `*` dereferences through [`resolve_cell`].

/// One flag's value, shared by its set, which `parse` writes through, and
/// the `__Cell` value the program reads it from.
pub(crate) type FlagCell = std::sync::Arc<parking_lot::Mutex<Value>>;

/// A flag set, shared by every copy of its handle.
pub(crate) type SharedSet = parking_lot::Mutex<SetState>;

// HashMap::new is not const-callable on our MSRV; these
// thread-locals construct on first access and stay registry-style
// for the life of the thread.
#[allow(
    clippy::missing_const_for_thread_local,
    reason = "HashMap::new with default RandomState is not const on MSRV"
)]
mod thread_local_registries {
    use super::RefCell;

    thread_local! {
        pub(crate) static STRUCT_UINT_FIELDS: RefCell<std::collections::HashMap<String, Vec<&'static str>>> =
            RefCell::new(std::collections::HashMap::new());
        pub(crate) static STRUCT_LAYOUTS: RefCell<std::collections::HashMap<String, Vec<&'static str>>> =
            RefCell::new(std::collections::HashMap::new());
        pub(crate) static VARIANT_OWNERS: RefCell<std::collections::HashMap<&'static str, &'static str>> =
            RefCell::new(std::collections::HashMap::new());
        pub(crate) static VARIANT_RANKS: RefCell<std::collections::HashMap<&'static str, i64>> =
            RefCell::new(std::collections::HashMap::new());
    }
}

pub(crate) use thread_local_registries::{
    STRUCT_LAYOUTS, STRUCT_UINT_FIELDS, VARIANT_OWNERS, VARIANT_RANKS,
};

/// Installs the variant-to-enum table that method dispatch consults to
/// resolve a call on an enum receiver.
///
/// A `Value::Variant` carries only the variant's own name, so without this
/// there is no way to tell which enum declared it, and a method call on an
/// enum value cannot be qualified by its type. Invoked by [`crate::Vm::load`]
/// before any program code runs.
///
/// A variant name declared by two different enums maps to neither: the
/// receiver alone cannot say which was meant, and guessing would reintroduce
/// exactly the misdispatch this table exists to prevent.
pub(crate) fn set_variant_owners(owners: &[(String, String)]) {
    let mut table: std::collections::HashMap<&'static str, &'static str> =
        std::collections::HashMap::new();
    let mut ambiguous: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    for (variant, owner) in owners {
        let variant = crate::value::intern_type_name(variant);
        let owner = crate::value::intern_type_name(owner);
        match table.get(variant) {
            Some(existing) if *existing != owner => {
                ambiguous.insert(variant);
            }
            _ => {
                table.insert(variant, owner);
            }
        }
    }
    for variant in ambiguous {
        table.remove(variant);
    }
    VARIANT_OWNERS.with(|cell| *cell.borrow_mut() = table);
}

/// The enum that declares `variant`, when exactly one does.
#[must_use]
pub(crate) fn variant_owner_of(variant: &str) -> Option<&'static str> {
    VARIANT_OWNERS.with(|cell| cell.borrow().get(variant).copied())
}

/// Installs the variant-to-rank table ordering compares enum values by.
///
/// A `Value::Variant` carries only its own name, so the position it was
/// declared at - which is what "lexicographic by variant rank" means - is
/// recorded here. A name two enums declare at different positions maps to
/// neither, exactly as [`set_variant_owners`] drops an ambiguous name.
pub(crate) fn set_variant_ranks(ranks: &[(String, i64)]) {
    let mut table: std::collections::HashMap<&'static str, i64> =
        std::collections::HashMap::new();
    let mut ambiguous: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    // `Option` and `Result` rank by the discriminant every tier stores:
    // `Some` and `Ok` are zero, `None` and `Err` one.
    for (name, rank) in [("Some", 0), ("Ok", 0), ("None", 1), ("Err", 1)] {
        table.insert(crate::value::intern_type_name(name), rank);
    }
    for (variant, rank) in ranks {
        let variant = crate::value::intern_type_name(variant);
        match table.get(variant) {
            Some(existing) if *existing != *rank => {
                ambiguous.insert(variant);
            }
            _ => {
                table.insert(variant, *rank);
            }
        }
    }
    for variant in ambiguous {
        table.remove(variant);
    }
    VARIANT_RANKS.with(|cell| *cell.borrow_mut() = table);
}

/// The declaration position of `variant`, when exactly one enum declares it.
#[must_use]
pub(crate) fn variant_rank_of(variant: &str) -> Option<i64> {
    VARIANT_RANKS.with(|cell| cell.borrow().get(variant).copied())
}

/// Installs the struct-field declaration-order table that
/// `__struct` consults when assembling a new `Value::Struct`.
/// Invoked by [`crate::Vm::load`] before any program code runs.
#[allow(
    clippy::implicit_hasher,
    reason = "stored verbatim in a RandomState-typed thread-local; generic hasher would force the thread-local to be generic too"
)]
pub fn set_struct_layouts(layouts: std::collections::HashMap<String, Vec<String>>) {
    // Intern each field name once, here at load time, so the per-construction
    // path in `builtin_struct_new` copies cached `&'static str` pointers with
    // no interning (and no global-intern lock) per struct value built.
    let interned: std::collections::HashMap<String, Vec<&'static str>> = layouts
        .into_iter()
        .map(|(name, fields)| {
            let interned_fields = fields
                .iter()
                .map(|f| crate::value::intern_type_name(f))
                .collect();
            (name, interned_fields)
        })
        .collect();
    STRUCT_LAYOUTS.with(|cell| *cell.borrow_mut() = interned);
}

/// Installs the per-struct list of fields declared `u64` / `usize`, which
/// `{:?}` reads so such a field renders as its own decimal rather than the
/// negative the same bits spell. Invoked by [`crate::Vm::load`] alongside
/// [`set_struct_layouts`].
#[allow(
    clippy::implicit_hasher,
    reason = "stored verbatim in a RandomState-typed thread-local; generic hasher would force the thread-local to be generic too"
)]
pub fn set_struct_uint_fields(fields: std::collections::HashMap<String, Vec<String>>) {
    let interned: std::collections::HashMap<String, Vec<&'static str>> = fields
        .into_iter()
        .map(|(name, names)| {
            let interned_names = names
                .iter()
                .map(|f| crate::value::intern_type_name(f))
                .collect();
            (name, interned_names)
        })
        .collect();
    STRUCT_UINT_FIELDS.with(|cell| *cell.borrow_mut() = interned);
}

/// Whether `field` of struct `name` was declared `u64` / `usize`.
pub(crate) fn struct_field_is_uint(name: &str, field: &str) -> bool {
    STRUCT_UINT_FIELDS.with(|cell| {
        cell.borrow()
            .get(name)
            .is_some_and(|fields| fields.contains(&field))
    })
}

#[derive(Debug, Clone)]
pub(crate) struct SetState {
    pub(crate) name: String,
    pub(crate) flag_order: Vec<String>,
    pub(crate) last_flag: Option<String>,
    pub(crate) flags: std::collections::HashMap<String, FlagDef>,
    /// Each flag's cell, by long name.
    pub(crate) cells: std::collections::HashMap<String, FlagCell>,
}

#[derive(Debug, Clone)]
pub(crate) struct FlagDef {
    pub(crate) short: Option<char>,
    pub(crate) kind: FlagKind,
    pub(crate) help: String,
    pub(crate) default: Value,
}

#[derive(Debug, Clone)]
pub(crate) enum FlagKind {
    String,
    Int,
    Uint,
    Float,
    Bool,
    Duration,
    StringList,
}

/// A new, empty flag set named `name`, as a `Set` handle.
pub(crate) fn new_set(name: String) -> Value {
    let state = SetState {
        name,
        flag_order: Vec::new(),
        last_flag: None,
        flags: std::collections::HashMap::new(),
        cells: std::collections::HashMap::new(),
    };
    crate::value::state_handle("Set", "__id", std::sync::Arc::new(SharedSet::new(state)))
}

/// The set a `Set` handle holds.
pub(crate) fn set_of(value: &Value) -> Option<std::sync::Arc<SharedSet>> {
    crate::value::handle_state(value, "Set", "__id")
}

/// Gives `state` a cell for `flag_name` holding `default`, and answers the
/// `__Cell` value that reads it.
pub(crate) fn make_cell(state: &mut SetState, flag_name: &str, default: Value) -> Value {
    let cell: FlagCell = std::sync::Arc::new(parking_lot::Mutex::new(default));
    state
        .cells
        .insert(flag_name.to_string(), std::sync::Arc::clone(&cell));
    Value::struct_(
        "__Cell",
        vec![
            ("__cell", Value::Opaque(crate::value::OpaqueState::new(cell))),
            (
                "__flag_name",
                Value::String(SmolStr::from(flag_name.to_string())),
            ),
        ],
    )
}

/// The current value of the flag a `__Cell` reads.
pub(crate) fn resolve_cell(cell: &crate::value::StructInner) -> Option<Value> {
    cell.fields.iter().find_map(|(ident, value)| match value {
        Value::Opaque(state) if *ident == "__cell" => state
            .downcast_ref::<FlagCell>()
            .map(|cell| cell.lock().clone()),
        _ => None,
    })
}

/// Installs stdlib-shaped built-ins (`println`, `print`, `eprintln`,
/// `eprint`, `format`, `panic`, ...) into the given global table,
/// plus a curated set of no-op stubs that let real-world example
/// programs at least reach the end of `main` without crashing.
pub(crate) fn install(globals: &mut Vec<(&'static str, Value)>) {
    install_io_builtins(globals);
    install_http_builtins(globals);
    install_variant_builtins(globals);
    install_module_builtins(globals);
    install_flag_builtins(globals);
    install_method_helpers(globals);
    install_concurrency_builtins(globals);
    install_regex_builtins(globals);
    crate::stdlib_builtins::install(globals);
    #[cfg(not(target_arch = "wasm32"))]
    globals.push(("serve", native("serve", native_http_serve)));
    #[cfg(not(target_arch = "wasm32"))]
    for key in ["Server::serve", "http::Server::serve"] {
        globals.push((
            key,
            native(key, crate::stdlib_builtins::http_server::native_http_server_serve),
        ));
    }
    install_leaf_module_aliases(globals);
}

/// Binds `<leaf>::<fn>` for every nested stdlib path registered only as
/// `<parent>::<leaf>::<fn>`. `use std::compress::gzip` brings the leaf
/// module into scope, so `gzip::encode(..)` is how the call is spelled -
/// the shape the compiled tiers already lower. A leaf spelling that two
/// modules would both claim is left unbound rather than resolved
/// arbitrarily, and an existing binding is never displaced.
fn install_leaf_module_aliases(globals: &mut Vec<(&'static str, Value)>) {
    use std::collections::{HashMap, HashSet};

    let taken: HashSet<&'static str> = globals.iter().map(|(name, _)| *name).collect();
    // A name registered more than once resolves to its last push, so the
    // alias has to carry that same binding.
    let mut effective: HashMap<&'static str, Value> = HashMap::new();
    for (name, value) in globals.iter() {
        effective.insert(*name, value.clone());
    }
    // Keyed by alias; the source path decides ambiguity, so the same
    // function registered twice still aliases.
    let mut candidates: HashMap<String, Option<(&'static str, Value)>> = HashMap::new();
    for (name, value) in globals.iter() {
        let segments: Vec<&str> = name.split("::").collect();
        if segments.len() < 3 {
            continue;
        }
        let leaf_module = segments[segments.len() - 2];
        // A type's associated function (`path::Path::new`) keeps its own
        // qualified spelling; only module segments alias.
        if leaf_module.chars().next().is_some_and(char::is_uppercase) {
            continue;
        }
        let alias = format!("{leaf_module}::{}", segments[segments.len() - 1]);
        if taken.contains(alias.as_str()) {
            continue;
        }
        match candidates.entry(alias) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let claimed_by_other = slot
                    .get()
                    .as_ref()
                    .is_some_and(|(source, _)| *source != *name);
                if claimed_by_other {
                    slot.insert(None);
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                let bound = effective.get(name).cloned().unwrap_or_else(|| value.clone());
                slot.insert(Some((*name, bound)));
            }
        }
    }
    for (alias, claimed) in candidates {
        if let Some((_, value)) = claimed {
            globals.push((Box::leak(alias.into_boxed_str()), value));
        }
    }
}

/// Returns the process-wide cached builtin table (built once on
/// first call). Each `Value::Builtin` / `Value::Native` payload is
/// behind an `Arc`, so cloning the entries is a refcount bump per
/// builtin - cheap enough that `Vm::new` can iterate the cached slice
/// when populating its globals map. The single shared cache avoids
/// rebuilding all ~330 entries per VM construction.
pub(crate) fn cached() -> &'static [(&'static str, Value)] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<(&'static str, Value)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut list = Vec::new();
        install(&mut list);
        list
    })
}

/// Every globally-registered builtin name (bare and qualified). The
/// resolver ships a checked-in table of the qualified stdlib paths so
/// `gos check` / the LSP can reject `module::nonexistent` calls before
/// runtime; a drift test compares that table against this list so it
/// never falls behind the runtime registry. Returns `&'static str`
/// since every key is a string literal or interned name.
#[must_use]
pub fn registered_names() -> Vec<&'static str> {
    cached().iter().map(|(name, _)| *name).collect()
}

/// Process-shared prelude `HashMap` of all built-in callables. Every
/// [`Vm`](crate::vm::Vm) `Arc::clone`s this map and consults it on
/// lookup miss against its own per-Vm overlay; no Vm copies the
/// prelude into its own storage. Late-registered binding natives
/// stay out of the prelude - they can land after Vm construction
/// and ride the per-Vm overlay instead.
pub(crate) fn prelude_globals()
-> std::sync::Arc<rustc_hash::FxHashMap<&'static str, crate::vm::Global>> {
    use std::sync::OnceLock;
    static PRELUDE: OnceLock<
        std::sync::Arc<rustc_hash::FxHashMap<&'static str, crate::vm::Global>>,
    > = OnceLock::new();
    std::sync::Arc::clone(PRELUDE.get_or_init(|| {
        let mut map = rustc_hash::FxHashMap::default();
        for (name, value) in cached() {
            map.insert(*name, crate::vm::Global::Value(value.clone()));
        }
        map.shrink_to_fit();
        std::sync::Arc::new(map)
    }))
}

