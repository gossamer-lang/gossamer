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
                let helper = if matches!(self.tcx.kind_of(elem), TyKind::String) {
                    "gos_rt_arr_sort_str"
                } else {
                    "gos_rt_arr_sort_i64"
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
        use gossamer_types::{IntTy, TyKind};
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit_ty = self.tcx.unit();
        match (joined, args.len()) {
            // Non-closure constructors / accessors.
            ("iter::collect", 1) => {
                let (v, lazy) = self.lower_iter_seq_arg_raw(&args[0])?;
                if let Some(family) = lazy {
                    let elem = match self.tcx.kind_of(self.locals[v.0 as usize].ty) {
                        TyKind::Iterator(elem) => *elem,
                        _ => i64_ty,
                    };
                    let dest_ty = self.tcx.intern(TyKind::Vec(elem));
                    let helper = match family {
                        LazyElemFamily::PairWord => "gos_rt_lazy_iter_collect_pair_i64",
                        LazyElemFamily::Aggr => "gos_rt_lazy_iter_collect_aggr",
                        _ => "gos_rt_lazy_iter_collect_i64",
                    };
                    let collected = self.emit_combinator_call(
                        helper,
                        vec![Operand::Copy(Place::local(v))],
                        dest_ty,
                        span,
                    );
                    if family == LazyElemFamily::Aggr {
                        self.tag_owned_elements(collected, elem, span);
                    }
                    return Some(collected);
                }
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_) | TyKind::Slice(_)) {
                    ty
                } else {
                    self.tcx.intern(TyKind::Vec(i64_ty))
                };
                Some(self.emit_combinator_call(
                    "gos_rt_vec_clone",
                    vec![Operand::Copy(Place::local(v))],
                    dest_ty,
                    span,
                ))
            }
            ("iter::count", 1) => {
                let (v, lazy) = self.lower_iter_seq_arg_raw(&args[0])?;
                if let Some(family) = lazy {
                    let helper = if family == LazyElemFamily::PairWord {
                        "gos_rt_lazy_iter_count_pair_i64"
                    } else {
                        "gos_rt_lazy_iter_count_i64"
                    };
                    return Some(self.emit_combinator_call(
                        helper,
                        vec![Operand::Copy(Place::local(v))],
                        i64_ty,
                        span,
                    ));
                }
                let dest = self.fresh(i64_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_iter_count".to_string())),
                    args: vec![Operand::Copy(Place::local(v))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::empty", 0) => {
                let elem_ty = match self.tcx.kind_of(ty) {
                    TyKind::Vec(e) | TyKind::Slice(e) => *e,
                    _ => i64_ty,
                };
                let elem_bytes_val = i128::from(self.elem_bytes_of(elem_ty).max(1));
                let elem_bytes = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(elem_bytes),
                    Rvalue::Use(Operand::Const(ConstValue::Int(elem_bytes_val))),
                    span,
                );
                let cap = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(cap),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_with_capacity".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(elem_bytes)),
                        Operand::Copy(Place::local(cap)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::once", 1) => {
                let v = self.lower_expr(&args[0])?;
                if let Some(family) = self.lazy_iter_ty_family(ty) {
                    let helper = format!("gos_rt_lazy_iter_once_{}", family.value_suffix());
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![Operand::Copy(Place::local(v))],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_) | TyKind::Slice(_)) {
                    ty
                } else {
                    self.tcx.intern(TyKind::Vec(i64_ty))
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_iter_repeat_i64".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(v)),
                        Operand::Const(ConstValue::Int(1)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::sum", 1) => {
                // Element-type dispatch: f64 vec → sum_f64, otherwise sum_i64.
                let (v, lazy) = self.lower_iter_seq_arg(&args[0])?;
                if let Some(family) = lazy {
                    let (helper, dest_ty) = match family {
                        LazyElemFamily::Float => (
                            "gos_rt_lazy_iter_sum_f64",
                            self.tcx.float_ty(gossamer_types::FloatTy::F64),
                        ),
                        _ => ("gos_rt_lazy_iter_sum_i64", i64_ty),
                    };
                    return Some(self.emit_combinator_call(
                        helper,
                        vec![Operand::Copy(Place::local(v))],
                        dest_ty,
                        span,
                    ));
                }
                let elem_is_f64 = self.iter_elem_abi(args[0].ty).1 == ElemAbi::Float;
                let helper = if elem_is_f64 {
                    "gos_rt_iter_sum_f64"
                } else {
                    "gos_rt_iter_sum_i64"
                };
                let dest_ty = if elem_is_f64 {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else {
                    i64_ty
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(v))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::product", 1) => {
                let (v, lazy) = self.lower_iter_seq_arg(&args[0])?;
                if let Some(family) = lazy {
                    let (helper, dest_ty) = match family {
                        LazyElemFamily::Float => (
                            "gos_rt_lazy_iter_product_f64",
                            self.tcx.float_ty(gossamer_types::FloatTy::F64),
                        ),
                        _ => ("gos_rt_lazy_iter_product_i64", i64_ty),
                    };
                    return Some(self.emit_combinator_call(
                        helper,
                        vec![Operand::Copy(Place::local(v))],
                        dest_ty,
                        span,
                    ));
                }
                let elem_is_f64 = self.iter_elem_abi(args[0].ty).1 == ElemAbi::Float;
                let helper = if elem_is_f64 {
                    "gos_rt_iter_product_f64"
                } else {
                    "gos_rt_iter_product_i64"
                };
                let dest_ty = if elem_is_f64 {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else {
                    i64_ty
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(v))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // Bare prelude `min(xs)` / `max(xs)` over a `Vec`/array return
            // `Option<T>`, exactly like `iter::min`/`iter::max`. The two-arg
            // scalar forms (`min(a, b)`) are handled separately; only the
            // single Vec/array argument reaches here.
            ("iter::min" | "min" | "math::min", 1) => {
                // An element the language orders structurally - a tuple, a
                // struct, a payload enum - is ordered through its descriptor.
                // The word path below is for elements whose slot spells their
                // own order, which is what the descriptor builder declines.
                let (wide_elem, _) = self.iter_elem_abi(args[0].ty);
                if let Some(local) =
                    self.lower_minmax_wide_elem(&args[0], wide_elem, ty, false, span)
                {
                    return Some(local);
                }
                if let Some(local) = self.lower_minmax_string_elem(&args[0], false, span) {
                    return Some(local);
                }
                if let Some(family) = self.lazy_iter_source_family_word(args[0].ty) {
                    let (iter, lazy) = self.lower_iter_seq_arg(&args[0])?;
                    if lazy.is_some() {
                        let helper =
                            format!("gos_rt_lazy_iter_min_{}", family.word_or_float_suffix());
                        return Some(self.emit_combinator_call(
                            &helper,
                            vec![Operand::Copy(Place::local(iter))],
                            ty,
                            span,
                        ));
                    }
                    return self.lower_iter_vec_opt_local(iter, "min", span);
                }
                self.lower_iter_simple_vec_opt("min", args, span)
            }
            ("iter::max" | "max" | "math::max", 1) => {
                // An element the language orders structurally - a tuple, a
                // struct, a payload enum - is ordered through its descriptor.
                // The word path below is for elements whose slot spells their
                // own order, which is what the descriptor builder declines.
                let (wide_elem, _) = self.iter_elem_abi(args[0].ty);
                if let Some(local) =
                    self.lower_minmax_wide_elem(&args[0], wide_elem, ty, true, span)
                {
                    return Some(local);
                }
                if let Some(local) = self.lower_minmax_string_elem(&args[0], true, span) {
                    return Some(local);
                }
                if let Some(family) = self.lazy_iter_source_family_word(args[0].ty) {
                    let (iter, lazy) = self.lower_iter_seq_arg(&args[0])?;
                    if lazy.is_some() {
                        let helper =
                            format!("gos_rt_lazy_iter_max_{}", family.word_or_float_suffix());
                        return Some(self.emit_combinator_call(
                            &helper,
                            vec![Operand::Copy(Place::local(iter))],
                            ty,
                            span,
                        ));
                    }
                    return self.lower_iter_vec_opt_local(iter, "max", span);
                }
                self.lower_iter_simple_vec_opt("max", args, span)
            }
            ("iter::range", 2) => {
                let a = self.lower_expr(&args[0])?;
                let b = self.lower_expr(&args[1])?;
                if matches!(self.tcx.kind_of(ty), TyKind::Iterator(e) if self.lazy_iter_carries_elem(*e))
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_range_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let vec_i64 = self.tcx.intern(TyKind::Vec(i64_ty));
                let dest = self.fresh(vec_i64);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_iter_range".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(a)),
                        Operand::Copy(Place::local(b)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::range_inclusive", 2) => {
                let a = self.lower_expr(&args[0])?;
                let b = self.lower_expr(&args[1])?;
                if matches!(self.tcx.kind_of(ty), TyKind::Iterator(e) if self.lazy_iter_carries_elem(*e))
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_range_inclusive_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(
                        "gos_rt_iter_range_inclusive".to_string(),
                    )),
                    args: vec![
                        Operand::Copy(Place::local(a)),
                        Operand::Copy(Place::local(b)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::repeat", 2) => {
                let v = self.lower_expr(&args[0])?;
                let n = self.lower_expr(&args[1])?;
                if let Some(family) = self.lazy_iter_ty_family(ty) {
                    let helper = format!("gos_rt_lazy_iter_repeat_{}", family.value_suffix());
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![
                            Operand::Copy(Place::local(v)),
                            Operand::Copy(Place::local(n)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let vec_i64 = self.tcx.intern(TyKind::Vec(i64_ty));
                let dest = self.fresh(vec_i64);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_iter_repeat_i64".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(v)),
                        Operand::Copy(Place::local(n)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("iter::take", 2) => {
                let n = self.lower_expr(&args[0])?;
                if self.lazy_iter_result_ty(ty)
                    && let Some(iter) = self.lower_lazy_iter_source_aggr(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_take_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(n)),
                            Operand::Copy(Place::local(iter)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    self.propagate_aggr_state(iter, dest);
                    return Some(dest);
                }
                let v = self.lower_iter_vec_arg(&args[1])?;
                let dest_ty = self.iter_result_vec_ty(ty, v);
                Some(self.emit_iter_combinator_call(
                    "take",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(n)),
                        Operand::Copy(Place::local(v)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::step_by", 2) => {
                let step = self.lower_expr(&args[0])?;
                if self.lazy_iter_result_ty(ty)
                    && let Some(iter) = self.lower_lazy_iter_source_aggr(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_step_by_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(step)),
                            Operand::Copy(Place::local(iter)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    self.propagate_aggr_state(iter, dest);
                    return Some(dest);
                }
                let v = self.lower_iter_vec_arg(&args[1])?;
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_) | TyKind::Slice(_)) {
                    ty
                } else {
                    self.tcx.intern(TyKind::Vec(i64_ty))
                };
                Some(self.emit_combinator_call(
                    "gos_rt_vec_step_by",
                    vec![
                        Operand::Copy(Place::local(v)),
                        Operand::Copy(Place::local(step)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::skip", 2) => {
                let n = self.lower_expr(&args[0])?;
                if self.lazy_iter_result_ty(ty)
                    && let Some(iter) = self.lower_lazy_iter_source_aggr(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_skip_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(n)),
                            Operand::Copy(Place::local(iter)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    self.propagate_aggr_state(iter, dest);
                    return Some(dest);
                }
                let v = self.lower_iter_vec_arg(&args[1])?;
                let dest_ty = self.iter_result_vec_ty(ty, v);
                Some(self.emit_iter_combinator_call(
                    "skip",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(n)),
                        Operand::Copy(Place::local(v)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::rev", 1) => {
                if self.lazy_iter_ty_family(ty).is_some()
                    && let Some(iter) = self.lower_lazy_iter_source(&args[0])
                {
                    // Reversal is only defined once the source's length is
                    // known, so the pipeline is snapshotted, reversed, and
                    // handed back as iterator state the rest of the chain
                    // (and the `for` desugar) can keep pulling from.
                    let elem = match self.tcx.kind_of(ty) {
                        TyKind::Iterator(elem) => *elem,
                        _ => i64_ty,
                    };
                    let vec_ty = self.tcx.intern(TyKind::Vec(elem));
                    let collect_symbol = self.lazy_collect_symbol(elem);
                    let collected = self.emit_combinator_call(
                        collect_symbol,
                        vec![Operand::Copy(Place::local(iter))],
                        vec_ty,
                        span,
                    );
                    let (_, snapshot_abi) = self.iter_elem_abi(vec_ty);
                    let reversed = self.emit_iter_combinator_call(
                        "rev",
                        snapshot_abi,
                        None,
                        vec![Operand::Copy(Place::local(collected))],
                        vec_ty,
                        span,
                    );
                    let from_vec = self.lazy_iter_elem_family(elem).map_or(
                        "gos_rt_lazy_iter_from_vec_i64",
                        LazyElemFamily::vec_source_symbol,
                    );
                    return Some(self.emit_combinator_call(
                        from_vec,
                        vec![Operand::Copy(Place::local(reversed))],
                        ty,
                        span,
                    ));
                }
                // The word-slot reversal moves one slot per element, which
                // for a wider element reorders its fields rather than the
                // elements; the vec form moves each element whole.
                let (_, rev_abi) = self.iter_elem_abi(args[0].ty);
                if rev_abi == ElemAbi::Ptr {
                    let source = self.lower_iter_vec_arg(&args[0])?;
                    let elem = self.sequence_elem_ty_of(self.locals[source.0 as usize].ty);
                    let dest_ty = elem.map_or(ty, |elem| self.tcx.intern(TyKind::Vec(elem)));
                    return Some(self.emit_combinator_call(
                        "gos_rt_vec_reversed",
                        vec![Operand::Copy(Place::local(source))],
                        dest_ty,
                        span,
                    ));
                }
                self.lower_iter_simple_vec_in_vec_out("rev", args, ty, span)
            }
            ("iter::chain", 2) => {
                if self.lazy_iter_ty_family(ty).is_some()
                    && let Some(a) = self.lower_lazy_iter_source(&args[0])
                    && let Some(b) = self.lower_lazy_iter_source(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_chain_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let a = self.lower_iter_vec_arg(&args[0])?;
                let b = self.lower_iter_vec_arg(&args[1])?;
                // The word-slot concatenation copies one slot per element; a
                // wider element is copied whole by the vec form, which also
                // gives the result its own share of each element's children.
                let (_, chain_abi) = self.iter_elem_abi(args[0].ty);
                if chain_abi == ElemAbi::Ptr {
                    let elem = self.sequence_elem_ty_of(self.locals[a.0 as usize].ty);
                    let dest_ty = elem.map_or(ty, |elem| self.tcx.intern(TyKind::Vec(elem)));
                    let joined_local = self.emit_combinator_call(
                        "gos_rt_vec_clone",
                        vec![Operand::Copy(Place::local(a))],
                        dest_ty,
                        span,
                    );
                    let unit_ty = self.tcx.unit();
                    let _ = self.emit_combinator_call(
                        "gos_rt_vec_extend",
                        vec![
                            Operand::Copy(Place::local(joined_local)),
                            Operand::Copy(Place::local(b)),
                        ],
                        unit_ty,
                        span,
                    );
                    return Some(joined_local);
                }
                let dest_ty = self.iter_result_vec_ty(ty, a);
                Some(self.emit_iter_combinator_call(
                    "chain",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(a)),
                        Operand::Copy(Place::local(b)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::dedup", 1) => {
                let v = self.lower_iter_vec_arg(&args[0])?;
                let vec_ty = self.iter_result_vec_ty(ty, v);
                // Consecutive equality is the element's own, so an element
                // whose slot word is not its value travels with the ordering
                // descriptor the runtime compares it through.
                let unit_ty = self.tcx.unit();
                let elem = self.iter_result_elem_ty(unit_ty, v);
                let desc = self.elem_equality_descriptor(elem);
                let has_desc = i128::from(!desc.is_empty());
                let string_ty = self.tcx.string_ty();
                let desc_local = self.fresh(string_ty);
                self.emit_assign(
                    Place::local(desc_local),
                    Rvalue::Use(Operand::Const(ConstValue::Str(desc))),
                    span,
                );
                Some(self.emit_iter_combinator_call(
                    "dedup",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(v)),
                        Operand::Copy(Place::local(desc_local)),
                        Operand::Const(ConstValue::Int(has_desc)),
                    ],
                    vec_ty,
                    span,
                ))
            }
            ("iter::flatten", 1) => {
                let vec_local = self.lower_iter_vec_arg(&args[0])?;
                // The flattened sequence carries the inner sequence's element,
                // which is the element of the element that was read.
                let inner_seq = self.iter_result_elem_ty(ty, vec_local);
                let elem = self.sequence_elem_ty_of(inner_seq).unwrap_or(inner_seq);
                let dest_ty = self.tcx.intern(TyKind::Vec(elem));
                Some(self.emit_iter_combinator_call(
                    "flatten",
                    ElemAbi::Word,
                    None,
                    vec![Operand::Copy(Place::local(vec_local))],
                    dest_ty,
                    span,
                ))
            }
            ("iter::enumerate", 1) => {
                if self.lazy_iter_ty_family(ty).is_some()
                    && let Some(iter_local) = self.lower_lazy_iter_source(&args[0])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_enumerate_i64".to_string(),
                        )),
                        args: vec![Operand::Copy(Place::local(iter_local))],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let vec_local = self.lower_iter_vec_arg(&args[0])?;
                // A wide element does not fit the word slot the pair helper
                // writes, so the pairs are built element by element, each one
                // carrying the element itself.
                let (wide_elem, wide_abi) = self.iter_elem_abi(args[0].ty);
                if wide_abi == ElemAbi::Ptr {
                    return Some(self.lower_enumerate_wide_elem(vec_local, wide_elem, span));
                }
                let unit_ty = self.tcx.unit();
                let elem = self.iter_result_elem_ty(unit_ty, vec_local);
                let pair = self.tcx.intern(TyKind::Tuple(vec![i64_ty, elem]));
                let dest_ty = self.tcx.intern(TyKind::Vec(pair));
                Some(self.emit_iter_combinator_call(
                    "enumerate",
                    ElemAbi::Word,
                    None,
                    vec![Operand::Copy(Place::local(vec_local))],
                    dest_ty,
                    span,
                ))
            }
            ("iter::zip", 2) => {
                if self.lazy_iter_ty_family(ty).is_some()
                    && let Some(a) = self.lower_lazy_iter_source(&args[0])
                    && let Some(b) = self.lower_lazy_iter_source(&args[1])
                {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_zip_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let a = self.lower_iter_vec_arg(&args[0])?;
                let b = self.lower_iter_vec_arg(&args[1])?;
                let (a_elem, _) = self.iter_elem_abi(args[0].ty);
                let (b_elem, _) = self.iter_elem_abi(args[1].ty);
                // The word-slot shim copies each side's slot verbatim, which
                // is the element itself only for an integer-shaped scalar; a
                // float, a String, or an aggregate carries a bit pattern or a
                // managed address the pair has to take ownership of.
                if self.zip_slot_is_the_element(a_elem) && self.zip_slot_is_the_element(b_elem) {
                    let pair = self.tcx.intern(TyKind::Tuple(vec![a_elem, b_elem]));
                    let dest_ty = self.tcx.intern(TyKind::Vec(pair));
                    return Some(self.emit_iter_combinator_call(
                        "zip",
                        ElemAbi::Word,
                        None,
                        vec![
                            Operand::Copy(Place::local(a)),
                            Operand::Copy(Place::local(b)),
                        ],
                        dest_ty,
                        span,
                    ));
                }
                Some(self.lower_zip_general(a, b, a_elem, b_elem, span))
            }
            // Successive overlapping pairs are the sequence zipped against
            // itself advanced by one, which is the general pairing lowering
            // and carries any element through by its own width.
            ("iter::pairwise", 1) => {
                let vec_local = self.lower_iter_vec_arg(&args[0])?;
                let elem = self
                    .sequence_elem_ty_of(self.locals[vec_local.0 as usize].ty)
                    .unwrap_or(i64_ty);
                let vec_ty = self.locals[vec_local.0 as usize].ty;
                let one = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(one),
                    Rvalue::Use(Operand::Const(ConstValue::Int(1))),
                    span,
                );
                let tail = self.emit_iter_combinator_call(
                    "skip",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(one)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    vec_ty,
                    span,
                );
                Some(self.lower_zip_general(vec_local, tail, elem, elem, span))
            }
            ("iter::windows", 2) => {
                let n = self.lower_expr(&args[0])?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let unit_ty = self.tcx.unit();
                let inner = self.iter_result_vec_ty(unit_ty, vec_local);
                let dest_ty = self.tcx.intern(TyKind::Vec(inner));
                Some(self.emit_iter_combinator_call(
                    "windows",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(n)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::chunks", 2) => {
                let n = self.lower_expr(&args[0])?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let unit_ty = self.tcx.unit();
                let inner = self.iter_result_vec_ty(unit_ty, vec_local);
                let dest_ty = self.tcx.intern(TyKind::Vec(inner));
                Some(self.emit_iter_combinator_call(
                    "chunks",
                    ElemAbi::Word,
                    None,
                    vec![
                        Operand::Copy(Place::local(n)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::unzip", 1) => {
                let vec_local = self.lower_iter_vec_arg(&args[0])?;
                // Each side carries its own half of the pair that was read.
                let unit_ty = self.tcx.unit();
                let pair = self.iter_result_elem_ty(unit_ty, vec_local);
                let halves = match self.tcx.kind_of(pair) {
                    TyKind::Tuple(parts) if parts.len() == 2 => Some((parts[0], parts[1])),
                    _ => None,
                };
                let (left, right) = halves.unwrap_or((i64_ty, i64_ty));
                let left_vec = self.tcx.intern(TyKind::Vec(left));
                let right_vec = self.tcx.intern(TyKind::Vec(right));
                let dest_ty = self.tcx.intern(TyKind::Tuple(vec![left_vec, right_vec]));
                Some(self.emit_iter_combinator_call(
                    "unzip",
                    ElemAbi::Word,
                    None,
                    vec![Operand::Copy(Place::local(vec_local))],
                    dest_ty,
                    span,
                ))
            }
            // Closure-taking helpers. Args are `(f, ..., xs)`; coerce
            // the closure to its callback FnTrait shape so the
            // unified callable infra ships an env pointer with the
            // body address at env[0].
            ("iter::for_each", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (mut in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                // A String is a managed pointer word, so it rides the word
                // shim while keeping its own type at the callback.
                if let Some(elem) = self.sequence_elem_ty_of(self.locals[vec_local.0 as usize].ty)
                    && matches!(self.tcx.kind_of(elem), TyKind::String)
                {
                    in_ty = elem;
                }
                let closure_local = self.lower_iter_closure(&args[0], &[in_ty], i64_ty, span)?;
                let _ = self.emit_iter_combinator_call(
                    "for_each",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    unit_ty,
                    span,
                );
                Some(self.lower_unit(span))
            }
            ("iter::map", 2) => {
                // Route an f64-element map through the float-ABI shim + closure
                // so the element rides an SSE register; a hardcoded i64 sig
                // would hand the closure integer-register bits it reads as a
                // garbage double. The output shape stays the closure's own
                // return type so a `[f64] -> [i64]` map (or the reverse) is
                // typed correctly.
                let (in_ty, in_abi) = self.iter_elem_abi(args[1].ty);
                let out_ty = self
                    .iter_element_kind(ty)
                    .map_or(i64_ty, |k| self.tcx.intern(k));
                let out_abi = self.scalar_abi_of(out_ty);
                // The lazy state carries the SOURCE elements, so a lazy result
                // type alone does not qualify the call: the input's own family
                // decides whether a handle can exist at all. A mapped element
                // wider than a slot is answered as the address of storage the
                // callback owns, and the lazy state holds one word per element
                // with nowhere to copy that block to, so it stays eager.
                // A struct, tuple, or array result is answered as the address of
                // the block the callback built, so the lazy state carries that
                // address and the stream is marked as address-carrying. A float
                // source hands the callback its element in another register
                // file, which the address form's callback shape does not.
                if self.elem_is_slot_addressed(out_ty)
                    && self.lazy_addressed_elem(out_ty)
                    && matches!(self.tcx.kind_of(ty), TyKind::Iterator(_))
                    && self
                        .lazy_iter_source_family(args[1].ty)
                        .is_some_and(|family| family != LazyElemFamily::Float)
                {
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[in_ty], out_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let width = i128::from(self.elem_bytes_of(out_ty));
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_lazy_iter_map_aggr".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                            Operand::Const(ConstValue::Int(width)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    // Each result is a block only the stream holds, so the
                    // stream hands it out as a counted blob this meta describes:
                    // the element's owning fields when it has any, the leaf
                    // copy meta when it does not.
                    if let Some(meta) = self
                        .ensure_aggr_struct_meta(out_ty)
                        .or_else(|| self.ensure_aggr_copy_meta(out_ty))
                    {
                        let unit = self.tcx.unit();
                        let sink = self.fresh(unit);
                        self.emit_assign(
                            Place::local(sink),
                            Rvalue::CallIntrinsic {
                                name: "gos_rt_lazy_iter_set_elem_meta",
                                args: vec![
                                    Operand::Copy(Place::local(dest)),
                                    Operand::Const(ConstValue::Str(meta)),
                                ],
                            },
                            span,
                        );
                    }
                    self.local_aggr_iter.insert(dest);
                    return Some(dest);
                }
                if let Some(source) = self.lazy_iter_source_family(args[1].ty)
                    && self.lazy_iter_ty_family(ty).is_some()
                    && !self.elem_is_slot_addressed(out_ty)
                    && self.elem_bytes_of(out_ty) <= 8
                {
                    // A `String` result is a fresh share the stream's consumers
                    // own, so it is produced by the helper that counts it.
                    let counted = self.lazy_iter_elem_family(out_ty) == Some(LazyElemFamily::Ptr);
                    let helper = match (source, out_abi) {
                        (LazyElemFamily::Float, ElemAbi::Float) => "gos_rt_lazy_iter_map_f64",
                        (LazyElemFamily::Float, _) if counted => "gos_rt_lazy_iter_map_f64_str",
                        (LazyElemFamily::Float, _) => "gos_rt_lazy_iter_map_f64_word",
                        (_, ElemAbi::Float) => "gos_rt_lazy_iter_map_word_f64",
                        _ if counted => "gos_rt_lazy_iter_map_str",
                        _ => "gos_rt_lazy_iter_map_i64",
                    };
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[in_ty], out_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper.to_string())),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                // A callback the compiler can name is called directly, one
                // element at a time, instead of through the runtime's
                // combinator shim: no closure environment, no indirect call
                // the optimiser has to see through, and no separate output
                // buffer built by the runtime.
                if let Some(local) = self.try_lower_direct_map(
                    &args[0], &args[1], in_ty, in_abi, out_ty, out_abi, ty, span,
                ) {
                    return Some(local);
                }
                // A word-result map changes the element type, so the output
                // vec cannot inherit the source's stride: the mapped element's
                // own declared width travels with the call.
                // The mapped element rides an integer register unless it is
                // an `f64`; an aggregate result travels as the address of its
                // slots, which is a word.
                let out_class = match out_abi {
                    ElemAbi::Float => ElemAbi::Float,
                    _ => ElemAbi::Word,
                };
                let declares_width = out_class == ElemAbi::Word;
                let closure_local = self.lower_iter_closure(&args[0], &[in_ty], out_ty, span)?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                // The eager shim answers with a `GosVec`, so the destination
                // carries a Vec type even where the surface promised iterator
                // state: downstream terminals read the value's real shape.
                let dest_ty = self.eager_seq_result_ty(ty, out_ty);
                let mut call_args = vec![
                    Operand::Copy(Place::local(closure_local)),
                    Operand::Copy(Place::local(vec_local)),
                ];
                if declares_width {
                    let width = i128::from(self.elem_bytes_of(out_ty));
                    call_args.push(Operand::Const(ConstValue::Int(width)));
                    // A mapped struct, tuple, or array is answered as the
                    // address of its slots whatever its width, so the shim
                    // copies the block rather than storing the word.
                    let by_block = i128::from(self.elem_is_slot_addressed(out_ty));
                    call_args.push(Operand::Const(ConstValue::Int(by_block)));
                }
                let mapped = self.emit_iter_combinator_call(
                    "map",
                    in_abi,
                    Some(out_class),
                    call_args,
                    dest_ty,
                    span,
                );
                // Each element the callback answered carries the shares its
                // fields own, which the result now holds.
                self.tag_owned_elements(mapped, out_ty, span);
                Some(mapped)
            }
            ("iter::filter", 2) => {
                let bool_ty = self.tcx.bool_ty();
                let (in_ty, in_abi) = self.iter_elem_abi(args[1].ty);
                if let Some(source) = self.lazy_iter_source_family(args[1].ty)
                    && self.lazy_iter_result_ty(ty)
                {
                    let helper =
                        format!("gos_rt_lazy_iter_filter_{}", source.word_or_float_suffix());
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    self.propagate_aggr_state(iter_local, dest);
                    return Some(dest);
                }
                let closure_local = self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let dest_ty = self.eager_seq_result_ty(ty, in_ty);
                Some(self.emit_iter_combinator_call(
                    "filter",
                    in_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::fold", 3) => {
                // The accumulator's type is the fold's result and the
                // callback's first parameter; the element fills its second.
                let init_local = self.lower_expr(&args[0])?;
                let acc_ty = self.locals[init_local.0 as usize].ty;
                let (elem_ty, _) = self.iter_elem_abi(args[2].ty);
                let closure_local =
                    self.lower_iter_closure(&args[1], &[acc_ty, elem_ty], acc_ty, span)?;
                self.lower_fold_loop(
                    FoldSeed::Init(init_local),
                    closure_local,
                    &args[2],
                    acc_ty,
                    elem_ty,
                    span,
                )
            }
            ("iter::sum_by", 2) => {
                let (elem_ty, elem_abi) = self.iter_elem_abi(args[1].ty);
                let out_abi = self.scalar_abi_of(ty);
                let out_ty = match out_abi {
                    ElemAbi::Float => self.tcx.float_ty(gossamer_types::FloatTy::F64),
                    _ => i64_ty,
                };
                let out_class = match out_abi {
                    ElemAbi::Float => ElemAbi::Float,
                    _ => ElemAbi::Word,
                };
                let closure_local = self.lower_iter_closure(&args[0], &[elem_ty], out_ty, span)?;
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                Some(self.emit_iter_combinator_call(
                    "sum_by",
                    elem_abi,
                    Some(out_class),
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    out_ty,
                    span,
                ))
            }
            ("iter::any", 2) => {
                let bool_ty = self.tcx.bool_ty();
                if let Some(source) = self.lazy_iter_ty_family(args[1].ty) {
                    let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                    let helper = format!("gos_rt_lazy_iter_any_{}", source.word_or_float_suffix());
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let dest = self.fresh(bool_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                // A wide element is handed to the predicate by slot address,
                // so the closure takes the element type and the by-pointer
                // shim feeds it; matches `iter::all`.
                let (elem_ty, elem_abi) = self.iter_elem_abi(args[1].ty);
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let closure_local = self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                let combinator = "any";
                // Bool-typed destination so `{}` renders true/false
                // like the VM; the shim returns i64 0/1.
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    bool_ty,
                    span,
                ))
            }
            ("iter::all", 2) => {
                let bool_ty = self.tcx.bool_ty();
                if let Some(source) = self.lazy_iter_ty_family(args[1].ty) {
                    let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                    let helper = format!("gos_rt_lazy_iter_all_{}", source.word_or_float_suffix());
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                    let iter_local = self.lower_lazy_iter_source_aggr(&args[1])?;
                    let dest = self.fresh(bool_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper)),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let (elem_ty, elem_abi) = self.iter_elem_abi(args[1].ty);
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let closure_local = self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                let combinator = "all";
                // Bool-typed destination so `{}` renders true/false
                // like the VM; the shim returns i64 0/1.
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    bool_ty,
                    span,
                ))
            }
            ("iter::find", 2) => {
                // Build an Option<i64> from a (flag, value) pair so
                // pattern matching on the result keeps working.
                let bool_ty = self.tcx.bool_ty();
                let (elem_ty, elem_abi) = self.iter_elem_abi(args[1].ty);
                // A wide element has no single-word `Option` payload to carry,
                // so it keeps the eager word-slot path only when its slots fit
                // a word; the combinator surface rejects the rest upstream.
                let closure_local = self.lower_iter_closure(&args[0], &[elem_ty], bool_ty, span)?;
                // A wide element is searched over its own storage: the matches
                // are kept as elements of that shape, and the first of them
                // becomes the payload the way `Some(kept[0])` builds one.
                if elem_abi == ElemAbi::Ptr {
                    return self.lower_find_wide_elem(closure_local, &args[1], elem_ty, ty, span);
                }
                if let Some(source) = self.lazy_iter_ty_family(args[1].ty) {
                    let helper = format!("gos_rt_lazy_iter_find_{}", source.word_or_float_suffix());
                    let iter_local = self.lower_lazy_iter_source(&args[1])?;
                    return Some(self.emit_combinator_call(
                        &helper,
                        vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        ty,
                        span,
                    ));
                }
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let value = self.emit_iter_combinator_call(
                    "find",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    i64_ty,
                    span,
                );
                let flag = self.emit_iter_combinator_call(
                    "find_flag",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    i64_ty,
                    span,
                );
                // Convert flag (0/1) → disc (0 for Some, 1 for None).
                let disc = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(disc),
                    Rvalue::BinaryOp {
                        op: crate::BinOp::Sub,
                        lhs: Operand::Const(ConstValue::Int(1)),
                        rhs: Operand::Copy(Place::local(flag)),
                    },
                    span,
                );
                let rty = self.result_repr_ty(ty);
                let dest = self.fresh(rty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_new",
                        args: vec![
                            Operand::Copy(Place::local(disc)),
                            Operand::Copy(Place::local(value)),
                        ],
                    },
                    span,
                );
                Some(dest)
            }
            _ => None,
        }
    }

    /// Destination type for `option::unwrap_or` / `result::unwrap_or`:
    /// the call expression's HIR type when concrete, else the payload
    /// type (`substs[0]`) recovered from the scrutinee's HIR or MIR
    /// type, else i64. The call-expression type is often still an
    /// inference Var here, and the dest must keep a heap payload's
    /// real type so the drop / fmt machinery sees e.g. a String
    /// rather than a raw i64.
    fn unwrap_default_dest_ty(
        &mut self,
        expr_ty: Ty,
        scrutinee_hir_ty: Ty,
        scrutinee_local: Local,
    ) -> Ty {
        use gossamer_types::TyKind;
        let concrete = |t: Ty| {
            !matches!(
                self.tcx.kind_of(t),
                TyKind::Var(_) | TyKind::Error | TyKind::Never
            )
        };
        if concrete(expr_ty) {
            return expr_ty;
        }
        let sources = [scrutinee_hir_ty, self.locals[scrutinee_local.0 as usize].ty];
        for src in sources {
            if let TyKind::Adt { substs, .. } = self.tcx.kind_of(src) {
                if let Some(payload) = substs.types().first().copied() {
                    if concrete(payload) {
                        return payload;
                    }
                }
            }
        }
        self.tcx.int_ty(gossamer_types::IntTy::I64)
    }

    /// The `option::unwrap` / `option::expect` helper for a payload of type
    /// `payload`: a carrier is boxed and loads back as two words.
    fn option_unwrap_helper(&self, payload: Ty) -> &'static str {
        if self.is_result_or_option_adt(payload) {
            "gos_rt_option_unwrap_carrier"
        } else {
            "gos_rt_option_unwrap"
        }
    }

    pub(crate) fn try_lower_option_call(
        &mut self,
        joined: &str,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::IntTy;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        match (joined, args.len()) {
            ("option::is_some", 1) => {
                self.lower_combinator_pred_call("gos_rt_option_is_some", args, span)
            }
            ("option::is_none", 1) => {
                self.lower_combinator_pred_call("gos_rt_option_is_none", args, span)
            }
            ("option::unwrap", 1) => {
                let opt = self.lower_expr(&args[0])?;
                let dest_ty = self.unwrap_default_dest_ty(ty, args[0].ty, opt);
                let helper = self.option_unwrap_helper(dest_ty);
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(opt))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("option::expect", 2) => {
                let _message = self.lower_expr(&args[0])?;
                let opt = self.lower_expr(&args[1])?;
                let dest_ty = self.unwrap_default_dest_ty(ty, args[1].ty, opt);
                let helper = self.option_unwrap_helper(dest_ty);
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![Operand::Copy(Place::local(opt))],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("option::unwrap_or", 2) => {
                let fallback = self.lower_expr(&args[0])?;
                let opt = self.lower_expr(&args[1])?;
                let payload_ty = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                if self.tcx.elem_is_addressed_aggregate(payload_ty)
                    && !self.carrier_payload_is_carrier(args[1].ty)
                {
                    return Some(self.lower_unwrap_or_inline(
                        opt,
                        CarrierFallback::Value(fallback),
                        args[1].ty,
                        true,
                        payload_ty,
                        span,
                    ));
                }
                let dest_ty = self.unwrap_default_dest_ty(ty, args[1].ty, opt);
                // A carrier payload is boxed, so its helper loads the two words
                // back and takes the option first.
                let nested = self.is_result_or_option_adt(dest_ty);
                let helper = if nested {
                    "gos_rt_result_unwrap_or_carrier"
                } else if matches!(self.tcx.kind_of(dest_ty), gossamer_types::TyKind::Float(_)) {
                    "gos_rt_option_default_f64"
                } else {
                    "gos_rt_option_default_i64"
                };
                let call_args = if nested {
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(fallback)),
                    ]
                } else {
                    vec![
                        Operand::Copy(Place::local(fallback)),
                        Operand::Copy(Place::local(opt)),
                    ]
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: call_args,
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            ("option::ok_or", 2) => {
                let err = self.lower_expr(&args[0])?;
                let opt = self.lower_expr(&args[1])?;
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_result_ok_or".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(err)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `option::map(f, opt) -> Option<U>`. Closure-arg first
            // (Gossamer's data-last `|>` syntactic-sugar passes the
            // pipe value as the *trailing* arg), opt second. Builds
            // a fresh Option packed in `*mut GosResult` with disc=0
            // for Some(mapped) and disc=1 for None passthrough.
            ("option::map", 2) => {
                // Lower the option first so the closure's parameter can be
                // typed from the payload it actually receives. A payload wider
                // than a slot - a tuple or a struct - travels as one word, but
                // the closure must still see its real type or a destructuring
                // pattern reads that word as the wrong shape.
                let opt_local = self.lower_expr(&args[1])?;
                let payload_ty = self
                    .option_payload_of(self.locals[opt_local.0 as usize].ty)
                    .unwrap_or(i64_ty);
                let closure_local =
                    self.lower_iter_closure(&args[0], &[payload_ty], i64_ty, span)?;
                let opt_ty = {
                    let substs = gossamer_types::Substs::from_types([i64_ty]);
                    self.tcx.intern(gossamer_types::TyKind::Adt {
                        def: gossamer_resolve::DefId::local(u32::MAX - 1),
                        substs,
                    })
                };
                let dest = self.fresh(opt_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_option_map_i64".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(opt_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `result::unwrap_or(v, res) -> T`. Data-last pipe: the
            // fallback value is arg 0, the Result arg 1. Returns the
            // `Ok` payload, or the fallback when the Result is `Err`.
            ("result::unwrap_or", 2) => {
                let fallback = self.lower_expr(&args[0])?;
                let res_local = self.lower_expr(&args[1])?;
                let payload_ty = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                if self.tcx.elem_is_addressed_aggregate(payload_ty)
                    && !self.carrier_payload_is_carrier(args[1].ty)
                {
                    return Some(self.lower_unwrap_or_inline(
                        res_local,
                        CarrierFallback::Value(fallback),
                        args[1].ty,
                        false,
                        payload_ty,
                        span,
                    ));
                }
                let dest_ty = self.unwrap_default_dest_ty(ty, args[1].ty, res_local);
                let helper =
                    if matches!(self.tcx.kind_of(dest_ty), gossamer_types::TyKind::Float(_)) {
                        "gos_rt_result_default_f64"
                    } else {
                        "gos_rt_result_default"
                    };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(helper.to_string())),
                    args: vec![
                        Operand::Copy(Place::local(fallback)),
                        Operand::Copy(Place::local(res_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `result::unwrap_or_else(f, res) -> T`. Data-last pipe: the
            // closure is arg 0, the Result arg 1. Returns the `Ok`
            // value, or the closure applied to the `Err` payload.
            ("result::unwrap_or_else", 2) => {
                let payload_ty = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                if self.tcx.elem_is_addressed_aggregate(payload_ty)
                    || self.carrier_payload_is_carrier(args[1].ty)
                {
                    let err_ty = self.enum_payload_ty(args[1].ty, 1).unwrap_or(i64_ty);
                    let closure = self.lower_iter_closure(&args[0], &[err_ty], payload_ty, span)?;
                    let res_local = self.lower_expr(&args[1])?;
                    return Some(self.lower_unwrap_or_inline(
                        res_local,
                        CarrierFallback::Call(closure),
                        args[1].ty,
                        false,
                        payload_ty,
                        span,
                    ));
                }
                let closure_local = self.lower_iter_closure(&args[0], &[i64_ty], i64_ty, span)?;
                let res_local = self.lower_expr(&args[1])?;
                let dest_ty = if matches!(
                    self.tcx.kind_of(ty),
                    gossamer_types::TyKind::Var(_)
                        | gossamer_types::TyKind::Error
                        | gossamer_types::TyKind::Never
                ) {
                    i64_ty
                } else {
                    ty
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(
                        "gos_rt_result_default_with".to_string(),
                    )),
                    args: vec![
                        Operand::Copy(Place::local(res_local)),
                        Operand::Copy(Place::local(closure_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `result::map_err(f, res) -> Result<T, F>`. Data-last
            // pipe: closure first, Result second. Routes through the
            // same env-first shim the method form uses; Ok passes
            // through unchanged.
            ("result::map_err", 2) => {
                let closure_local = self.lower_iter_closure(&args[0], &[i64_ty], i64_ty, span)?;
                let res_local = self.lower_expr(&args[1])?;
                let dest_ty = if matches!(
                    self.tcx.kind_of(ty),
                    gossamer_types::TyKind::Var(_)
                        | gossamer_types::TyKind::Error
                        | gossamer_types::TyKind::Never
                ) {
                    let err_ty = self.tcx.dyn_error_ty();
                    let substs = gossamer_types::Substs::from_types([i64_ty, err_ty]);
                    self.tcx.intern(gossamer_types::TyKind::Adt {
                        def: gossamer_resolve::DefId::local(u32::MAX),
                        substs,
                    })
                } else {
                    ty
                };
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_result_map_err".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(res_local)),
                        Operand::Copy(Place::local(closure_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // `result::map(f, res) -> Result<U, E>`. Same shape as
            // `option::map`; Err passes through unchanged.
            ("result::map", 2) => {
                let closure_local = self.lower_iter_closure(&args[0], &[i64_ty], i64_ty, span)?;
                let res_local = self.lower_expr(&args[1])?;
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([i64_ty, err_ty]);
                let res_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                let dest = self.fresh(res_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_result_map_i64".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(closure_local)),
                        Operand::Copy(Place::local(res_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            _ => None,
        }
    }

    /// Method-form chain combinators on a Result/Option receiver:
    /// `x.and_then(f)` / `or_else(f)` / `filter(p)` / `ok_or_else(f)`.
    /// Mirrors the data-last free forms below: the closure crosses the
    /// C-ABI as the env-blob `lower_iter_closure` builds.
    pub(crate) fn lower_variant_chain_method(
        &mut self,
        receiver: &HirExpr,
        method: &Ident,
        closure_arg: &HirExpr,
        receiver_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let is_option = self.is_option_adt(receiver_ty);
        // `unwrap_or_else` answers the payload rather than another carrier, so
        // it takes its own destination type instead of the carrier repr the
        // chain combinators below share.
        if method.name.as_str() == "unwrap_or_else" {
            let helper = if is_option {
                "gos_rt_option_default_with"
            } else {
                "gos_rt_result_default_with"
            };
            // Option's fallback is a nullary thunk; Result's receives the
            // `Err` payload word.
            let inputs: &[Ty] = if is_option { &[] } else { &[i64_ty] };
            let payload_ty = self.enum_payload_ty(receiver_ty, 0).unwrap_or(i64_ty);
            let dest_ty = if matches!(
                self.tcx.kind_of(ty),
                TyKind::Var(_) | TyKind::Error | TyKind::Never
            ) {
                payload_ty
            } else {
                ty
            };
            let recv = self.lower_expr(receiver)?;
            if self.tcx.elem_is_addressed_aggregate(payload_ty)
                || self.carrier_payload_is_carrier(receiver_ty)
            {
                let err_ty = self.enum_payload_ty(receiver_ty, 1).unwrap_or(i64_ty);
                let inputs: Vec<Ty> = if is_option { Vec::new() } else { vec![err_ty] };
                let closure = self.lower_iter_closure(closure_arg, &inputs, payload_ty, span)?;
                return Some(self.lower_unwrap_or_inline(
                    recv,
                    CarrierFallback::Call(closure),
                    receiver_ty,
                    is_option,
                    dest_ty,
                    span,
                ));
            }
            let closure = self.lower_iter_closure(closure_arg, inputs, payload_ty, span)?;
            return Some(self.emit_combinator_call(
                helper,
                vec![
                    Operand::Copy(Place::local(recv)),
                    Operand::Copy(Place::local(closure)),
                ],
                dest_ty,
                span,
            ));
        }
        let helper = match (method.name.as_str(), is_option) {
            ("and_then", true) => "gos_rt_option_and_then",
            ("and_then", false) => "gos_rt_result_and_then",
            ("or_else", true) => "gos_rt_option_or_else",
            ("or_else", false) => "gos_rt_result_or_else",
            ("filter", true) => "gos_rt_option_filter",
            ("ok_or_else", _) => "gos_rt_result_ok_or_else",
            _ => return None,
        };
        // Closure parameter shape: `and_then`/`filter` and Result's
        // `or_else` receive the payload word; Option's `or_else` and
        // `ok_or_else` are nullary thunks.
        let inputs: &[Ty] = match (method.name.as_str(), is_option) {
            ("or_else", true) | ("ok_or_else", _) => &[],
            _ => &[i64_ty],
        };
        let closure_out = match method.name.as_str() {
            "filter" => self.tcx.bool_ty(),
            "ok_or_else" => i64_ty,
            _ if is_option => {
                let payload = self.enum_payload_ty(receiver_ty, 0).unwrap_or(i64_ty);
                self.option_payload_adt_ty(payload)
            }
            _ => self.result_i64_error_adt_ty(),
        };
        let recv = self.lower_expr(receiver)?;
        let closure = self.lower_iter_closure(closure_arg, inputs, closure_out, span)?;
        let checked = if matches!(
            self.tcx.kind_of(ty),
            TyKind::Var(_) | TyKind::Error | TyKind::Never
        ) {
            receiver_ty
        } else {
            ty
        };
        let dest_ty = self.result_repr_ty(checked);
        Some(self.emit_combinator_call(
            helper,
            vec![
                Operand::Copy(Place::local(recv)),
                Operand::Copy(Place::local(closure)),
            ],
            dest_ty,
            span,
        ))
    }

    /// Lowers the closure-taking std combinators wired natively in the
    /// Task-22 pass: `result::and_then`/`or_else`/`ok`/`err`/`is_ok`/
    /// `is_err`, the remaining `option::*` family, and the newer
    /// closure-taking `iter::*` entries. Free data-last call shapes
    /// only; returns `None` for any other name so the generic call
    /// path keeps running.
    pub(crate) fn try_lower_combinator_call(
        &mut self,
        joined: &str,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        match (joined, args.len()) {
            ("result::and_then" | "result::or_else", 2) => {
                let out_ty = self.result_i64_error_adt_ty();
                let closure = self.lower_iter_closure(&args[0], &[i64_ty], out_ty, span)?;
                let res = self.lower_expr(&args[1])?;
                let dest_ty = self.result_repr_ty(
                    if matches!(
                        self.tcx.kind_of(ty),
                        TyKind::Var(_) | TyKind::Error | TyKind::Never
                    ) {
                        args[1].ty
                    } else {
                        ty
                    },
                );
                let helper = if joined == "result::and_then" {
                    "gos_rt_result_and_then"
                } else {
                    "gos_rt_result_or_else"
                };
                Some(self.emit_combinator_call(
                    helper,
                    vec![
                        Operand::Copy(Place::local(res)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("result::ok" | "result::err", 1) => {
                let res = self.lower_expr(&args[0])?;
                let slot = usize::from(joined == "result::err");
                let payload = self.enum_payload_ty(args[0].ty, slot).unwrap_or(i64_ty);
                let dest_ty = self.option_payload_adt_ty(payload);
                let helper = if joined == "result::ok" {
                    "gos_rt_result_to_opt_ok"
                } else {
                    "gos_rt_result_to_opt_err"
                };
                Some(self.emit_combinator_call(
                    helper,
                    vec![Operand::Copy(Place::local(res))],
                    dest_ty,
                    span,
                ))
            }
            ("result::is_ok", 1) => {
                self.lower_combinator_pred_call("gos_rt_result_is_ok", args, span)
            }
            ("result::is_err", 1) => {
                self.lower_combinator_pred_call("gos_rt_result_is_err", args, span)
            }
            ("option::and_then" | "option::or_else", 2) => {
                let payload = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                let out_ty = self.option_payload_adt_ty(payload);
                let inputs: &[Ty] = if joined == "option::and_then" {
                    &[i64_ty]
                } else {
                    &[]
                };
                let closure = self.lower_iter_closure(&args[0], inputs, out_ty, span)?;
                let opt = self.lower_expr(&args[1])?;
                let helper = if joined == "option::and_then" {
                    "gos_rt_option_and_then"
                } else {
                    "gos_rt_option_or_else"
                };
                Some(self.emit_combinator_call(
                    helper,
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    out_ty,
                    span,
                ))
            }
            ("option::filter", 2) => {
                let closure = self.lower_iter_closure(&args[0], &[i64_ty], bool_ty, span)?;
                let opt = self.lower_expr(&args[1])?;
                let payload = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                let dest_ty = self.option_payload_adt_ty(payload);
                Some(self.emit_combinator_call(
                    "gos_rt_option_filter",
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("option::or", 2) => {
                let alt = self.lower_expr(&args[0])?;
                let opt = self.lower_expr(&args[1])?;
                let payload = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                let dest_ty = self.option_payload_adt_ty(payload);
                Some(self.emit_combinator_call(
                    "gos_rt_option_or",
                    vec![
                        Operand::Copy(Place::local(alt)),
                        Operand::Copy(Place::local(opt)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("option::ok_or_else", 2) => {
                let closure = self.lower_iter_closure(&args[0], &[], i64_ty, span)?;
                let opt = self.lower_expr(&args[1])?;
                let dest_ty = if matches!(
                    self.tcx.kind_of(ty),
                    TyKind::Var(_) | TyKind::Error | TyKind::Never
                ) {
                    let payload = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                    let substs = gossamer_types::Substs::from_types([payload, i64_ty]);
                    self.tcx.intern(TyKind::Adt {
                        def: gossamer_resolve::DefId::local(u32::MAX),
                        substs,
                    })
                } else {
                    ty
                };
                Some(self.emit_combinator_call(
                    "gos_rt_result_ok_or_else",
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("sync::Once::call" | "Once::call", 2) => {
                // `Once::call(o, || ...)` - handle first, nullary closure
                // second. The closure crosses the C-ABI through the same
                // env-thunk convention as `option::unwrap_or_else`; the run
                // body's value is ignored (the i64 result is the ran flag).
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_once_call",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("middleware::bearer_ok" | "http::middleware::bearer_ok", 2) => {
                // `bearer_ok(req, verify)` - request first, a
                // String-taking verify closure second. Mirrors the
                // VM-native `native_bearer_ok`; the closure runs on the
                // extracted Bearer token and its bool result is returned
                // (false, without calling verify, when no Bearer header
                // is present).
                let string_ty = self.tcx.string_ty();
                let req = self.lower_expr(&args[0])?;
                let verify = self.lower_iter_closure(&args[1], &[string_ty], bool_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_http_bearer_ok",
                    vec![
                        Operand::Copy(Place::local(req)),
                        Operand::Copy(Place::local(verify)),
                    ],
                    bool_ty,
                    span,
                ))
            }
            ("sync::Shared::with" | "Shared::with", 2) => {
                // `Shared::with(shared, |v| ...)` - handle first, the
                // callback second. Mirrors the VM-native
                // `native_shared_with`: the lock is held across the call
                // and the guarded value is left as it was.
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[i64_ty], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_shared_with",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("sync::Shared::update" | "Shared::update", 2) => {
                // The callback's answer becomes the guarded value, and the
                // lock spans both the read and the write.
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[i64_ty], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_shared_update",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("sync::RwLock::with_read" | "RwLock::with_read", 2) => {
                // `RwLock::with_read(lock, |v| ...)` - handle first, an
                // i64-taking closure second. Mirrors the VM-native
                // `native_rwlock_with_read`; the callback runs under a
                // read lock and its result is returned unchanged.
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[i64_ty], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_rwlock_with_read",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("sync::RwLock::with_write" | "RwLock::with_write", 2) => {
                // `RwLock::with_write(lock, |v| ...)` - the callback runs
                // under a write lock and its result becomes the new
                // guarded value, which is also returned.
                let handle = self.lower_expr(&args[0])?;
                let closure = self.lower_iter_closure(&args[1], &[i64_ty], i64_ty, span)?;
                Some(self.emit_combinator_call(
                    "gos_rt_rwlock_with_write",
                    vec![
                        Operand::Copy(Place::local(handle)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("option::unwrap_or_else", 2) => {
                let payload_ty = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                if self.tcx.elem_is_addressed_aggregate(payload_ty)
                    || self.carrier_payload_is_carrier(args[1].ty)
                {
                    let closure = self.lower_iter_closure(&args[0], &[], payload_ty, span)?;
                    let opt = self.lower_expr(&args[1])?;
                    return Some(self.lower_unwrap_or_inline(
                        opt,
                        CarrierFallback::Call(closure),
                        args[1].ty,
                        true,
                        payload_ty,
                        span,
                    ));
                }
                let closure = self.lower_iter_closure(&args[0], &[], i64_ty, span)?;
                let opt = self.lower_expr(&args[1])?;
                let dest_ty = self.unwrap_default_dest_ty(ty, args[1].ty, opt);
                Some(self.emit_combinator_call(
                    "gos_rt_option_default_with",
                    vec![
                        Operand::Copy(Place::local(opt)),
                        Operand::Copy(Place::local(closure)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("option::zip", 2) => {
                // The pair is built here and wrapped the way any `Some((a, b))`
                // is, so the carrier holds a copy its own release reclaims.
                let first = self.lower_expr(&args[0])?;
                let second = self.lower_expr(&args[1])?;
                let a = self.enum_payload_ty(args[0].ty, 0).unwrap_or(i64_ty);
                let b = self.enum_payload_ty(args[1].ty, 0).unwrap_or(i64_ty);
                let pair = self.tcx.intern(TyKind::Tuple(vec![a, b]));
                let dest_ty = self.option_payload_adt_ty(pair);
                let dest_ty = self.result_repr_ty(dest_ty);
                let result = self.fresh(dest_ty);
                let both = self.new_block(span);
                let none = self.new_block(span);
                let join = self.new_block(span);
                let check = |this: &mut Self, carrier: Local, next: BlockId| {
                    let disc = this.fresh(i64_ty);
                    this.emit_assign(
                        Place::local(disc),
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_disc",
                            args: vec![Operand::Copy(Place::local(carrier))],
                        },
                        span,
                    );
                    this.terminate(Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::local(disc)),
                        arms: vec![(0, next)],
                        default: none,
                    });
                };
                let second_check = self.new_block(span);
                check(self, first, second_check);
                self.set_current(second_check);
                check(self, second, both);
                self.set_current(both);
                let payload_of = |this: &mut Self, carrier: Local, payload_ty: Ty| {
                    let extractor = if matches!(this.tcx.kind_of(payload_ty), TyKind::Float(_)) {
                        "gos_rt_result_payload_f64"
                    } else if this.is_by_value_enum_ty(payload_ty) {
                        "gos_rt_result_payload_i128"
                    } else {
                        "gos_rt_result_payload"
                    };
                    let payload = this.fresh(payload_ty);
                    this.emit_assign(
                        Place::local(payload),
                        Rvalue::CallIntrinsic {
                            name: extractor,
                            args: vec![Operand::Copy(Place::local(carrier))],
                        },
                        span,
                    );
                    payload
                };
                let left = payload_of(self, first, a);
                let right = payload_of(self, second, b);
                let tuple = self.fresh(pair);
                self.emit_assign(
                    Place::local(tuple),
                    Rvalue::Aggregate {
                        kind: crate::ir::AggregateKind::Tuple,
                        operands: vec![
                            Operand::Copy(Place::local(left)),
                            Operand::Copy(Place::local(right)),
                        ],
                    },
                    span,
                );
                let _ = self.ensure_aggr_copy_meta(pair);
                let some_disc = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(some_disc),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                self.lower_result_ctor_into(result, some_disc, tuple, span);
                self.terminate(Terminator::Goto { target: join });
                self.set_current(none);
                self.emit_assign(
                    Place::local(result),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_new",
                        args: vec![
                            Operand::Const(ConstValue::Int(1)),
                            Operand::Const(ConstValue::Int(0)),
                        ],
                    },
                    span,
                );
                self.terminate(Terminator::Goto { target: join });
                self.set_current(join);
                Some(result)
            }
            ("option::flatten", 1) => {
                let opt = self.lower_expr(&args[0])?;
                let inner = self.enum_payload_ty(args[0].ty, 0).unwrap_or(i64_ty);
                let dest_ty = self.result_repr_ty(inner);
                Some(self.emit_combinator_call(
                    "gos_rt_option_flatten",
                    vec![Operand::Copy(Place::local(opt))],
                    dest_ty,
                    span,
                ))
            }
            ("option::iter", 1) => {
                let opt = self.lower_expr(&args[0])?;
                let payload = self.enum_payload_ty(args[0].ty, 0).unwrap_or(i64_ty);
                let dest_ty = self.tcx.intern(TyKind::Vec(payload));
                Some(self.emit_combinator_call(
                    "gos_rt_option_iter",
                    vec![Operand::Copy(Place::local(opt))],
                    dest_ty,
                    span,
                ))
            }
            ("iter::filter_map" | "iter::find_map", 2) => {
                // Over a lazy stream the callback runs one element per pull, so
                // a consumer that stops early stops the calls with it.
                if joined == "iter::filter_map"
                    && matches!(self.tcx.kind_of(ty), TyKind::Iterator(_))
                    && let Some(source) = self.lazy_iter_source_family_word(args[1].ty)
                    && source != LazyElemFamily::Float
                    && let Some(payload_ty) = self
                        .callable_output_of(&args[0])
                        .and_then(|out| self.option_payload_of(out))
                    && let Some(helper) = match self.lazy_iter_elem_family(payload_ty) {
                        Some(LazyElemFamily::Word) => Some("gos_rt_lazy_iter_filter_map_i64"),
                        Some(LazyElemFamily::Ptr) => Some("gos_rt_lazy_iter_filter_map_str"),
                        _ => None,
                    }
                {
                    let (in_ty, _) = self.iter_elem_abi(args[1].ty);
                    let opt_payload = self.option_payload_adt_ty(payload_ty);
                    let closure_local =
                        self.lower_iter_closure(&args[0], &[in_ty], opt_payload, span)?;
                    let iter_local = self.lower_lazy_iter_source(&args[1])?;
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(helper.to_string())),
                        args: vec![
                            Operand::Copy(Place::local(closure_local)),
                            Operand::Copy(Place::local(iter_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                // The kept payloads are the callback's results, so the result
                // element is the callback's Some payload, not the input's.
                let payload_ty = self
                    .callable_output_of(&args[0])
                    .and_then(|out| self.option_payload_of(out))
                    .unwrap_or(i64_ty);
                let opt_payload = self.option_payload_adt_ty(payload_ty);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], opt_payload, span)?;
                if joined == "iter::filter_map" {
                    let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_)) {
                        ty
                    } else {
                        self.tcx.intern(TyKind::Vec(payload_ty))
                    };
                    // A kept payload wider than a slot is answered as the
                    // address of its storage, so the shim copies the block
                    // rather than storing the word.
                    let width = i128::from(self.elem_bytes_of(payload_ty));
                    let by_block = i128::from(self.elem_is_slot_addressed(payload_ty));
                    let kept = self.emit_iter_combinator_call(
                        "filter_map",
                        elem_abi,
                        None,
                        vec![
                            Operand::Copy(Place::local(closure)),
                            Operand::Copy(Place::local(vec_local)),
                            Operand::Const(ConstValue::Int(width)),
                            Operand::Const(ConstValue::Int(by_block)),
                        ],
                        dest_ty,
                        span,
                    );
                    // Each kept payload is the share its `Some` carried, which
                    // the result now holds.
                    self.tag_owned_elements(kept, payload_ty, span);
                    return Some(kept);
                }
                Some(self.emit_iter_combinator_call(
                    "find_map",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    opt_payload,
                    span,
                ))
            }
            ("iter::flat_map", 2) => {
                let vec_i64 = self.tcx.intern(TyKind::Vec(i64_ty));
                // A callback returning a fixed-size array hands back a
                // raw slot buffer with no GosVec header; route those
                // through the arr-variant shim with the static length.
                let arr_len = match self.callable_output_of(&args[0]) {
                    Some(out) => match self.tcx.kind_of(out) {
                        TyKind::Array { len, .. } => Some(*len),
                        _ => None,
                    },
                    None => None,
                };
                let cb_out = match arr_len {
                    Some(_) => self
                        .callable_output_of(&args[0])
                        .expect("arr_len derived from callable output"),
                    None => vec_i64,
                };
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], cb_out, span)?;
                // The concatenation carries the callback's element, not the
                // one that was read.
                let inner = self
                    .callable_output_of(&args[0])
                    .and_then(|out| self.sequence_elem_ty_of(out));
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Vec(_)) {
                    ty
                } else {
                    inner.map_or(vec_i64, |elem| self.tcx.intern(TyKind::Vec(elem)))
                };
                let mut call_args = vec![
                    Operand::Copy(Place::local(closure)),
                    Operand::Copy(Place::local(vec_local)),
                ];
                // A callback answering a fixed array hands back a raw slot
                // buffer with no header rather than a sequence, so the array
                // variant reads that buffer directly. The element it was
                // called on still crosses in its own class.
                if let Some(len) = arr_len {
                    let len_local = self.fresh(i64_ty);
                    let len_i128 = i128::try_from(len.to_usize()).unwrap_or(0);
                    self.emit_assign(
                        Place::local(len_local),
                        Rvalue::Use(Operand::Const(ConstValue::Int(len_i128))),
                        span,
                    );
                    call_args.push(Operand::Copy(Place::local(len_local)));
                    return Some(self.emit_iter_combinator_call(
                        "flat_map_arr",
                        elem_abi,
                        None,
                        call_args,
                        dest_ty,
                        span,
                    ));
                }
                Some(self.emit_iter_combinator_call(
                    "flat_map", elem_abi, None, call_args, dest_ty, span,
                ))
            }
            ("iter::reduce", 2) => {
                // The accumulator is an element, so the callback takes two of
                // them and answers one, and the Option carries that element.
                let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                let closure =
                    self.lower_iter_closure(&args[0], &[elem_ty, elem_ty], elem_ty, span)?;
                let result_ty = self.option_payload_adt_ty(elem_ty);
                self.lower_fold_loop(
                    FoldSeed::FirstElement(result_ty),
                    closure,
                    &args[1],
                    elem_ty,
                    elem_ty,
                    span,
                )
            }
            ("iter::scan", 3) => {
                // Each accumulator the callback answers is kept in the result,
                // so the loop pushes it there and replaces its own copy.
                let init = self.lower_expr(&args[0])?;
                let acc_ty = self.locals[init.0 as usize].ty;
                let (elem_ty, _) = self.iter_elem_abi(args[2].ty);
                let closure =
                    self.lower_iter_closure(&args[1], &[acc_ty, elem_ty], acc_ty, span)?;
                self.lower_scan_loop(init, closure, &args[2], acc_ty, elem_ty, span)
            }
            ("iter::product_by", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], i64_ty, span)?;
                Some(self.emit_iter_combinator_call(
                    "product_by",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    i64_ty,
                    span,
                ))
            }
            ("iter::position", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                // The answer is an index whatever the element was.
                let dest_ty = self.option_payload_adt_ty(i64_ty);
                Some(self.emit_iter_combinator_call(
                    "position",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::take_while" | "iter::skip_while", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                let dest_ty = self.iter_result_vec_ty(ty, vec_local);
                let combinator = if joined == "iter::take_while" {
                    "take_while"
                } else {
                    "skip_while"
                };
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::partition", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], bool_ty, span)?;
                let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Tuple(_)) {
                    ty
                } else {
                    let unit_ty = self.tcx.unit();
                    let side = self.iter_result_vec_ty(unit_ty, vec_local);
                    self.tcx.intern(TyKind::Tuple(vec![side, side]))
                };
                Some(self.emit_iter_combinator_call(
                    "partition",
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::sort_by" | "iter::min_by" | "iter::max_by", 2) => {
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                // The comparator takes two elements and answers an ordering,
                // so both operands ride the element's own register class.
                let closure = self.lower_iter_closure(&args[0], &[in_ty, in_ty], i64_ty, span)?;
                let unit_ty = self.tcx.unit();
                let elem = self.iter_result_elem_ty(unit_ty, vec_local);
                let (combinator, dest_ty) = match joined {
                    "iter::sort_by" => {
                        let dest = self.iter_result_vec_ty(ty, vec_local);
                        ("sort_by", dest)
                    }
                    "iter::min_by" => ("min_by", self.option_payload_adt_ty(elem)),
                    _ => ("max_by", self.option_payload_adt_ty(elem)),
                };
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::sort_by_key" | "iter::min_by_key" | "iter::max_by_key", 2) => {
                if joined != "iter::sort_by_key"
                    && let Some(dest) = self.try_lower_map_select_by_key(
                        joined == "iter::max_by_key",
                        &args[0],
                        &args[1],
                        span,
                    )
                {
                    return Some(dest);
                }
                if joined != "iter::sort_by_key"
                    && let Some(key_ty) = self.callable_output_of(&args[0])
                    && !self.key_orders_as_word(key_ty)
                {
                    let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                    let closure = self.lower_iter_closure(&args[0], &[elem_ty], key_ty, span)?;
                    return self.lower_select_by_key_loop(
                        closure,
                        &args[1],
                        elem_ty,
                        key_ty,
                        joined == "iter::max_by_key",
                        span,
                    );
                }
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                // The element and the key each pick their own register class,
                // so the callback must be built with, and called through, the
                // exact pair: a float in either position rides an SSE register
                // that an integer-shaped signature never fills.
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let key_ty = self.callable_output_of(&args[0]).unwrap_or(i64_ty);
                let key_is_f64 = matches!(self.tcx.kind_of(key_ty), TyKind::Float(_));
                let closure_ret = if key_is_f64 {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else {
                    i64_ty
                };
                let closure = self.lower_iter_closure(&args[0], &[in_ty], closure_ret, span)?;
                let unit_ty = self.tcx.unit();
                let elem = self.iter_result_elem_ty(unit_ty, vec_local);
                let (combinator, dest_ty) = match joined {
                    "iter::sort_by_key" => {
                        let dest = self.iter_result_vec_ty(ty, vec_local);
                        ("sort_by_key", dest)
                    }
                    "iter::min_by_key" => ("min_by_key", self.option_payload_adt_ty(elem)),
                    _ => ("max_by_key", self.option_payload_adt_ty(elem)),
                };
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                        Operand::Const(ConstValue::Int(i128::from(key_is_f64))),
                    ],
                    dest_ty,
                    span,
                ))
            }
            ("iter::chunk_by" | "iter::count_by", 2) => {
                // The runtime shims group by the key's word, which is the key
                // for every scalar; a `String`, an aggregate, and a payload enum
                // group by value.
                if let Some(key_ty) = self.callable_output_of(&args[0])
                    && (self.is_aggregate_key(key_ty)
                        || (self.struct_name_of(key_ty).is_none()
                            && self.ensure_enum_eq_desc(key_ty).is_some())
                        || matches!(self.tcx.kind_of(key_ty), TyKind::String))
                {
                    let (elem_ty, _) = self.iter_elem_abi(args[1].ty);
                    let closure = self.lower_iter_closure(&args[0], &[elem_ty], key_ty, span)?;
                    return self.lower_group_by_key_loop(
                        closure,
                        &args[1],
                        elem_ty,
                        key_ty,
                        joined == "iter::count_by",
                        span,
                    );
                }
                let vec_local = self.lower_iter_vec_arg(&args[1])?;
                let (in_ty, elem_abi) = self.iter_callback_shape(vec_local);
                let closure = self.lower_iter_closure(&args[0], &[in_ty], i64_ty, span)?;
                let (combinator, dest_ty) = if joined == "iter::chunk_by" {
                    let dest = if matches!(self.tcx.kind_of(ty), TyKind::HashMap { .. }) {
                        ty
                    } else {
                        // The groups hold the source's own elements.
                        let unit_ty = self.tcx.unit();
                        let group = self.iter_result_vec_ty(unit_ty, vec_local);
                        self.tcx.intern(TyKind::HashMap {
                            key: i64_ty,
                            value: group,
                            ordered: false,
                        })
                    };
                    ("chunk_by", dest)
                } else {
                    let dest = if matches!(self.tcx.kind_of(ty), TyKind::HashMap { .. }) {
                        ty
                    } else {
                        self.tcx.intern(TyKind::HashMap {
                            key: i64_ty,
                            value: i64_ty,
                            ordered: false,
                        })
                    };
                    ("count_by", dest)
                };
                Some(self.emit_iter_combinator_call(
                    combinator,
                    elem_abi,
                    None,
                    vec![
                        Operand::Copy(Place::local(closure)),
                        Operand::Copy(Place::local(vec_local)),
                    ],
                    dest_ty,
                    span,
                ))
            }
            _ => None,
        }
    }

    /// Declared output type of a callable-shaped argument expression
    /// (`FnPtr` / `FnTrait` sig, or a lifted closure's registered
    /// return type), or `None` when the shape is unknown.
    fn callable_output_of(&self, arg: &HirExpr) -> Option<Ty> {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(arg.ty) {
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => Some(sig.output),
            TyKind::FnDef { def, .. } => self.fn_returns.get(def).copied(),
            _ => None,
        }
    }

    /// `substs[idx]` of a Result/Option-shaped `ty`, ref-transparent;
    /// `None` when the type is not a resolved enum Adt or the payload
    /// slot is still an inference Var.
    pub(crate) fn enum_payload_ty(&self, ty: Ty, idx: usize) -> Option<Ty> {
        use gossamer_types::TyKind;
        let mut resolved = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(resolved) {
            resolved = *inner;
        }
        match self.tcx.kind_of(resolved) {
            TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                let payload = substs.types().get(idx).copied()?;
                if matches!(
                    self.tcx.kind_of(payload),
                    TyKind::Var(_) | TyKind::Error | TyKind::Never
                ) {
                    None
                } else {
                    Some(payload)
                }
            }
            _ => None,
        }
    }

    /// Emits a call to `helper` and returns the destination local.
    pub(crate) fn emit_combinator_call(
        &mut self,
        helper: &str,
        args: Vec<Operand>,
        dest_ty: Ty,
        span: Span,
    ) -> Local {
        // A shim that declares an element crossing has a family of twins its
        // C-ABI signature cannot tell apart, so the one to call follows from
        // the element class rather than from a name written at the call site.
        debug_assert!(
            gossamer_abi::combinator_abi_of(helper).is_none(),
            "{helper} declares an element crossing, so it must be reached \
             through emit_iter_combinator_call, which derives the symbol from \
             the class present at the call site"
        );
        self.emit_combinator_call_raw(helper, args, dest_ty, span)
    }

    /// Emits a sequence-combinator call, choosing the shim from the element
    /// crossing present rather than from a name spelled at the call site.
    ///
    /// `elem` is the class of the sequence being read and `result` the class
    /// of the element produced, for the combinators whose symbol distinguishes
    /// one. A crossing the registry declares no shim for is a gap in the shim
    /// family, and it is named here rather than reaching the linker.
    /// `m.iter().min_by_key(f)` / `max_by_key(f)` over a map of scalar values,
    /// with `f` a lifted closure whose body has no observable effect: the
    /// runtime finds the entry in one pass over the table, ties going to the
    /// smallest map key, which is the entry a walk in key order keeps. The
    /// winner lands in a one-pair vec that the ordinary combinator turns into
    /// the `Option`, so the result is built exactly as the general path builds
    /// it. `None`, having emitted nothing, leaves the call to that path.
    fn try_lower_map_select_by_key(
        &mut self,
        want_max: bool,
        callback: &HirExpr,
        source: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &source.kind
        else {
            return None;
        };
        if name.name != "iter" || !args.is_empty() {
            return None;
        }
        // A closure that captures nothing reaches here as the path to its
        // lifted body; one lowered before lifting settled still names it.
        let body_name = match &callback.kind {
            HirExprKind::Path { segments, .. } if segments.len() == 1 => &segments[0].name,
            HirExprKind::LiftedClosure { name, captures } if captures.is_empty() => &name.name,
            _ => return None,
        };
        let reads_key = *self.effect_free_pair_keys.get(body_name)?;
        let mut recv_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty) {
            recv_ty = *inner;
        }
        if !matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. }) {
            return None;
        }
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let (declared_key, declared_val) = self.hash_map_kv_tys(recv_ty)?;
        // A string key the callback never reads is handed over as a null word,
        // which is only sound when the body is known not to touch it.
        let key_ty = match self.hash_map_key_kind(recv_ty) {
            Some(MapKeyKind::String) if !reads_key => self.tcx.string_ty(),
            Some(MapKeyKind::I64) => declared_key,
            _ => return None,
        };
        if !matches!(self.hash_map_value_kind(recv_ty), Some(MapValueKind::I64)) {
            return None;
        }
        let tuple_ty = self.tcx.intern(TyKind::Tuple(vec![key_ty, declared_val]));
        if self.elem_bytes_of(tuple_ty) != 16 {
            return None;
        }
        let vec_ty = self.tcx.intern(TyKind::Vec(tuple_ty));
        let (_, elem_abi) = self.iter_elem_abi(vec_ty);
        if !matches!(elem_abi, ElemAbi::Ptr) {
            return None;
        }
        let key_out = self.callable_output_of(callback).unwrap_or(i64_ty);
        let key_is_f64 = matches!(self.tcx.kind_of(key_out), TyKind::Float(_));
        let closure_ret = if key_is_f64 {
            self.tcx.float_ty(gossamer_types::FloatTy::F64)
        } else {
            i64_ty
        };

        let recv_local = self.lower_expr(receiver)?;
        let elem_bytes = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes),
            Rvalue::Use(Operand::Const(ConstValue::Int(16))),
            span,
        );
        let winner = self.emit_combinator_call_raw(
            "Vec::new",
            vec![Operand::Copy(Place::local(elem_bytes))],
            vec_ty,
            span,
        );
        let (in_ty, _) = self.iter_callback_shape(winner);
        let closure = self.lower_iter_closure(callback, &[in_ty], closure_ret, span)?;
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call_raw(
            "gos_rt_map_select_by_key_into",
            vec![
                Operand::Copy(Place::local(recv_local)),
                Operand::Copy(Place::local(winner)),
                Operand::Copy(Place::local(closure)),
                Operand::Const(ConstValue::Int(i128::from(key_is_f64))),
                Operand::Const(ConstValue::Int(i128::from(want_max))),
            ],
            unit_ty,
            span,
        );
        let elem = self.iter_result_elem_ty(unit_ty, winner);
        let dest_ty = self.option_payload_adt_ty(elem);
        Some(self.emit_iter_combinator_call(
            if want_max { "max_by_key" } else { "min_by_key" },
            elem_abi,
            None,
            vec![
                Operand::Copy(Place::local(closure)),
                Operand::Copy(Place::local(winner)),
                Operand::Const(ConstValue::Int(i128::from(key_is_f64))),
            ],
            dest_ty,
            span,
        ))
    }

    pub(crate) fn emit_iter_combinator_call(
        &mut self,
        combinator: &str,
        elem: ElemAbi,
        result: Option<ElemAbi>,
        args: Vec<Operand>,
        dest_ty: Ty,
        span: Span,
    ) -> Local {
        let helper = iter_combinator_helper(combinator, elem, result);
        self.emit_combinator_call_raw(helper, args, dest_ty, span)
    }

    fn emit_combinator_call_raw(
        &mut self,
        helper: &str,
        args: Vec<Operand>,
        dest_ty: Ty,
        span: Span,
    ) -> Local {
        let dest = self.fresh(dest_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(helper.to_string())),
            args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    /// Lowers a 1-arg Result/Option predicate (`is_ok` / `is_some` /
    /// ...) with a bool-typed destination so `{}` prints true/false on
    /// every tier.
    fn lower_combinator_pred_call(
        &mut self,
        helper: &'static str,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let v = self.lower_expr(&args[0])?;
        let bool_ty = self.tcx.bool_ty();
        Some(self.emit_combinator_call(helper, vec![Operand::Copy(Place::local(v))], bool_ty, span))
    }

    pub(crate) fn lower_iter_simple_vec_i64(
        &mut self,
        helper: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let v = self.lower_iter_vec_arg(&args[0])?;
        let dest = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(helper.to_string())),
            args: vec![Operand::Copy(Place::local(v))],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// Like [`Self::lower_iter_simple_vec_i64`] but pins the dest as a
    /// boxed `Option<i64>` (the 16-byte Result/Option ABI), for
    /// terminals such as `iter::min` / `iter::max` whose Gossamer type
    /// is `Option<i64>`. The matching shim returns an i128-packed
    /// Option (None = 1, Some(m) = `gos_rt_result_new(0, m)`).
    /// Lowers `iter::min` / `iter::max` over a sequence to the shim matching
    /// the element type, so a float payload comes back as a float rather than
    /// as the integer its bits spell.
    pub(crate) fn lower_iter_simple_vec_opt(
        &mut self,
        combinator: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let v = self.lower_iter_vec_arg(&args[0])?;
        self.lower_iter_vec_opt_local(v, combinator, span)
    }

    /// `min` / `max` over an already-lowered sequence local, returning
    /// `Option<T>` typed from the local's own element type.
    pub(crate) fn lower_iter_vec_opt_local(
        &mut self,
        v: Local,
        combinator: &str,
        span: Span,
    ) -> Option<Local> {
        let (elem_ty, elem_abi) = self.iter_elem_abi(self.locals[v.0 as usize].ty);
        let payload_ty = match elem_abi {
            ElemAbi::Float => self.tcx.float_ty(gossamer_types::FloatTy::F64),
            _ => elem_ty,
        };
        let opt_ty = self.option_payload_adt_ty(payload_ty);
        Some(self.emit_iter_combinator_call(
            combinator,
            elem_abi,
            None,
            vec![Operand::Copy(Place::local(v))],
            opt_ty,
            span,
        ))
    }

    pub(crate) fn lower_iter_simple_vec_in_vec_out(
        &mut self,
        combinator: &str,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let v = self.lower_iter_vec_arg(&args[0])?;
        // Pin the dest to `Vec<elem>` (never the call's raw Array/Var
        // type): the shim returns a heap `*mut GosVec`, so an
        // unannotated `iter::rev(xs)[i]` would otherwise take the
        // stack-array index path on a heap pointer and SIGSEGV.
        let vec_ty = self.iter_result_vec_ty(ty, v);
        Some(self.emit_iter_combinator_call(
            combinator,
            ElemAbi::Word,
            None,
            vec![Operand::Copy(Place::local(v))],
            vec_ty,
            span,
        ))
    }

    /// How a combinator's callback receives one element of an eager sequence.
    ///
    /// The runtime reads a slot either as a word, as a double, or - when the
    /// element is wider than one slot - as the address of its storage. The
    /// helper name and the callback's parameter type both follow from this,
    /// so they are decided together and never drift apart.
    /// The body a callback argument names when the compiler can see it: a
    /// plain function, or a lifted closure that captured nothing and so needs
    /// no environment. `None` for anything reached through a value.
    fn direct_callback_body(&mut self, callback: &HirExpr) -> Option<String> {
        match &callback.kind {
            HirExprKind::LiftedClosure { name, captures } if captures.is_empty() => {
                Some(name.name.clone())
            }
            _ => None,
        }
    }

    /// `fold` and `reduce` as a loop in this body: each element is pulled from
    /// the source, handed to the callback with the accumulator, and the
    /// callback's answer replaces the accumulator.
    ///
    /// The accumulator is an ordinary local, so every value it holds is owned
    /// the way any local's is: the seed is copied in, each replaced
    /// accumulator is released when the next one is assigned, and the result
    /// leaves the loop as the local's value. A lazy source is pulled one
    /// element per turn, so its adapters run interleaved with the callback.
    fn lower_fold_loop(
        &mut self,
        seed: FoldSeed,
        closure: Local,
        source: &HirExpr,
        acc_ty: Ty,
        elem_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let pull = self.fold_source_pull(source, elem_ty, span)?;
        let acc = self.push_local(acc_ty, None, true);
        let exit = self.new_block(span);
        let (result, empty) = match seed {
            FoldSeed::Init(init) => {
                self.seed_accumulator(acc, init, span);
                (None, exit)
            }
            FoldSeed::FirstElement(result_ty) => {
                let result_ty = self.result_repr_ty(result_ty);
                let result = self.fresh(result_ty);
                let empty = self.new_block(span);
                // A pulled element is a share of its own, so it becomes the
                // accumulator where it lands; a container keeps the copy
                // `seed_accumulator` gives it.
                let into = self
                    .accumulator_clone_symbol(acc_ty)
                    .is_none()
                    .then_some(acc);
                let first = self.emit_fold_pull(&pull, elem_ty, empty, into, span);
                if first != acc {
                    self.seed_accumulator(acc, first, span);
                }
                (Some(result), empty)
            }
        };
        let header = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(header);
        let elem = self.emit_fold_pull(&pull, elem_ty, exit, None, span);
        let acc_arg = self.fold_callback_arg(acc, span);
        let elem_arg = self.fold_callback_arg(elem, span);
        let replaced = self.fresh(acc_ty);
        let after_call = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(closure)),
            args: vec![
                Operand::Copy(Place::local(acc_arg)),
                Operand::Copy(Place::local(elem_arg)),
            ],
            destination: Place::local(replaced),
            target: Some(after_call),
        });
        self.set_current(after_call);
        self.emit_assign(
            Place::local(acc),
            Rvalue::Use(Operand::Copy(Place::local(replaced))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        let Some(result) = result else {
            self.fresh_loop_results.insert(acc);
            return Some(acc);
        };
        // Both arms build the carrier straight into the result, so the
        // payload it carries is the result's own to hand over.
        let join = self.new_block(span);
        let some_disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(some_disc),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.lower_result_ctor_into(result, some_disc, acc, span);
        self.terminate(Terminator::Goto { target: join });

        self.set_current(empty);
        self.emit_assign(
            Place::local(result),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Const(ConstValue::Int(1)),
                    Operand::Const(ConstValue::Int(0)),
                ],
            },
            span,
        );
        self.terminate(Terminator::Goto { target: join });
        self.set_current(join);
        Some(result)
    }

    /// Lowers a loop's element source: lazy state pulled one element per turn,
    /// or a sequence read by index.
    fn fold_source_pull(&mut self, source: &HirExpr, elem_ty: Ty, span: Span) -> Option<FoldPull> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        if self.lazy_iter_source_family(source.ty).is_some()
            && matches!(self.tcx.kind_of(source.ty), TyKind::Iterator(_))
        {
            let state = self.lower_lazy_iter_source_aggr(source)?;
            let next_symbol = if self.local_aggr_iter.contains(&state) {
                "gos_rt_lazy_iter_next_i64"
            } else {
                self.lazy_iter_next_symbol(elem_ty)?
            };
            return Some(FoldPull::Lazy { state, next_symbol });
        }
        let vec = self.lower_iter_vec_arg(source)?;
        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(vec))],
            i64_ty,
            span,
        );
        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        Some(FoldPull::Vec { vec, len, counter })
    }

    /// Whether a key's slot word is its own order, which is what the word-key
    /// combinator shims compare. A `String`, an aggregate, a sequence, a
    /// payload enum, and an unsigned word order by value instead.
    fn key_orders_as_word(&self, key_ty: Ty) -> bool {
        use gossamer_types::{IntTy, TyKind};
        match self.tcx.kind_of(key_ty) {
            TyKind::Int(IntTy::U64 | IntTy::Usize) => false,
            TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_) => true,
            TyKind::Adt { def, .. } => {
                self.tcx.is_inline_enum_ty(key_ty)
                    && self
                        .tcx
                        .enum_variant_tys(*def)
                        .is_some_and(|variants| variants.iter().all(Vec::is_empty))
            }
            _ => false,
        }
    }

    /// Calls a key callback on `elem`, answering the key.
    fn emit_key_call(&mut self, closure: Local, elem: Local, key_ty: Ty, span: Span) -> Local {
        let arg = self.fold_callback_arg(elem, span);
        let key = self.fresh(key_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(closure)),
            args: vec![Operand::Copy(Place::local(arg))],
            destination: Place::local(key),
            target: Some(next),
        });
        self.set_current(next);
        key
    }

    /// `min_by_key` / `max_by_key` over a key that orders by value: each
    /// element's key is compared with the best one so far through the key
    /// type's ordering descriptor, and the first element holding the least (or
    /// greatest) key is answered.
    fn lower_select_by_key_loop(
        &mut self,
        closure: Local,
        source: &HirExpr,
        elem_ty: Ty,
        key_ty: Ty,
        greatest: bool,
        span: Span,
    ) -> Option<Local> {
        let desc: String = self
            .ordering_stream(key_ty)?
            .iter()
            .map(|&b| b as char)
            .collect();
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let pull = self.fold_source_pull(source, elem_ty, span)?;
        let result_ty = self.option_payload_adt_ty(elem_ty);
        let result_ty = self.result_repr_ty(result_ty);
        let result = self.fresh(result_ty);
        let best = self.push_local(elem_ty, None, true);
        let best_key = self.push_local(key_ty, None, true);
        let empty = self.new_block(span);
        let exit = self.new_block(span);
        let first = self.emit_fold_pull(&pull, elem_ty, empty, None, span);
        self.emit_assign(
            Place::local(best),
            Rvalue::Use(Operand::Copy(Place::local(first))),
            span,
        );
        let first_key = self.emit_key_call(closure, best, key_ty, span);
        self.emit_assign(
            Place::local(best_key),
            Rvalue::Use(Operand::Copy(Place::local(first_key))),
            span,
        );
        let header = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(header);
        let elem = self.emit_fold_pull(&pull, elem_ty, exit, None, span);
        let key = self.emit_key_call(closure, elem, key_ty, span);
        let key_slots = self.ordered_value_slots(key, key_ty, span);
        let best_slots = self.ordered_value_slots(best_key, key_ty, span);
        let order = self.emit_combinator_call(
            "gos_rt_desc_cmp",
            vec![
                Operand::Copy(Place::local(key_slots)),
                Operand::Copy(Place::local(best_slots)),
                Operand::Const(ConstValue::Str(desc)),
            ],
            i64_ty,
            span,
        );
        let replaces = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(replaces),
            Rvalue::BinaryOp {
                op: if greatest { BinOp::Gt } else { BinOp::Lt },
                lhs: Operand::Copy(Place::local(order)),
                rhs: Operand::Const(ConstValue::Int(0)),
            },
            span,
        );
        let replace = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(replaces)),
            arms: vec![(0, header)],
            default: replace,
        });
        self.set_current(replace);
        self.emit_assign(
            Place::local(best),
            Rvalue::Use(Operand::Copy(Place::local(elem))),
            span,
        );
        match self.accumulator_clone_symbol(key_ty) {
            // The comparison read this turn's container key, which stays this
            // turn's to reclaim, so the best key takes storage of its own.
            Some(symbol) => {
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(symbol.to_string())),
                    args: vec![Operand::Copy(Place::local(key))],
                    destination: Place::local(best_key),
                    target: Some(next),
                });
                self.set_current(next);
            }
            None => self.emit_assign(
                Place::local(best_key),
                Rvalue::Use(Operand::Copy(Place::local(key))),
                span,
            ),
        }
        self.terminate(Terminator::Goto { target: header });

        let join = self.new_block(span);
        self.set_current(exit);
        let some_disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(some_disc),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.lower_result_ctor_into(result, some_disc, best, span);
        self.terminate(Terminator::Goto { target: join });
        self.set_current(empty);
        self.emit_assign(
            Place::local(result),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Const(ConstValue::Int(1)),
                    Operand::Const(ConstValue::Int(0)),
                ],
            },
            span,
        );
        self.terminate(Terminator::Goto { target: join });
        self.set_current(join);
        Some(result)
    }

    /// `count_by` / `chunk_by` over a key that is hashed by value: each
    /// element's key reaches the map through the key type's descriptor, so
    /// equal keys built at different allocations name one entry.
    fn lower_group_by_key_loop(
        &mut self,
        closure: Local,
        source: &HirExpr,
        elem_ty: Ty,
        key_ty: Ty,
        counting: bool,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        // A `String` key is its text, which the typed string-keyed entries hash
        // directly; an aggregate or enum key reaches the map through its
        // descriptor.
        let (suffix, desc) = if matches!(self.tcx.kind_of(key_ty), TyKind::String) {
            ("typed_str_i64", None)
        } else {
            match self.ensure_enum_eq_desc(key_ty) {
                Some(desc) if self.struct_name_of(key_ty).is_none() => ("ekey", Some(desc)),
                _ => ("skey", Some(self.key_descriptor(key_ty)?)),
            }
        };
        let group_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let value_ty = if counting { i64_ty } else { group_ty };
        let map_ty = self.tcx.intern(TyKind::HashMap {
            key: key_ty,
            value: value_ty,
            ordered: false,
        });
        let map = self.emit_combinator_call("Map::new", Vec::new(), map_ty, span);
        if !counting {
            let unit = self.tcx.unit();
            let sink = self.fresh(unit);
            self.emit_assign(
                Place::local(sink),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_map_set_vec_values",
                    args: vec![Operand::Copy(Place::local(map))],
                },
                span,
            );
        }
        let pull = self.fold_source_pull(source, elem_ty, span)?;
        let exit = self.new_block(span);
        let header = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(header);
        let elem = self.emit_fold_pull(&pull, elem_ty, exit, None, span);
        let key = self.emit_key_call(closure, elem, key_ty, span);
        let desc_op = desc.map(|desc| Operand::Const(ConstValue::Str(desc)));
        if counting {
            let mut inc_args = vec![
                Operand::Copy(Place::local(map)),
                Operand::Copy(Place::local(key)),
            ];
            inc_args.extend(desc_op);
            inc_args.push(Operand::Const(ConstValue::Int(1)));
            self.emit_combinator_call(&format!("gos_rt_map_inc_{suffix}"), inc_args, i64_ty, span);
        } else {
            let bytes = i128::from(self.elem_bytes_of(elem_ty).max(1));
            let fresh_group = self.emit_combinator_call(
                "Vec::new",
                vec![Operand::Const(ConstValue::Int(bytes))],
                group_ty,
                span,
            );
            let mut insert_args = vec![
                Operand::Copy(Place::local(map)),
                Operand::Copy(Place::local(key)),
            ];
            insert_args.extend(desc_op);
            insert_args.push(Operand::Copy(Place::local(fresh_group)));
            let group = self.emit_combinator_call(
                &format!("gos_rt_map_or_insert_{suffix}"),
                insert_args,
                group_ty,
                span,
            );
            let unit = self.tcx.unit();
            self.emit_combinator_call(
                "gos_rt_vec_push",
                vec![
                    Operand::Copy(Place::local(group)),
                    Operand::Copy(Place::local(elem)),
                ],
                unit,
                span,
            );
        }
        self.terminate(Terminator::Goto { target: header });
        self.set_current(exit);
        Some(map)
    }

    /// Gives a fold's accumulator `acc` its starting value `seed`. A container
    /// is copied into storage of its own, so replacing the accumulator frees
    /// only what the fold itself holds.
    fn seed_accumulator(&mut self, acc: Local, seed: Local, span: Span) {
        let ty = self.locals[acc.0 as usize].ty;
        match self.accumulator_clone_symbol(ty) {
            Some(symbol) => {
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(symbol.to_string())),
                    args: vec![Operand::Copy(Place::local(seed))],
                    destination: Place::local(acc),
                    target: Some(next),
                });
                self.set_current(next);
            }
            None => self.emit_assign(
                Place::local(acc),
                Rvalue::Use(Operand::Copy(Place::local(seed))),
                span,
            ),
        }
    }

    /// The runtime copy a container accumulator of type `ty` starts from.
    fn accumulator_clone_symbol(&self, ty: Ty) -> Option<&'static str> {
        if matches!(
            self.tcx.kind_of(ty),
            gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
        ) {
            return Some("gos_rt_vec_clone");
        }
        self.map_or_set_clone_symbol(ty)
    }

    /// `scan`: a fold whose every accumulator is also pushed onto the result,
    /// in order.
    fn lower_scan_loop(
        &mut self,
        init: Local,
        closure: Local,
        source: &HirExpr,
        acc_ty: Ty,
        elem_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let pull = self.fold_source_pull(source, elem_ty, span)?;
        let out_ty = self.tcx.intern(TyKind::Vec(acc_ty));
        let bytes = i128::from(self.elem_bytes_of(acc_ty).max(1));
        let out = self.emit_combinator_call(
            "Vec::new",
            vec![Operand::Const(ConstValue::Int(bytes))],
            out_ty,
            span,
        );
        let acc = self.push_local(acc_ty, None, true);
        self.seed_accumulator(acc, init, span);
        let exit = self.new_block(span);
        let header = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });
        self.set_current(header);
        let elem = self.emit_fold_pull(&pull, elem_ty, exit, None, span);
        let acc_arg = self.fold_callback_arg(acc, span);
        let elem_arg = self.fold_callback_arg(elem, span);
        let replaced = self.fresh(acc_ty);
        let after_call = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(closure)),
            args: vec![
                Operand::Copy(Place::local(acc_arg)),
                Operand::Copy(Place::local(elem_arg)),
            ],
            destination: Place::local(replaced),
            target: Some(after_call),
        });
        self.set_current(after_call);
        self.emit_assign(
            Place::local(acc),
            Rvalue::Use(Operand::Copy(Place::local(replaced))),
            span,
        );
        let unit = self.tcx.unit();
        self.emit_combinator_call(
            "gos_rt_vec_push",
            vec![
                Operand::Copy(Place::local(out)),
                Operand::Copy(Place::local(acc)),
            ],
            unit,
            span,
        );
        self.terminate(Terminator::Goto { target: header });
        self.set_current(exit);
        Some(out)
    }

    /// Pulls the next element of a fold's source, branching to `done` when
    /// there is none, and answers the element in the shape a callback reads.
    /// A lazily pulled element lands in `into` when one is given.
    fn emit_fold_pull(
        &mut self,
        pull: &FoldPull,
        elem_ty: Ty,
        done: BlockId,
        into: Option<Local>,
        span: Span,
    ) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let body = self.new_block(span);
        match *pull {
            FoldPull::Vec { vec, len, counter } => {
                let more = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(more),
                    Rvalue::BinaryOp {
                        op: BinOp::Lt,
                        lhs: Operand::Copy(Place::local(counter)),
                        rhs: Operand::Copy(Place::local(len)),
                    },
                    span,
                );
                self.terminate(Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(more)),
                    arms: vec![(0, done)],
                    default: body,
                });
                self.set_current(body);
                let (elem, _) = self.load_vec_element(vec, counter, elem_ty, false, span);
                self.emit_assign(
                    Place::local(counter),
                    Rvalue::BinaryOp {
                        op: BinOp::Add,
                        lhs: Operand::Copy(Place::local(counter)),
                        rhs: Operand::Const(ConstValue::Int(1)),
                    },
                    span,
                );
                elem
            }
            FoldPull::Lazy { state, next_symbol } => {
                let carrier_ty = self.option_payload_adt_ty(elem_ty);
                let carrier = self.emit_combinator_call(
                    next_symbol,
                    vec![Operand::Copy(Place::local(state))],
                    carrier_ty,
                    span,
                );
                let disc = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(disc),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_result_disc",
                        args: vec![Operand::Copy(Place::local(carrier))],
                    },
                    span,
                );
                self.terminate(Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(disc)),
                    arms: vec![(0, body)],
                    default: done,
                });
                self.set_current(body);
                let extractor =
                    if matches!(self.tcx.kind_of(elem_ty), gossamer_types::TyKind::Float(_)) {
                        "gos_rt_result_payload_f64"
                    } else if self.is_by_value_enum_ty(elem_ty) {
                        "gos_rt_result_payload_i128"
                    } else {
                        "gos_rt_result_payload"
                    };
                let elem = into.unwrap_or_else(|| self.fresh(elem_ty));
                self.emit_assign(
                    Place::local(elem),
                    Rvalue::CallIntrinsic {
                        name: extractor,
                        args: vec![Operand::Copy(Place::local(carrier))],
                    },
                    span,
                );
                elem
            }
        }
    }

    /// The operand a combinator callback takes for `value`: the value itself,
    /// or its address for a two-word carrier the callback reads by reference
    /// (see [`Self::lower_iter_closure`]).
    fn fold_callback_arg(&mut self, value: Local, span: Span) -> Local {
        let ty = self.locals[value.0 as usize].ty;
        if !(crate::lower::carrier_ref::is_two_word_carrier(self.tcx, ty)
            && self.elem_bytes_of(ty) > 8)
        {
            return value;
        }
        let ref_ty = self.tcx.intern(gossamer_types::TyKind::Ref {
            mutability: gossamer_types::Mutbl::Not,
            inner: ty,
        });
        let reference = self.fresh(ref_ty);
        self.emit_assign(
            Place::local(reference),
            Rvalue::Ref {
                mutable: false,
                place: Place::local(value),
            },
            span,
        );
        reference
    }

    /// `xs.map(f)` over a word-slot element and a word-slot result, with `f`
    /// a body the compiler can name: emits the traversal here so the element
    /// is read, transformed, and pushed without a runtime shim in between.
    /// `None` leaves the call to the general lowering.
    #[allow(
        clippy::too_many_arguments,
        reason = "one parameter per shape the specialisation is gated on"
    )]
    fn try_lower_direct_map(
        &mut self,
        callback: &HirExpr,
        source: &HirExpr,
        in_ty: Ty,
        in_abi: ElemAbi,
        out_ty: Ty,
        out_abi: ElemAbi,
        result_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        if in_abi != ElemAbi::Word || out_abi != ElemAbi::Word {
            return None;
        }
        // The traversal below reads and writes one 8-byte slot per element,
        // so a narrower or wider element keeps the shim that knows its stride.
        if self.elem_bytes_of(in_ty) != 8 || self.elem_bytes_of(out_ty) != 8 {
            return None;
        }
        // The output vec's elements are plain words this loop owns outright;
        // an element that carries a heap child needs the shim's element-kind
        // bookkeeping instead.
        if !matches!(
            self.tcx.kind_of(out_ty),
            TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_)
        ) {
            return None;
        }
        if !matches!(
            self.tcx.kind_of(result_ty),
            TyKind::Vec(_) | TyKind::Slice(_)
        ) {
            return None;
        }
        let body = self.direct_callback_body(callback)?;
        let vec_local = self.lower_iter_vec_arg(source)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();

        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(vec_local))],
            i64_ty,
            span,
        );
        let out = self.emit_combinator_call(
            "gos_rt_vec_with_capacity_typed",
            vec![
                Operand::Const(ConstValue::Int(8)),
                Operand::Copy(Place::local(len)),
                Operand::Const(ConstValue::Int(0)),
            ],
            result_ty,
            span,
        );
        let index = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(index),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let more = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(more),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(len)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(more)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        let elem = self.emit_combinator_call(
            "gos_rt_vec_get_i64",
            vec![
                Operand::Copy(Place::local(vec_local)),
                Operand::Copy(Place::local(index)),
            ],
            in_ty,
            span,
        );
        let mapped = self.fresh(out_ty);
        let after_call = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(body)),
            args: vec![Operand::Copy(Place::local(elem))],
            destination: Place::local(mapped),
            target: Some(after_call),
        });
        self.set_current(after_call);
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call(
            "gos_rt_vec_push_i64",
            vec![
                Operand::Copy(Place::local(out)),
                Operand::Copy(Place::local(mapped)),
            ],
            unit_ty,
            span,
        );
        self.emit_assign(
            Place::local(index),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Const(ConstValue::Int(1)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(out)
    }

    /// The callback parameter type and element class for a combinator reading
    /// `vec_local`.
    ///
    /// An `f64` element reaches its callback in an SSE register, an element
    /// wider than one slot by the address of its storage, and everything else
    /// as the word its slot spells. The shim to call and the type the callback
    /// is built with both follow from this, so they are decided together and
    /// cannot drift apart.
    pub(crate) fn iter_callback_shape(&mut self, vec_local: Local) -> (Ty, ElemAbi) {
        let seq_ty = self.locals[vec_local.0 as usize].ty;
        let (elem, abi) = self.iter_elem_abi(seq_ty);
        match abi {
            ElemAbi::Word => (self.tcx.int_ty(gossamer_types::IntTy::I64), abi),
            ElemAbi::Float | ElemAbi::Ptr => (elem, abi),
        }
    }

    /// The ordering descriptor to compare two of `elem` through, or an empty
    /// string when comparing the element's bytes is already its equality.
    ///
    /// A scalar's slot spells its value, so its bytes are its identity. A
    /// String, a struct or tuple holding one, and an enum reached through its
    /// node are all values two of which can be equal at different addresses,
    /// and those are compared through the descriptor instead.
    pub(crate) fn elem_equality_descriptor(&mut self, elem: Ty) -> String {
        use gossamer_types::TyKind;
        let scalar = matches!(
            self.tcx.kind_of(elem),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
        );
        if scalar {
            return String::new();
        }
        self.ordering_stream(elem)
            .map(|tags| tags.iter().map(|&b| b as char).collect())
            .unwrap_or_default()
    }

    /// The element a combinator's result sequence carries: the expected type's
    /// element when the call site names a sequence, and otherwise the element
    /// of the sequence that was read.
    ///
    /// A combinator that answers what it read carries the element with it. The
    /// word fallback stands only for a sequence whose element neither side
    /// names, and reaching it for a wider element is what renders that element
    /// as its raw slot bits.
    pub(crate) fn iter_result_elem_ty(&mut self, expected: Ty, source: Local) -> Ty {
        if let Some(elem) = self.sequence_elem_ty_of(expected) {
            return elem;
        }
        let src_ty = self.locals[source.0 as usize].ty;
        self.sequence_elem_ty_of(src_ty)
            .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64))
    }

    /// `Vec<T>` over [`Self::iter_result_elem_ty`].
    pub(crate) fn iter_result_vec_ty(&mut self, expected: Ty, source: Local) -> Ty {
        let elem = self.iter_result_elem_ty(expected, source);
        self.tcx.intern(gossamer_types::TyKind::Vec(elem))
    }

    /// The class a combinator's sequence argument is read as, from the local
    /// holding it. The address-taken classes are what a callback receives by
    /// slot address rather than as a loaded word.
    pub(crate) fn iter_local_elem_abi(&mut self, vec_local: Local) -> ElemAbi {
        let ty = self.locals[vec_local.0 as usize].ty;
        self.iter_elem_abi(ty).1
    }

    /// Whether a map of type `ty` declares its keys `u64` / `usize`, whose
    /// stored words walk in unsigned order.
    pub(crate) fn map_keys_unsigned(&self, ty: Ty) -> bool {
        self.hash_map_kv_tys(ty).is_some_and(|(key, _)| {
            matches!(
                self.tcx.kind_of(self.peel_ref_ty(key)),
                gossamer_types::TyKind::Int(
                    gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
                )
            )
        })
    }

    /// Whether a set of type `ty` declares its elements `u64` / `usize`,
    /// whose stored words walk in unsigned order.
    pub(crate) fn set_elems_unsigned(&self, ty: Ty) -> bool {
        self.first_generic_of(self.peel_ref_ty(ty))
            .is_some_and(|elem| {
                matches!(
                    self.tcx.kind_of(self.peel_ref_ty(elem)),
                    gossamer_types::TyKind::Int(
                        gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
                    )
                )
            })
    }

    pub(crate) fn iter_elem_abi(&mut self, seq_ty: Ty) -> (Ty, ElemAbi) {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let Some(elem) = self.iter_element_kind(seq_ty).map(|k| self.tcx.intern(k)) else {
            return (i64_ty, ElemAbi::Word);
        };
        if matches!(self.tcx.kind_of(elem), TyKind::Float(_)) {
            return (
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
                ElemAbi::Float,
            );
        }
        // A struct, tuple, or array element is inline slot data the body
        // reaches through its address, whatever its width; a one-field struct
        // fits a slot but its field is still read at an offset, not from the
        // slot's own bits.
        if self.elem_bytes_of(elem) > 8 || self.elem_is_slot_addressed(elem) {
            return (elem, ElemAbi::Ptr);
        }
        (elem, ElemAbi::Word)
    }

    /// Whether an element's storage is read through its address rather than
    /// as the value its slot spells. An enum stays a word: its value is the
    /// inline tag or the RC node pointer the variant decoding reads directly.
    pub(crate) fn elem_is_slot_addressed(&mut self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(elem) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { def, .. } => {
                let def = *def;
                self.tcx.enum_variant_tys(def).is_none() && self.tcx.struct_field_tys(def).is_some()
            }
            _ => false,
        }
    }

    /// ABI class of a combinator's result element (`map`'s output, `fold`'s
    /// accumulator, `sum_by`'s projection): float or word.
    pub(crate) fn scalar_abi_of(&mut self, ty: Ty) -> ElemAbi {
        use gossamer_types::TyKind;
        if matches!(self.tcx.kind_of(ty), TyKind::Float(_)) {
            ElemAbi::Float
        } else {
            ElemAbi::Word
        }
    }

    /// Element type of an iterated sequence local when that element is wider
    /// than one slot, which a combinator's callback receives by slot address
    /// rather than as a loaded word.
    fn iter_wide_elem_ty(&self, vec_local: Local) -> Option<Ty> {
        use gossamer_types::TyKind;
        let elem = match self.tcx.kind_of(self.locals[vec_local.0 as usize].ty) {
            TyKind::Vec(elem) | TyKind::Slice(elem) => *elem,
            _ => return None,
        };
        // What decides the callback's shape is the element's width, not
        // whether it was written as a struct: a tuple of that width is stored
        // and reached exactly the same way.
        self.aggr_lazy_elem(elem).then_some(elem)
    }

    /// True when the iterated sequence's elements are `f64`, whose slot bits a
    /// combinator's callback must receive reinterpreted as a float.
    fn iter_elem_is_float(&self, vec_local: Local) -> bool {
        use gossamer_types::TyKind;
        matches!(
            self.tcx.kind_of(self.locals[vec_local.0 as usize].ty),
            TyKind::Vec(elem) | TyKind::Slice(elem) if matches!(self.tcx.kind_of(*elem), TyKind::Float(_))
        )
    }

    /// `carrier.unwrap_or(v)` / `carrier.unwrap_or_else(f)` for an aggregate
    /// payload, as the branch it stands for: the `Ok` / `Some` payload copied
    /// out of the carrier, or else the fallback. Both arms are copies the frame
    /// owns, so the answer takes its own shares of the value's children the
    /// way a `match` binding does, where a runtime helper would hand back the
    /// payload's address with no share behind it.
    pub(crate) fn lower_unwrap_or_inline(
        &mut self,
        recv: Local,
        fallback: CarrierFallback,
        receiver_ty: Ty,
        is_option: bool,
        dest_ty: Ty,
        span: Span,
    ) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let payload_ty = self.enum_payload_ty(receiver_ty, 0).unwrap_or(dest_ty);
        let dest = self.fresh(dest_ty);
        let disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(disc),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_disc",
                args: vec![Operand::Copy(Place::local(recv))],
            },
            span,
        );
        let present = self.new_block(span);
        let otherwise = self.new_block(span);
        let join = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(disc)),
            arms: vec![(0, present)],
            default: otherwise,
        });
        self.set_current(present);
        if self.carrier_payload_is_carrier(receiver_ty) {
            // A payload that is itself a carrier is boxed, so the reader
            // answers the two words the box holds. The destination takes its
            // own share of them where that call answers, which is a block only
            // this arm reaches.
            let reader = if is_option {
                "gos_rt_option_unwrap_carrier"
            } else {
                "gos_rt_result_unwrap_carrier"
            };
            let answered = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(reader.to_string())),
                args: vec![Operand::Copy(Place::local(recv))],
                destination: Place::local(dest),
                target: Some(answered),
            });
            self.set_current(answered);
        } else {
            let payload = self.fresh(payload_ty);
            self.emit_assign(
                Place::local(payload),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_result_payload",
                    args: vec![Operand::Copy(Place::local(recv))],
                },
                span,
            );
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Copy(Place::local(payload))),
                span,
            );
        }
        self.terminate(Terminator::Goto { target: join });
        self.set_current(otherwise);
        let closure = match fallback {
            CarrierFallback::Value(value) => {
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::Use(Operand::Copy(Place::local(value))),
                    span,
                );
                self.terminate(Terminator::Goto { target: join });
                self.set_current(join);
                return dest;
            }
            CarrierFallback::Call(closure) => closure,
        };
        let args = if is_option {
            Vec::new()
        } else {
            let err_ty = self.enum_payload_ty(receiver_ty, 1).unwrap_or(i64_ty);
            let err = self.fresh(err_ty);
            self.emit_assign(
                Place::local(err),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_result_payload",
                    args: vec![Operand::Copy(Place::local(recv))],
                },
                span,
            );
            vec![Operand::Copy(Place::local(err))]
        };
        self.terminate(Terminator::Call {
            callee: Operand::Copy(Place::local(closure)),
            args,
            destination: Place::local(dest),
            target: Some(join),
        });
        self.set_current(join);
        dest
    }

    pub(crate) fn lower_iter_closure(
        &mut self,
        closure_arg: &HirExpr,
        inputs: &[Ty],
        output: Ty,
        span: Span,
    ) -> Option<Local> {
        // A shim that reads the callback's result as a word still calls a body
        // that answers its own narrower scalar, so the callable keeps that
        // result type and its thunk widens it to the word the shim reads.
        let output = match self.callable_output_of(closure_arg) {
            Some(real)
                if output == self.tcx.int_ty(gossamer_types::IntTy::I64)
                    && matches!(
                        self.tcx.kind_of(real),
                        gossamer_types::TyKind::Bool
                            | gossamer_types::TyKind::Char
                            | gossamer_types::TyKind::Int(
                                gossamer_types::IntTy::I8
                                    | gossamer_types::IntTy::U8
                                    | gossamer_types::IntTy::I16
                                    | gossamer_types::IntTy::U16
                                    | gossamer_types::IntTy::I32
                                    | gossamer_types::IntTy::U32
                            )
                    ) =>
            {
                real
            }
            _ => output,
        };
        let raw = self.lower_expr(closure_arg)?;
        // A combinator hands a wide element to its callback by the address of
        // the element's storage. A struct, tuple, or array parameter IS that
        // address, so the element type is what the callback takes. An
        // `Option` / `Result` / inline-enum element is two words held BY
        // VALUE, so a parameter of that type would be read out of registers
        // the shim never filled: the callback takes a reference to it
        // instead, which is the address the shim passes, and the
        // carrier-reference pass rewrites the body's reads to go through it.
        let inputs: Vec<Ty> = inputs
            .iter()
            .map(|&input| {
                if crate::lower::carrier_ref::is_two_word_carrier(self.tcx, input)
                    && self.elem_bytes_of(input) > 8
                {
                    self.tcx.intern(gossamer_types::TyKind::Ref {
                        mutability: gossamer_types::Mutbl::Not,
                        inner: input,
                    })
                } else {
                    input
                }
            })
            .collect();
        let cb_sig = gossamer_types::FnSig { inputs, output };
        let cb_trait_ty = self.tcx.intern(gossamer_types::TyKind::FnTrait(cb_sig));
        Some(self.coerce_to_fn_trait_if_needed(raw, cb_trait_ty, span))
    }

    /// Lowers an iterator source to a `GosVec` handle the eager combinator
    /// shims can index. A fixed array widens to a Vec; lazy iterator state is
    /// a distinct runtime object, so it is drained into a snapshot Vec first.
    pub(crate) fn lower_iter_vec_arg(&mut self, arg: &HirExpr) -> Option<Local> {
        use gossamer_types::TyKind;
        // An eager traversal of `m.iter()` reads the materialised pairs
        // directly: a cursor over them would only be drained back into a
        // second copy of the same sequence.
        if let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &arg.kind
            && name.name == "iter"
            && args.is_empty()
        {
            let mut recv_ty = self
                .receiver_local_from_path(receiver)
                .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty) {
                recv_ty = *inner;
            }
            if matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. }) {
                return self.materialize_hashmap_entries(receiver, recv_ty, arg.span);
            }
        }
        let raw = self.lower_expr(arg)?;
        let raw_ty = self.locals[raw.0 as usize].ty;
        match self.tcx.kind_of(raw_ty).clone() {
            TyKind::Array { elem, len } => Some(self.coerce_array_to_vec(raw, elem, len, arg.span)),
            // Lazy state, whether it carries its element in a word slot or by
            // address. An eager combinator reads a `GosVec` header, so the
            // handle drains into the sequence it stands for first; handing the
            // handle over unchanged has it read as a vec of its own fields.
            TyKind::Iterator(elem)
                if self.lazy_iter_carries_elem(elem) || self.local_aggr_iter.contains(&raw) =>
            {
                let vec_ty = self.tcx.intern(TyKind::Vec(elem));
                let collect_symbol = self.lazy_collect_symbol_for(raw, elem);
                let drained = self.emit_combinator_call(
                    collect_symbol,
                    vec![Operand::Copy(Place::local(raw))],
                    vec_ty,
                    arg.span,
                );
                if collect_symbol == "gos_rt_lazy_iter_collect_aggr" {
                    self.tag_owned_elements(drained, elem, arg.span);
                }
                Some(drained)
            }
            _ => Some(raw),
        }
    }

    /// Runtime symbol that drains iterator state into a `Vec` of `elem`.
    /// A two-`i64` tuple element rides its own shim because the snapshot
    /// stores pairs rather than scalar slots.
    /// Collect helper for the state `local` holds. An element wider than one
    /// slot reaches a terminal either as a pair of words or as an address, and
    /// only the producing site knows which, so the marker decides before the
    /// element type does.
    pub(crate) fn lazy_collect_symbol_for(&mut self, local: Local, elem: Ty) -> &'static str {
        if self.local_aggr_iter.contains(&local) {
            return "gos_rt_lazy_iter_collect_aggr";
        }
        self.lazy_collect_symbol(elem)
    }

    pub(crate) fn lazy_collect_symbol(&mut self, elem: Ty) -> &'static str {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        match self.tcx.kind_of(elem) {
            TyKind::Tuple(fields)
                if fields.len() == 2 && fields.iter().all(|field| *field == i64_ty) =>
            {
                "gos_rt_lazy_iter_collect_pair_i64"
            }
            _ => "gos_rt_lazy_iter_collect_i64",
        }
    }

    /// Element family a lazy-combinator input can supply, whether it is
    /// iterator state already or a sequence about to be borrowed as one.
    /// `None` for a source the lazy slot cannot carry, which keeps the caller
    /// on the eager sequence surface - including the pair shape, whose state
    /// only `zip` and `enumerate` produce.
    pub(crate) fn lazy_iter_source_family(&self, arg_ty: Ty) -> Option<LazyElemFamily> {
        use gossamer_types::TyKind;
        let mut peeled = arg_ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        let elem = match self.tcx.kind_of(peeled) {
            TyKind::Iterator(elem)
            | TyKind::Range(elem)
            | TyKind::Vec(elem)
            | TyKind::Slice(elem)
            | TyKind::Array { elem, .. } => *elem,
            _ => return None,
        };
        match self.lazy_iter_elem_family(elem) {
            // The pair state is built by `zip` / `enumerate`, never borrowed
            // from a sequence, so a pair-shaped element borrows through the
            // address form like any other multi-slot element.
            Some(LazyElemFamily::PairWord) | None => self
                .lazy_addressed_elem(elem)
                .then_some(LazyElemFamily::Aggr),
            family => family,
        }
    }

    /// Source family for a combinator that reads each slot as an element
    /// value: an address-carrying stream is not one, so such a source stays on
    /// the eager surface where the element is read at its real width.
    pub(crate) fn lazy_iter_source_family_word(&self, arg_ty: Ty) -> Option<LazyElemFamily> {
        self.lazy_iter_source_family(arg_ty)
            .filter(|family| *family != LazyElemFamily::Aggr)
    }

    /// Whether a sequence of `elem` can be borrowed as a stream of element
    /// addresses: the element is stored inline, wider than the word slot a
    /// value-carrying stream reads, and its own storage is what a consumer
    /// reads through.
    pub(crate) fn aggr_lazy_elem(&self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        matches!(
            self.tcx.kind_of(elem),
            TyKind::Tuple(_) | TyKind::Adt { .. }
        ) && self.elem_bytes_of(elem) > 8
    }

    /// Whether a lazy stream of `elem` carries each element as the address of
    /// its storage: an element wider than one slot, or a struct, whose storage
    /// is reached through its address whatever its width.
    pub(crate) fn lazy_addressed_elem(&self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        self.aggr_lazy_elem(elem)
            || matches!(
                self.tcx.kind_of(elem),
                TyKind::Adt { def, .. }
                    if self.tcx.enum_variant_tys(*def).is_none()
                        && self.tcx.struct_field_tys(*def).is_some()
            )
    }

    /// Result type for an adapter that answered eagerly. A surface type of
    /// `Iterator<T>` describes state the eager shim does not build, so the
    /// value is typed as the `Vec<T>` it really is.
    pub(crate) fn eager_seq_result_ty(&mut self, surface: Ty, elem: Ty) -> Ty {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(surface) {
            TyKind::Iterator(_) => self.tcx.intern(TyKind::Vec(elem)),
            _ => surface,
        }
    }

    /// Lazy family of the state a lowered local actually holds.
    ///
    /// A combinator's result type says what the surface promises; the local's
    /// MIR type says what the producing arm built. Only the second decides
    /// whether a value is an iterator handle or a `GosVec`, so every consumer
    /// asks this rather than re-deriving the producer's choice from types.
    pub(crate) fn lowered_lazy_family(&self, local: Local) -> Option<LazyElemFamily> {
        if self.local_aggr_iter.contains(&local) {
            return Some(LazyElemFamily::Aggr);
        }
        self.lazy_iter_ty_family(self.locals[local.0 as usize].ty)
    }

    /// Wraps a materialised `Vec<(K, V)>` of map entries as lazy iterator
    /// state, so `m.iter()` answers the cursor its type names. `None` when the
    /// entry shape has no lazy state to carry it, leaving the caller with the
    /// vec it already built.
    pub(crate) fn entries_cursor(&mut self, entries: Local, span: Span) -> Option<Local> {
        let (handle, _family) = self.borrow_lazy_state(entries, span, true)?;
        Some(handle)
    }

    /// `xs.enumerate()` where the element is wider than one slot.
    ///
    /// The pair helper writes an index and one slot per element, which for a
    /// wider element keeps only its first field. Walking the source and
    /// building each `(index, element)` pair here gives every pair the
    /// element's own width.
    /// Whether an element's 8-byte slot is the element's own value, which is
    /// what lets the word-slot combinator shims copy it verbatim.
    fn zip_slot_is_the_element(&mut self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        matches!(
            self.tcx.kind_of(elem),
            TyKind::Int(_) | TyKind::Bool | TyKind::Char
        )
    }

    /// `iter::zip(a, b)` for any pair of element types: reads element `i` from
    /// each side through the ordinary element path, so each pair owns its
    /// halves and carries their declared types.
    fn lower_zip_general(
        &mut self,
        a: Local,
        b: Local,
        a_elem: Ty,
        b_elem: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let pair_ty = self.tcx.intern(TyKind::Tuple(vec![a_elem, b_elem]));
        let out_ty = self.tcx.intern(TyKind::Vec(pair_ty));
        let _ = self.ensure_aggr_copy_meta(pair_ty);
        let elem_bytes = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                self.type_slot_bytes(pair_ty).max(1),
            )))),
            span,
        );
        let a_len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(a))],
            i64_ty,
            span,
        );
        let b_len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(b))],
            i64_ty,
            span,
        );
        // The pairing stops at the shorter input.
        let shorter = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(shorter),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(a_len)),
                rhs: Operand::Copy(Place::local(b_len)),
            },
            span,
        );
        let len = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(len),
            Rvalue::Use(Operand::Copy(Place::local(b_len))),
            span,
        );
        let take_a = self.new_block(span);
        let after_len = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(shorter)),
            arms: vec![(0, after_len)],
            default: take_a,
        });
        self.set_current(take_a);
        self.emit_assign(
            Place::local(len),
            Rvalue::Use(Operand::Copy(Place::local(a_len))),
            span,
        );
        self.terminate(Terminator::Goto { target: after_len });
        self.set_current(after_len);

        let out = self.emit_combinator_call(
            "gos_rt_vec_with_capacity",
            vec![
                Operand::Copy(Place::local(elem_bytes)),
                Operand::Copy(Place::local(len)),
            ],
            out_ty,
            span,
        );
        let index = self.fresh(i64_ty);
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
        let more = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(more),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(len)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(more)),
            arms: vec![(0, exit)],
            default: body,
        });

        self.set_current(body);
        let left = self.zip_read_element(a, index, a_elem, span);
        let right = self.zip_read_element(b, index, b_elem, span);
        let pair = self.fresh(pair_ty);
        self.emit_assign(
            Place::local(pair),
            Rvalue::Aggregate {
                kind: crate::ir::AggregateKind::Tuple,
                operands: vec![
                    Operand::Copy(Place::local(left)),
                    Operand::Copy(Place::local(right)),
                ],
            },
            span,
        );
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call(
            "gos_rt_vec_push",
            vec![
                Operand::Copy(Place::local(out)),
                Operand::Copy(Place::local(pair)),
            ],
            unit_ty,
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
        out
    }

    /// Element `index` of `source`, read through its slot address so the copy
    /// carries the element's declared type and ownership.
    pub(crate) fn element_slot_ptr(
        &mut self,
        source: Local,
        index: Local,
        elem_ty: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let ref_ty = self.tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Not,
            inner: elem_ty,
        });
        self.emit_combinator_call(
            "gos_rt_vec_get_ptr",
            vec![
                Operand::Copy(Place::local(source)),
                Operand::Copy(Place::local(index)),
            ],
            ref_ty,
            span,
        )
    }

    /// Element `index` of `source` copied out of the sequence's own storage,
    /// the way `let x = xs[i]` reads one. An aggregate element is inline slot
    /// data, so the indexed place is what carries its width; reading it
    /// through a raw slot pointer left a one-slot struct as the pointer's own
    /// bits on the JIT.
    pub(crate) fn read_element_place(
        &mut self,
        source: Local,
        index: Local,
        elem_ty: Ty,
        span: Span,
    ) -> Local {
        let mut place = Place::local(source);
        place.projection.push(crate::ir::Projection::Index(index));
        let element = self.fresh(elem_ty);
        self.emit_assign(
            Place::local(element),
            Rvalue::Use(Operand::Copy(place)),
            span,
        );
        element
    }

    pub(crate) fn zip_read_element(
        &mut self,
        source: Local,
        index: Local,
        elem_ty: Ty,
        span: Span,
    ) -> Local {
        // An aggregate is read from the indexed place, which carries its
        // width; everything else is one slot the pointer path reads directly.
        if self.elem_is_slot_addressed(elem_ty) {
            return self.read_element_place(source, index, elem_ty, span);
        }
        let slot = self.element_slot_ptr(source, index, elem_ty, span);
        let mut slot_place = Place::local(slot);
        slot_place.projection.push(crate::ir::Projection::Deref);
        let element = self.fresh(elem_ty);
        self.emit_assign(
            Place::local(element),
            Rvalue::Use(Operand::Copy(slot_place)),
            span,
        );
        element
    }

    fn lower_enumerate_wide_elem(&mut self, source: Local, elem_ty: Ty, span: Span) -> Local {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let pair_ty = self.tcx.intern(TyKind::Tuple(vec![i64_ty, elem_ty]));
        let out_ty = self.tcx.intern(TyKind::Vec(pair_ty));
        let _ = self.ensure_aggr_copy_meta(pair_ty);
        let elem_bytes = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                self.type_slot_bytes(pair_ty).max(1),
            )))),
            span,
        );
        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(source))],
            i64_ty,
            span,
        );
        let out = self.emit_combinator_call(
            "gos_rt_vec_with_capacity",
            vec![
                Operand::Copy(Place::local(elem_bytes)),
                Operand::Copy(Place::local(len)),
            ],
            out_ty,
            span,
        );
        let index = self.fresh(i64_ty);
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
        let more = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(more),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index)),
                rhs: Operand::Copy(Place::local(len)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(more)),
            arms: vec![(0, exit)],
            default: body,
        });

        self.set_current(body);
        let ref_ty = self.tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Not,
            inner: elem_ty,
        });
        let slot = self.emit_combinator_call(
            "gos_rt_vec_get_ptr",
            vec![
                Operand::Copy(Place::local(source)),
                Operand::Copy(Place::local(index)),
            ],
            ref_ty,
            span,
        );
        let mut slot_place = Place::local(slot);
        slot_place.projection.push(crate::ir::Projection::Deref);
        let element = self.fresh(elem_ty);
        self.emit_assign(
            Place::local(element),
            Rvalue::Use(Operand::Copy(slot_place)),
            span,
        );
        let pair = self.fresh(pair_ty);
        self.emit_assign(
            Place::local(pair),
            Rvalue::Aggregate {
                kind: crate::ir::AggregateKind::Tuple,
                operands: vec![
                    Operand::Copy(Place::local(index)),
                    Operand::Copy(Place::local(element)),
                ],
            },
            span,
        );
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call(
            "gos_rt_vec_push",
            vec![
                Operand::Copy(Place::local(out)),
                Operand::Copy(Place::local(pair)),
            ],
            unit_ty,
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
        out
    }

    /// `xs.min()` / `xs.max()` where the element is wider than one slot.
    ///
    /// The word-slot terminals compare the first slot of each element, which
    /// is one field of it. Ordering a copy structurally - the comparison
    /// `sort` already uses - puts the answer at a known end, and the element
    /// there becomes the payload.
    /// `min` / `max` over a `String` sequence, which orders by text. The
    /// word shims would order the slots' addresses.
    fn lower_minmax_string_elem(
        &mut self,
        seq_arg: &HirExpr,
        want_max: bool,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let mut seq_ty = seq_arg.ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(seq_ty) {
            seq_ty = *inner;
        }
        let (TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) =
            self.tcx.kind_of(seq_ty).clone()
        else {
            return None;
        };
        if !matches!(self.tcx.kind_of(elem), TyKind::String) {
            return None;
        }
        let vec_local = self.lower_iter_vec_arg(seq_arg)?;
        let opt_ty = self.option_payload_adt_ty(elem);
        let helper = if want_max {
            "gos_rt_iter_max_str"
        } else {
            "gos_rt_iter_min_str"
        };
        Some(self.emit_combinator_call(
            helper,
            vec![Operand::Copy(Place::local(vec_local))],
            opt_ty,
            span,
        ))
    }

    fn lower_minmax_wide_elem(
        &mut self,
        seq_arg: &HirExpr,
        elem_ty: Ty,
        ty: Ty,
        want_max: bool,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let (count, tags) = self.tuple_element_stream(elem_ty)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let vec_local = self.lower_iter_vec_arg(seq_arg)?;
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        // The caller's order is its own; the ordering runs on a copy.
        let sorted = self.emit_combinator_call(
            "gos_rt_vec_clone",
            vec![Operand::Copy(Place::local(vec_local))],
            vec_ty,
            span,
        );
        let count_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(count_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(
                i128::try_from(count).unwrap_or(0),
            ))),
            span,
        );
        let tag_text: String = tags.iter().map(|&b| b as char).collect();
        let string_ty = self.tcx.string_ty();
        let tags_local = self.fresh(string_ty);
        self.emit_assign(
            Place::local(tags_local),
            Rvalue::Use(Operand::Const(ConstValue::Str(tag_text))),
            span,
        );
        let unit_ty = self.tcx.unit();
        let _ = self.emit_combinator_call(
            "gos_rt_vec_sort_tuple",
            vec![
                Operand::Copy(Place::local(sorted)),
                Operand::Copy(Place::local(count_local)),
                Operand::Copy(Place::local(tags_local)),
            ],
            unit_ty,
            span,
        );
        let index = self.fresh(i64_ty);
        if want_max {
            let len = self.emit_combinator_call(
                "gos_rt_vec_len",
                vec![Operand::Copy(Place::local(sorted))],
                i64_ty,
                span,
            );
            let one = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(one),
                Rvalue::Use(Operand::Const(ConstValue::Int(1))),
                span,
            );
            self.emit_assign(
                Place::local(index),
                Rvalue::BinaryOp {
                    op: BinOp::Sub,
                    lhs: Operand::Copy(Place::local(len)),
                    rhs: Operand::Copy(Place::local(one)),
                },
                span,
            );
        } else {
            self.emit_assign(
                Place::local(index),
                Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                span,
            );
        }
        Some(self.option_from_vec_element(sorted, index, elem_ty, ty, span))
    }

    /// `xs.find(p)` where the element is wider than one slot.
    ///
    /// The word-slot form carries the found element in the `Option` payload
    /// itself, which an element of this width has no room for. Keeping the
    /// matches first gives storage of the element's own shape to answer from,
    /// and the payload is then minted exactly as a `Some(elem)` in source is:
    /// a heap copy the drop pass reclaims.
    fn lower_find_wide_elem(
        &mut self,
        closure_local: Local,
        seq_arg: &HirExpr,
        elem_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let vec_local = self.lower_iter_vec_arg(seq_arg)?;
        let kept_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let kept = self.emit_iter_combinator_call(
            "filter",
            ElemAbi::Ptr,
            None,
            vec![
                Operand::Copy(Place::local(closure_local)),
                Operand::Copy(Place::local(vec_local)),
            ],
            kept_ty,
            span,
        );
        let zero = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let _ = bool_ty;
        Some(self.option_from_vec_element(kept, zero, elem_ty, ty, span))
    }

    /// `Some(v[index])` when `v` has an element to answer with, `None`
    /// otherwise, for an element wider than one slot.
    ///
    /// The payload is minted the way a `Some(elem)` written in source is: the
    /// backend heap-copies the element and the guarded layout registered here
    /// is what reclaims it, so the answer outlives the storage it was read
    /// from.
    fn option_from_vec_element(
        &mut self,
        vec_local: Local,
        index: Local,
        elem_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(vec_local))],
            i64_ty,
            span,
        );
        let found = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(found),
            Rvalue::BinaryOp {
                op: BinOp::Gt,
                lhs: Operand::Copy(Place::local(len)),
                rhs: Operand::Copy(Place::local(index)),
            },
            span,
        );
        let kept = vec_local;
        let zero = index;
        // The payload is the element by construction, so where the call site
        // names no `Option` the carrier takes its payload from the element
        // rather than falling back to a word.
        let rty = if self.option_payload_of(ty).is_some() {
            self.result_repr_ty(ty)
        } else {
            self.option_payload_adt_ty(elem_ty)
        };
        let dest = self.fresh(rty);
        let some_block = self.new_block(span);
        let none_block = self.new_block(span);
        let join = self.new_block(span);
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(found)),
            arms: vec![(0, none_block)],
            default: some_block,
        });

        self.set_current(some_block);
        let ref_ty = self.tcx.intern(TyKind::Ref {
            mutability: gossamer_types::Mutbl::Not,
            inner: elem_ty,
        });
        let slot = self.emit_combinator_call(
            "gos_rt_vec_get_ptr",
            vec![
                Operand::Copy(Place::local(kept)),
                Operand::Copy(Place::local(zero)),
            ],
            ref_ty,
            span,
        );
        let mut slot_place = Place::local(slot);
        slot_place.projection.push(crate::ir::Projection::Deref);
        let payload = self.fresh(elem_ty);
        self.emit_assign(
            Place::local(payload),
            Rvalue::Use(Operand::Copy(slot_place)),
            span,
        );
        // An aggregate payload outlives the vec it was read from, so the
        // backend's heap copy needs this element's guarded layout to reclaim it.
        if self.is_inline_aggregate_ty(elem_ty) {
            let _ = self.ensure_aggr_copy_meta(elem_ty);
        }
        let some_disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(some_disc),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Copy(Place::local(some_disc)),
                    Operand::Copy(Place::local(payload)),
                ],
            },
            span,
        );
        self.terminate(Terminator::Goto { target: join });

        self.set_current(none_block);
        let none_disc = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(none_disc),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Copy(Place::local(none_disc)),
                    Operand::Copy(Place::local(zero)),
                ],
            },
            span,
        );
        self.terminate(Terminator::Goto { target: join });

        self.set_current(join);
        dest
    }

    /// Carries the address-carrying marker from an adapter's upstream state to
    /// the state it produced, for an adapter that hands its slots through
    /// unchanged.
    fn propagate_aggr_state(&mut self, upstream: Local, dest: Local) {
        if self.local_aggr_iter.contains(&upstream) {
            self.local_aggr_iter.insert(dest);
        }
    }

    /// Drains address-carrying state into a `Vec` of its elements, so a
    /// consumer that reads elements at their real width has storage to read.
    fn drain_aggr_state(&mut self, state: Local, span: Span) -> Local {
        use gossamer_types::TyKind;
        let elem = self
            .sequence_elem_ty_of(self.locals[state.0 as usize].ty)
            .unwrap_or_else(|| self.tcx.int_ty(gossamer_types::IntTy::I64));
        let vec_ty = self.tcx.intern(TyKind::Vec(elem));
        let drained = self.emit_combinator_call(
            "gos_rt_lazy_iter_collect_aggr",
            vec![Operand::Copy(Place::local(state))],
            vec_ty,
            span,
        );
        self.tag_owned_elements(drained, elem, span);
        drained
    }

    /// Declares that the vec in `vec`, filled by a runtime shim with elements
    /// whose shares the shim handed over, owns what each `elem` element holds:
    /// a `String`, a nested container, a payload enum node, or the heap fields
    /// of an inline struct or tuple. A vec already tagged from its source keeps
    /// its layout; the tag records ownership and takes no shares itself.
    pub(crate) fn tag_owned_elements(&mut self, vec: Local, elem: Ty, span: Span) {
        use crate::lower::helpers::ElemOwnership;
        let (symbol, meta) = if matches!(self.tcx.kind_of(elem), gossamer_types::TyKind::String) {
            ("gos_rt_vec_mark_str_elems", None)
        } else {
            match crate::lower::helpers::elem_ownership(self.tcx, elem) {
                Some(
                    ownership @ (ElemOwnership::Owned(_)
                    | ElemOwnership::VecElems
                    | ElemOwnership::RcElems),
                ) => (ownership.symbol(), ownership.meta().map(str::to_string)),
                _ => return,
            }
        };
        let mut args = vec![Operand::Copy(Place::local(vec))];
        if let Some(meta) = meta {
            args.push(Operand::Const(ConstValue::Str(meta)));
        }
        let unit = self.tcx.unit();
        let sink = self.fresh(unit);
        self.emit_assign(
            Place::local(sink),
            Rvalue::CallIntrinsic { name: symbol, args },
            span,
        );
    }

    /// Lowers a combinator's sequence argument once, reporting whether the
    /// lowered value is genuinely lazy iterator state.
    ///
    /// Unlike [`Self::lower_iter_vec_arg`] this keeps iterator state as state;
    /// the caller decides between the lazy and the eager surface from the
    /// family this reports.
    pub(crate) fn lower_iter_seq_arg(
        &mut self,
        arg: &HirExpr,
    ) -> Option<(Local, Option<LazyElemFamily>)> {
        let (local, family) = self.lower_iter_seq_arg_raw(arg)?;
        // Address-carrying state reaches only the arms that pass slots to a
        // callback; everywhere else it is drained first, so a consumer never
        // reads an address as an element.
        if family == Some(LazyElemFamily::Aggr) {
            let drained = self.drain_aggr_state(local, arg.span);
            return Some((drained, None));
        }
        Some((local, family))
    }

    /// Like [`Self::lower_iter_seq_arg`], keeping address-carrying state as
    /// state for a caller that hands each slot to a callback.
    pub(crate) fn lower_iter_seq_arg_raw(
        &mut self,
        arg: &HirExpr,
    ) -> Option<(Local, Option<LazyElemFamily>)> {
        use gossamer_types::TyKind;
        let raw = self.lower_expr(arg)?;
        let raw_ty = self.locals[raw.0 as usize].ty;
        let local = match self.tcx.kind_of(raw_ty).clone() {
            TyKind::Array { elem, len } => self.coerce_array_to_vec(raw, elem, len, arg.span),
            _ => raw,
        };
        let family = self.lowered_lazy_family(local);
        // The pair state `zip` and `enumerate` build advances through a helper
        // of its own that no word adapter reads, so its pairs ride on as the
        // counted blobs an address-carrying stream hands out.
        if family == Some(LazyElemFamily::PairWord) {
            let ty = self.locals[local.0 as usize].ty;
            let handle = self.emit_combinator_call(
                "gos_rt_lazy_iter_pair_blobs",
                vec![Operand::Copy(Place::local(local))],
                ty,
                arg.span,
            );
            self.local_aggr_iter.insert(handle);
            return Some((handle, Some(LazyElemFamily::Aggr)));
        }
        Some((local, family))
    }

    /// Borrows a lowered `GosVec` as lazy state tagged with its own element
    /// family, or returns `None` when the elements are too wide for the slot.
    fn borrow_lazy_state(
        &mut self,
        source: Local,
        span: Span,
        allow_aggr: bool,
    ) -> Option<(Local, LazyElemFamily)> {
        use gossamer_types::TyKind;
        let family = self.lazy_iter_source_family(self.locals[source.0 as usize].ty)?;
        // An address-carrying stream is only for a consumer that hands each
        // slot to a callback; every other one reads elements at their real
        // width from storage, which is the eager surface.
        if family == LazyElemFamily::Aggr && !allow_aggr {
            return None;
        }
        let source_elem = self.sequence_elem_ty_of(self.locals[source.0 as usize].ty);
        let elem_ty = match family {
            LazyElemFamily::Float => self.tcx.float_ty(gossamer_types::FloatTy::F64),
            // The address rides the slot, but what the consumer reads through
            // it is the element, so the state names the element type.
            LazyElemFamily::Aggr => source_elem?,
            // A counted element keeps its type, so every consumer downstream
            // picks the helpers that account for the share each pull carries.
            LazyElemFamily::Ptr => self.tcx.string_ty(),
            _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
        };
        let iter_ty = self.tcx.intern(TyKind::Iterator(elem_ty));
        let helper = family.vec_source_symbol();
        let handle = self.emit_combinator_call(
            helper,
            vec![Operand::Copy(Place::local(source))],
            iter_ty,
            span,
        );
        if family == LazyElemFamily::Aggr {
            self.local_aggr_iter.insert(handle);
        }
        Some((handle, family))
    }

    /// Lowers an edition-2027 scalar iterator input. Existing iterator state
    /// passes through unchanged; Vec, slice, and fixed-array sources become a
    /// retained borrowed runtime iterator handle before an adapter consumes
    /// them.
    ///
    /// The borrow helper is chosen from the source's own element family, so
    /// the handle the adapter receives is tagged with the class its elements
    /// really have.
    fn lower_lazy_iter_source(&mut self, arg: &HirExpr) -> Option<Local> {
        self.lower_lazy_iter_source_classed(arg, false)
    }

    /// Lowers a lazy source for a combinator that hands each slot to a
    /// callback, so a multi-slot element can ride the stream as its address.
    fn lower_lazy_iter_source_aggr(&mut self, arg: &HirExpr) -> Option<Local> {
        self.lower_lazy_iter_source_classed(arg, true)
    }

    fn lower_lazy_iter_source_classed(&mut self, arg: &HirExpr, allow_aggr: bool) -> Option<Local> {
        let (local, family) = self.lower_iter_seq_arg_raw(arg)?;
        if let Some(family) = family {
            if family == LazyElemFamily::Aggr && !allow_aggr {
                return None;
            }
            return Some(local);
        }
        self.borrow_lazy_state(local, arg.span, allow_aggr)
            .map(|(h, _)| h)
    }

    pub(crate) fn try_lower_for_hashmap_iter(
        &mut self,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let HirExprKind::MethodCall { receiver, name, .. } = &for_loop.iter_expr.kind else {
            return None;
        };
        if name.name != "iter" {
            return None;
        }
        let mut recv_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
        // Peel `&` / `&mut` so `for (k, v) in m.iter()` over a `&HashMap`
        // parameter is recognised as a map receiver; otherwise it falls through
        // to the generic for-vec path, which reads the map handle as a Vec. The
        // downstream key / value kind helpers already peel, and the receiver
        // handle matches what `m.len()` / `m.get_or()` pass through a borrow.
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty) {
            recv_ty = *inner;
        }
        if !matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. }) {
            return None;
        }
        self.lower_for_hashmap_pairs(receiver, recv_ty, for_loop, span)
    }

    pub(crate) fn try_lower_for_bare_hashmap_iter(
        &mut self,
        receiver: &HirExpr,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let mut recv_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(recv_ty) {
            recv_ty = *inner;
        }
        if !matches!(self.tcx.kind_of(recv_ty), TyKind::HashMap { .. }) {
            return None;
        }
        self.lower_for_hashmap_pairs(receiver, recv_ty, for_loop, span)
    }

    fn lower_for_hashmap_pairs(
        &mut self,
        receiver: &HirExpr,
        recv_ty: Ty,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let HirPatKind::Tuple(elems) = &for_loop.loop_pat.kind else {
            return None;
        };
        if elems.len() != 2 {
            return None;
        }
        // Accept a `Binding` (bind the name) or `_` (read the slot but
        // bind no name) in either tuple position. The key is read either
        // way - the value lookup needs it - so a `_` key just suppresses
        // its user-visible binding. A literal or nested sub-pattern is
        // left to the generic for-vec path.
        let key_binding = match &elems[0].kind {
            HirPatKind::Binding { name, mutable } => Some((name.clone(), *mutable)),
            HirPatKind::Wildcard => None,
            _ => return None,
        };
        let val_binding = match &elems[1].kind {
            HirPatKind::Binding { name, mutable } => Some((name.clone(), *mutable)),
            HirPatKind::Wildcard => None,
            _ => return None,
        };
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let str_ty = self.tcx.string_ty();
        // An aggregate-keyed map stores its keys as flat content bytes under a
        // slot descriptor, so the snapshot rebuilds each key and the entry
        // lookup goes through the same content-keyed helper the inserts used.
        let skey = self
            .hash_map_kv_tys(recv_ty)
            .filter(|(key, _)| self.is_aggregate_key(*key))
            .and_then(|(key, _)| self.key_descriptor(key).map(|desc| (key, desc)));
        if let Some((key_struct_ty, descriptor)) = skey {
            return self.lower_for_skey_pairs(
                receiver,
                recv_ty,
                key_struct_ty,
                KeyedPairs::Aggregate(descriptor),
                key_binding,
                val_binding,
                for_loop,
                span,
            );
        }
        // A payload enum key is stored under its canonical bytes beside the
        // node itself, so the snapshot hands back the nodes and each value is
        // looked up through the enum's equality descriptor.
        if let Some((key_ty, _)) = self.hash_map_kv_tys(recv_ty)
            && self.struct_name_of(key_ty).is_none()
            && let Some(descriptor) = self.ensure_enum_eq_desc(key_ty)
        {
            return self.lower_for_skey_pairs(
                receiver,
                recv_ty,
                key_ty,
                KeyedPairs::Enum(descriptor),
                key_binding,
                val_binding,
                for_loop,
                span,
            );
        }
        // A key the runtime cannot rebuild cannot drive the loop. When the key
        // is unused (`_`) and the values are scalar, iterate the live values
        // directly - matching the VM, which yields each entry's value.
        if key_binding.is_none()
            && matches!(self.hash_map_key_kind(recv_ty), Some(MapKeyKind::Other))
            && matches!(self.hash_map_value_kind(recv_ty), Some(MapValueKind::I64))
        {
            return self.lower_for_struct_keyed_values(receiver, for_loop.loop_pat, for_loop, span);
        }
        let (key_ty, val_ty, keys_helper, get_or_helper) = {
            let key_kind = self.hash_map_key_kind(recv_ty);
            let value_kind = self.hash_map_value_kind(recv_ty);
            let key_ty = match key_kind {
                Some(MapKeyKind::String) => str_ty,
                _ => i64_ty,
            };
            let val_ty = match value_kind {
                Some(MapValueKind::String) => str_ty,
                // A struct value is stored as a boxed pointer; bind `v`
                // as a reference (a single box-pointer word) so field
                // access derefs the box. Typing the binding as the
                // by-value struct makes the drop pass treat the blob
                // pointer as an inline struct and release its RC fields
                // — a use-after-free (and on Windows a misaligned-RC
                // crash) once the map's own share is later released.
                // Non-struct aggregates keep the value type. Mirrors the
                // `for v in m.values()` binding in
                // [`try_lower_for_hashmap_iter`]'s sibling in ctrl.rs.
                Some(MapValueKind::Other) => {
                    let value_struct = self
                        .hash_map_kv_tys(recv_ty)
                        .map(|(_, v)| v)
                        .filter(|v| self.struct_name_of(*v).is_some());
                    match value_struct {
                        Some(v) => self.tcx.intern(TyKind::Ref {
                            mutability: gossamer_types::Mutbl::Not,
                            inner: v,
                        }),
                        None => self.hash_map_kv_tys(recv_ty).map_or(i64_ty, |(_, v)| v),
                    }
                }
                _ => i64_ty,
            };
            let keys_helper = match key_kind {
                Some(MapKeyKind::String) => "gos_rt_map_keys_str",
                _ if self.map_keys_unsigned(recv_ty) => "gos_rt_map_keys_u64",
                _ => "gos_rt_map_keys_i64",
            };
            let get_or_helper = match (key_kind, value_kind) {
                (Some(MapKeyKind::String), Some(MapValueKind::String)) => {
                    "gos_rt_map_get_or_str_str"
                }
                (Some(MapKeyKind::String), _) => "gos_rt_map_get_or_typed_str_i64",
                (_, Some(MapValueKind::String)) => "gos_rt_map_get_or_i64_str",
                _ => "gos_rt_map_get_or_i64",
            };
            (key_ty, val_ty, keys_helper, get_or_helper)
        };

        // A key and a value that each fit one word are read out of pairs the
        // runtime sorts once, with no lookup per entry, and each is bound as the
        // type the map declares: a scalar's slot holds its own bits.
        let pairs = match (
            self.hash_map_key_kind(recv_ty),
            self.hash_map_value_kind(recv_ty),
            self.hash_map_kv_tys(recv_ty),
        ) {
            (
                Some(MapKeyKind::String | MapKeyKind::I64),
                Some(MapValueKind::String | MapValueKind::I64),
                Some((declared_key, declared_val)),
            ) => {
                let pair_key = if key_ty == str_ty {
                    str_ty
                } else {
                    declared_key
                };
                let pair_val = if val_ty == str_ty {
                    str_ty
                } else {
                    declared_val
                };
                let pair = self.tcx.intern(TyKind::Tuple(vec![pair_key, pair_val]));
                (self.elem_bytes_of(pair) == 16).then_some((pair, pair_key, pair_val))
            }
            _ => None,
        };
        let (key_ty, val_ty) = pairs.map_or((key_ty, val_ty), |(_, k, v)| (k, v));

        let recv_local = self.lower_expr(receiver)?;
        let keys_vec = if let Some((pair, _, _)) = pairs {
            let elem_bytes = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(elem_bytes),
                Rvalue::Use(Operand::Const(ConstValue::Int(16))),
                span,
            );
            let pairs_ty = self.tcx.intern(TyKind::Vec(pair));
            let entries = self.emit_combinator_call_raw(
                "Vec::new",
                vec![Operand::Copy(Place::local(elem_bytes))],
                pairs_ty,
                span,
            );
            let unit_ty = self.tcx.unit();
            let entries_helper = if self.map_keys_unsigned(recv_ty) {
                "gos_rt_map_entries_into_u64"
            } else {
                "gos_rt_map_entries_into"
            };
            let _ = self.emit_combinator_call_raw(
                entries_helper,
                vec![
                    Operand::Copy(Place::local(recv_local)),
                    Operand::Copy(Place::local(entries)),
                ],
                unit_ty,
                span,
            );
            entries
        } else {
            let keys_vec_ty = self.tcx.intern(TyKind::Vec(key_ty));
            let keys_vec = self.fresh(keys_vec_ty);
            let after_keys = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(keys_helper.to_string())),
                args: vec![Operand::Copy(Place::local(recv_local))],
                destination: Place::local(keys_vec),
                target: Some(after_keys),
            });
            self.set_current(after_keys);
            keys_vec
        };

        let len_local = self.fresh(i64_ty);
        let after_len = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(keys_vec))],
            destination: Place::local(len_local),
            target: Some(after_len),
        });
        self.set_current(after_len);

        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        self.push_scope();
        // ptr = gos_rt_vec_get_ptr(keys, counter); k = *ptr
        let ptr_local = self.fresh(i64_ty);
        let after_ptr = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr".to_string())),
            args: vec![
                Operand::Copy(Place::local(keys_vec)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(ptr_local),
            target: Some(after_ptr),
        });
        self.set_current(after_ptr);
        let key_local = self.push_local(
            key_ty,
            key_binding.as_ref().map(|(n, _)| n.clone()),
            key_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &key_binding {
            self.bind_local(&name.name, key_local);
        }
        let after_load = self.new_block(span);
        let zero_off = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero_off),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
            args: vec![
                Operand::Copy(Place::local(ptr_local)),
                Operand::Copy(Place::local(zero_off)),
            ],
            destination: Place::local(key_local),
            target: Some(after_load),
        });
        self.set_current(after_load);

        let val_local = self.push_local(
            val_ty,
            val_binding.as_ref().map(|(n, _)| n.clone()),
            val_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &val_binding {
            self.bind_local(&name.name, val_local);
        }
        let after_val = self.new_block(span);
        if pairs.is_some() {
            // v = the pair's second word, read the way the key was.
            let value_off = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(value_off),
                Rvalue::Use(Operand::Const(ConstValue::Int(8))),
                span,
            );
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
                args: vec![
                    Operand::Copy(Place::local(ptr_local)),
                    Operand::Copy(Place::local(value_off)),
                ],
                destination: Place::local(val_local),
                target: Some(after_val),
            });
        } else {
            // v = m.get_or(k, default). Default-by-value-type: 0 for
            // i64-valued maps, an empty string for string-valued maps.
            let default_local = if val_ty == str_ty {
                let l = self.fresh(str_ty);
                self.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Const(ConstValue::Str(String::new()))),
                    span,
                );
                l
            } else {
                let l = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                l
            };
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(get_or_helper.to_string())),
                args: vec![
                    Operand::Copy(Place::local(recv_local)),
                    Operand::Copy(Place::local(key_local)),
                    Operand::Copy(Place::local(default_local)),
                ],
                destination: Place::local(val_local),
                target: Some(after_val),
            });
        }
        self.set_current(after_val);

        // Auto-region the body, exactly as the `for x in vec` path does: the
        // key/value bindings are read from the snapshot above (outside the
        // region), and only the body's per-iteration allocations are
        // bulk-freed at the iteration boundary. Eligibility rejects any
        // escape, so this can only speed the loop up, never change a result.
        let regioned = self.begin_loop_region(for_loop.body, span);
        // `continue` jumps to the counter bump, `break` leaves the loop.
        // Without this context the body's `break` / `continue` finds no
        // target: it reaches an outer loop's, or none at all.
        self.loop_stack.push(LoopContext {
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(for_loop.body);
        self.loop_stack.pop();
        self.pop_scope();
        self.end_auto_region(regioned, span);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let bumped = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(bumped),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Copy(Place::local(bumped))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        let unit_ty = self.tcx.unit();
        let unit = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(unit),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        Some(unit)
    }

    /// Lowers `for (k, v) in m.iter()` over an aggregate- or enum-keyed map.
    ///
    /// An aggregate key snapshot hands back the rebuilt aggregates as flat
    /// element slots, so the key binding observes each slot's address; an enum
    /// key snapshot hands back the nodes themselves. Either way the value comes
    /// from the same keyed lookup an explicit `m.get(k)` uses.
    #[allow(clippy::too_many_arguments)]
    fn lower_for_skey_pairs(
        &mut self,
        receiver: &HirExpr,
        recv_ty: Ty,
        key_struct_ty: Ty,
        keyed: KeyedPairs,
        key_binding: Option<(Ident, bool)>,
        val_binding: Option<(Ident, bool)>,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let val_ty = self.hash_map_kv_tys(recv_ty).map_or(i64_ty, |(_, v)| v);
        // The key binding names the element's storage, not a copy: an
        // aggregate slot is addressed in place, exactly as a struct-valued
        // binding is, so field reads deref the snapshot's own memory.
        let (key_ref_ty, keys_helper, key_reader, lookup_helper, descriptor) = match keyed {
            KeyedPairs::Aggregate(descriptor) => (
                self.tcx.intern(TyKind::Ref {
                    mutability: gossamer_types::Mutbl::Not,
                    inner: key_struct_ty,
                }),
                "gos_rt_map_keys_skey",
                "gos_rt_vec_get_ptr",
                "gos_rt_map_get_skey_opt",
                descriptor,
            ),
            KeyedPairs::Enum(descriptor) => (
                key_struct_ty,
                "gos_rt_map_keys_ekey",
                "gos_rt_vec_get_i64",
                "gos_rt_map_get_ekey_opt",
                descriptor,
            ),
        };
        let recv_local = self.lower_expr(receiver)?;
        let keys_vec_ty = self.tcx.intern(TyKind::Vec(key_struct_ty));
        let keys_vec = self.emit_combinator_call(
            keys_helper,
            vec![Operand::Copy(Place::local(recv_local))],
            keys_vec_ty,
            span,
        );
        let len_local = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(keys_vec))],
            i64_ty,
            span,
        );

        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        self.push_scope();
        let key_local = self.push_local(
            key_ref_ty,
            key_binding.as_ref().map(|(n, _)| n.clone()),
            key_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &key_binding {
            self.bind_local(&name.name, key_local);
        }
        let after_ptr = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(key_reader.to_string())),
            args: vec![
                Operand::Copy(Place::local(keys_vec)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(key_local),
            target: Some(after_ptr),
        });
        self.set_current(after_ptr);

        // `m.get(k)` in its content-keyed form: an `Option<V>` whose payload
        // word is the stored value, so the binding takes the payload directly.
        let opt_ty = self.option_payload_adt_ty(val_ty);
        let entry = self.emit_combinator_call(
            lookup_helper,
            vec![
                Operand::Copy(Place::local(recv_local)),
                Operand::Copy(Place::local(key_local)),
                Operand::Const(ConstValue::Str(descriptor)),
            ],
            opt_ty,
            span,
        );
        let val_local = self.push_local(
            val_ty,
            val_binding.as_ref().map(|(n, _)| n.clone()),
            val_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &val_binding {
            self.bind_local(&name.name, val_local);
        }
        let after_val = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_result_payload".to_string())),
            args: vec![Operand::Copy(Place::local(entry))],
            destination: Place::local(val_local),
            target: Some(after_val),
        });
        self.set_current(after_val);

        let regioned = self.begin_loop_region(for_loop.body, span);
        // `continue` jumps to the counter bump, `break` leaves the loop.
        // Without this context the body's `break` / `continue` finds no
        // target: it reaches an outer loop's, or none at all.
        self.loop_stack.push(LoopContext {
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(for_loop.body);
        self.loop_stack.pop();
        self.pop_scope();
        self.end_auto_region(regioned, span);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let bumped = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(bumped),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Copy(Place::local(bumped))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        let unit_ty = self.tcx.unit();
        let unit = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(unit),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        Some(unit)
    }

    /// Lowers `for (_, v) in m.iter()` over a struct / tuple-keyed map by
    /// driving the loop from the values snapshot. The key bytes a struct-keyed
    /// map stores are opaque (no `keys()` round-trip), so a key-driven loop
    /// would never iterate; reading the values directly yields each entry's
    /// value exactly as the VM's `m.iter()` does. The value binding's runtime
    /// kind selects the values helper and per-element getter.
    fn lower_for_struct_keyed_values(
        &mut self,
        receiver: &HirExpr,
        loop_pat: &HirPat,
        for_loop: &ForLoopShape<'_>,
        span: Span,
    ) -> Option<Local> {
        let HirPatKind::Tuple(elems) = &loop_pat.kind else {
            return None;
        };
        let val_binding = match &elems[1].kind {
            HirPatKind::Binding { name, mutable } => Some((name.clone(), *mutable)),
            HirPatKind::Wildcard => None,
            _ => return None,
        };
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let str_ty = self.tcx.string_ty();
        let recv_ty = self
            .receiver_local_from_path(receiver)
            .map_or(receiver.ty, |l| self.locals[l.0 as usize].ty);
        let (val_ty, values_helper, getter) = match self.hash_map_value_kind(recv_ty) {
            Some(MapValueKind::String) => (str_ty, "gos_rt_map_values_str", "gos_rt_vec_get_ptr"),
            Some(MapValueKind::Other) => {
                // Struct / aggregate values are boxed pointers in the snapshot.
                let v = self.hash_map_kv_tys(recv_ty).map_or(i64_ty, |(_, v)| v);
                (v, "gos_rt_map_values_vec", "gos_rt_vec_get_ptr")
            }
            _ => (
                i64_ty,
                if self.map_keys_unsigned(recv_ty) {
                    "gos_rt_map_values_u64"
                } else {
                    "gos_rt_map_values_i64"
                },
                "gos_rt_vec_get_i64_unchecked",
            ),
        };

        let recv_local = self.lower_expr(receiver)?;
        let vals_vec_ty = self.tcx.intern(gossamer_types::TyKind::Vec(val_ty));
        let vals_vec = self.fresh(vals_vec_ty);
        let after_vals = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(values_helper.to_string())),
            args: vec![Operand::Copy(Place::local(recv_local))],
            destination: Place::local(vals_vec),
            target: Some(after_vals),
        });
        self.set_current(after_vals);

        let len_local = self.fresh(i64_ty);
        let after_len = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(vals_vec))],
            destination: Place::local(len_local),
            target: Some(after_len),
        });
        self.set_current(after_len);

        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        self.push_scope();
        let val_local = self.push_local(
            val_ty,
            val_binding.as_ref().map(|(n, _)| n.clone()),
            val_binding.as_ref().is_some_and(|(_, m)| *m),
        );
        if let Some((name, _)) = &val_binding {
            self.bind_local(&name.name, val_local);
        }
        let after_get = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(getter.to_string())),
            args: vec![
                Operand::Copy(Place::local(vals_vec)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(val_local),
            target: Some(after_get),
        });
        self.set_current(after_get);

        // Auto-region the body: the value binding is read from the values
        // snapshot above (outside the region), so only the body's
        // per-iteration allocations are arena-freed at the boundary.
        let regioned = self.begin_loop_region(for_loop.body, span);
        // `continue` jumps to the counter bump, `break` leaves the loop.
        // Without this context the body's `break` / `continue` finds no
        // target: it reaches an outer loop's, or none at all.
        self.loop_stack.push(LoopContext {
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(for_loop.body);
        self.loop_stack.pop();
        self.pop_scope();
        self.end_auto_region(regioned, span);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let bumped = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(bumped),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Copy(Place::local(bumped))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        let unit_ty = self.tcx.unit();
        let unit = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(unit),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        Some(unit)
    }

    /// Materialise a map `m.iter()` bound directly to a
    /// `Vec<(K, V)>` into a real heap vector of `(K, V)` tuples.
    /// Mirrors the `for (k, v) in m.iter()` lowering: snapshot the
    /// keys via `gos_rt_map_keys_*`, then `get_or` each value and
    /// push the `(k, v)` tuple. The for-loop form is handled earlier
    /// by `try_lower_for_hashmap_iter`; this covers the direct-bind
    /// form (`let entries = m.iter()`) that otherwise dispatched the
    /// map receiver through `gos_rt_arr_iter` and segfaulted on the
    /// compiled tiers.
    pub(crate) fn materialize_hashmap_entries(
        &mut self,
        receiver: &HirExpr,
        recv_ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let str_ty = self.tcx.string_ty();
        let key_kind = self.hash_map_key_kind(recv_ty);
        let value_kind = self.hash_map_value_kind(recv_ty);
        let pairs_fill_in_runtime = matches!(key_kind, Some(MapKeyKind::String | MapKeyKind::I64))
            && matches!(value_kind, Some(MapValueKind::String | MapValueKind::I64));
        let key_ty = match key_kind {
            Some(MapKeyKind::String) => str_ty,
            _ => i64_ty,
        };
        // A struct value is stored boxed, so the entry's word is the box's
        // address. The materialised pair is an owned element - it outlives the
        // walk that produced it and can be stored - so the struct is copied out
        // of the box into the slot, and the vec's slot-children meta retains
        // what the copy shares with the map's own value. The `for (k, v) in
        // m.iter()` binding keeps the reference instead: it names the entry for
        // the body's duration only.
        let struct_val_ty = match value_kind {
            Some(MapValueKind::Other) => self
                .hash_map_kv_tys(recv_ty)
                .map(|(_, v)| v)
                .filter(|v| self.struct_name_of(*v).is_some()),
            _ => None,
        };
        let val_ty = match (&value_kind, struct_val_ty) {
            (Some(MapValueKind::String), _) => str_ty,
            (_, Some(value)) => value,
            (Some(MapValueKind::Other), None) => {
                self.hash_map_kv_tys(recv_ty).map_or(i64_ty, |(_, v)| v)
            }
            _ => i64_ty,
        };
        let boxed_val_ty = struct_val_ty.map(|value| {
            self.tcx.intern(TyKind::Ref {
                mutability: gossamer_types::Mutbl::Not,
                inner: value,
            })
        });
        let keys_helper = match key_kind {
            Some(MapKeyKind::String) => "gos_rt_map_keys_str",
            _ if self.map_keys_unsigned(recv_ty) => "gos_rt_map_keys_u64",
            _ => "gos_rt_map_keys_i64",
        };
        let get_or_helper = {
            match (key_kind, value_kind) {
                (Some(MapKeyKind::String), Some(MapValueKind::String)) => {
                    "gos_rt_map_get_or_str_str"
                }
                (Some(MapKeyKind::String), _) => "gos_rt_map_get_or_typed_str_i64",
                (_, Some(MapValueKind::String)) => "gos_rt_map_get_or_i64_str",
                _ => "gos_rt_map_get_or_i64",
            }
        };

        let unit_ty = self.tcx.unit();
        let tuple_ty = self.tcx.intern(TyKind::Tuple(vec![key_ty, val_ty]));
        let result_vec_ty = self.tcx.intern(TyKind::Vec(tuple_ty));

        // The runtime copies a scalar's stored word into the pair as it is, so
        // the pair takes the key and value types the map declares: an `f64`,
        // `char`, or `bool` slot is read as itself, never as the integer its
        // bits spell.
        let (tuple_ty, result_vec_ty) = match self.hash_map_kv_tys(recv_ty) {
            Some((declared_key, declared_val)) if pairs_fill_in_runtime => {
                let pair_key = if key_ty == str_ty {
                    str_ty
                } else {
                    declared_key
                };
                let pair_val = if val_ty == str_ty {
                    str_ty
                } else {
                    declared_val
                };
                let pair = self.tcx.intern(TyKind::Tuple(vec![pair_key, pair_val]));
                (pair, self.tcx.intern(TyKind::Vec(pair)))
            }
            _ => (tuple_ty, result_vec_ty),
        };

        let recv_local = self.lower_expr(receiver)?;

        // A pair of two words - each a `String` or a scalar - is written by the
        // runtime in one pass under the map's lock, sorted once, rather than
        // snapshotting the keys and looking every one of them up again.
        if pairs_fill_in_runtime && self.elem_bytes_of(tuple_ty) == 16 {
            let elem_bytes = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(elem_bytes),
                Rvalue::Use(Operand::Const(ConstValue::Int(16))),
                span,
            );
            let result_vec = self.fresh(result_vec_ty);
            let after_new = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("Vec::new".to_string())),
                args: vec![Operand::Copy(Place::local(elem_bytes))],
                destination: Place::local(result_vec),
                target: Some(after_new),
            });
            self.set_current(after_new);
            let filled = self.fresh(unit_ty);
            let after_fill = self.new_block(span);
            let entries_helper = if self.map_keys_unsigned(recv_ty) {
                "gos_rt_map_entries_into_u64"
            } else {
                "gos_rt_map_entries_into"
            };
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(entries_helper.to_string())),
                args: vec![
                    Operand::Copy(Place::local(recv_local)),
                    Operand::Copy(Place::local(result_vec)),
                ],
                destination: Place::local(filled),
                target: Some(after_fill),
            });
            self.set_current(after_fill);
            return Some(result_vec);
        }

        // keys = m.keys() - a fresh real Vec<K> snapshot.
        let keys_vec_ty = self.tcx.intern(TyKind::Vec(key_ty));
        let keys_vec = self.fresh(keys_vec_ty);
        let after_keys = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(keys_helper.to_string())),
            args: vec![Operand::Copy(Place::local(recv_local))],
            destination: Place::local(keys_vec),
            target: Some(after_keys),
        });
        self.set_current(after_keys);

        // len = keys.len()
        let len_local = self.fresh(i64_ty);
        let after_len = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(keys_vec))],
            destination: Place::local(len_local),
            target: Some(after_len),
        });
        self.set_current(after_len);

        // result = Vec::new(elem_bytes_of((K, V)))
        let elem_bytes_val = i128::from(self.elem_bytes_of(tuple_ty).max(8));
        let elem_bytes = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes),
            Rvalue::Use(Operand::Const(ConstValue::Int(elem_bytes_val))),
            span,
        );
        let result_vec = self.fresh(result_vec_ty);
        let after_new = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("Vec::new".to_string())),
            args: vec![Operand::Copy(Place::local(elem_bytes))],
            destination: Place::local(result_vec),
            target: Some(after_new),
        });
        self.set_current(after_new);

        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let bool_ty = self.tcx.bool_ty();
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len_local)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        // key = keys[counter]
        let ptr_local = self.fresh(i64_ty);
        let after_ptr = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr".to_string())),
            args: vec![
                Operand::Copy(Place::local(keys_vec)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(ptr_local),
            target: Some(after_ptr),
        });
        self.set_current(after_ptr);
        let key_local = self.fresh(key_ty);
        let zero_off = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero_off),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let after_load = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
            args: vec![
                Operand::Copy(Place::local(ptr_local)),
                Operand::Copy(Place::local(zero_off)),
            ],
            destination: Place::local(key_local),
            target: Some(after_load),
        });
        self.set_current(after_load);

        // val = m.get_or(key, default)
        let default_local = if val_ty == str_ty {
            let l = self.fresh(str_ty);
            self.emit_assign(
                Place::local(l),
                Rvalue::Use(Operand::Const(ConstValue::Str(String::new()))),
                span,
            );
            l
        } else {
            let l = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(l),
                Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                span,
            );
            l
        };
        let val_local = self.fresh(boxed_val_ty.unwrap_or(val_ty));
        let after_val = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(get_or_helper.to_string())),
            args: vec![
                Operand::Copy(Place::local(recv_local)),
                Operand::Copy(Place::local(key_local)),
                Operand::Copy(Place::local(default_local)),
            ],
            destination: Place::local(val_local),
            target: Some(after_val),
        });
        self.set_current(after_val);
        // The pair's slot holds the struct's own words, so read them through
        // the box the entry named.
        let val_local = if boxed_val_ty.is_some() {
            let copied = self.fresh(val_ty);
            self.emit_assign(
                Place::local(copied),
                Rvalue::Use(Operand::Copy(Place {
                    local: val_local,
                    projection: vec![crate::ir::Projection::Deref],
                })),
                span,
            );
            copied
        } else {
            val_local
        };

        // tuple = (key, val); result.push(tuple)
        let tuple_local = self.fresh(tuple_ty);
        self.emit_assign(
            Place::local(tuple_local),
            Rvalue::Aggregate {
                kind: crate::ir::AggregateKind::Tuple,
                operands: vec![
                    Operand::Copy(Place::local(key_local)),
                    Operand::Copy(Place::local(val_local)),
                ],
            },
            span,
        );
        let push_dest = self.fresh(unit_ty);
        let after_push = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_push".to_string())),
            args: vec![
                Operand::Copy(Place::local(result_vec)),
                Operand::Copy(Place::local(tuple_local)),
            ],
            destination: Place::local(push_dest),
            target: Some(after_push),
        });
        self.set_current(after_push);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        let one = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        let bumped = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(bumped),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Copy(Place::local(bumped))),
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(result_vec)
    }
}

/// How a keyed-pairs loop reaches a map whose keys hash by value.
enum KeyedPairs {
    /// A struct or tuple key, keyed through its slot descriptor.
    Aggregate(String),
    /// A payload enum key, keyed through its equality descriptor.
    Enum(String),
}
