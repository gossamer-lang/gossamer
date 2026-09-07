//! LLVM-backed release codegen.
//!
//! `gos build` uses this backend for both debug and release native
//! artifacts. This crate mirrors the Cranelift object-emission API,
//! emits LLVM IR text, and invokes minimal `opt` plus `llc -O0` for debug or
//! integrated Clang `-O3` for release code generation. The Cranelift backend remains
//! available for the in-process JIT and explicit companion paths.
//!
//! Requires Clang on `PATH` or via `GOS_LLVM_CLANG`; installations
//! without it can use `opt` and `llc` through their environment
//! overrides. The backend shells out so this crate stays FFI-free:
//! no `inkwell`/`llvm-sys` dependency, no unsafe Rust, no build-time
//! LLVM header requirements. The runtime `.a`
//! staticlib is unchanged - the linker stage (`cc`) wires
//! the LLVM-produced object against it the same way as
//! Cranelift's.
//!
//! Frontend-valid MIR is expected to lower through LLVM. A missed
//! MIR shape is reported as `BuildError::InternalLoweringBug` so
//! the driver fails loudly instead of producing a mixed-tier native
//! artifact.

// Allow patterns this backend deliberately uses:
//   - `doc_markdown` flags every reference to `i64`, `fasta_mt`,
//     `x86_64`, etc. in plain-prose docstrings.
//   - `nonminimal_bool` flags `if !cond { early_return; } else
//     { ... }` shapes that read more naturally than the
//     positively-phrased alternative.
//   - `too_many_lines` / `cognitive_complexity` fire on the
//     intrinsic-name / runtime-symbol dispatch arms.
//   - `if_not_else` is the same pattern as `nonminimal_bool`.
#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    clippy::nonminimal_bool,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::if_not_else,
    clippy::comparison_chain
)]

mod emit;
mod lower;
mod ty;

pub use emit::{
    BuildError, CodegenPhaseTimes, CompileOutcome, MINIMUM_LLVM_MAJOR, NativeObject, OptProfile,
    PREFERRED_LLVM_MAJOR, PgoMode, active_target_triple, codegen_phase_times, compile_to_object,
    compile_to_object_at_path, compile_with_fallback, compile_with_fallback_at_path, pgo_mode,
    render_ir_to_string, reproducible_enabled, set_cache_dir, set_debug_info, set_opt_profile,
    set_pgo_mode, set_race_instrumentation, set_reproducible, set_source_lines,
    set_static_musl_link, set_strict_lowering, set_target_triple, want_race_instrumentation,
};

/// Read-only view of the LLVM backend's runtime-symbol declaration
/// table. Each entry is a single LLVM IR `declare ...` line for a
/// `gos_rt_*` extern. Exposed for cross-crate consistency tests
/// (e.g. asserting every helper named in `gossamer-runtime::c_abi`
/// also has a declaration here, so the cranelift dispatch table,
/// the LLVM declaration table, and the c_abi exports never drift).
///
/// Generated from the typed `gossamer_abi::REGISTRY`; the old
/// hard-coded string array has been removed.
#[must_use]
pub fn runtime_declarations() -> Vec<String> {
    gossamer_abi::all_llvm_declarations()
}
