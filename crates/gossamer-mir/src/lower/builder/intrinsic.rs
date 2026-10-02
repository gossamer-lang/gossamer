#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::if_not_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::redundant_else)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::single_match)]
#![allow(clippy::useless_conversion)]

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_hir::{
    HirAdtKind, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirLiteral, HirMatchArm, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind, HirUnaryOp,
};
use gossamer_lex::Span;
use gossamer_types::{Ty, TyCtxt};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

use super::Builder;

/// The shim implementing `combinator` for one element crossing, from the ABI
/// registry rather than from a name written at the call site.
///
/// The registry records the class each shim reads its sequence buffer as,
/// which its C-ABI signature cannot express: a word-stride shim and its
/// by-address twin are both `(Ptr, Ptr) -> Ptr`. Deriving the symbol from the
/// class present makes a missing crossing a named gap in the shim family
/// instead of a call that reads the buffer at the wrong stride, or an
/// undefined symbol discovered at link time.
fn iter_combinator_helper(
    combinator: &str,
    elem: ElemAbi,
    result: Option<ElemAbi>,
) -> &'static str {
    gossamer_abi::combinator_symbol(combinator, elem, result).unwrap_or_else(|| {
        panic!(
            "the ABI registry declares no shim for iter::{combinator} over \
             {elem:?} elements producing {result:?}"
        )
    })
}

/// Where a fold's accumulator starts.
enum FoldSeed {
    /// A seed value the caller supplied (`fold`).
    Init(Local),
    /// The source's first element (`reduce`), answered as an `Option` of the
    /// named carrier type that is `None` for an empty source.
    FirstElement(Ty),
}

/// How a fold pulls its elements.
enum FoldPull {
    /// By index from a sequence.
    Vec {
        vec: Local,
        len: Local,
        counter: Local,
    },
    /// One pull per turn from lazy iterator state.
    Lazy {
        state: Local,
        next_symbol: &'static str,
    },
}

/// What an aggregate `unwrap_or` family answers when the carrier holds no
/// payload: a value already evaluated, or a closure called for it.
#[derive(Clone, Copy)]
pub(crate) enum CarrierFallback {
    Value(Local),
    Call(Local),
}

