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
#![forbid(unsafe_code)]
use super::*;
use std::collections::HashMap;
use std::fmt::Write as _;

use crate::BuildError;
use crate::ty::packed_byte_array_len;
use anyhow::Result;
use gossamer_abi as abi;
use gossamer_mir::{
    BasicBlock, BinOp, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, Statement,
    StatementKind, Terminator, UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

impl<'a> Lowerer<'a> {
    pub(crate) fn lower_operand(&mut self, op: &Operand) -> Result<String, BuildError> {
        match op {
            Operand::Copy(place) => Ok(self.lower_place_read(place)),
            Operand::Const(ConstValue::Str(text)) => {
                let (name, _len) = self.strings.borrow_mut().intern(text);
                Ok(name)
            }
            Operand::Const(value) => Ok(render_const(value)),
            Operand::FnRef { def, .. } => {
                // Emit the function symbol as a `ptr` value - that
                // matches `operand_llvm_ty(FnRef) = "ptr"` and the
                // `FnDef → ptr` rendering of the destination slot.
                // The MIR lowerer also stuffs fn addresses into
                // `i64` slots for the goroutine-spawn path; the
                // assignment-store branch in `lower_assign` coerces
                // ptr → i64 (`ptrtoint`) when the destination's
                // leaf type is integer-shaped, so both shapes get a
                // well-typed `store`.
                if let Some(name) = self.fn_name_by_def.get(&def.local).cloned() {
                    if let Some(argc) = super::lower_call::external_binding_arity(&name) {
                        let Some(symbol) =
                            super::lower_call::resolve_external_binding_symbol(&name, argc)
                        else {
                            return Err(BuildError::InternalLoweringBug(
                                "external binding FnRef symbol resolution failed",
                            ));
                        };
                        return Ok(format!("@\"{symbol}\""));
                    }
                    return Ok(format!("@\"{}\"", mangle_fn_name(&name)));
                }
                Err(BuildError::InternalLoweringBug(
                    "FnRef operand has no resolvable function name",
                ))
            }
        }
    }

    /// Reads a place, walking its `projection` chain. For a
    /// plain local (no projection) this is a single `load`
    /// against the stack slot. For `a[i].field` chains we
    /// compute the byte-offset address via `getelementptr i64`
    /// steps and then `load` the leaf scalar at its native
    /// type - matching the flat-slot layout the Cranelift
    /// backend emits. String byte indexing (`s[i]` where `s`
    /// is `String` or `&String`) is short-circuited through
    /// `gos_rt_str_byte_at`.
    pub(crate) fn lower_place_read(&mut self, place: &Place) -> String {
        if let Some(value) = self.try_string_byte_read(place) {
            return value;
        }
        let leaf_ty = self.place_leaf_ty(place);
        let leaf_llvm = render_ty(self.tcx, leaf_ty);
        if leaf_llvm == "void" {
            return String::new();
        }
        if place.projection.is_empty() {
            // Multi-slot aggregates (`[Body; 5]`, structs, tuples)
            // are stored as a flat `[N x i64]` slab - the "value"
            // downstream code expects is the slot address itself.
            //
            // Exception: enum (Adt) types whose field layout is unknown
            // to the type context (`slot_count = None`) are heap-pointer
            // aggregates. Their `[1 x i64]` slot holds a heap pointer,
            // not inline field data. For these, do a `load ptr` to
            // recover the heap pointer rather than returning the stack
            // slot address. Example: `List { Cons(i64, Box<List>), Nil }`.
            let local_ty = self.body.local_ty(place.local);
            if is_aggregate(self.tcx, local_ty) {
                let sc = slot_count(self.tcx, local_ty);
                // Enum / unknown-layout Adt: slot_count returns None.
                // Load the pointer value stored in the slot.
                if sc.is_none() {
                    let tmp = self.fresh();
                    writeln!(
                        self.out,
                        "  {tmp} = load {leaf_llvm}, ptr {slot}",
                        slot = local_slot(place.local)
                    )
                    .unwrap();
                    return tmp;
                }
                // Multi-slot or single-field struct: the address IS the value.
                return local_slot(place.local);
            }
            let tmp = self.fresh();
            writeln!(
                self.out,
                "  {tmp} = load {leaf_llvm}, ptr {slot}",
                slot = local_slot(place.local)
            )
            .unwrap();
            return tmp;
        }
        let addr = self.lower_place_address(place);
        // When the projected leaf is itself a multi-slot aggregate
        // (struct/tuple/array embedded inline), return its address
        // rather than collapsing the sub-aggregate to its first
        // word. Mirrors the cranelift behaviour and keeps any
        // downstream `Field`/`Index` projections walking memory.
        if is_aggregate(self.tcx, leaf_ty) && slot_count(self.tcx, leaf_ty).unwrap_or(1) > 1 {
            return addr;
        }
        let tbaa = self.place_payload_tbaa(place);
        if self.place_is_packed_byte_element(place) {
            let byte = self.fresh();
            writeln!(self.out, "  {byte} = load i8, ptr {addr}{tbaa}").unwrap();
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = zext i8 {byte} to i64").unwrap();
            tmp
        } else {
            let tmp = self.fresh();
            writeln!(self.out, "  {tmp} = load {leaf_llvm}, ptr {addr}{tbaa}").unwrap();
            tmp
        }
    }

    /// Stores an SSA value into a MIR place, including projected destinations.
    /// This is the write-side twin of `lower_place_read`: a bare local stores
    /// into its alloca, while field/index/deref projections first compute the
    /// leaf address. Packed byte-array elements are stored as `i8` regardless
    /// of their logical integer-shaped leaf type.
    pub(crate) fn store_value_to_place(&mut self, place: &Place, llvm_ty: &str, value: &str) {
        // A unit destination holds no value: `void` is only legal as a
        // function result, so both the store and any cast feeding it are
        // invalid IR. The slot exists in MIR to name the assignment, and
        // nothing ever reads a value back out of it.
        if llvm_ty == "void" || llvm_ty.is_empty() {
            return;
        }
        let addr = if place.projection.is_empty() {
            local_slot(place.local)
        } else {
            self.lower_place_address(place)
        };
        let tbaa = self.place_payload_tbaa(place);
        if self.place_is_packed_byte_element(place) {
            let byte = self.fresh();
            writeln!(self.out, "  {byte} = trunc {llvm_ty} {value} to i8").unwrap();
            writeln!(self.out, "  store i8 {byte}, ptr {addr}{tbaa}").unwrap();
        } else {
            writeln!(self.out, "  store {llvm_ty} {value}, ptr {addr}{tbaa}").unwrap();
        }
    }

    pub(crate) fn store_zero_to_place(&mut self, place: &Place, llvm_ty: &str) {
        let zero = match llvm_ty {
            "ptr" => "null",
            "double" | "float" => "0.0",
            _ => "0",
        };
        self.store_value_to_place(place, llvm_ty, zero);
    }

    /// Computes the pointer address for a projected place.
    /// Walks `Field` / `Index` / `Deref` steps as byte-offset
    /// `getelementptr` instructions against the root local's
    /// stack slot (or a dereferenced pointer).
    pub(crate) fn lower_place_address(&mut self, place: &Place) -> String {
        let mut current = local_slot(place.local);
        let mut current_ty = self.body.local_ty(place.local);
        // If the root local is a reference (`&[Body; 5]`) or a
        // runtime-managed pointer (Vec, Slice, String,
        // HashMap, …), the local's *slot* holds a pointer
        // to the actual storage; load it once so subsequent
        // projections walk the referent rather than the
        // alloca itself. Stack-allocated aggregates ([Body;5]
        // declared inline) hold the data directly in their slot
        // so we leave `current` pointing at the alloca.
        //
        // Skip this auto-deref when the first projection is itself
        // an explicit `Deref` - the projection performs the same
        // pointer-load, and applying both produces a double-deref
        // that lands on garbage (the case `*s = expr` where
        // `s: &mut i64`).
        let skip_auto_deref = matches!(place.projection.first(), Some(Projection::Deref));
        // `current` alternates between a loaded pointer value and the address
        // of a slot holding one. A runtime-managed handle (`Vec`, `Slice`) is
        // the value, so a step that consumes one loads it out of its slot
        // first; a step that only walks an offset leaves an address behind.
        let mut current_is_loaded = false;
        if !skip_auto_deref && Self::is_pointer_local_ty(self.tcx, current_ty) {
            let next = self.fresh();
            writeln!(self.out, "  {next} = load ptr, ptr {current}").unwrap();
            current = next;
            current_ty = self.unwrap_ref(current_ty);
            current_is_loaded = true;
        }
        let mut stride_slots: u32 = elem_slots(self.tcx, current_ty);
        for proj in &place.projection {
            match proj {
                Projection::Field(idx) => {
                    if matches!(self.tcx.kind(current_ty), Some(TyKind::Int(_))) {
                        let raw_ptr_bits = self.fresh();
                        writeln!(self.out, "  {raw_ptr_bits} = load i64, ptr {current}").unwrap();
                        let as_ptr = self.fresh();
                        writeln!(self.out, "  {as_ptr} = inttoptr i64 {raw_ptr_bits} to ptr")
                            .unwrap();
                        current = as_ptr;
                    }
                    // Sum prior fields' slot widths so a nested
                    // struct field (`outer.inner.x`) lands past
                    // the embedded inner's full layout instead of
                    // overlapping it. Falls back to `idx` when the
                    // type is opaque (sentinel Adt, references) -
                    // in those cases each field is exactly one
                    // slot and `idx == slot_offset`.
                    let slot_offset = field_slot_offset(self.tcx, current_ty, *idx);
                    let next = self.fresh();
                    writeln!(
                        self.out,
                        "  {next} = getelementptr i64, ptr {current}, i64 {slot_offset}"
                    )
                    .unwrap();
                    current = next;
                    current_is_loaded = false;
                    // Advance current_ty so the next projection's
                    // stride and field-offset computation reflects
                    // the projected field, not the parent.
                    current_ty = match self.tcx.kind(current_ty) {
                        Some(TyKind::Adt { def, substs }) => self
                            .tcx
                            .adt_field_tys(*def, substs)
                            .and_then(|tys| tys.get(*idx as usize).copied())
                            .unwrap_or(current_ty),
                        Some(TyKind::Tuple(elems)) => {
                            elems.get(*idx as usize).copied().unwrap_or(current_ty)
                        }
                        Some(TyKind::Array { elem, .. }) => *elem,
                        _ => current_ty,
                    };
                    stride_slots = elem_slots(self.tcx, current_ty);
                }
                Projection::Index(index_local) => {
                    // Load the index value, widen to i64, then
                    // multiply by the per-element slot count
                    // and add to the base pointer.
                    let idx_slot = local_slot(*index_local);
                    let idx_raw = self.fresh();
                    writeln!(self.out, "  {idx_raw} = load i64, ptr {idx_slot}").unwrap();
                    let next = self.fresh();
                    // Indexing is checked uniformly for scalar and aggregate
                    // array elements; APIs that intend a non-panicking probe
                    // must name that operation explicitly.
                    self.emit_array_bounds_check(current_ty, &idx_raw);
                    // A `Vec` / slice keeps its elements behind the header's
                    // data pointer at the header's own stride, so the element
                    // address comes from the runtime rather than from a walk
                    // off the header. An array holds its elements inline and
                    // keeps the inline walk below.
                    if matches!(
                        self.tcx.kind(current_ty),
                        Some(TyKind::Vec(_) | TyKind::Slice(_))
                    ) {
                        // The helper takes the header itself, so a handle
                        // still sitting in a struct field or a Vec element
                        // slot is loaded out of it here.
                        let handle = if current_is_loaded {
                            current.clone()
                        } else {
                            let loaded = self.fresh();
                            writeln!(self.out, "  {loaded} = load ptr, ptr {current}").unwrap();
                            loaded
                        };
                        declare_rt(&mut self.runtime_refs, "gos_rt_vec_get_ptr");
                        writeln!(
                            self.out,
                            "  {next} = call ptr @gos_rt_vec_get_ptr(ptr {handle}, i64 {idx_raw})"
                        )
                        .unwrap();
                        current = next;
                        current_is_loaded = false;
                        current_ty = match self.tcx.kind(current_ty) {
                            Some(TyKind::Vec(elem) | TyKind::Slice(elem)) => *elem,
                            _ => current_ty,
                        };
                        stride_slots = elem_slots(self.tcx, current_ty);
                        continue;
                    }
                    if packed_byte_array_len(self.tcx, current_ty).is_some() {
                        writeln!(
                            self.out,
                            "  {next} = getelementptr i8, ptr {current}, i64 {idx_raw}"
                        )
                        .unwrap();
                    } else if stride_slots == 1 {
                        writeln!(
                            self.out,
                            "  {next} = getelementptr i64, ptr {current}, i64 {idx_raw}"
                        )
                        .unwrap();
                    } else {
                        let scaled = self.fresh();
                        writeln!(self.out, "  {scaled} = mul i64 {idx_raw}, {stride_slots}")
                            .unwrap();
                        writeln!(
                            self.out,
                            "  {next} = getelementptr i64, ptr {current}, i64 {scaled}"
                        )
                        .unwrap();
                    }
                    current = next;
                    current_is_loaded = false;
                    // Advance current_ty to the element type so
                    // subsequent projections (multi-dim array, nested
                    // field, …) see the right shape. Without this,
                    // `arr[i][j]` over `[[T; N]; M]` would use the
                    // outer-array's stride/bounds for the inner index
                    // and panic / corrupt data - the underlying
                    // 3D-array bug from iron_knight's `make_zobrist`.
                    current_ty = match self.tcx.kind(current_ty) {
                        Some(
                            TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem),
                        ) => *elem,
                        _ => current_ty,
                    };
                    stride_slots = elem_slots(self.tcx, current_ty);
                }
                Projection::Deref => {
                    let next = self.fresh();
                    writeln!(self.out, "  {next} = load ptr, ptr {current}").unwrap();
                    current = next;
                    current_is_loaded = true;
                    stride_slots = 1;
                }
                Projection::Discriminant => {
                    // Discriminant at offset 0; no pointer
                    // change, but later Field offsets walk past
                    // the tag word.
                    stride_slots = 1;
                }
                Projection::Downcast(_) => {
                    // Skip the 8-byte tag word to land on the
                    // payload.
                    let next = self.fresh();
                    writeln!(
                        self.out,
                        "  {next} = getelementptr i8, ptr {current}, i64 8"
                    )
                    .unwrap();
                    current = next;
                    current_is_loaded = false;
                    stride_slots = 1;
                }
            }
        }
        current
    }

    /// The `!tbaa` suffix for the leaf load/store of a projected place:
    /// [`TBAA_DATA`] when the leaf address provably lies in aggregate payload
    /// memory, or the empty string when it may not.
    ///
    /// This mirrors the pointer steps [`Self::lower_place_address`] takes.
    /// Struct/tuple/fixed-array projections walk byte offsets inside one flat
    /// i64 slot slab, and a `&T` dereference lands on another such slab or on
    /// a `Vec` element-buffer slot - all payload memory, disjoint from the
    /// `GosVec` / string header words `crate::lower::TBAA_HEADER` tags. A
    /// pointer step through a runtime-managed type (`Vec`, `Slice`, `String`,
    /// `HashMap`, an opaque handle) instead starts at that type's header, so
    /// those places stay untagged and keep aliasing everything.
    pub(crate) fn place_payload_tbaa(&self, place: &Place) -> &'static str {
        if place.projection.is_empty() {
            return "";
        }
        let mut ty = self.body.local_ty(place.local);
        let skip_auto_deref = matches!(place.projection.first(), Some(Projection::Deref));
        if !skip_auto_deref && Self::is_pointer_local_ty(self.tcx, ty) {
            // One pointer load, so peel exactly one reference layer; anything
            // else the slot can hold addresses a runtime header.
            let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) else {
                return "";
            };
            ty = *inner;
        }
        for proj in &place.projection {
            match proj {
                Projection::Field(idx) => {
                    // The integer-typed root reinterprets the slot's bits as a
                    // pointer to storage this walk cannot classify.
                    if matches!(self.tcx.kind(ty), Some(TyKind::Int(_))) {
                        return "";
                    }
                    ty = match self.tcx.kind(ty) {
                        Some(TyKind::Adt { def, substs }) => self
                            .tcx
                            .adt_field_tys(*def, substs)
                            .and_then(|tys| tys.get(*idx as usize).copied())
                            .unwrap_or(ty),
                        Some(TyKind::Tuple(elems)) => {
                            elems.get(*idx as usize).copied().unwrap_or(ty)
                        }
                        Some(TyKind::Array { elem, .. }) => *elem,
                        _ => return "",
                    };
                }
                Projection::Index(_) => match self.tcx.kind(ty) {
                    Some(TyKind::Array { elem, .. }) => ty = *elem,
                    _ => return "",
                },
                Projection::Deref => match self.tcx.kind(ty) {
                    Some(TyKind::Ref { inner, .. }) => ty = *inner,
                    _ => return "",
                },
                Projection::Discriminant | Projection::Downcast(_) => {}
            }
        }
        TBAA_DATA
    }

    /// The `!tbaa` suffix for the slot writes an aggregate construction makes
    /// into `place`'s storage: [`TBAA_DATA`] when that storage is provably an
    /// inline slot slab, or the empty string when it may not be.
    ///
    /// A bare inline-aggregate local owns its whole `alloca` /
    /// `gos_rt_aggr_alloc` slab, so every slot in it is payload memory; a
    /// projected destination is classified by [`Self::place_payload_tbaa`].
    pub(crate) fn aggregate_dest_tbaa(&self, place: &Place) -> &'static str {
        if !place.projection.is_empty() {
            return self.place_payload_tbaa(place);
        }
        let ty = self.body.local_ty(place.local);
        if is_aggregate(self.tcx, ty) && slot_count(self.tcx, ty).is_some() {
            TBAA_DATA
        } else {
            ""
        }
    }

    pub(crate) fn place_is_packed_byte_element(&self, place: &Place) -> bool {
        let mut ty = self.body.local_ty(place.local);
        for projection in &place.projection {
            match projection {
                Projection::Index(_) => {
                    if packed_byte_array_len(self.tcx, ty).is_some() {
                        return true;
                    }
                    ty = match self.tcx.kind(ty) {
                        Some(TyKind::Array { elem, .. }) => *elem,
                        _ => ty,
                    };
                }
                _ => {}
            }
        }
        false
    }
}
