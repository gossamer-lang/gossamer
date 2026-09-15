//! Cranelift-backed codegen for the Gossamer compiler.
//! Three entry points are exported:
//! - [`emit_module`] - MIR → textual CLIF-style IR. Human-readable,
//!   no native codegen, always available.
//! - [`compile_to_object`] - MIR → native object bytes (ELF on
//!   Linux, Mach-O on macOS). Drives the real `cranelift-object`
//!   pipeline and supports integer arithmetic + direct calls +
//!   `return` today. A C-ABI `main(argc, argv) -> i32` shim is
//!   inserted automatically so the resulting object links through a
//!   standard `cc` invocation to produce an executable.
//! - [`compile_to_jit`] - MIR → in-process native code via the
//!   `cranelift-jit` backend. Returns a [`JitArtifact`] of raw fn
//!   pointers the bytecode VM dispatches into so `gos --vm`
//!   executes hot user functions natively while still falling back
//!   to the bytecode interpreter for constructs the codegen does
//!   not lower.
//!
//! Cranelift's public API is safe at the lowering layer; the JIT
//! dispatch trampoline holds raw pointers, so `jit` opts out of the
//! crate-wide `forbid(unsafe_code)` with a scoped `allow`.

#![deny(unsafe_code)]

mod emit;
mod jit;
mod jit_frames;
mod jit_memory;
mod native;
pub mod native_symbols;

pub use emit::{FunctionText, Module, emit_function, emit_module};
pub use jit::{
    ArrayElem, JitArtifact, JitBodyDecision, JitFn, JitKind, TupleElem, compile_to_jit,
    compile_to_jit_for_promotion, compile_to_jit_for_promotion_owned, has_worthy_jit_body,
    jit_compile_body_names, jit_compile_body_names_for_trigger, jit_eager_loop_bodies,
    jit_entry_body_names, jit_entry_body_names_with_admitted, jit_promotion_report,
};
pub use jit_frames::{JitFrame, active_frames as active_jit_frames};
pub use native::{
    CompileOptions, NativeObject, compile_to_object, compile_to_object_at_path,
    compile_to_object_at_path_with_options, compile_to_object_with_options,
};
pub use native_symbols::{
    NATIVE_SYMBOLS, NativeSymbol, NativeSymbolEntry, native_symbols_snapshot,
    register_native_symbol,
};