impl<'a> Builder<'a> {
    /// True when a value of `ty` is stored inline in its container slot
    /// (the flat struct / tuple / array layout the compiled tiers use), so
    /// the slot's ADDRESS is the value. False for the tagged-pointer and
    /// opaque-handle shapes, where the slot HOLDS the value as one word.
    /// Mirrors the compiled-tier `slot_count(ty).is_some()` classification.
    pub(crate) fn is_inline_aggregate(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { def, substs } => {
                // `Result` / `Option` (sentinels `u32::MAX` / `u32::MAX - 1`)
                // and inline-able user enums are the 2-word by-value shape.
                if def.local == u32::MAX || def.local == u32::MAX - 1 {
                    return true;
                }
                if self.tcx.is_inline_enum_ty(ty) {
                    return true;
                }
                // `http::Response` (`u32::MAX - 5`) is a `repr(Rust)` runtime
                // struct reached through the handle word, not an inline blob.
                def.local != u32::MAX - 5 && self.tcx.adt_field_tys(*def, substs).is_some()
            }
            _ => false,
        }
    }

    /// Lowers Rust-style `fill` for fixed arrays, slices, and Vec values as a
    /// typed element-store loop. Using MIR places preserves aggregate and RC
    /// element semantics instead of copying an erased machine word.
    /// The type the place names, following its projections.
    ///
    /// A receiver written as a field or an element is rooted in a local whose
    /// own type is the aggregate it sits in, so the local's type alone answers
    /// for the wrong value: `o.items.resize(..)` is rooted in `o`. The
    /// projections are what say which value the place is. A step that cannot be
    /// followed answers with the type reached so far, which leaves the caller
    /// exactly as unable to recognise the receiver as it was before.
    pub(crate) fn place_ty(&self, place: &Place) -> gossamer_types::Ty {
        use gossamer_types::TyKind;
        let mut ty = self.locals[place.local.0 as usize].ty;
        for step in &place.projection {
            let mut base = ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(base) {
                base = *inner;
            }
            ty = match step {
                crate::ir::Projection::Deref => match self.tcx.kind_of(ty) {
                    TyKind::Ref { inner, .. } => *inner,
                    _ => return ty,
                },
                crate::ir::Projection::Field(index) => match self.tcx.kind_of(base) {
                    TyKind::Tuple(elems) => match elems.get(*index as usize) {
                        Some(field) => *field,
                        None => return ty,
                    },
                    TyKind::Adt { def, .. } => {
                        match self
                            .tcx
                            .struct_field_tys(*def)
                            .and_then(|fields| fields.get(*index as usize))
                        {
                            Some(field) => *field,
                            None => return ty,
                        }
                    }
                    _ => return ty,
                },
                crate::ir::Projection::Index(_) => match self.tcx.kind_of(base) {
                    TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => *elem,
                    _ => return ty,
                },
                crate::ir::Projection::Downcast(_) | crate::ir::Projection::Discriminant => {
                    return ty;
                }
            };
        }
        ty
    }

    pub(crate) fn try_lower_sequence_fill(
        &mut self,
        receiver: &HirExpr,
        value: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, Mutbl, TyKind};

        let recv_place = self.lower_place_expr(receiver)?;
        let recv_ty = self.place_ty(&recv_place);
        let recv_kind = match self.tcx.kind_of(recv_ty) {
            TyKind::Ref { inner, .. } => self.tcx.kind_of(*inner).clone(),
            kind => kind.clone(),
        };
        let elem = match &recv_kind {
            TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => *elem,
            _ => return None,
        };
        let uses_vec_storage = matches!(recv_kind, TyKind::Slice(_) | TyKind::Vec(_));
        let value_local = self.lower_expr(value)?;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let len_local = self.fresh(i64_ty);
        match recv_kind {
            TyKind::Array { len, .. } => self.emit_assign(
                Place::local(len_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(
                    i128::try_from(len.to_usize()).unwrap_or(0),
                ))),
                span,
            ),
            TyKind::Vec(_) | TyKind::Slice(_) => {
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
                    args: vec![Operand::Copy(recv_place.clone())],
                    destination: Place::local(len_local),
                    target: Some(next),
                });
                self.set_current(next);
            }
            _ => unreachable!(),
        }

        let index = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let condition = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(condition),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(condition)),
            arms: vec![(0, exit)],
            default: body,
        });

        self.set_current(body);
        let element = if uses_vec_storage {
            // Vec and slice locals hold a GosVec header pointer, not an inline
            // element buffer. Resolve the actual element address before the
            // store. A flat Index projection would overwrite header fields,
            // including `elem_bytes`, and corrupt every later access.
            let ref_ty = self.tcx.intern(TyKind::Ref {
                mutability: Mutbl::Mut,
                inner: elem,
            });
            let ptr = self.fresh(ref_ty);
            let after_ptr = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr".to_string())),
                args: vec![
                    Operand::Copy(recv_place),
                    Operand::Copy(Place::local(index)),
                ],
                destination: Place::local(ptr),
                target: Some(after_ptr),
            });
            self.set_current(after_ptr);
            let mut place = Place::local(ptr);
            place.projection.push(crate::ir::Projection::Deref);
            place
        } else {
            let mut place = recv_place;
            place.projection.push(crate::ir::Projection::Index(index));
            place
        };
        self.emit_assign(
            element,
            Rvalue::Use(Operand::Copy(Place::local(value_local))),
            span,
        );
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let next_index = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(next_index),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Copy(Place::local(next_index))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });
        self.set_current(exit);
        Some(self.lower_unit(span))
    }

    /// `xs.copy_within(src, dest, len)`, `xs.copy_from_slice(src)`, and
    /// `xs.binary_search(needle)` on a Vec receiver.
    pub(crate) fn try_lower_vec_bulk_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};

        let recv_place = self.lower_place_expr(receiver)?;
        let recv_ty = self.place_ty(&recv_place);
        let mut peeled = recv_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        let elem = match self.tcx.kind_of(peeled) {
            TyKind::Vec(elem) | TyKind::Slice(elem) => *elem,
            _ => return None,
        };
        let unit_ty = self.tcx.unit();
        let (symbol, ret_ty) = match (method.name.as_str(), args.len()) {
            ("copy_within", 3) => ("gos_rt_vec_copy_within", unit_ty),
            ("copy_from_slice", 1) => ("gos_rt_vec_copy_from_slice", unit_ty),
            ("binary_search", 1) => {
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                let ret = self.result_adt_of(i64_ty, i64_ty);
                let elem = self.peel_ref_ty(elem);
                let symbol = match self.tcx.kind_of(elem) {
                    // A `u64` / `usize` word does not order as the signed word
                    // the i64 search compares, so it searches through its tag.
                    TyKind::Int(IntTy::U64 | IntTy::Usize) => {
                        return self.lower_vec_binary_search_ordered(
                            recv_place, elem, &args[0], ret, span,
                        );
                    }
                    TyKind::Int(_) | TyKind::Char => "gos_rt_vec_binary_search_i64",
                    TyKind::Float(_) => "gos_rt_vec_binary_search_f64",
                    TyKind::String => "gos_rt_vec_binary_search_str",
                    _ => {
                        return self.lower_vec_binary_search_ordered(
                            recv_place, elem, &args[0], ret, span,
                        );
                    }
                };
                (symbol, ret)
            }
            _ => return None,
        };
        let mut call_args = vec![Operand::Copy(recv_place)];
        for arg in args {
            let local = self.lower_expr(arg)?;
            call_args.push(Operand::Copy(Place::local(local)));
        }
        let dest = self.fresh(ret_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(symbol.to_string())),
            args: call_args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// `xs.binary_search(needle)` over elements whose slot word is not their
    /// order: the search compares through the element's tag stream, the same
    /// one `xs.sort()` put the sequence in order with.
    fn lower_vec_binary_search_ordered(
        &mut self,
        recv_place: Place,
        elem: Ty,
        needle: &HirExpr,
        ret_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::IntTy;

        let (count, tags) = self.tuple_element_stream(elem)?;
        let needle_local = self.lower_expr(needle)?;
        let slots = self.ordered_value_slots(needle_local, elem, span);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let count_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(count_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(
                i128::try_from(count).unwrap_or(0),
            ))),
            span,
        );
        let string_ty = self.tcx.string_ty();
        let tags_local = self.fresh(string_ty);
        self.emit_assign(
            Place::local(tags_local),
            Rvalue::Use(Operand::Const(ConstValue::Str(
                tags.iter().map(|&b| b as char).collect(),
            ))),
            span,
        );
        let dest = self.fresh(ret_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_binary_search_aggr".to_string())),
            args: vec![
                Operand::Copy(recv_place),
                Operand::Copy(Place::local(slots)),
                Operand::Copy(Place::local(count_local)),
                Operand::Copy(Place::local(tags_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// A local whose storage is `value` laid out as one element of a sequence
    /// of `elem`, so a runtime comparison can address its slots.
    ///
    /// A tuple or struct local is already that run of slots. Any other value
    /// is a word or a carrier held by value, so it is placed in a one-field
    /// tuple, whose storage is exactly the element's slots.
    pub(crate) fn ordered_value_slots(&mut self, value: Local, elem: Ty, span: Span) -> Local {
        use gossamer_types::TyKind;

        if self
            .inline_field_tys(elem)
            .is_some_and(|fields| !fields.is_empty())
        {
            return value;
        }
        let tuple_ty = self.tcx.intern(TyKind::Tuple(vec![elem]));
        let slots = self.fresh(tuple_ty);
        self.emit_assign(
            Place::local(slots),
            Rvalue::Aggregate {
                kind: crate::ir::AggregateKind::Tuple,
                operands: vec![Operand::Copy(Place::local(value))],
            },
            span,
        );
        slots
    }

    /// `xs.resize(new_len, value)` - shrink by truncation, or grow by
    /// appending copies of `value`.
    ///
    /// Lowered here rather than in a runtime helper because appending
    /// reaches `gos_rt_vec_push`, whose element argument each backend
    /// already spills for the element's own width; a helper taking the
    /// value would need that same treatment repeated per backend.
    pub(crate) fn try_lower_vec_resize(
        &mut self,
        receiver: &HirExpr,
        new_len: &HirExpr,
        value: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};

        let recv_place = self.lower_place_expr(receiver)?;
        let recv_ty = self.place_ty(&recv_place);
        let mut peeled = recv_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        if !matches!(self.tcx.kind_of(peeled), TyKind::Vec(_)) {
            return None;
        }
        let target_len = self.lower_expr(new_len)?;
        let value_local = self.lower_expr(value)?;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit_ty = self.tcx.unit();

        let len_local = self.fresh(i64_ty);
        let after_len = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(recv_place.clone())],
            destination: Place::local(len_local),
            target: Some(after_len),
        });
        self.set_current(after_len);

        // Truncation is a no-op when the target is at or above the
        // current length, so the shrink case needs no branch of its own.
        let trunc_dest = self.fresh(unit_ty);
        let after_trunc = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_truncate".to_string())),
            args: vec![
                Operand::Copy(recv_place.clone()),
                Operand::Copy(Place::local(target_len)),
            ],
            destination: Place::local(trunc_dest),
            target: Some(after_trunc),
        });
        self.set_current(after_trunc);

        let index = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Copy(Place::local(len_local))),
            span,
        );
        let header = self.new_block(span);
        let body = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let condition = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(condition),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(target_len)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(condition)),
            arms: vec![(0, exit)],
            default: body,
        });

        self.set_current(body);
        let push_dest = self.fresh(unit_ty);
        let after_push = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_push".to_string())),
            args: vec![
                Operand::Copy(recv_place.clone()),
                Operand::Copy(Place::local(value_local)),
            ],
            destination: Place::local(push_dest),
            target: Some(after_push),
        });
        self.set_current(after_push);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let next_index = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(next_index),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Copy(Place::local(next_index))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });
        self.set_current(exit);
        Some(self.lower_unit(span))
    }

    /// `xs.sort()` where the element type is a tuple. A tuple element
    /// spans several slots, so the scalar slot-wise sort would reorder
    /// slots rather than the tuples they belong to; this routes to the
    /// structural comparator instead, matching the VM.
    pub(crate) fn try_lower_tuple_sort(&mut self, receiver: &HirExpr, span: Span) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};

        let recv_place = self.lower_place_expr(receiver)?;
        let recv_ty = self.place_ty(&recv_place);
        let mut peeled = recv_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        // A fixed array is a flat element buffer with no header, so it
        // takes its length and stride as arguments; a Vec / slice is a
        // `GosVec` whose header carries both.
        let (elem, fixed_len) = match self.tcx.kind_of(peeled) {
            TyKind::Vec(elem) | TyKind::Slice(elem) => (*elem, None),
            TyKind::Array { elem, len } => (*elem, Some(len.to_usize())),
            _ => return None,
        };
        let (count, tags) = self.tuple_element_stream(elem)?;
        // Tag bytes are all below 0x80, so the stream round-trips through
        // the `ConstValue::Str` rodata pool one byte per tag.
        let tag_text: String = tags.iter().map(|&b| b as char).collect();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let int_arg = |builder: &mut Self, value: i128| {
            let local = builder.fresh(i64_ty);
            builder.emit_assign(
                Place::local(local),
                Rvalue::Use(Operand::Const(ConstValue::Int(value))),
                span,
            );
            Operand::Copy(Place::local(local))
        };
        let mut args = vec![Operand::Copy(Place::local(recv_place.local))];
        let helper = if let Some(len) = fixed_len {
            args.push(int_arg(self, i128::try_from(len).unwrap_or(0)));
            args.push(int_arg(self, i128::from(self.type_slot_bytes(elem).max(1))));
            "gos_rt_arr_sort_tuple"
        } else {
            "gos_rt_vec_sort_tuple"
        };
        args.push(int_arg(self, i128::try_from(count).unwrap_or(0)));
        let string_ty = self.tcx.string_ty();
        let tags_local = self.fresh(string_ty);
        self.emit_assign(
            Place::local(tags_local),
            Rvalue::Use(Operand::Const(ConstValue::Str(tag_text))),
            span,
        );
        args.push(Operand::Copy(Place::local(tags_local)));
        let unit_ty = self.tcx.unit();
        let dest = self.fresh(unit_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(helper.to_string())),
            args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(self.lower_unit(span))
    }

    pub(crate) fn try_lower_fixed_array_ordering(
        &mut self,
        receiver: &HirExpr,
        method: &str,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};

        let recv_place = self.lower_place_expr(receiver)?;
        let recv_ty = self.place_ty(&recv_place);
        let TyKind::Array { elem, len } = self.tcx.kind_of(recv_ty).clone() else {
            return None;
        };
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let len_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(len_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(
                i128::try_from(len.to_usize()).unwrap_or(0),
            ))),
            span,
        );
        let (helper, mut args) = match method {
            "sort" => {
                let helper = match self.tcx.kind_of(elem) {
                    TyKind::String => "gos_rt_arr_sort_str",
                    TyKind::Float(_) => "gos_rt_arr_sort_f64",
                    _ => "gos_rt_arr_sort_i64",
                };
                (
                    helper,
                    vec![
                        Operand::Copy(Place::local(recv_place.local)),
                        Operand::Copy(Place::local(len_local)),
                    ],
                )
            }
            "reverse" => {
                let bytes_local = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(bytes_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                        self.type_slot_bytes(elem).max(1),
                    )))),
                    span,
                );
                (
                    "gos_rt_arr_reverse",
                    vec![
                        Operand::Copy(Place::local(recv_place.local)),
                        Operand::Copy(Place::local(len_local)),
                        Operand::Copy(Place::local(bytes_local)),
                    ],
                )
            }
            _ => return None,
        };
        let unit_ty = self.tcx.unit();
        let dest = self.fresh(unit_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(helper.to_string())),
            args: std::mem::take(&mut args),
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(self.lower_unit(span))
    }

    pub(crate) fn try_lower_sort_by(
        &mut self,
        receiver: &HirExpr,
        closure_arg: &HirExpr,
        _ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};
        let recv_place = self.lower_place_expr(receiver)?;
        let recv_ty = self.place_ty(&recv_place);
        let recv_kind = match self.tcx.kind_of(recv_ty) {
            TyKind::Ref { inner, .. } => self.tcx.kind_of(*inner).clone(),
            other => other.clone(),
        };
        let elem_ty_concrete = match &recv_kind {
            TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => *elem,
            _ => return None,
        };
        let elem_kind = self.tcx.kind_of(elem_ty_concrete).clone();
        // Single-slot scalar elements (i64, String pointers, bools)
        // sort through the by-value i64 helpers; multi-slot
        // aggregates (Tuple / Adt) sort through the byte-stride
        // helpers that hand the comparator pointers to each
        // element. Arrays-of-T as elements aren't sortable -
        // their content fan-out makes the comparator ABI
        // ambiguous; bail out.
        // The opaque heap-blob / handle stdlib structs (`def.local` in the
        // `u32::MAX - 16 ..= u32::MAX - 2`
        // sentinel range) are single pointer-valued slots, so the
        // comparator receives the value directly like any scalar - the
        // aggregate helper would hand it a pointer to the slot (a pointer
        // to the pointer) and the comparison would read the wrong bytes.
        // A tagged-pointer user enum is the same single-slot shape: the slot
        // holds the handle, so the comparator takes it by word.
        let elem_is_opaque_handle = matches!(
            elem_kind,
            TyKind::Adt { def, .. } if (u32::MAX - 16..=u32::MAX - 2).contains(&def.local)
        ) || matches!(elem_kind, TyKind::Adt { .. })
            && !self.is_inline_aggregate(elem_ty_concrete);
        // Every single-slot scalar sorts by word, whatever its stride: the
        // comparator's own parameter types decide how the body reads the
        // bits, and the runtime moves elements through the header's
        // `elem_bytes`. A float is the one class that needs its own helper,
        // because its comparator takes SSE registers.
        let elem_is_float = matches!(elem_kind, TyKind::Float(_));
        let elem_is_scalar = matches!(
            elem_kind,
            TyKind::Int(_) | TyKind::String | TyKind::Bool | TyKind::Char | TyKind::Float(_)
        ) || elem_is_opaque_handle;
        let elem_is_aggregate =
            !elem_is_opaque_handle && matches!(elem_kind, TyKind::Tuple(_) | TyKind::Adt { .. });
        if !elem_is_scalar && !elem_is_aggregate {
            return None;
        }
        let raw_closure_local = self.lower_expr(closure_arg)?;
        // For scalar elements the closure receives values
        // directly. For aggregate elements the cranelift ABI
        // already passes aggregates as pointers (see
        // `cl_type_of`), so declaring the comparator inputs as
        // the concrete element type produces the right shape:
        // the runtime hands two element pointers, the closure
        // body's field-access projections walk off those
        // pointers correctly, no auto-deref needed.
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let elem_ty = if elem_is_scalar || elem_is_aggregate {
            elem_ty_concrete
        } else {
            i64_ty
        };
        let cmp_sig = gossamer_types::FnSig {
            inputs: vec![elem_ty, elem_ty],
            output: i64_ty,
        };
        let cmp_trait_ty = self.tcx.intern(TyKind::FnTrait(cmp_sig));
        let closure_local =
            self.coerce_to_fn_trait_if_needed(raw_closure_local, cmp_trait_ty, span);
        let unit_ty = self.tcx.unit();
        let vec_helper = match (elem_is_aggregate, elem_is_float) {
            (true, _) => "gos_rt_vec_sort_by_aggr",
            (_, true) => "gos_rt_vec_sort_by_f64",
            _ => "gos_rt_vec_sort_by_i64",
        };
        let arr_helper = match (elem_is_aggregate, elem_is_float) {
            (true, _) => "gos_rt_arr_sort_by_aggr",
            (_, true) => "gos_rt_arr_sort_by_f64",
            _ => "gos_rt_arr_sort_by_i64",
        };
        match &recv_kind {
            TyKind::Vec(_) | TyKind::Slice(_) => {
                let dest = self.fresh(unit_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(vec_helper.to_string())),
                    args: vec![
                        Operand::Copy(Place::local(recv_place.local)),
                        Operand::Copy(Place::local(closure_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(self.lower_unit(span))
            }
            TyKind::Array { len, .. } => {
                let len_local = self.fresh(i64_ty);
                let len_i128 = i128::try_from(len.to_usize()).unwrap_or(0);
                self.emit_assign(
                    Place::local(len_local),
                    Rvalue::Use(Operand::Const(ConstValue::Int(len_i128))),
                    span,
                );
                let mut args = vec![
                    Operand::Copy(Place::local(recv_place.local)),
                    Operand::Copy(Place::local(len_local)),
                ];
                if elem_is_aggregate {
                    // Stride helper needs the element width in
                    // bytes so it can advance the cursor between
                    // elements. The bytes value uses the same
                    // `type_slot_bytes` rule as Vec layouts.
                    let bytes_local = self.fresh(i64_ty);
                    let elem_bytes = i128::from(self.type_slot_bytes(elem_ty_concrete).max(8));
                    self.emit_assign(
                        Place::local(bytes_local),
                        Rvalue::Use(Operand::Const(ConstValue::Int(elem_bytes))),
                        span,
                    );
                    args.push(Operand::Copy(Place::local(bytes_local)));
                }
                args.push(Operand::Copy(Place::local(closure_local)));
                let dest = self.fresh(unit_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(arr_helper.to_string())),
                    args,
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(self.lower_unit(span))
            }
            _ => None,
        }
    }

    /// `fs::walk_dir(root, visit)` / `path::walk(root, visit)`: recursively
    /// visits every descendant, invoking `visit` for each entry. The
    /// visitor closure is coerced to an env-pointer value (the same shape
    /// `sort_by`'s comparator uses) and handed to the runtime walker, which
    /// calls back into it per entry and stops as soon as `visit` returns
    /// `Err`.
    pub(crate) fn try_lower_walk_dir(&mut self, args: &[HirExpr], span: Span) -> Option<Local> {
        use gossamer_types::TyKind;
        let [root_arg, visit_arg] = args else {
            return None;
        };
        let root_local = self.lower_expr(root_arg)?;
        let raw_visit_local = self.lower_expr(visit_arg)?;
        let entry_ty = self.tuple_dir_entry_ty();
        let visit_ret_ty = self.result_unit_error_adt_ty();
        let visit_sig = gossamer_types::FnSig {
            inputs: vec![entry_ty],
            output: visit_ret_ty,
        };
        let visit_trait_ty = self.tcx.intern(TyKind::FnTrait(visit_sig));
        let visit_local = self.coerce_to_fn_trait_if_needed(raw_visit_local, visit_trait_ty, span);
        let result_ty = self.result_unit_error_adt_ty();
        let dest = self.fresh(result_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_fs_walk_dir_raw".to_string())),
            args: vec![
                Operand::Copy(Place::local(root_local)),
                Operand::Copy(Place::local(visit_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// `__gos_par_run(len, mode, leaf)`: runs `leaf(lo, hi)` over the leaves
    /// of `[0, len)` on the pool and answers their `Vec`s concatenated in
    /// index order. The call's type is the leaf's own return type.
    pub(crate) fn try_lower_par_run(&mut self, args: &[HirExpr], span: Span) -> Option<Local> {
        use gossamer_types::TyKind;
        let [len_arg, mode_arg, leaf_arg] = args else {
            return None;
        };
        let (TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) = self.tcx.kind_of(leaf_arg.ty).clone()
        else {
            return None;
        };
        let result_ty = sig.output;
        let len_local = self.lower_expr(len_arg)?;
        let mode_local = self.lower_expr(mode_arg)?;
        let raw_leaf_local = self.lower_expr(leaf_arg)?;
        let leaf_trait_ty = self.tcx.intern(TyKind::FnTrait(sig));
        let leaf_local = self.coerce_to_fn_trait_if_needed(raw_leaf_local, leaf_trait_ty, span);
        let dest = self.fresh(result_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_par_run".to_string())),
            args: vec![
                Operand::Copy(Place::local(leaf_local)),
                Operand::Copy(Place::local(len_local)),
                Operand::Copy(Place::local(mode_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// `__gos_par_chunks(window, size, f)`: hands each `size`-element chunk
    /// of the window to `f` with its index, on the pool.
    pub(crate) fn try_lower_par_chunks(&mut self, args: &[HirExpr], span: Span) -> Option<Local> {
        use gossamer_types::TyKind;
        let [window_arg, size_arg, callback_arg] = args else {
            return None;
        };
        // The callback is handed a chunk's index and its window and answers
        // nothing, whatever form the argument was written in.
        let unit = self.tcx.unit();
        let sig = gossamer_types::FnSig {
            inputs: vec![self.tcx.int_ty(gossamer_types::IntTy::I64), window_arg.ty],
            output: unit,
        };
        let window_local = self.lower_expr(window_arg)?;
        let size_local = self.lower_expr(size_arg)?;
        let raw_callback = self.lower_expr(callback_arg)?;
        let callback_ty = self.tcx.intern(TyKind::FnTrait(sig));
        let callback_local = self.coerce_to_fn_trait_if_needed(raw_callback, callback_ty, span);
        let dest = self.fresh(unit);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_par_chunks".to_string())),
            args: vec![
                Operand::Copy(Place::local(callback_local)),
                Operand::Copy(Place::local(window_local)),
                Operand::Copy(Place::local(size_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    pub(crate) fn try_lower_array_swap(
        &mut self,
        receiver: &HirExpr,
        i_expr: &HirExpr,
        j_expr: &HirExpr,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        // Build a Place that names the receiver as a place
        // expression. Bail out if the receiver isn't an
        // assignable l-value (a path, field, or index chain).
        let recv_place = self.lower_place_expr(receiver)?;
        let i_local = self.lower_expr(i_expr)?;
        let j_local = self.lower_expr(j_expr)?;
        let recv_kind = self.tcx.kind_of(self.place_ty(&recv_place));
        let inner_kind = match recv_kind {
            gossamer_types::TyKind::Ref { inner, .. } => self.tcx.kind_of(*inner).clone(),
            other => other.clone(),
        };
        let is_vec_or_slice = matches!(
            inner_kind,
            gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
        );
        let elem_ty = match &inner_kind {
            gossamer_types::TyKind::Array { elem, .. } => *elem,
            gossamer_types::TyKind::Slice(elem) => *elem,
            gossamer_types::TyKind::Vec(elem) => *elem,
            _ => return None,
        };
        if is_vec_or_slice {
            // Vec/Slice swap goes through a checked helper so the GosVec
            // header is not mis-treated as a flat element buffer and invalid
            // indices are returned as an error. The receiver is passed as the
            // place it is written as, so a vec reached through a field or an
            // element is the one that gets swapped.
            let swap = self.fresh(ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_swap_safe".to_string())),
                args: vec![
                    Operand::Copy(recv_place.clone()),
                    Operand::Copy(Place::local(i_local)),
                    Operand::Copy(Place::local(j_local)),
                ],
                destination: Place::local(swap),
                target: Some(next),
            });
            self.set_current(next);
            return Some(swap);
        }
        let mut at_i = recv_place.clone();
        at_i.projection.push(crate::ir::Projection::Index(i_local));
        let mut at_j = recv_place.clone();
        at_j.projection.push(crate::ir::Projection::Index(j_local));
        let temp_i = self.fresh(elem_ty);
        let temp_j = self.fresh(elem_ty);
        self.emit_assign(
            Place::local(temp_i),
            Rvalue::Use(Operand::Copy(at_i.clone())),
            span,
        );
        self.emit_assign(
            Place::local(temp_j),
            Rvalue::Use(Operand::Copy(at_j.clone())),
            span,
        );
        self.emit_assign(at_i, Rvalue::Use(Operand::Copy(Place::local(temp_j))), span);
        self.emit_assign(at_j, Rvalue::Use(Operand::Copy(Place::local(temp_i))), span);
        let unit_local = self.lower_unit(span);
        Some(unit_local)
    }

    /// `m.insert/get/contains` on a `HashMap` keyed by a flat struct / tuple,
    /// routed to the content-hashing `skey` runtime so two equal-but-distinct
    /// allocations key the same slot (matching the VM). Returns `None` for any
    /// other key shape, leaving the normal pointer-keyed path to run.
    /// `m.insert/get/…` on a `HashMap` keyed by a user enum, routed to the
    /// `ekey` runtime so a key hashes by discriminant and payload rather than
    /// by node address - two equal-valued nodes then share a slot, matching
    /// the VM. Returns `None` for any other key shape.
    fn try_lower_enum_key_map_op(
        &mut self,
        receiver: &HirExpr,
        op: &str,
        args: &[HirExpr],
        span: Span,
        recv_ty: gossamer_types::Ty,
    ) -> Option<Local> {
        let (key_ty, val_ty) = self.hash_map_kv_tys(recv_ty)?;
        if self.struct_name_of(key_ty).is_some() {
            return None;
        }
        let desc_sym = self.ensure_enum_eq_desc(key_ty)?;
        // A multi-slot value crosses as one handle word the backend copies
        // onto the heap, as a content-keyed map's value does.
        let _ = self.ensure_aggr_struct_meta(val_ty);
        if self.is_inline_aggregate_ty(val_ty) {
            let _ = self.ensure_aggr_copy_meta(val_ty);
        }
        let recv_local = self.lower_expr(receiver)?;
        let key_local = self.lower_expr(args.first()?)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let (name, dest_ty, extra) = match op {
            "insert" if args.len() == 2 => {
                let val_local = self.lower_expr(&args[1])?;
                (
                    "gos_rt_map_insert_ekey_opt",
                    self.option_payload_adt_ty(val_ty),
                    Some(Operand::Copy(Place::local(val_local))),
                )
            }
            "get" if args.len() == 1 => (
                "gos_rt_map_get_ekey_opt",
                self.option_payload_adt_ty(val_ty),
                None,
            ),
            "pop" | "remove" if args.len() == 1 => (
                "gos_rt_map_pop_ekey",
                self.option_payload_adt_ty(val_ty),
                None,
            ),
            "contains_key" | "contains" if args.len() == 1 => {
                ("gos_rt_map_contains_ekey", self.tcx.bool_ty(), None)
            }
            "__range" if args.len() == 3 => {
                let hi_local = self.lower_expr(&args[1])?;
                (
                    "gos_rt_map_range_ekey",
                    recv_ty,
                    Some(Operand::Copy(Place::local(hi_local))),
                )
            }
            "get_or" if args.len() == 2 => {
                let default_local = self.lower_expr(&args[1])?;
                (
                    "gos_rt_map_get_or_ekey",
                    val_ty,
                    Some(Operand::Copy(Place::local(default_local))),
                )
            }
            "or_insert" if args.len() == 2 => {
                let default_local = self.lower_expr(&args[1])?;
                (
                    "gos_rt_map_or_insert_ekey",
                    val_ty,
                    Some(Operand::Copy(Place::local(default_local))),
                )
            }
            "inc" if args.len() <= 2 => {
                let by = match args.get(1) {
                    Some(expr) => Operand::Copy(Place::local(self.lower_expr(expr)?)),
                    None => Operand::Const(ConstValue::Int(1)),
                };
                ("gos_rt_map_inc_ekey", i64_ty, Some(by))
            }
            _ => return None,
        };
        let mut call_args = vec![
            Operand::Copy(Place::local(recv_local)),
            Operand::Copy(Place::local(key_local)),
            Operand::Const(ConstValue::Str(desc_sym)),
        ];
        call_args.extend(extra);
        if op == "__range" {
            let mode = self.lower_expr(args.get(2)?)?;
            call_args.push(Operand::Copy(Place::local(mode)));
        }
        let dest = self.fresh(dest_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name.to_string())),
            args: call_args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    pub(crate) fn try_lower_struct_key_map_op(
        &mut self,
        receiver: &HirExpr,
        op: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let recv_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
        let (key_ty, val_ty) = self.hash_map_kv_tys(recv_ty)?;
        // An enum key varies its layout per variant, so it content-hashes
        // through its structural descriptor rather than a flat slot list.
        if let Some(local) = self.try_lower_enum_key_map_op(receiver, op, args, span, recv_ty) {
            return Some(local);
        }
        // Only aggregate keys (struct / tuple / array) content-hash; bare
        // scalar and `String` keys keep their dedicated `_i64` / `_str` fast
        // paths.
        if !self.is_aggregate_key(key_ty) {
            return None;
        }
        let descriptor = self.key_descriptor(key_ty)?;
        // A multi-slot value crosses as one handle word, so the backend copies
        // its slots onto the heap. The copy meta is what tags the map as
        // holding blob values - the entry then takes the share the inserting
        // frame gives back - and the structural meta names the heap children
        // that copy owns.
        let _ = self.ensure_aggr_struct_meta(val_ty);
        if self.is_inline_aggregate_ty(val_ty) {
            let _ = self.ensure_aggr_copy_meta(val_ty);
        }
        let recv_local = self.lower_expr(receiver)?;
        let key_local = self.lower_expr(args.first()?)?;
        let desc_op = Operand::Const(ConstValue::Str(descriptor));
        let (name, dest_ty, val_arg) = match op {
            "insert" if args.len() == 2 => {
                let val_local = self.lower_expr(&args[1])?;
                (
                    "gos_rt_map_insert_skey_opt",
                    self.option_payload_adt_ty(val_ty),
                    Some(Operand::Copy(Place::local(val_local))),
                )
            }
            "get" if args.len() == 1 => (
                "gos_rt_map_get_skey_opt",
                self.option_payload_adt_ty(val_ty),
                None,
            ),
            // `remove` and `pop` are the same contract on a map - take the
            // slot out and hand back what it held.
            "pop" | "remove" if args.len() == 1 => (
                "gos_rt_map_pop_skey",
                self.option_payload_adt_ty(val_ty),
                None,
            ),
            "contains_key" | "contains" if args.len() == 1 => {
                ("gos_rt_map_contains_skey", self.tcx.bool_ty(), None)
            }
            "__range" if args.len() == 3 => {
                let hi_local = self.lower_expr(&args[1])?;
                (
                    "gos_rt_map_range_skey",
                    recv_ty,
                    Some(Operand::Copy(Place::local(hi_local))),
                )
            }
            "get_or" if args.len() == 2 => {
                let default_local = self.lower_expr(&args[1])?;
                (
                    "gos_rt_map_get_or_skey",
                    val_ty,
                    Some(Operand::Copy(Place::local(default_local))),
                )
            }
            "or_insert" if args.len() == 2 => {
                let default_local = self.lower_expr(&args[1])?;
                (
                    "gos_rt_map_or_insert_skey",
                    val_ty,
                    Some(Operand::Copy(Place::local(default_local))),
                )
            }
            "inc" if args.len() <= 2 => {
                let by = match args.get(1) {
                    Some(expr) => Operand::Copy(Place::local(self.lower_expr(expr)?)),
                    None => Operand::Const(ConstValue::Int(1)),
                };
                (
                    "gos_rt_map_inc_skey",
                    self.tcx.int_ty(gossamer_types::IntTy::I64),
                    Some(by),
                )
            }
            _ => return None,
        };
        let mut call_args = vec![
            Operand::Copy(Place::local(recv_local)),
            Operand::Copy(Place::local(key_local)),
            desc_op,
        ];
        call_args.extend(val_arg);
        if op == "__range" {
            let mode = self.lower_expr(args.get(2)?)?;
            call_args.push(Operand::Copy(Place::local(mode)));
        }
        let dest = self.fresh(dest_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name.to_string())),
            args: call_args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    pub(crate) fn try_lower_map_inc(
        &mut self,
        outer_recv: &HirExpr,
        outer_key: &HirExpr,
        value_expr: &HirExpr,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let HirExprKind::Binary {
            op: HirBinaryOp::Add,
            lhs,
            rhs,
        } = &value_expr.kind
        else {
            return None;
        };
        let (get_call, by_expr) = if let HirExprKind::MethodCall { name, .. } = &lhs.kind {
            if name.name.as_str() == "get_or" {
                (lhs.as_ref(), rhs.as_ref())
            } else {
                return None;
            }
        } else if let HirExprKind::MethodCall { name, .. } = &rhs.kind {
            if name.name.as_str() == "get_or" {
                (rhs.as_ref(), lhs.as_ref())
            } else {
                return None;
            }
        } else {
            return None;
        };
        let HirExprKind::MethodCall {
            receiver: inner_recv,
            args: get_args,
            ..
        } = &get_call.kind
        else {
            return None;
        };
        if get_args.len() != 2 {
            return None;
        }
        if !exprs_match(outer_recv, inner_recv) || !exprs_match(outer_key, &get_args[0]) {
            return None;
        }
        // Peephole only handles `HashMap<i64, i64>`. The
        // `gos_rt_map_inc_i64` helper takes the key as an i64;
        // forwarding a `*const c_char` here corrupts the lookup.
        // For non-i64 receivers fall through to the general
        // get_or + insert path so the key is hashed correctly.
        let outer_recv_ty = self
            .receiver_local_from_path(outer_recv)
            .map_or(outer_recv.ty, |l| self.locals[l.0 as usize].ty);
        let key_kind = self.hash_map_key_kind(outer_recv_ty);
        let value_kind = self.hash_map_value_kind(outer_recv_ty);
        if !matches!(
            (key_kind, value_kind),
            (Some(MapKeyKind::I64), Some(MapValueKind::I64))
        ) {
            return None;
        }
        let recv_local = self.lower_expr(outer_recv)?;
        let key_local = self.lower_expr(outer_key)?;
        let by_local = self.lower_expr(by_expr)?;
        let dest = self.fresh(ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_map_inc_i64".to_string())),
            args: vec![
                Operand::Copy(Place::local(recv_local)),
                Operand::Copy(Place::local(key_local)),
                Operand::Copy(Place::local(by_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    pub(crate) fn try_lower_result_map_with_eager_recv(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        closure_arg: &HirExpr,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let recv_local = self.lower_expr(receiver)?;
        let recv_ty = self.locals[recv_local.0 as usize].ty;
        if !matches!(self.tcx.kind_of(recv_ty), TyKind::Adt { .. })
            || !self.is_result_or_option_adt(recv_ty)
        {
            return None;
        }
        let closure_local = self.lower_expr(closure_arg)?;
        // Wrap a bare `__closure_N` fn-name local into a 16-byte
        // env blob `[fn_addr, _]` so the helper's first-word load
        // resolves to the lifted body. Mirrors the wrap that the
        // generic call dispatch performs.
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let env_local = if let Some(fn_name) = self.local_fn_name.get(&closure_local).cloned() {
            let size_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(size_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(16))),
                span,
            );
            let env = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(env),
                Rvalue::CallIntrinsic {
                    name: "gos_alloc",
                    args: vec![Operand::Copy(Place::local(size_local))],
                },
                span,
            );
            let fn_addr = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(fn_addr),
                Rvalue::CallIntrinsic {
                    name: "gos_fn_addr",
                    args: vec![Operand::Const(ConstValue::Str(fn_name))],
                },
                span,
            );
            let zero_off = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(zero_off),
                Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                span,
            );
            let store_dest = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(store_dest),
                Rvalue::CallIntrinsic {
                    name: "gos_store",
                    args: vec![
                        Operand::Copy(Place::local(env)),
                        Operand::Copy(Place::local(zero_off)),
                        Operand::Copy(Place::local(fn_addr)),
                    ],
                },
                span,
            );
            env
        } else {
            closure_local
        };
        let helper = match method.name.as_str() {
            "map_err" => "gos_rt_result_map_err",
            "map" => "gos_rt_result_map",
            _ => return None,
        };
        // `map` answers the closure's payload, not the receiver's. Typing the
        // destination from the receiver would tell the drop pass that
        // `Option<Struct>.map(|s| 1)` owns a pointer, and releasing the
        // integer `1` as one faults. The call's own type is used when
        // inference resolved it, then the closure's return type, and only
        // then the receiver's.
        let dest_ty = if self.is_result_or_option_adt(ty) {
            self.result_repr_ty(ty)
        } else if method.name.as_str() == "map"
            && self.is_option_adt(recv_ty)
            && let Some(output) = self.callable_output_ty(closure_local)
        {
            self.option_payload_adt_ty(output)
        } else {
            recv_ty
        };
        let dest = self.fresh(dest_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(helper.to_string())),
            args: vec![
                Operand::Copy(Place::local(recv_local)),
                Operand::Copy(Place::local(env_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    pub(crate) fn try_lower_iter_call(
        &mut self,
        joined: &str,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let local = self.try_lower_iter_call_raw(joined, args, ty, span)?;
        Some(self.canonical_wide_iterator(local, ty, span))
    }
}

/// How a keyed-pairs loop reaches a map whose keys hash by value.
enum KeyedPairs {
    /// A struct or tuple key, keyed through its slot descriptor.
    Aggregate(String),
    /// A payload enum key, keyed through its equality descriptor.
    Enum(String),
}

mod combinators;
mod elements;
mod iterators;
mod lazy_sources;
