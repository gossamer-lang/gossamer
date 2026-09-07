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
    AssertMessage, BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place,
    Rvalue, Statement, StatementKind, Terminator, UnOp,
};

use super::*;

use super::Builder;

/// How a map's values are compared: the kind the runtime reads a value word
/// as, and the descriptor its content is folded by when the word stands for a
/// whole value.
struct MapValueCompare {
    kind: u8,
    desc: Option<String>,
}

impl<'a> Builder<'a> {
    /// Coerces a source value to the function's declared return ABI shape.
    /// Both explicit `return value` and an implicit tail expression must pass
    /// through this single path so their caller-facing representations agree.
    pub(crate) fn coerce_return_value(
        &mut self,
        mut local: Local,
        ret_ty: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let value_ty = self.locals[local.0 as usize].ty;
        let dest_callable = matches!(
            self.tcx.kind_of(ret_ty),
            TyKind::FnPtr(_) | TyKind::FnTrait(_)
        );
        let src_is_fn_def = matches!(self.tcx.kind_of(value_ty), TyKind::FnDef { .. });
        let src_names_fn = self.local_fn_name.contains_key(&local);
        if dest_callable && (src_is_fn_def || src_names_fn) {
            local = self.coerce_to_fn_trait_if_needed(local, ret_ty, span);
        }

        if let TyKind::Array { elem, len } = self.tcx.kind_of(value_ty).clone() {
            let target_elem = match self.tcx.kind_of(ret_ty) {
                TyKind::Vec(elem) | TyKind::Slice(elem) => Some(*elem),
                _ => None,
            };
            if target_elem == Some(elem) {
                local = self.coerce_array_to_vec(local, elem, len, span);
            }
        }
        local
    }

