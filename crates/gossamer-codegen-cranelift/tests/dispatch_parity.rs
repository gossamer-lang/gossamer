//! Compile-time-style audit: every `gos_rt_*` runtime helper declared
//! in `crates/gossamer-runtime/src/c_abi.rs` must have at least one
//! dispatch arm in `crates/gossamer-codegen-cranelift/src/native.rs`.
//!
//! When this test fails, the AOT cranelift backend is silently
//! returning `iconst i64 0` from a missing helper (the soft-fallback
//! at `native.rs`'s end). That is exactly the failure mode that
//! produced four months of "works in interp / fails in --release"
//! bugs. The test is intentionally crude - a string scan over both
//! files - so it catches new regressions even when the dispatch
//! pattern grows.
//!
//! Helpers handled by prefix-dispatch (e.g. `gos_rt_fn_tramp_*`)
//! must be added to `PREFIX_HANDLED` below.

#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

const RUNTIME_PATH: &str = "../gossamer-runtime/src/c_abi.rs";
const NATIVE_PATH: &str = "src/native.rs";

/// Helpers reached through a prefix-matching dispatch arm rather
/// than a per-name match. Add new prefixes here when the AOT
/// dispatch grows them.
const PREFIX_HANDLED: &[&str] = &[
    "gos_rt_fn_tramp_", // 0..=8 trampolines, dispatched as a family
    // F#-parity Phase 1b: every `gos_rt_iter_*` / `gos_rt_option_*` /
    // `gos_rt_result_*` helper is dispatched through the generic
    // registry-lookup branch in `native.rs` (`if name.starts_with
    // ("gos_rt_") { gossamer_abi::lookup(name) → extern_fn_by_name }`)
    // and via per-call `declare_rt(name)` in `gossamer-codegen-llvm/
    // src/lower.rs`. No per-name match arm needed.
    "gos_rt_iter_",
    "gos_rt_option_",
    "gos_rt_result_",
    // aggregate alloc/free emitted directly by
    // the codegen for `Rvalue::Aggregate` / `Rvalue::Repeat` and
    // by the MIR drop pass at scope exit. Generic registry-lookup
    // dispatch handles the CallIntrinsic path; the `_alloc` is
    // called by name from both backends' Aggregate sites.
    "gos_rt_aggr_",
];

