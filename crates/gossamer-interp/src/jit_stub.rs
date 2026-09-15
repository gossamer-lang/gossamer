//! No-op JIT backend for wasm32-unknown-unknown.
//!
//! Cranelift does not target wasm32-unknown-unknown, so the browser
//! playground links this stub in place of `gossamer-codegen-cranelift`.
//! It mirrors the public types and entry points the VM references
//! (`JitKind`, `JitFn`, `JitArtifact`, [`has_worthy_jit_body`],
//! [`compile_to_jit_for_promotion`]) but never compiles anything:
//! `has_worthy_jit_body` always returns `false` and both compile entry
//! points yield an empty artifact, so every Gossamer function runs on the
//! bytecode VM. The JIT is purely a speed optimisation, so this is a clean
//! functional equivalent.

// This stub deliberately mirrors the cranelift backend's public API (types,
// variants, entry points) so the VM's `jit_call` trampoline type-checks
// unchanged on wasm, even though nothing here is ever constructed or reached
// (no body is JIT-compiled). The dead-code / unreachable-pub lints flag exactly
// that intentional mirroring.
#![allow(dead_code, unreachable_pub)]

use std::collections::HashMap;

use gossamer_abi::jit_carrier::{CarrierBoxMetas, CarrierShape};

/// ABI classification of a JIT slot. Mirrors the cranelift enum so the
/// VM's trampoline (`jit_call`) type-checks; no instance is ever
/// constructed on wasm because no body is JIT-compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitKind {
    /// 64-bit signed integer.
    I64,
    /// 64-bit IEEE-754 float.
    F64,
    /// Boolean.
    Bool,
    /// `char` carried as its Unicode scalar value in an integer register.
    Char,
    /// Unit (no representation).
    Unit,
    /// Packed runtime `GossamerValue`.
    Value,
    /// Heap enum as its native tagged pointer; payload is the VM
    /// shape-table index.
    EnumPtr(u32),
    /// `Result<Enum, _>` return as the by-value two-word `i128`; payload is
    /// the `Ok` enum's VM shape-table index.
    ResultEnumPtr(u32),
    /// `Option` / `Result` as its two words through a carrier thunk; payload
    /// names what each arm's payload word holds.
    Carrier(CarrierShape),
    /// All-scalar user struct as a pointer to its flat field-slot block;
    /// payload is the VM struct-shape-table index.
    StructPtr(u32),
    /// `String` as a native cstring pointer.
    NativeStr,
    /// `Vec<i64>` as a native `GosVec` pointer.
    NativeVecI64,
    /// `Vec<f64>` as a native `GosVec` pointer.
    NativeVecF64,
    /// `Vec<String>` as a native `GosVec` of owned cstring slots.
    NativeVecStr,
    /// `Vec<(i64, f64)>` as a native `GosVec` of 16-byte primitive slots.
    NativeVecTupleIF,
    /// `Vec<Vec<i64>>` as a native outer `GosVec` of inner `GosVec` pointers.
    NativeVecVecI64,
    /// `U8Vec` byte-buffer handle, marshalled by copy-in / copy-back.
    U8VecHandle,
    /// A 2-element tuple RETURN of scalars / heap enums; payload is the
    /// per-element decode kind. Return-only.
    TupleReturn([TupleElem; 2]),
    /// `[T; N]` over a scalar element as a pointer to a flat block of `N`
    /// 8-byte slots; payloads are the element count and slot encoding.
    ArrayBlockPtr(u32, ArrayElem),
}

/// Mirrors `gossamer_codegen_cranelift::ArrayElem` so the shared dispatch
/// code in `jit_call` type-checks on wasm; no instance is ever constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayElem {
    /// A signed 64-bit integer slot.
    I64,
    /// An IEEE-754 double stored as its bit pattern.
    F64,
    /// A Unicode scalar stored as its code point.
    Char,
}

/// Mirrors `gossamer_codegen_cranelift::TupleElem` so the shared dispatch
/// code in `jit_call` type-checks on wasm; no instance is ever constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TupleElem {
    /// 64-bit integer slot.
    I64,
    /// 64-bit float slot (raw bits).
    F64,
    /// Boolean slot (non-zero = true).
    Bool,
    /// Unicode scalar slot (low 32 bits).
    Char,
    /// Heap string slot (tagged c-string pointer).
    Str,
    /// Heap enum slot; payload is the VM shape-table index.
    Enum(u32),
}

/// A compiled function handle. On wasm none is ever produced.
#[derive(Clone)]
pub struct JitFn {
    /// Gossamer source name.
    pub name: std::sync::Arc<str>,
    /// Entry pointer (never valid on wasm - no instance is created).
    pub ptr: *const u8,
    /// Parameter kinds in source order.
    pub params: Box<[JitKind]>,
    /// Return slot kind.
    pub returns: JitKind,
    /// Mirrors `gossamer_codegen_cranelift::JitFn::returns_fresh` so the
    /// shared dispatch code in `jit_call` compiles against either handle.
    /// The wasm stub never promotes a body, so the value is irrelevant.
    pub returns_fresh: bool,
    /// Mirrors `gossamer_codegen_cranelift::JitFn::carrier_box_metas`.
    pub carrier_box_metas: Box<[CarrierBoxMetas]>,
}

