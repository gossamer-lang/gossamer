//! In-process Cranelift JIT used by `gos --vm`.
//!
//! Reuses the [`super::native::lower_program_serial`] HIR → MIR → CLIF
//! pipeline that the AOT object backend drives, swapping the
//! `ObjectModule` for a `JITModule`. The resulting raw fn pointers
//! are returned in a [`JitArtifact`] that the bytecode VM reads at
//! every `Op::Call` so hot user functions execute as native code
//! instead of dispatching through the bytecode loop.
//!
//! The VM's register-based dispatch maps cleanly onto SSA, so the
//! same MIR form the AOT path consumes drops straight in. Functions
//! whose codegen path can't lower a feature (closures, dynamic
//! shapes, …) are simply skipped; the VM's existing bytecode
//! interpreter still handles them.

#![allow(unsafe_code)]

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use cranelift_jit::{JITBuilder, JITModule};
use gossamer_mir::Body;
use gossamer_types::{ArrayLen, Ty, TyCtxt, TyKind};

use crate::jit_memory::NativeCodeHeap;
use crate::native::{FailedBody, build_native_isa, lower_program_serial};

/// Encoding of one slot in a fixed-array parameter's flat block. Every
/// class occupies a full 8-byte slot; they differ in how the trampoline
/// writes a VM value into it and reads one back out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayElem {
    /// A signed 64-bit integer, written as itself.
    I64,
    /// An IEEE-754 double, written as its bit pattern.
    F64,
    /// A Unicode scalar, written as its `u32` code point.
    Char,
}

/// The scalar an `Ok` payload word carries in a [`JitKind::ResultScalar`]
/// return, which is what the trampoline re-wraps it as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultScalarKind {
    /// The word is the integer itself.
    I64,
    /// The word is the double's bit pattern.
    F64,
    /// The low bit of the word is the boolean.
    Bool,
    /// The word is the Unicode scalar's code point.
    Char,
}

/// Cranelift register class for one parameter or return slot of a
/// JIT-compiled body. Used by the dispatch trampoline to pick the
/// right marshalling shape per slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitKind {
    /// A 64-bit signed integer (`i64`, `i32` widened, `usize`, …).
    I64,
    /// A 64-bit IEEE-754 float.
    F64,
    /// A 1-bit boolean represented as `i8` in the cranelift ABI.
    Bool,
    /// A Unicode scalar (`char`) crossing the boundary as its `u32` code
    /// point in an integer register. The trampoline reads a `Value::Char`
    /// for a parameter and re-wraps a returned word through `char::from_u32`.
    Char,
    /// The unit value (no representation; the body has no return).
    Unit,
    /// A runtime [`gossamer_runtime::GossamerValue`] - the u64-packed shape the
    /// codegen uses for any non-scalar type (String, Tuple, Array,
    /// Struct, Variant, Closure, Channel). Aggregate values cross
    /// the JIT boundary as `gossamer_runtime::GossamerValue`
    /// handles; the trampoline marshals via
    /// `Value::to_raw` / `Value::from_raw`.
    Value,
    /// A heap enum crossing the boundary as its NATIVE tagged pointer
    /// (the compiled-tier representation the JIT body works with
    /// directly, zero conversion). The payload is the VM-side
    /// shape-table index used to re-wrap returned pointers. Integer
    /// register class.
    EnumPtr(u32),
    /// A `Result<E, errors::Error>` RETURN whose `Ok` payload is a heap enum
    /// of the carried shape-table index. The body returns the by-value
    /// two-word `i128` `[disc, payload]` (the `gos_rt_result_new` shape): on
    /// `Ok` the payload is the native enum pointer, on `Err` a native
    /// `*mut GosError`. The trampoline decodes the `i128` and marshals each
    /// side back to a VM `Value`. Return-only.
    ResultEnumPtr(u32),
    /// A `Result<String, errors::Error>` RETURN using the same two-word
    /// carrier shape as [`Self::ResultEnumPtr`]. On `Ok`, the payload is an
    /// owned native string pointer; on `Err`, a native `*mut GosError`.
    /// Return-only.
    ResultNativeStr,
    /// A `Result<i64 | f64 | bool | char, errors::Error>` RETURN on the same
    /// two-word carrier. On `Ok` the payload word is the scalar itself (an
    /// `f64` as its bit pattern); on `Err` a native `*mut GosError`. This is
    /// the shape every `?`-using arithmetic helper returns. Return-only.
    ResultScalar(ResultScalarKind),
    /// An `Option<i64 | f64 | bool | char>` RETURN on the same two-word
    /// carrier: `disc` 0 is `Some` and the payload word is the scalar, any
    /// other disc is `None`. Return-only.
    OptionScalar(ResultScalarKind),
    /// An all-scalar user struct (`&self` / `&mut self` / by-value)
    /// crossing the boundary as a pointer to a flat field-slot block
    /// (one 8-byte slot per field, field `i` at byte offset `i * 8`, NO
    /// RC header - the compiled tier's struct layout). The payload is
    /// the VM-side struct-shape-table index. The trampoline builds a
    /// fresh block from the VM `Value::Struct`, passes its pointer, and
    /// for a `&mut` parameter writes the mutated block back into the
    /// caller's binding. A by-value return uses the native structural-return
    /// ABI: the trampoline supplies a caller-owned block as a hidden trailing
    /// argument and reads that block after the body returns. Integer register
    /// class.
    StructPtr(u32),
    /// A fixed array of a scalar element (`[i64; N]`, `[f64; N]`,
    /// `[bool; N]`, `[char; N]`) crossing the boundary as a pointer to a
    /// flat block of `N` 8-byte slots - element `i` at byte offset
    /// `i * 8`, no header, the layout the compiled tier indexes with a
    /// static stride. The payloads are the element count, which is part of
    /// the type so the block needs no length word, and the element's class,
    /// which says how each slot is encoded. The trampoline builds a fresh
    /// block from the VM array, passes its pointer, and reclaims the block
    /// once the body returns. Parameter-only. Integer register class.
    ArrayBlockPtr(u32, ArrayElem),
    /// A `String` crossing the boundary as the runtime's native
    /// `*mut c_char` cstring pointer (the flat-ABI shape the codegen
    /// uses). The trampoline builds a fresh owned cstring from the VM
    /// String for a param, and reads back + frees a returned cstring.
    /// Integer register class.
    NativeStr,
    /// A `Vec<i64>` crossing the boundary as the runtime's native
    /// `*mut GosVec` pointer (8-byte primitive slots). The trampoline
    /// builds a fresh owned `GosVec` from the VM vec for a param, and
    /// reads back + frees a returned `GosVec`. Integer register class.
    NativeVecI64,
    /// A `Vec<String>` / `[String]` crossing as the runtime's `*mut GosVec`
    /// tagged as string-element storage, each slot an owned cstring. The
    /// trampoline builds one from the VM sequence and frees it after the
    /// call; a returned one is read back and freed the same way.
    NativeVecStr,
    /// A `Vec<f64>` crossing as a native `*mut GosVec` (8-byte float
    /// slots). Same marshalling as [`Self::NativeVecI64`], f64 elements.
    NativeVecF64,
    /// A `Vec<(i64, f64)>` crossing as a native `*mut GosVec` with
    /// 16-byte primitive slots (`[i64 @ +0][f64 @ +8]`, the compiled-tier
    /// tuple layout). The trampoline builds the vector from the VM tuples
    /// and frees it after the call. Integer register class.
    NativeVecTupleIF,
    /// A `Vec<Vec<i64>>` (`[[i64]]`) crossing as a native outer `*mut GosVec`
    /// tagged `vec_elem_kind::VEC` (8-byte pointer slots), each slot a pointer
    /// to an inner `*mut GosVec` of i64 - the AOT-tier `[[i64]]` layout the
    /// JIT body reads directly when it indexes `graph[node]` and iterates the
    /// inner vec. The trampoline marshals through an Arc-identity cache (a
    /// graph reused across calls is built once) and frees the nested structure
    /// at Vm teardown rather than per call. Param only. Integer register class.
    NativeVecVecI64,
    /// A `U8Vec` opaque byte buffer crossing as the runtime's native
    /// `*mut GosU8Vec` pointer. The bytecode VM backs `U8Vec` with a
    /// registry handle, so the trampoline copies the bytes into a fresh
    /// native buffer for the call and copies the (mutated) bytes back
    /// afterwards. Integer register class.
    U8VecHandle,
    /// A 2-element tuple RETURN whose elements are scalars / heap enums (the
    /// common `(Node, i64)` / `(i64, i64)` "build" shape). The compiled tier
    /// returns it as a pointer to a `gos_rt_aggr_alloc` block of two 8-byte
    /// slots; the trampoline reads each slot per its [`TupleElem`] kind (an
    /// `Enum` slot becomes an owning native handle) into a `Value::Tuple`, then
    /// shallow-frees the block. Return-only. Letting a tuple-returning
    /// constructor JIT keeps the values it produces native end to end.
    TupleReturn([TupleElem; 2]),
}

/// One element of a JIT-marshalled tuple return ([`JitKind::TupleReturn`]).
/// Mirrors the runtime's `NativeFieldKind` for the slot decode, but lives in
/// the codegen crate so `JitKind` stays self-contained.
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
    /// Heap enum slot; the payload is the VM shape-table index used to re-wrap
    /// the returned pointer as a native handle.
    Enum(u32),
}

/// Raw handle for a JIT-compiled function: a fn pointer plus the
/// per-slot kinds that tell the dispatch trampoline how to marshal
/// arguments and the return value.
#[derive(Debug, Clone)]
pub struct JitFn {
    /// The Gossamer source name of the function. Mainly for
    /// `GOS_JIT_TRACE` diagnostics.
    pub name: std::sync::Arc<str>,
    /// Raw pointer to the entry of the compiled function. Valid for
    /// the lifetime of the owning [`JitArtifact`].
    pub ptr: *const u8,
    /// One [`JitKind`] per parameter, in source order.
    pub params: Box<[JitKind]>,
    /// The return slot's kind.
    pub returns: JitKind,
    /// `true` when the body's return value provably originates from a
    /// fresh allocation (a constructor, or a call to another fresh body)
    /// rather than a projection / passthrough of an enum parameter. The
    /// interpreter uses this to decide whether it may marshal a bytecode
    /// `Value::Variant` enum argument and free the temporary after the
    /// call: safe only when the native result can't alias the freed input.
    /// See `compute_returns_fresh`.
    pub returns_fresh: bool,
}

// SAFETY: `ptr` is read-only from any thread, but the VM is
// single-threaded today. We do not implement Send/Sync for `JitFn`
// - anyone who copies it must keep it on the owning thread.

/// Emits one machine-readable admission summary per JIT compilation
/// attempt on stderr, gated on `GOS_JIT_STATS`. Verification harnesses read
/// the lines back to prove the Cranelift tier actually installed native
/// entries instead of silently running the whole program on bytecode.
fn report_jit_stats(compiled: usize) {
    if std::env::var_os("GOS_JIT_STATS").is_some() {
        eprintln!("gos-jit-stats: compiled={compiled}");
    }
}

/// Owns finalized native allocations and a name → [`JitFn`] map.
/// Dropping the artifact frees every page that backs the function
/// pointers it has handed out, so the VM must hold the artifact
/// for as long as any compiled fn is reachable.
pub struct JitArtifact {
    /// Shared allocation owner retained after the compiler module is dropped.
    /// Empty artifacts do not construct a native heap.
    heap: Option<std::sync::Arc<NativeCodeHeap>>,
    /// Compiled functions keyed by their Gossamer source name.
    /// Handles are immutable and are shared with the VM's dispatch map.
    /// Keeping one `Arc<JitFn>` per native entry avoids duplicating the
    /// signature vectors and names merely to install an override.
    pub functions: HashMap<std::sync::Arc<str>, std::sync::Arc<JitFn>>,
    /// Exact number of bytes Cranelift generated for the lowered user bodies
    /// in this artifact. It includes machine code, jump tables, and constant
    /// data in each function's finalized code buffer; it deliberately does
    /// not estimate executable-page allocation or unrelated runtime code.
    pub code_bytes: u64,
    /// Whether this artifact may be reused by another VM. Writable module
    /// data belongs to one VM instance, so artifacts containing `static mut`
    /// accessors must never cross that boundary.
    pub cacheable: bool,
}

/// Deterministic static admission result for one MIR body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitBodyDecision {
    /// Source-level or mangled body name.
    pub name: String,
    /// Whether the body belongs to the transitive native compile set.
    pub admitted: bool,
    /// Stable machine-readable rejection categories.
    pub reasons: Vec<&'static str>,
}

impl std::fmt::Debug for JitArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The `module` field is intentionally omitted - its
        // pointer-shaped `Debug` output churns across runs and
        // adds no signal. `finish_non_exhaustive` documents the
        // skip in a clippy-blessed way.
        f.debug_struct("JitArtifact")
            .field("functions", &self.functions.keys().collect::<Vec<_>>())
            .field("code_bytes", &self.code_bytes)
            .field("cacheable", &self.cacheable)
            .field("detached", &self.is_detached())
            .finish_non_exhaustive()
    }
}

impl JitArtifact {
    /// Confirms that a non-empty artifact no longer retains its compiler
    /// module. This is public so integration and cross-platform CI tests can
    /// prove execution happens after module destruction.
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.heap.as_ref().is_none_or(|heap| heap.is_detached())
    }
}

/// Returns the names of user-defined bodies called by `body`.
/// User-function callees of `body`: by-name calls into known bodies
/// plus `FnRef` calls resolved through the def -> body-name map. The
/// second tuple slot reports an UNRESOLVABLE `FnRef` (a def with no
/// MIR body - e.g. a prelude scalar): such a body cannot be compiled
/// (the lowering refuses zero-stubs) and must be excluded.
/// A body named by a const-string operand, which is how a closure reaches
/// its code without a call terminator.
fn referenced_body<'a>(
    operand: &gossamer_mir::Operand,
    all_names: &std::collections::HashSet<&'a str>,
) -> Option<&'a str> {
    use gossamer_mir::{ConstValue, Operand};
    match operand {
        Operand::Const(ConstValue::Str(name)) => all_names.get(name.as_str()).copied(),
        _ => None,
    }
}

fn body_user_calls<'a>(
    body: &'a Body,
    all_names: &std::collections::HashSet<&'a str>,
    def_to_name: &HashMap<u32, &'a str>,
) -> (Vec<&'a str>, bool) {
    use gossamer_mir::{ConstValue, Operand, Terminator};
    let mut calls = Vec::new();
    let mut unresolved = false;
    // A closure reaches its body by address rather than through a call
    // terminator: the body is named in an operand and invoked indirectly. It
    // still has to travel into the same compile unit, or the module cannot
    // resolve the address at all.

    for block in &body.blocks {
        for stmt in &block.stmts {
            let gossamer_mir::StatementKind::Assign { rvalue, .. } = &stmt.kind else {
                continue;
            };
            match rvalue {
                gossamer_mir::Rvalue::Use(op) => {
                    calls.extend(referenced_body(op, all_names));
                }
                gossamer_mir::Rvalue::Aggregate { operands, .. } => {
                    for op in operands {
                        calls.extend(referenced_body(op, all_names));
                    }
                }
                gossamer_mir::Rvalue::CallIntrinsic { args, .. } => {
                    for op in args {
                        calls.extend(referenced_body(op, all_names));
                    }
                }
                _ => {}
            }
        }
        if let Terminator::Call { args, .. } = &block.terminator {
            for arg in args {
                calls.extend(referenced_body(arg, all_names));
            }
        }
        let Terminator::Call { callee, .. } = &block.terminator else {
            continue;
        };
        match callee {
            Operand::Const(ConstValue::Str(name)) if all_names.contains(name.as_str()) => {
                calls.push(name.as_str());
            }
            Operand::Const(ConstValue::Str(name)) => {
                let suffix = format!("::{name}");
                let mut matching_methods = all_names
                    .iter()
                    .copied()
                    .filter(|candidate| candidate.ends_with(&suffix));
                match (matching_methods.next(), matching_methods.next()) {
                    (Some(method), None) => calls.push(method),
                    (Some(_), Some(_)) => unresolved = true,
                    _ => {}
                }
            }
            Operand::FnRef { def, .. } => match def_to_name.get(&def.local) {
                Some(name) => calls.push(name),
                None => unresolved = true,
            },
            _ => {}
        }
    }
    (calls, unresolved)
}

/// Whether every `Iterator`-typed local in `body` is one the body itself
/// constructed through a `gos_rt_lazy_iter_*` call.
///
/// The bytecode tier and the runtime both spell a lazy iterator as one word
/// of the same type, but the words are not interchangeable: one is a registry
/// index into a thread-local map, the other a heap pointer. A handle the body
/// built is the runtime's; one that arrives as a parameter, from a global, or
/// as another body's return may be either, and native code reading the wrong
/// one dereferences an index.
fn body_builds_every_iterator_local(body: &Body, tcx: &TyCtxt) -> bool {
    use gossamer_mir::{ConstValue, Operand, StatementKind, Terminator};
    let is_iter_local = |local: gossamer_mir::Local| {
        matches!(tcx.kind_of(body.local_ty(local)), TyKind::Iterator(_))
    };
    // A parameter or the return slot is by definition not built here.
    for index in 0..=body.arity {
        if is_iter_local(gossamer_mir::Local(index)) {
            return false;
        }
    }
    let mut built: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut assigned: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() || !is_iter_local(place.local) {
                continue;
            }
            assigned.insert(place.local.0);
            // A copy of a handle the body already built carries the same
            // provenance; anything else is an unknown word.
            if let gossamer_mir::Rvalue::Use(Operand::Copy(source)) = rvalue
                && source.projection.is_empty()
                && built.contains(&source.local.0)
            {
                built.insert(place.local.0);
            }
        }
        if let Terminator::Call {
            callee,
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && is_iter_local(destination.local)
        {
            assigned.insert(destination.local.0);
            if matches!(
                callee,
                Operand::Const(ConstValue::Str(name)) if name.starts_with("gos_rt_lazy_iter_")
            ) {
                built.insert(destination.local.0);
            }
        }
    }
    assigned.iter().all(|local| built.contains(local))
}

/// `true` when `body` holds a local representation the JIT cannot lower
/// faithfully as part of a promoted region.
/// A sentinel `DefId` naming a runtime container - `Set` / `BTreeSet`,
/// `Deque` / `Queue` / `Stack`, `MaxHeap` / `MinHeap` - whose value is the
/// handle word itself.
fn is_bare_container_handle(def_local: u32) -> bool {
    // 46 is `sync::Shared`: a pointer the body only ever hands to a
    // `gos_rt_shared_*` call, so a local holding one lowers as that pointer.
    // 26 is `regex::Pattern`, which is the same shape: the body only ever
    // hands it to a `gos_rt_regex_*` call.
    matches!(
        u32::MAX - def_local,
        7 | 18 | 19 | 26 | 28 | 30 | 31 | 32 | 46
    )
}

fn body_uses_unlowerable_local_repr(
    body: &Body,
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> bool {
    // A reference to a parameter-typed local is an address into this body's
    // own frame. A generic template hands it to whichever impl the
    // instantiation selected, and that callee is a separate admission
    // decision - one that stays on bytecode reads the address from a frame it
    // does not own. Keep the whole body on bytecode so both sides of the call
    // agree on where the receiver lives.
    let borrows_a_parameter_local = body.blocks.iter().any(|block| {
        block.stmts.iter().any(|stmt| {
            matches!(
                &stmt.kind,
                gossamer_mir::StatementKind::Assign {
                    rvalue: gossamer_mir::Rvalue::Ref { place, .. },
                    ..
                } if place.projection.is_empty()
                    && body
                        .locals
                        .get(place.local.0 as usize)
                        .is_some_and(|l| matches!(tcx.kind_of(l.ty), TyKind::Param { .. }))
            )
        })
    });
    if borrows_a_parameter_local {
        if std::env::var("GOS_JIT_TRACE").is_ok() {
            eprintln!("jit: parameter-borrow {} stays on bytecode", body.name);
        }
        return true;
    }
    if !body_builds_every_iterator_local(body, tcx) {
        if std::env::var("GOS_JIT_TRACE").is_ok() {
            eprintln!(
                "jit: {} holds a lazy iterator it did not build; stays on bytecode",
                body.name
            );
        }
        return true;
    }
    body.locals.iter().enumerate().any(|(idx, l)| {
        let hit = match tcx.kind_of(l.ty) {
            TyKind::Int(gossamer_types::IntTy::I128 | gossamer_types::IntTy::U128) => true,
            TyKind::Adt { def, .. } => {
                if def.local == u32::MAX {
                    return false;
                }
                if def.local == u32::MAX - 1 {
                    return !jit_option_locals_ok();
                }
                if tcx.is_inline_enum_ty(l.ty) {
                    return true;
                }
                // A runtime container's handle is one machine word the body
                // only ever passes to a runtime call, so a local holding one
                // lowers as the pointer it is.
                if is_bare_container_handle(def.local) {
                    return false;
                }
                // Opaque stdlib-handle sentinels (the high def-id band) that
                // carry no marshalling shape stay on bytecode; ordinary user
                // structs / enums (small def-ids) are lowerable even when they
                // are not all-scalar, so they are admitted as internal JIT'd
                // locals.
                let is_sentinel = def.local >= u32::MAX - 64;
                is_sentinel
                    && !tcx.is_rc_managed(l.ty)
                    && ty_to_kind(tcx, l.ty, enum_shapes, struct_shapes).is_none()
            }
            _ => false,
        };
        if hit && std::env::var("GOS_JIT_TRACE").is_ok() {
            eprintln!(
                "jit: unlowerable-local {} local#{idx} kind={:?}",
                body.name,
                tcx.kind_of(l.ty)
            );
        }
        hit
    })
}

/// Computes the minimal set of body names needed in the JIT module.
///
/// Starts from bodies whose param/return types support JIT promotion
/// AND can amortize the fixed compiler/runtime resident cost across
/// repeated native entries: recursive helpers. Tiny straight-line helpers
/// called from bytecode do not gain enough from native compilation to pay
/// the boundary and compiler setup costs. Loops in frames that are already
/// running cannot switch to native code without OSR, so they stay on
/// bytecode. From recursive roots the set BFS-expands to every user body
/// they transitively call so intra-module call references resolve.
#[allow(
    clippy::too_many_lines,
    reason = "admission, dependency closure, and rejection propagation form one fixpoint"
)]
fn jit_compile_set<'a>(
    bodies: &'a [Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> std::collections::HashSet<&'a str> {
    let all_names: std::collections::HashSet<&str> =
        bodies.iter().map(|b| b.name.as_str()).collect();
    let body_map: HashMap<&str, &Body> = bodies.iter().map(|b| (b.name.as_str(), b)).collect();
    let def_to_name: HashMap<u32, &str> = bodies
        .iter()
        .filter_map(|b| b.def.map(|d| (d.local, b.name.as_str())))
        .collect();

    let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
    for b in bodies {
        let (calls, _) = body_user_calls(b, &all_names, &def_to_name);
        graph.insert(b.name.as_str(), calls);
    }
    let reaches_self = |start: &str| -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<&str> = graph.get(start).into_iter().flatten().copied().collect();
        while let Some(node) = stack.pop() {
            if node == start {
                return true;
            }
            if seen.insert(node)
                && let Some(succ) = graph.get(node)
            {
                stack.extend(succ.iter().copied());
            }
        }
        false
    };
    let loop_bodies: std::collections::HashSet<&str> = bodies
        .iter()
        .filter(|body| body_has_loop(body))
        .map(|body| body.name.as_str())
        .collect();
    let reaches_loop_body = |start: &str| -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<&str> = graph.get(start).into_iter().flatten().copied().collect();
        while let Some(node) = stack.pop() {
            if loop_bodies.contains(node) {
                return true;
            }
            if seen.insert(node)
                && let Some(succ) = graph.get(node)
            {
                stack.extend(succ.iter().copied());
            }
        }
        false
    };

    // Seed from recursive and loop-bearing bodies. Loop bodies are admitted
    // eagerly before their first invocation, including `main`, because they
    // cannot report backedge work or switch a live frame without OSR.
    // A body that is not itself hot but is reachable from one of those roots
    // is pulled in by the BFS below so intra-module call references resolve.
    let trace = std::env::var("GOS_JIT_TRACE").is_ok();
    let mut included: std::collections::HashSet<&str> = bodies
        .iter()
        .filter(|b| {
            let kinds_ok = body_kinds(b, tcx, enum_shapes, struct_shapes).is_some();
            let unlowerable_local =
                body_uses_unlowerable_local_repr(b, tcx, enum_shapes, struct_shapes);
            let goroutine_hit = body_has_cross_goroutine_ops(b);
            let unsupported = body_jit_unsupported(b, tcx);
            let recursive = reaches_self(b.name.as_str());
            // Recursive aggregate transforms are safe to promote: ownership
            // of their result is described by `compute_returns_fresh`, and
            // the VM/native boundary uses that bit when lifting the return.
            // The former blanket rejection kept JSON/tree transforms in
            // bytecode even when every recursive call was otherwise
            // lowerable.
            let recursive_rc_return = false;
            let amortizes = recursive || body_has_loop(b) || reaches_loop_body(b.name.as_str());
            if trace
                && (!kinds_ok
                    || unlowerable_local
                    || goroutine_hit
                    || unsupported
                    || recursive_rc_return)
            {
                eprintln!(
                    "jit: seed-reject {} (kinds_ok={kinds_ok} \
                     unlowerable_local={unlowerable_local} \
                     goroutine={goroutine_hit} \
                     unsupported={unsupported} \
                     recursive_rc_return={recursive_rc_return})",
                    b.name
                );
            }
            kinds_ok
                && !unlowerable_local
                && !goroutine_hit
                && !unsupported
                && !recursive_rc_return
                && amortizes
        })
        .map(|b| b.name.as_str())
        .collect();

    let mut worklist: Vec<&str> = included.iter().copied().collect();
    while let Some(name) = worklist.pop() {
        let Some(body) = body_map.get(name) else {
            continue;
        };
        let (calls, _) = body_user_calls(body, &all_names, &def_to_name);
        for callee in calls {
            let ok = body_map.get(callee).is_some_and(|b| {
                // A dependency pulled into the same native module never
                // crosses the VM trampoline. Its parameters and return may
                // therefore use the native aggregate ABI even when `JitKind`
                // cannot marshal that shape at a VM entry. Requiring
                // `body_kinds` here rejected callers such as n-body's hot
                // `main`, solely because its internal `energy(&[Body; 5])`
                // helper accepts a fixed-array reference.
                !body_uses_unlowerable_local_repr(b, tcx, enum_shapes, struct_shapes)
                    && !body_has_cross_goroutine_ops(b)
                    && !body_jit_unsupported(b, tcx)
            });
            if ok && included.insert(callee) {
                worklist.push(callee);
            }
        }
    }
    // Exclude bodies the lowering would hard-fail on (an FnRef to a
    // def with no MIR body), and propagate: a caller of an excluded
    // body is itself uncompilable. One bad body must not fail the
    // whole module.
    loop {
        let mut removed = false;
        let snapshot: Vec<&str> = included.iter().copied().collect();
        for name in snapshot {
            let Some(body) = body_map.get(name) else {
                continue;
            };
            let (calls, unresolved) = body_user_calls(body, &all_names, &def_to_name);
            let calls_excluded = calls.iter().any(|c| !included.contains(c));
            if unresolved || calls_excluded {
                if trace {
                    eprintln!(
                        "jit: propagate-reject {name} (unresolved={unresolved} \
                         excluded_callee={calls_excluded})"
                    );
                }
                included.remove(name);
                removed = true;
            }
        }
        if !removed {
            break;
        }
    }
    included
}

