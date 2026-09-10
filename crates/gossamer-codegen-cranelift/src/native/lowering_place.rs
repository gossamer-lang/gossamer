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

pub(super) fn lower_place_address(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    place: &Place,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let var = ensure_var(
        builder,
        locals,
        body,
        tcx,
        module,
        &intrinsics.body_cl_types,
        place.local,
    );
    let ptr_ty = module.target_config().pointer_type();
    let root_value = builder.use_var(var);
    // The root local holds a pointer (an aggregate's stack-slot
    // address). Widen it to the target's pointer type so later
    // `iadd`s don't fail on mismatched operand widths.
    let mut current = match value_type(root_value, builder) {
        t if t == ptr_ty => root_value,
        t if t == types::I64 && ptr_ty == types::I32 => builder.ins().ireduce(ptr_ty, root_value),
        t if t == types::I32 && ptr_ty == types::I64 => builder.ins().uextend(ptr_ty, root_value),
        _ => root_value,
    };
    // Track the type at each step so nested struct/tuple projections
    // can compute their byte offsets from the actual field layout
    // (each prior field's slot count) rather than a flat `idx * 8`.
    let mut current_ty = body.local_ty(place.local);
    // Track the per-element stride in slots for `Index(_)`. Seeded
    // from the root local's recorded metadata (or the type's
    // element type when no metadata exists), then re-derived from
    // the live `current_ty` after each projection step.
    let mut stride_slots = intrinsics
        .elem_slots
        .get(&place.local)
        .copied()
        .or_else(|| stride_slots_from_ty(tcx, body.local_ty(place.local)))
        .unwrap_or(1);
    let last_proj = place.projection.len().saturating_sub(1);
    // `current` alternates between the value the cursor names and the address
    // of a slot holding it. A runtime-managed handle (`Vec`, `Slice`) is the
    // value, so a step that consumes one loads it out of its slot first; a
    // step that only walks an offset leaves an address behind.
    let mut current_is_loaded = true;
    for (proj_idx, projection) in place.projection.iter().enumerate() {
        match projection {
            Projection::Field(idx) => {
                let off_bytes = field_byte_offset(tcx, current_ty, *idx);
                let offset = builder.ins().iconst(ptr_ty, i64::from(off_bytes));
                current = builder.ins().iadd(current, offset);
                current_is_loaded = false;
                if let Some(ft) = field_ty_at(tcx, current_ty, *idx) {
                    current_ty = ft;
                    stride_slots = stride_slots_from_ty(tcx, current_ty).unwrap_or(1);
                } else {
                    stride_slots = 1;
                }
            }
            Projection::Index(index_local) => {
                let index_var = ensure_var(
                    builder,
                    locals,
                    body,
                    tcx,
                    module,
                    &intrinsics.body_cl_types,
                    *index_local,
                );
                let idx_val = builder.use_var(index_var);
                // Audit C6: bounds-check every dynamic Index against
                // the statically-known length of a fixed-size array.
                // Negative indices are caught by the unsigned compare
                // (i64-as-u64 wraps to a large value that trips the
                // `>=` test). The check is opt-out via
                // `GOSSAMER_DISABLE_BOUNDS_CHECK=1` for micro-bench
                // programs that can prove safety. A `Vec` / slice
                // carries its own bounds check in the runtime helper
                // the branch below calls.
                emit_array_bounds_check(module, builder, intrinsics, current_ty, idx_val, tcx)?;
                // A `Vec` / slice keeps its elements behind the header's
                // data pointer at the header's own stride, so the element
                // address comes from the runtime rather than from a walk off
                // the header. An array holds its elements inline and keeps
                // the stride walk below.
                let mut indexed = current_ty;
                while let TyKind::Ref { inner, .. } = tcx.kind_of(indexed).clone() {
                    indexed = inner;
                }
                if let TyKind::Vec(elem) | TyKind::Slice(elem) = tcx.kind_of(indexed).clone() {
                    let idx_i64 = match value_type(idx_val, builder) {
                        t if t == types::I64 => idx_val,
                        _ => builder.ins().sextend(types::I64, idx_val),
                    };
                    // The helper takes the header itself, so a handle still
                    // sitting in a struct field or a Vec element slot is
                    // loaded out of it here.
                    let handle = if current_is_loaded {
                        current
                    } else {
                        builder
                            .ins()
                            .load(ptr_ty, MemFlagsData::trusted(), current, 0)
                    };
                    let get_ptr = intrinsics.extern_fn(
                        module,
                        "gos_rt_vec_get_ptr",
                        &[ptr_ty, types::I64],
                        &[ptr_ty],
                    )?;
                    let fref = module.declare_func_in_func(get_ptr, builder.func);
                    let call = builder.ins().call(fref, &[handle, idx_i64]);
                    current = builder.inst_results(call)[0];
                    current_is_loaded = false;
                    current_ty = elem;
                    stride_slots = stride_slots_from_ty(tcx, current_ty).unwrap_or(1);
                    continue;
                }
                let idx_ptr = match value_type(idx_val, builder) {
                    t if t == ptr_ty => idx_val,
                    t if t == types::I64 && ptr_ty == types::I32 => {
                        builder.ins().ireduce(ptr_ty, idx_val)
                    }
                    t if t == types::I32 && ptr_ty == types::I64 => {
                        builder.ins().uextend(ptr_ty, idx_val)
                    }
                    _ => idx_val,
                };
                let stride = builder.ins().iconst(ptr_ty, i64::from(stride_slots) * 8);
                let byte_offset = builder.ins().imul(idx_ptr, stride);
                current = builder.ins().iadd(current, byte_offset);
                current_is_loaded = false;
                // After indexing, the cursor sits inside a single
                // element; advance `current_ty` to the element type
                // so subsequent Field projections compute their
                // offsets relative to that element's layout. Peel
                // any `Ref` wrappers first so `&[(T, U); N][j].0`
                // descends into the tuple instead of treating the
                // element as opaque.
                let mut peeled = current_ty;
                while let TyKind::Ref { inner, .. } = tcx.kind_of(peeled).clone() {
                    peeled = inner;
                }
                current_ty = match tcx.kind_of(peeled).clone() {
                    TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => elem,
                    _ => current_ty,
                };
                // The next Index (if any) steps within THIS element, so its
                // stride is the element's own element size - a nested array
                // `[[[i64; 8]; 6]; 2]` walks 48-, then 8-, then 1-slot
                // strides. A non-array element leaves no further Index to
                // take; 1 keeps the terminal scalar load at the cursor.
                stride_slots = stride_slots_from_ty(tcx, current_ty).unwrap_or(1);
            }
            Projection::Deref => {
                // `*ptr`: the local already holds a pointer; after
                // this projection the address is just that pointer
                // value. Subsequent Field/Index projections
                // compute offsets off of it.
                //
                // only emit the indirect load
                // when the source is a heap-pointer-shaped Adt
                // (slot_count = None). Inline multi-slot
                // aggregates already hold the slot address in the
                // Cranelift Variable - loading would dereference
                // the stack slot's first 8 bytes (typically a
                // field, possibly 0) as if it were the pointer,
                // segfaulting at the next projection. This
                // mirrors the LLVM fix recorded in
                // `llvm_call_arg_ref_aggregate_fix.md`.
                let peeled = match tcx.kind_of(current_ty) {
                    TyKind::Ref { inner, .. } => *inner,
                    _ => current_ty,
                };
                let inline_aggregate =
                    matches!(tcx.kind_of(peeled), TyKind::Tuple(_) | TyKind::Array { .. })
                        || (matches!(tcx.kind_of(peeled), TyKind::Adt { .. })
                            && type_slot_count(tcx, peeled) > 1);
                // A TERMINAL `Deref` whose pointee is a one-word value (scalar
                // or `String`, the `&mut x`-on-place shapes the MIR lowers to a
                // slot-address `Rvalue::Ref`) keeps `current` as the slot
                // ADDRESS: `lower_place_read` issues the single load and a store
                // writes through it. Loading here would yield the value, which
                // the consumer's own load then dereferences a second time -
                // `*out += s` for `out: &mut String` faulting on `**out`.
                // A `&mut` payload enum is the same shape: the callee rebinds
                // the caller's binding whole (`*self = Variant(..)`), so its
                // reference names the slot and the store writes into it. A
                // shared reference to one carries the node itself.
                let mut_enum_slot = matches!(
                    tcx.kind_of(current_ty),
                    TyKind::Ref {
                        mutability: gossamer_types::Mutbl::Mut,
                        inner,
                    } if tcx.is_payload_enum(*inner)
                );
                let terminal_value = proj_idx == last_proj
                    && (matches!(
                        tcx.kind_of(peeled),
                        TyKind::Int(_)
                            | TyKind::Float(_)
                            | TyKind::Bool
                            | TyKind::Char
                            | TyKind::String
                    ) || mut_enum_slot);
                if !inline_aggregate && !terminal_value {
                    let loaded = builder
                        .ins()
                        .load(ptr_ty, MemFlagsData::trusted(), current, 0);
                    current = loaded;
                    current_is_loaded = true;
                }
                if let TyKind::Ref { inner, .. } = tcx.kind_of(current_ty).clone() {
                    current_ty = inner;
                }
                stride_slots = stride_slots_from_ty(tcx, current_ty).unwrap_or(1);
            }
            Projection::Discriminant => {
                // Discriminant lives at offset 0 of an enum's
                // backing storage. The following load reads it as
                // i64.
                // No offset change; subsequent projections read
                // the tag word directly.
                stride_slots = 1;
            }
            Projection::Downcast(_) => {
                // Downcast skips past the tag word to the payload.
                let tag_bytes = builder.ins().iconst(ptr_ty, 8);
                current = builder.ins().iadd(current, tag_bytes);
                current_is_loaded = false;
                stride_slots = 1;
            }
        }
    }
    Ok(current)
}

