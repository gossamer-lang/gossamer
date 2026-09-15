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

pub(super) fn operand_locals(rvalue: &Rvalue) -> Vec<Local> {
    let mut out = Vec::new();
    let mut push = |op: &Operand| {
        if let Operand::Copy(place) = op {
            if place.projection.is_empty() {
                out.push(place.local);
            }
        }
    };
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp { operand: op, .. } => push(op),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            push(lhs);
            push(rhs);
        }
        Rvalue::Cast { operand, .. } => push(operand),
        _ => {}
    }
    out
}

pub(super) enum PrintKind {
    StrPtr,
    Int,
    /// Unsigned integer (any width). Routed through
    /// `gos_rt_print_u64` so values >= 2^63 print without a
    /// leading `-` (the bug `Int` would have).
    Uint,
    Float,
    Bool,
    Char,
    /// `Vec<i64>` (or any 8-byte-elem Vec): formatted at runtime
    /// via `gos_rt_vec_format_i64` into a `[v0, v1, …]` string.
    /// The same rendering as the kind it wraps, in the bare-bracket spelling
    /// a fixed array and a slice take. A slice shares the `Vec` runtime
    /// representation, so only the static type tells them apart.
    Seq(Box<PrintKind>),
    VecI64,
    /// `Vec<u64>` / `Vec<usize>`: the same slots read as unsigned, so an
    /// element at or above `i64::MAX` renders as its own decimal.
    VecUint,
    /// `Vec<f64>` formatted via `gos_rt_vec_format_f64`.
    VecF64,
    /// `Vec<bool>` formatted via `gos_rt_vec_format_bool`.
    VecBool,
    /// `Vec<String>` formatted via `gos_rt_vec_format_string`.
    VecString,
    /// `Vec<Vec<i64>>` formatted via `gos_rt_vec_format_vec_i64`.
    VecVecI64,
    /// `Vec<Vec<String>>` formatted via `gos_rt_vec_format_vec_string`.
    VecVecString,
    /// A `Vec` of tuples formatted via `gos_rt_vec_format_tuple`: the
    /// per-field tag stream each element renders through, and its arity.
    VecTuple(Vec<u8>, i64),
    /// `[i64; N]` flat-buffer literal (no GosVec header). Formatted
    /// via `gos_rt_arr_format_i64(ptr, len)`.
    ArrI64(i64),
    /// `[f64; N]` flat-buffer literal.
    ArrF64(i64),
    /// `[bool; N]` flat-buffer literal.
    ArrBool(i64),
    /// `[String; N]` flat-buffer literal.
    ArrString(i64),
    /// `[[i64; M]; N]` flat-buffer nested array (`N * M` contiguous
    /// slots, rows inline): `gos_rt_arr_format_arr_i64(ptr, N, M)`.
    ArrArrI64(i64, i64),
    /// `[[f64; M]; N]` flat-buffer nested array.
    ArrArrF64(i64, i64),
    /// `[[bool; M]; N]` flat-buffer nested array.
    ArrArrBool(i64, i64),
    /// `json::Value` - rendered via `gos_rt_json_render`.
    JsonValue,
    /// A `DynValue`, rendered through `gos_rt_dyn_format`.
    DynValue,
    /// `errors::Error` - calls `gos_rt_error_message` then prints as string.
    ErrorMessage,
    /// A tuple of scalar elements - rendered via `gos_rt_tuple_format`
    /// with a per-element tag array computed from the element types.
    Tuple,
    /// A scalar-keyed, scalar/string-valued `HashMap` - rendered via
    /// `gos_rt_map_format`.
    Map,
    /// A `HashMap` whose key or value was declared `u64` / `usize`: the tags
    /// carry each side's width to `gos_rt_map_format_tagged`, so a slot at or
    /// above `i64::MAX` reads as its own decimal.
    MapTagged(u8, u8),
    /// A container handle - `Deque` / `Queue` / `Stack` / `MaxHeap` /
    /// `MinHeap` - rendered by the named runtime shim, which owns the one
    /// text form every tier prints.
    HandleFormat(&'static str),
    /// A `HashSet` / `BTreeSet` handle rendered by the named runtime shim;
    /// the flag selects the ordered display prefix.
    SetFormat(&'static str, i32),
    /// `{:?}` of an `Option<T>` with a scalar / String payload, rendered via
    /// `gos_rt_debug_option`. The `u8` is the payload formatter kind.
    Option(u8),
    /// `{:?}` of a `Result<T, E>` with scalar / String payloads, rendered via
    /// `gos_rt_debug_result`. The two `u8`s are the Ok / Err payload kinds.
    Result(u8, u8),
    Unsupported(&'static str),
}

/// Per-element tag for `gos_rt_tuple_format`, or `None` when the
/// element type can't be rendered straight from a raw 8-byte tuple
/// slot. Integers are restricted to 64-bit width and floats to `f64`
/// (a narrower scalar writes fewer than 8 bytes into its slot, so
/// reading the slot back as an i64 / f64 bit pattern would pick up
/// adjacent bytes); `bool` (low bit) and `char` (low 32 bits) read
/// through a mask, so both are safe.
fn tuple_elem_tag(tcx: &TyCtxt, ty: Ty) -> Option<u8> {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    match tcx.kind_of(ty) {
        // A `u64` / `usize` slot spans the whole unsigned range, so it reads
        // as unsigned wherever a tag stream names it.
        TyKind::Int(IntTy::U64 | IntTy::Usize) => Some(1),
        // Every other integer's slot holds its value widened to a word, sign-
        // extended when signed and zero-extended when not, so the signed word
        // is the value itself.
        TyKind::Int(_) | TyKind::Duration | TyKind::Instant => Some(0),
        // An `f32` slot holds the double every float slot does.
        TyKind::Float(_) => Some(2),
        TyKind::Bool => Some(3),
        TyKind::Char => Some(4),
        TyKind::String => Some(5),
        _ => None,
    }
}

/// Maps an `Option` / `Result` payload type to the `gos_rt_debug_*` formatter
/// kind (0=i64/signed, 1=u64, 2=f64, 3=bool, 4=char, 5=String), or `None` for
/// an aggregate / nested payload. A `u64` / `usize` payload reads as unsigned,
/// so a value at or above `i64::MAX` renders as its own decimal; the VM boxes
/// the payload as its `Uint` value for the same reason.
fn debug_payload_kind(tcx: &TyCtxt, ty: Ty) -> Option<u8> {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    match tcx.kind_of(ty) {
        TyKind::Int(IntTy::U64 | IntTy::Usize) => Some(1),
        TyKind::Int(_) => Some(0),
        TyKind::Float(_) => Some(2),
        TyKind::Bool => Some(3),
        TyKind::Char => Some(4),
        TyKind::String => Some(5),
        _ => None,
    }
}

/// Tags describing `ty` in a `gos_rt_tuple_format` stream: one byte for
/// a scalar, or the `8, count, <nested tags…>` form for a nested tuple
/// whose slots are flattened into the parent's buffer.
fn tuple_elem_tags(tcx: &TyCtxt, ty: Ty) -> Option<Vec<u8>> {
    let mut peeled = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(peeled) {
        peeled = *inner;
    }
    if let TyKind::Tuple(nested) = tcx.kind_of(peeled) {
        let nested: Vec<Ty> = nested.clone();
        if nested.is_empty() || nested.len() > usize::from(u8::MAX) {
            return None;
        }
        let mut out = vec![gossamer_abi::TUPLE_TAG_NESTED, nested.len() as u8];
        for e in &nested {
            out.extend(tuple_elem_tags(tcx, *e)?);
        }
        return Some(out);
    }
    tuple_elem_tag(tcx, ty).map(|tag| vec![tag])
}

/// The tag stream for `ty` when it is a printable tuple, or `None`.
/// Drives the pre-intern pass, which must place every stream the
/// parallel phase can ask for into the intrinsic cache first.
pub(super) fn tuple_tags_for_ty(tcx: &TyCtxt, ty: Ty) -> Option<Vec<u8>> {
    let mut peeled = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(peeled) {
        peeled = *inner;
    }
    let TyKind::Tuple(elems) = tcx.kind_of(peeled) else {
        return None;
    };
    if elems.is_empty() {
        return None;
    }
    let mut tags = Vec::with_capacity(elems.len());
    for e in elems.clone() {
        tags.extend(tuple_elem_tags(tcx, e)?);
    }
    Some(tags)
}

/// Every tuple type reachable from `ty`, outermost first. A nested
/// tuple can be printed on its own (`t.0`), and a sequence of tuples
/// renders each element through its tuple's stream, so those streams need
/// interning too.
pub(super) fn nested_tuple_types(tcx: &TyCtxt, ty: Ty, out: &mut Vec<Ty>) {
    let mut peeled = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(peeled) {
        peeled = *inner;
    }
    if let TyKind::Vec(elem) | TyKind::Slice(elem) = tcx.kind_of(peeled) {
        nested_tuple_types(tcx, *elem, out);
        return;
    }
    let TyKind::Tuple(elems) = tcx.kind_of(peeled) else {
        return;
    };
    out.push(peeled);
    for e in elems.clone() {
        nested_tuple_types(tcx, e, out);
    }
}

/// The print kind of a sequence whose element is the tuple `elem`: the
/// per-field tags its renderer walks, or `Unsupported(label)` when a field has
/// no flat tag.
fn vec_tuple_kind(tcx: &TyCtxt, elem: Ty, label: &'static str) -> PrintKind {
    let arity = match tcx.kind_of(peel_refs(tcx, elem)) {
        TyKind::Tuple(fields) => fields.len(),
        _ => 0,
    };
    match tuple_elem_tags(tcx, elem) {
        // The element's own stream starts with the nested marker and count;
        // the renderer takes the per-field tags after them.
        Some(tags) if arity > 0 && tags.len() > 2 => {
            PrintKind::VecTuple(tags[2..].to_vec(), i64::try_from(arity).unwrap_or(0))
        }
        _ => PrintKind::Unsupported(label),
    }
}

/// The top-level element count and tag stream for a tuple operand, or
/// `None` when any element type isn't formattable from a flat slot.
/// Drives both the `PrintKind::Tuple` gate and the emit-time tag blob.
pub(super) fn tuple_tags(tcx: &TyCtxt, body: &Body, operand: &Operand) -> Option<(usize, Vec<u8>)> {
    let Operand::Copy(place) = operand else {
        return None;
    };
    let mut ty = resolve_place_ty(tcx, body, place);
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    let TyKind::Tuple(elems) = tcx.kind_of(ty) else {
        return None;
    };
    if elems.is_empty() {
        return None;
    }
    let count = elems.len();
    let mut tags = Vec::with_capacity(count);
    for e in elems.clone() {
        tags.extend(tuple_elem_tags(tcx, e)?);
    }
    Some((count, tags))
}

/// True when a `HashMap` key/value type is one `gos_rt_map_format`
/// renders from its live storage: an integer (signed decimal, like
/// the VM) or a `String` (bare).
fn map_kv_supported(tcx: &TyCtxt, ty: Ty) -> bool {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    matches!(tcx.kind_of(ty), TyKind::Int(_) | TyKind::String)
}

pub(super) fn operand_cl_type(
    body: &Body,
    tcx: &TyCtxt,
    operand: &Operand,
    module: &dyn Module,
) -> Option<ir::Type> {
    match operand {
        Operand::Const(ConstValue::Int(_)) => Some(types::I64),
        Operand::Const(ConstValue::Float(_)) => Some(types::F64),
        Operand::Const(ConstValue::Bool(_)) => Some(types::I8),
        Operand::Const(ConstValue::Char(_)) => Some(types::I32),
        Operand::Const(ConstValue::Unit) => None,
        Operand::Const(ConstValue::Str(_)) => Some(module.target_config().pointer_type()),
        Operand::Copy(place) => {
            let ty = resolve_place_ty(tcx, body, place);
            match tcx.kind_of(ty) {
                TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Float(_) => {
                    Some(cl_type_of(tcx, ty, module))
                }
                _ => None,
            }
        }
        Operand::FnRef { .. } => None,
    }
}

/// `ty` with every reference peeled off.
fn peel_refs(tcx: &TyCtxt, ty: Ty) -> Ty {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    ty
}

/// Runtime format shim for a container handle's sentinel `DefId`:
/// `Deque` (`u32::MAX - 19`), `MaxHeap` (`- 28`), `MinHeap` (`- 30`),
/// `Queue` (`- 31`), `Stack` (`- 32`).
fn container_format_symbol(def_local: u32) -> Option<&'static str> {
    Some(match u32::MAX - def_local {
        19 => "gos_rt_deque_format",
        28 => "gos_rt_bheap_max_format",
        30 => "gos_rt_bheap_min_format",
        31 => "gos_rt_queue_format",
        32 => "gos_rt_stack_format",
        _ => return None,
    })
}

/// Print plan for a container the MIR types as a bare `i64` handle,
/// recovered from the constructor that produced `local`. A set's element
/// kind comes from the constructor or from the inserts against it, the
/// same evidence the LLVM planner reads.
fn container_handle_print_kind(body: &Body, local: Local) -> Option<PrintKind> {
    container_handle_print_kind_seen(body, local, &mut Vec::new())
}

/// The copy chain this walks is a graph, not a tree: MIR may assign a local
/// from itself, and two locals may be assigned from each other. `seen` records
/// the locals already on the current walk so a cycle ends the search instead
/// of following it forever.
fn container_handle_print_kind_seen(
    body: &Body,
    local: Local,
    seen: &mut Vec<Local>,
) -> Option<PrintKind> {
    if seen.contains(&local) {
        return None;
    }
    seen.push(local);
    let mut set_ordered = None;
    let mut set_symbol = None;
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.local == local
                && place.projection.is_empty()
                && let Rvalue::Use(Operand::Copy(src)) = rvalue
                && src.projection.is_empty()
                && let Some(kind) = container_handle_print_kind_seen(body, src.local, seen)
            {
                return Some(kind);
            }
        }
        let Terminator::Call {
            callee,
            args,
            destination,
            ..
        } = &block.terminator
        else {
            continue;
        };
        let Operand::Const(ConstValue::Str(name)) = callee else {
            continue;
        };
        if destination.local == local && destination.projection.is_empty() {
            if let Some(sym) = container_ctor_format_symbol(name) {
                return Some(PrintKind::HandleFormat(sym));
            }
            // `let` binds a `Set`/`BTreeSet` through a defensive
            // `gos_rt_set_clone` (neither type carries its own refcount), so
            // the element kind lives on the cloned source handle, not on
            // this call's own operands.
            // Set algebra answers a set of the receiver's element type, so the
            // answer renders the way its receiver does.
            if matches!(
                name.as_str(),
                "gos_rt_set_clone"
                    | "gos_rt_set_union"
                    | "gos_rt_set_intersection"
                    | "gos_rt_set_difference"
                    | "gos_rt_set_symmetric_difference"
            ) && let Some(Operand::Copy(src)) = args.first()
                && src.projection.is_empty()
                && let Some(kind) = container_handle_print_kind(body, src.local)
            {
                return Some(kind);
            }
            match name.as_str() {
                "gos_rt_set_new" => set_ordered = Some(0),
                "gos_rt_btree_set_new" => set_ordered = Some(1),
                "gos_rt_set_from_vec_i64" => {
                    set_ordered = Some(0);
                    set_symbol = Some("gos_rt_set_format_i64");
                }
                "gos_rt_set_from_vec_str" => {
                    set_ordered = Some(0);
                    set_symbol = Some("gos_rt_set_format_string");
                }
                "gos_rt_btree_set_from_vec_i64" => {
                    set_ordered = Some(1);
                    set_symbol = Some("gos_rt_set_format_i64");
                }
                "gos_rt_btree_set_from_vec_str" => {
                    set_ordered = Some(1);
                    set_symbol = Some("gos_rt_set_format_string");
                }
                _ => {}
            }
        }
        let receives_local = args.first().is_some_and(|arg| {
            matches!(arg, Operand::Copy(place) if place.local == local && place.projection.is_empty())
        });
        if receives_local {
            match name.as_str() {
                "gos_rt_set_insert_i64" => set_symbol = Some("gos_rt_set_format_i64"),
                "gos_rt_set_insert" => set_symbol = Some("gos_rt_set_format_string"),
                _ => {}
            }
        }
    }
    let ordered = set_ordered?;
    Some(PrintKind::SetFormat(
        set_symbol.unwrap_or("gos_rt_set_format_string"),
        ordered,
    ))
}