/// `true` when at least one body is worth promoting to native code:
/// it is JIT-promotable (scalar/enum-pointer signature) AND does
/// substantial work per cross-boundary call because it recurses or contains
/// a loop. The
/// in-process JIT's only speedup is eliding per-call
/// bytecode dispatch, which the VM<->native boundary marshalling
/// cancels for a tiny straight-line leaf called from bytecode. A program
/// with no worthy body gains nothing from preparing the native compiler,
/// so the interpreter consults this before invoking
/// [`compile_to_jit_for_promotion`] and stays on bytecode when it is
/// `false`. `compile_to_jit` itself stays unfiltered so its
/// compile-correctness is independently testable.
#[allow(
    clippy::implicit_hasher,
    reason = "single interp caller passes the same HashMap shape compile_to_jit uses"
)]
#[must_use]
pub fn has_worthy_jit_body(
    bodies: &[Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> bool {
    let mut compile_set = jit_compile_set(bodies, tcx, enum_shapes, struct_shapes);
    restrict_static_leaky_bodies(&mut compile_set, bodies);
    !compile_set.is_empty()
}

/// Returns the exact transitive body set retained for deferred promotion.
/// The interpreter uses this before storing its compiler snapshot so bodies
/// that are guaranteed to stay on bytecode do not remain resident until the
/// tier-up threshold fires.
#[allow(
    clippy::implicit_hasher,
    reason = "single interp caller passes the same HashMap shape compile_to_jit uses"
)]
#[must_use]
pub fn jit_compile_body_names(
    bodies: &[Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> std::collections::HashSet<String> {
    let mut compile_set = jit_compile_set(bodies, tcx, enum_shapes, struct_shapes);
    restrict_static_leaky_bodies(&mut compile_set, bodies);
    compile_set.into_iter().map(str::to_string).collect()
}

/// Returns admitted bodies that can amortize a VM-to-native entry. Other
/// bodies may still be compiled as link dependencies, but exposing those as
/// VM overrides adds boundary cost to tiny helpers such as random-number
/// steps called from an interpreted loop.
#[allow(
    clippy::implicit_hasher,
    reason = "single interp caller passes the same HashMap shape compile_to_jit uses"
)]
#[must_use]
pub fn jit_entry_body_names(
    bodies: &[Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> std::collections::HashSet<String> {
    let admitted = jit_compile_body_names(bodies, tcx, enum_shapes, struct_shapes);
    jit_entry_body_names_with_admitted(bodies, &admitted)
}

/// [`jit_entry_body_names`] for a caller that already computed the admitted
/// set. The admission analysis walks the whole call graph, so a tier-up that
/// needs both sets derives it once and passes it here.
#[allow(
    clippy::implicit_hasher,
    reason = "single interp caller passes the set jit_compile_body_names returns"
)]
#[must_use]
pub fn jit_entry_body_names_with_admitted(
    bodies: &[Body],
    admitted: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let all_names: std::collections::HashSet<&str> =
        bodies.iter().map(|body| body.name.as_str()).collect();
    let def_to_name: HashMap<u32, &str> = bodies
        .iter()
        .filter_map(|body| body.def.map(|def| (def.local, body.name.as_str())))
        .collect();
    let graph: HashMap<&str, Vec<&str>> = bodies
        .iter()
        .map(|body| {
            let (calls, _) = body_user_calls(body, &all_names, &def_to_name);
            (body.name.as_str(), calls)
        })
        .collect();
    let recursive = |start: &str| {
        let mut seen = std::collections::HashSet::new();
        let mut pending: Vec<&str> = graph.get(start).into_iter().flatten().copied().collect();
        while let Some(name) = pending.pop() {
            if name == start {
                return true;
            }
            if seen.insert(name)
                && let Some(callees) = graph.get(name)
            {
                pending.extend(callees.iter().copied());
            }
        }
        false
    };
    let reaches_admitted_hot_body = |start: &str| {
        let mut seen = std::collections::HashSet::new();
        let mut pending: Vec<&str> = graph.get(start).into_iter().flatten().copied().collect();
        while let Some(name) = pending.pop() {
            let is_hot = bodies
                .iter()
                .find(|body| body.name == name)
                .is_some_and(|body| body_has_loop(body) || recursive(name));
            if admitted.contains(name) && is_hot {
                return true;
            }
            if seen.insert(name)
                && let Some(callees) = graph.get(name)
            {
                pending.extend(callees.iter().copied());
            }
        }
        false
    };
    bodies
        .iter()
        .filter(|body| {
            let entry_compatible = admitted.contains(body.name.as_str());
            entry_compatible
                && (body_has_loop(body)
                    || recursive(body.name.as_str())
                    || reaches_admitted_hot_body(body.name.as_str()))
        })
        .map(|body| body.name.clone())
        .collect()
}

/// Returns the hot recursive SCC containing `trigger` plus the minimum
/// outbound user-body dependency closure required to link it. Unrelated hot
/// roots remain available for a later artifact instead of being pulled into
/// the first compilation.
#[allow(
    clippy::implicit_hasher,
    reason = "single interp caller passes the same HashMap shape compile_to_jit uses"
)]
#[must_use]
pub fn jit_compile_body_names_for_trigger(
    bodies: &[Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
    trigger: &str,
) -> std::collections::HashSet<String> {
    fn reachable<'a>(
        graph: &HashMap<&'a str, Vec<&'a str>>,
        start: &'a str,
    ) -> std::collections::HashSet<&'a str> {
        let mut found = std::collections::HashSet::new();
        let mut pending = vec![start];
        while let Some(name) = pending.pop() {
            if found.insert(name)
                && let Some(callees) = graph.get(name)
            {
                pending.extend(callees.iter().copied());
            }
        }
        found
    }

    let admitted = jit_compile_body_names(bodies, tcx, enum_shapes, struct_shapes);
    if !admitted.contains(trigger) {
        return std::collections::HashSet::new();
    }
    let all_names: std::collections::HashSet<&str> =
        bodies.iter().map(|body| body.name.as_str()).collect();
    let def_to_name: HashMap<u32, &str> = bodies
        .iter()
        .filter_map(|body| body.def.map(|def| (def.local, body.name.as_str())))
        .collect();
    let graph: HashMap<&str, Vec<&str>> = bodies
        .iter()
        .map(|body| {
            let (calls, _) = body_user_calls(body, &all_names, &def_to_name);
            (body.name.as_str(), calls)
        })
        .collect();

    let from_trigger = reachable(&graph, trigger);
    let scc: std::collections::HashSet<&str> = from_trigger
        .iter()
        .copied()
        .filter(|candidate| reachable(&graph, candidate).contains(trigger))
        .collect();
    let mut selected = scc;
    let mut pending: Vec<&str> = selected.iter().copied().collect();
    while let Some(name) = pending.pop() {
        if let Some(callees) = graph.get(name) {
            for &callee in callees {
                if admitted.contains(callee) && selected.insert(callee) {
                    pending.push(callee);
                }
            }
        }
    }
    selected.into_iter().map(str::to_owned).collect()
}

/// Explains static JIT admission for every body in stable name order without
/// constructing a Cranelift module.
#[allow(
    clippy::implicit_hasher,
    reason = "single interp caller passes the same HashMap shape compile_to_jit uses"
)]
#[must_use]
pub fn jit_promotion_report(
    bodies: &[Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> Vec<JitBodyDecision> {
    let admitted = jit_compile_body_names(bodies, tcx, enum_shapes, struct_shapes);
    let mut report: Vec<_> = bodies
        .iter()
        .map(|body| {
            let is_admitted = admitted.contains(body.name.as_str());
            let mut reasons = Vec::new();
            if !is_admitted {
                if body_kinds(body, tcx, enum_shapes, struct_shapes).is_none() {
                    reasons.push("unsupported-boundary");
                }
                if body_uses_unlowerable_local_repr(body, tcx, enum_shapes, struct_shapes) {
                    reasons.push("unsupported-local-representation");
                }
                if body_has_cross_goroutine_ops(body) {
                    reasons.push("cross-goroutine-state");
                }
                if body_jit_unsupported(body, tcx) {
                    reasons.push("unsupported-operation");
                }
                if reasons.is_empty() {
                    reasons.push("not-hot-or-not-in-promotable-closure");
                }
            }
            reasons.sort_unstable();
            reasons.dedup();
            JitBodyDecision {
                name: body.name.clone(),
                admitted: is_admitted,
                reasons,
            }
        })
        .collect();
    report.sort_by(|left, right| left.name.cmp(&right.name));
    report
}

/// Names of admitted loop-bearing bodies that must compile before entry
/// because an already-running bytecode frame cannot switch tiers without OSR.
#[allow(
    clippy::implicit_hasher,
    reason = "single interp caller passes the same HashMap shape compile_to_jit uses"
)]
#[must_use]
pub fn jit_eager_loop_bodies(
    bodies: &[Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> Vec<String> {
    let mut compile_set = jit_compile_set(bodies, tcx, enum_shapes, struct_shapes);
    restrict_static_leaky_bodies(&mut compile_set, bodies);
    bodies
        .iter()
        .filter(|body| body_has_loop(body) && compile_set.contains(body.name.as_str()))
        .map(|body| body.name.clone())
        .collect()
}

fn body_has_loop(body: &Body) -> bool {
    use gossamer_mir::Terminator;
    fn successors(term: &Terminator, out: &mut Vec<usize>) {
        out.clear();
        match term {
            Terminator::Goto { target } => out.push(target.0 as usize),
            Terminator::SwitchInt { arms, default, .. } => {
                for (_, b) in arms {
                    out.push(b.0 as usize);
                }
                out.push(default.0 as usize);
            }
            Terminator::Call {
                target: Some(b), ..
            } => out.push(b.0 as usize),
            Terminator::Assert { target, .. } | Terminator::Drop { target, .. } => {
                out.push(target.0 as usize);
            }
            Terminator::Return
            | Terminator::Unreachable
            | Terminator::Panic { .. }
            | Terminator::Call { target: None, .. } => {}
        }
    }
    let n = body.blocks.len();
    let mut color = vec![0u8; n]; // 0 = white, 1 = grey, 2 = black
    let mut succ = Vec::new();
    for start in 0..n {
        if color[start] != 0 {
            continue;
        }
        color[start] = 1;
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(node, idx)) = stack.last() {
            successors(&body.blocks[node].terminator, &mut succ);
            if idx < succ.len() {
                stack.last_mut().expect("nonempty stack").1 += 1;
                let next = succ[idx];
                if next >= n {
                    continue;
                }
                match color[next] {
                    0 => {
                        color[next] = 1;
                        stack.push((next, 0));
                    }
                    1 => return true,
                    _ => {}
                }
            } else {
                color[node] = 2;
                stack.pop();
            }
        }
    }
    false
}

/// Every `static mut` symbol read or written by `body`.
fn body_static_symbols(body: &Body) -> Vec<&str> {
    use gossamer_mir::{Rvalue, StatementKind};
    let mut out: Vec<&str> = Vec::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign {
                    rvalue: Rvalue::StaticLoad(sref),
                    ..
                } => out.push(sref.symbol.as_str()),
                StatementKind::StaticStore { target, .. } => out.push(target.symbol.as_str()),
                _ => {}
            }
        }
    }
    out
}

/// Removes from `compile_set` every body that accesses a `static mut` shared
/// with a body outside the set. A JIT-compiled body reads and writes the
/// static's native backing cell; a body left on the VM reads and writes the
/// VM's separate `Global::MutStatic` cell. Correctness requires every accessor
/// of a static to sit on the same side of the tier boundary, so if any accessor
/// is not compiled, none of them are - they all fall back to the VM's one
/// shared cell. Iterated to a fixpoint because dropping one accessor can strand
/// another static's last remaining compiled accessor.
fn restrict_static_leaky_bodies<'a>(
    compile_set: &mut std::collections::HashSet<&'a str>,
    bodies: &'a [Body],
) {
    let mut accessors: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
    for b in bodies {
        for sym in body_static_symbols(b) {
            accessors.entry(sym).or_default().push(b.name.as_str());
        }
    }
    if accessors.is_empty() {
        return;
    }
    loop {
        let mut to_remove: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for bods in accessors.values() {
            if !bods.iter().all(|n| compile_set.contains(n)) {
                for n in bods {
                    if compile_set.contains(n) {
                        to_remove.insert(*n);
                    }
                }
            }
        }
        if to_remove.is_empty() {
            break;
        }
        for n in to_remove {
            compile_set.remove(n);
        }
    }
}

/// Compiles every body in `bodies` through cranelift-jit and returns
/// the resulting handle table. This is the low-level compiler entry
/// point used by tests and diagnostics: it deliberately does not apply
/// the VM's promotion cost model, so simple straight-line functions
/// still exercise the backend.
///
/// Functions whose codegen path errors, or whose ABI shape is not
/// supported by the dispatch trampoline, are silently skipped - the VM's
/// existing bytecode dispatch picks them up.
#[allow(
    clippy::implicit_hasher,
    reason = "single internal caller; generalizing the hasher adds a type parameter for nothing"
)]
pub fn compile_to_jit(
    bodies: &[Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> Result<JitArtifact> {
    if bodies.is_empty() {
        report_jit_stats(0);
        return Ok(JitArtifact {
            heap: None,
            functions: HashMap::new(),
            code_bytes: 0,
            cacheable: true,
        });
    }
    compile_bodies(bodies, tcx, enum_shapes, struct_shapes)
}

/// Compiles only the bodies the bytecode VM should promote at runtime.
///
/// This entry point applies the JIT RAM cost model before constructing a
/// Cranelift module. Short programs and once-entered loops that cannot
/// amortize Cranelift's fixed resident footprint return an empty
/// artifact without instantiating the JIT backend.
#[allow(
    clippy::implicit_hasher,
    reason = "single internal caller; generalizing the hasher adds a type parameter for nothing"
)]
pub fn compile_to_jit_for_promotion(
    bodies: &[Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> Result<JitArtifact> {
    compile_to_jit_for_promotion_owned(bodies.to_vec(), tcx, enum_shapes, struct_shapes)
}

/// Ownership-taking promotion path. This avoids cloning the retained MIR
/// snapshot when the compiling VM is its sole owner.
#[allow(
    clippy::implicit_hasher,
    reason = "single interp caller passes the same HashMap shape compile_to_jit uses"
)]
pub fn compile_to_jit_for_promotion_owned(
    mut bodies: Vec<Body>,
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> Result<JitArtifact> {
    // Decide the compile set BEFORE touching Cranelift. When no body is
    // worth promoting, return an empty artifact without instantiating the JIT
    // module. That avoids preparing the native compiler for programs that
    // would stay on bytecode either way.
    let mut compile_set = jit_compile_set(&bodies, tcx, enum_shapes, struct_shapes);
    // Keep every accessor of a `static mut` on the same tier: a compiled body
    // uses the static's native cell, a VM body its `Global::MutStatic` cell.
    restrict_static_leaky_bodies(&mut compile_set, &bodies);
    if compile_set.is_empty() {
        report_jit_stats(0);
        return Ok(JitArtifact {
            heap: None,
            functions: HashMap::new(),
            code_bytes: 0,
            cacheable: true,
        });
    }

    // Pre-filter: only compile bodies reachable from JIT-promotable roots.
    // Bodies whose param/return types can't be marshalled through the
    // trampoline (aggregates, closures) will never be promoted - compiling
    // them wastes Cranelift IR capacity. The BFS in
    // `jit_compile_set` already found the transitive closure of user-function
    // calls from the promotable roots so inter-body calls resolve. Clone only
    // the bodies we'll actually compile.
    let compile_names: std::collections::HashSet<String> =
        compile_set.into_iter().map(str::to_string).collect();
    bodies.retain(|body| compile_names.contains(body.name.as_str()));
    let filtered = bodies;

    compile_bodies_dropping_failures(filtered, tcx, enum_shapes, struct_shapes)
}

/// Compiles `bodies`, dropping any single body whose lowering fails and
/// retrying with the rest. A body the codegen cannot lower runs on the
/// bytecode VM, which is the reference semantics either way; abandoning the
/// whole module instead would take every unrelated hot body down with it.
fn compile_bodies_dropping_failures(
    mut bodies: Vec<Body>,
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> Result<JitArtifact> {
    let trace = std::env::var("GOS_JIT_TRACE").is_ok();
    loop {
        let err = match compile_bodies(&bodies, tcx, enum_shapes, struct_shapes) {
            Ok(artifact) => return Ok(artifact),
            Err(err) => err,
        };
        // The failure names its body when it came from IR construction or
        // Cranelift compilation; anything else is module-wide and retrying
        // would only repeat it.
        let Some(failed) = err
            .downcast_ref::<FailedBody>()
            .map(|FailedBody(name)| name.clone())
        else {
            return Err(err);
        };
        if trace {
            eprintln!("jit: module build failed ({err:#}); retrying without {failed}");
        }
        // Dropping a body sends it to the VM; re-run the static-mut
        // connectivity check so no compiled body is left sharing a static
        // with a now-interpreted one.
        let mut retry_set: std::collections::HashSet<&str> = bodies
            .iter()
            .map(|body| body.name.as_str())
            .filter(|name| *name != failed)
            .collect();
        if retry_set.len() == bodies.len() {
            return Err(err);
        }
        restrict_static_leaky_bodies(&mut retry_set, &bodies);
        let retry_names: std::collections::HashSet<String> =
            retry_set.into_iter().map(str::to_string).collect();
        bodies.retain(|body| retry_names.contains(body.name.as_str()));
        if bodies.is_empty() {
            return Ok(JitArtifact {
                heap: None,
                functions: HashMap::new(),
                code_bytes: 0,
                cacheable: true,
            });
        }
    }
}

/// Builds one finalised cranelift [`JitArtifact`] from an already-filtered
/// body set. Separated from [`compile_to_jit`] so a whole-module lowering
/// failure (an un-lowerable `main`) can be retried against a reduced set
/// rather than switching the JIT off for every body.
#[allow(
    clippy::implicit_hasher,
    reason = "single internal caller; generalizing the hasher adds a type parameter for nothing"
)]
fn compile_bodies(
    filtered: &[Body],
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> Result<JitArtifact> {
    let isa = build_native_isa(false)?;
    let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let heap = NativeCodeHeap::new();
    builder.memory_provider(Box::new(NativeCodeHeap::provider(&heap)));
    let mut runtime_symbol_set = register_runtime_symbols(&mut builder);
    // Register every `gos_binding_<...>` C-ABI thunk advertised by
    // the binding crates. Without these, `JITModule::finalize_definitions`
    // panics with `can't resolve symbol gos_binding_<...>` because
    // the default libloading-based resolver only sees the dynamic
    // symbol table, and Cargo binaries don't expose statically-linked
    // `pub extern "C"` symbols there.
    let leaked_binding_names = register_binding_symbols(&mut builder);
    runtime_symbol_set.extend(leaked_binding_names);
    let mut module = JITModule::new(builder);

    // Rename the user's `main` to `gos_main` in the JIT's symbol
    // table. The host binary already exports `main` (the Rust
    // runtime's entry point); declaring a second `Linkage::Local`
    // `main` produced flaky SIGILLs on bring-up. The lookup map
    // we hand back to the VM keeps the original Gossamer name as
    // the key, so dispatch is unaffected.
    let lowered = lower_program_serial(&mut module, filtered, tcx, Some("gos_main"))?;

    // A `Result<T, _>`-returning body hands its `[disc, payload]` carrier
    // back as an `i128` by value. The Rust trampoline reads that return from
    // a register the Windows x64 ABI disagrees with, so wrap each such body
    // in an out-pointer thunk (Cranelift-to-Cranelift call, then a pointer
    // store) and dispatch through the thunk instead. `carrier_thunks` maps
    // the body name to its thunk's `FuncId`.
    let mut carrier_thunks: HashMap<String, cranelift_module::FuncId> = HashMap::new();
    for body in filtered {
        if !matches!(
            body_kinds(body, tcx, enum_shapes, struct_shapes),
            Some((
                _,
                JitKind::ResultEnumPtr(_)
                    | JitKind::ResultNativeStr
                    | JitKind::ResultScalar(_)
                    | JitKind::OptionScalar(_)
            ))
        ) {
            continue;
        }
        let Some(&body_id) = lowered.function_ids_by_name.get(&body.name) else {
            continue;
        };
        let thunk_id = crate::native::emit_carrier_outptr_thunk(&mut module, body_id, &body.name)?;
        carrier_thunks.insert(body.name.clone(), thunk_id);
    }

    module
        .finalize_definitions()
        .map_err(|e| anyhow!("jit finalize: {e}"))?;

    let body_name_set: std::collections::HashSet<&str> =
        filtered.iter().map(|b| b.name.as_str()).collect();
    let returns_fresh = compute_returns_fresh(filtered, tcx);
    let trace = std::env::var("GOS_JIT_TRACE").is_ok();
    let mut functions = HashMap::new();
    for body in filtered {
        let Some(id) = lowered.function_ids_by_name.get(&body.name).copied() else {
            if trace {
                eprintln!("jit: entry-skip {} (not lowered)", body.name);
            }
            continue;
        };
        let Some((params, returns)) = body_kinds(body, tcx, enum_shapes, struct_shapes) else {
            // Some param/return type isn't a primitive scalar - the
            // dispatch trampoline can't marshal it, so the VM will
            // fall back to bytecode for this fn.
            if trace {
                eprintln!("jit: entry-skip {} (unsupported boundary)", body.name);
            }
            continue;
        };
        if body_calls_jit_unsafe(body, &runtime_symbol_set, &body_name_set) {
            // Body invokes something cranelift would lower as the
            // "soft-zero stub" at native.rs (~line 2099): unknown
            // by-name calls that aren't in the runtime symbol table
            // and aren't user-defined bodies. The stub silently
            // zeroes the destination, scrambling the program state.
            // Skip the JIT entry so the bytecode VM keeps semantics
            // intact for this function. The most common offenders
            // are closure-callback methods (`sort_by`, `sort_by_key`,
            // `map`, `filter`) plus any other user-facing helper
            // wired in the interpreter but not yet in the codegen
            // dispatch table.
            if trace {
                eprintln!("jit: entry-skip {} (unsafe call)", body.name);
            }
            continue;
        }
        if body_jit_unsupported(body, tcx) {
            // Uses a `&mut` param or passes a closure to a higher-order
            // call - shapes the trampoline / codegen mishandle. Keep the
            // body on bytecode so its semantics stay correct.
            if trace {
                eprintln!("jit: entry-skip {} (unsupported operation)", body.name);
            }
            continue;
        }
        // A `ResultEnumPtr` body dispatches through its out-pointer carrier
        // thunk (the trampoline passes a stack buffer and reads the carrier
        // back from memory); every other body is called directly.
        let ptr = match carrier_thunks.get(&body.name) {
            Some(&thunk_id) => module.get_finalized_function(thunk_id),
            None => module.get_finalized_function(id),
        };
        #[allow(
            clippy::arc_with_non_send_sync,
            reason = "JIT pointers remain thread-confined; Arc shares immutable metadata with the VM override map"
        )]
        let name: std::sync::Arc<str> = body.name.as_str().into();
        #[allow(
            clippy::arc_with_non_send_sync,
            reason = "JIT pointers remain thread-confined; Arc shares immutable metadata with the VM override map"
        )]
        let handle = std::sync::Arc::new(JitFn {
            name: std::sync::Arc::clone(&name),
            ptr,
            params: params.into_boxed_slice(),
            returns,
            returns_fresh: returns_fresh.get(&body.name).copied().unwrap_or(false),
        });
        functions.insert(name, handle);
    }

    // Ordinary `JITModule` drop releases declarations, symbol maps, the ISA,
    // compiled-blob relocation metadata, and the provider adapter. It does
    // not call `free_memory`; the artifact's shared heap owns those mappings.
    drop(module);
    heap.mark_detached();

    report_jit_stats(functions.len());
    Ok(JitArtifact {
        heap: Some(heap),
        functions,
        code_bytes: lowered.emitted_code_bytes,
        cacheable: !filtered
            .iter()
            .any(|body| !body_static_symbols(body).is_empty()),
    })
}

/// Option uses the same two-word discriminant/payload carrier already handled
/// for inline variants. Keeping it admitted avoids rejecting an otherwise
/// scalar loop merely because one local represents `Some` or `None`.
const fn jit_option_locals_ok() -> bool {
    true
}

/// Splits a comma-separated `GOS_JIT_ONLY` / `GOS_JIT_SKIP` value into a
/// set of trimmed, non-empty function names.
/// Returns `true` when `body` contains a `Call(Const(Str(name)))`
/// whose `name` cranelift would lower as the "soft-zero stub"
/// (native.rs ~line 2099) - i.e. neither a registered runtime
/// symbol nor a user-defined body name nor a recognised
/// variant-constructor / qualified-path shape. The stub silently
/// zeroes the destination, so JIT-promoting such a body would
/// corrupt every program that exercises that call.
///
/// Only names with a proven lowering are accepted. String shape is not a
/// callable registry: qualified and capitalized unknowns are hard errors in
/// native lowering and must be rejected here too.
fn body_calls_jit_unsafe(
    body: &Body,
    runtime_symbols: &std::collections::HashSet<&'static str>,
    body_names: &std::collections::HashSet<&str>,
) -> bool {
    use gossamer_mir::{ConstValue, Operand, Terminator};
    for block in &body.blocks {
        let Terminator::Call { callee, .. } = &block.terminator else {
            continue;
        };
        let Operand::Const(ConstValue::Str(name)) = callee else {
            continue;
        };
        let n = name.as_str();
        if runtime_symbols.contains(n) {
            continue;
        }
        if body_names.contains(n) {
            continue;
        }
        // Bare prelude I/O intrinsics the cranelift backend lowers directly
        // (`intrinsic_io_math.rs`): the `println!` family and the format-prec
        // helper `__fmt_prec` (also `__`-prefixed below). They are NOT registered
        // runtime symbols, so without this a top-level `main` that ends in a
        // `println!` would be judged unsafe and never promote. `panic` is
        // deliberately excluded - panicking bodies stay on bytecode so the VM
        // renders the call-stack trace (the interp gates them separately).
        if matches!(n, "println" | "print" | "eprintln" | "eprint") {
            continue;
        }
        if matches!(
            n,
            "math::sqrt"
                | "math::sin"
                | "math::cos"
                | "math::ln"
                | "math::log"
                | "math::exp"
                | "math::abs"
                | "math::floor"
                | "math::ceil"
        ) {
            continue;
        }
        // Variant constructors and qualified stdlib/core constructors have
        // dedicated native lowering arms. Do not reject hot bodies just because
        // they allocate a local Vec/U8Vec before entering the loop.
        let starts_uppercase = n.chars().next().is_some_and(char::is_uppercase);
        if matches!(n, "Ok" | "Err" | "Some" | "None") || starts_uppercase || n.contains("::") {
            continue;
        }
        // Compiler-internal intrinsics always have a dedicated cranelift
        // lowering arm, so a body calling one is JIT-safe: the
        // double-underscore family (`__concat` for `format!`) and the
        // `gos_`-namespaced aggregate / enum / alloc intrinsics
        // (`gos_load`, `gos_store`, `gos_enum_*`, `gos_rc_alloc*`,
        // `gos_alloc`, `gos_fn_addr`) - emitted as terminator calls when an
        // aggregate load/store feeds a successor block. (`gos_rt_*` runtime
        // shims are already covered by `runtime_symbols` above.)
        if n.starts_with("__") || n.starts_with("gos_") {
            continue;
        }
        if std::env::var("GOS_JIT_TRACE").is_ok() {
            eprintln!("jit: unsafe call in {}: {n}", body.name);
        }
        return true;
    }
    false
}

