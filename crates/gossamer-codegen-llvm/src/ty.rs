//! MIR `Ty` → LLVM IR type string.
//!
//! The emitter works in textual IR so types are rendered as
//! the short strings LLVM expects (`i64`, `double`, `i1`,
//! `ptr`, …). Aggregates that don't fit in a register
//! (strings, slices, arbitrary structs) flow through the
//! runtime as opaque `ptr` - same choice the Cranelift
//! backend makes in `lower_ty`.

use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

/// Aggregate locals above this footprint are backed by a runtime heap
/// allocation instead of an LLVM stack `alloca`. This is a codegen storage
/// choice: fixed-size array semantics remain `[T; N]`.
pub(crate) const STACK_AGGREGATE_SPILL_BYTES: u64 = 512 * 1024;

/// LLVM type rendering for a MIR type. Returns the short
/// textual form (`i64`, `double`, `i1`, `ptr`, `void`).
///
/// Canonical integer model: every integer type up to 64 bits is a
/// 64-bit runtime value, matching the bytecode VM (which computes
/// all integer arithmetic at i64 width) and the 8-byte GosVec /
/// flat-slot storage convention. Narrow declared widths (u8/i8/
/// u16/i16/u32/i32) only matter at explicit `as` casts, which mask
/// to the target width - see `lower_cast`. Rendering them as
/// narrow LLVM types made arithmetic wrap at the declared width
/// (`sum += b` over `[u8]` gave sum mod 256) and produced invalid
/// mixed-width IR when MIR pairs an i64 local with a u8 operand.
pub(crate) fn render_ty(tcx: &TyCtxt, ty: Ty) -> String {
    match tcx.kind(ty) {
        Some(TyKind::Unit) => "void".to_string(),
        Some(TyKind::Bool) => "i1".to_string(),
        Some(TyKind::Int(
            IntTy::I8
            | IntTy::U8
            | IntTy::I16
            | IntTy::U16
            | IntTy::I32
            | IntTy::U32
            | IntTy::I64
            | IntTy::U64
            | IntTy::Isize
            | IntTy::Usize,
        )) => "i64".to_string(),
        Some(TyKind::Int(IntTy::I128 | IntTy::U128)) => "i128".to_string(),
        // `f32` is represented as `double` at runtime, matching the
        // bytecode VM, which computes every float operation at f64 width
        // and stores f32 values as f64 - the `f32` annotation only rounds
        // at an explicit `as f32` cast (see `lower_cast`). Rendering it as
        // a 32-bit `float` made compiled-tier f32 arithmetic diverge from
        // the VM (`3.5 / 1.5` differed in the trailing digits) and produced
        // width-mismatched IR against the 8-byte slot model.
        Some(TyKind::Float(FloatTy::F32 | FloatTy::F64)) => "double".to_string(),
        Some(TyKind::Char) => "i32".to_string(),
        // `Result<T,E>` (sentinel def `u32::MAX`) and `Option<T>`
        // (`u32::MAX - 1`) are a 2-word by-value `i128` (disc + payload),
        // not a heap box - see `gos_rt_result_new`.
        Some(TyKind::Adt { def, .. }) if def.local == u32::MAX || def.local == u32::MAX - 1 => {
            "i128".to_string()
        }
        // Inline-able user enums share the same 2-word by-value `i128` shape.
        Some(TyKind::Adt { .. }) if tcx.is_inline_enum_ty(ty) => "i128".to_string(),
        // A runtime container or opaque handle occupies one i64 slot: the
        // MIR produces it from its constructor as an `i64` and hands it
        // back to every accessor as one. The annotated type and the
        // constructed value must render alike, or a parameter slot and its
        // argument disagree at the call boundary.
        Some(TyKind::Adt { def, .. }) if is_bare_handle_def(def.local) => "i64".to_string(),
        Some(TyKind::String) => "ptr".to_string(),
        // A reference to a 2-word by-value enum (`&Option` / `&Result` /
        // `&InlineEnum`) carries the i128 value itself - the reference is
        // transparent in this codegen. Rendering it as `ptr` would truncate
        // the aggregate to its low word at every call / field / return
        // boundary, discarding the payload.
        Some(TyKind::Ref { inner, .. }) if is_by_value_enum(tcx, *inner) => "i128".to_string(),
        Some(TyKind::Ref { .. }) => "ptr".to_string(),
        Some(TyKind::FnPtr(_) | TyKind::FnDef { .. }) => "ptr".to_string(),
        Some(
            TyKind::Array { .. }
            | TyKind::Slice(_)
            | TyKind::Vec(_)
            | TyKind::Iterator(_)
            | TyKind::Adt { .. }
            | TyKind::Tuple(_)
            | TyKind::Dyn(_)
            | TyKind::HashMap { .. }
            | TyKind::Sender(_)
            | TyKind::Receiver(_)
            | TyKind::JoinHandle(_),
        ) => "ptr".to_string(),
        // `Never` / `Error` / `Var` / `Param` / `Closure` /
        // `Alias` - treated as opaque pointers by the runtime
        // so the backend can still emit a signature that
        // typechecks.
        _ => "ptr".to_string(),
    }
}

