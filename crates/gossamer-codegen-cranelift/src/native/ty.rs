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
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
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
//! Real Cranelift-backed native codegen.
//! Lowers a slice of MIR [`Body`]s into a `cranelift-object` module
//! and serialises the result as ELF (or the host's equivalent object
//! format). Supported today:
//! - `fn main() -> i64` with integer arithmetic (`+`, `-`, `*`, `/`,
//!   `%`, `&`, `|`, `^`, `<<`, `>>`, unary `-`, `!`),
//! - integer constants,
//! - direct calls between lowered functions,
//! - `return` of an `i64`.
//!
//! A C-ABI shim `main(argc, argv) -> i32` is emitted automatically:
//! it calls the Gossamer `main` and truncates the `i64` result into
//! the process exit code, so the object file links through a
//! standard `cc` invocation.
//! Aggregates (tuples/arrays/structs), strings, closures, and
//! anything that needs a GC heap are not yet lowered - those
//! constructs fall back to [`crate::emit::emit_module`] for
//! inspection.

// Allow patterns the Cranelift lowering deliberately uses:
//   - `similar_names` fires on `print_str`/`print_i64`/etc.
//     intrinsic-name shadowing within the same arm. The
//     parallel naming makes the dispatch table readable.
//   - `many_single_char_names` fires on hot inner-loop locals
//     (`a`, `b`, `n`, `m`, `k`) where longer names would
//     overflow the 100-col limit.
//   - `items_after_statements` flags inline `extern "C"` decls
//     localised to the one helper that uses them. Hoisting them
//     to module scope spreads the FFI surface; localised wins.
//   - `too_many_lines` / `cognitive_complexity` fire on the
//     intrinsic-dispatch arm and the `lower_intrinsic_call`
//     match. Splitting either hides the one-arm-per-symbol
//     structure that makes the table grep-able.
//   - `unnecessary_wraps` flags helpers whose `Result` exists
//     so call sites can still `?` them once a future lowering
//     can fail.
//   - `if_chain_can_be_rewritten_with_match` would flatten
//     short `if let Some(x) = .. else if let Some(y) = ..`
//     chains into match-on-tuple-of-options that's strictly
//     uglier here.
//   - `doc_markdown` flags identifiers like `i64`, `f64`,
//     etc. in plain-prose docs. Backticking every numeric
//     type name in every comment is noise.
//   - `manual_debug_impl` flags `JitModule`'s `Debug` impl
//     (which deliberately omits the JIT module pointer to keep
//     debug output stable across runs).
#![forbid(unsafe_code)]
#![allow(clippy::comparison_chain)]

use std::collections::HashMap;

use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};
use cranelift_codegen::ir::{
    AbiParam, ExtFuncData, Function, GlobalValueData, InstBuilder, MemFlagsData, Signature,
    StackSlotData, StackSlotKind, UserExternalName, UserFuncName, condcodes::IntCC,
    immediates::Imm64, types,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, ir};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module, ModuleDeclarations};
use cranelift_object::{ObjectBuilder, ObjectModule};
use gossamer_mir::{
    BinOp, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, StatementKind, Terminator,
    UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};
use rayon::prelude::*;

use super::*;

pub(super) fn abi_type_to_cranelift(ty: gossamer_abi::AbiType) -> Option<ir::Type> {
    match ty {
        gossamer_abi::AbiType::Void => None,
        gossamer_abi::AbiType::I8 => Some(types::I8),
        gossamer_abi::AbiType::I32 => Some(types::I32),
        gossamer_abi::AbiType::I64 | gossamer_abi::AbiType::U64 => Some(types::I64),
        gossamer_abi::AbiType::I128 => Some(types::I128),
        gossamer_abi::AbiType::F64 => Some(types::F64),
        gossamer_abi::AbiType::Ptr => Some(types::I64),
    }
}

/// `true` when the target passes `extern "C"` arguments under the Win64 ABI,
/// where rustc hands an `i128` over by pointer and returns one in a 16-byte
/// vector register instead of Cranelift's native integer-register pair.
pub(super) fn is_win64_abi(cfg: TargetFrontendConfig) -> bool {
    cfg.default_call_conv == CallConv::WindowsFastcall
}

/// Wire type of a logical `extern "C"` parameter: an `i128` crosses the Win64
/// boundary as the address of a 16-byte slot.
pub(super) fn win64_wire_param(cfg: TargetFrontendConfig, logical: ir::Type) -> ir::Type {
    if is_win64_abi(cfg) && logical == types::I128 {
        cfg.pointer_type()
    } else {
        logical
    }
}

/// Wire type of a logical `extern "C"` return: an `i128` comes back from a
/// Win64 callee in a 16-byte vector register.
pub(super) fn win64_wire_return(cfg: TargetFrontendConfig, logical: ir::Type) -> ir::Type {
    if is_win64_abi(cfg) && logical == types::I128 {
        types::I8X16
    } else {
        logical
    }
}