/// Whether a value of `ty` carries a `Vec` or slice of a payload-bearing
/// enum, directly or through an aggregate it holds. Such a value cannot be
/// handed back from a native body until its lift path retains each element.
fn ty_carries_payload_enum_vec(tcx: &TyCtxt, ty: Ty) -> bool {
    fn walk(tcx: &TyCtxt, ty: Ty, seen: &mut Vec<Ty>) -> bool {
        if seen.contains(&ty) {
            return false;
        }
        seen.push(ty);
        match tcx.kind_of(ty) {
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => {
                tcx.is_payload_enum(*elem) || walk(tcx, *elem, seen)
            }
            TyKind::Tuple(elems) => elems.iter().any(|elem| walk(tcx, *elem, seen)),
            TyKind::HashMap { key, value, .. } => walk(tcx, *key, seen) || walk(tcx, *value, seen),
            TyKind::Adt { def, substs } => {
                let def = *def;
                // `Option<T>` / `Result<T, E>` carry their payload in the
                // substitution; a user aggregate carries it in its fields.
                let args = substs.types();
                if args.iter().any(|arg| walk(tcx, *arg, seen)) {
                    return true;
                }
                if let Some(fields) = tcx.struct_field_tys(def)
                    && fields.to_vec().iter().any(|f| walk(tcx, *f, seen))
                {
                    return true;
                }
                tcx.enum_variant_tys(def).is_some_and(|variants| {
                    variants
                        .to_vec()
                        .iter()
                        .any(|fields| fields.iter().any(|f| walk(tcx, *f, seen)))
                })
            }
            TyKind::Ref { inner, .. } => walk(tcx, *inner, seen),
            TyKind::Nominal { repr, .. } => walk(tcx, *repr, seen),
            _ => false,
        }
    }
    walk(tcx, ty, &mut Vec::new())
}

/// Returns `true` when `body` uses a construct the JIT lowers incorrectly,
/// so the VM must keep it on the bytecode interpreter:
///
/// - A `&mut` parameter. The dispatch trampoline marshals aggregate
///   arguments (`String`, `Vec<i64>`, ...) by value - a fresh runtime
///   object reclaimed after the call - so a write through the reference
///   never reaches the caller, and the copy-back of an in-place
///   `String`/`Vec` append corrupts the heap (a segfault).
/// - A goroutine-spawn site or a cross-goroutine sync primitive
///   (channel / `WaitGroup` / Mutex / Atomic / ... - see
///   [`is_cross_goroutine_rt`]). Under `gos` the spawned side runs
///   on the VM scheduler against the interpreter's own handle
///   registries; a native body would mint and wait on the *runtime*
///   registries instead, so the two sides never observe each other
///   (a `wg.wait()` deadlock). Such bodies stay on bytecode.
#[allow(
    clippy::too_many_lines,
    reason = "keeping every native-admission safety check in one audit point is deliberate"
)]
fn body_jit_unsupported(body: &Body, tcx: &TyCtxt) -> bool {
    use gossamer_mir::{ConstValue, Operand, Rvalue, StatementKind, Terminator};
    use gossamer_types::Mutbl;

    if body_has_oversized_fixed_aggregate(body, tcx) {
        if std::env::var("GOS_JIT_TRACE").is_ok() {
            eprintln!("jit: unsupported {} oversized fixed aggregate", body.name);
        }
        return true;
    }
    for idx in 0..body.locals.len() {
        let lty =
            tcx.kind_of(body.local_ty(gossamer_mir::Local(u32::try_from(idx).unwrap_or(u32::MAX))));
        if jit_local_ty_needs_bytecode(
            tcx,
            body.local_ty(gossamer_mir::Local(u32::try_from(idx).unwrap_or(u32::MAX))),
        ) {
            if std::env::var("GOS_JIT_TRACE").is_ok() {
                eprintln!(
                    "jit: unsupported {} local#{idx} bytecode-only {}",
                    body.name,
                    describe_jit_local_ty(tcx, lty)
                );
            }
            return true;
        }
        // A `Vec`/`[T]` whose element is a payload-bearing enum is safe to
        // read, but handing one back needs native-to-VM element ownership
        // transfer. Keep that shape on bytecode until its lift path can retain
        // every vector element before the native aggregate is released. What
        // matters is the value that crosses back, so a body that only reads
        // such a vector - a serializer, a derived `fmt` - is admitted.
        if let TyKind::Vec(elem) | TyKind::Slice(elem) = lty
            && tcx.is_payload_enum(*elem)
            && ty_carries_payload_enum_vec(tcx, body.local_ty(gossamer_mir::Local::RETURN))
        {
            if std::env::var("GOS_JIT_TRACE").is_ok() {
                eprintln!(
                    "jit: unsupported {} local#{idx} payload-enum vec return kind={lty:?}",
                    body.name
                );
            }
            return true;
        }
    }
    // A `&mut Vec` / `&mut Slice` / `&mut HashMap` parameter. The trampoline
    // marshals aggregates by value (the content pointer), but such a body
    // expects a pointer to the caller's slot to write through; the
    // value-marshalled pointer is the wrong shape and the in-place append
    // corrupts the heap. Keep these on bytecode. `&mut String` is the one
    // write-through shape the trampoline does handle (a pointer-to-slot cell
    // read back after the call - see `invoke_prepared_native`), so it is
    // admitted.
    for pidx in 1..=body.arity {
        if let TyKind::Ref {
            mutability: Mutbl::Mut,
            inner,
        } = tcx.kind_of(body.local_ty(gossamer_mir::Local(pidx)))
            && matches!(
                tcx.kind_of(*inner),
                TyKind::Vec(_) | TyKind::Slice(_) | TyKind::HashMap { .. }
            )
        {
            if std::env::var("GOS_JIT_TRACE").is_ok() {
                eprintln!(
                    "jit: unsupported {} param#{pidx} mutable aggregate ref",
                    body.name
                );
            }
            return true;
        }
    }
    if body_has_cross_goroutine_ops(body) {
        if std::env::var("GOS_JIT_TRACE").is_ok() {
            eprintln!("jit: unsupported {} cross-goroutine op", body.name);
        }
        return true;
    }
    let has_fn_arg = |args: &[Operand]| args.iter().any(|a| matches!(a, Operand::FnRef { .. }));
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &stmt.kind
            {
                if *name == "gos_jit_unsupported_user_iterator" || has_fn_arg(args) {
                    return true;
                }
            }
        }
        if let Terminator::Call { callee, args, .. } = &block.terminator {
            if has_fn_arg(args) {
                if std::env::var("GOS_JIT_TRACE").is_ok() {
                    eprintln!("jit: unsupported {} function argument call", body.name);
                }
                return true;
            }
            // Appending into a `&mut String` PARAMETER writes through a
            // reference the caller still holds, so the runtime cannot take
            // the accumulator's buffer and each append copies the prefix -
            // linear work per step, quadratic over a builder loop. An
            // accumulator that is the body's own local has no second holder
            // and grows in place, which is what the self-consuming append
            // lowering builds.
            if matches!(callee, Operand::Const(ConstValue::Str(name)) if name == "gos_rt_str_append_bytes")
                && append_target_is_mut_ref_param(body, tcx, args)
            {
                if std::env::var("GOS_JIT_TRACE").is_ok() {
                    eprintln!(
                        "jit: unsupported {} string append into a &mut param",
                        body.name
                    );
                }
                return true;
            }
        }
    }
    false
}

/// Whether a `gos_rt_str_append_bytes` call writes into a `&mut String`
/// parameter rather than into a local the body owns outright.
fn append_target_is_mut_ref_param(
    body: &Body,
    tcx: &TyCtxt,
    args: &[gossamer_mir::Operand],
) -> bool {
    use gossamer_mir::Operand;
    let Some(target) = args.first() else {
        return false;
    };
    let local = match target {
        Operand::Copy(place) => place.local,
        _ => return false,
    };
    if local.0 == 0 || body.arity < local.0 {
        return false;
    }
    matches!(
        tcx.kind_of(body.local_ty(local)),
        TyKind::Ref {
            mutability: gossamer_types::Mutbl::Mut,
            ..
        }
    )
}

const JIT_MAX_FIXED_AGGREGATE_SLOTS: u64 = (64 * 1024) / 8;

fn body_has_oversized_fixed_aggregate(body: &Body, tcx: &TyCtxt) -> bool {
    body.locals.iter().any(|local| {
        fixed_aggregate_slots(tcx, local.ty, &mut std::collections::HashSet::new())
            .is_some_and(|slots| slots > JIT_MAX_FIXED_AGGREGATE_SLOTS)
    })
}

fn fixed_aggregate_slots(
    tcx: &TyCtxt,
    ty: Ty,
    visiting: &mut std::collections::HashSet<Ty>,
) -> Option<u64> {
    let ty = match tcx.kind_of(ty) {
        TyKind::Ref { inner, .. } => *inner,
        _ => ty,
    };
    match tcx.kind_of(ty) {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::String
        | TyKind::Duration
        | TyKind::Instant => Some(1),
        TyKind::Array {
            elem,
            len: ArrayLen::Concrete(len),
        } => fixed_aggregate_slots(tcx, *elem, visiting)
            .map(|elem_slots| (*len as u64).saturating_mul(elem_slots)),
        TyKind::Tuple(elems) => {
            let mut total = 0u64;
            for elem in elems {
                total = total.saturating_add(fixed_aggregate_slots(tcx, *elem, visiting)?);
            }
            Some(total)
        }
        TyKind::Adt { def, substs } => {
            if !visiting.insert(ty) {
                return Some(1);
            }
            let fields = tcx.adt_field_tys(*def, substs)?;
            let mut total = 0u64;
            for field in fields {
                total = total.saturating_add(fixed_aggregate_slots(tcx, *field, visiting)?);
            }
            Some(total.max(1))
        }
        _ => Some(1),
    }
}

fn jit_local_ty_needs_bytecode(tcx: &TyCtxt, ty: Ty) -> bool {
    jit_local_ty_needs_bytecode_inner(tcx, ty, &mut std::collections::HashSet::new())
}

/// True when a map key or value of this type is stored directly by the typed
/// `gos_rt_map_*` entry points, so a native body owns the entry outright.
/// Anything else - a struct, an enum, a nested container - is stored as an
/// owned blob or an RC child whose lifetime the VM manages per entry.
fn jit_map_component_ok(tcx: &TyCtxt, ty: Ty) -> bool {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    matches!(
        tcx.kind_of(ty),
        TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_) | TyKind::String
    )
}

/// True when a map key of this type reaches the content-hashing `skey`
/// runtime, which folds the key's flat slot block through the per-slot
/// descriptor the MIR emits alongside the call.
///
/// Mirrors the descriptor the MIR builds for an aggregate key: a tuple,
/// fixed array, or plain struct whose leaf slots are scalars or `String`.
/// A key shape the descriptor cannot spell keeps the whole body on
/// bytecode, because there is no `skey` call for it to lower to.
fn jit_map_key_descriptor_ok(tcx: &TyCtxt, ty: Ty, depth: u32) -> bool {
    if depth > 8 {
        return false;
    }
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    match tcx.kind_of(ty) {
        TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_) | TyKind::String => true,
        TyKind::Tuple(elems) => {
            !elems.is_empty()
                && elems
                    .iter()
                    .all(|elem| jit_map_key_descriptor_ok(tcx, *elem, depth + 1))
        }
        TyKind::Array { elem, len } => {
            matches!(len, gossamer_types::ArrayLen::Concrete(n) if *n > 0)
                && jit_map_key_descriptor_ok(tcx, *elem, depth + 1)
        }
        // A plain struct inlines its fields into the same slot block. An
        // enum varies its layout per variant and keys through the separate
        // `ekey` descriptor instead, handled by [`jit_map_enum_key_ok`].
        TyKind::Adt { def, substs } if def.local < u32::MAX - 64 => {
            tcx.enum_variant_tys(*def).is_none()
                && tcx.adt_field_tys(*def, substs).is_some_and(|fields| {
                    !fields.is_empty()
                        && fields
                            .iter()
                            .all(|field| jit_map_key_descriptor_ok(tcx, *field, depth + 1))
                })
        }
        _ => false,
    }
}

/// True when a map key of this enum type reaches the `ekey` runtime, which
/// hashes by discriminant and payload through the structural-equality
/// descriptor the MIR interns for the type.
///
/// The descriptor's presence is the authority: the MIR registers it exactly
/// when it routes the type's map operations to `ekey`, and declines both
/// together for a variant shape it cannot classify. An enum without one keys
/// by node address, which no native entry point reproduces.
fn jit_map_enum_key_ok(tcx: &TyCtxt, ty: Ty) -> bool {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    matches!(tcx.kind_of(ty), TyKind::Adt { def, .. } if tcx.enum_variant_tys(*def).is_some())
        && tcx
            .rc_meta(&format!("gos_rc_meta_enumeq_{}", ty.as_u32()))
            .is_some()
}

fn jit_local_ty_needs_bytecode_inner(
    tcx: &TyCtxt,
    ty: Ty,
    visiting: &mut std::collections::HashSet<Ty>,
) -> bool {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        if matches!(tcx.kind_of(*inner), TyKind::Adt { def, .. } if def.local == u32::MAX)
            || matches!(tcx.kind_of(*inner), TyKind::Adt { def, .. } if def.local == u32::MAX - 1)
                && !jit_option_locals_ok()
        {
            return true;
        }
        ty = *inner;
    }
    if !visiting.insert(ty) {
        return false;
    }
    match tcx.kind_of(ty) {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::Unit
        | TyKind::Never
        | TyKind::String
        | TyKind::Duration
        | TyKind::Instant
        // A dynamic value is the same one-word handle on both tiers, and
        // every operation on it lowers to the `gos_rt_dyn_*` call the AOT
        // backend emits.
        | TyKind::DynValue => false,
        TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => {
            jit_local_ty_needs_bytecode_inner(tcx, *elem, visiting)
        }
        // Erased before lowering; a leak takes its representation's answer.
        TyKind::Nominal { repr, .. } => jit_local_ty_needs_bytecode_inner(tcx, *repr, visiting),
        TyKind::Tuple(elems) => elems
            .iter()
            .any(|elem| jit_local_ty_needs_bytecode_inner(tcx, *elem, visiting)),
        TyKind::Adt { def, .. } if def.local == u32::MAX - 1 => !jit_option_locals_ok(),
        TyKind::Adt { def, substs } if def.local == u32::MAX => {
            substs.types().into_iter().any(|payload| {
                !matches!(tcx.kind_of(payload), TyKind::DynError)
                    && jit_local_ty_needs_bytecode_inner(tcx, payload, visiting)
            })
        }
        TyKind::Adt { def, .. } if def.local == u32::MAX - 20 => false,
        // A runtime container's handle is the same one word on both tiers,
        // and every operation on it lowers to the `gos_rt_*` call the AOT
        // backend emits, so a body that builds and consumes one internally
        // needs no bytecode representation for it. The boundary is decided
        // separately by `ty_to_kind`.
        TyKind::Adt { def, .. } if is_bare_container_handle(def.local) => false,
        // A callable value is one machine word: either a raw code address
        // (`FnDef`) or an env pointer whose first word is the code address
        // (`FnPtr` / `FnTrait` / `Closure`, post the MIR's coercion). The
        // combinator surface built on them renders and reads every element
        // class the same way the bytecode tier does, so a body holding one
        // compiles; only a signature mentioning a type with no native
        // representation keeps it back.
        TyKind::FnDef { .. } => false,
        TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
            let sig = sig.clone();
            sig.inputs
                .iter()
                .chain(std::iter::once(&sig.output))
                .any(|ty| jit_local_ty_needs_bytecode_inner(tcx, *ty, visiting))
        }
        TyKind::Closure { .. } => false,
        TyKind::Adt { def, substs } if def.local < u32::MAX - 64 => {
            let struct_unsafe = tcx.adt_field_tys(*def, substs).is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| jit_local_ty_needs_bytecode_inner(tcx, *field, visiting))
            });
            let enum_unsafe = tcx.enum_variant_tys(*def).is_some_and(|variants| {
                variants
                    .iter()
                    .flatten()
                    .any(|field| jit_local_ty_needs_bytecode_inner(tcx, *field, visiting))
            });
            struct_unsafe || enum_unsafe
        }
        // A map local is the runtime `GosMap` pointer the compiled tiers
        // already use, and every operation on it lowers to the same
        // `gos_rt_map_*` call the AOT backend emits, so a body that builds and
        // consumes a map internally needs no conversion. A map in a parameter
        // or return position is a different question - the VM holds its own
        // representation there - and `ty_to_kind` declines those separately.
        //
        // Only the storage shapes the typed map helpers understand qualify. A
        // map whose value is a struct or carries heap children needs the
        // per-entry ownership the VM applies when it hands a value back, which
        // the native entry points do not reproduce.
        //
        // A content-hashed key is one the `skey` / `ekey` runtimes fold
        // through the descriptor the MIR passes with every call, so a tuple,
        // fixed array, plain struct, or descriptor-bearing enum key stays
        // native alongside the scalar and `String` fast paths.
        TyKind::HashMap { key, value, .. } => {
            !(jit_map_component_ok(tcx, *key)
                || jit_map_key_descriptor_ok(tcx, *key, 0)
                || jit_map_enum_key_ok(tcx, *key))
                || !jit_map_component_ok(tcx, *value)
        }
        // An `errors::Error` is a `*mut GosError` handle the runtime owns, the
        // same one word the LLVM tier lowers with no special casing, so a
        // local that never crosses the boundary needs no bytecode
        // representation. The boundary itself is decided by `ty_to_kind`.
        TyKind::DynError => false,
        // Options, other tagged standard-library carriers, and opaque handles
        // still need the bytecode path. Ordinary user aggregates are safe as
        // internal native locals and are checked recursively above.
        // A lazy iterator is one word either way, but not the SAME word: the
        // runtime's `gos_rt_lazy_iter_*` hands back a pointer where the
        // bytecode tier keeps a registry index under the identical type.
        // Nothing in the type tells them apart, so the local is admitted only
        // where the body itself built it - `body_builds_every_iterator_local`
        // proves that before this check is consulted.
        TyKind::Iterator(elem) => jit_local_ty_needs_bytecode_inner(tcx, *elem, visiting),
        // A `json::Value` is a runtime handle - one word, declared non-RC -
        // so a local holding one has a native representation. It stays on
        // bytecode all the same: the derived `__gos_serde_from_json_*` bodies
        // are the ones that hold it, and their lowering answers a different
        // total once compiled alongside their callers.
        TyKind::JsonValue
        | TyKind::Adt { .. }
        | TyKind::Range(_)
        | TyKind::Sender(_)
        | TyKind::Receiver(_)
        | TyKind::JoinHandle(_)
        | TyKind::Alias { .. }
        | TyKind::Dyn(_)
        | TyKind::Error => true,
        TyKind::Var(_) | TyKind::Param { .. } => false,
        TyKind::Ref { .. } => unreachable!("reference layers are peeled above"),
    }
}

fn describe_jit_local_ty(tcx: &TyCtxt, kind: &TyKind) -> String {
    match kind {
        TyKind::Vec(elem) | TyKind::Slice(elem) => {
            format!("{kind:?} elem={:?}", tcx.kind_of(*elem))
        }
        TyKind::Array { elem, .. } => {
            format!("{kind:?} elem={:?}", tcx.kind_of(*elem))
        }
        TyKind::Adt { substs, .. } => {
            let payloads: Vec<&TyKind> = substs
                .types()
                .into_iter()
                .map(|ty| tcx.kind_of(ty))
                .collect();
            format!("{kind:?} payloads={payloads:?}")
        }
        _ => format!("{kind:?}"),
    }
}

/// `true` when `body` spawns a goroutine or touches a cross-goroutine
/// sync primitive (see [`is_cross_goroutine_rt`]). Checked per body in
/// [`body_jit_unsupported`] and transitively in [`jit_compile_set`]:
/// native callers of such a body would reach the runtime's registries
/// through the native call, so the whole chain stays on bytecode.
fn body_has_cross_goroutine_ops(body: &Body) -> bool {
    use gossamer_mir::{ConstValue, Operand, Rvalue, StatementKind, Terminator};
    body.blocks.iter().any(|block| {
        let stmt_hit = block.stmts.iter().any(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                    ..
                } if is_cross_goroutine_rt(name)
            )
        });
        stmt_hit
            || matches!(
                &block.terminator,
                Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(name)),
                    ..
                } if is_cross_goroutine_rt(name)
            )
    })
}

/// Runtime shims whose VM twins live in the interpreter's own handle
/// registries (builtins.rs), not in `gossamer-runtime`'s. A JIT'd body
/// calling one would operate on native-registry handles that VM-side
/// goroutines can never resolve, so any body referencing this family -
/// a `go` spawn, a channel op, or a sync-handle mint / use - must stay
/// on the bytecode path where every participant shares one registry.
fn is_cross_goroutine_rt(name: &str) -> bool {
    const FAMILIES: &[&str] = &[
        "gos_rt_spawn",
        "gos_rt_cohort_",
        "gos_rt_join",
        "gos_rt_chan_",
        "gos_rt_wg_",
        "gos_rt_mutex_",
        "gos_rt_atomic_",
        "gos_rt_rwlock_",
        "gos_rt_once_",
        "gos_rt_barrier_",
        "gos_rt_sync_",
    ];
    FAMILIES.iter().any(|prefix| name.starts_with(prefix))
}

/// Interprocedural fixpoint computing, per JIT-compiled body, whether its
/// return value provably originates from a fresh allocation (a constructor
/// or a call to another fresh body) rather than a passthrough / projection
/// of an enum parameter. Sound and conservative: any value that might alias
/// an enum input is "tainted", and a body is fresh only when its return
/// local is never tainted. See [`JitFn::returns_fresh`].
// One cohesive interprocedural dataflow fixpoint; the taint helpers and the
// inner/outer iteration read together, so splitting it would obscure the
// analysis rather than clarify it.
#[allow(clippy::too_many_lines)]
fn compute_returns_fresh(bodies: &[Body], tcx: &TyCtxt) -> HashMap<String, bool> {
    use gossamer_mir::{Local, Operand, Rvalue, StatementKind, Terminator};
    use gossamer_types::TyKind;
    use std::collections::HashSet;

    let is_enum_ref = |body: &Body, l: Local| -> bool {
        let mut ty = body.local_ty(l);
        while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
            ty = *inner;
        }
        tcx.is_rc_managed(ty)
    };
    // An operand may alias an enum input: a tainted local, or any place
    // reached through a projection (an enum-child read).
    let op_alias = |op: &Operand, tainted: &HashSet<u32>| -> bool {
        matches!(op, Operand::Copy(p) if !p.projection.is_empty() || tainted.contains(&p.local.0))
    };
    // Whether an rvalue produces (or stores) a value that may alias an enum
    // input. Enum construction lowers to intrinsics, not `Aggregate`:
    // `gos_rc_alloc*` is a FRESH node; `gos_enum_load` reads a child of its
    // base and `gos_enum_tag` / `gos_enum_set_disc` return the node, so both
    // carry their base operand's taint; other intrinsics / rvalues are
    // conservatively treated as aliasing.
    let rvalue_alias = |rvalue: &Rvalue, tainted: &HashSet<u32>| -> bool {
        match rvalue {
            Rvalue::Use(op) => op_alias(op, tainted),
            Rvalue::Aggregate { operands, .. } => operands.iter().any(|op| op_alias(op, tainted)),
            Rvalue::CallIntrinsic { name, args } => {
                if name.starts_with("gos_rc_alloc") {
                    false
                } else if *name == "gos_enum_load"
                    || *name == "gos_enum_slot_ptr"
                    || name.starts_with("gos_enum_tag")
                    || name.starts_with("gos_enum_set_disc")
                    || name.starts_with("gos_enum_disc_tag")
                {
                    args.first().is_some_and(|op| op_alias(op, tainted))
                } else {
                    true
                }
            }
            _ => true,
        }
    };

    let def_to_name: HashMap<_, &str> = bodies
        .iter()
        .filter_map(|b| b.def.map(|d| (d, b.name.as_str())))
        .collect();
    let mut fresh: HashMap<String, bool> = bodies.iter().map(|b| (b.name.clone(), true)).collect();
    loop {
        let mut outer_changed = false;
        for body in bodies {
            let mut tainted: HashSet<u32> = HashSet::new();
            for p in 1..=body.arity {
                if is_enum_ref(body, Local(p)) {
                    tainted.insert(p);
                }
            }
            loop {
                let mut changed = false;
                for block in &body.blocks {
                    for stmt in &block.stmts {
                        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                            continue;
                        };
                        // Taint the assignment's root local: for a bare target
                        // that is the produced value; for a field-projection
                        // target (`node.field = child`) the node co-owns a
                        // stored aliasing child, so it is tainted too.
                        let root = place.local;
                        if !is_enum_ref(body, root)
                            || tainted.contains(&root.0)
                            || !rvalue_alias(rvalue, &tainted)
                        {
                            continue;
                        }
                        tainted.insert(root.0);
                        changed = true;
                    }
                    if let Terminator::Call {
                        callee,
                        destination,
                        ..
                    } = &block.terminator
                        && destination.is_simple()
                        && is_enum_ref(body, destination.local)
                        && !tainted.contains(&destination.local.0)
                    {
                        let callee_fresh = match callee {
                            Operand::FnRef { def, substs } if substs.is_empty() => def_to_name
                                .get(def)
                                .and_then(|n| fresh.get(*n))
                                .copied()
                                .unwrap_or(false),
                            _ => false,
                        };
                        if !callee_fresh {
                            tainted.insert(destination.local.0);
                            changed = true;
                        }
                    }
                }
                if !changed {
                    break;
                }
            }
            let ret_fresh = !is_enum_ref(body, Local(0)) || !tainted.contains(&0);
            if fresh[&body.name] && !ret_fresh {
                fresh.insert(body.name.clone(), false);
                outer_changed = true;
            }
        }
        if !outer_changed {
            break;
        }
    }
    fresh
}