pub(super) fn lower_place_store(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    place: &Place,
    value: ir::Value,
    leaf_ty: ir::Type,
    intrinsics: &mut IntrinsicContext,
) -> Result<()> {
    let addr = lower_place_address(module, builder, locals, body, tcx, place, intrinsics)?;
    // Option/Result carriers occupy two inline words. Projection assignment
    // must replace both the discriminant and payload. Narrowing an i128
    // carrier to the pointer-sized leaf type stores only the low word, leaving
    // the old payload behind. That made `node.next = Some(new_node)` observe
    // the previous child and could turn cyclic aggregates into malformed
    // graphs in JIT code.
    let leaf_place_ty = resolve_place_ty(tcx, body, place);
    let leaf_slots = type_slot_count(tcx, leaf_place_ty);
    if value_type(value, builder) == types::I128 && leaf_slots > 1 {
        store_i128_words(builder, value, addr, 0);
        return Ok(());
    }
    // An inline aggregate leaf is a block of words the value names the
    // address of, so the whole block is what the store replaces. One store
    // would write its first field and leave the rest of the leaf as the
    // previous value left it. A leaf whose value IS one word - a handle, a
    // tagged enum pointer - keeps the single store, whatever width its
    // contents occupy elsewhere.
    if leaf_slots > 1 && inline_aggregate_leaf(tcx, leaf_place_ty) {
        let ptr_ty = module.target_config().pointer_type();
        let src = coerce_arg_to(builder, value, ptr_ty).unwrap_or(value);
        for word_idx in 0..leaf_slots {
            let off = ir::immediates::Offset32::new((word_idx as i32) * 8);
            let word = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), src, off);
            builder
                .ins()
                .store(MemFlagsData::trusted(), word, addr, off);
        }
        return Ok(());
    }
    // Coerce the value to the leaf's cranelift type where possible;
    // bail loudly when that would be lossy.
    let coerced = coerce_store_value(builder, value, leaf_ty)?;
    builder
        .ins()
        .store(MemFlagsData::trusted(), coerced, addr, 0);
    Ok(())
}