/// Runtime format shim for a container handle a local was constructed by.
fn container_ctor_format_symbol(ctor: &str) -> Option<&'static str> {
    Some(match ctor {
        // The clone a `let` binds through names the container kind exactly
        // as its constructor does, so the rendering follows it the same way.
        "gos_rt_deque_new" | "gos_rt_deque_from_vec_i64" | "gos_rt_deque_clone" => {
            "gos_rt_deque_format"
        }
        "gos_rt_queue_new" | "gos_rt_queue_from_vec_i64" | "gos_rt_queue_clone" => {
            "gos_rt_queue_format"
        }
        "gos_rt_stack_new" | "gos_rt_stack_from_vec_i64" | "gos_rt_stack_clone" => {
            "gos_rt_stack_format"
        }
        "gos_rt_bheap_max_new_i64" | "gos_rt_bheap_max_from_vec_i64" => "gos_rt_bheap_max_format",
        "gos_rt_bheap_min_new_i64" | "gos_rt_bheap_min_from_vec_i64" => "gos_rt_bheap_min_format",
        _ => return None,
    })
}

pub(super) fn operand_print_kind(body: &Body, tcx: &TyCtxt, operand: &Operand) -> PrintKind {
    match operand {
        Operand::Const(ConstValue::Str(_)) => PrintKind::StrPtr,
        Operand::Const(ConstValue::Int(_)) => PrintKind::Int,
        Operand::Const(ConstValue::Float(_)) => PrintKind::Float,
        Operand::Const(ConstValue::Bool(_)) => PrintKind::Bool,
        Operand::Const(ConstValue::Char(_)) => PrintKind::Char,
        Operand::Const(ConstValue::Unit) => PrintKind::Int,
        Operand::Copy(place) => {
            let ty = tcx.peel_nominal(resolve_place_ty(tcx, body, place));
            // A shared reference to a string, sequence, map, tuple, or array
            // carries the same word as the value it names, so it renders as
            // that value.
            let ty = match tcx.kind_of(ty) {
                TyKind::Ref {
                    inner,
                    mutability: gossamer_types::Mutbl::Not,
                } if matches!(
                    tcx.kind_of(*inner),
                    TyKind::String
                        | TyKind::Vec(_)
                        | TyKind::Slice(_)
                        | TyKind::HashMap { .. }
                        | TyKind::Tuple(_)
                        | TyKind::Array { .. }
                ) =>
                {
                    tcx.peel_nominal(*inner)
                }
                _ => ty,
            };
            // A container renders through its own runtime shim whether the
            // local carries the container's type or the bare i64 handle the
            // constructor returned.
            if let TyKind::Adt { def, substs } = tcx.kind_of(ty)
                && let Some(sym) = container_format_symbol(def.local)
            {
                // The shim reads each element as one integer word. An
                // element of any other shape needs the descriptor stream
                // this tier does not emit, so the body runs on the VM
                // rather than printing the slot as a number.
                let elem = substs.types().first().copied();
                if elem.is_some_and(|elem| {
                    !matches!(tcx.kind_of(peel_refs(tcx, elem)), TyKind::Int(_))
                }) {
                    return PrintKind::Unsupported("container element needs a descriptor");
                }
                return PrintKind::HandleFormat(sym);
            }
            match tcx.kind_of(ty) {
                TyKind::Bool => PrintKind::Bool,
                TyKind::Char => PrintKind::Char,
                TyKind::Int(int_ty) => {
                    // A runtime container the MIR types as its bare i64
                    // handle renders through the container's own shim,
                    // not as the pointer value.
                    if place.projection.is_empty()
                        && let Some(kind) = container_handle_print_kind(body, place.local)
                    {
                        return kind;
                    }
                    // A value's declared width decides how it reads: a
                    // `u64` / `usize` spans the whole unsigned range, so
                    // one at or above `i64::MAX` prints as its own
                    // decimal rather than the negative the same slot
                    // spells. Every narrower int is signed at runtime and
                    // prints signed; `u128` keeps the unsigned printer
                    // outright.
                    if matches!(int_ty, IntTy::U64 | IntTy::Usize | IntTy::U128) {
                        PrintKind::Uint
                    } else {
                        PrintKind::Int
                    }
                }
                // `time::Duration` / `time::Instant` are transparent
                // `i64`s; print the millisecond count they carry.
                TyKind::Unit | TyKind::Never | TyKind::Duration | TyKind::Instant => PrintKind::Int,
                TyKind::Float(_) => PrintKind::Float,
                TyKind::String | TyKind::Ref { .. } => PrintKind::StrPtr,
                // `Var(_)` means the typechecker did not resolve
                // this operand's type. The dominant producer of
                // unresolved-typed locals that flow into println
                // is `__concat` (whose return type is currently
                // not pinned by the typechecker - it returns a
                // String pointer at runtime). Falling back to
                // StrPtr keeps `println!("a={n}")` correct;
                // falling back to Int (the previous default)
                // re-prints the empty-string pointer as a giant
                // integer.
                TyKind::Var(_) => PrintKind::StrPtr,
                // Aggregate / collection / variant-typed values
                // need a Display impl to print sensibly. The
                // compiled tier doesn't dispatch user-defined
                // Display, and silently printing a stack
                // pointer (the previous behavior) is a footgun.
                // Refuse loudly so the user knows to call
                // `format!("{x:?}")` or write their own
                // stringification.
                TyKind::Tuple(_) => {
                    if tuple_tags(tcx, body, operand).is_some() {
                        PrintKind::Tuple
                    } else {
                        PrintKind::Unsupported("tuple")
                    }
                }
                // Fixed-size arrays: flat slot storage. The runtime
                // helpers that print `VecI64` / `VecF64` / etc. read
                // a `*mut GosVec` header, but a fixed array is just
                // a stack slot of `N * elem_bytes`. Routing fixed
                // arrays through the same Vec print kind works
                // because `nums` ends up as a `*mut GosVec` after
                // the typed-array promotion (`BuildIntArray` etc.)
                // - without this, `let nums = [1, 2, 3]; println!
                // ("{:?}", nums)` printed `<value>` even though
                // the array is fully typed and the helper exists.
                TyKind::Array { elem, len } | TyKind::Simd { elem, lanes: len } => {
                    let n = i64::try_from(len.to_usize()).unwrap_or(0);
                    match tcx.kind_of(*elem) {
                        // A `u64` / `usize` slot printed as a signed word
                        // would spell a value at or above `i64::MAX` negative.
                        TyKind::Int(IntTy::U64 | IntTy::Usize) => {
                            PrintKind::Unsupported("unsigned array")
                        }
                        TyKind::Int(_) => PrintKind::ArrI64(n),
                        TyKind::Float(_) => PrintKind::ArrF64(n),
                        TyKind::Bool => PrintKind::ArrBool(n),
                        TyKind::String => PrintKind::ArrString(n),
                        // Nested fixed array: rows are inline (N * M
                        // contiguous slots), so the formatter takes both
                        // static lengths.
                        TyKind::Array {
                            elem: inner_elem,
                            len: inner_len,
                        } => {
                            let m = i64::try_from(inner_len.to_usize()).unwrap_or(0);
                            match tcx.kind_of(*inner_elem) {
                                TyKind::Int(IntTy::U64 | IntTy::Usize) => {
                                    PrintKind::Unsupported("unsigned nested array")
                                }
                                TyKind::Int(_) => PrintKind::ArrArrI64(n, m),
                                TyKind::Float(_) => PrintKind::ArrArrF64(n, m),
                                TyKind::Bool => PrintKind::ArrArrBool(n, m),
                                _ => PrintKind::Unsupported("nested array"),
                            }
                        }
                        _ => PrintKind::Unsupported("array"),
                    }
                }
                // A slice renders in bare brackets over the same runtime
                // object a `Vec` carries, so it wraps the `Vec` kind rather
                // than naming a parallel set of its own.
                TyKind::Slice(elem) => {
                    let inner = match tcx.kind_of(*elem) {
                        TyKind::Int(IntTy::U64 | IntTy::Usize) => PrintKind::VecUint,
                        TyKind::Int(_) => PrintKind::VecI64,
                        TyKind::Float(_) => PrintKind::VecF64,
                        TyKind::Bool => PrintKind::VecBool,
                        TyKind::String => PrintKind::VecString,
                        TyKind::Vec(inner) => match tcx.kind_of(*inner) {
                            TyKind::Int(IntTy::U64 | IntTy::Usize) => {
                                PrintKind::Unsupported("unsigned nested slice")
                            }
                            TyKind::Int(_) => PrintKind::VecVecI64,
                            TyKind::String => PrintKind::VecVecString,
                            _ => PrintKind::Unsupported("nested slice"),
                        },
                        TyKind::Tuple(_) => vec_tuple_kind(tcx, *elem, "slice"),
                        _ => PrintKind::Unsupported("slice"),
                    };
                    match inner {
                        PrintKind::Unsupported(reason) => PrintKind::Unsupported(reason),
                        kind => PrintKind::Seq(Box::new(kind)),
                    }
                }
                TyKind::Vec(elem) => match tcx.kind_of(*elem) {
                    TyKind::Int(IntTy::U64 | IntTy::Usize) => PrintKind::VecUint,
                    TyKind::Int(_) => PrintKind::VecI64,
                    TyKind::Float(_) => PrintKind::VecF64,
                    TyKind::Bool => PrintKind::VecBool,
                    TyKind::String => PrintKind::VecString,
                    TyKind::Vec(inner) => match tcx.kind_of(*inner) {
                        TyKind::Int(IntTy::U64 | IntTy::Usize) => {
                            PrintKind::Unsupported("unsigned nested Vec")
                        }
                        TyKind::Int(_) => PrintKind::VecVecI64,
                        TyKind::String => PrintKind::VecVecString,
                        _ => PrintKind::Unsupported("nested Vec"),
                    },
                    TyKind::Tuple(_) => vec_tuple_kind(tcx, *elem, "Vec"),
                    _ => PrintKind::Unsupported("Vec"),
                },
                TyKind::Iterator(_) | TyKind::Range(_) => PrintKind::Unsupported("iterator"),
                TyKind::HashMap { key, value, .. } => {
                    let unsigned_tag = |ty: Ty| {
                        let mut peeled = ty;
                        while let TyKind::Ref { inner, .. } = tcx.kind_of(peeled) {
                            peeled = *inner;
                        }
                        u8::from(matches!(
                            tcx.kind_of(peeled),
                            TyKind::Int(IntTy::U64 | IntTy::Usize)
                        ))
                    };
                    if !(map_kv_supported(tcx, *key) && map_kv_supported(tcx, *value)) {
                        PrintKind::Unsupported("HashMap")
                    } else if unsigned_tag(*key) == 1 || unsigned_tag(*value) == 1 {
                        PrintKind::MapTagged(unsigned_tag(*key), unsigned_tag(*value))
                    } else {
                        PrintKind::Map
                    }
                }
                TyKind::Sender(_) | TyKind::Receiver(_) | TyKind::JoinHandle(_) => {
                    PrintKind::Unsupported("channel")
                }
                TyKind::JsonValue => PrintKind::JsonValue,
                TyKind::DynValue => PrintKind::DynValue,
                // `{:?}` of a built-in by-value enum (`Option` def `u32::MAX-1`,
                // `Result` def `u32::MAX`) with scalar / String payloads -
                // rendered via the runtime debug helper. User structs / enums
                // with a derived fmt are routed before reaching here.
                TyKind::Adt { def, substs }
                    if def.local == u32::MAX || def.local == u32::MAX - 1 =>
                {
                    let tys = substs.types();
                    if def.local == u32::MAX - 1 {
                        match tys.first().and_then(|t| debug_payload_kind(tcx, *t)) {
                            Some(k) => PrintKind::Option(k),
                            None => PrintKind::Unsupported("struct or enum"),
                        }
                    } else {
                        match (
                            tys.first().and_then(|t| debug_payload_kind(tcx, *t)),
                            tys.get(1).and_then(|t| debug_payload_kind(tcx, *t)),
                        ) {
                            (Some(ok), Some(err)) => PrintKind::Result(ok, err),
                            _ => PrintKind::Unsupported("struct or enum"),
                        }
                    }
                }
                TyKind::Adt { def, substs }
                    if def.local == u32::MAX - 7 || def.local == u32::MAX - 18 =>
                {
                    let ordered = i32::from(def.local == u32::MAX - 18);
                    match substs.types().first().map(|elem| tcx.kind_of(*elem)) {
                        Some(TyKind::Int(IntTy::U64 | IntTy::Usize)) => {
                            PrintKind::SetFormat("gos_rt_set_format_u64", ordered)
                        }
                        Some(TyKind::Int(_)) => {
                            PrintKind::SetFormat("gos_rt_set_format_i64", ordered)
                        }
                        Some(TyKind::String) => {
                            PrintKind::SetFormat("gos_rt_set_format_string", ordered)
                        }
                        _ => PrintKind::Unsupported("set"),
                    }
                }
                TyKind::Adt { .. } => PrintKind::Unsupported("struct or enum"),
                TyKind::Closure { .. } => PrintKind::Unsupported("closure"),
                TyKind::FnDef { .. } | TyKind::FnPtr(_) | TyKind::FnTrait(_) => {
                    PrintKind::Unsupported("function")
                }
                TyKind::Dyn(_) => PrintKind::Unsupported("dyn Trait"),
                TyKind::DynError => PrintKind::ErrorMessage,
                TyKind::Param { .. } | TyKind::Alias { .. } | TyKind::Error => {
                    PrintKind::Unsupported("opaque type")
                }
                TyKind::Nominal { .. } => unreachable!("nominal aliases are peeled above"),
            }
        }
        Operand::FnRef { .. } => PrintKind::Unsupported("function"),
    }
}