fn body_kinds(
    body: &Body,
    tcx: &TyCtxt,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> Option<(Vec<JitKind>, JitKind)> {
    let mut params = Vec::with_capacity(body.arity as usize);
    for pidx in 1..=body.arity {
        let local = gossamer_mir::Local(pidx);
        let ty = body.local_ty(local);
        let kind = ty_to_kind(tcx, ty, enum_shapes, struct_shapes)?;
        // A lazy iterator is a runtime handle when compiled code built it and
        // a bytecode registry index when the VM did. The two are
        // indistinguishable in a parameter, so a body that takes one stays on
        // bytecode, where both spellings mean the same thing.
        if matches!(tcx.kind_of(ty), TyKind::Iterator(_)) {
            return None;
        }
        // A `Vec<String>` the trampoline built is freed - strings and all -
        // after the call, so it may only be lent to the body. Taken by value
        // the body owns its elements and may consume them, and the free would
        // then read storage the body already released.
        if matches!(kind, JitKind::NativeVecStr) && !matches!(tcx.kind_of(ty), TyKind::Ref { .. }) {
            return None;
        }
        // Result carriers / `TupleReturn` are return-only marshalling shapes;
        // the trampoline has no inbound parameter path for them, so a body
        // taking one as a parameter stays on bytecode.
        if matches!(
            kind,
            JitKind::ResultEnumPtr(_)
                | JitKind::ResultNativeStr
                | JitKind::ResultScalar(_)
                | JitKind::OptionScalar(_)
                | JitKind::TupleReturn(_)
        ) {
            return None;
        }
        params.push(kind);
    }
    if matches!(
        tcx.kind_of(body.local_ty(gossamer_mir::Local(0))),
        TyKind::Iterator(_)
    ) {
        return None;
    }
    let returns = ty_to_kind(
        tcx,
        body.local_ty(gossamer_mir::Local::RETURN),
        enum_shapes,
        struct_shapes,
    )?;
    // A flat array block is an inbound marshalling shape only: a returned
    // one would have to outlive the body with no owner to free it.
    if matches!(returns, JitKind::ArrayBlockPtr(..)) {
        return None;
    }
    Some((params, returns))
}

/// The slot encoding for a fixed array's element, or `None` when the
/// element is not one the flat block can carry.
///
/// Only an element the compiled tier strides a full 8-byte slot for
/// belongs here: the block hands the body one slot per element, so an
/// element it addresses at a narrower stride would read and write the
/// wrong bytes. `bool` packs to a single byte and is excluded for that
/// reason, as is every narrower integer.
fn array_elem_class(tcx: &TyCtxt, elem: Ty) -> Option<ArrayElem> {
    match tcx.kind_of(elem) {
        TyKind::Int(gossamer_types::IntTy::I64) => Some(ArrayElem::I64),
        TyKind::Float(gossamer_types::FloatTy::F64) => Some(ArrayElem::F64),
        TyKind::Char => Some(ArrayElem::Char),
        _ => None,
    }
}

/// True when `ty` is the 2-tuple `(i64, f64)` - the element shape the
/// trampoline marshals as a 16-byte primitive `GosVec` slot.
fn is_i64_f64_tuple(tcx: &TyCtxt, ty: Ty) -> bool {
    if let TyKind::Tuple(elems) = tcx.kind_of(ty) {
        elems.len() == 2
            && matches!(
                tcx.kind_of(elems[0]),
                TyKind::Int(gossamer_types::IntTy::I64)
            )
            && matches!(
                tcx.kind_of(elems[1]),
                TyKind::Float(gossamer_types::FloatTy::F64)
            )
    } else {
        false
    }
}

/// True when `ty` is `Vec<i64>` / `[i64]` - the inner element shape of a
/// `Vec<Vec<i64>>` the trampoline marshals as a nested `GosVec`.
fn is_i64_vec(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind_of(ty),
        TyKind::Vec(inner) | TyKind::Slice(inner)
            if matches!(tcx.kind_of(*inner), TyKind::Int(gossamer_types::IntTy::I64))
    )
}

/// Reports the peeled shape `ty_to_kind` is about to classify, under
/// `GOS_TYDUMP`.
fn trace_ty_to_kind(tcx: &TyCtxt, ty: Ty) {
    if std::env::var("GOS_TYDUMP").is_err() {
        return;
    }
    let inner = match tcx.kind_of(ty) {
        TyKind::Vec(e) | TyKind::Slice(e) => Some(tcx.kind_of(*e).clone()),
        _ => None,
    };
    let is_i64_vec_of_elem = match tcx.kind_of(ty) {
        TyKind::Vec(e) | TyKind::Slice(e) => is_i64_vec(tcx, *e),
        _ => false,
    };
    eprintln!(
        "TYDUMP ty_to_kind peeled kind = {:?} inner = {inner:?} is_i64_vec_of_elem = {is_i64_vec_of_elem:?}",
        tcx.kind_of(ty),
    );
}

/// The scalar shape a two-word carrier's payload word holds for `ty`, or
/// `None` when the payload is not a scalar the word can stand for.
fn carrier_scalar_kind(tcx: &TyCtxt, ty: Ty) -> Option<ResultScalarKind> {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    match tcx.kind_of(ty) {
        TyKind::Int(_) => Some(ResultScalarKind::I64),
        TyKind::Float(gossamer_types::FloatTy::F64) => Some(ResultScalarKind::F64),
        TyKind::Bool => Some(ResultScalarKind::Bool),
        TyKind::Char => Some(ResultScalarKind::Char),
        _ => None,
    }
}

fn ty_to_kind(
    tcx: &TyCtxt,
    ty: Ty,
    enum_shapes: &HashMap<u32, u32>,
    struct_shapes: &HashMap<u32, u32>,
) -> Option<JitKind> {
    // References to heap enums / structs are the same native pointer at
    // the ABI (compiled convention) - peel before classifying.
    let mut ty = ty;
    let mut was_borrowed = false;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
        was_borrowed = true;
    }
    trace_ty_to_kind(tcx, ty);
    match tcx.kind_of(ty) {
        TyKind::Bool => Some(JitKind::Bool),
        TyKind::Int(_) => Some(JitKind::I64),
        TyKind::Float(_) => Some(JitKind::F64),
        // A Unicode scalar crosses the boundary as its `u32` code point in an
        // integer register (the compiled-tier `char` ABI); the trampoline
        // re-wraps a returned word as `Value::Char`.
        TyKind::Char => Some(JitKind::Char),
        TyKind::Unit => Some(JitKind::Unit),
        // Heap enums with a registered VM-side shape cross as native
        // tagged pointers; the body works on the compiled-tier
        // representation directly (zero conversion).
        TyKind::Adt { def, .. } if tcx.is_rc_managed(ty) => enum_shapes
            .get(&def.local)
            .map(|idx| JitKind::EnumPtr(*idx)),
        // `Result<Enum, errors::Error>`: the by-value two-word `i128` return
        // whose `Ok` payload is a registered heap enum. Only the `Ok`-enum
        // shape needs marshalling (the `Err` side is read back generically),
        // so classify by the `Ok` type's shape. Return-only; a `Result`
        // parameter keeps a body on bytecode (no `ty_to_kind` for it as a
        // param is wired in the trampoline).
        // `Option<scalar>`: the same `[disc, payload]` carrier a `Result`
        // rides, with `Some` in the zero discriminant.
        TyKind::Adt { def, substs } if def.local == u32::MAX - 1 => {
            carrier_scalar_kind(tcx, *substs.types().first()?).map(JitKind::OptionScalar)
        }
        TyKind::Adt { def, substs } if def.local == u32::MAX => {
            let ok_ty = *substs.types().first()?;
            let mut ok_ty = ok_ty;
            while let TyKind::Ref { inner, .. } = tcx.kind_of(ok_ty) {
                ok_ty = *inner;
            }
            match tcx.kind_of(ok_ty) {
                TyKind::Adt { def: ok_def, .. } if tcx.is_rc_managed(ok_ty) => enum_shapes
                    .get(&ok_def.local)
                    .map(|idx| JitKind::ResultEnumPtr(*idx)),
                TyKind::String => Some(JitKind::ResultNativeStr),
                _ => carrier_scalar_kind(tcx, ok_ty).map(JitKind::ResultScalar),
            }
        }
        // An all-scalar user struct with a registered VM-side shape
        // crosses as a pointer to its flat field-slot block (the
        // compiled tier's struct layout - no RC header). Only structs
        // the interpreter registered (all-scalar fields) appear here, so
        // the marshal is always O(field count) with no heap children.
        TyKind::Adt { def, .. } if struct_shapes.contains_key(&def.local) => struct_shapes
            .get(&def.local)
            .map(|idx| JitKind::StructPtr(*idx)),
        // `String` and `Vec<i64>` cross as the runtime's native pointer
        // (the flat-ABI shape `mir_ty_to_cabi` emits: a single pointer
        // slot). The trampoline builds a fresh runtime object from the
        // VM value at the call boundary and reclaims it after the call,
        // so the body sees the same `*mut c_char` / `*mut GosVec` shape
        // the AOT tier passes (zero in-body conversion).
        TyKind::String => Some(JitKind::NativeStr),
        // `[T; N]` over a scalar element: a flat block of N slots, distinct
        // from the `Vec` / slice spellings above - the compiled tier indexes
        // it by static stride against a statically known length, with no
        // `GosVec` header in front of the elements.
        TyKind::Array { elem, len } if array_elem_class(tcx, *elem).is_some() => {
            let class = array_elem_class(tcx, *elem)?;
            u32::try_from(len.to_usize())
                .ok()
                .map(|n| JitKind::ArrayBlockPtr(n, class))
        }
        // `[i64]` / `[f64]` / `[(i64, f64)]` slices marshal identically to the
        // `Vec<...>` spelling - the runtime object is the same `*mut GosVec` - so
        // the idiomatic slice-param helper (`fn f(xs: &[i64])`) promotes just
        // like its `Vec` twin instead of falling to `_ => None`.
        TyKind::Vec(elem) | TyKind::Slice(elem)
            if matches!(tcx.kind_of(*elem), TyKind::Int(gossamer_types::IntTy::I64)) =>
        {
            Some(JitKind::NativeVecI64)
        }
        TyKind::Vec(elem) | TyKind::Slice(elem)
            if matches!(
                tcx.kind_of(*elem),
                TyKind::Float(gossamer_types::FloatTy::F64)
            ) =>
        {
            Some(JitKind::NativeVecF64)
        }
        TyKind::Vec(elem) | TyKind::Slice(elem) if is_i64_f64_tuple(tcx, *elem) => {
            Some(JitKind::NativeVecTupleIF)
        }
        // `&Vec<String>` / `&[String]`: a `STRING`-kind `GosVec` whose slots
        // hold owned cstrings, which is the layout both compiled tiers build.
        // Borrowed only - the trampoline owns the vec it builds and deep-frees
        // its strings afterwards, so a body that could consume an element
        // would leave that free reading storage it no longer owns.
        TyKind::Vec(elem) | TyKind::Slice(elem) if matches!(tcx.kind_of(*elem), TyKind::String) => {
            let _ = was_borrowed;
            Some(JitKind::NativeVecStr)
        }
        // `Vec<Vec<i64>>` / `[[i64]]`: an outer vec / slice whose elements are
        // themselves `Vec<i64>` / `[i64]`. Crosses as the AOT nested layout
        // (outer `VEC`-kind slots holding inner `GosVec` pointers); the
        // trampoline builds it once per source Arc. A `&[[i64]]` parameter
        // peels to `Slice(Slice(i64))`, so both outer shapes are accepted.
        TyKind::Vec(elem) | TyKind::Slice(elem) if is_i64_vec(tcx, *elem) => {
            Some(JitKind::NativeVecVecI64)
        }
        // `U8Vec` (prelude sentinel `u32::MAX - 20`): a byte-buffer handle.
        // The trampoline copies its bytes through a fresh native `GosU8Vec`
        // and copies the mutations back. Other prelude handles (Mutex,
        // WaitGroup, I64Vec, Atomic) are cross-goroutine shared state and
        // stay on bytecode (no `JitKind`), so they fall through below.
        TyKind::Adt { def, .. } if def.local == u32::MAX - 20 => Some(JitKind::U8VecHandle),
        // A 2-element tuple of scalars / heap enums is marshalled as a tuple
        // RETURN (the `(Node, i64)` / `(i64, i64)` constructor shape). Both
        // elements must classify; otherwise stay on bytecode. `body_kinds`
        // rejects this as a parameter - it is a return-only marshalling shape.
        TyKind::Tuple(elems) if elems.len() == 2 => {
            let e0 = ty_to_tuple_elem(tcx, elems[0], enum_shapes)?;
            let e1 = ty_to_tuple_elem(tcx, elems[1], enum_shapes)?;
            Some(JitKind::TupleReturn([e0, e1]))
        }
        // Remaining aggregates (larger `Tuple`, struct `Adt`, `Vec` of
        // unsupported element, channels …) stay on bytecode: the
        // trampoline has no marshalling shape for them yet, and
        // JIT-promoting them risks segfaults at the boundary.
        _ => None,
    }
}

/// Classifies one tuple element type into a [`TupleElem`] for tuple-return
/// marshalling, or `None` when it is not a scalar / heap-enum the trampoline
/// can decode (a nested tuple, `Vec`, struct, …). References peel first.
fn ty_to_tuple_elem(
    tcx: &TyCtxt,
    ty: Ty,
    enum_shapes: &HashMap<u32, u32>,
) -> Option<crate::jit::TupleElem> {
    use crate::jit::TupleElem;
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    match tcx.kind_of(ty) {
        TyKind::Int(_) => Some(TupleElem::I64),
        TyKind::Float(_) => Some(TupleElem::F64),
        TyKind::Bool => Some(TupleElem::Bool),
        TyKind::Char => Some(TupleElem::Char),
        TyKind::String => Some(TupleElem::Str),
        TyKind::Adt { def, .. } if tcx.is_rc_managed(ty) => {
            enum_shapes.get(&def.local).copied().map(TupleElem::Enum)
        }
        _ => None,
    }
}