pub(super) fn lower_first_ptr_arg(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    args: &[Operand],
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    let ptr_ty = module.target_config().pointer_type();
    let value = match args.first() {
        Some(a) => lower_operand(
            module,
            builder,
            locals,
            body,
            tcx,
            a,
            Some(ptr_ty),
            intrinsics,
        )?,
        None => builder.ins().iconst(ptr_ty, 0),
    };
    coerce_arg_to(builder, value, ptr_ty)
}

pub(super) fn lower_place_read(
    module: &mut dyn Module,
    builder: &mut FunctionBuilder<'_>,
    locals: &mut HashMap<Local, Variable>,
    body: &Body,
    tcx: &TyCtxt,
    place: &Place,
    hint: Option<ir::Type>,
    intrinsics: &mut IntrinsicContext,
) -> Result<ir::Value> {
    if place.projection.is_empty() {
        let var = ensure_var(
            builder,
            locals,
            body,
            tcx,
            module,
            &intrinsics.body_cl_types,
            place.local,
        );
        return Ok(builder.use_var(var));
    }
    let addr = lower_place_address(module, builder, locals, body, tcx, place, intrinsics)?;
    // When the projected leaf is itself a multi-slot aggregate
    // (struct/tuple/array embedded inline), return the field's
    // address rather than reading a single i64 word. The receiving
    // local treats the value as a pointer-to-aggregate and walks
    // further projections off of it; loading would collapse the
    // sub-struct to its first slot and segfault on any subsequent
    // `Field`/`Index` step.
    let leaf_ty_mir = resolve_place_ty(tcx, body, place);
    // An `Option<T>` / `Result<T, E>` leaf is a by-value two-word carrier
    // - an i128 SSA value everywhere else in the JIT - not an
    // address-backed aggregate. Its consumers (`gos_rt_result_disc` /
    // `_payload`, stores, call args) take the packed value, so load the
    // 16-byte carrier rather than returning the field's address.
    if is_carrier_ty(tcx, leaf_ty_mir) {
        return Ok(builder
            .ins()
            .load(types::I128, MemFlagsData::new(), addr, 0));
    }
    // A one-word address-represented aggregate leaf (a single-managed-field
    // struct embedded in a parent) is likewise handed out by address so the
    // receiving local keeps the aggregate representation.
    if type_slot_count(tcx, leaf_ty_mir) > 1 || single_slot_addr_aggregate(tcx, leaf_ty_mir) {
        return Ok(addr);
    }
    let leaf_ty = resolve_place_cl_type(tcx, body, place, module, hint);
    // Use plain `MemFlagsData::new()` instead of `trusted()` - without
    // it cranelift's alias analysis was load-CSEing reads across
    // unrelated stores, e.g. in
    //   let t = arr[lo]
    //   let u = arr[hi]
    //   arr[hi] = t
    //   arr[lo] = u
    // the second store materialised `u` from a fresh load of
    // `arr+hi*8` *after* `arr+hi*8` had been overwritten with `t`,
    // collapsing the swap to a degenerate `arr[lo] = arr[lo]`.
    Ok(builder.ins().load(leaf_ty, MemFlagsData::new(), addr, 0))
}

/// Whether a place's leaf holds its words inline, so a store replaces the
/// whole block rather than one handle-shaped word.
fn inline_aggregate_leaf(tcx: &TyCtxt, ty: Ty) -> bool {
    if is_inline_two_word_ty(tcx, ty) {
        return false;
    }
    match tcx.kind_of(ty) {
        TyKind::Tuple(_) | TyKind::Array { .. } => true,
        TyKind::Adt { def, .. } => {
            def.local < u32::MAX - 16 && tcx.struct_field_tys(*def).is_some()
        }
        _ => false,
    }
}