    pub(crate) fn lower_expr(&mut self, expr: &HirExpr) -> Option<Local> {
        match &expr.kind {
            HirExprKind::Literal(lit) => Some(self.lower_literal(lit, expr.ty, expr.span)),
            HirExprKind::Path { segments, def, .. } => {
                self.lower_path(segments, *def, expr.ty, expr.span)
            }
            HirExprKind::Unary { op, operand } => {
                self.lower_unary(*op, operand, expr.ty, expr.span)
            }
            HirExprKind::Binary { op, lhs, rhs } => {
                self.lower_binary(*op, lhs, rhs, expr.ty, expr.span)
            }
            HirExprKind::Assign { place, value } => {
                self.lower_assign(place, value, expr.span);
                Some(self.lower_unit(expr.span))
            }
            HirExprKind::Call { callee, args } => self.lower_call(callee, args, expr.ty, expr.span),
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.lower_if(
                condition,
                then_branch,
                else_branch.as_deref(),
                expr.ty,
                expr.span,
            ),
            HirExprKind::While {
                condition,
                body,
                label,
            } => {
                self.pending_loop_label.clone_from(label);
                self.lower_while(condition, body, expr.span);
                Some(self.lower_unit(expr.span))
            }
            HirExprKind::Loop { body, label } => {
                self.pending_loop_label.clone_from(label);
                self.lower_loop(body, expr.ty, expr.span)
            }
            HirExprKind::Block(block) => self.lower_block(block),
            HirExprKind::Return(value) => {
                if let Some(value) = value {
                    if let Some(local) = self.lower_expr(value) {
                        let ret_ty = self.locals[Local::RETURN.0 as usize].ty;
                        let local = self.coerce_return_value(local, ret_ty, expr.span);
                        self.emit_assign(
                            Place::local(Local::RETURN),
                            Rvalue::Use(Operand::Copy(Place::local(local))),
                            expr.span,
                        );
                    }
                }
                // `return` leaves every enclosing block: run all pending defer
                // frames (LIFO, innermost first) after the return value is
                // computed, before the actual Return.
                self.emit_defers_above(0);
                self.terminate(Terminator::Return);
                None
            }
            HirExprKind::Break { value, label } => {
                // Jump to the target loop's break block - the labelled
                // loop when `'l` is given, otherwise the innermost.
                // Outside a loop (or an unknown label) the resolver is
                // supposed to reject this; if it slips through, fall
                // back to `Unreachable` rather than a dangling jump.
                let Some(idx) = self.resolve_loop_target(label.as_deref()) else {
                    self.terminate(Terminator::Unreachable);
                    return None;
                };
                self.loop_stack[idx].break_used = true;
                let break_to = self.loop_stack[idx].break_to;
                let result_local = self.loop_stack[idx].result;
                let defer_depth = self.loop_stack[idx].defer_depth;
                if let (Some(value), Some(result)) = (value, result_local) {
                    if let Some(value_local) = self.lower_expr(value) {
                        self.emit_assign(
                            Place::local(result),
                            Rvalue::Use(Operand::Copy(Place::local(value_local))),
                            expr.span,
                        );
                    }
                }
                // Run the defers of the blocks being exited (loop body and any
                // nested blocks), but not the loop's enclosing frames.
                self.emit_defers_above(defer_depth);
                self.terminate(Terminator::Goto { target: break_to });
                None
            }
            HirExprKind::Continue { label } => {
                if let Some(idx) = self.resolve_loop_target(label.as_deref()) {
                    let continue_to = self.loop_stack[idx].continue_to;
                    let defer_depth = self.loop_stack[idx].defer_depth;
                    self.emit_defers_above(defer_depth);
                    self.terminate(Terminator::Goto {
                        target: continue_to,
                    });
                } else {
                    self.terminate(Terminator::Unreachable);
                }
                None
            }
            HirExprKind::Tuple(elems) => self.lower_tuple(elems, expr.ty, expr.span),
            HirExprKind::Array(gossamer_hir::HirArrayExpr::List(elems)) => {
                self.lower_array_list(elems, expr.ty, expr.span)
            }
            HirExprKind::Array(gossamer_hir::HirArrayExpr::Repeat { value, count }) => {
                self.lower_array_repeat(value, count, expr.ty, expr.span)
            }
            HirExprKind::TupleIndex { receiver, index } => {
                self.lower_tuple_index(receiver, *index, expr.ty, expr.span)
            }
            HirExprKind::Index { base, index } => {
                self.lower_index_access(base, index, expr.ty, expr.span)
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.lower_match(scrutinee, arms, expr.ty, expr.span)
            }
            HirExprKind::Cast { value, ty: target } => {
                self.lower_cast(value, *target, expr.ty, expr.span)
            }
            HirExprKind::Field { receiver, name } => {
                self.lower_field_access(receiver, name, expr.ty, expr.span)
            }
            HirExprKind::LiftedClosure { name, captures } => {
                self.lower_lifted_closure(name, captures, expr.ty, expr.span)
            }
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                owner,
            } => self.lower_method_call(receiver, name, args, expr.ty, expr.span, owner.as_ref()),
            HirExprKind::Select { arms } => {
                // Real multiplexing via the runtime select builder. The arms
                // are registered in source order; `gos_rt_select_wait` polls
                // ready communications in pseudo-random order and parks the
                // goroutine until one is ready unless a default arm exists.
                // The recv payload rides the same
                // 8-byte word contract as `gos_rt_chan_recv_option`.
                use gossamer_hir::{HirPatKind, HirSelectOp};
                let span = expr.span;
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let unit_ty = self.tcx.unit();

                let n = arms.len() as i128;
                let builder = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(builder),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_select_new",
                        args: vec![Operand::Const(ConstValue::Int(n))],
                    },
                    span,
                );
                for arm in arms {
                    let d = self.fresh(unit_ty);
                    match &arm.op {
                        HirSelectOp::Recv { channel, .. } => {
                            let ch = self.lower_expr(channel)?;
                            self.emit_assign(
                                Place::local(d),
                                Rvalue::CallIntrinsic {
                                    name: "gos_rt_select_arm_recv",
                                    args: vec![
                                        Operand::Copy(Place::local(builder)),
                                        Operand::Copy(Place::local(ch)),
                                    ],
                                },
                                span,
                            );
                        }
                        HirSelectOp::Send { channel, value } => {
                            let ch = self.lower_expr(channel)?;
                            let v = self.lower_expr(value)?;
                            self.emit_assign(
                                Place::local(d),
                                Rvalue::CallIntrinsic {
                                    name: "gos_rt_select_arm_send",
                                    args: vec![
                                        Operand::Copy(Place::local(builder)),
                                        Operand::Copy(Place::local(ch)),
                                        Operand::Copy(Place::local(v)),
                                    ],
                                },
                                span,
                            );
                        }
                        HirSelectOp::Default => {
                            self.emit_assign(
                                Place::local(d),
                                Rvalue::CallIntrinsic {
                                    name: "gos_rt_select_arm_default",
                                    args: vec![Operand::Copy(Place::local(builder))],
                                },
                                span,
                            );
                        }
                    }
                }
                let idx = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(idx),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_select_wait",
                        args: vec![Operand::Copy(Place::local(builder))],
                    },
                    span,
                );
                let recv_val = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(recv_val),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_select_value",
                        args: vec![Operand::Copy(Place::local(builder))],
                    },
                    span,
                );
                let freed = self.fresh(unit_ty);
                self.emit_assign(
                    Place::local(freed),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_select_free",
                        args: vec![Operand::Copy(Place::local(builder))],
                    },
                    span,
                );

                let result = self.fresh(expr.ty);
                let join = self.new_block(span);
                let arm_blocks: Vec<BlockId> = arms.iter().map(|_| self.new_block(span)).collect();
                let switch_arms: Vec<(i128, BlockId)> = arm_blocks
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (i as i128, *b))
                    .collect();
                self.terminate(Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(idx)),
                    arms: switch_arms,
                    default: join,
                });
                for (arm, block) in arms.iter().zip(arm_blocks) {
                    self.set_current(block);
                    self.push_scope();
                    if let HirSelectOp::Recv { pattern, channel } = &arm.op {
                        match &pattern.kind {
                            HirPatKind::Binding { name, .. } => {
                                // `gos_rt_select_value` is the raw word the
                                // firing arm produced - a scalar, or a boxed
                                // pointer for a struct payload. Bind the name
                                // through a local typed as the channel's
                                // element so a struct payload box-derefs on
                                // field access instead of reading the pointer
                                // bits inline.
                                let mut ch_ty = channel.ty;
                                while let gossamer_types::TyKind::Ref { inner, .. } =
                                    self.tcx.kind_of(ch_ty)
                                {
                                    ch_ty = *inner;
                                }
                                let elem_ty = match self.tcx.kind_of(ch_ty) {
                                    gossamer_types::TyKind::Receiver(e) => *e,
                                    _ => i64_ty,
                                };
                                if elem_ty == i64_ty {
                                    self.bind_local(&name.name, recv_val);
                                } else {
                                    let bound = self.fresh(elem_ty);
                                    self.emit_assign(
                                        Place::local(bound),
                                        Rvalue::Use(Operand::Copy(Place::local(recv_val))),
                                        span,
                                    );
                                    self.bind_local(&name.name, bound);
                                }
                            }
                            HirPatKind::Wildcard => {}
                            _ => panic!(
                                "MIR lower: select recv arm has an unsupported pattern \
                                 shape; bind the received value to a name or `_`"
                            ),
                        }
                    }
                    if let Some(v) = self.lower_expr(&arm.body) {
                        use gossamer_types::TyKind;
                        let arm_ty = self.locals[v.0 as usize].ty;
                        let result_kind = self.tcx.kind_of(self.locals[result.0 as usize].ty);
                        let arm_kind = self.tcx.kind_of(arm_ty);
                        if matches!(result_kind, TyKind::Var(_) | TyKind::Error | TyKind::Never)
                            && !matches!(arm_kind, TyKind::Var(_) | TyKind::Error | TyKind::Never)
                        {
                            self.locals[result.0 as usize].ty = arm_ty;
                        }
                        self.emit_assign(
                            Place::local(result),
                            Rvalue::Use(Operand::Copy(Place::local(v))),
                            span,
                        );
                    }
                    if self.current.is_some() {
                        self.terminate(Terminator::Goto { target: join });
                    }
                    self.pop_scope();
                }
                self.set_current(join);
                Some(result)
            }
            HirExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                // Standalone ranges are lazy iterator state on every tier.
                // An omitted lower bound starts at zero; an omitted upper
                // bound remains an open-ended increasing iterator.
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let lo_local = if let Some(s) = start {
                    self.lower_expr(s)?
                } else {
                    let l = self.fresh(i64_ty);
                    self.emit_assign(
                        Place::local(l),
                        Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                        expr.span,
                    );
                    l
                };
                let hi_local = if let Some(e) = end {
                    self.lower_expr(e)?
                } else {
                    let l = self.fresh(i64_ty);
                    self.emit_assign(
                        Place::local(l),
                        Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(i64::MAX)))),
                        expr.span,
                    );
                    l
                };
                let dest = self.fresh(expr.ty);
                let next = self.new_block(expr.span);
                let (callee, args) = if end.is_none() {
                    (
                        "gos_rt_lazy_iter_range_from_i64",
                        vec![Operand::Copy(Place::local(lo_local))],
                    )
                } else if *inclusive {
                    (
                        "gos_rt_lazy_iter_range_inclusive_i64",
                        vec![
                            Operand::Copy(Place::local(lo_local)),
                            Operand::Copy(Place::local(hi_local)),
                        ],
                    )
                } else {
                    (
                        "gos_rt_lazy_iter_range_i64",
                        vec![
                            Operand::Copy(Place::local(lo_local)),
                            Operand::Copy(Place::local(hi_local)),
                        ],
                    )
                };
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(callee.to_string())),
                    args,
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                Some(dest)
            }
            // The native build pipeline runs `gossamer_hir::lift_closures`
            // upstream, so by the time we lower a Closure here we are
            // either in the VM's pre-JIT pass (which never executes the
            // resulting MIR - execution stays on the tree-walker) or
            // an unreachable path. Emit a zero-shaped placeholder so
            // pre-pass lowering succeeds without claiming to lower the
            // closure semantically. Same shape for the resolver's
            // `Placeholder` sentinel: parse / resolve diagnostics halt
            // the build before a real run, so any survivor reaches MIR
            // only on the VM's no-execute pre-pass.
            HirExprKind::Closure { .. } | HirExprKind::Placeholder => {
                let dest = self.fresh(expr.ty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    expr.span,
                );
                Some(dest)
            }
        }
    }

    pub(crate) fn lower_literal(&mut self, lit: &HirLiteral, ty: Ty, span: Span) -> Local {
        // Pin the literal's MIR type to the concrete kind the
        // literal implies, not the HIR expression's `ty` which may
        // still be an unresolved inference variable. Downstream
        // passes (string-concat detection, cranelift type
        // inference) rely on this being grounded.
        use gossamer_types::{FloatTy as Ft, IntTy as It, TyKind};
        let concrete = match lit {
            HirLiteral::String(_) => Some(self.tcx.string_ty()),
            HirLiteral::Bool(_) => Some(self.tcx.bool_ty()),
            HirLiteral::Char(_) => Some(self.tcx.char_ty()),
            HirLiteral::Unit => Some(self.tcx.unit()),
            _ => None,
        };
        let local_ty = match concrete {
            Some(concrete_ty) => concrete_ty,
            None => match self.tcx.kind_of(ty) {
                TyKind::Int(_) | TyKind::Float(_) => ty,
                _ => match lit {
                    HirLiteral::Int(_) => self.tcx.int_ty(It::I64),
                    HirLiteral::Float(_) => self.tcx.float_ty(Ft::F64),
                    _ => ty,
                },
            },
        };
        let local = self.fresh(local_ty);
        let value = literal_to_const(lit);
        self.emit_assign(
            Place::local(local),
            Rvalue::Use(Operand::Const(value)),
            span,
        );
        local
    }

    pub(crate) fn lower_path(
        &mut self,
        segments: &[Ident],
        def: Option<gossamer_resolve::DefId>,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        if let Some(first) = segments.first() {
            if let Some(local) = self.lookup_local(&first.name) {
                return Some(local);
            }
        }
        // A `static mut` read loads the live global cell rather than
        // inlining the declaration value.
        if let Some(def) = def
            && let Some(sref) = self.mut_statics.get(&def).cloned()
        {
            let local = self.fresh(sref.ty);
            self.emit_assign(Place::local(local), Rvalue::StaticLoad(sref), span);
            return Some(local);
        }
        // `None` as a value (no payload) lowers to a heap-allocated
        // `gos_rt_result_new(1, 0)` so the match disc check can
        // distinguish it from `Some(_)`.
        if let Some(last) = segments.last() {
            if last.name.as_str() == "None" && segments.len() == 1 {
                return self.lower_result_no_payload(1, ty, span);
            }
        }
        // Enum variant constructor (no-payload form): `Color::Green`
        // / `List::Nil`. When the enum has any payload-bearing
        // sibling, allocate a one-word `[disc]` heap aggregate so
        // match dispatch can uniformly load disc from offset 0.
        // Otherwise emit the variant index directly as i64.
        if let Some((enum_name, idx)) = self.enums.lookup(segments) {
            if self.enums.enum_has_any_payload(&enum_name) {
                let result = self.lower_user_enum_ctor(
                    &enum_name,
                    u32::try_from(idx).unwrap_or(0),
                    &[],
                    ty,
                    span,
                );
                // A unit variant of a payload-bearing enum is a tagged null,
                // which carries no type of its own; record the enum so `{:?}`
                // and `==` reach the derived methods.
                if let Some(local) = result {
                    self.local_struct.insert(local, enum_name);
                }
                return result;
            }
            let int_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let local = self.push_local(int_ty, None, false);
            self.emit_assign(
                Place::local(local),
                Rvalue::Use(Operand::Const(ConstValue::Int(idx as i128))),
                span,
            );
            // The value is a bare index, so record which enum it belongs to;
            // `{:?}` and `==` recover the type from this tag to reach the
            // derived methods.
            self.local_struct.insert(local, enum_name);
            return Some(local);
        }
        // `json::Value::Null` (path expression, no parens) is a unit
        // variant constructor for the stdlib `json::Value` enum.
        // The user-enum path above doesn't catch it because
        // `json::Value` isn't declared in the program - it lives in
        // `gossamer-std`. Without this arm the path falls through to
        // the FnRef fallback below, producing a function-pointer
        // value that downstream code interpreted as a `*mut GosJson`
        // and segfaulted on dereference (askq's
        // `json::parse(...).unwrap_or(json::Value::Null)`). Route
        // through the runtime constructor instead so the resulting
        // local holds a real, dereferenceable GosJson handle.
        let names: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
        let strip_std: &[&str] = if names.first() == Some(&"std") {
            &names[1..]
        } else {
            &names[..]
        };
        if let Some((int_ty, value)) = int_assoc_const(strip_std) {
            let ty = self.tcx.int_ty(int_ty);
            let local = self.fresh(ty);
            self.emit_assign(
                Place::local(local),
                Rvalue::Use(Operand::Const(ConstValue::Int(value))),
                span,
            );
            return Some(local);
        }
        let json_unit = matches!(
            (
                strip_std.first(),
                strip_std.get(1),
                strip_std.get(2),
                strip_std.last()
            ),
            (Some(&"json"), Some(&"Value"), Some(&"Null"), _)
                | (Some(&"encoding"), _, _, Some(&"Null"))
        ) && strip_std.last() == Some(&"Null")
            && (strip_std.len() == 3 || strip_std.len() == 4);
        if json_unit {
            let json_ty = self.tcx.json_value_ty();
            let dest = self.fresh(json_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_json_value_null".to_string())),
                args: Vec::new(),
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return Some(dest);
        }
        // `math::PI` / `math::E` / … are `f64` constants in
        // `gossamer-std`; the interpreter binds them as `Value::Float`
        // globals, but the compiled tiers never see a `const` def for
        // them, so the path would otherwise fall through to the
        // FnRef/string fallback below - printing the literal "math::PI"
        // and feeding a string-tag pointer into arithmetic. Inline the
        // IEEE value so every tier folds them identically.
        // `fs::SEEK_SET` / `SEEK_CUR` / `SEEK_END` name the `whence`
        // selector `File::seek` takes. The interpreter binds them as
        // integer globals; the compiled tiers see no `const` def, so
        // fold the value here for one meaning on every tier.
        if strip_std.len() == 2
            && strip_std[0] == "fs"
            && let Some(value) = seek_whence_value(strip_std[1])
        {
            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(local),
                Rvalue::Use(Operand::Const(ConstValue::Int(value))),
                span,
            );
            return Some(local);
        }
        if strip_std.len() == 2
            && strip_std[0] == "math"
            && let Some(bits) = math_const_bits(strip_std[1])
        {
            let f64_ty = self.tcx.float_ty(gossamer_types::FloatTy::F64);
            let local = self.fresh(f64_ty);
            self.emit_assign(
                Place::local(local),
                Rvalue::Use(Operand::Const(ConstValue::Float(bits))),
                span,
            );
            return Some(local);
        }
        // A `const` / immutable `static` whose initializer is heap-typed
        // (`Vec`, `Map`, `Set`, an aggregate) has no `ConstValue`
        // representation, so it never reaches `self.consts` below. Re-lower
        // its declaration expression at this reference site - the same
        // "instantiated afresh per use" semantics a scalar const gets via
        // `Operand::Const` - instead of falling through to the `FnRef` arm,
        // which misreads the heap value as a function pointer and hands
        // native/JIT code a bogus data pointer.
        if let Some(def) = def
            && self.consts.get(&def).is_none()
            && let Some(init) = self.const_inits.get(&def).cloned()
        {
            return self.lower_expr(&init);
        }
        // When the typechecker leaves a path-expr's type as `Var(_)`
        // - common for paths that resolve to `const` / `static`
        // items because the const-value pass runs after typeck -
        // pin the local's MIR type from the folded `ConstValue`'s
        // shape. Without this, the local stays `Var` and downstream
        // dispatch (operand_print_kind, format-helper selection)
        // falls through to a default that treats the value as a
        // c-string pointer; passing an integer there segfaults the
        // first strlen.
        let pinned_ty = if matches!(self.tcx.kind_of(ty), gossamer_types::TyKind::Var(_))
            && let Some(def) = def
            && let Some(value) = self.consts.get(&def)
        {
            match value {
                ConstValue::Int(_) => self.tcx.int_ty(gossamer_types::IntTy::I64),
                ConstValue::Float(_) => self.tcx.float_ty(gossamer_types::FloatTy::F64),
                ConstValue::Bool(_) => self.tcx.bool_ty(),
                ConstValue::Char(_) => self.tcx.char_ty(),
                ConstValue::Str(_) => self.tcx.string_ty(),
                ConstValue::Unit => ty,
            }
        } else {
            ty
        };
        let local = self.fresh(pinned_ty);
        let joined_name = segments
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        let operand = if let Some(def) = def {
            // A path that resolves to a top-level `const` item
            // inlines the literal value here. Without this, the
            // FnRef fallback below would treat the const like a
            // function pointer and the codegen would emit zero
            // (or a string-tag pointer) at every use site.
            if let Some(value) = self.consts.get(&def) {
                Operand::Const(value.clone())
            } else {
                // Path resolves to a named item. Record the joined
                // name when the result is function-shaped so a later
                // call site (closure env, router handler, etc.) can
                // round-trip back to the symbol via `gos_fn_addr`.
                if matches!(
                    self.tcx.kind_of(pinned_ty),
                    gossamer_types::TyKind::FnDef { .. }
                        | gossamer_types::TyKind::FnPtr(_)
                        | gossamer_types::TyKind::FnTrait(_)
                        | gossamer_types::TyKind::Closure { .. }
                ) {
                    self.local_fn_name.insert(local, joined_name.clone());
                }
                Operand::FnRef {
                    def,
                    substs: self.substs_of(pinned_ty),
                }
            }
        } else {
            // Record that `local` holds a function-name constant
            // so a later `let` binding + call can still dispatch
            // directly to the named function without treating
            // the local as a closure env pointer.
            //
            // A tabled std fn used as a value (`errors::new` passed
            // to `map_err`) is recorded under its runtime symbol -
            // the eta-expansion target the compiled tiers can take
            // the address of. The thunk machinery then forwards to
            // the C-ABI shim exactly like a lifted bare closure.
            let resolved_name = gossamer_types::std_fn_values::rt_symbol_for_std_fn(&joined_name)
                .map_or(joined_name, str::to_string);
            self.local_fn_name.insert(local, resolved_name.clone());
            Operand::Const(ConstValue::Str(resolved_name))
        };
        self.emit_assign(Place::local(local), Rvalue::Use(operand), span);
        Some(local)
    }

    pub(crate) fn lower_unary(
        &mut self,
        op: HirUnaryOp,
        operand: &HirExpr,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let runtime_projected_field = match &operand.kind {
            HirExprKind::Field { receiver, .. } => {
                let runtime_kind = self
                    .lower_place_expr(receiver)
                    .and_then(|place| self.local_runtime_kind.get(&place.local).copied());
                let receiver_ty = match self.tcx.kind_of(receiver.ty) {
                    TyKind::Ref { inner, .. } => *inner,
                    _ => receiver.ty,
                };
                matches!(runtime_kind, Some("http::Response" | "http::Request"))
                    || matches!(
                        self.struct_name_of(receiver_ty).as_deref(),
                        Some("Response" | "Request")
                    )
                    || matches!(
                        self.tcx.kind_of(receiver_ty),
                        TyKind::Adt { def, .. } if def.local >= u32::MAX - 64
                    )
            }
            _ => false,
        };
        // Heap-backed sequence and string values already are the pointer a
        // shared reference must carry. Read a place directly into the
        // reference-typed destination so borrowing a projected field does not
        // first create an owned temporary that drop insertion later releases.
        // That extra owner was especially harmful for `&record.vec_field`:
        // the temporary was never retained, so native cleanup over-released
        // the Vec header after repeated struct construction.
        // A place reached through a reference (`&*v`) is already the handle
        // the borrow must carry, so re-addressing it would hand over the
        // address of the reference instead of the value it names.
        if matches!(op, HirUnaryOp::RefShared)
            && matches!(
                self.tcx.kind_of(operand.ty),
                TyKind::Slice(_) | TyKind::Vec(_) | TyKind::HashMap { .. }
            )
            && !runtime_projected_field
            && let Some(place) = self.lower_place_expr(operand)
            && !place
                .projection
                .iter()
                .any(|p| matches!(p, crate::ir::Projection::Deref))
        {
            let dest = self.fresh(ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::Ref {
                    mutable: false,
                    place,
                },
                span,
            );
            return Some(dest);
        }
        if matches!(op, HirUnaryOp::RefMut)
            && let Some(place) = self.lower_place_expr(operand)
            && place.projection.is_empty()
            && matches!(
                self.tcx.kind_of(self.locals[place.local.0 as usize].ty),
                TyKind::Ref { .. }
            )
        {
            return Some(place.local);
        }
        // `&mut place.field` over an inline aggregate names the field's own
        // storage. Reading the field as a value first copies its slots into a
        // fresh local, and every write the callee makes would land on that
        // copy - the caller's struct would never see them.
        if matches!(op, HirUnaryOp::RefMut)
            && self.inline_aggregate_ty(operand.ty)
            && let Some(place) = self.lower_place_expr(operand)
            && !place.projection.is_empty()
        {
            let dest = self.fresh(ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::Ref {
                    mutable: true,
                    place,
                },
                span,
            );
            return Some(dest);
        }
        let inner = self.lower_expr(operand)?;
        // `-x` / `!x` on a user struct / enum route to its `neg` / `not`
        // impl method.
        if let Some(method) = match op {
            HirUnaryOp::Neg => Some("neg"),
            HirUnaryOp::Not => Some("not"),
            _ => None,
        } {
            let sname = self
                .adt_dispatch_name(operand.ty)
                .or_else(|| self.adt_dispatch_name(self.locals[inner.0 as usize].ty));
            if let Some(sname) = sname {
                let mangled = format!("{sname}::{method}");
                if self.impl_methods.contains_key(&mangled) {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(mangled)),
                        args: vec![Operand::Copy(Place::local(inner))],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
            }
        }
        let mir_op = match op {
            HirUnaryOp::Neg => UnOp::Neg,
            HirUnaryOp::Not => UnOp::Not,
            HirUnaryOp::RefShared | HirUnaryOp::RefMut => {
                // For aggregate-typed operands (Vec, String, HashMap,
                // struct, …) and opaque-handle Adts (Regex, SqlDb,
                // Channel - all stored as i64 locals carrying a
                // ptr-shaped value) the existing `inner` local already
                // holds the canonical pointer the callee expects, so
                // `&x` is a no-op.
                //
                // For `&mut`-on-named-place-of-scalar (i.e.
                // `&mut state` where `state: i64`), the callee
                // genuinely wants a pointer that lets it write back
                // - without this, deref-assign through the borrowed
                // ref lands on the value-as-ptr and segfaults. Emit
                // `Rvalue::Ref` so the backend pulls a real slot
                // address.
                //
                // We restrict the Rvalue::Ref path to `&mut` on
                // genuine place expressions (path / field / index /
                // nested deref) for SCALAR operands and for `String`.
                // A `String` is a flat `*mut c_char` (the pointer IS
                // the value, not a stable header like `GosVec`), so a
                // callee's `*s = v` / `*s += v` must land on the
                // caller's SLOT, not on a passed-by-value copy of the
                // pointer - hence the by-slot-address `Rvalue::Ref`,
                // exactly as for a scalar. The post-call reload in
                // `lower_call` pulls the callee's new pointer back into
                // the caller's local. Shared `&` on a literal or
                // temporary keeps the historical value-passthrough so
                // existing dispatch sites (e.g. `map.get(&k)` lowering
                // to `gos_rt_map_get_i64(m, k_value)`) continue to work.
                let scalar =
                    self.mut_ref_takes_slot_address(operand.ty, self.locals[inner.0 as usize].ty);
                let is_place_expr = matches!(
                    operand.kind,
                    HirExprKind::Path { .. }
                        | HirExprKind::Field { .. }
                        | HirExprKind::TupleIndex { .. }
                        | HirExprKind::Index { .. }
                        | HirExprKind::Unary {
                            op: HirUnaryOp::Deref,
                            ..
                        }
                );
                if !(scalar && matches!(op, HirUnaryOp::RefMut) && is_place_expr) {
                    return Some(inner);
                }
                let place = self
                    .lower_place_expr(operand)
                    .unwrap_or(Place::local(inner));
                let dest = self.fresh(ty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::Ref {
                        mutable: true,
                        place,
                    },
                    span,
                );
                return Some(dest);
            }
            HirUnaryOp::Deref => {
                let cell_kind = self.local_runtime_kind.get(&inner).copied();
                let (helper, dest_ty): (Option<&'static str>, Ty) = match cell_kind {
                    Some("flag::Cell::String") => {
                        (Some("gos_rt_flag_cell_load_str"), self.tcx.string_ty())
                    }
                    Some("flag::Cell::Int" | "flag::Cell::Uint" | "flag::Cell::Duration") => (
                        Some("gos_rt_flag_cell_load_i64"),
                        self.tcx.int_ty(gossamer_types::IntTy::I64),
                    ),
                    Some("flag::Cell::Bool") => {
                        (Some("gos_rt_flag_cell_load_bool"), self.tcx.bool_ty())
                    }
                    Some("flag::Cell::Float") => (
                        Some("gos_rt_flag_cell_load_f64"),
                        self.tcx.float_ty(gossamer_types::FloatTy::F64),
                    ),
                    Some("flag::Cell::StringList") => {
                        let s = self.tcx.string_ty();
                        (
                            Some("gos_rt_flag_cell_load_vec"),
                            self.tcx.intern(gossamer_types::TyKind::Vec(s)),
                        )
                    }
                    _ => (None, ty),
                };
                if let Some(helper_name) = helper {
                    let dest = self.fresh(dest_ty);
                    self.emit_assign(
                        Place::local(dest),
                        Rvalue::CallIntrinsic {
                            name: helper_name,
                            args: vec![Operand::Copy(Place::local(inner))],
                        },
                        span,
                    );
                    return Some(dest);
                }
                // Real reference deref: when the inner local has
                // type `&T` (taken via `&x` or yielded by an
                // iterator like `vec.iter()`), `*p` read as a value
                // must load from the address rather than yield the
                // pointer. Without this load, `for x in v.iter() { *x }`
                // prints the iterator's slot pointer, not the element.
                // A `&mut String` is a pointer-to-slot (double
                // indirection: the mutable ref must be able to rebind the
                // whole String via `*s = ...`), so `*s` as a value needs
                // one load to reach the String pointer, exactly like a
                // scalar pointee. An immutable `&String` already holds the
                // String pointer directly (single indirection) and so
                // stays the identity `return Some(inner)` below - its
                // caller-reference is minted by the return-copy retain.
                // Larger aggregates (structs, Vec, Adt) are consumed
                // through place projections, not this by-value path.
                let inner_ty = self.locals[inner.0 as usize].ty;
                if let gossamer_types::TyKind::Ref {
                    inner: pointee,
                    mutability,
                } = self.tcx.kind_of(inner_ty)
                {
                    let pointee = *pointee;
                    let mutability = *mutability;
                    let scalar_pointee = matches!(
                        self.tcx.kind_of(pointee),
                        gossamer_types::TyKind::Int(_)
                            | gossamer_types::TyKind::Float(_)
                            | gossamer_types::TyKind::Bool
                            | gossamer_types::TyKind::Char
                            | gossamer_types::TyKind::Duration
                            | gossamer_types::TyKind::Instant
                    );
                    // An aggregate pointee read as a value is a copy of
                    // what the reference names, the same copy
                    // `let taken = *p` makes. Answering the reference
                    // itself instead leaves the caller holding an alias:
                    // a container given one stores the address of a slot
                    // that is about to go away, and a one-word element
                    // store writes the pointer where the value belongs.
                    if self.aggregate_deref_copies(pointee) {
                        let dest = self.fresh(pointee);
                        let mut place = Place::local(inner);
                        place.projection.push(crate::ir::Projection::Deref);
                        self.emit_assign(
                            Place::local(dest),
                            Rvalue::Use(Operand::Copy(place)),
                            span,
                        );
                        return Some(dest);
                    }
                    let mut_string_pointee = mutability == gossamer_types::Mutbl::Mut
                        && matches!(self.tcx.kind_of(pointee), gossamer_types::TyKind::String);
                    if scalar_pointee || mut_string_pointee {
                        let zero_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                        let zero = self.fresh(zero_ty);
                        self.emit_assign(
                            Place::local(zero),
                            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                            span,
                        );
                        let dest = self.fresh(pointee);
                        self.emit_assign(
                            Place::local(dest),
                            Rvalue::CallIntrinsic {
                                name: "gos_load",
                                args: vec![
                                    Operand::Copy(Place::local(inner)),
                                    Operand::Copy(Place::local(zero)),
                                ],
                            },
                            span,
                        );
                        return Some(dest);
                    }
                }
                return Some(inner);
            }
        };
        // When the HIR type is unresolved (`Var(_)`) the unary
        // result inherits the operand's type - `!bool` is `bool`,
        // `-i64` is `i64`, etc. Without this fallback the
        // destination local is `Var`/ptr-shaped, and downstream
        // print kinds route the i1 result through `print_str`
        // (treating the bit as a string pointer) rather than
        // `print_bool`, segfaulting in `strlen` on `0x1`.
        let ty = if matches!(self.tcx.kind_of(ty), TyKind::Var(_) | TyKind::Error) {
            let inner_ty = self.locals[inner.0 as usize].ty;
            if matches!(
                self.tcx.kind_of(inner_ty),
                TyKind::Bool | TyKind::Int(_) | TyKind::Float(_) | TyKind::Char
            ) {
                inner_ty
            } else {
                ty
            }
        } else {
            ty
        };
        let local = self.fresh(ty);
        self.emit_assign(
            Place::local(local),
            Rvalue::UnaryOp {
                op: mir_op,
                operand: Operand::Copy(Place::local(inner)),
            },
            span,
        );
        Some(local)
    }

    /// Whether `*p` on a reference to `ty` answers a copy of the value
    /// rather than the reference.
    ///
    /// A struct, a tuple, or a fixed array is stored as a run of slots
    /// and copied by those slots. The heap containers are excluded: each
    /// is one managed handle, and copying the handle is what sharing it
    /// already means.
    fn aggregate_deref_copies(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            // A user struct; the sentinel `Adt`s are `Option` / `Result`
            // and the runtime handles, which travel as one word.
            TyKind::Adt { def, .. } => {
                def.local < u32::MAX - 16 && self.tcx.struct_field_tys(*def).is_some()
            }
            _ => false,
        }
    }

    /// Marks `value` (and its reachable RC subgraph) as escaped to
    /// another goroutine, so it switches to atomic reference counting
    /// and is excluded from the per-thread cycle collector. Emitted at
    /// escape points (`go f(args)`, `spawn` closure captures, channel
    /// `send`). No-op unless `value`'s static type is RC-managed, so the
    /// runtime helper never sees a scalar / non-RC pointer.
    pub(crate) fn emit_mark_shared_if_rc(&mut self, value: Local, span: Span) {
        let ty = self.locals[value.0 as usize].ty;
        // A map is not RC-managed (it is a bare runtime handle freed by
        // `gos_rt_map_free`), so it needs its own escape signal: marking
        // it shared flips it from the goroutine-local lock-free fast path
        // to the synchronized one before it is published to the spawned
        // goroutine / channel peer.
        let callee = if self.ty_is_hashmap(ty) {
            "gos_rt_map_mark_shared"
        } else if matches!(self.tcx.kind_of(ty), gossamer_types::TyKind::Vec(_)) {
            "gos_rt_vec_mark_shared"
        } else if self.tcx.is_rc_managed(ty) {
            "gos_rt_rc_mark_shared"
        } else {
            return;
        };
        let unit_ty = self.tcx.unit();
        let dest = self.fresh(unit_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(callee.to_string())),
            args: vec![Operand::Copy(Place::local(value))],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
    }

    /// True when `t` resolves to a `HashMap` (peeling references).
    fn ty_is_hashmap(&self, t: Ty) -> bool {
        let mut cur = t;
        loop {
            match self.tcx.kind_of(cur) {
                gossamer_types::TyKind::HashMap { .. } => return true,
                gossamer_types::TyKind::Ref { inner, .. } => cur = *inner,
                _ => return false,
            }
        }
    }

    /// True when `t` resolves to `String` (peeling references).
    fn ty_is_string(&self, t: Ty) -> bool {
        let mut cur = t;
        loop {
            match self.tcx.kind_of(cur) {
                gossamer_types::TyKind::String => return true,
                gossamer_types::TyKind::Ref { inner, .. } => cur = *inner,
                _ => return false,
            }
        }
    }

    /// If `lhs + rhs` is a String concatenation, returns the full left-nested
    /// chain's operands left-to-right; otherwise `None`. Detection uses HIR
    /// types only so it runs before the operands are lowered.
    fn str_concat_chain<'h>(
        &self,
        lhs: &'h HirExpr,
        rhs: &'h HirExpr,
        result_ty: Ty,
    ) -> Option<Vec<&'h HirExpr>> {
        if !(self.ty_is_string(result_ty) || self.ty_is_string(lhs.ty) || self.ty_is_string(rhs.ty))
        {
            return None;
        }
        let mut parts = Vec::new();
        self.collect_concat_operand(lhs, &mut parts);
        self.collect_concat_operand(rhs, &mut parts);
        Some(parts)
    }

    /// Recursively flattens a String `+` sub-expression into `parts`, or pushes
    /// `expr` itself when it is not a String `+`.
    fn collect_concat_operand<'h>(&self, expr: &'h HirExpr, parts: &mut Vec<&'h HirExpr>) {
        if let HirExprKind::Binary {
            op: HirBinaryOp::Add,
            lhs,
            rhs,
        } = &expr.kind
            && (self.ty_is_string(expr.ty)
                || self.ty_is_string(lhs.ty)
                || self.ty_is_string(rhs.ty))
        {
            self.collect_concat_operand(lhs, parts);
            self.collect_concat_operand(rhs, parts);
        } else {
            parts.push(expr);
        }
    }

    pub(crate) fn lower_binary(
        &mut self,
        op: HirBinaryOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        // Short-circuit `&&` / `||`. The pre-0.10.0 lowering called
        // `lower_expr` on BOTH sides up front, so any side-effecting
        // or out-of-bounds RHS fired even when the LHS already
        // determined the result. `while j > 0 && arr[j - 1] < si`
        // panicked with "index is -1" once j reached 0 because
        // `arr[j - 1]` evaluated unconditionally. Build a small
        // branch lattice (eval LHS, branch on it, eval RHS only on
        // the path that needs it, merge into a single result).
        if matches!(op, HirBinaryOp::And | HirBinaryOp::Or) {
            let bool_ty = self.tcx.bool_ty();
            let result = self.fresh(bool_ty);
            let lhs_local = self.lower_expr(lhs)?;
            let rhs_block = self.new_block(span);
            let short_block = self.new_block(span);
            let join_block = self.new_block(span);
            // For `&&`: LHS true → eval RHS; LHS false → short-circuit false.
            // For `||`: LHS true → short-circuit true; LHS false → eval RHS.
            let (true_target, false_target) = match op {
                HirBinaryOp::And => (rhs_block, short_block),
                HirBinaryOp::Or => (short_block, rhs_block),
                _ => unreachable!(),
            };
            self.terminate(Terminator::SwitchInt {
                discriminant: Operand::Copy(Place::local(lhs_local)),
                arms: vec![(0, false_target)],
                default: true_target,
            });
            // Short-circuit arm: write the LHS-determined constant.
            self.set_current(short_block);
            let short_value = i64::from(!matches!(op, HirBinaryOp::And));
            self.emit_assign(
                Place::local(result),
                Rvalue::Use(Operand::Const(ConstValue::Int(short_value as i128))),
                span,
            );
            self.terminate(Terminator::Goto { target: join_block });
            // RHS arm: evaluate the right operand and copy into result.
            self.set_current(rhs_block);
            let rhs_local = self.lower_expr(rhs)?;
            self.emit_assign(
                Place::local(result),
                Rvalue::Use(Operand::Copy(Place::local(rhs_local))),
                span,
            );
            self.terminate(Terminator::Goto { target: join_block });
            self.set_current(join_block);
            return Some(result);
        }
        // Fold a left-nested String `+` chain (`a + b + c + ...`) into one
        // n-ary `__concat` so the whole chain allocates a single result buffer
        // instead of one intermediate String per operator. This reuses the
        // same single-pass join `format!` lowers to, so it stays byte-identical
        // across every tier. Only a >= 3-operand chain detectable from the HIR
        // types folds here; a 2-operand `+` or an inference-shaped concat takes
        // the pairwise path below.
        if matches!(op, HirBinaryOp::Add)
            && let Some(parts) = self.str_concat_chain(lhs, rhs, ty)
            && parts.len() >= 3
        {
            let mut arg_operands = Vec::with_capacity(parts.len());
            for p in parts {
                let pl = self.lower_expr(p)?;
                let pl = self.auto_deref_cell(pl, span);
                arg_operands.push(Operand::Copy(Place::local(pl)));
            }
            let str_ty = self.tcx.string_ty();
            let dest = self.fresh(str_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("__concat".to_string())),
                args: arg_operands,
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return Some(dest);
        }
        let lhs_local = self.lower_expr(lhs)?;
        let rhs_local = self.lower_expr(rhs)?;
        // 0.7.0 flag::Cell auto-deref at the binary-op boundary -
        // `flags.output == "text"` works without `*`. Matches the
        // VM tier's `values_equal` / `compare` auto-unwrap shape.
        let lhs_local = self.auto_deref_cell(lhs_local, span);
        let rhs_local = self.auto_deref_cell(rhs_local, span);
        // Detect string concatenation (`s1 + s2` where at least
        // one side is a `String`) and route it through the native
        // runtime's `gos_rt_str_concat` helper rather than the
        // integer `+`. HIR types may still carry unresolved
        // inference variables here, so we inspect the lowered
        // MIR locals' concrete types too.
        let is_string_ty = |this: &Self, t: Ty| -> bool {
            let mut cur = t;
            loop {
                match this.tcx.kind_of(cur) {
                    TyKind::String => return true,
                    TyKind::Ref { inner, .. } => cur = *inner,
                    _ => return false,
                }
            }
        };
        if matches!(op, HirBinaryOp::Add) {
            if is_string_ty(self, ty)
                || is_string_ty(self, lhs.ty)
                || is_string_ty(self, rhs.ty)
                || is_string_ty(self, self.locals[lhs_local.0 as usize].ty)
                || is_string_ty(self, self.locals[rhs_local.0 as usize].ty)
            {
                let dest_ty = self.tcx.string_ty();
                let dest = self.fresh(dest_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_concat".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(lhs_local)),
                        Operand::Copy(Place::local(rhs_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return Some(dest);
            }
        }
        // String equality / inequality. Pointer-compare miscompares
        // because two byte-equal strings often have distinct
        // backing buffers (cell-loaded values, format!-built
        // strings, etc.). Route through the runtime's byte-level
        // helper.
        if matches!(op, HirBinaryOp::Eq | HirBinaryOp::Ne)
            && (is_string_ty(self, lhs.ty)
                || is_string_ty(self, rhs.ty)
                || is_string_ty(self, self.locals[lhs_local.0 as usize].ty)
                || is_string_ty(self, self.locals[rhs_local.0 as usize].ty))
        {
            let bool_ty = self.tcx.bool_ty();
            let cmp = self.fresh(bool_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_str_eq".to_string())),
                args: vec![
                    Operand::Copy(Place::local(lhs_local)),
                    Operand::Copy(Place::local(rhs_local)),
                ],
                destination: Place::local(cmp),
                target: Some(next),
            });
            self.set_current(next);
            if matches!(op, HirBinaryOp::Ne) {
                let dest = self.fresh(bool_ty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::UnaryOp {
                        op: UnOp::Not,
                        operand: Operand::Copy(Place::local(cmp)),
                    },
                    span,
                );
                return Some(dest);
            }
            return Some(cmp);
        }
        // Struct / enum equality: route `==` / `!=` to a `Type::eq`
        // method when one exists (synthesized by `#[derive(PartialEq)]`
        // or hand-written). Without this an aggregate `==` pointer-
        // compares, so two distinct allocations of equal values differ.
        if matches!(op, HirBinaryOp::Eq | HirBinaryOp::Ne) {
            let sname = self
                .adt_dispatch_name(lhs.ty)
                .or_else(|| self.adt_dispatch_name(self.locals[lhs_local.0 as usize].ty))
                .or_else(|| self.adt_dispatch_name(rhs.ty))
                .or_else(|| self.adt_dispatch_name(self.locals[rhs_local.0 as usize].ty))
                .or_else(|| self.local_struct.get(&lhs_local).cloned())
                .or_else(|| self.local_struct.get(&rhs_local).cloned())
                .or_else(|| self.struct_name_from_expr(lhs))
                .or_else(|| self.struct_name_from_expr(rhs));
            if let Some(sname) = sname {
                let mangled = format!("{sname}::eq");
                if self.impl_methods.contains_key(&mangled) {
                    let bool_ty = self.tcx.bool_ty();
                    let cmp = self.fresh(bool_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(mangled)),
                        args: vec![
                            Operand::Copy(Place::local(lhs_local)),
                            Operand::Copy(Place::local(rhs_local)),
                        ],
                        destination: Place::local(cmp),
                        target: Some(next),
                    });
                    self.set_current(next);
                    if matches!(op, HirBinaryOp::Ne) {
                        let dest = self.fresh(bool_ty);
                        self.emit_assign(
                            Place::local(dest),
                            Rvalue::UnaryOp {
                                op: UnOp::Not,
                                operand: Operand::Copy(Place::local(cmp)),
                            },
                            span,
                        );
                        return Some(dest);
                    }
                    return Some(cmp);
                }
            }
        }
        // Heap (recursive / `Box`) enum `==` / `!=` with no user `eq` impl:
        // route to the runtime structural walk so two distinct allocations of
        // an equal value compare true (matching the VM's `values_equal`)
        // instead of by pointer identity. `ensure_enum_eq_desc` returns `None`
        // (keep identity) for an enum with a field the walk cannot describe.
        if matches!(op, HirBinaryOp::Eq | HirBinaryOp::Ne) {
            // Operand types are often still inference vars at MIR time; recover
            // the enum via the same name chain the `eq`-method dispatch uses,
            // then map the name to the enum's concrete type.
            let sname = self
                .adt_dispatch_name(lhs.ty)
                .or_else(|| self.adt_dispatch_name(self.locals[lhs_local.0 as usize].ty))
                .or_else(|| self.adt_dispatch_name(rhs.ty))
                .or_else(|| self.adt_dispatch_name(self.locals[rhs_local.0 as usize].ty))
                .or_else(|| self.local_struct.get(&lhs_local).cloned())
                .or_else(|| self.local_struct.get(&rhs_local).cloned())
                .or_else(|| self.struct_name_from_expr(lhs))
                .or_else(|| self.struct_name_from_expr(rhs));
            let desc_sym = sname
                .and_then(|n| self.tcx.enum_ty_by_name(&n))
                .and_then(|ety| self.ensure_enum_eq_desc(ety));
            if let Some(sym) = desc_sym {
                let bool_ty = self.tcx.bool_ty();
                let cmp = self.fresh(bool_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_enum_struct_eq".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(lhs_local)),
                        Operand::Copy(Place::local(rhs_local)),
                        Operand::Const(ConstValue::Str(sym)),
                    ],
                    destination: Place::local(cmp),
                    target: Some(next),
                });
                self.set_current(next);
                if matches!(op, HirBinaryOp::Ne) {
                    let dest = self.fresh(bool_ty);
                    self.emit_assign(
                        Place::local(dest),
                        Rvalue::UnaryOp {
                            op: UnOp::Not,
                            operand: Operand::Copy(Place::local(cmp)),
                        },
                        span,
                    );
                    return Some(dest);
                }
                return Some(cmp);
            }
        }
        // Struct / enum ordering: route `<` `<=` `>` `>=` to a `Type::cmp`
        // method (synthesized for a by-value-comparable type or hand-written),
        // testing its -1 / 0 / 1 result against 0. Mirrors the `==` route.
        if matches!(
            op,
            HirBinaryOp::Lt | HirBinaryOp::Le | HirBinaryOp::Gt | HirBinaryOp::Ge
        ) {
            let sname = self
                .adt_dispatch_name(lhs.ty)
                .or_else(|| self.adt_dispatch_name(self.locals[lhs_local.0 as usize].ty))
                .or_else(|| self.adt_dispatch_name(rhs.ty))
                .or_else(|| self.adt_dispatch_name(self.locals[rhs_local.0 as usize].ty))
                .or_else(|| self.local_struct.get(&lhs_local).cloned())
                .or_else(|| self.local_struct.get(&rhs_local).cloned())
                .or_else(|| self.struct_name_from_expr(lhs))
                .or_else(|| self.struct_name_from_expr(rhs));
            if let Some(sname) = sname {
                let mangled = format!("{sname}::cmp");
                if self.impl_methods.contains_key(&mangled) {
                    let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                    let cmpres = self.fresh(i64_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(mangled)),
                        args: vec![
                            Operand::Copy(Place::local(lhs_local)),
                            Operand::Copy(Place::local(rhs_local)),
                        ],
                        destination: Place::local(cmpres),
                        target: Some(next),
                    });
                    self.set_current(next);
                    let bool_ty = self.tcx.bool_ty();
                    let dest = self.fresh(bool_ty);
                    self.emit_assign(
                        Place::local(dest),
                        Rvalue::BinaryOp {
                            op: lower_binop(op),
                            lhs: Operand::Copy(Place::local(cmpres)),
                            rhs: Operand::Const(ConstValue::Int(0)),
                        },
                        span,
                    );
                    return Some(dest);
                }
            }
        }
        // Arithmetic operator overloading: route `a + b` (and `- * /`) on a
        // user struct/enum to its `add`/`sub`/`mul`/`div` impl method. The
        // checker has already typed the result as that method's return type
        // (`ty`) and rejected ADT operands with no such impl, so a present
        // `impl_methods` entry is the method the call must reach.
        if let Some(method) = arith_overload_method(op) {
            let sname = self
                .adt_dispatch_name(lhs.ty)
                .or_else(|| self.adt_dispatch_name(self.locals[lhs_local.0 as usize].ty))
                .or_else(|| self.adt_dispatch_name(rhs.ty))
                .or_else(|| self.adt_dispatch_name(self.locals[rhs_local.0 as usize].ty))
                .or_else(|| self.local_struct.get(&lhs_local).cloned())
                .or_else(|| self.local_struct.get(&rhs_local).cloned())
                .or_else(|| self.struct_name_from_expr(lhs))
                .or_else(|| self.struct_name_from_expr(rhs));
            if let Some(sname) = sname {
                let mangled = format!("{sname}::{method}");
                if self.impl_methods.contains_key(&mangled) {
                    let dest = self.fresh(ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(mangled)),
                        args: vec![
                            Operand::Copy(Place::local(lhs_local)),
                            Operand::Copy(Place::local(rhs_local)),
                        ],
                        destination: Place::local(dest),
                        target: Some(next),
                    });
                    self.set_current(next);
                    return Some(dest);
                }
            }
        }
        // Mixed growable-vec vs fixed-array `==` / `!=` (`v == [1, 2]`):
        // the fixed-array operand is a flat slot aggregate, not a `GosVec`,
        // so neither the flat tuple compare nor `gos_rt_vec_eq` can read
        // the operand pair as-is. Materialize a borrowed vec view over the
        // array side and route through the element-wise vec compare - the
        // structural semantics the VM already implements.
        if matches!(op, HirBinaryOp::Eq | HirBinaryOp::Ne) {
            let lhs_arr = self.fixed_array_cmp_shape(lhs_local);
            let rhs_arr = self.fixed_array_cmp_shape(rhs_local);
            let lhs_vec = self.vec_elem_cmp_tag(self.locals[lhs_local.0 as usize].ty);
            let rhs_vec = self.vec_elem_cmp_tag(self.locals[rhs_local.0 as usize].ty);
            match (lhs_arr, rhs_arr, lhs_vec, rhs_vec) {
                (Some((elem, len)), None, None, Some(tag)) => {
                    let coerced = self.coerce_borrow_array_to_vec(lhs_local, elem, len, span);
                    return Some(self.lower_vec_eq(op, coerced, rhs_local, tag, span));
                }
                (None, Some((elem, len)), Some(tag), None) => {
                    let coerced = self.coerce_borrow_array_to_vec(rhs_local, elem, len, span);
                    return Some(self.lower_vec_eq(op, lhs_local, coerced, tag, span));
                }
                _ => {}
            }
        }
        // Tuple comparison: route `==`/`!=`/`<`/`<=`/`>`/`>=` to a runtime
        // structural compare over the flat slot buffer (the VM compares
        // tuples element-wise at runtime). Only tuples of scalar/string
        // elements are routed; others fall through.
        if matches!(
            op,
            HirBinaryOp::Eq
                | HirBinaryOp::Ne
                | HirBinaryOp::Lt
                | HirBinaryOp::Le
                | HirBinaryOp::Gt
                | HirBinaryOp::Ge
        ) {
            if let Some(tags) = self
                .tuple_cmp_tags(lhs.ty)
                .or_else(|| self.tuple_cmp_tags(self.locals[lhs_local.0 as usize].ty))
            {
                return Some(self.lower_tuple_cmp(op, lhs_local, rhs_local, &tags, span));
            }
        }
        // Vec/array equality: route `[..] == [..]` / `!=` to a runtime
        // element-wise compare (ordering on vecs is left to the scalar path).
        if matches!(op, HirBinaryOp::Eq | HirBinaryOp::Ne) {
            if let Some(tag) = self
                .vec_elem_cmp_tag(lhs.ty)
                .or_else(|| self.vec_elem_cmp_tag(self.locals[lhs_local.0 as usize].ty))
            {
                return Some(self.lower_vec_eq(op, lhs_local, rhs_local, tag, span));
            }
        }
        // Map and set equality: a handle compares by the entries or members
        // it stands for, not by its identity or by which storage shape it
        // settled into - the contract Rust's `HashMap` / `HashSet` have.
        if matches!(op, HirBinaryOp::Eq | HirBinaryOp::Ne) {
            if let Some(value) = self
                .map_value_compare(lhs.ty)
                .or_else(|| self.map_value_compare(self.locals[lhs_local.0 as usize].ty))
            {
                return Some(self.lower_map_eq(op, lhs_local, rhs_local, value, span));
            }
            if self.is_set_ty(lhs.ty) || self.is_set_ty(self.locals[lhs_local.0 as usize].ty) {
                return Some(self.lower_set_eq(op, lhs_local, rhs_local, span));
            }
            // A dynamic value compares by the contents it stands for, an arm
            // by its runtime name and every payload field, so two equal
            // values built separately are equal.
            if self.is_dyn_value_ty(lhs.ty)
                || self.is_dyn_value_ty(self.locals[lhs_local.0 as usize].ty)
            {
                return Some(self.lower_container_eq(
                    op,
                    "gos_rt_dyn_eq",
                    vec![
                        Operand::Copy(Place::local(lhs_local)),
                        Operand::Copy(Place::local(rhs_local)),
                    ],
                    span,
                ));
            }
        }
        // When the HIR type is still an inference variable, ground the
        // result type from the operands so the LLVM backend uses the
        // correct alloca type (double vs ptr). Without this, f64
        // arithmetic in impl methods produces ptr-typed intermediate
        // locals whose integer bit-pattern arithmetic diverges from
        // the correct float computation.
        let ty = if matches!(self.tcx.kind_of(ty), TyKind::Var(_) | TyKind::Error) {
            let lhs_ty = self.locals[lhs_local.0 as usize].ty;
            let rhs_ty = self.locals[rhs_local.0 as usize].ty;
            let resolve_float_or_int =
                |this: &Self, t: gossamer_types::Ty| -> Option<gossamer_types::Ty> {
                    match this.tcx.kind_of(t) {
                        TyKind::Float(_) | TyKind::Int(_) | TyKind::Bool => Some(t),
                        _ => None,
                    }
                };
            // For comparison ops, result is bool regardless of operand types.
            if matches!(
                op,
                HirBinaryOp::Eq
                    | HirBinaryOp::Ne
                    | HirBinaryOp::Lt
                    | HirBinaryOp::Le
                    | HirBinaryOp::Gt
                    | HirBinaryOp::Ge
            ) {
                self.tcx.bool_ty()
            } else if let Some(concrete) = resolve_float_or_int(self, lhs_ty) {
                concrete
            } else if let Some(concrete) = resolve_float_or_int(self, rhs_ty) {
                concrete
            } else {
                ty
            }
        } else {
            ty
        };
        let bin_op = lower_binop(op);
        // Integer divide / modulo guards. The compiled tiers lower
        // these to raw `sdiv`/`srem`, which trap (SIGFPE on x86) on a
        // zero divisor and on the signed `MIN / -1` overflow. Match
        // the VM: a zero divisor is a clean panic; `MIN / -1` wraps to
        // `MIN` and `MIN % -1` to 0. Floats follow IEEE (`x / 0.0` is
        // ±inf), so only integer operands are guarded.
        let int_ty: Option<gossamer_types::IntTy> = if matches!(bin_op, BinOp::Div | BinOp::Rem) {
            match self.tcx.kind_of(ty) {
                gossamer_types::TyKind::Int(it) => Some(*it),
                _ => None,
            }
        } else {
            None
        };
        // A literal divisor lets us drop the guards at compile time: a
        // known non-zero, non-(-1) constant (the common `/ 2`, `/ 10`,
        // `/ 1` case) can neither divide by zero nor overflow, so the
        // plain `BinaryOp` is emitted and identity/strength-reduction
        // folds still fire.
        let rhs_const: Option<i128> = match &rhs.kind {
            HirExprKind::Literal(lit @ HirLiteral::Int(_)) => match literal_to_const(lit) {
                ConstValue::Int(n) => Some(n),
                _ => None,
            },
            _ => None,
        };
        let divisor_maybe_zero = !matches!(rhs_const, Some(n) if n != 0);
        let divisor_maybe_neg1 = !matches!(rhs_const, Some(n) if n != -1);
        if int_ty.is_some() && divisor_maybe_zero {
            // Divide-by-zero: emit the wired `Assert{DivideByZero}`
            // (consumed by every backend). `expected: true` ⇒ panic
            // when `cond` (rhs != 0) is false, i.e. when the divisor
            // is zero (each backend's `lower_assert` polarity).
            let bool_ty = self.tcx.bool_ty();
            let nonzero = self.fresh(bool_ty);
            self.emit_assign(
                Place::local(nonzero),
                Rvalue::BinaryOp {
                    op: BinOp::Ne,
                    lhs: Operand::Copy(Place::local(rhs_local)),
                    rhs: Operand::Const(ConstValue::Int(0)),
                },
                span,
            );
            let ok = self.new_block(span);
            self.terminate(Terminator::Assert {
                cond: Operand::Copy(Place::local(nonzero)),
                expected: true,
                msg: AssertMessage::DivideByZero,
                target: ok,
            });
            self.set_current(ok);
        }
        // Signed `MIN / -1`: the wrapped result the VM produces (`MIN` for
        // `/`, `0` for `%`) instead of a trapping `sdiv`. Only the widths that
        // actually overflow the i64 runtime representation need the guard:
        // integer arithmetic runs at i64 width, so `i8`/`i16`/`i32 MIN / -1`
        // do NOT overflow there (the VM's `i64::wrapping_div` yields the
        // non-wrapped `+2^(n-1)`); guarding them with the narrow MIN would
        // const-fold the wrong wrapped value. Only `i64`/`isize` (and the
        // rejected `i128`) overflow.
        let signed_min: Option<i128> = match int_ty {
            Some(gossamer_types::IntTy::I64 | gossamer_types::IntTy::Isize) => {
                Some(i128::from(i64::MIN))
            }
            Some(gossamer_types::IntTy::I128) => Some(i128::MIN),
            _ => None,
        };
        if let Some(min_val) = signed_min.filter(|_| divisor_maybe_neg1) {
            let bool_ty = self.tcx.bool_ty();
            let is_min = self.fresh(bool_ty);
            self.emit_assign(
                Place::local(is_min),
                Rvalue::BinaryOp {
                    op: BinOp::Eq,
                    lhs: Operand::Copy(Place::local(lhs_local)),
                    rhs: Operand::Const(ConstValue::Int(min_val)),
                },
                span,
            );
            let is_neg1 = self.fresh(bool_ty);
            self.emit_assign(
                Place::local(is_neg1),
                Rvalue::BinaryOp {
                    op: BinOp::Eq,
                    lhs: Operand::Copy(Place::local(rhs_local)),
                    rhs: Operand::Const(ConstValue::Int(-1)),
                },
                span,
            );
            let ovf = self.fresh(bool_ty);
            self.emit_assign(
                Place::local(ovf),
                Rvalue::BinaryOp {
                    op: BinOp::BitAnd,
                    lhs: Operand::Copy(Place::local(is_min)),
                    rhs: Operand::Copy(Place::local(is_neg1)),
                },
                span,
            );
            let result = self.fresh(ty);
            let ovf_block = self.new_block(span);
            let normal_block = self.new_block(span);
            let join = self.new_block(span);
            self.terminate(Terminator::SwitchInt {
                discriminant: Operand::Copy(Place::local(ovf)),
                arms: vec![(0, normal_block)],
                default: ovf_block,
            });
            // Overflow arm: the wrapped result (MIN for `/`, 0 for `%`).
            self.set_current(ovf_block);
            let wrapped = if matches!(bin_op, BinOp::Rem) {
                0
            } else {
                min_val
            };
            self.emit_assign(
                Place::local(result),
                Rvalue::Use(Operand::Const(ConstValue::Int(wrapped))),
                span,
            );
            self.terminate(Terminator::Goto { target: join });
            // Normal arm: the actual division.
            self.set_current(normal_block);
            self.emit_assign(
                Place::local(result),
                Rvalue::BinaryOp {
                    op: bin_op,
                    lhs: Operand::Copy(Place::local(lhs_local)),
                    rhs: Operand::Copy(Place::local(rhs_local)),
                },
                span,
            );
            self.terminate(Terminator::Goto { target: join });
            self.set_current(join);
            return Some(result);
        }
        let local = self.fresh(ty);
        self.emit_assign(
            Place::local(local),
            Rvalue::BinaryOp {
                op: bin_op,
                lhs: Operand::Copy(Place::local(lhs_local)),
                rhs: Operand::Copy(Place::local(rhs_local)),
            },
            span,
        );
        Some(local)
    }

    pub(crate) fn lower_assign(&mut self, place: &HirExpr, value: &HirExpr, span: Span) {
        // Rebinding a reference binding changes the alias target. The name is
        // bound directly to its current source local, so emitting an ordinary
        // assignment here would overwrite the old source instead.
        if let HirExprKind::Path { segments, .. } = &place.kind
            && let [name] = segments.as_slice()
            && self.reference_alias_local(&name.name).is_some()
            && let HirExprKind::Unary {
                op: HirUnaryOp::RefShared | HirUnaryOp::RefMut,
                operand,
            } = &value.kind
            && let Some(source) = self.lower_expr(operand)
            && self.rebind_reference_alias(&name.name, source)
        {
            return;
        }
        // A `static mut` assignment (`COUNTER = v`, `COUNTER += 1`) stores
        // into the live global cell. The RHS is lowered first so a read of
        // the same static inside it observes the pre-write value.
        if let HirExprKind::Path { def: Some(def), .. } = &place.kind
            && let Some(sref) = self.mut_statics.get(def).cloned()
        {
            let Some(value_local) = self.lower_expr(value) else {
                return;
            };
            self.emit_static_store(sref, Operand::Copy(Place::local(value_local)), span);
            return;
        }
        // `xs[idx] = v` on a `Vec<T>` / `Slice<T>` receiver routes
        // through the runtime helper `gos_rt_vec_set_i64`. The
        // generic projection path computes a flat-array address
        // `base + idx * stride` that's correct for `[T; N]` but
        // wrong for Vec headers (the data lives at `header.ptr`,
        // not directly in the slot). Without this short-circuit
        // `tc_names[idx] = s` in askq's chat round was a no-op
        // and the LLM's tool name came back empty. Pointer-sized
        // element types (i64 / String / json::Value / Adt
        // pointers) all use the same i64-sized helper since the
        // values are pointer-shaped on the wire.
        if let HirExprKind::Index { base, index } = &place.kind {
            use gossamer_types::TyKind;
            let base_local_for_kind = self
                .receiver_local_from_path(base)
                .map_or(base.ty, |l| self.locals[l.0 as usize].ty);
            let mut peeled = base_local_for_kind;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                peeled = *inner;
            }
            let elem_ty = match self.tcx.kind_of(peeled) {
                TyKind::Vec(elem) | TyKind::Slice(elem) => Some(*elem),
                _ => None,
            };
            if let Some(elem_ty) = elem_ty {
                let Some(value_local) = self.lower_expr(value) else {
                    return;
                };
                let Some(base_local) = self.lower_expr(base) else {
                    return;
                };
                let Some(idx_local) = self.lower_expr(index) else {
                    return;
                };
                // An `Option` / `Result` / inline enum element is the 16-byte
                // by-value carrier: the i64 store keeps only the discriminant
                // and drops the payload, so a two-word carrier writes both
                // words through the i128 store.
                // A tuple / struct / fixed-array element occupies a block
                // of slots, so a word-sized store writes its first field
                // and leaves the rest of the element as it was. Such an
                // element reaches the runtime as the address of its slot
                // block, the way `gos_rt_vec_push` takes one.
                let setter = if self.is_inline_slot_block(elem_ty) {
                    "gos_rt_vec_set_slots"
                } else if crate::lower::carrier_ref::is_two_word_carrier(self.tcx, elem_ty) {
                    "gos_rt_vec_set_i128"
                } else {
                    "gos_rt_vec_set_i64"
                };
                let unit_ty = self.tcx.unit();
                let dest = self.fresh(unit_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(setter.to_string())),
                    args: vec![
                        Operand::Copy(Place::local(base_local)),
                        Operand::Copy(Place::local(idx_local)),
                        Operand::Copy(Place::local(value_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return;
            }
        }
        // Fuse `acc += format!(...)`: append each format piece straight onto
        // the accumulator so the text reaches `acc` in one copy, instead of
        // routing through the concat buffer and a throwaway result string that
        // `+=` then copies a second time.
        if let HirExprKind::Binary {
            op: HirBinaryOp::Add,
            lhs,
            rhs,
        } = &value.kind
            && let (Some(acc), Some(lhs_local)) = (
                self.receiver_local_from_path(place),
                self.receiver_local_from_path(lhs),
            )
            && acc == lhs_local
        {
            let mut pieces = Vec::new();
            Self::collect_append_pieces(rhs, &mut pieces);
            if self.try_lower_append_fused(acc, &pieces, span) {
                return;
            }
        }
        // Fuse `*s += piece` where `*s` is a `&mut String` deref place: append
        // in place via the self-consuming runtime helper, then store the
        // result back through the slot. The bare-path fast path above cannot
        // reach a deref accumulator (`receiver_local_from_path` is `None` for
        // `*s`), so this is its deref counterpart.
        if let HirExprKind::Binary {
            op: HirBinaryOp::Add,
            lhs,
            rhs,
        } = &value.kind
            && let Some(s_local) = self.deref_string_place_local(place)
            && self.deref_string_place_local(lhs) == Some(s_local)
            && self.try_lower_deref_append_fused(s_local, rhs, span)
        {
            return;
        }
        // A container parameter crosses the call as its own handle, so the
        // pointee `*p` names is the container the caller's binding holds:
        // the assignment replaces that container's contents in place.
        if let Some((holder, symbol)) = self.deref_container_assign_target(place) {
            let Some(value_local) = self.lower_expr(value) else {
                return;
            };
            let unit_ty = self.tcx.unit();
            let dest = self.fresh(unit_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(symbol.to_string())),
                args: vec![
                    Operand::Copy(Place::local(holder)),
                    Operand::Copy(Place::local(value_local)),
                ],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return;
        }
        let Some(mut value_local) = self.lower_expr(value) else {
            return;
        };
        let Some(mir_place) = self.lower_place_expr(place) else {
            return;
        };
        // Overwrite of a `&mut String` deref place (`*s = v`): the slot's
        // previous String is displaced, so release it before the store binds
        // the new value. The store's own `Copy` retain (in the RC pass) gives
        // the slot its share of the new value. The self-consuming `*s += …`
        // append handled above never reaches here (it returns early), so this
        // fires only for genuine overwrites - no double release of a value the
        // append helper already consumed.
        if self.deref_string_place_local(place).is_some() {
            let unit_ty = self.tcx.unit();
            let rel = self.fresh(unit_ty);
            self.emit_assign(
                Place::local(rel),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_rc_release",
                    args: vec![Operand::Copy(mir_place.clone())],
                },
                span,
            );
        }
        // Same callable-coercion as `let` and `return`: when the
        // lvalue's static type is callable and the rvalue is a
        // bare fn item, wrap the fn into the env+code blob so the
        // slot ends up env-shaped.
        {
            use gossamer_types::TyKind;
            let dest_callable = matches!(
                self.tcx.kind_of(place.ty),
                TyKind::FnPtr(_) | TyKind::FnTrait(_)
            );
            let value_ty = self.locals[value_local.0 as usize].ty;
            let src_is_fn_def = matches!(self.tcx.kind_of(value_ty), TyKind::FnDef { .. });
            let src_names_fn = self.local_fn_name.contains_key(&value_local);
            if dest_callable && (src_is_fn_def || src_names_fn) {
                value_local = self.coerce_to_fn_trait_if_needed(value_local, place.ty, span);
            }
        }
        self.emit_assign(
            mir_place,
            Rvalue::Use(Operand::Copy(Place::local(value_local))),
            span,
        );
    }

    /// Runtime structural-compare tag for a scalar/string type (matching the
    /// `gos_rt_tuple_format` encoding: 0 Int, 2 Float, 3 Bool, 4 Char, 5 Str),
    /// or `None` for an aggregate / unsupported element.
    fn scalar_cmp_tag(&self, ty: Ty) -> Option<u8> {
        use gossamer_types::TyKind;
        let mut t = ty;
        loop {
            match self.tcx.kind_of(t) {
                TyKind::Ref { inner, .. } => t = *inner,
                TyKind::Int(_) => return Some(0),
                TyKind::Float(_) => return Some(2),
                TyKind::Bool => return Some(3),
                TyKind::Char => return Some(4),
                TyKind::String => return Some(5),
                _ => return None,
            }
        }
    }

    /// Tags describing `ty` in a `gos_rt_tuple_cmp` / `gos_rt_tuple_format`
    /// stream: one byte for a scalar, or the `8, count, <nested tags…>`
    /// form for a nested tuple, whose slots are flattened into the
    /// parent's buffer. `None` when any leaf is not slot-comparable.
    /// Field types of a value stored inline as a run of slots - a tuple, or a
    /// struct, which lays its fields out the same way. `None` for anything
    /// else, including an enum, whose payload is not a plain field run.
    /// The payload of an `Option` whose value the slot comparison can read,
    /// or `None` for any other type. A payload reached by pointer - a
    /// `String`, a container - is excluded: its slot holds an address, and
    /// ordering addresses is not ordering values.
    pub(crate) fn option_scalar_payload(&self, ty: Ty) -> Option<Ty> {
        use gossamer_types::TyKind;
        let TyKind::Adt { def, substs } = self.tcx.kind_of(ty) else {
            return None;
        };
        if def.local != u32::MAX - 1 {
            return None;
        }
        let payload = *substs.types().first()?;
        matches!(
            self.tcx.kind_of(payload),
            TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_)
        )
        .then_some(payload)
    }

    pub(crate) fn inline_field_tys(&self, ty: Ty) -> Option<Vec<Ty>> {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Tuple(elems) => Some(elems.clone()),
            TyKind::Adt { def, .. }
                if def.local < u32::MAX - 16 && !self.tcx.is_inline_enum_ty(ty) =>
            {
                self.tcx.struct_field_tys(*def).map(<[Ty]>::to_vec)
            }
            _ => None,
        }
    }

    pub(crate) fn tuple_stream_tags(&self, ty: Ty) -> Option<Vec<u8>> {
        use gossamer_types::TyKind;
        let mut peeled = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        if let Some(elems) = self.inline_field_tys(peeled) {
            if elems.is_empty() || elems.len() > usize::from(u8::MAX) {
                return None;
            }
            let mut out = vec![gossamer_abi::TUPLE_TAG_NESTED, elems.len() as u8];
            for e in &elems {
                out.extend(self.tuple_stream_tags(*e)?);
            }
            return Some(out);
        }
        // An `Option` field occupies the two slots its carrier is - the
        // discriminant, then the payload - so it streams as a nested pair
        // rather than as one leaf. Reading it as a single slot would compare
        // half of the field and walk the rest of the element off by one.
        if let Some(payload) = self.option_scalar_payload(peeled) {
            let value = self.scalar_cmp_tag(payload)?;
            return Some(vec![gossamer_abi::TUPLE_TAG_NESTED, 2, 0, value]);
        }
        self.scalar_cmp_tag(ty).map(|tag| vec![tag])
    }

    /// The top-level element count and tag stream for a tuple type, or
    /// `None` when it is not a tuple of slot-comparable leaves.
    /// The ordering descriptor for `ty`: the byte stream the runtime walks
    /// to compare two values of that type, alongside their slots. `None`
    /// when the type has no ordering the language defines - a map, a set, a
    /// handle - so the caller can decline rather than compare raw words.
    pub(crate) fn ordering_stream(&self, ty: Ty) -> Option<Vec<u8>> {
        self.ordering_stream_inner(ty, None, 0)
    }

    fn ordering_stream_inner(
        &self,
        ty: Ty,
        self_enum: Option<gossamer_resolve::DefId>,
        depth: u32,
    ) -> Option<Vec<u8>> {
        use gossamer_types::{ArrayLen, IntTy, TyKind};
        if depth > 12 {
            return None;
        }
        let mut peeled = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        match self.tcx.kind_of(peeled) {
            TyKind::Int(IntTy::U64 | IntTy::Usize) => Some(vec![1]),
            TyKind::Int(_) | TyKind::Duration | TyKind::Instant => Some(vec![0]),
            TyKind::Float(_) => Some(vec![2]),
            TyKind::Bool => Some(vec![3]),
            TyKind::Char => Some(vec![4]),
            TyKind::String => Some(vec![5]),
            TyKind::Array {
                elem,
                len: ArrayLen::Concrete(n),
            } => {
                let (elem, n) = (*elem, *n);
                let count = u8::try_from(n).ok()?;
                let span = u8::try_from(self.type_slot_bytes(elem).max(8) / 8).ok()?;
                let mut out = vec![gossamer_abi::DESC_ARRAY, count, span];
                out.extend(self.ordering_stream_inner(elem, self_enum, depth + 1)?);
                Some(out)
            }
            TyKind::Vec(elem) | TyKind::Slice(elem) => {
                let elem = *elem;
                let mut out = vec![gossamer_abi::DESC_VEC];
                out.extend(self.ordering_stream_inner(elem, self_enum, depth + 1)?);
                Some(out)
            }
            TyKind::Tuple(elems) => {
                let elems = elems.clone();
                let arity = u8::try_from(elems.len()).ok()?;
                let mut out = vec![gossamer_abi::TUPLE_TAG_NESTED, arity];
                for e in &elems {
                    out.extend(self.ordering_stream_inner(*e, self_enum, depth + 1)?);
                }
                Some(out)
            }
            TyKind::Adt { def, substs } => {
                let (def, substs) = (*def, substs.clone());
                // `Option` and `Result` order by arm, then by payload.
                if def.local == u32::MAX || def.local == u32::MAX - 1 {
                    let is_option = def.local == u32::MAX - 1;
                    let tys = substs.types();
                    let mut out = vec![if is_option {
                        gossamer_abi::DESC_OPTION
                    } else {
                        gossamer_abi::DESC_RESULT
                    }];
                    out.extend(self.ordering_stream_inner(*tys.first()?, self_enum, depth + 1)?);
                    if !is_option {
                        out.extend(self.ordering_stream_inner(
                            *tys.get(1)?,
                            self_enum,
                            depth + 1,
                        )?);
                    }
                    return Some(out);
                }
                if self_enum == Some(def) {
                    return Some(vec![gossamer_abi::DESC_SELF]);
                }
                if let Some(fields) = self.tcx.struct_field_tys(def) {
                    let fields = fields.to_vec();
                    let arity = u8::try_from(fields.len()).ok()?;
                    let mut out = vec![gossamer_abi::TUPLE_TAG_NESTED, arity];
                    for f in &fields {
                        out.extend(self.ordering_stream_inner(*f, self_enum, depth + 1)?);
                    }
                    return Some(out);
                }
                let variants = self.tcx.enum_variant_tys(def)?.to_vec();
                let count = u8::try_from(variants.len()).ok()?;
                let inline = u8::from(self.tcx.is_inline_enum_ty(peeled));
                let mut out = vec![gossamer_abi::DESC_ENUM, inline, count];
                for fields in &variants {
                    out.push(u8::try_from(fields.len()).ok()?);
                    for f in fields {
                        out.extend(self.ordering_stream_inner(*f, Some(def), depth + 1)?);
                    }
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// Whether `ty` is an enum some variant of which carries fields.
    ///
    /// A variant-only enum's slot spells its variant rank, so the word it
    /// holds is already its order; one with a payload is reached through the
    /// node holding that payload, and ordering its slot would order addresses.
    fn enum_has_payload(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let TyKind::Adt { def, .. } = self.tcx.kind_of(ty) else {
            return false;
        };
        self.tcx
            .enum_variant_tys(*def)
            .is_some_and(|variants| variants.iter().any(|fields| !fields.is_empty()))
    }

    pub(crate) fn tuple_element_stream(&self, ty: Ty) -> Option<(usize, Vec<u8>)> {
        use gossamer_types::TyKind;
        let mut peeled = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        // An `Option` is a two-slot carrier - the discriminant, then the
        // payload - and reading it as one word sorts half of each element.
        // Streaming both slots orders it by the language's own rule for an
        // enum: by variant rank, and `Some` is declared first, so its zero
        // discriminant sorts ahead of `None`'s; equal ranks fall through to
        // the payload.
        if let Some(payload) = self.option_scalar_payload(peeled) {
            // The discriminant is an integer slot; tag 0 is what
            // `scalar_cmp_tag` answers for one, without interning a type.
            let value = self.scalar_cmp_tag(payload)?;
            return Some((2, vec![0, value]));
        }
        let Some(elems) = self.inline_field_tys(peeled).filter(|e| !e.is_empty()) else {
            // An element whose slot word is not its own order - a payload
            // enum reached through its RC node - is ordered by the descriptor
            // the runtime walks alongside its slot. An inline enum keeps the
            // word path: its slot spells the tag its order is, and the
            // descriptor's own inline layout spans a second slot no sequence
            // element carries.
            if self.tcx.is_inline_enum_ty(peeled) || !self.enum_has_payload(peeled) {
                return None;
            }
            let desc = self.ordering_stream(peeled)?;
            return desc
                .first()
                .is_some_and(|tag| *tag == gossamer_abi::DESC_ENUM)
                .then_some((1, desc));
        };
        let mut tags = Vec::with_capacity(elems.len());
        for e in &elems {
            tags.extend(self.tuple_stream_tags(*e)?);
        }
        Some((elems.len(), tags))
    }

    /// Emits `lhs == rhs` for two already-lowered locals holding values of
    /// `ty`, applying the same structural dispatch a source-level `a == b`
    /// gets: a `Type::eq` impl, the runtime String and heap-enum walks, or the
    /// element-wise tuple / sequence compares. Scalars fall through to the
    /// primitive slot compare. A caller that synthesizes a comparison rather
    /// than lowering one the user wrote routes through this so its answer
    /// matches `==` for every element type.
    pub(crate) fn emit_structural_eq(
        &mut self,
        lhs_local: Local,
        rhs_local: Local,
        ty: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let bool_ty = self.tcx.bool_ty();
        let peel = |this: &Self, t: Ty| -> Ty {
            let mut cur = t;
            while let TyKind::Ref { inner, .. } = this.tcx.kind_of(cur) {
                cur = *inner;
            }
            cur
        };
        let ty = peel(self, ty);
        let lhs_ty = peel(self, self.locals[lhs_local.0 as usize].ty);

        if matches!(self.tcx.kind_of(ty), TyKind::String)
            || matches!(self.tcx.kind_of(lhs_ty), TyKind::String)
        {
            let cmp = self.fresh(bool_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_str_eq".to_string())),
                args: vec![
                    Operand::Copy(Place::local(lhs_local)),
                    Operand::Copy(Place::local(rhs_local)),
                ],
                destination: Place::local(cmp),
                target: Some(next),
            });
            self.set_current(next);
            return cmp;
        }

        // A map and a set are equal to one holding the same entries, whatever
        // order those went in and whichever storage each side settled into,
        // so both route to the runtime's content comparison rather than to
        // the pointer equality a handle would otherwise get.
        if let Some(value) = self
            .map_value_compare(ty)
            .or_else(|| self.map_value_compare(lhs_ty))
        {
            return self.lower_map_eq(HirBinaryOp::Eq, lhs_local, rhs_local, value, span);
        }
        if self.is_set_ty(ty) || self.is_set_ty(lhs_ty) {
            return self.lower_set_eq(HirBinaryOp::Eq, lhs_local, rhs_local, span);
        }
        if self.is_dyn_value_ty(ty) || self.is_dyn_value_ty(lhs_ty) {
            return self.lower_container_eq(
                HirBinaryOp::Eq,
                "gos_rt_dyn_eq",
                vec![
                    Operand::Copy(Place::local(lhs_local)),
                    Operand::Copy(Place::local(rhs_local)),
                ],
                span,
            );
        }

        let sname = self
            .adt_dispatch_name(ty)
            .or_else(|| self.adt_dispatch_name(lhs_ty))
            .or_else(|| self.local_struct.get(&lhs_local).cloned())
            .or_else(|| self.local_struct.get(&rhs_local).cloned());
        if let Some(sname) = sname.clone() {
            let mangled = format!("{sname}::eq");
            if self.impl_methods.contains_key(&mangled) {
                let cmp = self.fresh(bool_ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(mangled)),
                    args: vec![
                        Operand::Copy(Place::local(lhs_local)),
                        Operand::Copy(Place::local(rhs_local)),
                    ],
                    destination: Place::local(cmp),
                    target: Some(next),
                });
                self.set_current(next);
                return cmp;
            }
        }
        if let Some(sym) = sname
            .and_then(|n| self.tcx.enum_ty_by_name(&n))
            .and_then(|ety| self.ensure_enum_eq_desc(ety))
        {
            let cmp = self.fresh(bool_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_enum_struct_eq".to_string())),
                args: vec![
                    Operand::Copy(Place::local(lhs_local)),
                    Operand::Copy(Place::local(rhs_local)),
                    Operand::Const(ConstValue::Str(sym)),
                ],
                destination: Place::local(cmp),
                target: Some(next),
            });
            self.set_current(next);
            return cmp;
        }
        if let Some(tags) = self
            .tuple_cmp_tags(ty)
            .or_else(|| self.tuple_cmp_tags(lhs_ty))
        {
            return self.lower_tuple_cmp(HirBinaryOp::Eq, lhs_local, rhs_local, &tags, span);
        }
        if let Some(tag) = self
            .vec_elem_cmp_tag(ty)
            .or_else(|| self.vec_elem_cmp_tag(lhs_ty))
        {
            return self.lower_vec_eq(HirBinaryOp::Eq, lhs_local, rhs_local, tag, span);
        }
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Eq,
                lhs: Operand::Copy(Place::local(lhs_local)),
                rhs: Operand::Copy(Place::local(rhs_local)),
            },
            span,
        );
        cmp
    }

    /// How the runtime reads a map's value word for `==`: `0` a word, `1` an
    /// `f64`, `2` a `String`, `3` a single slot the descriptor describes, `4`
    /// a block of slots it addresses. `None` when `ty` is not a map.
    fn map_value_compare(&self, ty: Ty) -> Option<MapValueCompare> {
        use gossamer_types::TyKind;
        let TyKind::HashMap { value, .. } = self.tcx.kind_of(ty) else {
            return None;
        };
        let mut cur = *value;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let kind = match self.tcx.kind_of(cur) {
            TyKind::Float(_) => 1,
            TyKind::String => 2,
            TyKind::Vec(_) | TyKind::Slice(_) => 3,
            _ if self.inline_field_tys(cur).is_some() => 4,
            _ => 0,
        };
        let desc = match kind {
            3 | 4 => self.key_descriptor(cur),
            _ => None,
        };
        // A shape with no descriptor is read as the word it is; without one
        // there is nothing to fold its content by.
        let kind = if matches!(kind, 3 | 4) && desc.is_none() {
            0
        } else {
            kind
        };
        Some(MapValueCompare { kind, desc })
    }

    /// Whether `ty` is the open dynamic value.
    fn is_dyn_value_ty(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        matches!(self.tcx.kind_of(cur), TyKind::DynValue)
    }

    /// Whether `ty` is a `Set` or a `BTreeSet`.
    fn is_set_ty(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        matches!(self.tcx.kind_of(cur), TyKind::Adt { def, .. }
            if self.tcx.def_name(*def).is_some_and(|name| matches!(name, "Set" | "BTreeSet")))
    }

    /// Per-element tags for a flat-buffer aggregate of scalar/string elements:
    /// a tuple (`(A, B)`) or a fixed-size array (`[T; N]`, a flat `[N x i64]`
    /// buffer like a tuple). `None` for a non-flat or aggregate-element type.
    fn tuple_cmp_tags(&self, ty: Ty) -> Option<Vec<u8>> {
        use gossamer_types::{ArrayLen, TyKind};
        let mut t = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(t) {
            t = *inner;
        }
        match self.tcx.kind_of(t) {
            TyKind::Tuple(elems) => elems
                .clone()
                .iter()
                .map(|e| self.scalar_cmp_tag(*e))
                .collect(),
            TyKind::Array {
                elem,
                len: ArrayLen::Concrete(n),
            } => {
                let n = *n;
                self.scalar_cmp_tag(*elem).map(|tag| vec![tag; n])
            }
            _ => None,
        }
    }

    /// Fixed-size array shape `(elem, len)` of a comparison operand's MIR
    /// local, resolving through references, when its elements are
    /// scalar/string-comparable and the length is concrete. The MIR local
    /// type is authoritative for the operand's runtime representation (a
    /// flat slot aggregate), regardless of what the HIR coerced the
    /// expression type to.
    fn fixed_array_cmp_shape(&self, local: Local) -> Option<(Ty, gossamer_types::ArrayLen)> {
        use gossamer_types::{ArrayLen, TyKind};
        let mut t = self.locals[local.0 as usize].ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(t) {
            t = *inner;
        }
        match self.tcx.kind_of(t) {
            TyKind::Array {
                elem,
                len: len @ ArrayLen::Concrete(_),
            } if self.scalar_cmp_tag(*elem).is_some() => Some((*elem, *len)),
            _ => None,
        }
    }

    /// Element tag for a growable `Vec<T>` / slice `[T]` (a `GosVec` pointer)
    /// whose element is scalar/string, or `None` otherwise. Fixed arrays are
    /// flat buffers handled by [`Self::tuple_cmp_tags`], not here.
    fn vec_elem_cmp_tag(&self, ty: Ty) -> Option<u8> {
        use gossamer_types::TyKind;
        let mut t = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(t) {
            t = *inner;
        }
        match self.tcx.kind_of(t) {
            TyKind::Vec(e) | TyKind::Slice(e) => self.scalar_cmp_tag(*e),
            _ => None,
        }
    }

    /// Emits `gos_rt_tuple_cmp(lhs, rhs, n, tags)` then maps its `-1/0/1`
    /// result to a bool for the comparison operator (`cmp <op> 0`).
    fn lower_tuple_cmp(
        &mut self,
        op: HirBinaryOp,
        lhs_local: Local,
        rhs_local: Local,
        tags: &[u8],
        span: Span,
    ) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let cmp = self.fresh(i64_ty);
        let tag_str: String = tags.iter().map(|&b| b as char).collect();
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_tuple_cmp".to_string())),
            args: vec![
                Operand::Copy(Place::local(lhs_local)),
                Operand::Copy(Place::local(rhs_local)),
                Operand::Const(ConstValue::Int(tags.len() as i128)),
                Operand::Const(ConstValue::Str(tag_str)),
            ],
            destination: Place::local(cmp),
            target: Some(next),
        });
        self.set_current(next);
        let bool_ty = self.tcx.bool_ty();
        let dest = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::BinaryOp {
                op: lower_binop(op),
                lhs: Operand::Copy(Place::local(cmp)),
                rhs: Operand::Const(ConstValue::Int(0)),
            },
            span,
        );
        dest
    }

    /// Emits `gos_rt_vec_eq(lhs, rhs, elem_tag)`, negating the result for `!=`.
    /// Emits `gos_rt_map_eq(lhs, rhs, value_kind, value_desc)`, negated for
    /// `!=`. A value that stands for a whole value rather than a number - a
    /// string, a sequence, an aggregate - carries the descriptor its content
    /// is folded by, so two equal values at distinct allocations compare
    /// equal, exactly as they do on the interpreter.
    fn lower_map_eq(
        &mut self,
        op: HirBinaryOp,
        lhs_local: Local,
        rhs_local: Local,
        value: MapValueCompare,
        span: Span,
    ) -> Local {
        let desc = match value.desc {
            Some(desc) => Operand::Const(ConstValue::Str(desc)),
            None => Operand::Const(ConstValue::Int(0)),
        };
        self.lower_container_eq(
            op,
            "gos_rt_map_eq",
            vec![
                Operand::Copy(Place::local(lhs_local)),
                Operand::Copy(Place::local(rhs_local)),
                Operand::Const(ConstValue::Int(i128::from(value.kind))),
                desc,
            ],
            span,
        )
    }

    /// Emits `gos_rt_set_eq(lhs, rhs)`, negated for `!=`.
    fn lower_set_eq(
        &mut self,
        op: HirBinaryOp,
        lhs_local: Local,
        rhs_local: Local,
        span: Span,
    ) -> Local {
        self.lower_container_eq(
            op,
            "gos_rt_set_eq",
            vec![
                Operand::Copy(Place::local(lhs_local)),
                Operand::Copy(Place::local(rhs_local)),
            ],
            span,
        )
    }

    fn lower_container_eq(
        &mut self,
        op: HirBinaryOp,
        helper: &str,
        args: Vec<Operand>,
        span: Span,
    ) -> Local {
        let bool_ty = self.tcx.bool_ty();
        let eq = self.fresh(bool_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(helper.to_string())),
            args,
            destination: Place::local(eq),
            target: Some(next),
        });
        self.set_current(next);
        if matches!(op, HirBinaryOp::Ne) {
            let dest = self.fresh(bool_ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::UnaryOp {
                    op: UnOp::Not,
                    operand: Operand::Copy(Place::local(eq)),
                },
                span,
            );
            return dest;
        }
        eq
    }

    fn lower_vec_eq(
        &mut self,
        op: HirBinaryOp,
        lhs_local: Local,
        rhs_local: Local,
        elem_tag: u8,
        span: Span,
    ) -> Local {
        let bool_ty = self.tcx.bool_ty();
        let eq = self.fresh(bool_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_eq".to_string())),
            args: vec![
                Operand::Copy(Place::local(lhs_local)),
                Operand::Copy(Place::local(rhs_local)),
                Operand::Const(ConstValue::Int(i128::from(elem_tag))),
            ],
            destination: Place::local(eq),
            target: Some(next),
        });
        self.set_current(next);
        if matches!(op, HirBinaryOp::Ne) {
            let dest = self.fresh(bool_ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::UnaryOp {
                    op: UnOp::Not,
                    operand: Operand::Copy(Place::local(eq)),
                },
                span,
            );
            return dest;
        }
        eq
    }

    /// Resolves a `format!` / append piece to a concrete type for the append
    /// fusion. The HIR expression type is used when already concrete; otherwise,
    /// for a bare path referencing a bound local (e.g. an enum-payload binding
    /// whose HIR type the checker left as an inference var, as in
    /// `match v { JsonVal::Int(n) => *out += format!("{}", n) }`), the local's
    /// pinned MIR type is used - the pattern binding pins it to the variant's
    /// declared payload type. Returns the original (possibly unresolved) type
    /// when neither is concrete, so the caller's `append_fn` gate still rejects
    /// it and the buffered-concat fallback fires.
    fn resolved_piece_ty(&self, p: &HirExpr) -> Ty {
        use gossamer_types::TyKind;
        if !matches!(self.tcx.kind_of(p.ty), TyKind::Var(_) | TyKind::Error) {
            return p.ty;
        }
        if let HirExprKind::Path { segments, .. } = &p.kind
            && let [seg] = segments.as_slice()
            && let Some(local) = self.lookup_local(&seg.name)
        {
            let lty = self.locals[local.0 as usize].ty;
            if !matches!(self.tcx.kind_of(lty), TyKind::Var(_) | TyKind::Error) {
                return lty;
            }
        }
        p.ty
    }

    /// Lowers `acc += __concat(pieces...)` to one in-place append per piece
    /// (`acc = gos_rt_str_append_*(acc, piece)`), each copying its piece a
    /// single time into the accumulator. Returns `false` without emitting
    /// anything when a piece is not a `String` / `i64` / `f64`, so the caller
    /// falls back to the buffered concat path.
    fn try_lower_append_fused(&mut self, acc: Local, pieces: &[&HirExpr], span: Span) -> bool {
        use gossamer_types::{FloatTy, IntTy, TyKind};
        if pieces.is_empty() {
            return false;
        }
        let append_fn = |this: &Self, ty: Ty| -> Option<&'static str> {
            let mut cur = ty;
            while let TyKind::Ref { inner, .. } = this.tcx.kind_of(cur) {
                cur = *inner;
            }
            match this.tcx.kind_of(cur) {
                TyKind::String => Some("gos_rt_str_concat_drop_a"),
                TyKind::Int(IntTy::I64) => Some("gos_rt_str_append_i64"),
                TyKind::Float(FloatTy::F64) => Some("gos_rt_str_append_f64"),
                _ => None,
            }
        };
        // The accumulator must itself be a `String` for the append result to
        // type-check back into its slot.
        if append_fn(self, self.locals[acc.0 as usize].ty) != Some("gos_rt_str_concat_drop_a") {
            return false;
        }
        let mut fns = Vec::with_capacity(pieces.len());
        for p in pieces {
            match append_fn(self, self.resolved_piece_ty(p)) {
                Some(f) => fns.push(f),
                None => return false,
            }
        }
        for (&p, fname) in pieces.iter().zip(fns) {
            let literal_len = match &p.kind {
                HirExprKind::Literal(gossamer_hir::HirLiteral::String(s))
                    if fname == "gos_rt_str_concat_drop_a" =>
                {
                    Some(s.len() as i128)
                }
                _ => None,
            };
            let Some(piece_local) = self.lower_expr(p) else {
                return true;
            };
            let piece_local = self.auto_deref_cell(piece_local, span);
            let next = self.new_block(span);
            let (call_name, call_args) = match literal_len {
                Some(len) => (
                    "gos_rt_str_append_bytes",
                    vec![
                        Operand::Copy(Place::local(acc)),
                        Operand::Copy(Place::local(piece_local)),
                        Operand::Const(ConstValue::Int(len)),
                    ],
                ),
                None => (
                    fname,
                    vec![
                        Operand::Copy(Place::local(acc)),
                        Operand::Copy(Place::local(piece_local)),
                    ],
                ),
            };
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(call_name.to_string())),
                args: call_args,
                destination: Place::local(acc),
                target: Some(next),
            });
            self.set_current(next);
        }
        true
    }

    /// Flattens the RHS of `acc += ...` into the pieces that should be appended
    /// to `acc`. This covers both parser-injected `__concat(...)` calls and
    /// ordinary right-associated `a + b + c` trees. The accumulator itself is
    /// deliberately not part of this list; callers invoke this only after
    /// proving the outer `lhs` is the destination accumulator.
    fn collect_append_pieces<'expr>(expr: &'expr HirExpr, out: &mut Vec<&'expr HirExpr>) {
        if let HirExprKind::Call { callee, args } = &expr.kind
            && matches!(
                &callee.kind,
                HirExprKind::Path { segments, .. }
                    if segments.len() == 1 && segments[0].name.as_str() == "__concat"
            )
        {
            out.extend(args.iter());
            return;
        }
        if let HirExprKind::Binary {
            op: HirBinaryOp::Add,
            lhs,
            rhs,
        } = &expr.kind
        {
            Self::collect_append_pieces(lhs, out);
            Self::collect_append_pieces(rhs, out);
            return;
        }
        out.push(expr);
    }

    /// For a `*s` deref-assign place whose `s` is a bare local of type
    /// `&_ String`, returns `s`'s local; otherwise `None`. Used to route
    /// `&mut String` deref appends/overwrites onto the in-place append and
    /// release-old paths.
    fn deref_string_place_local(&self, expr: &HirExpr) -> Option<Local> {
        use gossamer_types::TyKind;
        let HirExprKind::Unary {
            op: HirUnaryOp::Deref,
            operand,
        } = &expr.kind
        else {
            return None;
        };
        let HirExprKind::Path { segments, .. } = &operand.kind else {
            return None;
        };
        let [seg] = segments.as_slice() else {
            return None;
        };
        let local = self.lookup_local(&seg.name)?;
        match self.tcx.kind_of(self.locals[local.0 as usize].ty) {
            TyKind::Ref { inner, .. } if matches!(self.tcx.kind_of(*inner), TyKind::String) => {
                Some(local)
            }
            _ => None,
        }
    }

    /// The `&mut` parameter a `*p` place reads through, with the runtime
    /// helper that replaces its container's contents, when the pointee is a
    /// type that crosses a call as its own handle (`Vec`, a slice view,
    /// `Map`, `Set`, `Deque`, a queue, a stack, or a heap). A `&mut` binding
    /// made inside the body aliases its source local instead and is not a
    /// `Ref`-typed local, so it never answers here.
    fn deref_container_assign_target(&self, expr: &HirExpr) -> Option<(Local, &'static str)> {
        use gossamer_types::TyKind;
        let HirExprKind::Unary {
            op: HirUnaryOp::Deref,
            operand,
        } = &expr.kind
        else {
            return None;
        };
        let HirExprKind::Path { segments, .. } = &operand.kind else {
            return None;
        };
        let [seg] = segments.as_slice() else {
            return None;
        };
        let local = self.lookup_local(&seg.name)?;
        if !self.param_locals.contains(&local) {
            return None;
        }
        let TyKind::Ref { inner, .. } = self.tcx.kind_of(self.locals[local.0 as usize].ty) else {
            return None;
        };
        let symbol = match self.tcx.kind_of(*inner) {
            TyKind::Vec(_) | TyKind::Slice(_) => "gos_rt_vec_assign",
            TyKind::HashMap { .. } => "gos_rt_map_assign",
            _ => match self.map_or_set_clone_symbol(*inner)? {
                "gos_rt_set_clone" => "gos_rt_set_assign",
                "gos_rt_vec_clone" => "gos_rt_vec_assign",
                _ => "gos_rt_deque_assign",
            },
        };
        Some((local, symbol))
    }

    /// Lowers `*s += piece` (deref `&mut String` accumulator) to one
    /// self-consuming append (`gos_rt_str_concat_drop_a` / `_append_i64` /
    /// `_append_f64`) per piece, each reading the slot's current value,
    /// appending, and storing the result back through the slot. The append
    /// helper consumes the old buffer in place, so the RC pass recognises the
    /// `tmp = append(*s, piece); *s = Copy(tmp)` copy-back and emits neither a
    /// release of the old value nor a retain of the result.
    ///
    /// `*s += format!("{}", n)` lowers to `*s += __concat(pieces...)`; this
    /// detects that `__concat` and appends each formatted piece straight onto
    /// the deref accumulator, skipping the throwaway String the concat would
    /// otherwise build (the deref counterpart of `try_lower_append_fused`). A
    /// non-`__concat` `piece` is treated as a single-element piece list.
    /// Returns `false` (no emission) when any piece is not a `String` / `i64`
    /// / `f64`, so the caller falls back to the general path.
    fn try_lower_deref_append_fused(
        &mut self,
        s_local: Local,
        piece: &HirExpr,
        span: Span,
    ) -> bool {
        use gossamer_types::{FloatTy, IntTy, TyKind};
        let append_fn = |this: &Self, ty: Ty| -> Option<&'static str> {
            let mut cur = ty;
            while let TyKind::Ref { inner, .. } = this.tcx.kind_of(cur) {
                cur = *inner;
            }
            match this.tcx.kind_of(cur) {
                TyKind::String => Some("gos_rt_str_concat_drop_a"),
                TyKind::Int(IntTy::I64) => Some("gos_rt_str_append_i64"),
                TyKind::Float(FloatTy::F64) => Some("gos_rt_str_append_f64"),
                _ => None,
            }
        };
        let pieces: &[HirExpr] = if let HirExprKind::Call { callee, args } = &piece.kind
            && matches!(
                &callee.kind,
                HirExprKind::Path { segments, .. }
                    if segments.len() == 1 && segments[0].name.as_str() == "__concat"
            ) {
            args.as_slice()
        } else {
            std::slice::from_ref(piece)
        };
        if pieces.is_empty() {
            return false;
        }
        // Per-piece appends need every piece to be a known scalar / String. A
        // piece whose type is unresolved (e.g. an enum-payload binding the
        // checker left as an inference var inside `format!("{}", n)`) collapses
        // the whole `piece` back to a single String append: its runtime value
        // is always a String (a `format!` / `__concat` result, or a String
        // operand), so `gos_rt_str_concat_drop_a` appends it in place. This
        // keeps the deref accumulator on the correct in-place path instead of
        // falling through to the general `*s + piece` lowering, which reads the
        // `&mut String` slot pointer as string bytes.
        let per_piece: Option<Vec<&'static str>> = pieces
            .iter()
            .map(|p| append_fn(self, self.resolved_piece_ty(p)))
            .collect();
        let (emit_pieces, fns): (Vec<&HirExpr>, Vec<&'static str>) = match per_piece {
            Some(fns) => (pieces.iter().collect(), fns),
            None => (vec![piece], vec!["gos_rt_str_concat_drop_a"]),
        };
        let string_ty = self.tcx.string_ty();
        for (p, fname) in emit_pieces.into_iter().zip(fns) {
            // A string-literal piece carries a compile-time byte length, so it
            // appends through `gos_rt_str_append_bytes` (length-counted, no
            // per-call strlen) which the LLVM tier inlines to a capacity-check
            // + memcpy. Non-literal String pieces keep `concat_drop_a`.
            let literal_len = match &p.kind {
                HirExprKind::Literal(gossamer_hir::HirLiteral::String(s))
                    if fname == "gos_rt_str_concat_drop_a" =>
                {
                    Some(s.len() as i128)
                }
                _ => None,
            };
            let Some(piece_local) = self.lower_expr(p) else {
                return true;
            };
            let piece_local = self.auto_deref_cell(piece_local, span);
            let deref_place = Place {
                local: s_local,
                projection: vec![crate::ir::Projection::Deref],
            };
            let tmp = self.fresh(string_ty);
            let next = self.new_block(span);
            let (call_name, call_args) = match literal_len {
                Some(len) => (
                    "gos_rt_str_append_bytes",
                    vec![
                        Operand::Copy(deref_place.clone()),
                        Operand::Copy(Place::local(piece_local)),
                        Operand::Const(ConstValue::Int(len)),
                    ],
                ),
                None => (
                    fname,
                    vec![
                        Operand::Copy(deref_place.clone()),
                        Operand::Copy(Place::local(piece_local)),
                    ],
                ),
            };
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(call_name.to_string())),
                args: call_args,
                destination: Place::local(tmp),
                target: Some(next),
            });
            self.set_current(next);
            self.emit_assign(
                deref_place,
                Rvalue::Use(Operand::Copy(Place::local(tmp))),
                span,
            );
        }
        true
    }

    pub(crate) fn lower_place_expr(&mut self, expr: &HirExpr) -> Option<Place> {
        match &expr.kind {
            HirExprKind::Path { segments, .. } => {
                let first = segments.first()?;
                let local = self.lookup_local(&first.name)?;
                Some(Place::local(local))
            }
            HirExprKind::Field { receiver, name } => {
                let mut base = self.lower_place_expr(receiver)?;
                // Resolve which struct's field ordering to use.
                // Prefer the receiver's static type - for nested
                // projections (`o.inner.x`) the receiver expression
                // is `o.inner` whose type is `Inner`, while
                // `local_struct[base.local]` would point at `o`'s
                // root struct `Outer` and miss the field. The
                // local-struct fallback is kept for cases where
                // type information is partial (inference variables
                // leaking through).
                let struct_name = self
                    .struct_name_from_expr(receiver)
                    .or_else(|| self.local_struct.get(&base.local).cloned())?;
                let order = self.structs.get(&struct_name)?;
                let idx = u32::try_from(order.iter().position(|f| f == &name.name)?).ok()?;
                base.projection.push(crate::ir::Projection::Field(idx));
                Some(base)
            }
            HirExprKind::TupleIndex { receiver, index } => {
                let mut base = self.lower_place_expr(receiver)?;
                base.projection.push(crate::ir::Projection::Field(*index));
                Some(base)
            }
            HirExprKind::Index { base, index } => {
                // For a Vec / Slice base whose elements are multi-slot
                // aggregates (`bodies[i].x` over a `Vec<Body>`), a flat
                // `Projection::Index` would treat the local's value as
                // an inline element buffer - but the local holds a
                // `*mut GosVec` *header*, so the index strides off the
                // header fields instead of the data buffer (every
                // access past element 0 reads garbage). Route through
                // `lower_index_access`, which emits
                // `gos_rt_vec_get_ptr` to materialise the real element
                // address; the returned local carries the element's
                // struct tag so the appended `Field` projection
                // resolves correctly.
                // Prefer the base local's MIR-resolved type over the
                // HIR expression type: a `let mut bodies: [Body; N]`
                // binding is promoted to `Vec<Body>` on the MIR side
                // (its local type is rewritten), but the HIR `base.ty`
                // still reads `[Body; N]`. Using only the HIR type
                // would miss the promotion and fall through to the
                // flat-projection path that strides off the GosVec
                // header.
                let base_ty_effective = match &base.kind {
                    HirExprKind::Path { segments, .. } => segments
                        .first()
                        .and_then(|seg| self.lookup_local(&seg.name))
                        .map_or(base.ty, |l| self.locals[l.0 as usize].ty),
                    _ => base.ty,
                };
                let base_kind = {
                    use gossamer_types::TyKind;
                    let raw = self.tcx.kind_of(base_ty_effective).clone();
                    match raw {
                        TyKind::Ref { inner, .. } => self.tcx.kind_of(inner).clone(),
                        other => other,
                    }
                };
                let tagged_elem_ty = if let HirExprKind::Path { segments, .. } = &base.kind {
                    segments
                        .first()
                        .and_then(|seg| self.lookup_local(&seg.name))
                        .and_then(|local| self.local_elem_struct.get(&local))
                        .and_then(|elem_struct| {
                            self.struct_defs.iter().find_map(|(def, name)| {
                                (name == elem_struct).then(|| {
                                    self.tcx.intern(gossamer_types::TyKind::Adt {
                                        def: *def,
                                        substs: gossamer_types::Substs::new(),
                                    })
                                })
                            })
                        })
                } else {
                    None
                };
                let vec_elem = match base_kind {
                    gossamer_types::TyKind::Vec(elem) | gossamer_types::TyKind::Slice(elem) => {
                        Some(tagged_elem_ty.unwrap_or(elem))
                    }
                    gossamer_types::TyKind::Var(_) | gossamer_types::TyKind::Error => {
                        tagged_elem_ty
                    }
                    _ => None,
                };
                if let Some(elem) = vec_elem {
                    // Any struct/tuple element lives by value inside the Vec's
                    // data buffer, so a `vec[i].field` place must take the
                    // element's address (`gos_rt_vec_get_ptr`) and walk the
                    // `Field` projection from there. A single-pointer-field
                    // struct (slot_bytes == 8) was previously excluded by a
                    // `> 8` gate and fell through to the flat-projection path,
                    // which strides off the GosVec header instead of its data
                    // pointer (segfault on `buckets[i].items.push(...)`).
                    let elem_is_user_struct = self.struct_name_of(elem).is_some_and(|name| {
                        self.struct_defs
                            .values()
                            .any(|struct_name| struct_name == &name)
                    });
                    let elem_aggregate = matches!(
                        self.tcx.kind_of(elem),
                        gossamer_types::TyKind::Tuple(_) | gossamer_types::TyKind::Adt { .. }
                    ) && (self.type_slot_bytes(elem) >= 8
                        || elem_is_user_struct);
                    if elem_aggregate {
                        // Materialise the element address with
                        // `gos_rt_vec_get_ptr` and bind it to a
                        // `&elem`-typed local. A reference-typed local
                        // makes the backend auto-deref it (load the
                        // stored pointer) before walking the appended
                        // `Field` projection, so both reads *and*
                        // writes land on the element inside the Vec's
                        // data buffer rather than a stack copy. The
                        // element's struct tag is propagated so the
                        // `Field` arm can resolve field indices.
                        use gossamer_types::{Mutbl, TyKind};
                        let base_place = self.lower_place_expr(base)?;
                        // The helper takes the Vec handle itself. A Vec
                        // reached through a projection (`self.columns[i]`,
                        // `rows[j].cells[i]`) has to be loaded out of its
                        // holder first: passing the holder's own local would
                        // hand the enclosing struct to a helper that reads it
                        // as a `GosVec`.
                        let vec_local = if base_place.projection.is_empty() {
                            base_place.local
                        } else {
                            let holder = self.fresh(base.ty);
                            self.emit_assign(
                                Place::local(holder),
                                Rvalue::Use(Operand::Copy(base_place.clone())),
                                expr.span,
                            );
                            holder
                        };
                        let index_local = self.lower_expr(index)?;
                        self.emit_vec_index_bounds_assert(vec_local, index_local, expr.span);
                        let ref_ty = self.tcx.intern(TyKind::Ref {
                            mutability: Mutbl::Mut,
                            inner: elem,
                        });
                        let ptr_local = self.fresh(ref_ty);
                        let next = self.new_block(expr.span);
                        self.terminate(Terminator::Call {
                            callee: Operand::Const(ConstValue::Str(
                                "gos_rt_vec_get_ptr".to_string(),
                            )),
                            args: vec![
                                Operand::Copy(Place::local(vec_local)),
                                Operand::Copy(Place::local(index_local)),
                            ],
                            destination: Place::local(ptr_local),
                            target: Some(next),
                        });
                        self.set_current(next);
                        if let Some(elem_struct) =
                            self.local_elem_struct.get(&base_place.local).cloned()
                        {
                            self.local_struct.insert(ptr_local, elem_struct);
                        } else if let Some(name) = self.struct_name_of(elem) {
                            self.local_struct.insert(ptr_local, name);
                        }
                        return Some(Place::local(ptr_local));
                    }
                }
                let mut base_place = self.lower_place_expr(base)?;
                let index_local = self.lower_expr(index)?;
                base_place
                    .projection
                    .push(crate::ir::Projection::Index(index_local));
                Some(base_place)
            }
            // `*operand = ...` - deref-assign through a `&mut T` /
            // `*mut T`. The Place is the base local with a `Deref`
            // projection appended; the lowerer's `Place::Deref`
            // arm in cranelift/LLVM stores through the pointer.
            HirExprKind::Unary {
                op: gossamer_hir::HirUnaryOp::Deref,
                operand,
            } => {
                if let HirExprKind::Path { segments, .. } = &operand.kind
                    && let [name] = segments.as_slice()
                    && self.reference_alias_local(&name.name).is_some()
                {
                    return self.lower_place_expr(operand);
                }
                let mut base = self.lower_place_expr(operand)?;
                base.projection.push(crate::ir::Projection::Deref);
                Some(base)
            }
            _ => None,
        }
    }

    pub(crate) fn lower_tuple(&mut self, elems: &[HirExpr], ty: Ty, span: Span) -> Option<Local> {
        use gossamer_types::TyKind;
        // Declared element types (when `ty` resolved to a concrete
        // Tuple) let us coerce a flat `[T; N]` array-literal element
        // into a heap `GosVec` when the tuple slot is declared `[T]`
        // (e.g. the `(String, [u8])` pairs passed to
        // `archive::tar::write`). Without this the inline array
        // widens the tuple and the consumer reads element[0] as the
        // Vec pointer.
        let elem_tys: Option<Vec<Ty>> = match self.tcx.kind_of(ty) {
            TyKind::Tuple(tys) => Some(tys.clone()),
            _ => None,
        };
        let mut operands = Vec::with_capacity(elems.len());
        for (i, elem) in elems.iter().enumerate() {
            let mut local = self.lower_expr(elem)?;
            if let Some(field_ty) = elem_tys.as_ref().and_then(|t| t.get(i)).copied() {
                let val_ty = self.locals[local.0 as usize].ty;
                if let TyKind::Array { elem: e, len } = self.tcx.kind_of(val_ty).clone()
                    && matches!(
                        self.tcx.kind_of(field_ty),
                        TyKind::Vec(_) | TyKind::Slice(_)
                    )
                {
                    local = self.coerce_array_to_vec(local, e, len, span);
                }
            }
            operands.push(Operand::Copy(Place::local(local)));
        }
        let dest = self.fresh(ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::Aggregate {
                kind: crate::ir::AggregateKind::Tuple,
                operands,
            },
            span,
        );
        Some(dest)
    }

    pub(crate) fn lower_tuple_index(
        &mut self,
        receiver: &HirExpr,
        index: u32,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        // Fast path: a tuple index of a PLACE expression (`table[j].1`,
        // `p.pair.0`) reads the field through one combined projection instead
        // of copying the whole tuple out and then extracting a field - the hot
        // `table[j].0`/`.1` shape (fasta). Restricted to a concrete field type
        // so the backend picks the right load kind; the unannotated (`Var`)
        // case keeps the materialising slow path, which first pins the
        // receiver's tuple field types.
        {
            use gossamer_types::TyKind;
            let field_concrete = !matches!(self.tcx.kind_of(ty), TyKind::Var(_) | TyKind::Error);
            if field_concrete
                && matches!(
                    receiver.kind,
                    HirExprKind::Path { .. }
                        | HirExprKind::Index { .. }
                        | HirExprKind::Field { .. }
                        | HirExprKind::TupleIndex { .. }
                )
                && let Some(mut place) = self.lower_place_expr(receiver)
            {
                place.projection.push(crate::ir::Projection::Field(index));
                let dest = self.fresh(ty);
                self.emit_assign(Place::local(dest), Rvalue::Use(Operand::Copy(place)), span);
                return Some(dest);
            }
        }
        let receiver_local = self.lower_expr(receiver)?;
        // A tuple element read out of a Vec (`v[i].0`) inherits its
        // field types from the element type the binding was pinned
        // to; when that came from an unannotated `let mut xs = []`
        // the int-literal fields can remain `Var`, which lowers to a
        // `ptr` load (and the value is then mis-dispatched as a
        // string). Pin the receiver's tuple field types to i64 so the
        // field load reads an integer slot.
        let recv_ty = self.locals[receiver_local.0 as usize].ty;
        let resolved_recv_ty = self.resolve_var_tuple_fields(recv_ty);
        if resolved_recv_ty != recv_ty {
            self.locals[receiver_local.0 as usize].ty = resolved_recv_ty;
        }
        // Pin the destination to the resolved field type when the
        // expression's own type is still unresolved.
        let dest_ty = if matches!(
            self.tcx.kind_of(ty),
            gossamer_types::TyKind::Var(_) | gossamer_types::TyKind::Error
        ) {
            match self.tcx.kind_of(resolved_recv_ty) {
                gossamer_types::TyKind::Tuple(fields) => {
                    fields.get(index as usize).copied().unwrap_or(ty)
                }
                _ => ty,
            }
        } else {
            ty
        };
        let dest = self.fresh(dest_ty);
        let place = Place {
            local: receiver_local,
            projection: vec![crate::ir::Projection::Field(index)],
        };
        self.emit_assign(Place::local(dest), Rvalue::Use(Operand::Copy(place)), span);
        Some(dest)
    }

    /// Emits a bounds `Assert` (`0 <= index < vec.len()`) before an
    /// aggregate Vec element is addressed. Primitive Vec elements keep the
    /// lenient zero-value-on-OOB contract; an aggregate element cannot
    /// cheaply yield a zero value, and `gos_rt_vec_get_ptr` hands back a
    /// null slot pointer on OOB which the appended `Field`/`Index`
    /// projection would dereference. The wired `BoundsCheck` assert turns
    /// that into a clean "index out of bounds" panic, bit-identical across
    /// the VM, Cranelift, and LLVM tiers, and also rejects OOB aggregate
    /// writes (`v[i].field = x`) instead of corrupting memory.
    pub(crate) fn emit_vec_index_bounds_assert(
        &mut self,
        vec_local: Local,
        index_local: Local,
        span: Span,
    ) {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let len = self.fresh(i64_ty);
        let after_len = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
            args: vec![Operand::Copy(Place::local(vec_local))],
            destination: Place::local(len),
            target: Some(after_len),
        });
        self.set_current(after_len);
        let ge0 = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(ge0),
            Rvalue::BinaryOp {
                op: BinOp::Ge,
                lhs: Operand::Copy(Place::local(index_local)),
                rhs: Operand::Const(ConstValue::Int(0)),
            },
            span,
        );
        let lt = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(lt),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(index_local)),
                rhs: Operand::Copy(Place::local(len)),
            },
            span,
        );
        let in_bounds = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(in_bounds),
            Rvalue::BinaryOp {
                op: BinOp::BitAnd,
                lhs: Operand::Copy(Place::local(ge0)),
                rhs: Operand::Copy(Place::local(lt)),
            },
            span,
        );
        let ok = self.new_block(span);
        self.terminate(Terminator::Assert {
            cond: Operand::Copy(Place::local(in_bounds)),
            expected: true,
            msg: AssertMessage::BoundsCheck,
            target: ok,
        });
        self.set_current(ok);
    }

    pub(crate) fn lower_index_access(
        &mut self,
        base: &HirExpr,
        index: &HirExpr,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        // `a[i]` on a user struct / enum routes to its `index` impl method.
        // (A range index `a[lo..hi]` keeps the slice path below.)
        if !matches!(index.kind, HirExprKind::Range { .. })
            && let Some(sname) = self.adt_dispatch_name(base.ty)
        {
            let mangled = format!("{sname}::index");
            if self.impl_methods.contains_key(&mangled) {
                let base_local = self.lower_expr(base)?;
                let index_local = self.lower_expr(index)?;
                let dest = self.fresh(ty);
                let next = self.new_block(span);
                self.terminate(Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(mangled)),
                    args: vec![
                        Operand::Copy(Place::local(base_local)),
                        Operand::Copy(Place::local(index_local)),
                    ],
                    destination: Place::local(dest),
                    target: Some(next),
                });
                self.set_current(next);
                return Some(dest);
            }
        }
        // Slice expression `arr[lo..hi]`: the index is a Range
        // value rather than a single integer. Route through a
        // runtime slice helper. Returns a `*mut GosVec` so the
        // surrounding code can iterate or `to_vec()` on it.
        if let HirExprKind::Range {
            start,
            end,
            inclusive,
        } = &index.kind
        {
            let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let mut base_local = self.lower_expr(base)?;
            let mut base_kind = self.locals[base_local.0 as usize].ty;
            while let TyKind::Ref { inner, .. } = self.tcx.kind_of(base_kind) {
                base_kind = *inner;
            }
            let base_is_string = matches!(self.tcx.kind_of(base_kind), TyKind::String);
            let base_array = match self.tcx.kind_of(base_kind).clone() {
                TyKind::Array { elem, len } => Some((elem, len)),
                _ => None,
            };
            let lo_local = if let Some(s) = start {
                self.lower_expr(s)?
            } else {
                let l = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(l),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                l
            };
            let hi_local = if let Some(e) = end {
                self.lower_expr(e)?
            } else {
                // `arr[lo..]` / `s[lo..]` - substitute `.len()` as
                // the upper bound. Fixed arrays carry a static length;
                // strings need their c-string helper; Vecs and slices use
                // the len-prefixed sequence helper.
                let l = self.fresh(i64_ty);
                if let Some((_, len)) = base_array {
                    self.emit_assign(
                        Place::local(l),
                        Rvalue::Use(Operand::Const(ConstValue::Int(len.to_usize() as i128))),
                        span,
                    );
                } else {
                    let next = self.new_block(span);
                    let len_callee = if base_is_string {
                        "gos_rt_str_len"
                    } else {
                        "gos_rt_len"
                    };
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(len_callee.to_string())),
                        args: vec![Operand::Copy(Place::local(base_local))],
                        destination: Place::local(l),
                        target: Some(next),
                    });
                    self.set_current(next);
                }
                l
            };
            let hi_local = if *inclusive {
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
                        lhs: Operand::Copy(Place::local(hi_local)),
                        rhs: Operand::Copy(Place::local(one)),
                    },
                    span,
                );
                bumped
            } else {
                hi_local
            };
            if let Some((elem, len)) = base_array {
                base_local = self.coerce_array_to_vec(base_local, elem, len, span);
            }
            let dest_ty = ty;
            let dest = self.fresh(dest_ty);
            let next = self.new_block(span);
            let callee = if base_is_string {
                "gos_rt_str_substring"
            } else {
                "gos_rt_vec_slice"
            };
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(callee.to_string())),
                args: vec![
                    Operand::Copy(Place::local(base_local)),
                    Operand::Copy(Place::local(lo_local)),
                    Operand::Copy(Place::local(hi_local)),
                ],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return Some(dest);
        }
        // Walk through references so `&String` indexing behaves
        // the same as indexing a bare `String`. Prefer the MIR
        // local's pinned type over the HIR type when the base is
        // a simple Path - the type checker may have left the HIR
        // type as an unresolved inference variable for receivers
        // produced by runtime helpers (e.g. `read_to_string`),
        // and the indexing path needs the concrete `String` to
        // route to the Unicode scalar helper instead of falling
        // through to the array-projection helper.
        let mut base_kind = self
            .receiver_local_from_path(base)
            .map_or(base.ty, |local| self.locals[local.0 as usize].ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(base_kind) {
            base_kind = *inner;
        }
        let base_is_string = matches!(self.tcx.kind_of(base_kind), TyKind::String);
        if base_is_string {
            let base_local = self.lower_expr(base)?;
            let index_local = self.lower_expr(index)?;
            let dest_ty = self.tcx.char_ty();
            let dest = self.fresh(dest_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_str_char_at".to_string())),
                args: vec![
                    Operand::Copy(Place::local(base_local)),
                    Operand::Copy(Place::local(index_local)),
                ],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return Some(dest);
        }
        let base_local = self.lower_expr(base)?;
        let index_local = self.lower_expr(index)?;
        // For Vec / Slice receivers (whose runtime layout is a
        // `*mut GosVec` header, not a flat element buffer) route
        // index reads through `gos_rt_vec_get_i64`. A naked
        // `Projection::Index` would treat the local's first 8
        // bytes as element 0 - which is the GosVec `len` field,
        // not the data buffer.
        let actual_base_kind = self
            .tcx
            .kind_of(self.locals[base_local.0 as usize].ty)
            .clone();
        let actual_base_kind = match actual_base_kind {
            TyKind::Ref { inner, .. } => self.tcx.kind_of(inner).clone(),
            other => other,
        };
        if let TyKind::Vec(elem) | TyKind::Slice(elem) = actual_base_kind {
            // Pin the destination's MIR type to the element type
            // recorded on the base. The HIR-side `ty` is sometimes
            // an inference variable that lost contact with the
            // concrete element kind by the time we get here (e.g.
            // for the result of `flag::Set::parse`'s `Vec<String>`
            // routed through a `?` Try), and printing/formatting
            // dispatch then routes the i64 ptr through the integer
            // helper instead of the string helper.
            //
            // `Vec<Option<T>>` / `Vec<Result<T, _>>` store each
            // element as the wrapped tagged-union handle (a
            // `*mut [disc, payload]` cell), exactly like any other
            // enum - `xs.push(None)` and `xs.push(Some(v))` must be
            // distinguishable at read time. Read the element back as
            // the wrapper so `match v[i] { Some(k) => …, None => … }`
            // sees the real discriminant; the `Some(k)` arm still
            // binds `k: T`. (An earlier "peel to the unwrapped T"
            // shortcut assumed a happy-path encoding the compiled
            // push side never used, so `xs[i]` handed back the raw
            // handle pointer as if it were the payload.)
            let expected_elem_ty = self
                .struct_name_of(ty)
                .filter(|name| {
                    self.struct_defs
                        .values()
                        .any(|struct_name| struct_name == name)
                })
                .map(|_| ty);
            let tagged_elem_ty = self
                .local_elem_struct
                .get(&base_local)
                .and_then(|elem_struct| {
                    self.struct_defs.iter().find_map(|(def, name)| {
                        (name == elem_struct).then(|| {
                            self.tcx.intern(TyKind::Adt {
                                def: *def,
                                substs: gossamer_types::Substs::new(),
                            })
                        })
                    })
                });
            let elem_unwrapped = expected_elem_ty.or(tagged_elem_ty).unwrap_or(elem);
            let elem_kind_now = self.tcx.kind_of(elem_unwrapped);
            let dest_ty = match elem_kind_now {
                TyKind::String | TyKind::Bool | TyKind::Char | TyKind::Float(_) => elem_unwrapped,
                TyKind::Int(_) => elem_unwrapped,
                // Aggregate elements (struct, tuple, fixed array) -
                // keep the pinned element type so subsequent
                // `Field(idx)` / `Index(k)` projections find the
                // right slot layout. A `Vec<[i64; 2]>` element is a
                // multi-slot inline array; preserving the `Array`
                // type lets the chained `v[i][j]` read each slot.
                TyKind::Adt { .. } | TyKind::Tuple(_) | TyKind::Array { .. } => elem_unwrapped,
                // `Vec<json::Value>` indexing must produce a
                // `JsonValue`-typed local so subsequent
                // `json::get(&v[i], ...)` / `v[i].clone()` dispatch
                // through the json runtime helpers. Without this
                // pin, the dest fell through to the i64 default
                // and `tcs[k].clone()` (askq) lost the json tag
                // - every nested field probe missed.
                TyKind::JsonValue => elem_unwrapped,
                // `Vec<Vec<T>>` indexing: preserve the inner Vec type
                // so that subsequent indexing on the result routes
                // through `gos_rt_vec_get_i64` rather than direct
                // pointer arithmetic on the GosVec header.
                // Without this, `caps[0]` on a `Vec<Vec<String>>`
                // returns a local typed `i64`, and `row[1]` then reads
                // the GosVec `cap` field instead of element 1.
                TyKind::Vec(_) | TyKind::Slice(_) => elem_unwrapped,
                // A generic element keeps its parameter so specialisation can
                // resolve it. Collapsing it to the scalar default here erases
                // the only record of what the element is, and the copy made
                // for a concrete instantiation then reads an aggregate element
                // as its first eight bytes.
                TyKind::Param { .. } => elem_unwrapped,
                _ => match self.tcx.kind_of(ty) {
                    TyKind::Int(_) | TyKind::String | TyKind::Bool | TyKind::Float(_) => ty,
                    _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
                },
            };
            // Multi-slot element types (tuples, named structs with
            // multiple fields) need `gos_rt_vec_get_ptr` - the
            // single-slot `gos_rt_vec_get_i64` only reads the first
            // 8 bytes of the slot, so a `(String, String)` element
            // would hand back just the first String and the
            // subsequent `.0` / `.1` projection would dereference
            // that c-string ptr as if it were a tuple aggregate.
            // A by-value `Result`/`Option` element is a 16-byte `i128` read
            // through the dedicated helper (not the 8-byte `get_i64`, which
            // would drop the payload, nor the aggregate-address `get_ptr`).
            let elem_is_result_option = matches!(
                self.tcx.kind_of(elem_unwrapped),
                TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
            );
            // A user struct is address-is-value even at a single 8-byte
            // slot - its `.field` projections dereference off the slot
            // pointer, so it must come back through `gos_rt_vec_get_ptr`
            // like any multi-slot aggregate. Read as an `i64` scalar it
            // would hand back the boxed pointer's bits, and the following
            // `.field` access would dereference a wrong address. Key this
            // off the resolved struct name rather than numeric DefId ranges:
            // user code can flow through inferred repeat-array/Vec shapes
            // that still name the struct even when the raw local id is not
            // useful for distinguishing it from stdlib sentinels. Opaque
            // stdlib handle structs are absent from `struct_defs` and stay on
            // the scalar read; enums (no struct fields) keep the get_i64
            // handle the matcher wants.
            let elem_is_user_struct = self.struct_name_of(elem_unwrapped).is_some_and(|name| {
                self.struct_defs
                    .values()
                    .any(|struct_name| struct_name == &name)
            });
            let elem_is_struct_adt = matches!(
                self.tcx.kind_of(elem_unwrapped),
                TyKind::Adt { def, .. }
                    if elem_is_user_struct && self.tcx.struct_field_tys(*def).is_some()
            );
            // A tuple element is address-is-value at any width, exactly like a
            // user struct - a single-element tuple `(T,)` occupies one 8-byte
            // slot yet its `.0` projection still dereferences off the slot
            // pointer, so it must come back through `gos_rt_vec_get_ptr` too.
            // (`Result`/`Option` are handled by `elem_is_result_option` above.)
            let elem_is_tuple = matches!(self.tcx.kind_of(elem_unwrapped), TyKind::Tuple(_));
            let elem_is_multislot = self.tcx.elem_is_addressed_aggregate(elem_unwrapped)
                || elem_is_struct_adt
                || elem_is_tuple;
            let (helper, dest_ty) = if elem_is_result_option {
                ("gos_rt_vec_get_i128", elem_unwrapped)
            } else if elem_is_multislot {
                ("gos_rt_vec_get_ptr", dest_ty)
            } else {
                ("gos_rt_vec_get_i64", dest_ty)
            };
            // Aggregate elements (struct/tuple/array, including by-value
            // Result/Option) are addressed or read whole; an OOB read would
            // dereference a null slot pointer or fabricate a bogus
            // discriminant. Guard with the wired bounds assert. Primitive
            // elements keep the lenient zero-value-on-OOB behavior.
            if matches!(
                self.tcx.kind_of(elem_unwrapped),
                TyKind::Tuple(_) | TyKind::Adt { .. } | TyKind::Array { .. }
            ) {
                self.emit_vec_index_bounds_assert(base_local, index_local, span);
            }
            let dest = self.fresh(dest_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(helper.to_string())),
                args: vec![
                    Operand::Copy(Place::local(base_local)),
                    Operand::Copy(Place::local(index_local)),
                ],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            // Propagate the base's element-struct tag onto the
            // result so subsequent `entries[i].<field>` access
            // (or `let entry = entries[i]; entry.<field>`)
            // resolves through `lookup_place_expr`'s struct-name
            // path instead of falling through to JsonValue.
            if let Some(elem_struct) = self.local_elem_struct.get(&base_local).cloned() {
                self.local_struct.insert(dest, elem_struct);
            } else if let Some(name) = self.struct_name_of(elem) {
                self.local_struct.insert(dest, name);
            }
            return Some(dest);
        }
        // Fixed-array index (`a[j]` where `a: [T; N]`). When the
        // array came out of a multi-slot Vec element (`v[i][j]`) its
        // element type can still be an inference variable, which the
        // codegen would load as a `ptr` and then mis-dispatch (e.g.
        // concat as a string). Resolve a `Var` element to i64 on the
        // base local and pin the destination to match.
        let base_ty = self.locals[base_local.0 as usize].ty;
        let resolved_base_ty = self.resolve_var_tuple_fields(base_ty);
        if resolved_base_ty != base_ty {
            self.locals[base_local.0 as usize].ty = resolved_base_ty;
        }
        let dest_ty = if matches!(self.tcx.kind_of(ty), TyKind::Var(_) | TyKind::Error) {
            match self.tcx.kind_of(resolved_base_ty) {
                TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => *elem,
                _ => ty,
            }
        } else {
            ty
        };
        let dest = self.fresh(dest_ty);
        let place = Place {
            local: base_local,
            projection: vec![crate::ir::Projection::Index(index_local)],
        };
        self.emit_assign(Place::local(dest), Rvalue::Use(Operand::Copy(place)), span);
        if let Some(elem_struct) = self.local_elem_struct.get(&base_local).cloned() {
            self.local_struct.insert(dest, elem_struct);
        }
        Some(dest)
    }
}