pub(super) fn cl_type_of(tcx: &TyCtxt, ty: Ty, module: &dyn Module) -> ir::Type {
    match tcx.kind_of(ty) {
        TyKind::Bool => types::I8,
        TyKind::Char => types::I32,
        // Canonical integer model: every integer type is a 64-bit
        // runtime value, matching the bytecode VM (which computes
        // all integer arithmetic at i64 width) and the C-ABI map
        // in `mir_ty_to_cabi`. Narrow declared widths only become
        // observable at explicit `as` casts, which mask to the
        // target width - see the `Rvalue::Cast` lowering. Modelling
        // narrow ints as I8/I16/I32 made arithmetic wrap at the
        // declared width (`sum += b` over `[u8]` gave sum mod 256
        // once the JIT tiered up), diverging from the VM.
        TyKind::Int(_) => types::I64,
        // Every float is a 64-bit runtime value, matching the VM and the LLVM
        // tier: an aggregate slot holds an f32 as a double word, and the
        // declared width rounds only at an explicit `as f32` cast.
        TyKind::Float(_) => types::F64,
        TyKind::Unit | TyKind::Never => types::I64,
        _ => module.target_config().pointer_type(),
    }
}

/// True when a `static mut` of this type is a single inline scalar word the
/// native backend can back with a writable data object. String/ref/aggregate
/// statics fall back to the VM: their initializer is a heap value, not an
/// inline word.
pub(super) fn is_scalar_static_ty(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind_of(ty),
        TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Float(_)
    )
}

pub(super) fn resolve_place_cl_type(
    tcx: &TyCtxt,
    body: &Body,
    place: &Place,
    module: &dyn Module,
    hint: Option<ir::Type>,
) -> ir::Type {
    let mut ty = body.local_ty(place.local);
    let mut hit_opaque = false;
    for projection in &place.projection {
        match projection {
            Projection::Field(idx) => {
                if let Some(next) = field_ty_at(tcx, ty, *idx) {
                    ty = next;
                } else {
                    hit_opaque = true;
                }
            }
            Projection::Index(_) => match tcx.kind_of(ty) {
                TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => {
                    ty = *elem;
                }
                _ => hit_opaque = true,
            },
            Projection::Deref => match tcx.kind_of(ty) {
                TyKind::Ref { inner, .. } => ty = *inner,
                _ => hit_opaque = true,
            },
            Projection::Downcast(_) | Projection::Discriminant => hit_opaque = true,
        }
    }
    if hit_opaque {
        if let Some(h) = hint {
            return h;
        }
    }
    cl_type_of(tcx, ty, module)
}

pub(super) fn cl_type_of_if_concrete(
    tcx: &TyCtxt,
    ty: Ty,
    module: &dyn Module,
) -> Option<ir::Type> {
    match tcx.kind_of(ty) {
        TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Float(_) => {
            Some(cl_type_of(tcx, ty, module))
        }
        TyKind::Ref { .. } | TyKind::String => Some(module.target_config().pointer_type()),
        _ => None,
    }
}

pub(super) fn value_type(value: ir::Value, builder: &FunctionBuilder<'_>) -> ir::Type {
    builder.func.dfg.value_type(value)
}

pub(super) fn int_ty_is_unsigned(t: IntTy) -> bool {
    matches!(
        t,
        IntTy::U8 | IntTy::U16 | IntTy::U32 | IntTy::U64 | IntTy::U128 | IntTy::Usize
    )
}

pub(super) fn resolve_place_ty(tcx: &TyCtxt, body: &Body, place: &Place) -> Ty {
    let mut ty = body.local_ty(place.local);
    for projection in &place.projection {
        ty = match projection {
            Projection::Field(idx) => field_ty_at(tcx, ty, *idx).unwrap_or(ty),
            Projection::Index(_) => {
                // `&[(i64, f64); 15][j]` keeps the indexed type as the
                // tuple element, not as the reference. Without peeling
                // here, downstream `type_slot_count` sees `Ref` (1 slot)
                // and the by-value read path drops the second half of
                // every tuple element. Peel through any chain of `Ref`
                // wrappers first, then walk into the array/slice/vec.
                let mut peeled = ty;
                while let TyKind::Ref { inner, .. } = tcx.kind_of(peeled) {
                    peeled = *inner;
                }
                match tcx.kind_of(peeled) {
                    TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => *elem,
                    _ => ty,
                }
            }
            Projection::Deref => match tcx.kind_of(ty) {
                TyKind::Ref { inner, .. } => *inner,
                _ => ty,
            },
            Projection::Downcast(_) | Projection::Discriminant => ty,
        };
    }
    ty
}