/// Helpers that are deliberately Rust-only (used inside the runtime
/// but never reached from generated MIR). Skip these.
const RUST_ONLY: &[&str] = &[
    "gos_rt_string_view",         // helper used inside other helpers
    "gos_rt_vec_sanity_check",    // debug-only assertion helper
    "gos_rt_static_set_str_rust", // safe Rust API mirror
    // GC internals - called from vec_free and other runtime helpers,
    // never emitted from MIR.
    "gos_rt_gc_deregister",
    // Slot duplication for a JSON-handle vector, called from the
    // element-share walk; MIR never names it.
    "gos_rt_json_clone_handle",
    // FFI / Rust-binding helpers - declared in c_abi.rs for external
    // callers (Rust bindings, runtime tests) but never lowered from
    // MIR. AOT codegen does not need to dispatch them.
    "gos_rt_arena_restore",
    "gos_rt_arena_save",
    "gos_rt_atomic_i64_cas",
    "gos_rt_atomic_i64_cas_acq_rel",
    "gos_rt_atomic_i64_fetch_add_acqrel",
    "gos_rt_atomic_i64_load_acquire",
    "gos_rt_atomic_i64_load_relaxed",
    "gos_rt_atomic_i64_store_relaxed",
    "gos_rt_atomic_i64_store_release",
    "gos_rt_atomic_i64_swap",
    "gos_rt_callback_invoke",
    // callback registry. Not emitted from MIR;
    // called only by the binding crate's `BindingCallback` Drop
    // impl and by user code that opts into the ABI 0.4 callback
    // surface.
    "gos_rt_callback_register",
    "gos_rt_callback_unregister",
    "gos_rt_chan_drop",
    // Cross-crate context-cancellation bridge: installed once
    // by `gossamer-std::context` at first context use, called
    // by the context-aware runtime entry. Not emitted from MIR
    // yet (the `.gos`-level `rx.recv_ctx(&ctx)` surface needs
    // language frontend wiring before either backend would
    // call it).
    "gos_rt_chan_recv_ctx_option",
    "gos_rt_install_ctx_hooks",
    "gos_rt_install_trace_hook",
    "gos_rt_concat_f64_prec",
    "gos_rt_gc_reset",
    "gos_rt_heap_i64_free",
    "gos_rt_heap_u8_free",
    // RC inspection/release helpers used by the interpreter and build
    // linkage keepalive list, not emitted as MIR runtime calls.
    "gos_rt_rc_release_no_buffer",
    "gos_rt_rc_strong_count",
    "gos_rt_str_free",
    "gos_rt_result_dbg",
    "gos_rt_sync_i64_add",
    "gos_rt_sync_i64_drop",
    "gos_rt_sync_i64_get",
    "gos_rt_sync_i64_len",
    "gos_rt_sync_i64_new",
    "gos_rt_sync_i64_push",
    "gos_rt_sync_i64_set",
    "gos_rt_sync_u8_drop",
    "gos_rt_sync_u8_get",
    "gos_rt_sync_u8_len",
    "gos_rt_sync_u8_new",
    "gos_rt_sync_u8_push",
    "gos_rt_sync_u8_set",
    "gos_rt_u64_to_str",
    "gos_rt_wg_error",
    "gos_rt_wg_error_clear",
    // Called from Rust (gossamer-interp) to override argv[0], not
    // emitted from MIR.
    "gos_rt_set_program_name",
    // 0.6.0 deep-vec-free typed allocators. The codegen dispatch
    // for `Vec::new` / `Vec::with_capacity` calls these directly
    // (not as a named MIR call), so they don't appear in the
    // syntactic `name == "..."` arms of native.rs even though they
    // are emitted via `extern_fn_by_name`. Mark Rust-only at the
    // parity-check level so the missing-arm guard doesn't trip.
    "gos_rt_vec_new_typed",
    "gos_rt_vec_with_capacity_typed",
    // 0.6.0 binding-side map free helper. Called from the
    // gossamer-binding crate's drop path for binding-owned maps
    // whose layout differs from the runtime's `GosMap`. Never
    // emitted from MIR.
    "gos_rt_binding_map_free",
    // No-op compatibility shims retained from the retired tracing
    // GC (see `c_abi::gc`); never emitted from MIR.
    "gos_rt_gc_alloc_count",
    "gos_rt_gc_collect",
    // test/diagnostic hook. Not emitted from
    // MIR - called only by tests asserting clean goroutine
    // shutdown.
    "gos_rt_goroutine_panicked",
];

fn read_to_string(rel: &str) -> String {
    // The split of `c_abi.rs` → `c_abi/` directory and `native.rs` →
    // `native/` directory means a single file read is no longer
    // enough. When both the module file and a sibling directory
    // exist, concatenate both; otherwise the helper scan can pass
    // over only `pub mod` declarations.
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let direct = crate_root.join(rel);
    let dir = if let Some(stripped) = rel.strip_suffix(".rs") {
        crate_root.join(stripped)
    } else {
        direct.clone()
    };
    let mut out = String::new();
    if direct.is_file() {
        out.push_str(
            &fs::read_to_string(&direct)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", direct.display())),
        );
        out.push('\n');
    }
    if dir.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("failed to read_dir {}: {e}", dir.display()))
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rs"))
            .collect();
        entries.sort();
        for path in entries {
            out.push_str(&fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("failed to read {}: {e}", path.display());
            }));
            out.push('\n');
        }
        return out;
    }
    if !out.is_empty() {
        return out;
    }
    panic!(
        "failed to locate {} or its directory form",
        direct.display()
    );
}

fn declared_helpers(src: &str) -> BTreeSet<String> {
    // Match `pub unsafe extern "C" fn gos_rt_<name>(`
    let mut out = BTreeSet::new();
    let needle = "pub unsafe extern \"C\" fn ";
    let safe_needle = "pub extern \"C\" fn ";
    for prefix in [needle, safe_needle] {
        let mut cursor = 0;
        while let Some(idx) = src[cursor..].find(prefix) {
            let start = cursor + idx + prefix.len();
            let rest = &src[start..];
            let end = rest
                .find(|c: char| c == '(' || c.is_whitespace())
                .unwrap_or(rest.len());
            let name = &rest[..end];
            if name.starts_with("gos_rt_") {
                out.insert(name.to_string());
            }
            cursor = start + end;
        }
    }
    out
}