/// Registers every `gos_rt_*` C-ABI symbol the codegen may emit
/// against the JIT builder so that compiled bodies can call into the
/// runtime in-process. Kept in lock-step with the symbol set the
/// AOT object backend imports - anything the codegen knows how to
/// emit must resolve here.
///
/// Returns the set of registered symbol names so the JIT-eligibility
/// check can identify bodies that call something cranelift can't
/// resolve (and would silently lower to a typed-zero stub).
#[allow(
    clippy::too_many_lines,
    reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
)]
fn register_runtime_symbols(builder: &mut JITBuilder) -> std::collections::HashSet<&'static str> {
    use gossamer_runtime::c_abi as rt;
    use gossamer_runtime::preempt;
    let mut names: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    macro_rules! reg {
        ($($name:literal => $f:path),* $(,)?) => {
            $(
                builder.symbol($name, $f as *const u8);
                names.insert($name);
            )*
        };
    }
    reg! {
        "gos_rt_set_args"            => rt::gos_rt_set_args,
        "gos_rt_program_start"       => rt::gos_rt_program_start,
        "gos_rt_os_args"             => rt::gos_rt_os_args,
        "gos_rt_arr_len"             => rt::gos_rt_arr_len,
        "gos_rt_len"                 => rt::gos_rt_len,
        "gos_rt_str_len"             => rt::gos_rt_str_len,
        "gos_rt_str_byte_len"        => rt::gos_rt_str_byte_len,
        "gos_rt_str_with_capacity"   => rt::gos_rt_str_with_capacity,
        "gos_rt_str_byte_at"         => rt::gos_rt_str_byte_at,
        "gos_rt_str_char_at"         => rt::gos_rt_str_char_at,
        "gos_rt_str_substring"       => rt::gos_rt_str_substring,
        "gos_rt_os_read_dir"         => rt::gos_rt_os_read_dir,
        "gos_rt_str_concat"          => rt::gos_rt_str_concat,
        "gos_rt_str_concat_drop_a"     => rt::gos_rt_str_concat_drop_a,
        "gos_rt_str_append_bytes"      => rt::gos_rt_str_append_bytes,
        "gos_rt_str_append_i64"        => rt::gos_rt_str_append_i64,
        "gos_rt_str_append_f64"        => rt::gos_rt_str_append_f64,
        "gos_rt_str_trim"            => rt::gos_rt_str_trim,
        "gos_rt_str_to_upper"        => rt::gos_rt_str_to_upper,
        "gos_rt_str_to_lower"        => rt::gos_rt_str_to_lower,
        "gos_rt_str_contains"        => rt::gos_rt_str_contains,
        "gos_rt_str_starts_with"     => rt::gos_rt_str_starts_with,
        "gos_rt_str_ends_with"       => rt::gos_rt_str_ends_with,
        "gos_rt_str_find"            => rt::gos_rt_str_find,
        "gos_rt_str_replace"         => rt::gos_rt_str_replace,
        "gos_rt_str_split"           => rt::gos_rt_str_split,
        "gos_rt_str_split_once"      => rt::gos_rt_str_split_once,
        "gos_rt_str_rsplit_once"     => rt::gos_rt_str_rsplit_once,
        "gos_rt_str_count"           => rt::gos_rt_str_count,
        "gos_rt_str_chars"           => rt::gos_rt_str_chars,
        "gos_rt_str_strip_chars"     => rt::gos_rt_str_strip_chars,
        "gos_rt_str_lstrip_chars"    => rt::gos_rt_str_lstrip_chars,
        "gos_rt_str_rstrip_chars"    => rt::gos_rt_str_rstrip_chars,
        "gos_rt_str_zfill"           => rt::gos_rt_str_zfill,
        "gos_rt_str_center"          => rt::gos_rt_str_center,
        "gos_rt_str_slice"           => rt::gos_rt_str_slice,
        "gos_rt_str_rfind_opt"       => rt::gos_rt_str_rfind_opt,
        "gos_rt_str_lines"           => rt::gos_rt_str_lines,
        "gos_rt_str_push_char"       => rt::gos_rt_str_push_char,
        "gos_rt_str_push_utf8"        => rt::gos_rt_str_push_utf8,
        "gos_rt_str_push_byte"       => rt::gos_rt_str_push_byte,
        "gos_rt_deque_new"           => rt::gos_rt_deque_new,
        "gos_rt_deque_from_vec_i64"  => rt::gos_rt_deque_from_vec_i64,
        "gos_rt_set_from_vec_i64" => rt::gos_rt_set_from_vec_i64,
        "gos_rt_set_from_vec_str" => rt::gos_rt_set_from_vec_str,
        "gos_rt_btree_set_from_vec_i64" => rt::gos_rt_btree_set_from_vec_i64,
        "gos_rt_btree_set_from_vec_str" => rt::gos_rt_btree_set_from_vec_str,
        "gos_rt_queue_new"           => rt::gos_rt_queue_new,
        "gos_rt_queue_from_vec_i64"  => rt::gos_rt_queue_from_vec_i64,
        "gos_rt_stack_new"           => rt::gos_rt_stack_new,
        "gos_rt_stack_from_vec_i64"  => rt::gos_rt_stack_from_vec_i64,
        "gos_rt_deque_push_back"     => rt::gos_rt_deque_push_back,
        "gos_rt_bheap_max_format_desc" => rt::container_heap::gos_rt_bheap_max_format_desc,
        "gos_rt_bheap_max_from_vec_desc" => rt::container_heap::gos_rt_bheap_max_from_vec_desc,
        "gos_rt_bheap_max_pop_desc" => rt::container_heap::gos_rt_bheap_max_pop_desc,
        "gos_rt_bheap_max_push_desc" => rt::container_heap::gos_rt_bheap_max_push_desc,
        "gos_rt_bheap_min_format_desc" => rt::container_heap::gos_rt_bheap_min_format_desc,
        "gos_rt_bheap_min_from_vec_desc" => rt::container_heap::gos_rt_bheap_min_from_vec_desc,
        "gos_rt_bheap_min_pop_desc" => rt::container_heap::gos_rt_bheap_min_pop_desc,
        "gos_rt_bheap_max_pop_desc_into" => rt::container_heap::gos_rt_bheap_max_pop_desc_into,
        "gos_rt_bheap_min_pop_desc_into" => rt::container_heap::gos_rt_bheap_min_pop_desc_into,
        "gos_rt_bheap_min_push_desc" => rt::container_heap::gos_rt_bheap_min_push_desc,
        "gos_rt_bheap_new_typed" => rt::container_heap::gos_rt_bheap_new_typed,
        "gos_rt_bheap_peek_elem" => rt::container_heap::gos_rt_bheap_peek_elem,
        "gos_rt_desc_cmp" => rt::desc_cmp::gos_rt_desc_cmp,
        "gos_rt_set_format_tagged" => rt::set::gos_rt_set_format_tagged,
        "gos_rt_set_insert_ekey" => rt::set::gos_rt_set_insert_ekey,
        "gos_rt_set_contains_ekey" => rt::set::gos_rt_set_contains_ekey,
        "gos_rt_set_remove_ekey" => rt::set::gos_rt_set_remove_ekey,
        "gos_rt_set_to_vec_ekey" => rt::set::gos_rt_set_to_vec_ekey,
        "gos_rt_set_format_ekey" => rt::set::gos_rt_set_format_ekey,
        "gos_rt_deque_push_back_wide" => rt::gos_rt_deque_push_back_wide,
        "gos_rt_deque_push_front_wide" => rt::gos_rt_deque_push_front_wide,
        "gos_rt_deque_new_typed"     => rt::gos_rt_deque_new_typed,
        "gos_rt_deque_from_vec"      => rt::gos_rt_deque_from_vec,
        "gos_rt_deque_vec"           => rt::gos_rt_deque_vec,
        "gos_rt_deque_format_desc"   => rt::gos_rt_deque_format_desc,
        "gos_rt_queue_format_desc"   => rt::gos_rt_queue_format_desc,
        "gos_rt_stack_format_desc"   => rt::gos_rt_stack_format_desc,
        "gos_rt_deque_push_back_f64" => rt::gos_rt_deque_push_back_f64,
        "gos_rt_deque_push_front"    => rt::gos_rt_deque_push_front,
        "gos_rt_deque_push_front_f64" => rt::gos_rt_deque_push_front_f64,
        "gos_rt_deque_pop_front"     => rt::gos_rt_deque_pop_front,
        "gos_rt_deque_pop_back"      => rt::gos_rt_deque_pop_back,
        "gos_rt_deque_pop_front_into" => rt::gos_rt_deque_pop_front_into,
        "gos_rt_deque_pop_back_into" => rt::gos_rt_deque_pop_back_into,
        "gos_rt_deque_peek_front"    => rt::gos_rt_deque_peek_front,
        "gos_rt_deque_peek_back"     => rt::gos_rt_deque_peek_back,
        "gos_rt_deque_len"           => rt::gos_rt_deque_len,
        "gos_rt_deque_is_empty"      => rt::gos_rt_deque_is_empty,
        "gos_rt_set_is_empty"        => rt::gos_rt_set_is_empty,
        "gos_rt_deque_clear"         => rt::gos_rt_deque_clear,
        "gos_rt_deque_free"          => rt::gos_rt_deque_free,
        "gos_rt_strings_join"        => rt::gos_rt_strings_join,
        "gos_rt_path_glob"           => rt::gos_rt_path_glob,
        "gos_rt_path_matches"        => rt::gos_rt_path_matches,
        "gos_rt_sort_stable_i64"     => rt::gos_rt_sort_stable_i64,
        "gos_rt_sort_stable_f64"     => rt::gos_rt_sort_stable_f64,
        "gos_rt_sort_binary_search_f64" => rt::gos_rt_sort_binary_search_f64,
        "gos_rt_sort_partition_point_f64" => rt::gos_rt_sort_partition_point_f64,
        "gos_rt_sort_stable_str"     => rt::gos_rt_sort_stable_str,
        "gos_rt_sort_stable_aggr"    => rt::gos_rt_sort_stable_aggr,
        "gos_rt_sort_binary_search_aggr" => rt::gos_rt_sort_binary_search_aggr,
        "gos_rt_sort_partition_point_aggr" => rt::gos_rt_sort_partition_point_aggr,
        "gos_rt_sort_binary_search_i64" => rt::gos_rt_sort_binary_search_i64,
        "gos_rt_sort_binary_search_str" => rt::gos_rt_sort_binary_search_str,
        "gos_rt_sort_partition_point_i64" => rt::gos_rt_sort_partition_point_i64,
        "gos_rt_sort_partition_point_str" => rt::gos_rt_sort_partition_point_str,
        "gos_rt_path_base"           => rt::gos_rt_path_base,
        "gos_rt_path_components"     => rt::gos_rt_path_components,
        "gos_rt_path_prefixes"       => rt::gos_rt_path_prefixes,
        "gos_rt_path_unique_prefixes" => rt::gos_rt_path_unique_prefixes,
        "gos_rt_path_dir"            => rt::gos_rt_path_dir,
        "gos_rt_path_ext"            => rt::gos_rt_path_ext,
        "gos_rt_path_file_name"      => rt::gos_rt_path_file_name,
        "gos_rt_path_parent"         => rt::gos_rt_path_parent,
        "gos_rt_path_stem"           => rt::gos_rt_path_stem,
        "gos_rt_vec_first"           => rt::gos_rt_vec_first,
        "gos_rt_vec_last"            => rt::gos_rt_vec_last,
        "gos_rt_vec_get_opt"         => rt::gos_rt_vec_get_opt,
        "gos_rt_vec_reversed"        => rt::gos_rt_vec_reversed,
        "gos_rt_vec_reverse"         => rt::gos_rt_vec_reverse,
        "gos_rt_vec_index_of_i64"    => rt::gos_rt_vec_index_of_i64,
        "gos_rt_vec_index_of_str"    => rt::gos_rt_vec_index_of_str,
        "gos_rt_vec_count_of_i64"    => rt::gos_rt_vec_count_of_i64,
        "gos_rt_vec_count_of_str"    => rt::gos_rt_vec_count_of_str,
        "gos_rt_vec_contains_i64"    => rt::gos_rt_vec_contains_i64,
        "gos_rt_vec_contains_str"    => rt::gos_rt_vec_contains_str,
        "gos_rt_vec_slice_result"    => rt::gos_rt_vec_slice_result,
        "gos_rt_intarr_slice_result" => rt::gos_rt_intarr_slice_result,
        "gos_rt_bytearr_slice_result" => rt::gos_rt_bytearr_slice_result,
        "gos_rt_floatarr_slice_result" => rt::gos_rt_floatarr_slice_result,
        "gos_rt_vec_insert_safe"     => rt::gos_rt_vec_insert_safe,
        "gos_rt_vec_remove_at"       => rt::gos_rt_vec_remove_at,
        "gos_rt_vec_remove_safe"     => rt::gos_rt_vec_remove_safe,
        "gos_rt_vec_swap_safe"       => rt::gos_rt_vec_swap_safe,
        "gos_rt_map_keys_vec"        => rt::gos_rt_map_keys_vec,
        "gos_rt_map_values_vec"      => rt::gos_rt_map_values_vec,
        "gos_rt_map_pop_i64"         => rt::gos_rt_map_pop_i64,
        "gos_rt_map_pop_str"         => rt::gos_rt_map_pop_str,
        "gos_rt_map_pop_typed_str"   => rt::gos_rt_map_pop_typed_str,
        "gos_rt_min_i64"             => rt::gos_rt_min_i64,
        "gos_rt_max_i64"             => rt::gos_rt_max_i64,
        "gos_rt_clamp_i64"           => rt::gos_rt_clamp_i64,
        "gos_rt_min_f64"             => rt::gos_rt_min_f64,
        "gos_rt_max_f64"             => rt::gos_rt_max_f64,
        "gos_rt_clamp_f64"           => rt::gos_rt_clamp_f64,
        "gos_rt_str_repeat"          => rt::gos_rt_str_repeat,
        "gos_rt_str_eq"              => rt::gos_rt_str_eq,
        "gos_rt_str_compare"         => rt::gos_rt_str_compare,
        "gos_rt_str_is_empty"        => rt::gos_rt_str_is_empty,
        "gos_rt_str_free"            => rt::gos_rt_str_free,
        "gos_rt_str_free_typed"      => rt::gos_rt_str_free_typed,
        "gos_rt_str_retain_typed"    => rt::gos_rt_str_retain_typed,
        "gos_rt_len_is_zero"         => rt::gos_rt_len_is_zero,
        "gos_rt_error_new"           => rt::gos_rt_error_new,
        "gos_rt_error_from"          => rt::gos_rt_error_from,
        "gos_rt_error_wrap"          => rt::gos_rt_error_wrap,
        "gos_rt_error_message"       => rt::gos_rt_error_message,
        "gos_rt_error_display"       => rt::gos_rt_error_display,
        "gos_rt_error_cause"         => rt::gos_rt_error_cause,
        "gos_rt_error_is"            => rt::gos_rt_error_is,
        "gos_rt_error_is_sentinel"   => rt::gos_rt_error_is_sentinel,
        "gos_rt_error_chain"         => rt::gos_rt_error_chain,
        "gos_rt_error_field"         => rt::gos_rt_error_field,
        "gos_rt_error_fields"        => rt::gos_rt_error_fields,
        "gos_rt_error_with_field"    => rt::gos_rt_error_with_field,
        "gos_rt_regex_compile"       => rt::gos_rt_regex_compile,
        "gos_rt_regex_is_match"      => rt::gos_rt_regex_is_match,
        "gos_rt_regex_count"         => rt::gos_rt_regex_count,
        "gos_rt_regex_find"          => rt::gos_rt_regex_find,
        "gos_rt_regex_find_opt"      => rt::gos_rt_regex_find_opt,
        "gos_rt_regex_captures"      => rt::gos_rt_regex_captures,
        "gos_rt_regex_find_all"      => rt::gos_rt_regex_find_all,
        "gos_rt_regex_replace_all"   => rt::gos_rt_regex_replace_all,
        "gos_rt_regex_split"         => rt::gos_rt_regex_split,
        "gos_rt_fs_read_to_string"   => rt::gos_rt_fs_read_to_string,
        "gos_rt_fs_write"            => rt::gos_rt_fs_write,
        "gos_rt_fs_create_dir_all"   => rt::gos_rt_fs_create_dir_all,
        "gos_rt_fs_create_dir_all_mode" => rt::gos_rt_fs_create_dir_all_mode,
        "gos_rt_fs_create_dir_mode"  => rt::gos_rt_fs_create_dir_mode,
        "gos_rt_fs_permissions"      => rt::gos_rt_fs_permissions,
        "gos_rt_fs_set_permissions"  => rt::gos_rt_fs_set_permissions,
        "gos_rt_fs_write_mode"       => rt::gos_rt_fs_write_mode,
        "gos_rt_fs_file_close"       => rt::gos_rt_fs_file_close,
        "gos_rt_fs_file_create"      => rt::gos_rt_fs_file_create,
        "gos_rt_fs_file_flush"       => rt::gos_rt_fs_file_flush,
        "gos_rt_fs_file_open"        => rt::gos_rt_fs_file_open,
        "gos_rt_fs_file_read"        => rt::gos_rt_fs_file_read,
        "gos_rt_fs_file_read_to_string" => rt::gos_rt_fs_file_read_to_string,
        "gos_rt_fs_file_write"       => rt::gos_rt_fs_file_write,
        "gos_rt_fs_temp_dir"         => rt::gos_rt_fs_temp_dir,
        "gos_rt_fs_temp_file"        => rt::gos_rt_fs_temp_file,
        "gos_rt_fs_open_options_append" => rt::gos_rt_fs_open_options_append,
        "gos_rt_fs_open_options_create" => rt::gos_rt_fs_open_options_create,
        "gos_rt_fs_open_options_create_new" => rt::gos_rt_fs_open_options_create_new,
        "gos_rt_fs_open_options_new" => rt::gos_rt_fs_open_options_new,
        "gos_rt_fs_open_options_open" => rt::gos_rt_fs_open_options_open,
        "gos_rt_fs_open_options_read" => rt::gos_rt_fs_open_options_read,
        "gos_rt_fs_open_options_truncate" => rt::gos_rt_fs_open_options_truncate,
        "gos_rt_fs_open_options_write" => rt::gos_rt_fs_open_options_write,
        "gos_rt_path_join"           => rt::gos_rt_path_join,
        "gos_rt_flag_set_new"        => rt::gos_rt_flag_set_new,
        "gos_rt_flag_set_string"     => rt::gos_rt_flag_set_string,
        "gos_rt_flag_set_int"        => rt::gos_rt_flag_set_int,
        "gos_rt_flag_set_uint"       => rt::gos_rt_flag_set_uint,
        "gos_rt_flag_set_float"      => rt::gos_rt_flag_set_float,
        "gos_rt_flag_set_bool"       => rt::gos_rt_flag_set_bool,
        "gos_rt_flag_set_duration"   => rt::gos_rt_flag_set_duration,
        "gos_rt_flag_set_string_list" => rt::gos_rt_flag_set_string_list,
        "gos_rt_flag_set_short"      => rt::gos_rt_flag_set_short,
        "gos_rt_flag_set_usage"      => rt::gos_rt_flag_set_usage,
        "gos_rt_flag_set_parse"      => rt::gos_rt_flag_set_parse,
        "gos_rt_duration_from_secs"  => rt::gos_rt_duration_from_secs,
        "gos_rt_duration_from_millis" => rt::gos_rt_duration_from_millis,
        "gos_rt_time_format_rfc3339" => rt::gos_rt_time_format_rfc3339,
        "gos_rt_flag_parse"          => rt::gos_rt_flag_parse,
        "gos_rt_flag_map_get"        => rt::gos_rt_flag_map_get,
        "gos_rt_os_env"              => rt::gos_rt_os_env,
        "gos_rt_os_cwd"              => rt::gos_rt_os_cwd,
        "gos_rt_fs_list_dir"         => rt::gos_rt_fs_list_dir,
        "gos_rt_fs_walk_dir"         => rt::gos_rt_fs_walk_dir,
        "gos_rt_exec_run"            => rt::gos_rt_exec_run,
        "gos_rt_exec_run_in"         => rt::gos_rt_exec_run_in,
        "gos_rt_exec_spawn"          => rt::gos_rt_exec_spawn,
        "gos_rt_exec_spawn_piped"    => rt::gos_rt_exec_spawn_piped,
        "gos_rt_child_write_stdin"   => rt::gos_rt_child_write_stdin,
        "gos_rt_child_close_stdin"   => rt::gos_rt_child_close_stdin,
        "gos_rt_child_read_line"     => rt::gos_rt_child_read_line,
        "gos_rt_child_read_stdout"   => rt::gos_rt_child_read_stdout,
        "gos_rt_child_wait"          => rt::gos_rt_child_wait,
        "gos_rt_child_kill"          => rt::gos_rt_child_kill,
        "gos_rt_exec_kill"           => rt::gos_rt_exec_kill,
        "gos_rt_exec_signal"         => rt::gos_rt_exec_signal,
        "gos_rt_exec_kill_group"     => rt::gos_rt_exec_kill_group,
        "gos_rt_exec_wait_timeout"   => rt::gos_rt_exec_wait_timeout,
        "gos_rt_exec_pipeline_run"   => rt::gos_rt_exec_pipeline_run,
        "gos_rt_signal_on"           => rt::gos_rt_signal_on,
        "gos_rt_signal_wait"         => rt::gos_rt_signal_wait,
        "gos_rt_signal_try_wait"     => rt::gos_rt_signal_try_wait,
        "gos_rt_os_set_env"          => rt::gos_rt_os_set_env,
        "gos_rt_os_unset_env"        => rt::gos_rt_os_unset_env,
        "gos_rt_os_user_current_name" => rt::gos_rt_os_user_current_name,
        "gos_rt_os_user_current_uid" => rt::gos_rt_os_user_current_uid,
        "gos_rt_os_user_current_gid" => rt::gos_rt_os_user_current_gid,
        "gos_rt_os_user_current_home" => rt::gos_rt_os_user_current_home,
        "gos_rt_os_user_lookup_uid"  => rt::gos_rt_os_user_lookup_uid,
        "gos_rt_os_user_lookup_name" => rt::gos_rt_os_user_lookup_name,
        "gos_rt_netip_is_valid"      => rt::gos_rt_netip_is_valid,
        "gos_rt_netip_is_v4"         => rt::gos_rt_netip_is_v4,
        "gos_rt_netip_is_v6"         => rt::gos_rt_netip_is_v6,
        "gos_rt_netip_is_loopback"   => rt::gos_rt_netip_is_loopback,
        "gos_rt_netip_is_unspecified" => rt::gos_rt_netip_is_unspecified,
        "gos_rt_netip_is_multicast"  => rt::gos_rt_netip_is_multicast,
        "gos_rt_netip_is_private"    => rt::gos_rt_netip_is_private,
        "gos_rt_netip_normalize"     => rt::gos_rt_netip_normalize,
        "gos_rt_netip_host_of"       => rt::gos_rt_netip_host_of,
        "gos_rt_netip_port_of"       => rt::gos_rt_netip_port_of,
        "gos_rt_netip_join_addr_port" => rt::gos_rt_netip_join_addr_port,
        "gos_rt_mime_parse"          => rt::gos_rt_mime_parse,
        "gos_rt_mime_top"            => rt::gos_rt_mime_top,
        "gos_rt_mime_sub"            => rt::gos_rt_mime_sub,
        "gos_rt_mime_charset"        => rt::gos_rt_mime_charset,
        "gos_rt_mime_boundary"       => rt::gos_rt_mime_boundary,
        "gos_rt_mime_param"          => rt::gos_rt_mime_param,
        "gos_rt_mime_type_by_extension" => rt::gos_rt_mime_type_by_extension,
        "gos_rt_mime_extension_by_type" => rt::gos_rt_mime_extension_by_type,
        "gos_rt_mime_is_valid"       => rt::gos_rt_mime_is_valid,
        "gos_rt_toml_to_json"        => rt::gos_rt_toml_to_json,
        "gos_rt_toml_from_json"      => rt::gos_rt_toml_from_json,
        "gos_rt_toml_is_valid"       => rt::gos_rt_toml_is_valid,
        "gos_rt_toml_pretty"         => rt::gos_rt_toml_pretty,
        "gos_rt_yaml_to_json"        => rt::gos_rt_yaml_to_json,
        "gos_rt_yaml_from_json"      => rt::gos_rt_yaml_from_json,
        "gos_rt_yaml_is_valid"       => rt::gos_rt_yaml_is_valid,
        "gos_rt_sync_map_new"        => rt::gos_rt_sync_map_new,
        "gos_rt_sync_map_set"        => rt::gos_rt_sync_map_set,
        "gos_rt_sync_map_get"        => rt::gos_rt_sync_map_get,
        "gos_rt_sync_map_delete"     => rt::gos_rt_sync_map_delete,
        "gos_rt_sync_map_len"        => rt::gos_rt_sync_map_len,
        "gos_rt_sync_map_contains"   => rt::gos_rt_sync_map_contains,
        "gos_rt_sync_map_keys"       => rt::gos_rt_sync_map_keys,
        "gos_rt_barrier_new"         => rt::gos_rt_barrier_new,
        "gos_rt_barrier_wait"        => rt::gos_rt_barrier_wait,
        "gos_rt_once_new"            => rt::gos_rt_once_new,
        "gos_rt_once_call"           => rt::gos_rt_once_call,
        "gos_rt_math_rng_new"        => rt::gos_rt_math_rng_new,
        "gos_rt_math_rng_next_f64"   => rt::gos_rt_math_rng_next_f64,
        "gos_rt_math_rng_next_u32"   => rt::gos_rt_math_rng_next_u32,
        "gos_rt_math_rng_next_u64"   => rt::gos_rt_math_rng_next_u64,
        "gos_rt_math_rng_range_u64"  => rt::gos_rt_math_rng_range_u64,
        "gos_rt_bytes_builder_new"   => rt::gos_rt_bytes_builder_new,
        "gos_rt_bytes_builder_with_capacity" => rt::gos_rt_bytes_builder_with_capacity,
        "gos_rt_bytes_builder_write" => rt::gos_rt_bytes_builder_write,
        "gos_rt_bytes_builder_write_char" => rt::gos_rt_bytes_builder_write_char,
        "gos_rt_bytes_builder_build" => rt::gos_rt_bytes_builder_build,
        "gos_rt_bytes_builder_as_str" => rt::gos_rt_bytes_builder_as_str,
        "gos_rt_bytes_builder_len"   => rt::gos_rt_bytes_builder_len,
        "gos_rt_bytes_buffer_new"    => rt::gos_rt_bytes_buffer_new,
        "gos_rt_bytes_buffer_with_capacity" => rt::gos_rt_bytes_buffer_with_capacity,
        "gos_rt_bytes_buffer_write_str" => rt::gos_rt_bytes_buffer_write_str,
        "gos_rt_bytes_buffer_push"   => rt::gos_rt_bytes_buffer_push,
        "gos_rt_bytes_buffer_len"    => rt::gos_rt_bytes_buffer_len,
        "gos_rt_bytes_buffer_is_empty" => rt::gos_rt_bytes_buffer_is_empty,
        "gos_rt_bytes_buffer_clear"  => rt::gos_rt_bytes_buffer_clear,
        "gos_rt_bytes_buffer_to_string" => rt::gos_rt_bytes_buffer_to_string,
        "gos_rt_bytes_split"         => rt::gos_rt_bytes_split,
        "gos_rt_bytes_replace"       => rt::gos_rt_bytes_replace,
        "gos_rt_net_ip_octets"       => rt::gos_rt_net_ip_octets,
        "gos_rt_tcp_listener_close"  => rt::gos_rt_tcp_listener_close,
        "gos_rt_tcp_stream_close"    => rt::gos_rt_tcp_stream_close,
        "gos_rt_tcp_stream_clear_read_timeout" => rt::gos_rt_tcp_stream_clear_read_timeout,
        "gos_rt_tcp_stream_clear_write_timeout" => rt::gos_rt_tcp_stream_clear_write_timeout,
        "gos_rt_tcp_stream_set_read_timeout_ms" => rt::gos_rt_tcp_stream_set_read_timeout_ms,
        "gos_rt_tcp_stream_set_write_timeout_ms" => rt::gos_rt_tcp_stream_set_write_timeout_ms,
        "gos_rt_tcp_stream_set_nodelay" => rt::gos_rt_tcp_stream_set_nodelay,
        "gos_rt_tcp_stream_read_into" => rt::gos_rt_tcp_stream_read_into,
        "gos_rt_udp_close"           => rt::gos_rt_udp_close,
        "gos_rt_field_error_new" => rt::gos_rt_field_error_new,
        "gos_rt_field_error_path" => rt::gos_rt_field_error_path,
        "gos_rt_field_error_message" => rt::gos_rt_field_error_message,
        "gos_rt_field_error_code" => rt::gos_rt_field_error_code,
        "gos_rt_validate_errors_new" => rt::gos_rt_validate_errors_new,
        "gos_rt_validate_errors_add" => rt::gos_rt_validate_errors_add,
        "gos_rt_validate_errors_is_empty" => rt::gos_rt_validate_errors_is_empty,
        "gos_rt_validate_errors_len" => rt::gos_rt_validate_errors_len,
        "gos_rt_validate_errors_count" => rt::gos_rt_validate_errors_count,
        "gos_rt_validate_errors_get" => rt::gos_rt_validate_errors_get,
        "gos_rt_validate_errors_collect" => rt::gos_rt_validate_errors_collect,
        "gos_rt_rwlock_new" => rt::gos_rt_rwlock_new,
        "gos_rt_rwlock_get" => rt::gos_rt_rwlock_get,
        "gos_rt_rwlock_set" => rt::gos_rt_rwlock_set,
        "gos_rt_rwlock_with_read" => rt::gos_rt_rwlock_with_read,
        "gos_rt_rwlock_with_write" => rt::gos_rt_rwlock_with_write,
        "gos_rt_shared_new" => rt::gos_rt_shared_new,
        "gos_rt_shared_get" => rt::gos_rt_shared_get,
        "gos_rt_shared_set" => rt::gos_rt_shared_set,
        "gos_rt_shared_with" => rt::gos_rt_shared_with,
        "gos_rt_shared_update" => rt::gos_rt_shared_update,
        "gos_rt_ctx_background" => rt::gos_rt_ctx_background,
        "gos_rt_ctx_with_cancel" => rt::gos_rt_ctx_with_cancel,
        "gos_rt_ctx_with_timeout" => rt::gos_rt_ctx_with_timeout,
        "gos_rt_ctx_cancel" => rt::gos_rt_ctx_cancel,
        "gos_rt_ctx_cancelled" => rt::gos_rt_ctx_cancelled,
        "gos_rt_ctx_is_cancelled" => rt::gos_rt_ctx_is_cancelled,
        "gos_rt_ctx_done" => rt::gos_rt_ctx_done,
        "gos_rt_metrics_counter_new" => rt::gos_rt_metrics_counter_new,
        "gos_rt_metrics_counter_inc" => rt::gos_rt_metrics_counter_inc,
        "gos_rt_metrics_counter_value" => rt::gos_rt_metrics_counter_value,
        "gos_rt_metrics_gauge_new" => rt::gos_rt_metrics_gauge_new,
        "gos_rt_metrics_gauge_set" => rt::gos_rt_metrics_gauge_set,
        "gos_rt_metrics_gauge_inc" => rt::gos_rt_metrics_gauge_inc,
        "gos_rt_metrics_gauge_dec" => rt::gos_rt_metrics_gauge_dec,
        "gos_rt_metrics_gauge_value" => rt::gos_rt_metrics_gauge_value,
        "gos_rt_metrics_histogram_new" => rt::gos_rt_metrics_histogram_new,
        "gos_rt_metrics_histogram_observe" => rt::gos_rt_metrics_histogram_observe,
        "gos_rt_metrics_histogram_sum" => rt::gos_rt_metrics_histogram_sum,
        "gos_rt_metrics_histogram_count" => rt::gos_rt_metrics_histogram_count,
        "gos_rt_metrics_registry_new" => rt::gos_rt_metrics_registry_new,
        "gos_rt_metrics_registry_register" => rt::gos_rt_metrics_registry_register,
        "gos_rt_metrics_registry_render" => rt::gos_rt_metrics_registry_render,
        "gos_rt_middleware_new"      => rt::gos_rt_middleware_new,
        "gos_rt_middleware_new_kind" => rt::gos_rt_middleware_new_kind,
        "gos_rt_mw_cors_permissive"  => rt::gos_rt_mw_cors_permissive,
        "gos_rt_mw_cors_new"         => rt::gos_rt_mw_cors_new,
        "gos_rt_mw_hsts_safe_default" => rt::gos_rt_mw_hsts_safe_default,
        "gos_rt_mw_hsts_strict"      => rt::gos_rt_mw_hsts_strict,
        "gos_rt_mw_security_strict"  => rt::gos_rt_mw_security_strict,
        "gos_rt_mw_security_off"     => rt::gos_rt_mw_security_off,
        "gos_rt_mw_cache_no_store"   => rt::gos_rt_mw_cache_no_store,
        "gos_rt_mw_cache_immutable_for" => rt::gos_rt_mw_cache_immutable_for,
        "gos_rt_mw_rate_limit_per_ip" => rt::gos_rt_mw_rate_limit_per_ip,
        "gos_rt_middleware_serve"    => rt::gos_rt_middleware_serve,
        "gos_rt_trace_tracer_new" => rt::gos_rt_trace_tracer_new,
        "gos_rt_trace_tracer_start_span" => rt::gos_rt_trace_tracer_start_span,
        "gos_rt_trace_span_set_attribute" => rt::gos_rt_trace_span_set_attribute,
        "gos_rt_trace_span_set_status" => rt::gos_rt_trace_span_set_status,
        "gos_rt_trace_span_end" => rt::gos_rt_trace_span_end,
        "gos_rt_trace_ended_to_otlp_json" => rt::gos_rt_trace_ended_to_otlp_json,
        "gos_rt_bheap_len"           => rt::gos_rt_bheap_len,
        "gos_rt_bheap_is_empty"      => rt::gos_rt_bheap_is_empty,
        "gos_rt_bheap_clear"         => rt::gos_rt_bheap_clear,
        "gos_rt_bheap_max_new_i64"   => rt::gos_rt_bheap_max_new_i64,
        "gos_rt_bheap_max_from_vec_i64" => rt::gos_rt_bheap_max_from_vec_i64,
        "gos_rt_bheap_max_from_vec_f64" => rt::gos_rt_bheap_max_from_vec_f64,
        "gos_rt_bheap_max_push_i64"  => rt::gos_rt_bheap_max_push_i64,
        "gos_rt_bheap_max_push_f64" => rt::gos_rt_bheap_max_push_f64,
        "gos_rt_bheap_max_pop_i64"   => rt::gos_rt_bheap_max_pop_i64,
        "gos_rt_bheap_max_pop_f64" => rt::gos_rt_bheap_max_pop_f64,
        "gos_rt_bheap_max_peek_i64"  => rt::gos_rt_bheap_max_peek_i64,
        "gos_rt_bheap_min_new_i64"   => rt::gos_rt_bheap_min_new_i64,
        "gos_rt_bheap_min_from_vec_i64" => rt::gos_rt_bheap_min_from_vec_i64,
        "gos_rt_bheap_min_from_vec_f64" => rt::gos_rt_bheap_min_from_vec_f64,
        "gos_rt_bheap_min_push_i64"  => rt::gos_rt_bheap_min_push_i64,
        "gos_rt_bheap_min_push_f64" => rt::gos_rt_bheap_min_push_f64,
        "gos_rt_bheap_min_pop_i64"   => rt::gos_rt_bheap_min_pop_i64,
        "gos_rt_bheap_min_pop_f64" => rt::gos_rt_bheap_min_pop_f64,
        "gos_rt_bheap_min_peek_i64"  => rt::gos_rt_bheap_min_peek_i64,
        "gos_rt_url_query_escape"    => rt::gos_rt_url_query_escape,
        "gos_rt_url_path_escape"     => rt::gos_rt_url_path_escape,
        "gos_rt_url_query_unescape"  => rt::gos_rt_url_query_unescape,
        "gos_rt_url_path_unescape"   => rt::gos_rt_url_path_unescape,
        "gos_rt_os_exists"           => rt::gos_rt_os_exists,
        "gos_rt_os_is_file"          => rt::gos_rt_os_is_file,
        "gos_rt_os_is_dir"           => rt::gos_rt_os_is_dir,
        "gos_rt_os_is_symlink"       => rt::gos_rt_os_is_symlink,
        "gos_rt_os_file_size"        => rt::gos_rt_os_file_size,
        "gos_rt_os_remove_file"      => rt::gos_rt_os_remove_file,
        "gos_rt_result_map_bare"     => rt::gos_rt_result_map_bare,
        "gos_rt_result_map_err_bare" => rt::gos_rt_result_map_err_bare,
        "gos_rt_os_write_file_result" => rt::gos_rt_os_write_file_result,
        "gos_rt_os_write_file_bytes_result" => rt::gos_rt_os_write_file_bytes_result,
        "gos_rt_fs_read_bytes_result" => rt::gos_rt_fs_read_bytes_result,
        "gos_rt_os_mkdir_all_result"  => rt::gos_rt_os_mkdir_all_result,
        "gos_rt_os_remove_file_result" => rt::gos_rt_os_remove_file_result,
        "gos_rt_os_remove_dir_all_result" => rt::gos_rt_os_remove_dir_all_result,
        "gos_rt_http_stream"         => rt::gos_rt_http_stream,
        "gos_rt_http_get"            => rt::gos_rt_http_get,
        "gos_rt_http_head"           => rt::gos_rt_http_head,
        "gos_rt_http_options"        => rt::gos_rt_http_options,
        "gos_rt_http_post"           => rt::gos_rt_http_post,
        "gos_rt_http_put"            => rt::gos_rt_http_put,
        "gos_rt_http_delete"         => rt::gos_rt_http_delete,
        "gos_rt_http_request"        => rt::gos_rt_http_request,
        "gos_rt_http_request_bytes"  => rt::gos_rt_http_request_bytes,
        "gos_rt_http_stream_next_line" => rt::gos_rt_http_stream_next_line,
        "gos_rt_http_stream_next_chunk" => rt::gos_rt_http_stream_next_chunk,
        "gos_rt_bufio_scanner_new"   => rt::gos_rt_bufio_scanner_new,
        "gos_rt_bufio_scanner_scan"  => rt::gos_rt_bufio_scanner_scan,
        "gos_rt_bufio_scanner_text"  => rt::gos_rt_bufio_scanner_text,
        "gos_rt_http_client_new"     => rt::gos_rt_http_client_new,
        "gos_rt_http_client_get"     => rt::gos_rt_http_client_get,
        "gos_rt_http_client_post"    => rt::gos_rt_http_client_post,
        "gos_rt_http_client_put"     => rt::gos_rt_http_client_put,
        "gos_rt_http_client_options" => rt::gos_rt_http_client_options,
        "gos_rt_http_client_delete"  => rt::gos_rt_http_client_delete,
        "gos_rt_http_client_head"    => rt::gos_rt_http_client_head,
        "gos_rt_http_client_builder_new" => rt::gos_rt_http_client_builder_new,
        "gos_rt_http_client_builder_max_redirects" => rt::gos_rt_http_client_builder_max_redirects,
        "gos_rt_http_client_builder_timeout_ms" => rt::gos_rt_http_client_builder_timeout_ms,
        "gos_rt_http_client_builder_cookie_jar" => rt::gos_rt_http_client_builder_cookie_jar,
        "gos_rt_http_client_builder_proxy" => rt::gos_rt_http_client_builder_proxy,
        "gos_rt_http_client_builder_build" => rt::gos_rt_http_client_builder_build,
        "gos_rt_http_client_request" => rt::gos_rt_http_client_request,
        "gos_rt_http_client_request_bytes" => rt::gos_rt_http_client_request_bytes,
        "gos_rt_http_request_header" => rt::gos_rt_http_request_header,
        "gos_rt_http_request_body"   => rt::gos_rt_http_request_body,
        "gos_rt_http_request_send"   => rt::gos_rt_http_request_send,
        "gos_rt_http_response_status" => rt::gos_rt_http_response_status,
        "gos_rt_http_response_body"  => rt::gos_rt_http_response_body,
        "gos_rt_http_response_raw_bytes" => rt::gos_rt_http_response_raw_bytes,
        "gos_rt_http_response_headers" => rt::gos_rt_http_response_headers,
        "gos_rt_http_response_content_type" => rt::gos_rt_http_response_content_type,
        "gos_rt_http_response_location" => rt::gos_rt_http_response_location,
        "gos_rt_vec_get_i64"         => rt::gos_rt_vec_get_i64,
        "gos_rt_vec_set_i64"         => rt::gos_rt_vec_set_i64,
        "gos_rt_vec_set_slots"       => rt::gos_rt_vec_set_slots,
        "gos_rt_vec_set_i128"        => rt::gos_rt_vec_set_i128,
        "gos_rt_vec_format_i64"      => rt::gos_rt_vec_format_i64,
        "gos_rt_vec_format_u64"      => rt::gos_rt_vec_format_u64,
        "gos_rt_vec_format_f64"      => rt::gos_rt_vec_format_f64,
        "gos_rt_vec_format_bool"     => rt::gos_rt_vec_format_bool,
        "gos_rt_vec_format_string"   => rt::gos_rt_vec_format_string,
        "gos_rt_vec_format_vec_i64"  => rt::gos_rt_vec_format_vec_i64,
        "gos_rt_vec_format_vec_string" => rt::gos_rt_vec_format_vec_string,
        "gos_rt_tuple_format"        => rt::gos_rt_tuple_format,
        "gos_rt_tuple_format_desc"   => rt::gos_rt_tuple_format_desc,
        "gos_rt_concat_init"         => rt::gos_rt_concat_init,
        "gos_rt_concat_str"          => rt::gos_rt_concat_str,
        "gos_rt_concat_i64"          => rt::gos_rt_concat_i64,
        "gos_rt_concat_u64"          => rt::gos_rt_concat_u64,
        "gos_rt_concat_f64"          => rt::gos_rt_concat_f64,
        "gos_rt_concat_f64_prec"     => rt::gos_rt_concat_f64_prec,
        "gos_rt_concat_bool"         => rt::gos_rt_concat_bool,
        "gos_rt_concat_char"         => rt::gos_rt_concat_char,
        "gos_rt_concat_finish"       => rt::gos_rt_concat_finish,
        "gos_rt_main_exit_code"      => rt::gos_rt_main_exit_code,
        "gos_rt_result_new"          => rt::gos_rt_result_new,
        "gos_rt_result_new_owned"    => rt::gos_rt_result_new,
        "gos_rt_result_new_f64"      => rt::gos_rt_result_new_f64,
        "gos_rt_result_disc"         => rt::gos_rt_result_disc,
        "gos_rt_result_payload"      => rt::gos_rt_result_payload,
        "gos_rt_result_payload_f64"  => rt::gos_rt_result_payload_f64,
        "gos_rt_enum_unit"           => rt::gos_rt_enum_unit,
        "gos_rt_strconv_parse_i64"   => rt::gos_rt_strconv_parse_i64,
        "gos_rt_strconv_parse_i64_bytes" => rt::gos_rt_strconv_parse_i64_bytes,
        "gos_rt_strconv_parse_f64"   => rt::gos_rt_strconv_parse_f64,
        "gos_rt_strconv_parse_f64_bytes" => rt::gos_rt_strconv_parse_f64_bytes,
        "gos_rt_option_unwrap"       => rt::gos_rt_option_unwrap,
        "gos_rt_result_unwrap"       => rt::gos_rt_result_unwrap,
        "gos_rt_result_unwrap_or"    => rt::gos_rt_result_unwrap_or,
        "gos_rt_result_unwrap_or_vec" => rt::vec::gos_rt_result_unwrap_or_vec,
        "gos_rt_result_unwrap_or_str" => rt::vec::gos_rt_result_unwrap_or_str,
        "gos_rt_result_ok_payload_release" => rt::vec::gos_rt_result_ok_payload_release,
        "gos_rt_result_unwrap_or_carrier" => rt::vec::gos_rt_result_unwrap_or_carrier,
        "gos_rt_result_payload_i128" => rt::gos_rt_result_payload_i128,
        "gos_rt_result_ok"           => rt::gos_rt_result_ok,
        "gos_rt_result_err"          => rt::gos_rt_result_err,
        "gos_rt_result_ok_or"        => rt::gos_rt_result_ok_or,
        "gos_rt_result_ok_or_else"   => rt::gos_rt_result_ok_or_else,
        "gos_rt_result_is_ok"        => rt::gos_rt_result_is_ok,
        "gos_rt_result_is_err"       => rt::gos_rt_result_is_err,
        "gos_rt_set_new"             => rt::gos_rt_set_new,
        "gos_rt_btree_set_new"       => rt::gos_rt_btree_set_new,
        "gos_rt_set_insert"          => rt::gos_rt_set_insert,
        "gos_rt_set_insert_skey"     => rt::gos_rt_set_insert_skey,
        "gos_rt_set_contains"        => rt::gos_rt_set_contains,
        "gos_rt_set_contains_skey"   => rt::gos_rt_set_contains_skey,
        "gos_rt_set_remove"          => rt::gos_rt_set_remove,
        "gos_rt_set_remove_skey"     => rt::gos_rt_set_remove_skey,
        "gos_rt_set_len"             => rt::gos_rt_set_len,
        "gos_rt_set_to_vec_skey"     => rt::gos_rt_set_to_vec_skey,
        "gos_rt_set_union"           => rt::gos_rt_set_union,
        "gos_rt_set_intersection"    => rt::gos_rt_set_intersection,
        "gos_rt_set_intersection_skey" => rt::gos_rt_set_intersection_skey,
        "gos_rt_set_intersection_to_vec" => rt::gos_rt_set_intersection_to_vec,
        "gos_rt_set_intersection_to_vec_i64" => rt::gos_rt_set_intersection_to_vec_i64,
        "gos_rt_set_intersection_to_vec_skey" => rt::gos_rt_set_intersection_to_vec_skey,
        "gos_rt_set_difference"      => rt::gos_rt_set_difference,
        "gos_rt_set_symmetric_difference" => rt::gos_rt_set_symmetric_difference,
        "gos_rt_set_is_subset"       => rt::gos_rt_set_is_subset,
        "gos_rt_set_is_superset"     => rt::gos_rt_set_is_superset,
        "gos_rt_set_is_disjoint"     => rt::gos_rt_set_is_disjoint,
        "gos_rt_str_as_bytes"        => rt::gos_rt_str_as_bytes,
        "gos_rt_regex_captures_all"  => rt::gos_rt_regex_captures_all,
        "gos_rt_vec_clone"           => rt::gos_rt_vec_clone,
        "gos_rt_vec_set_slot_children" => rt::gos_rt_vec_set_slot_children,
        "gos_rt_vec_mark_rc_elems"   => rt::gos_rt_vec_mark_rc_elems,
        "gos_rt_vec_mark_vec_elems"  => rt::gos_rt_vec_mark_vec_elems,
        "gos_rt_map_inc_str_i64"        => rt::gos_rt_map_inc_str_i64,
        "gos_rt_map_inc_typed_str_i64"  => rt::gos_rt_map_inc_typed_str_i64,
        "gos_rt_map_or_insert_str_i64"  => rt::gos_rt_map_or_insert_str_i64,
        "gos_rt_map_or_insert_typed_str_i64" => rt::gos_rt_map_or_insert_typed_str_i64,
        "gos_rt_map_or_insert_i64_i64"  => rt::gos_rt_map_or_insert_i64_i64,
        "gos_rt_errors_join"            => rt::gos_rt_errors_join,
        "gos_rt_errors_join_vec"        => rt::gos_rt_errors_join_vec,
        "gos_rt_json_value_object_n"    => rt::gos_rt_json_value_object_n,
        "gos_rt_http_response_set_header" => rt::gos_rt_http_response_set_header,
        "gos_rt_http_response_set_content_type" => rt::gos_rt_http_response_set_content_type,
        "gos_rt_http_response_set_body_bytes" => rt::gos_rt_http_response_set_body_bytes,
        "gos_rt_http_response_with_header" => rt::gos_rt_http_response_with_header,
        "gos_rt_http_response_get_header" => rt::gos_rt_http_response_get_header,
        "gos_rt_http_request_set_header" => rt::gos_rt_http_request_set_header,
        "gos_rt_http_request_get_header" => rt::gos_rt_http_request_get_header,
        "gos_rt_http_request_path"   => rt::gos_rt_http_request_path,
        "gos_rt_http_request_method" => rt::gos_rt_http_request_method,
        "gos_rt_http_request_query"  => rt::gos_rt_http_request_query,
        "gos_rt_http_request_peer_addr" => rt::gos_rt_http_request_peer_addr,
        "gos_rt_http_request_context" => rt::gos_rt_http_request_context,
        "gos_rt_http_request_headers" => rt::gos_rt_http_request_headers,
        "gos_rt_http_request_body_str" => rt::gos_rt_http_request_body_str,
        "gos_rt_http_request_raw_body" => rt::gos_rt_http_request_raw_body,
        "gos_rt_http_response_text_new" => rt::gos_rt_http_response_text_new,
        "gos_rt_http_response_json_new" => rt::gos_rt_http_response_json_new,
        "gos_rt_http_response_stream_new" => rt::gos_rt_http_response_stream_new,
        "gos_rt_http_response_free"  => rt::gos_rt_http_response_free,
        "gos_rt_gzip_encode"         => rt::gos_rt_gzip_encode,
        "gos_rt_gzip_decode"         => rt::gos_rt_gzip_decode,
        "gos_rt_sha256_hex"          => rt::gos_rt_sha256_hex,
        "gos_rt_sha512_hex"          => rt::gos_rt_sha512_hex,
        "gos_rt_blake3_hex"          => rt::gos_rt_blake3_hex,
        "gos_rt_hmac_sha256_hex"     => rt::gos_rt_hmac_sha256_hex,
        "gos_rt_slog_info"           => rt::gos_rt_slog_info,
        "gos_rt_slog_warn"           => rt::gos_rt_slog_warn,
        "gos_rt_slog_error"          => rt::gos_rt_slog_error,
        "gos_rt_slog_debug"          => rt::gos_rt_slog_debug,
        // std::database::sql C-ABI shims.
        "gos_rt_sql_value_null"             => rt::sql::gos_rt_sql_value_null,
        "gos_rt_sql_value_bool"             => rt::sql::gos_rt_sql_value_bool,
        "gos_rt_sql_value_int"              => rt::sql::gos_rt_sql_value_int,
        "gos_rt_sql_value_float"            => rt::sql::gos_rt_sql_value_float,
        "gos_rt_sql_value_text"             => rt::sql::gos_rt_sql_value_text,
        "gos_rt_sql_open"                   => rt::sql::gos_rt_sql_open,
        "gos_rt_sql_drivers"                => rt::sql::gos_rt_sql_drivers,
        "gos_rt_sql_conn_execute"           => rt::sql::gos_rt_sql_conn_execute,
        "gos_rt_sql_conn_query"             => rt::sql::gos_rt_sql_conn_query,
        "gos_rt_sql_conn_begin"             => rt::sql::gos_rt_sql_conn_begin,
        "gos_rt_sql_conn_begin_with"        => rt::sql::gos_rt_sql_conn_begin_with,
        "gos_rt_sql_conn_ping"              => rt::sql::gos_rt_sql_conn_ping,
        "gos_rt_sql_conn_set_busy_timeout"  => rt::sql::gos_rt_sql_conn_set_busy_timeout,
        "gos_rt_sql_conn_interrupt"         => rt::sql::gos_rt_sql_conn_interrupt,
        "gos_rt_sql_rows_next_row"          => rt::sql::gos_rt_sql_rows_next_row,
        "gos_rt_sql_rows_close"             => rt::sql::gos_rt_sql_rows_close,
        "gos_rt_sql_rows_columns"           => rt::sql::gos_rt_sql_rows_columns,
        "gos_rt_sql_row_get_i64"            => rt::sql::gos_rt_sql_row_get_i64,
        "gos_rt_sql_row_get_f64"            => rt::sql::gos_rt_sql_row_get_f64,
        "gos_rt_sql_row_get_bool"           => rt::sql::gos_rt_sql_row_get_bool,
        "gos_rt_sql_row_get_text"           => rt::sql::gos_rt_sql_row_get_text,
        "gos_rt_sql_row_get_blob"           => rt::sql::gos_rt_sql_row_get_blob,
        "gos_rt_sql_row_get_opt_i64"        => rt::sql::gos_rt_sql_row_get_opt_i64,
        "gos_rt_sql_row_get_opt_f64"        => rt::sql::gos_rt_sql_row_get_opt_f64,
        "gos_rt_sql_row_get_opt_bool"       => rt::sql::gos_rt_sql_row_get_opt_bool,
        "gos_rt_sql_row_get_opt_text"       => rt::sql::gos_rt_sql_row_get_opt_text,
        "gos_rt_sql_row_is_null"            => rt::sql::gos_rt_sql_row_is_null,
        "gos_rt_sql_row_width"              => rt::sql::gos_rt_sql_row_width,
        "gos_rt_sql_tx_commit"              => rt::sql::gos_rt_sql_tx_commit,
        "gos_rt_sql_tx_rollback"            => rt::sql::gos_rt_sql_tx_rollback,
        "gos_rt_sql_tx_execute"             => rt::sql::gos_rt_sql_tx_execute,
        "gos_rt_sql_tx_savepoint"           => rt::sql::gos_rt_sql_tx_savepoint,
        "gos_rt_sql_tx_release_savepoint"   => rt::sql::gos_rt_sql_tx_release_savepoint,
        "gos_rt_sql_tx_rollback_to_savepoint" => rt::sql::gos_rt_sql_tx_rollback_to_savepoint,
        // Closure-callback combinators (Vec::sort_by / iter::map / ...).
        // Adds these to the JIT dispatch table so user bodies that
        // call them stop falling through to the bytecode VM.
        "gos_rt_arr_sort_by_f64"     => rt::gos_rt_arr_sort_by_f64,
        "gos_rt_arr_sort_by_i64"     => rt::gos_rt_arr_sort_by_i64,
        "gos_rt_arr_reverse"         => rt::gos_rt_arr_reverse,
        "gos_rt_arr_sort_i64"        => rt::gos_rt_arr_sort_i64,
        "gos_rt_arr_sort_str"        => rt::gos_rt_arr_sort_str,
        "gos_rt_arr_sort_tuple"      => rt::gos_rt_arr_sort_tuple,
        "gos_rt_vec_sort_by_f64"     => rt::gos_rt_vec_sort_by_f64,
        "gos_rt_vec_sort_by_i64"     => rt::gos_rt_vec_sort_by_i64,
        "gos_rt_vec_sort_i64"        => rt::gos_rt_vec_sort_i64,
        "gos_rt_vec_sort_str"        => rt::gos_rt_vec_sort_str,
        "gos_rt_vec_sort_tuple"      => rt::gos_rt_vec_sort_tuple,
        "gos_rt_arr_sort_by_aggr"    => rt::gos_rt_arr_sort_by_aggr,
        "gos_rt_vec_sort_by_aggr"    => rt::gos_rt_vec_sort_by_aggr,
        "gos_rt_callback_invoke"     => rt::gos_rt_callback_invoke,
        "gos_rt_lazy_iter_range_i64" => rt::gos_rt_lazy_iter_range_i64,
        "gos_rt_lazy_iter_range_from_i64" => rt::gos_rt_lazy_iter_range_from_i64,
        "gos_rt_lazy_iter_range_inclusive_i64" => rt::gos_rt_lazy_iter_range_inclusive_i64,
        "gos_rt_lazy_iter_from_vec_i64" => rt::gos_rt_lazy_iter_from_vec_i64,
        "gos_rt_lazy_iter_str_chars" => rt::gos_rt_lazy_iter_str_chars,
        "gos_rt_lazy_iter_str_bytes" => rt::gos_rt_lazy_iter_str_bytes,
        "gos_rt_lazy_iter_repeat_i64" => rt::gos_rt_lazy_iter_repeat_i64,
        "gos_rt_lazy_iter_once_i64" => rt::gos_rt_lazy_iter_once_i64,
        "gos_rt_lazy_iter_take_i64" => rt::gos_rt_lazy_iter_take_i64,
        "gos_rt_lazy_iter_skip_i64" => rt::gos_rt_lazy_iter_skip_i64,
        "gos_rt_lazy_iter_chain_i64" => rt::gos_rt_lazy_iter_chain_i64,
        "gos_rt_lazy_iter_enumerate_i64" => rt::gos_rt_lazy_iter_enumerate_i64,
        "gos_rt_lazy_iter_zip_i64" => rt::gos_rt_lazy_iter_zip_i64,
        "gos_rt_lazy_iter_map_i64" => rt::gos_rt_lazy_iter_map_i64,
        "gos_rt_lazy_iter_filter_i64" => rt::gos_rt_lazy_iter_filter_i64,
        "gos_rt_lazy_iter_collect_i64" => rt::gos_rt_lazy_iter_collect_i64,
        "gos_rt_lazy_iter_collect_aggr" => rt::gos_rt_lazy_iter_collect_aggr,
        "gos_rt_lazy_iter_from_vec_aggr" => rt::gos_rt_lazy_iter_from_vec_aggr,
        "gos_rt_lazy_iter_collect_pair_i64" => rt::gos_rt_lazy_iter_collect_pair_i64,
        "gos_rt_lazy_iter_count_i64" => rt::gos_rt_lazy_iter_count_i64,
        "gos_rt_lazy_iter_count_pair_i64" => rt::gos_rt_lazy_iter_count_pair_i64,
        "gos_rt_lazy_iter_drop_i64" => rt::gos_rt_lazy_iter_drop_i64,
        "gos_rt_lazy_iter_drop_pair_i64" => rt::gos_rt_lazy_iter_drop_pair_i64,
        "gos_rt_lazy_iter_sum_i64" => rt::gos_rt_lazy_iter_sum_i64,
        "gos_rt_lazy_iter_product_i64" => rt::gos_rt_lazy_iter_product_i64,
        "gos_rt_lazy_iter_min_i64" => rt::gos_rt_lazy_iter_min_i64,
        "gos_rt_lazy_iter_max_i64" => rt::gos_rt_lazy_iter_max_i64,
        "gos_rt_lazy_iter_fold_i64" => rt::gos_rt_lazy_iter_fold_i64,
        "gos_rt_lazy_iter_any_i64" => rt::gos_rt_lazy_iter_any_i64,
        "gos_rt_lazy_iter_all_i64" => rt::gos_rt_lazy_iter_all_i64,
        "gos_rt_lazy_iter_find_i64" => rt::gos_rt_lazy_iter_find_i64,
        "gos_rt_testing_check"       => rt::gos_rt_testing_check,
        "gos_rt_testing_check_eq_i64" => rt::gos_rt_testing_check_eq_i64,
        "gos_rt_testing_wait_for_scheduler_idle" => rt::gos_rt_testing_wait_for_scheduler_idle,
        "gos_rt_httptest_server" => rt::gos_rt_httptest_server,
        "gos_rt_image_new" => rt::gos_rt_image_new,
        "gos_rt_image_filled" => rt::gos_rt_image_filled,
        "gos_rt_image_decode_base64" => rt::gos_rt_image_decode_base64,
        "gos_rt_image_width" => rt::gos_rt_image_width,
        "gos_rt_image_height" => rt::gos_rt_image_height,
        "gos_rt_image_pixel" => rt::gos_rt_image_pixel,
        "gos_rt_image_set_pixel" => rt::gos_rt_image_set_pixel,
        "gos_rt_image_encode_png_base64" => rt::gos_rt_image_encode_png_base64,
        "gos_rt_image_encode_jpeg_base64" => rt::gos_rt_image_encode_jpeg_base64,
        "gos_rt_runtime_scheduler_stats_json" => rt::gos_rt_runtime_scheduler_stats_json,
        "gos_rt_pprof_cpu_profile" => rt::gos_rt_pprof_cpu_profile,
        "gos_rt_pprof_heap_profile" => rt::gos_rt_pprof_heap_profile,
        "gos_rt_pprof_goroutine_profile" => rt::gos_rt_pprof_goroutine_profile,
        "gos_rt_pprof_mutex_profile" => rt::gos_rt_pprof_mutex_profile,
        "gos_rt_pprof_block_profile" => rt::gos_rt_pprof_block_profile,
        "gos_rt_pprof_execution_trace" => rt::gos_rt_pprof_execution_trace,
        "gos_rt_pprof_route" => rt::gos_rt_pprof_route,
        "gos_rt_runtime_cycle_collection_supported" => rt::gos_rt_runtime_cycle_collection_supported,
        "gos_rt_parse_i64"           => rt::gos_rt_parse_i64,
        "gos_rt_parse_i64_result"    => rt::gos_rt_parse_i64_result,
        "gos_rt_println_fn_f64" => rt::gos_rt_println_fn_f64,
        "gos_rt_println_fn_i64" => rt::gos_rt_println_fn_i64,
        "gos_rt_println_fn_str_word" => rt::gos_rt_println_fn_str_word,
        "gos_rt_option_and_then" => rt::gos_rt_option_and_then,
        "gos_rt_option_default_with" => rt::gos_rt_option_default_with,
        "gos_rt_option_filter" => rt::gos_rt_option_filter,
        "gos_rt_option_flatten" => rt::gos_rt_option_flatten,
        "gos_rt_option_iter" => rt::gos_rt_option_iter,
        "gos_rt_option_or" => rt::gos_rt_option_or,
        "gos_rt_option_or_else" => rt::gos_rt_option_or_else,
        "gos_rt_option_zip" => rt::gos_rt_option_zip,
        "gos_rt_result_and_then" => rt::gos_rt_result_and_then,
        "gos_rt_result_or_else" => rt::gos_rt_result_or_else,
        "gos_rt_result_to_opt_err" => rt::gos_rt_result_to_opt_err,
        "gos_rt_result_to_opt_ok" => rt::gos_rt_result_to_opt_ok,
        "gos_rt_result_map_err"      => rt::gos_rt_result_map_err,
        "gos_rt_result_map"          => rt::gos_rt_result_map,
        "gos_rt_flag_cell_load_str"  => rt::gos_rt_flag_cell_load_str,
        "gos_rt_flag_cell_load_i64"  => rt::gos_rt_flag_cell_load_i64,
        "gos_rt_flag_cell_load_bool" => rt::gos_rt_flag_cell_load_bool,
        "gos_rt_flag_cell_load_f64"  => rt::gos_rt_flag_cell_load_f64,
        "gos_rt_flag_cell_load_vec"  => rt::gos_rt_flag_cell_load_vec,
        "gos_rt_json_free"           => rt::gos_rt_json_free,
        "gos_rt_json_value_string"   => rt::gos_rt_json_value_string,
        "gos_rt_json_value_int"      => rt::gos_rt_json_value_int,
        "gos_rt_json_value_float"    => rt::gos_rt_json_value_float,
        "gos_rt_json_value_bool"     => rt::gos_rt_json_value_bool,
        "gos_rt_json_value_null"     => rt::gos_rt_json_value_null,
        "gos_rt_json_free_slots"     => rt::gos_rt_json_free_slots,
        "gos_rt_json_value_array"    => rt::gos_rt_json_value_array,
        "gos_rt_json_value_array_owned" => rt::gos_rt_json_value_array_owned,
        "gos_rt_json_value_object_owned" => rt::gos_rt_json_value_object_owned,
        "gos_rt_json_value_object"   => rt::gos_rt_json_value_object,
        "gos_rt_parse_f64"           => rt::gos_rt_parse_f64,
        "gos_rt_i64_chars"           => rt::gos_rt_i64_chars,
        "gos_rt_i64_to_str"          => rt::gos_rt_i64_to_str,
        "gos_rt_u64_to_str"          => rt::gos_rt_u64_to_str,
        "gos_rt_uuid_v4"             => rt::gos_rt_uuid_v4,
        "gos_rt_uuid_v7"             => rt::gos_rt_uuid_v7,
        "gos_rt_uuid_is_valid"       => rt::gos_rt_uuid_is_valid,
        "gos_rt_uuid_normalize"      => rt::gos_rt_uuid_normalize,
        "gos_rt_uuid_simple"         => rt::gos_rt_uuid_simple,
        "gos_rt_f64_to_str"          => rt::gos_rt_f64_to_str,
        "gos_rt_f64_prec_to_str"     => rt::gos_rt_f64_prec_to_str,
        "gos_rt_str_prec_to_str"     => rt::gos_rt_str_prec_to_str,
        "gos_rt_flush_stdout"        => rt::gos_rt_flush_stdout,
        "gos_rt_print_str"           => rt::gos_rt_print_str,
        "gos_rt_print_i64"           => rt::gos_rt_print_i64,
        "gos_rt_print_u64"           => rt::gos_rt_print_u64,
        "gos_rt_print_f64"           => rt::gos_rt_print_f64,
        "gos_rt_print_bool"          => rt::gos_rt_print_bool,
        "gos_rt_print_char"          => rt::gos_rt_print_char,
        "gos_rt_eprint_str"          => rt::gos_rt_eprint_str,
        "gos_rt_eprintln"            => rt::gos_rt_eprintln,
        "gos_rt_io_copy"             => rt::gos_rt_io_copy,
        "gos_rt_io_string_reader"    => rt::gos_rt_io_string_reader,
        "gos_rt_io_buffer_writer"    => rt::gos_rt_io_buffer_writer,
        "gos_rt_io_limit_reader"     => rt::gos_rt_io_limit_reader,
        "gos_rt_io_tee_reader"       => rt::gos_rt_io_tee_reader,
        "gos_rt_io_multi_reader"     => rt::gos_rt_io_multi_reader,
        "gos_rt_io_pipe"             => rt::gos_rt_io_pipe,
        "gos_rt_io_copy_n"           => rt::gos_rt_io_copy_n,
        "gos_rt_io_drain"            => rt::gos_rt_io_drain,
        "gos_rt_io_contents"         => rt::gos_rt_io_contents,
        "gos_rt_io_write_str"        => rt::gos_rt_io_write_str,
        "gos_rt_io_close_writer"     => rt::gos_rt_io_close_writer,
        "gos_rt_io_read_all"         => rt::gos_rt_io_read_all,
        "gos_rt_io_stdin"            => rt::gos_rt_io_stdin,
        "gos_rt_io_stdout"           => rt::gos_rt_io_stdout,
        "gos_rt_io_stderr"           => rt::gos_rt_io_stderr,
        "gos_rt_stream_write_byte"   => rt::gos_rt_stream_write_byte,
        "gos_rt_stream_write_str"    => rt::gos_rt_stream_write_str,
        "gos_rt_stream_flush"        => rt::gos_rt_stream_flush,
        "gos_rt_stream_read_line"    => rt::gos_rt_stream_read_line,
        "gos_rt_stream_next_line"    => rt::gos_rt_stream_next_line,
        "gos_rt_stream_read_to_string" => rt::gos_rt_stream_read_to_string,
        "gos_rt_println"             => rt::gos_rt_println,
        "gos_rt_stdout_acquire"      => rt::gos_rt_stdout_acquire,
        "gos_rt_stdout_release"      => rt::gos_rt_stdout_release,
        "gos_rt_vec_new"             => rt::gos_rt_vec_new,
        "gos_rt_vec_with_capacity"   => rt::gos_rt_vec_with_capacity,
        "gos_rt_vec_repeat_primitive" => rt::gos_rt_vec_repeat_primitive,
        "gos_rt_vec_capacity"        => rt::gos_rt_vec_capacity,
        "gos_rt_vec_from_arr"        => rt::gos_rt_vec_from_arr,
        "gos_rt_vec_borrow_arr"      => rt::gos_rt_vec_borrow_arr,
        "gos_rt_nested_arr_to_vec"   => rt::gos_rt_nested_arr_to_vec,
        "gos_rt_vec_len"             => rt::gos_rt_vec_len,
        "gos_rt_vec_push"            => rt::gos_rt_vec_push,
        "gos_rt_vec_push_i64"        => rt::gos_rt_vec_push_i64,
        "gos_rt_vec_reserve_at_least" => rt::gos_rt_vec_reserve_at_least,
        "gos_rt_vec_reserve_exact"   => rt::gos_rt_vec_reserve_exact,
        "gos_rt_vec_get_ptr"         => rt::gos_rt_vec_get_ptr,
        "gos_rt_vec_pop"             => rt::gos_rt_vec_pop,
        "gos_rt_vec_pop_opt"         => rt::gos_rt_vec_pop_opt,
        "gos_rt_vec_pop_into"        => rt::gos_rt_vec_pop_into,
        "gos_rt_vec_slice"           => rt::gos_rt_vec_slice,
        "gos_rt_map_new"             => rt::gos_rt_map_new,
        "gos_rt_map_len"             => rt::gos_rt_map_len,
        "gos_rt_map_insert"          => rt::gos_rt_map_insert,
        "gos_rt_map_get"             => rt::gos_rt_map_get,
        "gos_rt_map_get_or_i64"      => rt::gos_rt_map_get_or_i64,
        "gos_rt_map_inc_i64"         => rt::gos_rt_map_inc_i64,
        "gos_rt_map_or_insert_str_i64" => rt::gos_rt_map_or_insert_str_i64,
        "gos_rt_map_or_insert_typed_str_i64" => rt::gos_rt_map_or_insert_typed_str_i64,
        "gos_rt_map_or_insert_i64_i64" => rt::gos_rt_map_or_insert_i64_i64,
        "gos_rt_map_remove"          => rt::gos_rt_map_remove,
        "gos_rt_map_insert_i64_i64"  => rt::gos_rt_map_insert_i64_i64,
        "gos_rt_map_insert_i64_i64_opt" => rt::gos_rt_map_insert_i64_i64_opt,
        "gos_rt_map_insert_skey"     => rt::gos_rt_map_insert_skey,
        "gos_rt_map_insert_skey_opt" => rt::gos_rt_map_insert_skey_opt,
        "gos_rt_map_get_skey_opt"    => rt::gos_rt_map_get_skey_opt,
        "gos_rt_map_pop_skey"        => rt::gos_rt_map_pop_skey,
        "gos_rt_map_keys_skey"       => rt::gos_rt_map_keys_skey,
        "gos_rt_map_contains_skey"   => rt::gos_rt_map_contains_skey,
        "gos_rt_map_get_or_skey"     => rt::gos_rt_map_get_or_skey,
        "gos_rt_map_or_insert_skey"  => rt::gos_rt_map_or_insert_skey,
        "gos_rt_map_inc_skey"        => rt::gos_rt_map_inc_skey,
        "gos_rt_map_insert_ekey_opt" => rt::gos_rt_map_insert_ekey_opt,
        "gos_rt_map_get_ekey_opt" => rt::gos_rt_map_get_ekey_opt,
        "gos_rt_map_contains_ekey" => rt::gos_rt_map_contains_ekey,
        "gos_rt_map_pop_ekey" => rt::gos_rt_map_pop_ekey,
        "gos_rt_map_get_or_ekey" => rt::gos_rt_map_get_or_ekey,
        "gos_rt_map_or_insert_ekey" => rt::gos_rt_map_or_insert_ekey,
        "gos_rt_map_inc_ekey" => rt::gos_rt_map_inc_ekey,
        "gos_rt_map_keys_ekey" => rt::gos_rt_map_keys_ekey,
        "gos_rt_map_get_i64"         => rt::gos_rt_map_get_i64,
        "gos_rt_map_remove_i64"      => rt::gos_rt_map_remove_i64,
        "gos_rt_map_contains_key_i64" => rt::gos_rt_map_contains_key_i64,
        "gos_rt_map_insert_str_i64"  => rt::gos_rt_map_insert_str_i64,
        "gos_rt_map_insert_typed_str_i64" => rt::gos_rt_map_insert_typed_str_i64,
        "gos_rt_map_insert_str_i64_opt" => rt::gos_rt_map_insert_str_i64_opt,
        "gos_rt_map_insert_typed_str_i64_opt" => rt::gos_rt_map_insert_typed_str_i64_opt,
        "gos_rt_map_get_str_i64"     => rt::gos_rt_map_get_str_i64,
        "gos_rt_map_get_typed_str_i64" => rt::gos_rt_map_get_typed_str_i64,
        "gos_rt_map_get_typed_str_opt" => rt::gos_rt_map_get_typed_str_opt,
        "gos_rt_map_insert_str_str"  => rt::gos_rt_map_insert_str_str,
        "gos_rt_map_insert_str_str_opt" => rt::gos_rt_map_insert_str_str_opt,
        "gos_rt_map_get_str_str"     => rt::gos_rt_map_get_str_str,
        "gos_rt_map_contains_key_str" => rt::gos_rt_map_contains_key_str,
        "gos_rt_map_contains_key_typed_str" => rt::gos_rt_map_contains_key_typed_str,
        "gos_rt_map_remove_str"      => rt::gos_rt_map_remove_str,
        "gos_rt_map_remove_typed_str" => rt::gos_rt_map_remove_typed_str,
        "gos_rt_map_clear"           => rt::gos_rt_map_clear,
        "gos_rt_map_inc_at_str_i64"  => rt::gos_rt_map_inc_at_str_i64,
        "gos_rt_map_free"            => rt::gos_rt_map_free,
        "gos_rt_vec_free"            => rt::gos_rt_vec_free,
        "gos_rt_vec_retain"          => rt::gos_rt_vec_retain,
        "gos_rt_vec_mark_shared"     => rt::gos_rt_vec_mark_shared,
        "gos_rt_set_free"            => rt::gos_rt_set_free,
        "gos_rt_map_keys_i64"        => rt::gos_rt_map_keys_i64,
        "gos_rt_map_values_i64"      => rt::gos_rt_map_values_i64,
        "gos_rt_map_keys_str"        => rt::gos_rt_map_keys_str,
        "gos_rt_map_values_str"      => rt::gos_rt_map_values_str,
        "gos_rt_map_get_or_str_i64"  => rt::gos_rt_map_get_or_str_i64,
        "gos_rt_map_get_or_typed_str_i64" => rt::gos_rt_map_get_or_typed_str_i64,
        "gos_rt_map_get_or_str_str"  => rt::gos_rt_map_get_or_str_str,
        "gos_rt_map_get_or_i64_str"  => rt::gos_rt_map_get_or_i64_str,
        "gos_rt_map_insert_i64_str"  => rt::gos_rt_map_insert_i64_str,
        "gos_rt_map_insert_i64_str_opt" => rt::gos_rt_map_insert_i64_str_opt,
        "gos_rt_map_get_i64_str"     => rt::gos_rt_map_get_i64_str,
        "gos_rt_map_format"          => rt::gos_rt_map_format,
        "gos_rt_map_format_tagged"   => rt::gos_rt_map_format_tagged,
        "gos_rt_set_format_i64"      => rt::gos_rt_set_format_i64,
        "gos_rt_set_format_string"   => rt::gos_rt_set_format_string,
        "gos_rt_set_format_u64"      => rt::gos_rt_set_format_u64,
        "gos_rt_deque_format"        => rt::deque::gos_rt_deque_format,
        "gos_rt_queue_format"        => rt::deque::gos_rt_queue_format,
        "gos_rt_stack_format"        => rt::deque::gos_rt_stack_format,
        "gos_rt_bheap_max_format"    => rt::gos_rt_bheap_max_format,
        "gos_rt_bheap_min_format"    => rt::gos_rt_bheap_min_format,
        "gos_rt_json_parse"          => rt::gos_rt_json_parse,
        "gos_rt_json_render"         => rt::gos_rt_json_render,
        "gos_rt_json_writer_begin_array" => rt::gos_rt_json_writer_begin_array,
        "gos_rt_json_writer_begin_object" => rt::gos_rt_json_writer_begin_object,
        "gos_rt_json_writer_bool" => rt::gos_rt_json_writer_bool,
        "gos_rt_json_writer_end_array" => rt::gos_rt_json_writer_end_array,
        "gos_rt_json_writer_end_object" => rt::gos_rt_json_writer_end_object,
        "gos_rt_json_writer_f64" => rt::gos_rt_json_writer_f64,
        "gos_rt_json_writer_finish" => rt::gos_rt_json_writer_finish,
        "gos_rt_json_writer_i64" => rt::gos_rt_json_writer_i64,
        "gos_rt_json_writer_key" => rt::gos_rt_json_writer_key,
        "gos_rt_json_writer_new" => rt::gos_rt_json_writer_new,
        "gos_rt_json_writer_new_pretty" => rt::gos_rt_json_writer_new_pretty,
        "gos_rt_json_writer_null" => rt::gos_rt_json_writer_null,
        "gos_rt_json_writer_str" => rt::gos_rt_json_writer_str,
        "gos_rt_json_writer_value" => rt::gos_rt_json_writer_value,
        "gos_rt_json_display"        => rt::gos_rt_json_display,
        "gos_rt_json_get"            => rt::gos_rt_json_get,
        "gos_rt_json_get_opt"        => rt::gos_rt_json_get_opt,
        "gos_rt_json_keys_opt"       => rt::gos_rt_json_keys_opt,
        "gos_rt_json_as_array_opt"   => rt::gos_rt_json_as_array_opt,
        "gos_rt_json_at"             => rt::gos_rt_json_at,
        "gos_rt_json_len"            => rt::gos_rt_json_len,
        "gos_rt_json_is_null"        => rt::gos_rt_json_is_null,
        "gos_rt_json_as_i64"         => rt::gos_rt_json_as_i64,
        "gos_rt_json_as_f64"         => rt::gos_rt_json_as_f64,
        "gos_rt_json_as_str"         => rt::gos_rt_json_as_str,
        "gos_rt_json_as_bool"        => rt::gos_rt_json_as_bool,
        "gos_rt_json_identity"       => rt::gos_rt_json_identity,
        "gos_rt_chan_new"            => rt::gos_rt_chan_new,
        "gos_rt_chan_send"               => rt::gos_rt_chan_send,
        "gos_rt_chan_set_elem_kind"      => rt::gos_rt_chan_set_elem_kind,
        "gos_rt_chan_set_elem_desc"      => rt::gos_rt_chan_set_elem_desc,
        "gos_rt_chan_try_send"           => rt::gos_rt_chan_try_send,
        "gos_rt_chan_recv"               => rt::gos_rt_chan_recv,
        "gos_rt_chan_recv_option"        => rt::gos_rt_chan_recv_option,
        "gos_rt_chan_try_recv"           => rt::gos_rt_chan_try_recv,
        "gos_rt_chan_try_recv_option"    => rt::gos_rt_chan_try_recv_option,
        "gos_rt_chan_close"              => rt::gos_rt_chan_close,
        "gos_rt_go_yield"            => rt::gos_rt_go_yield,
        "gos_rt_spawn"               => rt::gos_rt_spawn,
        "gos_rt_spawn_ex"            => rt::gos_rt_spawn_ex,
        "gos_rt_cohort_push"         => rt::cohort::gos_rt_cohort_push,
        "gos_rt_cohort_join"         => rt::cohort::gos_rt_cohort_join,
        "gos_rt_cohort_pop"          => rt::cohort::gos_rt_cohort_pop,
        "gos_rt_lifecycle_ready" => rt::lifecycle::gos_rt_lifecycle_ready,
        "gos_rt_lifecycle_set_ready" => rt::lifecycle::gos_rt_lifecycle_set_ready,
        "gos_rt_lifecycle_is_ready" => rt::lifecycle::gos_rt_lifecycle_is_ready,
        "gos_rt_lifecycle_shutdown" => rt::lifecycle::gos_rt_lifecycle_shutdown,
        "gos_rt_lifecycle_is_shutting_down" => rt::lifecycle::gos_rt_lifecycle_is_shutting_down,
        "gos_rt_lifecycle_await_shutdown" => rt::lifecycle::gos_rt_lifecycle_await_shutdown,
        "gos_rt_lifecycle_notify_status" => rt::lifecycle::gos_rt_lifecycle_notify_status,
        "gos_rt_http_server_new" => rt::http_server_handle::gos_rt_http_server_new,
        "gos_rt_time_freeze" => rt::gos_rt_time_freeze,
        "gos_rt_smtp_send" => rt::gos_rt_smtp_send,
        "gos_rt_smtp_send_auth" => rt::gos_rt_smtp_send_auth,
        "gos_rt_httptest_record" => rt::testing::gos_rt_httptest_record,
        "gos_rt_time_advance" => rt::gos_rt_time_advance,
        "gos_rt_time_unfreeze" => rt::gos_rt_time_unfreeze,
        "gos_rt_time_is_frozen" => rt::gos_rt_time_is_frozen,
        "gos_rt_http_response_stream_open" => rt::http_stream_writer::gos_rt_http_response_stream_open,
        "gos_rt_http_response_stream_write" => rt::http_stream_writer::gos_rt_http_response_stream_write,
        "gos_rt_http_response_stream_write_bytes" => rt::http_stream_writer::gos_rt_http_response_stream_write_bytes,
        "gos_rt_http_response_stream_close" => rt::http_stream_writer::gos_rt_http_response_stream_close,
        "gos_rt_http_response_stream_is_open" => rt::http_stream_writer::gos_rt_http_response_stream_is_open,
        "gos_rt_http_server_read_header_timeout_ms" => rt::http_server_handle::gos_rt_http_server_read_header_timeout_ms,
        "gos_rt_http_server_request_timeout_ms" => rt::http_server_handle::gos_rt_http_server_request_timeout_ms,
        "gos_rt_http_server_read_body_timeout_ms" => rt::http_server_handle::gos_rt_http_server_read_body_timeout_ms,
        "gos_rt_http_server_write_timeout_ms" => rt::http_server_handle::gos_rt_http_server_write_timeout_ms,
        "gos_rt_http_server_idle_timeout_ms" => rt::http_server_handle::gos_rt_http_server_idle_timeout_ms,
        "gos_rt_http_server_max_header_bytes" => rt::http_server_handle::gos_rt_http_server_max_header_bytes,
        "gos_rt_http_server_max_body_bytes" => rt::http_server_handle::gos_rt_http_server_max_body_bytes,
        "gos_rt_http_server_max_connections" => rt::http_server_handle::gos_rt_http_server_max_connections,
        "gos_rt_http_server_server_name" => rt::http_server_handle::gos_rt_http_server_server_name,
        "gos_rt_http_server_listen" => rt::http_server_handle::gos_rt_http_server_listen,
        "gos_rt_http_server_addr" => rt::http_server_handle::gos_rt_http_server_addr,
        "gos_rt_http_server_serve" => rt::http_server_handle::gos_rt_http_server_serve,
        "gos_rt_http_server_shutdown" => rt::http_server_handle::gos_rt_http_server_shutdown,
        "gos_rt_cohort_cancelled"    => rt::cohort::gos_rt_cohort_cancelled,
        "gos_rt_cohort_cancel"       => rt::cohort::gos_rt_cohort_cancel,
        "gos_rt_join"                => rt::gos_rt_join,
        "gos_rt_sleep_ns"            => rt::gos_rt_sleep_ns,
        "gos_rt_sleep_ms"            => rt::gos_rt_sleep_ms,
        "gos_rt_now_ns"              => rt::gos_rt_now_ns,
        "gos_rt_gc_alloc"            => rt::gos_rt_gc_alloc,
        "gos_rt_aggr_alloc"          => rt::gos_rt_aggr_alloc,
        "gos_rt_aggr_free"           => rt::gos_rt_aggr_free,
        "gos_rt_rc_alloc"            => rt::gos_rt_rc_alloc,
        "gos_rt_rc_alloc_reuse"      => rt::gos_rt_rc_alloc_reuse,
        "gos_rt_rc_drop_reuse"       => rt::gos_rt_rc_drop_reuse,
        "gos_rt_rc_retain"           => rt::gos_rt_rc_retain,
        "gos_rt_rc_release"          => rt::gos_rt_rc_release,
        "gos_rt_rc_downgrade"        => rt::gos_rt_rc_downgrade,
        "gos_rt_rc_weak_retain"      => rt::gos_rt_rc_weak_retain,
        "gos_rt_rc_weak_release"     => rt::gos_rt_rc_weak_release,
        "gos_rt_rc_weak_upgrade"     => rt::gos_rt_rc_weak_upgrade,
        "gos_rt_rc_weak_upgrade_opt" => rt::gos_rt_rc_weak_upgrade_opt,
        "gos_rt_arena_push"         => rt::gos_rt_arena_push,
        "gos_rt_arena_pop"          => rt::gos_rt_arena_pop,
        "gos_rt_collect_cycles"      => rt::gos_rt_collect_cycles,
        "gos_rt_goroutine_panicked"  => rt::gos_rt_goroutine_panicked,
        "gos_rt_callback_invoke"     => rt::gos_rt_callback_invoke,
        "gos_rt_callback_register"   => rt::gos_rt_callback_register,
        "gos_rt_callback_unregister" => rt::gos_rt_callback_unregister,
        "gos_rt_env_home_dir"        => rt::gos_rt_env_home_dir,
        "gos_rt_env_vars"            => rt::gos_rt_env_vars,
        "gos_rt_env_temp_dir"        => rt::gos_rt_env_temp_dir,
        "gos_rt_vec_new_typed"       => rt::gos_rt_vec_new_typed,
        "gos_rt_vec_with_capacity_typed" => rt::gos_rt_vec_with_capacity_typed,
        "gos_rt_binding_map_free"    => rt::gos_rt_binding_map_free,
        "gos_rt_map_eq"              => rt::gos_rt_map_eq,
        "gos_rt_dyn_nil" => rt::gos_rt_dyn_nil,
        "gos_rt_dyn_bool" => rt::gos_rt_dyn_bool,
        "gos_rt_dyn_int" => rt::gos_rt_dyn_int,
        "gos_rt_dyn_float" => rt::gos_rt_dyn_float,
        "gos_rt_dyn_char" => rt::gos_rt_dyn_char,
        "gos_rt_dyn_string" => rt::gos_rt_dyn_string,
        "gos_rt_dyn_bytes" => rt::gos_rt_dyn_bytes,
        "gos_rt_dyn_list" => rt::gos_rt_dyn_list,
        "gos_rt_dyn_map" => rt::gos_rt_dyn_map,
        "gos_rt_dyn_tagged" => rt::gos_rt_dyn_tagged,
        "gos_rt_dyn_kind" => rt::gos_rt_dyn_kind,
        "gos_rt_dyn_name" => rt::gos_rt_dyn_name,
        "gos_rt_dyn_kind_name" => rt::gos_rt_dyn_kind_name,
        "gos_rt_dyn_len" => rt::gos_rt_dyn_len,
        "gos_rt_dyn_at" => rt::gos_rt_dyn_at,
        "gos_rt_dyn_arm_index" => rt::gos_rt_dyn_arm_index,
        "gos_rt_dyn_field_i64" => rt::gos_rt_dyn_field_i64,
        "gos_rt_dyn_field_f64" => rt::gos_rt_dyn_field_f64,
        "gos_rt_dyn_field_str" => rt::gos_rt_dyn_field_str,
        "gos_rt_dyn_field_dyn" => rt::gos_rt_dyn_field_dyn,
        "gos_rt_dyn_key_at" => rt::gos_rt_dyn_key_at,
        "gos_rt_dyn_as_i64" => rt::gos_rt_dyn_as_i64,
        "gos_rt_dyn_as_f64" => rt::gos_rt_dyn_as_f64,
        "gos_rt_dyn_as_bool" => rt::gos_rt_dyn_as_bool,
        "gos_rt_dyn_as_char" => rt::gos_rt_dyn_as_char,
        "gos_rt_dyn_as_str" => rt::gos_rt_dyn_as_str,
        "gos_rt_dyn_as_bytes" => rt::gos_rt_dyn_as_bytes,
        "gos_rt_dyn_clone" => rt::gos_rt_dyn_clone,
        "gos_rt_dyn_free" => rt::gos_rt_dyn_free,
        "gos_rt_dyn_eq" => rt::gos_rt_dyn_eq,
        "gos_rt_dyn_format" => rt::gos_rt_dyn_format,
        "gos_rt_dyn_from_binding_variant" => rt::gos_rt_dyn_from_binding_variant,
        "gos_rt_set_eq"              => rt::gos_rt_set_eq,
        "gos_rt_binding_bytes_from_vec" => rt::gos_rt_binding_bytes_from_vec,
        "gos_rt_binding_bytes_to_vec" => rt::gos_rt_binding_bytes_to_vec,
        "gos_rt_binding_map_from_map" => rt::gos_rt_binding_map_from_map,
        "gos_rt_binding_map_to_map"  => rt::gos_rt_binding_map_to_map,
        "gos_rt_binding_struct_from_slots" => rt::gos_rt_binding_struct_from_slots,
        "gos_rt_binding_struct_to_slots" => rt::gos_rt_binding_struct_to_slots,
        "gos_rt_binding_tuple_from_slots" => rt::gos_rt_binding_tuple_from_slots,
        "gos_rt_binding_tuple_to_slots" => rt::gos_rt_binding_tuple_to_slots,
        "gos_rt_panic_oob"           => rt::gos_rt_panic_oob,
        "gos_rt_gc_deregister"       => rt::gos_rt_gc_deregister,
        "gos_rt_gc_collect"          => rt::gos_rt_gc_collect,
        "gos_rt_gc_alloc_count"      => rt::gos_rt_gc_alloc_count,
        "gos_rt_gc_reset"            => rt::gos_rt_gc_reset,
        "gos_rt_arena_save"          => rt::gos_rt_arena_save,
        "gos_rt_arena_restore"       => rt::gos_rt_arena_restore,
        "gos_rt_http_serve"          => rt::gos_rt_http_serve,
        "gos_rt_http2_bind_and_run_h2c" => rt::gos_rt_http2_bind_and_run_h2c,
        "gos_rt_chunked_encode"      => rt::gos_rt_chunked_encode,
        "gos_rt_chunked_decode"      => rt::gos_rt_chunked_decode,
        "gos_rt_sse_encode_event"    => rt::gos_rt_sse_encode_event,
        "gos_rt_sse_encode_comment"  => rt::gos_rt_sse_encode_comment,
        "gos_rt_sse_encode_retry"    => rt::gos_rt_sse_encode_retry,
        "gos_rt_mw_new_request_id"   => rt::gos_rt_mw_new_request_id,
        "gos_rt_mw_accepts_gzip"     => rt::gos_rt_mw_accepts_gzip,
        "gos_rt_mw_decode_basic_auth" => rt::gos_rt_mw_decode_basic_auth,
        "gos_rt_ws_accept"           => rt::gos_rt_ws_accept,
        "gos_rt_ws_is_upgrade"       => rt::gos_rt_ws_is_upgrade,
        "gos_rt_ws_accept_key"       => rt::gos_rt_ws_accept_key,
        "gos_rt_static_mime_for_path" => rt::gos_rt_static_mime_for_path,
        "gos_rt_static_serve_file"   => rt::gos_rt_static_serve_file,
        "gos_rt_router_new"           => rt::gos_rt_router_new,
        "gos_rt_router_add"           => rt::gos_rt_router_add,
        "gos_rt_router_get"           => rt::gos_rt_router_get,
        "gos_rt_router_post"          => rt::gos_rt_router_post,
        "gos_rt_router_put"           => rt::gos_rt_router_put,
        "gos_rt_router_delete"        => rt::gos_rt_router_delete,
        "gos_rt_router_patch"         => rt::gos_rt_router_patch,
        "gos_rt_router_head"          => rt::gos_rt_router_head,
        "gos_rt_router_options"       => rt::gos_rt_router_options,
        "gos_rt_router_add_fn"        => rt::gos_rt_router_add_fn,
        "gos_rt_router_add_pattern"   => rt::gos_rt_router_add_pattern,
        "gos_rt_router_lookup"        => rt::gos_rt_router_lookup,
        "gos_rt_router_get_fn"        => rt::gos_rt_router_get_fn,
        "gos_rt_router_post_fn"       => rt::gos_rt_router_post_fn,
        "gos_rt_router_put_fn"        => rt::gos_rt_router_put_fn,
        "gos_rt_router_delete_fn"     => rt::gos_rt_router_delete_fn,
        "gos_rt_router_patch_fn"      => rt::gos_rt_router_patch_fn,
        "gos_rt_router_head_fn"       => rt::gos_rt_router_head_fn,
        "gos_rt_router_options_fn"    => rt::gos_rt_router_options_fn,
        "gos_rt_router_serve"         => rt::gos_rt_router_serve,
        "gos_rt_file_server_new"      => rt::gos_rt_file_server_new,
        "gos_rt_file_server_serve"    => rt::gos_rt_file_server_serve,
        "gos_rt_native_client_new"    => rt::gos_rt_native_client_new,
        "gos_rt_native_client_get"    => rt::gos_rt_native_client_get,
        "gos_rt_nc_get"               => rt::gos_rt_nc_get,
        "gos_rt_nc_delete"            => rt::gos_rt_nc_delete,
        "gos_rt_nc_post"              => rt::gos_rt_nc_post,
        "gos_rt_nc_put"               => rt::gos_rt_nc_put,
        "gos_rt_proxy_new"            => rt::gos_rt_proxy_new,
        "gos_rt_proxy_forward"        => rt::gos_rt_proxy_forward,
        "gos_rt_proxy_forward_url"    => rt::gos_rt_proxy_forward_url,
        "gos_rt_ws_frame_text"        => rt::gos_rt_ws_frame_text,
        "gos_rt_panic"               => rt::gos_rt_panic,
        "gos_rt_panic_oob"           => rt::gos_rt_panic_oob,
        "gos_rt_stack_push"          => rt::gos_rt_stack_push,
        "gos_rt_stack_pop"           => rt::gos_rt_stack_pop,
        "gos_rt_stack_set_line"      => rt::gos_rt_stack_set_line,
        "gos_rt_cov_record"          => rt::gos_rt_cov_record,
        "gos_rt_cov_bump"            => rt::gos_rt_cov_bump,
        "gos_rt_cov_reset"           => rt::gos_rt_cov_reset,
        "gos_rt_cov_set_enabled"     => rt::gos_rt_cov_set_enabled,
        "gos_rt_exit"                => rt::gos_rt_exit,
        "gos_rt_process_id"          => rt::gos_rt_process_id,
        "gos_rt_process_abort"       => rt::gos_rt_process_abort,
        "gos_rt_time_now"            => rt::gos_rt_time_now,
        "gos_rt_time_add_date_raw"   => rt::gos_rt_time_add_date_raw,
        "gos_rt_time_civil_raw"      => rt::gos_rt_time_civil_raw,
        "gos_rt_time_fixed_location_raw" => rt::gos_rt_time_fixed_location_raw,
        "gos_rt_time_format_in_raw"  => rt::gos_rt_time_format_in_raw,
        "gos_rt_time_location_raw"   => rt::gos_rt_time_location_raw,
        "gos_rt_time_resolve_raw"    => rt::gos_rt_time_resolve_raw,
        "gos_rt_math_sqrt"           => rt::gos_rt_math_sqrt,
        "gos_rt_math_pow"            => rt::gos_rt_math_pow,
        "gos_rt_math_sin"            => rt::gos_rt_math_sin,
        "gos_rt_math_cos"            => rt::gos_rt_math_cos,
        "gos_rt_math_log"            => rt::gos_rt_math_log,
        "gos_rt_math_exp"            => rt::gos_rt_math_exp,
        "gos_rt_math_abs"            => rt::gos_rt_math_abs,
        "gos_rt_math_floor"          => rt::gos_rt_math_floor,
        "gos_rt_math_ceil"           => rt::gos_rt_math_ceil,
        "gos_rt_time_now_ms"         => rt::gos_rt_time_now_ms,
        // Fn-trait coercion trampolines (closure_fn_trait_plan.md).
        // Emitted by the cranelift codegen when a bare `fn`/`fn item`
        // value is wrapped into a `Fn(args) -> ret` slot - the env
        // blob's offset 0 holds one of these, offset 8 holds the
        // real fn ptr.
        "gos_rt_fn_tramp_0"          => rt::gos_rt_fn_tramp_0,
        "gos_rt_fn_tramp_1"          => rt::gos_rt_fn_tramp_1,
        "gos_rt_fn_tramp_2"          => rt::gos_rt_fn_tramp_2,
        "gos_rt_fn_tramp_3"          => rt::gos_rt_fn_tramp_3,
        "gos_rt_fn_tramp_4"          => rt::gos_rt_fn_tramp_4,
        "gos_rt_fn_tramp_5"          => rt::gos_rt_fn_tramp_5,
        "gos_rt_fn_tramp_6"          => rt::gos_rt_fn_tramp_6,
        "gos_rt_fn_tramp_7"          => rt::gos_rt_fn_tramp_7,
        "gos_rt_fn_tramp_8"          => rt::gos_rt_fn_tramp_8,
        // Stringification helpers for compound `println!` /
        // `format!`. The codegen emits these whenever an arg's
        // print-kind is bool or char.
        "gos_rt_bool_to_str"         => rt::gos_rt_bool_to_str,
        "gos_rt_char_to_str"         => rt::gos_rt_char_to_str,
        // Block-write helpers used by `Stream::write_byte_array`
        // for bulk per-line byte dumps.
        "gos_rt_stream_write_byte_array" => rt::gos_rt_stream_write_byte_array,
        // Heap-allocated i64 vector - `I64Vec` in source.
        // Used as a shared scratch buffer by goroutine workers.
        "gos_rt_heap_i64_new"        => rt::gos_rt_heap_i64_new,
        "gos_rt_heap_i64_free"       => rt::gos_rt_heap_i64_free,
        "gos_rt_heap_i64_get"        => rt::gos_rt_heap_i64_get,
        "gos_rt_heap_i64_set"        => rt::gos_rt_heap_i64_set,
        "gos_rt_heap_i64_len"        => rt::gos_rt_heap_i64_len,
        "gos_rt_heap_i64_write_lines_to_stdout"
                                     => rt::gos_rt_heap_i64_write_lines_to_stdout,
        "gos_rt_heap_i64_write_bytes_to_stdout"
                                     => rt::gos_rt_heap_i64_write_bytes_to_stdout,
        // U8Vec - 1-byte-per-element heap vec for byte-oriented
        // scratch buffers. Same shape as the i64 family but
        // with byte-aligned storage.
        "gos_rt_heap_u8_new"         => rt::gos_rt_heap_u8_new,
        "gos_rt_heap_u8_free"        => rt::gos_rt_heap_u8_free,
        "gos_rt_heap_u8_count_kmers" => rt::gos_rt_heap_u8_count_kmers,
        "gos_rt_heap_u8_count_pairs" => rt::gos_rt_heap_u8_count_pairs,
        "gos_rt_heap_u8_count_singles" => rt::gos_rt_heap_u8_count_singles,
        "gos_rt_heap_u8_window_key" => rt::gos_rt_heap_u8_window_key,
        "gos_rt_heap_u8_get"         => rt::gos_rt_heap_u8_get,
        "gos_rt_heap_u8_set"         => rt::gos_rt_heap_u8_set,
        "gos_rt_heap_u8_len"         => rt::gos_rt_heap_u8_len,
        "gos_rt_heap_u8_to_string"   => rt::gos_rt_heap_u8_to_string,
        "gos_rt_heap_u8_write_lines_to_stdout"
                                     => rt::gos_rt_heap_u8_write_lines_to_stdout,
        "gos_rt_heap_u8_write_bytes_to_stdout"
                                     => rt::gos_rt_heap_u8_write_bytes_to_stdout,
        // Sync primitives + LCG jump used by goroutine
        // worker patterns.
        "gos_rt_mutex_new"           => rt::gos_rt_mutex_new,
        "gos_rt_mutex_lock"          => rt::gos_rt_mutex_lock,
        "gos_rt_mutex_unlock"        => rt::gos_rt_mutex_unlock,
        "gos_rt_wg_new"              => rt::gos_rt_wg_new,
        "gos_rt_wg_add"              => rt::gos_rt_wg_add,
        "gos_rt_wg_done"             => rt::gos_rt_wg_done,
        "gos_rt_wg_wait"             => rt::gos_rt_wg_wait,
        "gos_rt_wg_error"            => rt::gos_rt_wg_error,
        "gos_rt_wg_error_clear"      => rt::gos_rt_wg_error_clear,
        "gos_rt_atomic_i64_new"      => rt::gos_rt_atomic_i64_new,
        "gos_rt_atomic_bool_new"     => rt::gos_rt_atomic_bool_new,
        "gos_rt_atomic_bool_cas"     => rt::gos_rt_atomic_bool_cas,
        "gos_rt_atomic_bool_load"    => rt::gos_rt_atomic_bool_load,
        "gos_rt_atomic_bool_store"   => rt::gos_rt_atomic_bool_store,
        "gos_rt_atomic_i64_load"     => rt::gos_rt_atomic_i64_load,
        "gos_rt_atomic_i64_store"    => rt::gos_rt_atomic_i64_store,
        "gos_rt_atomic_i64_fetch_add"=> rt::gos_rt_atomic_i64_fetch_add,
        "gos_rt_atomic_i64_fetch_sub"=> rt::gos_rt_atomic_i64_fetch_sub,
        "gos_rt_atomic_i64_load_acquire"
                                     => rt::gos_rt_atomic_i64_load_acquire,
        "gos_rt_atomic_i64_store_release"
                                     => rt::gos_rt_atomic_i64_store_release,
        "gos_rt_atomic_i64_load_relaxed"
                                     => rt::gos_rt_atomic_i64_load_relaxed,
        "gos_rt_atomic_i64_store_relaxed"
                                     => rt::gos_rt_atomic_i64_store_relaxed,
        "gos_rt_atomic_i64_fetch_add_acqrel"
                                     => rt::gos_rt_atomic_i64_fetch_add_acqrel,
        "gos_rt_atomic_i64_cas"      => rt::gos_rt_atomic_i64_cas,
        "gos_rt_atomic_i64_cas_acq_rel"
                                     => rt::gos_rt_atomic_i64_cas_acq_rel,
        "gos_rt_atomic_i64_swap"     => rt::gos_rt_atomic_i64_swap,
        "gos_rt_preempt_check"       => preempt::gos_rt_preempt_check,
        "gos_rt_preempt_check_and_yield"
                                     => preempt::gos_rt_preempt_check_and_yield,
        "gos_rt_stdout_acquire"      => rt::gos_rt_stdout_acquire,
        "gos_rt_stdout_release"      => rt::gos_rt_stdout_release,
        "gos_rt_sync_i64_new"        => rt::gos_rt_sync_i64_new,
        "gos_rt_sync_i64_drop"       => rt::gos_rt_sync_i64_drop,
        "gos_rt_sync_i64_len"        => rt::gos_rt_sync_i64_len,
        "gos_rt_sync_i64_get"        => rt::gos_rt_sync_i64_get,
        "gos_rt_sync_i64_set"        => rt::gos_rt_sync_i64_set,
        "gos_rt_sync_i64_push"       => rt::gos_rt_sync_i64_push,
        "gos_rt_sync_i64_add"        => rt::gos_rt_sync_i64_add,
        "gos_rt_sync_u8_new"         => rt::gos_rt_sync_u8_new,
        "gos_rt_sync_u8_drop"        => rt::gos_rt_sync_u8_drop,
        "gos_rt_sync_u8_len"         => rt::gos_rt_sync_u8_len,
        "gos_rt_sync_u8_get"         => rt::gos_rt_sync_u8_get,
        "gos_rt_sync_u8_set"         => rt::gos_rt_sync_u8_set,
        "gos_rt_sync_u8_push"        => rt::gos_rt_sync_u8_push,
        "gos_rt_lcg_jump"            => rt::gos_rt_lcg_jump,
    }
    // Register every remaining `gos_rt_*` symbol from the ABI registry's
    // address table. The explicit `reg!` block above covers the common
    // symbols with their bespoke spellings; this pass closes the gap so
    // that a body calling any registered runtime helper (math, strconv,
    // unicode, encoding, …) resolves at JIT-finalize time instead of
    // failing the whole module and collapsing the program to the
    // interpreter. A runtime test asserts the table is registry-complete.
    for (name, addr) in rt::runtime_symbol_addrs() {
        if names.insert(name) {
            builder.symbol(name, addr);
        }
    }
    names
}

