//! Bytecode VM and interpreter support for Gossamer.
//!
//! The production execution path compiles HIR from [`gossamer_hir`] to
//! validated bytecode and executes it in [`Vm`]. Legacy direct-evaluation
//! helpers remain only for focused compatibility tests; this crate is not a
//! tree-walking runtime.
//! Values use reference-counted heap aggregates, mirroring the GC
//! semantics described in SPEC §3.3 even though the real garbage
//! collector does not land.

// Crate-level note: `unsafe` is forbidden in every module
// except `vm.rs`. The VM's inner dispatch loop uses
// `get_unchecked` for register / const-pool access; every
// index is bounded by the `FnChunk`'s compile-time counts.
// See `vm.rs` for the full invariant list.
#![deny(unsafe_code)]

pub mod builtin_effects;
mod builtins;
mod bytecode;
mod cast;
mod compile;
mod comptime;
mod comptime_gate;
pub mod external_natives;
mod flag_set_builtins;
#[cfg(feature = "fuel")]
pub mod fuel;
#[cfg(not(target_arch = "wasm32"))]
mod http_client_builtins;
mod jit_call;
// No-op JIT backend used in place of `gossamer-codegen-cranelift` on
// wasm32, where Cranelift is unavailable.
#[cfg(target_arch = "wasm32")]
mod jit_stub;
pub mod profile;
mod regex_builtins;
mod stdlib_builtins;
// Bytecode is validated once when a chunk is installed. The dispatch loop
// deliberately relies on validated indices for its unchecked fast paths, so
// release builds must retain this boundary too.
mod validate;
pub mod value;
mod vm;

pub use builtins::coverage;
pub use builtins::{
    TestTally, registered_names, reset_test_tally, set_assertion_location, set_http_max_requests,
    set_program_args, set_program_name, set_stderr_writer, set_stdout_writer, set_struct_layouts,
    set_struct_uint_fields, take_test_tally,
};
pub use jit_call::{
    force_jit_disabled as set_jit_disabled, force_jit_enable as set_jit_enabled,
    jit_force_disabled_state,
};
pub use stdlib_builtins::iter::reset_lazy_iterator_state;

/// Pushes `args` into the runtime's `ARGS_PTR` so JIT-compiled
/// `gos_rt_os_args` reads see the same list `os::args()` returns
/// in the bytecode VM. Process-lifetime ownership of the
/// `CString`s lives in a `Mutex` here; `*const c_char` doesn't
/// implement `Send` so we wrap the pointer table in a
/// `repr(transparent)` newtype that we explicitly mark `Send`.
///
/// Called by [`builtins::set_program_args`] which is the only
/// public entry point for both the bytecode and JIT arg lists.
#[doc(hidden)]
#[allow(
    clippy::similar_names,
    reason = "argc/argv are the Unix-standard names for the C main signature"
)]
pub(crate) fn set_runtime_args(args: &[String]) {
    use std::ffi::CString;
    use std::os::raw::c_char;
    use std::sync::Mutex;

    /// `*const c_char` is not `Send`, so we wrap it. The values
    /// are read-only after `gos_rt_set_args` has copied them into
    /// its atomics; we never share the inner pointers across
    /// threads at the Rust type level.
    #[repr(transparent)]
    struct ArgPtr(*const c_char);
    // SAFETY: the pointers are owned by `CString`s held in the
    // same Mutex; nothing mutates them, and the runtime side
    // accesses them via SeqCst-ordered atomics.
    #[allow(unsafe_code)]
    unsafe impl Send for ArgPtr {}

    static OWNED: Mutex<(Vec<CString>, Vec<ArgPtr>)> = Mutex::new((Vec::new(), Vec::new()));

    let mut owned = OWNED.lock().expect("runtime-args mutex poisoned");
    let mut all = vec![CString::new("gos").expect("static label")];
    for a in args {
        let cstr = CString::new(a.as_bytes()).unwrap_or_else(|_| {
            let cleaned: Vec<u8> = a.bytes().filter(|b| *b != 0).collect();
            CString::new(cleaned).expect("cleaned bytes have no NUL")
        });
        all.push(cstr);
    }
    let ptrs: Vec<ArgPtr> = all.iter().map(|c| ArgPtr(c.as_ptr())).collect();
    let argc = i32::try_from(ptrs.len()).unwrap_or(1);
    let raw_argv = ptrs.as_ptr().cast::<*const c_char>();
    // SAFETY: `gos_rt_set_args` is `unsafe extern "C"` purely for
    // FFI uniformity; its preconditions are (argc >= 0) and
    // (argv addresses argc consecutive valid c-strings or is
    // NULL). Both hold here: `all` owns every string, `ptrs`
    // captures their `as_ptr()`, and the storage outlives the
    // runtime's read because `OWNED` is a `'static` Mutex.
    #[allow(unsafe_code)]
    unsafe {
        gossamer_runtime::c_abi::gos_rt_set_args(argc, raw_argv);
    }
    // Replace previous batch *after* the runtime has copied the
    // pointer values; dropping the prior CStrings now is safe.
    owned.0 = all;
    owned.1 = ptrs;
}