/// LLVM type rendering for a parameter or argument slot. `void` is a
/// return-only type, so a zero-sized parameter (a field-less struct, `()`)
/// takes an unread pointer word instead. Definition, declaration, and call
/// site all go through this so the three agree on arity and shape.
pub(crate) fn param_llvm_ty(tcx: &TyCtxt, ty: Ty) -> String {
    let rendered = render_ty(tcx, ty);
    if rendered == "void" {
        "ptr".to_string()
    } else {
        rendered
    }
}

/// True when a sentinel `DefId` names a runtime value the MIR carries in
/// a single i64 slot: the collection handles (`HashSet` `u32::MAX - 7`,
/// `BTreeSet` `- 18`, `Deque` `- 19`, `MaxHeap` `- 28`, `MinHeap` `- 30`,
/// `Queue` `- 31`, `Stack` `- 32`) and the opaque stdlib handles in the
/// `- 48 ..= - 34` band, the `std::sync` band `- 57 ..= - 50`, and the
/// `trace` spans `- 59` / `- 60`. The shared word and byte buffers (`I64Vec`
/// `- 58`, `U8Vec` `- 20`) are pointers. The field-bearing sentinel blobs
/// (`http::ResponseStream`, `http::Response`) are excluded: their fields are
/// read through a pointer.
fn is_bare_handle_def(def_local: u32) -> bool {
    let offset = u32::MAX - def_local;
    matches!(offset, 7 | 18 | 19 | 28 | 30 | 31 | 32)
        || (34..=48).contains(&offset)
        || (50..=57).contains(&offset)
        || matches!(offset, 59 | 60)
}

/// True when `ty` lowers to the 2-word by-value enum representation:
/// the `Option` / `Result` sentinel Adts (`u32::MAX` / `u32::MAX - 1`)
/// or an inline-able user enum. These cross the ABI as a packed `i128`.
fn is_by_value_enum(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind(ty),
        Some(TyKind::Adt { def, .. }) if def.local == u32::MAX || def.local == u32::MAX - 1
    ) || tcx.is_inline_enum_ty(ty)
}

/// Convenience: returns `true` when the type is `()`, i.e.
/// should be elided in LLVM (no return value, no parameter).
pub(crate) fn is_unit(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(tcx.kind(ty), Some(TyKind::Unit))
}

/// Returns the LLVM IR integer width for an integer type,
/// used by `Cast` to pick `trunc` / `zext` / `sext`.
pub(crate) fn int_width(int_ty: IntTy) -> u32 {
    match int_ty {
        IntTy::I8 | IntTy::U8 => 8,
        IntTy::I16 | IntTy::U16 => 16,
        IntTy::I32 | IntTy::U32 => 32,
        IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
        IntTy::I128 | IntTy::U128 => 128,
    }
}

/// Returns `true` when the integer type is signed - controls
/// `sdiv`/`udiv`, `srem`/`urem`, `icmp slt` vs `icmp ult`
/// selection.
pub(crate) fn int_signed(int_ty: IntTy) -> bool {
    matches!(
        int_ty,
        IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64 | IntTy::I128 | IntTy::Isize
    )
}

/// Classifies the numeric family of a [`Ty`] for `BinaryOp`
/// dispatch (int vs float vs other).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumericKind {
    Int(IntTy),
    Float(FloatTy),
    Other,
}

pub(crate) fn numeric_kind(tcx: &TyCtxt, ty: Ty) -> NumericKind {
    match tcx.kind(ty) {
        Some(TyKind::Int(i)) => NumericKind::Int(*i),
        Some(TyKind::Float(f)) => NumericKind::Float(*f),
        _ => NumericKind::Other,
    }
}