/// Walks both binding-symbol registration paths and registers each
/// `(name, addr)` with the JIT builder.
///
/// - [`crate::native_symbols::NATIVE_SYMBOLS`] - the link-time
///   `linkme::distributed_slice` populated by
///   `gossamer_binding::register_module!` for every binding item.
/// - [`crate::native_symbols::native_symbols_snapshot`] - the
///   runtime `Mutex<Vec>` registry populated by the legacy
///   `force_link()` path. Kept for backward compatibility with
///   bindings that publish from a runtime hook.
///
/// Returns the leaked `&'static str` names so the caller can fold
/// them into the runtime-symbol set used by [`body_calls_jit_unsafe`] -
/// that keeps bodies that call bindings eligible for JIT
/// promotion (the `body_kinds` primitive-only filter still vetoes
/// anything the dispatch trampoline can't marshal).
fn register_binding_symbols(builder: &mut JITBuilder) -> Vec<&'static str> {
    use std::collections::HashSet;

    let mut names: Vec<&'static str> = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::new();
    // `linkme::DistributedSlice` doesn't impl `IntoIterator` for
    // `&Self`; the explicit `.iter()` is the only way in.
    #[allow(
        clippy::explicit_iter_loop,
        reason = "DistributedSlice has no &Self IntoIterator impl"
    )]
    for entry in crate::native_symbols::NATIVE_SYMBOLS.iter() {
        if seen.insert(entry.name) {
            builder.symbol(entry.name, (entry.addr_fn)());
            names.push(entry.name);
        }
    }
    for sym in crate::native_symbols::native_symbols_snapshot() {
        if seen.insert(sym.name) {
            builder.symbol(sym.name, sym.addr);
            names.push(sym.name);
        }
    }
    names
}