/// A JIT frame on the machine stack. wasm compiles none.
pub struct JitFrame {
    /// The body's source name.
    pub function: std::sync::Arc<str>,
    /// Where in the source the frame stands.
    pub span: Option<gossamer_lex::Span>,
}

/// wasm runs no JIT code, so no JIT frame is ever on the stack.
#[must_use]
pub fn active_jit_frames() -> Vec<JitFrame> {
    Vec::new()
}

/// A set of compiled functions. Always empty on wasm.
#[derive(Default)]
pub struct JitArtifact {
    /// Compiled functions keyed by Gossamer source name.
    pub functions: HashMap<std::sync::Arc<str>, std::sync::Arc<JitFn>>,
    /// Native code emitted for this artifact. The wasm stub emits none.
    pub code_bytes: u64,
    /// wasm artifacts are empty and safe to reuse.
    pub cacheable: bool,
}

/// Static admission record matching the native backend API.
pub struct JitBodyDecision {
    /// Body name.
    pub name: String,
    /// Always false on wasm.
    pub admitted: bool,
    /// Stable rejection categories.
    pub reasons: Vec<&'static str>,
}

/// wasm never promotes a body to native code.
#[must_use]
pub fn has_worthy_jit_body(
    _bodies: &[gossamer_mir::Body],
    _tcx: &gossamer_types::TyCtxt,
    _enum_shapes: &HashMap<u32, u32>,
    _struct_shapes: &HashMap<u32, u32>,
) -> bool {
    false
}

/// wasm retains no native compiler snapshot.
#[must_use]
pub fn jit_compile_body_names(
    _bodies: &[gossamer_mir::Body],
    _tcx: &gossamer_types::TyCtxt,
    _enum_shapes: &HashMap<u32, u32>,
    _struct_shapes: &HashMap<u32, u32>,
) -> std::collections::HashSet<String> {
    std::collections::HashSet::new()
}

/// wasm retains no trigger-specific compiler snapshot.
#[must_use]
pub fn jit_compile_body_names_for_trigger(
    _bodies: &[gossamer_mir::Body],
    _tcx: &gossamer_types::TyCtxt,
    _enum_shapes: &HashMap<u32, u32>,
    _struct_shapes: &HashMap<u32, u32>,
    _trigger: &str,
) -> std::collections::HashSet<String> {
    std::collections::HashSet::new()
}

/// wasm exposes no native entry candidates.
#[must_use]
pub fn jit_entry_body_names(
    _bodies: &[gossamer_mir::Body],
    _tcx: &gossamer_types::TyCtxt,
    _enum_shapes: &HashMap<u32, u32>,
    _struct_shapes: &HashMap<u32, u32>,
) -> std::collections::HashSet<String> {
    std::collections::HashSet::new()
}

/// wasm exposes no native entry candidates.
#[must_use]
pub fn jit_entry_body_names_with_admitted(
    _bodies: &[gossamer_mir::Body],
    _admitted: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    std::collections::HashSet::new()
}

/// wasm reports every body as unavailable for native promotion.
#[must_use]
pub fn jit_promotion_report(
    bodies: &[gossamer_mir::Body],
    _tcx: &gossamer_types::TyCtxt,
    _enum_shapes: &HashMap<u32, u32>,
    _struct_shapes: &HashMap<u32, u32>,
) -> Vec<JitBodyDecision> {
    bodies
        .iter()
        .map(|body| JitBodyDecision {
            name: body.name.clone(),
            admitted: false,
            reasons: vec!["native-jit-unavailable"],
        })
        .collect()
}

/// wasm promotes nothing, so there are no eager-compile candidates.
#[must_use]
pub fn jit_eager_loop_bodies(
    _bodies: &[gossamer_mir::Body],
    _tcx: &gossamer_types::TyCtxt,
    _enum_shapes: &HashMap<u32, u32>,
    _struct_shapes: &HashMap<u32, u32>,
) -> Vec<String> {
    Vec::new()
}

/// wasm yields an empty artifact - the VM installs no native overrides
/// and runs everything on the bytecode interpreter.
#[allow(clippy::missing_errors_doc, clippy::unnecessary_wraps)]
pub fn compile_to_jit(
    _bodies: &[gossamer_mir::Body],
    _tcx: &gossamer_types::TyCtxt,
    _enum_shapes: &HashMap<u32, u32>,
    _struct_shapes: &HashMap<u32, u32>,
) -> Result<JitArtifact, String> {
    Ok(JitArtifact::default())
}

/// wasm applies no promotion policy because it cannot compile native code.
#[allow(clippy::missing_errors_doc, clippy::unnecessary_wraps)]
pub fn compile_to_jit_for_promotion(
    _bodies: &[gossamer_mir::Body],
    _tcx: &gossamer_types::TyCtxt,
    _enum_shapes: &HashMap<u32, u32>,
    _struct_shapes: &HashMap<u32, u32>,
) -> Result<JitArtifact, String> {
    Ok(JitArtifact::default())
}

/// Ownership-taking wasm stub matching the native backend API.
#[allow(clippy::missing_errors_doc, clippy::unnecessary_wraps)]
pub fn compile_to_jit_for_promotion_owned(
    _bodies: Vec<gossamer_mir::Body>,
    _tcx: &gossamer_types::TyCtxt,
    _enum_shapes: &HashMap<u32, u32>,
    _struct_shapes: &HashMap<u32, u32>,
) -> Result<JitArtifact, String> {
    Ok(JitArtifact::default())
}