/// Size in 8-byte slots of a `Ty` when it's laid out as a
/// flat aggregate (matches what the Cranelift backend does -
/// every scalar field takes one i64-wide slot, structs /
/// tuples chain their fields, arrays stride by
/// `elem_count × elem_slots`). Scalars / opaque pointers
/// count as 1. When the shape isn't statically determinable
/// (an inference variable, an unknown `Adt` def) we return
/// `None` so the caller can fall back to scalar handling.
pub(crate) fn slot_count(tcx: &TyCtxt, ty: Ty) -> Option<u32> {
    match tcx.kind(ty)? {
        TyKind::Unit => Some(0),
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::String
        | TyKind::Ref { .. }
        | TyKind::FnPtr(_)
        | TyKind::FnDef { .. }
        | TyKind::Slice(_)
        | TyKind::Vec(_)
        | TyKind::Iterator(_)
        | TyKind::HashMap { .. }
        | TyKind::Sender(_)
        | TyKind::Receiver(_)
        | TyKind::JoinHandle(_) => Some(1),
        TyKind::Tuple(elems) => {
            // Mirror `Array`'s behaviour: if any element type didn't
            // resolve to anything concrete (typeck left a `Var(_)`),
            // assume one slot instead of collapsing the whole tuple
            // to `None`. Without this fallback, a tuple literal whose
            // elements have inference variables (e.g. the operand of
            // `xs.push((1, 1.5))` whose tuple-local is left as
            // `(Var, Var)` because the surrounding `Vec<(i64, f64)>`
            // element type didn't reach the operands) collapses the
            // alloca to a single slot, and the second-slot store
            // overflows the alloca and clobbers adjacent stack
            // memory. Same root cause for the `(array, scalar)` /
            // nested-loop tuple-return regressions.
            let mut total = 0u32;
            for e in elems {
                total += slot_count(tcx, *e).unwrap_or(1).max(1);
            }
            Some(total)
        }
        TyKind::Array { elem, len } => {
            // An array whose element type didn't resolve (e.g. the
            // typechecker leaked a `Var(...)`) still has a known
            // length. Assume the element is scalar (1 slot) instead
            // of returning `None`, which collapses the alloca to a
            // single i64 slot and makes a 3-element array literal
            // overflow into adjacent locals.
            let elem_slots = slot_count(tcx, *elem).unwrap_or(1).max(1);
            Some(elem_slots * (len.to_usize() as u32))
        }
        TyKind::Adt { def, substs } => {
            // `Result<T,E>` (sentinel `u32::MAX`) and `Option<T>`
            // (`u32::MAX - 1`) are the 2-word by-value `i128` (16-byte)
            // representation: 2 flat slots. Inside an aggregate (array/Vec/
            // struct element) they occupy two i64 slots, not one - sizing
            // them at one slot makes adjacent elements overlap and clobber
            // the payload (every-other Some loses its value).
            if def.local == u32::MAX || def.local == u32::MAX - 1 || tcx.is_inline_enum_ty(ty) {
                return Some(2);
            }
            // `http::Response` is the only sentinel stdlib struct
            // backed by a `repr(Rust)` runtime struct (`GosHttpResponse`)
            // rather than an inline-flat heap blob. Its accessors
            // (`gos_rt_http_response_*`) take `*const GosHttpResponse`
            // and read fields at Rust-decided offsets, so the local
            // must round-trip the raw pointer instead of memcpy'ing
            // an inline view (which would truncate / clobber given
            // the field reordering Rust applies). Reporting `None`
            // here picks the heap-pointer code path in
            // `lower_call_arg` and the assignment-of-Ok-payload sites.
            //
            // The sibling sentinel ResponseStream @ u32::MAX-4 IS an
            // inline heap blob the runtime allocates with raw `*mut i64`
            // sized for the declared field count - their fields are
            // read by `Field(idx)` projection, so the existing
            // inline slot_count path is correct.
            if def.local == u32::MAX - 5 {
                return None;
            }
            // `struct_field_tys` returning `None` is the
            // genuinely-unknown-layout case (recursive enum, opaque
            // sentinel) - keep that as `None` so the caller falls
            // through to the heap-pointer path. When the field list
            // exists but a single field has a `Var` type, mirror
            // the `Tuple` fallback above so the alloca still gets
            // sized for the known fields.
            if let Some(layout) = tcx.packed_layout(ty) {
                return Some(layout.size / 8);
            }
            let field_tys = tcx.adt_field_tys(*def, substs)?;
            let mut total = 0u32;
            for t in field_tys {
                total += slot_count(tcx, *t).unwrap_or(1).max(1);
            }
            Some(total)
        }
        _ => None,
    }
}

/// Byte footprint for an inline aggregate's flat-slot representation.
pub(crate) fn aggregate_storage_bytes(tcx: &TyCtxt, ty: Ty) -> Option<u64> {
    if let Some(len) = packed_byte_array_len(tcx, ty) {
        return Some(len.max(1));
    }
    slot_count(tcx, ty).map(|slots| u64::from(slots.max(1)) * 8)
}