#[cfg(test)]
mod promotion_report_tests {
    use super::{
        jit_compile_body_names, jit_entry_body_names, jit_local_ty_needs_bytecode,
        jit_promotion_report,
    };
    use gossamer_lex::{SourceMap, Span};
    use gossamer_mir::{
        BasicBlock, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Terminator,
    };
    use gossamer_resolve::DefId;
    use gossamer_types::{ArrayLen, IntTy, Mutbl, Substs, TyCtxt, TyKind};
    use std::collections::HashMap;

    fn span() -> Span {
        let mut map = SourceMap::new();
        let file = map.add_file("jit-report.gos", "");
        Span::new(file, 0, 0)
    }

    fn body(name: &str, ty: gossamer_types::Ty, loop_back: bool) -> Body {
        let span = span();
        Body {
            name: name.to_string(),
            def: None,
            arity: 0,
            locals: vec![LocalDecl {
                ty,
                debug_name: None,
                mutable: false,
                region: false,
            }],
            blocks: vec![BasicBlock {
                id: BlockId(0),
                stmts: Vec::new(),
                terminator: if loop_back {
                    Terminator::Goto { target: BlockId(0) }
                } else {
                    Terminator::Return
                },
                span,
            }],
            span,
        }
    }

    #[test]
    fn report_is_stable_sorted_and_machine_categorized() {
        let mut tcx = TyCtxt::new();
        let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
        let map_ty = tcx.intern(TyKind::HashMap {
            key: i64_ty,
            value: i64_ty,
            ordered: false,
        });
        let bodies = vec![
            body("z_hot", i64_ty, true),
            body("a_map_boundary", map_ty, false),
        ];
        let enums = HashMap::new();
        let structs = HashMap::new();
        let first = jit_promotion_report(&bodies, &tcx, &enums, &structs);
        let second = jit_promotion_report(&bodies, &tcx, &enums, &structs);
        assert_eq!(first, second, "report must be deterministic");
        assert_eq!(first[0].name, "a_map_boundary");
        assert!(!first[0].admitted);
        assert!(first[0].reasons.contains(&"unsupported-boundary"));
        assert_eq!(first[1].name, "z_hot");
        assert!(
            first[1].admitted,
            "repeatedly entered loop helpers can promote"
        );
    }