/// Pushes the program name into `PROGRAM_NAME_PTR` in the runtime so
/// JIT-compiled `gos_rt_os_program_name` calls see the script path
/// rather than the synthetic `"gos"` placeholder. Mirrors the same
/// pattern as [`set_runtime_args`].
pub(crate) fn set_runtime_program_name(name: &str) {
    use std::ffi::CString;

    let cstr = CString::new(name.as_bytes()).unwrap_or_else(|_| {
        let cleaned: Vec<u8> = name.bytes().filter(|b| *b != 0).collect();
        CString::new(cleaned).expect("cleaned bytes have no NUL")
    });
    // SAFETY: `set_program_name` copies the bytes into a
    // leaked CString; the temporary `cstr` can be dropped after the
    // call returns.
    #[allow(unsafe_code)]
    unsafe {
        gossamer_runtime::c_abi::set_program_name(cstr.as_ptr());
    }
}

/// Flushes any data the JIT-compiled code has written to the
/// runtime's thread-local stdout buffer. The bytecode VM writes
/// through the Rust-side `set_stdout_writer` path which doesn't
/// touch this buffer, but JIT-compiled functions go through the
/// runtime's C-ABI `gos_rt_print_*` family which writes into
/// `STDOUT_BUF` and only flushes on `gos_rt_flush_stdout`.
///
/// The CLI calls this once after `vm.call("main", ...)` returns
/// so any JIT-promoted body's output reaches the user. Cheap
/// no-op when nothing was buffered.
pub fn flush_runtime_stdout() {
    // SAFETY: `gos_rt_flush_stdout` is `unsafe extern "C"` for
    // FFI uniformity but has no preconditions - it just drains
    // the per-thread `STDOUT_BUF` and writes to FD 1.
    #[allow(unsafe_code)]
    unsafe {
        gossamer_runtime::c_abi::gos_rt_flush_stdout();
    }
}
pub use bytecode::{FnChunk, InstructionLocation, Op, SourceLocation};
pub use comptime::{fold_into_source, fold_into_source_anchored};
pub use external_natives::{
    clear_external_natives_for_test, external_natives_snapshot, register_external_native,
};
pub use stdlib_builtins::cohort::{close_root as close_root_cohort, open_root as open_root_cohort};
pub use value::{
    Channel, Closure, NativeEnumOwner, NativeEnumShape, NativeStructShape, RuntimeError,
    RuntimeResult, SmolStr, Value, native_enum_to_variant, registry_stats_for_test,
};
pub use vm::VM_THREAD_STACK_BYTES;
pub use vm::goroutine::join_outstanding_goroutines;
pub use vm::{CallStackFrame, JitMetrics, Vm};

/// Process-wide panic hook value registered by
/// `runtime::set_panic_hook` on the interpreter tier. The report
/// paths invoke it with the rendered message instead of the default
/// report.
static PANIC_HOOK_VALUE: std::sync::LazyLock<parking_lot::Mutex<Option<value::Value>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(None));

/// Store (or clear) the interpreter-tier panic hook.
pub fn set_panic_hook_value(v: Option<value::Value>) {
    *PANIC_HOOK_VALUE.lock() = v;
}

/// Current interpreter-tier panic hook, if any.
#[must_use]
pub fn panic_hook_value() -> Option<value::Value> {
    PANIC_HOOK_VALUE.lock().clone()
}

/// The bare panic message of a runtime error: strips the
/// `error[GX0005]: panic: ` prefix so hooks see the same text on
/// every tier.
#[must_use]
pub fn panic_message(err: &value::RuntimeError) -> String {
    let rendered = err.to_string();
    rendered
        .strip_prefix("error[GX0005]: panic: ")
        .map_or(rendered.clone(), ToString::to_string)
}

/// True when a runtime error is a user panic (GX0005).
#[must_use]
pub fn is_panic_error(err: &value::RuntimeError) -> bool {
    err.to_string().starts_with("error[GX0005]")
}

/// The status a program asked for with `process::exit`, or `None` when
/// `err` reports anything else.
///
/// Only a build with no process of its own to end raises it; a native
/// build has already exited by the time this could be asked.
#[must_use]
pub fn exit_status(err: &value::RuntimeError) -> Option<i32> {
    match err {
        value::RuntimeError::Exit(code) => Some(*code),
        _ => None,
    }
}

/// The rendered payload of an `Err` value, or `None` for any other value.
///
/// A fallible function reports its failure as a returned value rather than as
/// a runtime error, so a caller that treats returning at all as success needs
/// this to see the failure.
#[must_use]
pub fn err_payload_message(value: &value::Value) -> Option<String> {
    let value::Value::Variant(inner) = value else {
        return None;
    };
    if inner.name != "Err" {
        return None;
    }
    Some(
        inner
            .fields
            .first()
            .map_or_else(|| "Err".to_string(), ToString::to_string),
    )
}

/// Whether `value` is an `Ok` variant - a test declared `-> Result<(), E>`
/// reports its verdict through this arm rather than through an assertion.
#[must_use]
pub fn is_ok_variant(value: &value::Value) -> bool {
    matches!(value, value::Value::Variant(inner) if inner.name == "Ok")
}

/// Whether `err` is the stack-overflow fault. Reported like a panic -
/// the compiled tiers raise it through the same path and exit 101 - so
/// the exit status does not depend on which tier ran the program.
#[must_use]
pub fn is_stack_overflow(err: &value::RuntimeError) -> bool {
    err.to_string().starts_with("error[GX0008]")
}