/// Direct fixed arrays of bytes use their declared element width in native
/// stack storage. Integer arithmetic still uses the canonical i64 value model.
pub(crate) fn packed_byte_array_len(tcx: &TyCtxt, ty: Ty) -> Option<u64> {
    let TyKind::Array { elem, len } = tcx.kind(ty)? else {
        return None;
    };
    matches!(tcx.kind(*elem), Some(TyKind::Int(IntTy::U8))).then(|| len.to_usize() as u64)
}

/// Size in slots of a *single element* of an aggregate type -
/// 1 for scalar arrays, `fields.len()` for arrays of structs,
/// used to compute the array stride when lowering
/// `a[i].field` projections.
pub(crate) fn elem_slots(tcx: &TyCtxt, ty: Ty) -> u32 {
    match tcx.kind(ty) {
        Some(TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem)) => {
            slot_count(tcx, *elem).unwrap_or(1)
        }
        _ => 1,
    }
}

/// Returns the slot offset (in 8-byte words) of field `idx` of
/// `ty` - the sum of `slot_count` for every preceding field. Used
/// by the projection lowerers so a nested struct/tuple field
/// (`outer.inner.x`) lands past the inline-flattened sub-aggregate
/// instead of overlapping its first scalar.
pub(crate) fn field_slot_offset(tcx: &TyCtxt, ty: Ty, idx: u32) -> u32 {
    let target = idx as usize;
    match tcx.kind(ty) {
        Some(TyKind::Tuple(elems)) => elems
            .iter()
            .take(target)
            .map(|t| slot_count(tcx, *t).unwrap_or(1).max(1))
            .sum(),
        Some(TyKind::Adt { def, substs }) => {
            if def.local == u32::MAX || def.local == u32::MAX - 1 || tcx.is_inline_enum_ty(ty) {
                return idx;
            }
            tcx.adt_field_tys(*def, substs).map_or(idx, |tys| {
                tys.iter()
                    .take(target)
                    .map(|t| slot_count(tcx, *t).unwrap_or(1).max(1))
                    .sum()
            })
        }
        Some(TyKind::Array { elem, .. }) => {
            idx.saturating_mul(slot_count(tcx, *elem).unwrap_or(1).max(1))
        }
        Some(TyKind::Ref { inner, .. }) => field_slot_offset(tcx, *inner, idx),
        _ => idx,
    }
}

/// Retired with the bump arena. Kept around for one release cycle
/// in case downstream forks reach for it; new code should not.
#[allow(dead_code)]
pub(crate) fn is_pure_primitive_aggregate(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind(ty) {
        Some(TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Float(_) | TyKind::Unit) => {
            true
        }
        Some(TyKind::Array { elem, .. }) => is_pure_primitive_aggregate(tcx, *elem),
        Some(TyKind::Tuple(elems)) => elems.iter().all(|t| is_pure_primitive_aggregate(tcx, *t)),
        Some(TyKind::Adt { def, substs }) => {
            // Reject the Result/Option sentinel Adts up front -
            // they are pointer-shaped and not really aggregates.
            if def.local == u32::MAX || def.local == u32::MAX - 1 || tcx.is_inline_enum_ty(ty) {
                return false;
            }
            match tcx.adt_field_tys(*def, substs) {
                Some(fields) => fields.iter().all(|t| is_pure_primitive_aggregate(tcx, *t)),
                None => false,
            }
        }
        _ => false,
    }
}

/// True when the type is an aggregate whose memory lives in a
/// stack slot rather than a scalar SSA value. Drives the
/// choice between a scalar `alloca <ty>` and an aggregate
/// `alloca [N x i64]`.
pub(crate) fn is_aggregate(tcx: &TyCtxt, ty: Ty) -> bool {
    if let Some(TyKind::Adt { def, .. }) = tcx.kind(ty) {
        // Result/Option sentinel Adts (DefId::local == u32::MAX or
        // u32::MAX - 1) are heap-allocated `*mut GosResult` values
        // returned from runtime helpers. Treating them as flat-slot
        // aggregates here makes `emit_named_call` memcpy the first
        // 8 bytes of the runtime's 16-byte struct into a
        // `[1 x i64]` alloca and then pass `ptr %alloca` to the
        // next helper - which reads stack garbage as the payload.
        // Treat them as scalar `ptr`s so the caller stores the
        // returned pointer directly into the local slot.
        if def.local == u32::MAX || def.local == u32::MAX - 1 || tcx.is_inline_enum_ty(ty) {
            return false;
        }
        // A runtime container or opaque handle is one i64 slot holding the
        // handle itself, so it moves by value like a scalar.
        if is_bare_handle_def(def.local) {
            return false;
        }
    }
    matches!(
        tcx.kind(ty),
        Some(TyKind::Array { .. } | TyKind::Tuple(_) | TyKind::Adt { .. })
    )
}
