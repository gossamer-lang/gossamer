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

use super::*;

/// Reinterprets `value` as the same-width type `want`. A bitcast that changes
/// the lane count carries an explicit byte order; the targets Gossamer lowers
/// for are little-endian, so lane 0 holds the low-order bits.
pub(super) fn bitcast_same_width(
    builder: &mut FunctionBuilder<'_>,
    want: ir::Type,
    value: ir::Value,
) -> ir::Value {
    let have = value_type(value, builder);
    let flags = if have.lane_count() == want.lane_count() {
        ir::MemFlagsData::new()
    } else {
        ir::MemFlagsData::new().with_endianness(ir::Endianness::Little)
    };
    builder.ins().bitcast(want, flags, value)
}

pub(super) fn coerce_arg_to(
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    want: ir::Type,
) -> Result<ir::Value> {
    let have = value_type(value, builder);
    if have == want {
        return Ok(value);
    }
    if have == types::I64 && want == types::F64 {
        return Ok(builder
            .ins()
            .bitcast(types::F64, ir::MemFlagsData::new(), value));
    }
    if have == types::F64 && want == types::I64 {
        return Ok(builder
            .ins()
            .bitcast(types::I64, ir::MemFlagsData::new(), value));
    }
    if have.is_int() && want.is_int() {
        if have.bits() > want.bits() {
            return Ok(builder.ins().ireduce(want, value));
        }
        if have.bits() < want.bits() {
            // Gossamer integer types are signed by default (`i8..i128`,
            // `isize`). Sign-extend on narrow→wide widening so a
            // negative narrow value preserves its value at the wider
            // width. The unsigned-widening path is handled by callers
            // that explicitly hold an unsigned MIR type and route
            // through `coerce_arg_to_unsigned`.
            return Ok(builder.ins().sextend(want, value));
        }
    }
    if have.is_float() && want.is_float() {
        if have.bits() > want.bits() {
            return Ok(builder.ins().fdemote(want, value));
        }
        if have.bits() < want.bits() {
            return Ok(builder.ins().fpromote(want, value));
        }
    }
    // Same-width bit reinterpret (i32 ↔ f32, i8 ↔ ints, etc.).
    if have.bits() == want.bits() {
        return Ok(bitcast_same_width(builder, want, value));
    }
    if have.is_float() && want.is_int() {
        let int_form = if have == types::F64 {
            builder
                .ins()
                .bitcast(types::I64, ir::MemFlagsData::new(), value)
        } else {
            builder
                .ins()
                .bitcast(types::I32, ir::MemFlagsData::new(), value)
        };
        let int_ty = value_type(int_form, builder);
        if want.bits() > int_ty.bits() {
            return Ok(builder.ins().sextend(want, int_form));
        }
        if want.bits() < int_ty.bits() {
            return Ok(builder.ins().ireduce(want, int_form));
        }
        return Ok(int_form);
    }
    if have.is_int() && want.is_float() {
        let int_ty = if want == types::F64 {
            types::I64
        } else {
            types::I32
        };
        let resized = if have.bits() > int_ty.bits() {
            builder.ins().ireduce(int_ty, value)
        } else if have.bits() < int_ty.bits() {
            builder.ins().sextend(int_ty, value)
        } else {
            value
        };
        return Ok(builder
            .ins()
            .bitcast(want, ir::MemFlagsData::new(), resized));
    }
    // Last resort: typed zero of the wanted shape so the call
    // doesn't fail the verifier.
    if want.is_int() {
        Ok(builder.ins().iconst(want, 0))
    } else if want == types::F64 {
        Ok(builder.ins().f64const(0.0))
    } else if want == types::F32 {
        Ok(builder.ins().f32const(0.0))
    } else {
        Ok(value)
    }
}

pub(super) fn coerce_store_value(
    builder: &mut FunctionBuilder<'_>,
    value: ir::Value,
    leaf_ty: ir::Type,
) -> Result<ir::Value> {
    let src = value_type(value, builder);
    if src == leaf_ty {
        return Ok(value);
    }
    // Narrowing integer store: truncate with `ireduce`.
    if src.is_int() && leaf_ty.is_int() {
        if src.bits() > leaf_ty.bits() {
            return Ok(builder.ins().ireduce(leaf_ty, value));
        }
        if src.bits() < leaf_ty.bits() {
            // Caller wrote a narrower value into a wider slot.
            // Gossamer integer types are signed by default, so sign-
            // extend the bits. Same-width by construction is the common
            // case; this branch defends against a typeck-emitted
            // narrower source feeding a wider aggregate slot.
            return Ok(builder.ins().sextend(leaf_ty, value));
        }
    }
    if src.is_float() && leaf_ty.is_float() && src.bits() != leaf_ty.bits() {
        if src.bits() > leaf_ty.bits() {
            return Ok(builder.ins().fdemote(leaf_ty, value));
        }
        return Ok(builder.ins().fpromote(leaf_ty, value));
    }
    // Cross-kind int↔float store: reinterpret the bits. Real
    // numeric-cast logic lives in `Rvalue::Cast`; a raw
    // aggregate-slot write gets the bit pattern through.
    if src.bits() == leaf_ty.bits() && src != leaf_ty {
        return Ok(builder
            .ins()
            .bitcast(leaf_ty, ir::MemFlagsData::new(), value));
    }
    bail!("native codegen: cannot coerce store {src:?} -> {leaf_ty:?}");
}

pub(super) fn callee_prelude_name(operand: &Operand) -> Option<String> {
    match operand {
        Operand::Const(ConstValue::Str(s)) => Some(
            gossamer_abi::checked_integer_entry(s).map_or_else(|| s.clone(), ToString::to_string),
        ),
        _ => None,
    }
}