    #[test]
    fn unlowerable_dependency_rejects_caller() {
        let mut tcx = TyCtxt::new();
        let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
        let i128_ty = tcx.intern(TyKind::Int(IntTy::I128));
        let mut caller = body("caller", i64_ty, true);
        let span = caller.span;
        caller.blocks[0].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("unsupported".to_string())),
            args: Vec::new(),
            destination: Place::local(Local(0)),
            target: Some(BlockId(1)),
        };
        caller.blocks.push(BasicBlock {
            id: BlockId(1),
            stmts: Vec::new(),
            terminator: Terminator::Goto { target: BlockId(1) },
            span,
        });
        let bodies = vec![caller, body("unsupported", i128_ty, false)];
        let admitted = jit_compile_body_names(&bodies, &tcx, &HashMap::new(), &HashMap::new());
        assert!(
            admitted.is_empty(),
            "unlowerable native dependency should reject its caller too: {admitted:?}"
        );
        let entries = jit_entry_body_names(&bodies, &tcx, &HashMap::new(), &HashMap::new());
        assert!(
            entries.is_empty(),
            "caller with unsupported dependency must not become a VM entry: {entries:?}"
        );
    }

    #[test]
    fn unsupported_method_dependency_rejects_dynamic_leaf_name_caller() {
        let mut tcx = TyCtxt::new();
        let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
        let i128_ty = tcx.intern(TyKind::Int(IntTy::I128));
        let mut caller = body("main", i64_ty, true);
        let span = caller.span;
        caller.blocks[0].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("next".to_string())),
            args: Vec::new(),
            destination: Place::local(Local(0)),
            target: Some(BlockId(1)),
        };
        caller.blocks.push(BasicBlock {
            id: BlockId(1),
            stmts: Vec::new(),
            terminator: Terminator::Goto { target: BlockId(1) },
            span,
        });
        let bodies = vec![caller, body("Counter::next", i128_ty, false)];
        let admitted = jit_compile_body_names(&bodies, &tcx, &HashMap::new(), &HashMap::new());
        assert!(
            admitted.is_empty(),
            "dynamic method dispatch must retain its unsupported dependency: {admitted:?}"
        );
    }

    #[test]
    fn scalar_map_returning_dependency_uses_native_only_abi() {
        let mut tcx = TyCtxt::new();
        let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
        let map_ty = tcx.intern(TyKind::HashMap {
            key: i64_ty,
            value: i64_ty,
            ordered: false,
        });
        let mut caller = body("caller", i64_ty, true);
        let span = caller.span;
        caller.blocks[0].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("build".to_string())),
            args: Vec::new(),
            destination: Place::local(Local(0)),
            target: Some(BlockId(1)),
        };
        caller.blocks.push(BasicBlock {
            id: BlockId(1),
            stmts: Vec::new(),
            terminator: Terminator::Goto { target: BlockId(1) },
            span,
        });

        let bodies = vec![caller, body("build", map_ty, false)];
        let admitted = jit_compile_body_names(&bodies, &tcx, &HashMap::new(), &HashMap::new());
        assert_eq!(
            admitted,
            std::collections::HashSet::from(["build".to_string(), "caller".to_string()]),
            "a map the typed runtime helpers store directly links natively"
        );
        // The VM keeps its own map representation, so the dependency stays off
        // the trampoline while its caller remains a valid entry.
        let entries = jit_entry_body_names(&bodies, &tcx, &HashMap::new(), &HashMap::new());
        assert_eq!(
            entries,
            std::collections::HashSet::from(["caller".to_string()])
        );
    }

    #[test]
    fn fixed_array_reference_dependency_uses_native_only_abi() {
        let mut tcx = TyCtxt::new();
        let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
        let body_def = DefId::local(17);
        let body_ty = tcx.intern(TyKind::Adt {
            def: body_def,
            substs: Substs::new(),
        });
        tcx.register_struct_fields(body_def, vec![i64_ty]);
        let array_ty = tcx.intern(TyKind::Array {
            elem: body_ty,
            len: ArrayLen::Concrete(5),
        });
        let array_ref_ty = tcx.intern(TyKind::Ref {
            mutability: Mutbl::Not,
            inner: array_ty,
        });

        let mut main = body("main", i64_ty, true);
        let span = main.span;
        main.blocks[0].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("energy".to_string())),
            args: Vec::new(),
            destination: Place::local(Local(0)),
            target: Some(BlockId(1)),
        };
        main.blocks.push(BasicBlock {
            id: BlockId(1),
            stmts: Vec::new(),
            terminator: Terminator::Goto { target: BlockId(1) },
            span,
        });
        let mut energy = body("energy", i64_ty, false);
        energy.arity = 1;
        energy.locals.push(LocalDecl {
            ty: array_ref_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });

        let bodies = vec![main, energy];
        let admitted = jit_compile_body_names(&bodies, &tcx, &HashMap::new(), &HashMap::new());
        assert_eq!(
            admitted,
            std::collections::HashSet::from(["energy".to_string(), "main".to_string()])
        );
        let entries = jit_entry_body_names(&bodies, &tcx, &HashMap::new(), &HashMap::new());
        assert_eq!(
            entries,
            std::collections::HashSet::from(["main".to_string()])
        );
    }

    #[test]
    fn straight_line_link_dependency_is_not_a_vm_entry() {
        let mut tcx = TyCtxt::new();
        let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
        let mut main = body("main", i64_ty, true);
        let span = main.span;
        main.blocks[0].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("rand".to_string())),
            args: Vec::new(),
            destination: Place::local(Local(0)),
            target: Some(BlockId(1)),
        };
        main.blocks.push(BasicBlock {
            id: BlockId(1),
            stmts: Vec::new(),
            terminator: Terminator::Goto { target: BlockId(1) },
            span,
        });
        let bodies = vec![main, body("rand", i64_ty, false)];
        let admitted = jit_compile_body_names(&bodies, &tcx, &HashMap::new(), &HashMap::new());
        assert_eq!(
            admitted,
            std::collections::HashSet::from(["main".to_string(), "rand".to_string()])
        );
        let entries = jit_entry_body_names(&bodies, &tcx, &HashMap::new(), &HashMap::new());
        assert_eq!(
            entries,
            std::collections::HashSet::from(["main".to_string()])
        );
    }

    /// Builds an append-in-a-loop body whose accumulator is `target`.
    fn string_builder_body(tcx: &mut TyCtxt, target: Operand, param: bool) -> Body {
        let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
        let string_ty = tcx.intern(TyKind::String);
        let mut_str_ty = tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Mut,
            inner: string_ty,
        });
        let mut builder = body("build", i64_ty, true);
        let span = builder.span;
        if param {
            builder.arity = 1;
            builder.locals.push(LocalDecl {
                ty: mut_str_ty,
                debug_name: None,
                mutable: true,
                region: false,
            });
        } else {
            builder.locals.push(LocalDecl {
                ty: string_ty,
                debug_name: None,
                mutable: true,
                region: false,
            });
        }
        builder.blocks[0].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_str_append_bytes".to_string())),
            args: vec![target],
            destination: Place::local(Local(0)),
            target: Some(BlockId(1)),
        };
        builder.blocks.push(BasicBlock {
            id: BlockId(1),
            stmts: Vec::new(),
            terminator: Terminator::Goto { target: BlockId(1) },
            span,
        });
        builder
    }

    /// An accumulator the body owns grows in place, so the builder loop is
    /// linear and worth compiling; one reached through a `&mut String`
    /// parameter is still held by the caller, so each append copies the
    /// prefix and the loop stays where it is linear.
    #[test]
    fn string_builder_compiles_for_a_local_accumulator_only() {
        let mut tcx = TyCtxt::new();
        let local_acc = string_builder_body(&mut tcx, Operand::Copy(Place::local(Local(1))), false);
        let admitted = jit_compile_body_names(&[local_acc], &tcx, &HashMap::new(), &HashMap::new());
        assert_eq!(
            admitted,
            std::collections::HashSet::from(["build".to_string()]),
            "a local accumulator compiles: {admitted:?}"
        );

        let param_acc = string_builder_body(&mut tcx, Operand::Copy(Place::local(Local(1))), true);
        let admitted = jit_compile_body_names(&[param_acc], &tcx, &HashMap::new(), &HashMap::new());
        assert!(
            admitted.is_empty(),
            "a `&mut String` parameter accumulator stays on bytecode: {admitted:?}"
        );
    }

    #[test]
    fn internal_aggregate_locals_do_not_block_promotion() {
        let mut tcx = TyCtxt::new();
        let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
        let string_ty = tcx.intern(TyKind::String);
        let vec_string_ty = tcx.intern(TyKind::Vec(string_ty));
        let pair_ty = tcx.intern(TyKind::Tuple(vec![i64_ty, string_ty]));
        let def = DefId::local(7);
        let record_ty = tcx.intern(TyKind::Adt {
            def,
            substs: Substs::new(),
        });
        tcx.register_struct_fields(def, vec![pair_ty]);
        let record_vec_ty = tcx.intern(TyKind::Vec(record_ty));

        assert!(!jit_local_ty_needs_bytecode(&tcx, vec_string_ty));
        assert!(!jit_local_ty_needs_bytecode(&tcx, record_vec_ty));
    }

    #[test]
    fn recursive_user_enum_and_internal_option_are_safe() {
        let mut tcx = TyCtxt::new();
        let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
        let tree_def = DefId::local(9);
        let tree_ty = tcx.intern(TyKind::Adt {
            def: tree_def,
            substs: Substs::new(),
        });
        tcx.register_enum_variant_tys(tree_def, vec![vec![], vec![i64_ty, tree_ty, tree_ty]]);
        let option_ty = tcx.intern(TyKind::Adt {
            def: DefId::local(u32::MAX - 1),
            substs: Substs::from_types([i64_ty]),
        });
        let option_ref_ty = tcx.intern(TyKind::Ref {
            mutability: Mutbl::Not,
            inner: option_ty,
        });

        assert!(!jit_local_ty_needs_bytecode(&tcx, tree_ty));
        assert!(!jit_local_ty_needs_bytecode(&tcx, option_ty));
        assert!(!jit_local_ty_needs_bytecode(&tcx, option_ref_ty));

        let mut pop_loop = body("pop_loop", i64_ty, true);
        pop_loop.locals.push(LocalDecl {
            ty: option_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let admitted = jit_compile_body_names(&[pop_loop], &tcx, &HashMap::new(), &HashMap::new());
        assert_eq!(
            admitted,
            std::collections::HashSet::from(["pop_loop".to_string()]),
            "an internal Option result such as discarded Vec::pop must not block promotion"
        );
    }
}

#[cfg(test)]
mod failure_isolation_tests {
    use super::compile_bodies_dropping_failures;
    use gossamer_lex::SourceMap;
    use gossamer_mir::{
        BasicBlock, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue, Statement,
        StatementKind, Terminator,
    };
    use gossamer_types::{IntTy, TyCtxt};
    use std::collections::HashMap;

    /// `fn <name>() -> i64 { <rvalue> }`.
    fn body(name: &str, ty: gossamer_types::Ty, rvalue: Rvalue) -> Body {
        let mut map = SourceMap::new();
        let file = map.add_file("jit-isolation.gos", "");
        let span = gossamer_lex::Span::new(file, 0, 0);
        Body {
            name: name.to_string(),
            def: None,
            arity: 0,
            locals: vec![LocalDecl {
                ty,
                debug_name: None,
                mutable: false,
                region: false,
            }],
            blocks: vec![BasicBlock {
                id: BlockId(0),
                stmts: vec![Statement {
                    span,
                    kind: StatementKind::Assign {
                        place: Place::local(Local(0)),
                        rvalue,
                    },
                }],
                terminator: Terminator::Return,
                span,
            }],
            span,
        }
    }

    #[test]
    fn an_unlowerable_body_does_not_take_the_rest_of_the_module_with_it() {
        let mut tcx = TyCtxt::new();
        let i64_ty = tcx.int_ty(IntTy::I64);
        let bodies = vec![
            body(
                "broken",
                i64_ty,
                Rvalue::CallIntrinsic {
                    name: "definitely_not_a_lowered_intrinsic",
                    args: Vec::new(),
                },
            ),
            body(
                "healthy",
                i64_ty,
                Rvalue::Use(Operand::Const(ConstValue::Int(7))),
            ),
        ];
        let artifact =
            compile_bodies_dropping_failures(bodies, &tcx, &HashMap::new(), &HashMap::new())
                .expect("an un-lowerable body must not fail the whole module");
        assert!(
            artifact.functions.contains_key("healthy"),
            "the lowerable body keeps its native entry"
        );
        assert!(
            !artifact.functions.contains_key("broken"),
            "the un-lowerable body stays on the bytecode VM"
        );
    }
}