pub(super) fn operand_is_string(tcx: &TyCtxt, body: &Body, operand: &Operand) -> bool {
    match operand {
        // After copy-propagation, string locals may be substituted with
        // Const(Str(...)) inline. These are always strings.
        Operand::Const(gossamer_mir::ConstValue::Str(_)) => return true,
        Operand::Copy(p) => {
            let ty = resolve_place_ty(tcx, body, p);
            return match tcx.kind_of(ty) {
                TyKind::String => true,
                TyKind::Ref { inner, .. } => matches!(tcx.kind_of(*inner), TyKind::String),
                _ => false,
            };
        }
        _ => {}
    }
    false
}

pub(super) fn operand_is_char(body: &Body, tcx: &TyCtxt, op: &Operand) -> bool {
    match op {
        Operand::Const(ConstValue::Char(_)) => true,
        Operand::Copy(p) => matches!(tcx.kind_of(body.local_ty(p.local)), TyKind::Char),
        _ => false,
    }
}

/// True for a user-declared struct ADT: not an enum, not an `Option` /
/// `Result` carrier, and not one of the opaque stdlib heap-blob handles whose
/// `DefId` sits in the `u32::MAX - 16 ..= u32::MAX` sentinel range.
fn is_user_struct_adt(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind_of(ty),
        TyKind::Adt { def, substs }
            if def.local < u32::MAX - 16
                && !tcx.is_inline_enum_ty(ty)
                && tcx.adt_field_tys(*def, substs).is_some()
    )
}