/// Bit pattern of the `f64` behind a `std::math` constant, or `None`
/// if `name` is not one. Mirrors `gossamer_std::math`'s constants so
/// the compiled tiers fold them identically to the interpreter's
/// `Value::Float` globals.
/// Value of a `fs::SEEK_*` whence selector.
fn seek_whence_value(name: &str) -> Option<i128> {
    match name {
        "SEEK_SET" => Some(0),
        "SEEK_CUR" => Some(1),
        "SEEK_END" => Some(2),
        _ => None,
    }
}

fn math_const_bits(name: &str) -> Option<u64> {
    let v: f64 = match name {
        "PI" => std::f64::consts::PI,
        "E" => std::f64::consts::E,
        "SQRT_2" => std::f64::consts::SQRT_2,
        "LN_2" => std::f64::consts::LN_2,
        "LN_10" => std::f64::consts::LN_10,
        "LOG2_E" => std::f64::consts::LOG2_E,
        "LOG10_E" => std::f64::consts::LOG10_E,
        "PHI" => 1.618_033_988_749_895,
        "MAX_F64" => f64::MAX,
        "MIN_POSITIVE_F64" => f64::MIN_POSITIVE,
        "INF" => f64::INFINITY,
        "NEG_INF" => f64::NEG_INFINITY,
        "NAN" => f64::NAN,
        _ => return None,
    };
    Some(v.to_bits())
}

