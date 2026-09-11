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

impl<'a> Builder<'a> {
    pub(crate) fn detect_string_bytes_iter<'h>(
        &self,
        iter_expr: &'h HirExpr,
    ) -> Option<&'h HirExpr> {
        use gossamer_types::TyKind;
        // Peel an outer `.iter()` if present.
        let cur = match &iter_expr.kind {
            HirExprKind::MethodCall { receiver, name, .. } if name.name == "iter" => receiver,
            _ => iter_expr,
        };
        // Direct `<x>.as_bytes()` / `<x>.bytes()`: both name the same byte
        // sequence, so the iteration walks the bytes of `<x>` directly rather
        // than a copy of them.
        if let HirExprKind::MethodCall { receiver, name, .. } = &cur.kind
            && matches!(name.name.as_str(), "as_bytes" | "bytes")
            && self.is_string_receiver(receiver)
        {
            return Some(receiver);
        }
        // Otherwise: a path / general expression whose value is a
        // String is also iterable byte-by-byte under the same
        // contract, since `as_bytes` is a runtime no-op (the
        // receiver IS the bytes - see the dispatch entry
        // `"as_bytes" => Some("")` in this file).
        let cur_ty = self
            .receiver_local_from_path(cur)
            .map_or(cur.ty, |local| self.locals[local.0 as usize].ty);
        let mut peeled = cur_ty;
        loop {
            match self.tcx.kind_of(peeled) {
                TyKind::String => return Some(cur),
                TyKind::Ref { inner, .. } => peeled = *inner,
                _ => return None,
            }
        }
    }

    pub(crate) fn is_string_receiver(&self, expr: &HirExpr) -> bool {
        use gossamer_types::TyKind;
        let mut ty = self
            .receiver_local_from_path(expr)
            .map_or(expr.ty, |local| self.locals[local.0 as usize].ty);
        loop {
            match self.tcx.kind_of(ty) {
                TyKind::String => return true,
                TyKind::Ref { inner, .. } => ty = *inner,
                _ => return false,
            }
        }
    }

    pub(crate) fn detect_tuple_iter<'h>(
        &self,
        iter_expr: &'h HirExpr,
    ) -> Option<(&'h HirExpr, Vec<Ty>)> {
        use gossamer_types::TyKind;
        let cur = match &iter_expr.kind {
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if name.name == "iter" && args.is_empty() => receiver.as_ref(),
            _ => iter_expr,
        };
        let mut ty = self
            .receiver_local_from_path(cur)
            .map_or(cur.ty, |local| self.locals[local.0 as usize].ty);
        loop {
            match self.tcx.kind_of(ty) {
                TyKind::Tuple(elems) => return Some((cur, elems.clone())),
                TyKind::Ref { inner, .. } => ty = *inner,
                _ => return None,
            }
        }
    }

    pub(crate) fn lower_for_string_bytes(
        &mut self,
        string_expr: &HirExpr,
        loop_pat: &HirPat,
        body: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);

        let s_local = self.lower_expr(string_expr)?;

        // The cursor reads a byte at a byte offset, so the bound is the
        // string's byte length: a `String` counts Unicode scalars, and any
        // multi-byte one has fewer of those than it has bytes.
        let len_local = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_str_byte_len".to_string())),
            args: vec![Operand::Copy(Place::local(s_local))],
            destination: Place::local(len_local),
            target: Some(next),
        });
        self.set_current(next);

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
        // byte = gos_rt_str_byte_at(s, counter)
        let byte_local = self.fresh(i64_ty);
        let after_byte = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_str_byte_at".to_string())),
            args: vec![
                Operand::Copy(Place::local(s_local)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(byte_local),
            target: Some(after_byte),
        });
        self.set_current(after_byte);
        if let HirPatKind::Binding { name, .. } = &loop_pat.kind {
            self.bind_local(&name.name, byte_local);
        }
        let regioned = self.begin_loop_region(body, span);
        self.loop_stack.push(LoopContext {
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(body);
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
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(self.lower_unit(span))
    }

    pub(crate) fn lower_for_enumerate(
        &mut self,
        inner: &HirExpr,
        idx_pat: &HirPat,
        val_pat: &HirPat,
        body: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);

        let inner_local = self.lower_expr(inner)?;

        // Detect literal-length array vs runtime vec, and recover
        // the element type from whichever side has it concrete.
        let mut arr_len: Option<i64> = None;
        let mut found_elem: Option<Ty> = None;
        let mut peeled = self.locals[inner_local.0 as usize].ty;
        loop {
            match self.tcx.kind_of(peeled) {
                TyKind::Array { len, elem } => {
                    arr_len = i64::try_from(len.to_usize()).ok();
                    found_elem = Some(*elem);
                    break;
                }
                TyKind::Vec(elem) | TyKind::Slice(elem) => {
                    found_elem = Some(*elem);
                    break;
                }
                TyKind::Ref { inner, .. } => peeled = *inner,
                _ => break,
            }
        }
        if arr_len.is_none() {
            if let HirExprKind::Array(arr) = &inner.kind {
                arr_len = match arr {
                    gossamer_hir::HirArrayExpr::List(elems) => Some(elems.len() as i64),
                    gossamer_hir::HirArrayExpr::Repeat { count, .. } => {
                        literal_u64(count).and_then(|c| i64::try_from(c).ok())
                    }
                };
            }
        }
        let mut elem_ty = found_elem.unwrap_or(val_pat.ty);
        if matches!(self.tcx.kind_of(elem_ty), TyKind::Var(_) | TyKind::Error) {
            elem_ty = i64_ty;
        }
        let array_mode = arr_len.is_some();

        let len_local = if array_mode {
            let l = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(l),
                Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                    arr_len.unwrap_or(0),
                )))),
                span,
            );
            l
        } else {
            let l = self.fresh(i64_ty);
            let after = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
                args: vec![Operand::Copy(Place::local(inner_local))],
                destination: Place::local(l),
                target: Some(after),
            });
            self.set_current(after);
            l
        };

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

        // Bind idx_pat to a copy of the counter - the binding is a
        // separate slot so the body can shadow / reassign without
        // perturbing the loop counter.
        if let HirPatKind::Binding { name, mutable } = &idx_pat.kind {
            let bind_local = self.push_local(i64_ty, Some(name.clone()), *mutable);
            self.bind_local(&name.name, bind_local);
            self.emit_assign(
                Place::local(bind_local),
                Rvalue::Use(Operand::Copy(Place::local(counter))),
                span,
            );
        }

        // Bind val_pat to the current element. Two shapes: literal
        // array (Index projection) vs runtime vec (get_ptr + load).
        if array_mode {
            if let HirPatKind::Binding { name, mutable } = &val_pat.kind {
                let bind_local = self.push_local(elem_ty, Some(name.clone()), *mutable);
                self.bind_local(&name.name, bind_local);
                let indexed = Place {
                    local: inner_local,
                    projection: vec![crate::ir::Projection::Index(counter)],
                };
                self.emit_assign(
                    Place::local(bind_local),
                    Rvalue::Use(Operand::Copy(indexed)),
                    span,
                );
            }
        } else {
            let ptr_local = self.fresh(i64_ty);
            let after_ptr = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr".to_string())),
                args: vec![
                    Operand::Copy(Place::local(inner_local)),
                    Operand::Copy(Place::local(counter)),
                ],
                destination: Place::local(ptr_local),
                target: Some(after_ptr),
            });
            self.set_current(after_ptr);
            if let HirPatKind::Binding { name, mutable } = &val_pat.kind {
                let bind_local = self.push_local(elem_ty, Some(name.clone()), *mutable);
                self.bind_local(&name.name, bind_local);
                if self.is_inline_aggregate_ty(elem_ty) {
                    // A tuple/struct Vec element occupies several inline slots.
                    // `gos_load` only returns one word, so loading it as a
                    // whole silently truncates the aggregate. Keep the slot
                    // address instead, matching `lower_for_vec_over_local`;
                    // later field projections read their own offsets.
                    self.emit_assign(
                        Place::local(bind_local),
                        Rvalue::Use(Operand::Copy(Place::local(ptr_local))),
                        span,
                    );
                } else {
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
                        destination: Place::local(bind_local),
                        target: Some(after_load),
                    });
                    self.set_current(after_load);
                }
            }
        }

        let regioned = self.begin_loop_region(body, span);
        self.loop_stack.push(LoopContext {
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(body);
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
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(self.lower_unit(span))
    }

    pub(crate) fn lower_for_vec(
        &mut self,
        iter_expr: &HirExpr,
        elem_ty: Ty,
        loop_pat: &HirPat,
        body: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        let yields_mut_refs = matches!(
            self.tcx.kind_of(iter_expr.ty),
            gossamer_types::TyKind::Ref {
                mutability: gossamer_types::Mutbl::Mut,
                inner,
            } if matches!(
                self.tcx.kind_of(*inner),
                gossamer_types::TyKind::Vec(_)
                    | gossamer_types::TyKind::Slice(_)
                    | gossamer_types::TyKind::Array { .. }
            )
        );
        let iter_local = self.lower_expr(iter_expr)?;
        self.lower_for_vec_over_local(iter_local, elem_ty, loop_pat, body, span, yields_mut_refs)
    }

    /// Iterates an already-lowered `GosVec` local (`iter_local`) by index,
    /// binding `loop_pat` per element. Shared by `lower_for_vec` (which
    /// lowers the iterand first) and the `HashSet` for-loop path (which
    /// snapshots the set to a Vec before iterating).
    pub(crate) fn lower_for_vec_over_local(
        &mut self,
        iter_local: Local,
        elem_ty: Ty,
        loop_pat: &HirPat,
        body: &HirExpr,
        span: Span,
        yields_mut_refs: bool,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);

        // len = gos_rt_vec_len(vec)
        let len_local = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(iter_local))],
            destination: Place::local(len_local),
            target: Some(next),
        });
        self.set_current(next);

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
        // The element slot address - needed only by the tuple-destructure body
        // below, which reads each field via `gos_load(slot, i*8)`. Set on the
        // `gos_rt_vec_get_ptr` path; `None` for the by-value `i128` path.
        let mut tuple_slot_ptr: Option<Local> = None;
        // A by-value `Result`/`Option` element is a 16-byte `i128` read
        // directly into the loop var - not via `gos_rt_vec_get_ptr` (which
        // would bind the slot address and let `match` decode garbage) nor the
        // 8-byte `gos_load` (which drops the payload).
        let elem_local = if yields_mut_refs {
            let ref_ty = self.tcx.intern(gossamer_types::TyKind::Ref {
                mutability: gossamer_types::Mutbl::Mut,
                inner: elem_ty,
            });
            let ptr_local = self.fresh(ref_ty);
            let after_ptr = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr".to_string())),
                args: vec![
                    Operand::Copy(Place::local(iter_local)),
                    Operand::Copy(Place::local(counter)),
                ],
                destination: Place::local(ptr_local),
                target: Some(after_ptr),
            });
            self.set_current(after_ptr);
            // A heap-container element's slot holds a pointer, so a method
            // call on this binding has to load it; an assignment through the
            // binding still writes the slot. Scalar and inline-aggregate
            // elements are addressed directly and need no such load.
            if matches!(
                self.tcx.kind_of(elem_ty),
                gossamer_types::TyKind::Vec(_)
                    | gossamer_types::TyKind::Slice(_)
                    | gossamer_types::TyKind::HashMap { .. }
            ) {
                self.slot_ref_locals.insert(ptr_local);
            }
            ptr_local
        } else if matches!(
            self.tcx.kind_of(elem_ty),
            gossamer_types::TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
        ) {
            let l = self.fresh(elem_ty);
            let after = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_i128".to_string())),
                args: vec![
                    Operand::Copy(Place::local(iter_local)),
                    Operand::Copy(Place::local(counter)),
                ],
                destination: Place::local(l),
                target: Some(after),
            });
            self.set_current(after);
            l
        } else {
            let elem_is_aggregate = matches!(
                self.tcx.kind_of(elem_ty),
                gossamer_types::TyKind::Tuple(_)
                    | gossamer_types::TyKind::Adt { .. }
                    | gossamer_types::TyKind::Array { .. }
            );
            // A user struct stored inline in a vec is address-is-value: its
            // field projections deref off the slot pointer. This holds at any
            // width - a single-field struct occupies one 8-byte slot yet is
            // still address-is-value, so it must bind the slot pointer rather
            // than read the slot's bytes as a scalar. The opaque heap-blob
            // stdlib structs (`fs::DirInfo` and friends, `def.local` in the
            // `u32::MAX - 16 ..= u32::MAX` sentinel range) are `Box` handles
            // whose slot holds a pointer, so they stay on the by-value scalar
            // read; only genuine user structs take the slot-address path.
            let elem_is_struct_adt = matches!(
                self.tcx.kind_of(elem_ty),
                gossamer_types::TyKind::Adt { def, .. }
                    if def.local < u32::MAX - 16 && self.tcx.struct_field_tys(*def).is_some()
            );
            let elem_is_multislot = (elem_is_aggregate
                && self.tcx.elem_is_addressed_aggregate(elem_ty))
                || elem_is_struct_adt;
            // `f64` elements must be read as a float bit-pattern; everything
            // else single-slot (i64 / bool / char / String / heap-handle ptr)
            // reads through one `gos_rt_vec_get_i64`.
            let elem_is_float =
                matches!(self.tcx.kind_of(elem_ty), gossamer_types::TyKind::Float(_));
            if !elem_is_multislot && !elem_is_float {
                // Single-slot scalar: ONE `gos_rt_vec_get_i64` reads the 8-byte
                // slot directly, halving the per-element runtime calls vs
                // `gos_rt_vec_get_ptr` + `gos_load` (the hot path for
                // `for x in vec_of_scalars`, e.g. BFS adjacency iteration).
                //
                // The counter is a fresh `0..len` induction with `len =
                // gos_rt_vec_len(vec)` of this same iterated vec, and the
                // header only branches into the body while `counter < len`,
                // so the index is provably in `[0, len)` and the receiver
                // non-null at the read. For primitive non-pointer elements
                // (int/bool/char) that proof lets us emit the bounds-free
                // `gos_rt_vec_get_i64_unchecked`, which the LLVM tier inlines
                // branch-free. String / heap-handle-ptr elements keep the
                // checked reader: they carry RC/borrow semantics and the
                // unchecked variant is restricted to leave those paths alone.
                let elem_is_unchecked_scalar = matches!(
                    self.tcx.kind_of(elem_ty),
                    gossamer_types::TyKind::Int(_)
                        | gossamer_types::TyKind::Bool
                        | gossamer_types::TyKind::Char
                );
                let callee_name = if elem_is_unchecked_scalar {
                    "gos_rt_vec_get_i64_unchecked"
                } else {
                    "gos_rt_vec_get_i64"
                };
                let l = self.fresh(elem_ty);
                let after = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(callee_name.to_string())),
                    args: vec![
                        Operand::Copy(Place::local(iter_local)),
                        Operand::Copy(Place::local(counter)),
                    ],
                    destination: Place::local(l),
                    target: Some(after),
                });
                self.set_current(after);
                l
            } else {
                // ptr = gos_rt_vec_get_ptr(vec, counter); elem = *ptr.
                // Multi-slot inline aggregates bind the slot address (the body
                // walks fields via Field / TupleIndex projections); `f64`
                // single-slots load the float bit-pattern.
                let ptr_local = self.fresh(i64_ty);
                let after_ptr = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(iter_local)),
                        Operand::Copy(Place::local(counter)),
                    ],
                    destination: Place::local(ptr_local),
                    target: Some(after_ptr),
                });
                self.set_current(after_ptr);
                tuple_slot_ptr = Some(ptr_local);
                if elem_is_multislot {
                    let l = self.fresh(elem_ty);
                    self.emit_assign(
                        Place::local(l),
                        Rvalue::Use(Operand::Copy(Place::local(ptr_local))),
                        span,
                    );
                    l
                } else {
                    let l = self.fresh(elem_ty);
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
                        destination: Place::local(l),
                        target: Some(after_load),
                    });
                    self.set_current(after_load);
                    l
                }
            }
        };
        // Carry an opaque stdlib blob element tag (`fs::DirInfo` from `walk_dir`
        // / `list_dir`) onto the loop binding so `e.field` resolves to a
        // `gos_load(handle, idx * 8)` against the registered shape instead of
        // falling through to `gos_rt_json_get` (which reads the blob as a JSON
        // value and yields garbage natively). Restricted to the known blob
        // handles: tagging a plain user-struct / tuple / String loop var would
        // reroute its field access and its drop classification.
        if let Some(en) = self.local_elem_struct.get(&iter_local).cloned()
            && matches!(en.as_str(), "DirInfo" | "DirEntry")
        {
            self.local_struct.entry(elem_local).or_insert(en);
        }
        match &loop_pat.kind {
            HirPatKind::Binding { name, .. } => {
                self.bind_local(&name.name, elem_local);
            }
            HirPatKind::Tuple(sub_pats) => {
                // Destructure the tuple element: load each field
                // through `gos_load(ptr_local, i*8)`. Without this,
                // sub-bindings fall back to string literals at
                // resolve-time and `actual == expected` reaches
                // codegen as `bool(i8) == str(ptr)`.
                //
                // The tuple pattern is authoritative that the element is a
                // multi-slot aggregate, so a slot address must exist. The
                // scalar/`i128`-by-value fast paths above never set one
                // (they classify by the element type, which can under-widen
                // to the single-slot fallback when it is left unresolved).
                // Materialize the slot pointer here from the same
                // `gos_rt_vec_get_ptr(vec, counter)` the multi-slot path
                // uses, so field reads always have a base to load from.
                let slot_ptr = match tuple_slot_ptr {
                    Some(p) => p,
                    None => {
                        let p = self.fresh(i64_ty);
                        let after_ptr = self.new_block(span);
                        self.terminate(Terminator::Call {
                            callee: Operand::Const(ConstValue::Str(
                                "gos_rt_vec_get_ptr".to_string(),
                            )),
                            args: vec![
                                Operand::Copy(Place::local(iter_local)),
                                Operand::Copy(Place::local(counter)),
                            ],
                            destination: Place::local(p),
                            target: Some(after_ptr),
                        });
                        self.set_current(after_ptr);
                        p
                    }
                };
                let elem_kinds: Vec<Ty> = match self.tcx.kind_of(elem_ty) {
                    gossamer_types::TyKind::Tuple(elems) => elems.clone(),
                    _ => Vec::new(),
                };
                // Each part starts where the ones before it end, so the walk
                // carries a running byte offset rather than a per-index one: a
                // part that is itself an aggregate occupies several slots.
                let mut byte_off: i128 = 0;
                for (i, sub_pat) in sub_pats.iter().enumerate() {
                    let field_ty = if matches!(
                        self.tcx.kind_of(sub_pat.ty),
                        gossamer_types::TyKind::Var(_) | gossamer_types::TyKind::Error
                    ) {
                        elem_kinds
                            .get(i)
                            .copied()
                            .filter(|t| {
                                !matches!(
                                    self.tcx.kind_of(*t),
                                    gossamer_types::TyKind::Var(_) | gossamer_types::TyKind::Error
                                )
                            })
                            .unwrap_or(i64_ty)
                    } else {
                        sub_pat.ty
                    };
                    let field_bytes = i128::from(self.elem_bytes_of(field_ty).max(8));
                    let part_off = byte_off;
                    byte_off += field_bytes;
                    let HirPatKind::Binding { name, mutable } = &sub_pat.kind else {
                        continue;
                    };
                    let bind_local = self.push_local(field_ty, Some(name.clone()), *mutable);
                    self.bind_local(&name.name, bind_local);
                    // An inline aggregate part holds its words in the element's
                    // own slots, so the binding names their address and later
                    // field reads take their offsets from there. A single-word
                    // part is the word itself.
                    if self.is_inline_aggregate_ty(field_ty) {
                        self.emit_assign(
                            Place::local(bind_local),
                            Rvalue::BinaryOp {
                                op: BinOp::Add,
                                lhs: Operand::Copy(Place::local(slot_ptr)),
                                rhs: Operand::Const(ConstValue::Int(part_off)),
                            },
                            span,
                        );
                        continue;
                    }
                    let off_local = self.fresh(i64_ty);
                    self.emit_assign(
                        Place::local(off_local),
                        Rvalue::Use(Operand::Const(ConstValue::Int(part_off))),
                        span,
                    );
                    let after_load = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
                        args: vec![
                            Operand::Copy(Place::local(slot_ptr)),
                            Operand::Copy(Place::local(off_local)),
                        ],
                        destination: Place::local(bind_local),
                        target: Some(after_load),
                    });
                    self.set_current(after_load);
                }
            }
            _ => {}
        }
        let regioned = self.begin_loop_region(body, span);
        self.loop_stack.push(LoopContext {
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(body);
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
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(self.lower_unit(span))
    }

    /// `for x in set` over a bare `HashSet`: snapshot the set to a sorted
    /// `Vec<T>` (the same deterministic order `set.to_vec()` / `.iter()`
    /// produce on every tier) and iterate it. A `HashSet` handle is not a
    /// `GosVec`, so iterating it directly would read the set as a vec.
    pub(crate) fn lower_for_set(
        &mut self,
        set_expr: &HirExpr,
        elem_ty: Ty,
        sym: &str,
        descriptor: Option<String>,
        loop_pat: &HirPat,
        body: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        let set_local = self.lower_expr(set_expr)?;
        let vec_ty = self.tcx.intern(gossamer_types::TyKind::Vec(elem_ty));
        let vec_local = self.fresh(vec_ty);
        let mut args = vec![Operand::Copy(Place::local(set_local))];
        // A content-keyed snapshot rebuilds each element from its slot
        // descriptor, so the walk passes the same one the inserts used.
        if let Some(descriptor) = descriptor {
            args.push(Operand::Const(ConstValue::Str(descriptor)));
        }
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(sym.to_string())),
            args,
            destination: Place::local(vec_local),
            target: Some(next),
        });
        self.set_current(next);
        self.lower_for_vec_over_local(vec_local, elem_ty, loop_pat, body, span, false)
    }

    pub(crate) fn lower_for_range(
        &mut self,
        start: &HirExpr,
        end: Option<&HirExpr>,
        inclusive: bool,
        loop_pat: &HirPat,
        body: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy as It, TyKind};
        let start_local = self.lower_expr(start)?;
        let end_local = match end {
            Some(end) => Some(self.lower_expr(end)?),
            None => None,
        };
        // The loop counter's cranelift width must be concrete. Prefer
        // the MIR type picked by `lower_literal` for `start`; fall
        // back to i64 when neither HIR nor lowered MIR gave an
        // integer kind (unsuffixed literal, leaked inference var, …).
        let int_ty = {
            let start_mir_ty = self.locals[start_local.0 as usize].ty;
            let hir_kind = self.tcx.kind_of(start.ty);
            let mir_kind = self.tcx.kind_of(start_mir_ty);
            match hir_kind {
                TyKind::Int(_) => start.ty,
                _ => match mir_kind {
                    TyKind::Int(_) => start_mir_ty,
                    _ => self.tcx.int_ty(It::I64),
                },
            }
        };
        let counter = self.push_local(int_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Copy(Place::local(start_local))),
            span,
        );

        let header = self.new_block(span);
        let body_block = self.new_block(span);
        let step_block = self.new_block(span);
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        if let Some(end_local) = end_local {
            let bool_ty = self.tcx.bool_ty();
            let cmp = self.fresh(bool_ty);
            let op = if inclusive { BinOp::Le } else { BinOp::Lt };
            self.emit_assign(
                Place::local(cmp),
                Rvalue::BinaryOp {
                    op,
                    lhs: Operand::Copy(Place::local(counter)),
                    rhs: Operand::Copy(Place::local(end_local)),
                },
                span,
            );
            self.terminate(Terminator::SwitchInt {
                discriminant: Operand::Copy(Place::local(cmp)),
                arms: vec![(0, exit)],
                default: body_block,
            });
        } else {
            self.terminate(Terminator::Goto { target: body_block });
        }

        self.set_current(body_block);
        self.push_scope();
        if let HirPatKind::Binding { name, mutable } = &loop_pat.kind {
            let bind_local = self.push_local(int_ty, Some(name.clone()), *mutable);
            self.bind_local(&name.name, bind_local);
            self.emit_assign(
                Place::local(bind_local),
                Rvalue::Use(Operand::Copy(Place::local(counter))),
                span,
            );
        }
        // Auto-region: a `for i in a..b` body whose allocations provably die
        // at the iteration boundary is wrapped in an arena region, so the
        // iteration's heap is bulk-freed at the back-edge instead of a
        // per-node refcount teardown - the same optimisation `lower_while`
        // applies.
        let regioned = self.begin_loop_region(body, span);
        // `continue` skips the rest of the body but must still
        // advance the counter, so it lands on `step_block`, not
        // on `header` directly. `break` exits the loop entirely.
        self.loop_stack.push(LoopContext {
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(body);
        self.loop_stack.pop();
        self.pop_scope();
        self.end_auto_region(regioned, span);
        self.terminate(Terminator::Goto { target: step_block });

        self.set_current(step_block);
        let one = self.fresh(int_ty);
        self.emit_assign(
            Place::local(one),
            Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            span,
        );
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(self.lower_unit(span))
    }

    pub(crate) fn lower_for_array(
        &mut self,
        iter_expr: &HirExpr,
        loop_pat: &HirPat,
        body: &HirExpr,
        array_len: i64,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let array_local = self.lower_expr(iter_expr)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let len_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(len_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(array_len)))),
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
        // Derive the element type from the array when the HIR-side
        // `loop_pat.ty` is unresolved (a leaked `Var(...)`). Without
        // this, `render_ty(Var) = "ptr"` and the codegen emits
        // `load ptr` against an i64 array element + `store i64 <ptr>`
        // - a type mismatch that makes opt complain and silently
        // misroutes through the Cranelift fallback path.
        let mut elem_ty = loop_pat.ty;
        let needs_pin = matches!(self.tcx.kind_of(elem_ty), TyKind::Var(_) | TyKind::Error);
        if needs_pin {
            let mut candidates = vec![self.locals[array_local.0 as usize].ty, iter_expr.ty];
            while let Some(cand) = candidates.pop() {
                let mut peeled = cand;
                loop {
                    match self.tcx.kind_of(peeled) {
                        TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => {
                            elem_ty = *elem;
                            candidates.clear();
                            break;
                        }
                        TyKind::Ref { inner, .. } => peeled = *inner,
                        _ => break,
                    }
                }
            }
            if matches!(self.tcx.kind_of(elem_ty), TyKind::Var(_) | TyKind::Error) {
                elem_ty = i64_ty;
            }
        }
        match &loop_pat.kind {
            HirPatKind::Binding { name, mutable } => {
                let bind_local = self.push_local(elem_ty, Some(name.clone()), *mutable);
                self.bind_local(&name.name, bind_local);
                let indexed_place = Place {
                    local: array_local,
                    projection: vec![crate::ir::Projection::Index(counter)],
                };
                let binding_value =
                    if let TyKind::Ref { mutability, .. } = self.tcx.kind_of(elem_ty) {
                        Rvalue::Ref {
                            place: indexed_place,
                            mutable: matches!(mutability, gossamer_types::Mutbl::Mut),
                        }
                    } else {
                        Rvalue::Use(Operand::Copy(indexed_place))
                    };
                self.emit_assign(Place::local(bind_local), binding_value, span);
            }
            HirPatKind::Tuple(sub_pats) => {
                // Bind each tuple sub-pattern to its own local that
                // reads `array[counter].i`. Avoids materialising the
                // whole tuple into a scalar local - `cl_type_of` of a
                // tuple is `ptr_ty`, so the existing single-binding
                // path would only copy the first 8-byte slot.
                let elem_kinds: Vec<Ty> = match self.tcx.kind_of(elem_ty) {
                    TyKind::Tuple(elems) => elems.clone(),
                    _ => Vec::new(),
                };
                for (i, sub_pat) in sub_pats.iter().enumerate() {
                    let HirPatKind::Binding { name, mutable } = &sub_pat.kind else {
                        continue;
                    };
                    // Pick the most concrete field type available:
                    // sub_pat.ty (HIR-typeck output) → tuple element
                    // type from elem_kinds → i64 fallback. Either of
                    // the first two can leak `Var(...)` if inference
                    // didn't propagate through the array literal,
                    // so resolve them in turn.
                    let field_ty =
                        if matches!(self.tcx.kind_of(sub_pat.ty), TyKind::Var(_) | TyKind::Error) {
                            elem_kinds
                                .get(i)
                                .copied()
                                .filter(|t| {
                                    !matches!(self.tcx.kind_of(*t), TyKind::Var(_) | TyKind::Error)
                                })
                                .unwrap_or(i64_ty)
                        } else {
                            sub_pat.ty
                        };
                    let bind_local = self.push_local(field_ty, Some(name.clone()), *mutable);
                    self.bind_local(&name.name, bind_local);
                    let projected = Place {
                        local: array_local,
                        projection: vec![
                            crate::ir::Projection::Index(counter),
                            crate::ir::Projection::Field(i as u32),
                        ],
                    };
                    self.emit_assign(
                        Place::local(bind_local),
                        Rvalue::Use(Operand::Copy(projected)),
                        span,
                    );
                }
            }
            _ => {}
        }
        let regioned = self.begin_loop_region(body, span);
        self.loop_stack.push(LoopContext {
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(body);
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
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(self.lower_unit(span))
    }

    pub(crate) fn lower_for_tuple(
        &mut self,
        iter_expr: &HirExpr,
        elem_tys: &[Ty],
        loop_pat: &HirPat,
        body: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let tuple_local = self.lower_expr(iter_expr)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let exit = self.new_block(span);
        let label = self.pending_loop_label.take();

        for (idx, elem_ty) in elem_tys.iter().copied().enumerate() {
            let next = self.new_block(span);
            self.push_scope();
            match &loop_pat.kind {
                HirPatKind::Binding { name, mutable } => {
                    let bind_local = self.push_local(elem_ty, Some(name.clone()), *mutable);
                    self.bind_local(&name.name, bind_local);
                    let projected = Place {
                        local: tuple_local,
                        projection: vec![crate::ir::Projection::Field(idx as u32)],
                    };
                    self.emit_assign(
                        Place::local(bind_local),
                        Rvalue::Use(Operand::Copy(projected)),
                        span,
                    );
                }
                HirPatKind::Tuple(sub_pats) => {
                    let field_tys: Vec<Ty> = match self.tcx.kind_of(elem_ty) {
                        TyKind::Tuple(fields) => fields.clone(),
                        _ => Vec::new(),
                    };
                    for (field_idx, sub_pat) in sub_pats.iter().enumerate() {
                        let HirPatKind::Binding { name, mutable } = &sub_pat.kind else {
                            continue;
                        };
                        let field_ty = if matches!(
                            self.tcx.kind_of(sub_pat.ty),
                            TyKind::Var(_) | TyKind::Error
                        ) {
                            field_tys
                                .get(field_idx)
                                .copied()
                                .filter(|ty| {
                                    !matches!(self.tcx.kind_of(*ty), TyKind::Var(_) | TyKind::Error)
                                })
                                .unwrap_or(i64_ty)
                        } else {
                            sub_pat.ty
                        };
                        let bind_local = self.push_local(field_ty, Some(name.clone()), *mutable);
                        self.bind_local(&name.name, bind_local);
                        let projected = Place {
                            local: tuple_local,
                            projection: vec![
                                crate::ir::Projection::Field(idx as u32),
                                crate::ir::Projection::Field(field_idx as u32),
                            ],
                        };
                        self.emit_assign(
                            Place::local(bind_local),
                            Rvalue::Use(Operand::Copy(projected)),
                            span,
                        );
                    }
                }
                _ => {}
            }
            let regioned = self.begin_loop_region(body, span);
            self.loop_stack.push(LoopContext {
                continue_to: next,
                break_to: exit,
                result: None,
                break_used: false,
                defer_depth: self.defer_stack.len(),
                label: label.clone(),
            });
            let _ = self.lower_expr(body);
            self.loop_stack.pop();
            self.pop_scope();
            self.end_auto_region(regioned, span);
            self.terminate(Terminator::Goto { target: next });
            self.set_current(next);
        }

        self.terminate(Terminator::Goto { target: exit });
        self.set_current(exit);
        Some(self.lower_unit(span))
    }

    pub(crate) fn lower_for_json(
        &mut self,
        iter_expr: &HirExpr,
        loop_pat: &HirPat,
        body: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let json_ty = self.tcx.json_value_ty();

        let iter_local = self.lower_expr(iter_expr)?;

        // len = gos_rt_json_len(iter)
        let len_local = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_json_len".to_string())),
            args: vec![Operand::Copy(Place::local(iter_local))],
            destination: Place::local(len_local),
            target: Some(next),
        });
        self.set_current(next);

        let counter = self.push_local(i64_ty, None, true);
        self.emit_assign(
            Place::local(counter),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );

        let header = self.new_block(span);
        let body_block = self.new_block(span);
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
        // elem = gos_rt_json_at(iter, counter)
        let elem_local = self.fresh(json_ty);
        let after_at = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_json_at".to_string())),
            args: vec![
                Operand::Copy(Place::local(iter_local)),
                Operand::Copy(Place::local(counter)),
            ],
            destination: Place::local(elem_local),
            target: Some(after_at),
        });
        self.set_current(after_at);
        if let HirPatKind::Binding { name, .. } = &loop_pat.kind {
            self.bind_local(&name.name, elem_local);
        }
        let step_block = self.new_block(span);
        let regioned = self.begin_loop_region(body, span);
        self.loop_stack.push(LoopContext {
            continue_to: step_block,
            break_to: exit,
            result: None,
            break_used: false,
            defer_depth: self.defer_stack.len(),
            label: self.pending_loop_label.take(),
        });
        let _ = self.lower_expr(body);
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
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(one)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(self.lower_unit(span))
    }

    pub(crate) fn lower_unit(&mut self, span: Span) -> Local {
        let unit_ty = self.tcx.unit();
        let local = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(local),
            Rvalue::Use(Operand::Const(ConstValue::Unit)),
            span,
        );
        local
    }
}