fn dispatched_helpers(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut cursor = 0;
    let needle = "gos_rt_";
    while let Some(idx) = src[cursor..].find(needle) {
        let start = cursor + idx;
        let rest = &src[start..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let name = &rest[..end];
        out.insert(name.to_string());
        cursor = start + end;
    }
    out
}

#[test]
fn every_runtime_helper_has_llvm_declaration_or_prefix_handler() {
    // After ABI Phase 5 the LLVM backend declares runtime symbols
    // lazily via per-function `declare_rt()` calls in `lower.rs`
    // rather than emitting a single static prelude. We therefore
    // scan BOTH `emit.rs` and `lower.rs` for `gos_rt_` references.
    //
    // Symbols classified as `Tier::Cranelift` in the ABI registry
    // are never called by the LLVM backend and need no declaration;
    // we skip them here. Symbols absent from the registry but also
    // absent from LLVM source must appear in `RUST_ONLY`.
    let runtime = read_to_string(RUNTIME_PATH);
    let llvm_emit = read_to_string("../gossamer-codegen-llvm/src/emit.rs");
    let llvm_lower = read_to_string("../gossamer-codegen-llvm/src/lower.rs");
    let declared = declared_helpers(&runtime);
    let mut llvm_dispatched = dispatched_helpers(&llvm_emit);
    llvm_dispatched.extend(dispatched_helpers(&llvm_lower));

    // Build a set from the typed ABI registry. Registry-known
    // `gos_rt_*` calls are declared through the dynamic `declare_rt`
    // path in the LLVM lowerer; Cranelift-only entries are skipped for
    // the same reason as before.
    let registry_known: BTreeSet<&str> = gossamer_abi::REGISTRY.iter().map(|e| e.name).collect();
    let cranelift_only: BTreeSet<&str> = gossamer_abi::REGISTRY
        .iter()
        .filter(|e| e.tier == gossamer_abi::Tier::Cranelift)
        .map(|e| e.name)
        .collect();

    let missing: Vec<&String> = declared
        .iter()
        .filter(|name| !llvm_dispatched.contains(name.as_str()))
        .filter(|name| !PREFIX_HANDLED.iter().any(|p| name.starts_with(p)))
        .filter(|name| !RUST_ONLY.contains(&name.as_str()))
        .filter(|name| !registry_known.contains(name.as_str()))
        .filter(|name| !cranelift_only.contains(name.as_str()))
        .collect();
    if !missing.is_empty() {
        let names = missing
            .iter()
            .map(|n| format!("  - {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} runtime helper(s) declared in c_abi.rs have no LLVM \
             reference in emit.rs or lower.rs (and are not Cranelift-only):\n\
             {names}\n\nFix: add a `declare_rt()` call in lower.rs, or add \
             the symbol to `RUST_ONLY` if it is never called from MIR.",
            missing.len(),
        );
    }
}

#[test]
fn every_runtime_helper_has_aot_dispatch_or_prefix_handler() {
    let runtime = read_to_string(RUNTIME_PATH);
    let native = read_to_string(NATIVE_PATH);
    let declared = declared_helpers(&runtime);
    let dispatched = dispatched_helpers(&native);
    let registry_known: BTreeSet<&str> = gossamer_abi::REGISTRY.iter().map(|e| e.name).collect();

    let missing: Vec<&String> = declared
        .iter()
        .filter(|name| !dispatched.contains(name.as_str()))
        .filter(|name| !PREFIX_HANDLED.iter().any(|p| name.starts_with(p)))
        .filter(|name| !RUST_ONLY.contains(&name.as_str()))
        .filter(|name| !registry_known.contains(name.as_str()))
        .collect();

    if !missing.is_empty() {
        let names = missing
            .iter()
            .map(|n| format!("  - {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} runtime helper(s) declared in c_abi.rs have no AOT dispatch \
             arm in native.rs (and no prefix handler). They will silently \
             return `iconst i64 0` from compiled programs:\n{names}\n\nFix: \
             add a dispatch arm in `crates/gossamer-codegen-cranelift/src/native.rs`.",
            missing.len(),
        );
    }
}