fn int_assoc_const(segments: &[&str]) -> Option<(gossamer_types::IntTy, i128)> {
    use gossamer_types::IntTy;
    if segments.len() != 2 {
        return None;
    }
    let ty = match segments[0] {
        "i8" => IntTy::I8,
        "i16" => IntTy::I16,
        "i32" => IntTy::I32,
        "i64" => IntTy::I64,
        "isize" => IntTy::Isize,
        "u8" => IntTy::U8,
        "u16" => IntTy::U16,
        "u32" => IntTy::U32,
        "u64" => IntTy::U64,
        "usize" => IntTy::Usize,
        _ => return None,
    };
    let value = match (ty, segments[1]) {
        (IntTy::I8, "MIN") => i128::from(i8::MIN),
        (IntTy::I8, "MAX") => i128::from(i8::MAX),
        (IntTy::I16, "MIN") => i128::from(i16::MIN),
        (IntTy::I16, "MAX") => i128::from(i16::MAX),
        (IntTy::I32, "MIN") => i128::from(i32::MIN),
        (IntTy::I32, "MAX") => i128::from(i32::MAX),
        (IntTy::I64 | IntTy::Isize, "MIN") => i128::from(i64::MIN),
        (IntTy::I64 | IntTy::Isize, "MAX") => i128::from(i64::MAX),
        (IntTy::U8, "MIN") => 0,
        (IntTy::U8, "MAX") => i128::from(u8::MAX),
        (IntTy::U16, "MIN") => 0,
        (IntTy::U16, "MAX") => i128::from(u16::MAX),
        (IntTy::U32, "MIN") => 0,
        (IntTy::U32, "MAX") => i128::from(u32::MAX),
        (IntTy::U64 | IntTy::Usize, "MIN") => 0,
        (IntTy::U64 | IntTy::Usize, "MAX") => i128::from(u64::MAX),
        _ => return None,
    };
    Some((ty, value))
}

/// Operator-overload impl-method name for an arithmetic binary operator
/// (`+` -> `add`, `-` -> `sub`, `*` -> `mul`, `/` -> `div`), or `None` for
/// operators that do not dispatch to a user method.
fn arith_overload_method(op: HirBinaryOp) -> Option<&'static str> {
    match op {
        HirBinaryOp::Add => Some("add"),
        HirBinaryOp::Sub => Some("sub"),
        HirBinaryOp::Mul => Some("mul"),
        HirBinaryOp::Div => Some("div"),
        HirBinaryOp::Rem => Some("rem"),
        HirBinaryOp::BitAnd => Some("bitand"),
        HirBinaryOp::BitOr => Some("bitor"),
        HirBinaryOp::BitXor => Some("bitxor"),
        HirBinaryOp::Shl => Some("shl"),
        HirBinaryOp::Shr => Some("shr"),
        _ => None,
    }
}