pub(super) fn operand_aggregate_slots(body: &Body, tcx: &TyCtxt, op: &Operand) -> Option<u32> {
    match op {
        Operand::Copy(place) if place.projection.is_empty() => {
            let ty = body.local_ty(place.local);
            // `Option` / `Result` carriers are two-word *values* (i128
            // SSA), not address-backed aggregates - callers must not
            // take their address and word-copy through it.
            if is_carrier_ty(tcx, ty) {
                return None;
            }
            if matches!(
                tcx.kind_of(ty),
                TyKind::Tuple(_) | TyKind::Adt { .. } | TyKind::Array { .. }
            ) {
                let slots = type_slot_count(tcx, ty);
                // MIR treats an aggregate as address-is-value at every
                // width, so a one-field struct, a one-element tuple, and a
                // one-element array are each handed over as the address of
                // their backing storage - the consumer memcpys the single
                // slot. Reading the local as a scalar word instead would
                // hand over the storage pointer as if it were the field. An
                // opaque stdlib heap-blob handle (`def.local` in the
                // `u32::MAX - 16 ..= u32::MAX` sentinel range) genuinely is
                // a pointer word, so it stays by value.
                if tcx.elem_is_addressed_aggregate(ty) {
                    return Some(slots.max(1));
                }
            }
            None
        }
        _ => None,
    }
}

pub(super) fn operand_cabi_ty(
    operand: &Operand,
    body: &Body,
    tcx: &TyCtxt,
    ptr_ty: ir::Type,
) -> ir::Type {
    match operand {
        Operand::Copy(place) => {
            mir_ty_to_cabi(tcx, body.local_ty(place.local), ptr_ty).unwrap_or(types::I64)
        }
        Operand::Const(value) => match value {
            ConstValue::Bool(_) => types::I8,
            ConstValue::Float(_) => types::F64,
            ConstValue::Char(_) => types::I32,
            ConstValue::Str(_) => ptr_ty,
            _ => types::I64,
        },
        Operand::FnRef { .. } => ptr_ty,
    }
}
