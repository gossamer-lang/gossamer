#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
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
use std::ops::ControlFlow;

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
    /// Lowers a prelude `assert` / `assert_eq` call to a conditional
    /// `panic`, so the abort fires identically on every compiled tier.
    /// Returns a unit local (the call's value).
    /// True when an `assert_eq` operand can be rendered into the failure
    /// message. The message is built with `__concat`, which formats scalars
    /// and Strings; an aggregate has no Display form there, so those keep the
    /// bare heading rather than failing the build.
    fn assert_operand_renders(&self, hir_ty: gossamer_types::Ty, local: Local) -> bool {
        use gossamer_types::TyKind;
        let renders = |this: &Self, t: gossamer_types::Ty| {
            let mut cur = t;
            while let TyKind::Ref { inner, .. } = this.tcx.kind_of(cur) {
                cur = *inner;
            }
            matches!(
                this.tcx.kind_of(cur),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::String
            )
        };
        renders(self, hir_ty) || renders(self, self.locals[local.0 as usize].ty)
    }

    fn lower_assert(&mut self, args: &[HirExpr], eq: bool, span: Span) -> Option<Local> {
        let unit_ty = self.tcx.unit();
        let mut compared: Option<(Local, Local)> = None;
        let cond = if eq {
            let a = self.lower_expr(&args[0])?;
            let b = self.lower_expr(args.get(1)?)?;
            compared = Some((a, b));
            self.emit_structural_eq(a, b, args[0].ty, span)
        } else {
            self.lower_expr(&args[0])?
        };
        let ok = self.new_block(span);
        let fail = self.new_block(span);
        // `cond == 0` (false) jumps to the panic block.
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cond)),
            arms: vec![(0, fail)],
            default: ok,
        });
        self.set_current(fail);
        let msg_idx = if eq { 2 } else { 1 };
        let user_msg = match args.get(msg_idx) {
            Some(m) => Some(self.lower_expr(m)?),
            None => None,
        };
        // `assert_eq` names the two values that differ
        // (`assertion failed: <msg>: <a> != <b>`); `assert` panics with the
        // supplied text verbatim, or the bare heading when none was given.
        let rendered = compared.filter(|(a, b)| {
            self.assert_operand_renders(args[0].ty, *a)
                && self.assert_operand_renders(args[1].ty, *b)
        });
        let msg_local = if let Some((a, b)) = rendered {
            let mut pieces = vec![Operand::Const(ConstValue::Str(
                "assertion failed: ".to_string(),
            ))];
            if let Some(m) = user_msg {
                pieces.push(Operand::Copy(Place::local(m)));
                pieces.push(Operand::Const(ConstValue::Str(": ".to_string())));
            }
            pieces.push(Operand::Copy(Place::local(a)));
            pieces.push(Operand::Const(ConstValue::Str(" != ".to_string())));
            pieces.push(Operand::Copy(Place::local(b)));
            let s_ty = self.tcx.string_ty();
            let dest = self.fresh(s_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("__concat".to_string())),
                args: pieces,
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            dest
        } else if let Some(m) = user_msg {
            m
        } else {
            let s_ty = self.tcx.string_ty();
            let s = self.fresh(s_ty);
            self.emit_assign(
                Place::local(s),
                Rvalue::Use(Operand::Const(ConstValue::Str(
                    "assertion failed".to_string(),
                ))),
                span,
            );
            s
        };
        let panic_dest = self.fresh(unit_ty);
        let dead = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("panic".to_string())),
            args: vec![Operand::Copy(Place::local(msg_local))],
            destination: Place::local(panic_dest),
            target: Some(dead),
        });
        self.set_current(dead);
        self.terminate(Terminator::Unreachable);
        self.set_current(ok);
        let dest = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::Use(Operand::Const(ConstValue::Unit)),
            span,
        );
        Some(dest)
    }

    /// Stringify one slog field arg by its type so it crosses the FFI
    /// as a display c-string, matching the VM's `format!("{value}")`.
    fn slog_field_to_string(&mut self, local: Local, span: Span) -> Local {
        use gossamer_types::TyKind;
        let mut t = self.locals[local.0 as usize].ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(t) {
            t = *inner;
        }
        let sym = match self.tcx.kind_of(t) {
            TyKind::String => return local,
            TyKind::Int(_) => "gos_rt_i64_to_str",
            TyKind::Float(_) => "gos_rt_f64_to_str",
            TyKind::Bool => "gos_rt_bool_to_str",
            TyKind::Char => "gos_rt_char_to_str",
            _ => return local,
        };
        let string_ty = self.tcx.string_ty();
        let dest = self.fresh(string_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(sym.to_string())),
            args: vec![Operand::Copy(Place::local(local))],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    /// Lower `slog::<level>(msg, k1, v1, …)` to a call carrying the
    /// paired key/value fields as a `Vec<String>` of display c-strings.
    fn lower_slog(&mut self, sym: &str, args: &[HirExpr], span: Span) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};
        let string_ty = self.tcx.string_ty();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit_ty = self.tcx.unit();
        let msg_local = match args.first() {
            Some(a) => self.lower_expr(a)?,
            None => {
                let m = self.fresh(string_ty);
                self.emit_assign(
                    Place::local(m),
                    Rvalue::Use(Operand::Const(ConstValue::Str(String::new()))),
                    span,
                );
                m
            }
        };
        let fields_ty = self.tcx.intern(TyKind::Vec(string_ty));
        let field_count = args.len().saturating_sub(1);
        let elem_bytes = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(elem_bytes),
            Rvalue::Use(Operand::Const(ConstValue::Int(8))),
            span,
        );
        let cap = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(cap),
            Rvalue::Use(Operand::Const(ConstValue::Int(field_count as i128))),
            span,
        );
        let vec_local = self.fresh(fields_ty);
        let after_new = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_with_capacity".to_string())),
            args: vec![
                Operand::Copy(Place::local(elem_bytes)),
                Operand::Copy(Place::local(cap)),
            ],
            destination: Place::local(vec_local),
            target: Some(after_new),
        });
        self.set_current(after_new);
        for arg in &args[1..] {
            let v = self.lower_expr(arg)?;
            let s = self.slog_field_to_string(v, span);
            let push_dest = self.fresh(unit_ty);
            let after_push = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_push".to_string())),
                args: vec![
                    Operand::Copy(Place::local(vec_local)),
                    Operand::Copy(Place::local(s)),
                ],
                destination: Place::local(push_dest),
                target: Some(after_push),
            });
            self.set_current(after_push);
        }
        let dest = self.fresh(unit_ty);
        let after = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(sym.to_string())),
            args: vec![
                Operand::Copy(Place::local(msg_local)),
                Operand::Copy(Place::local(vec_local)),
            ],
            destination: Place::local(dest),
            target: Some(after),
        });
        self.set_current(after);
        Some(dest)
    }

    /// Lowers `middleware::tag(inner) -> Handler` (Go-style wrap-and-return
    /// composition). Resolves the inner handler's serve fn-address (a
    /// nested middleware serves through `gos_rt_middleware_serve`, a struct
    /// handler through its possibly ok-wrapped `{Struct}::serve`) and
    /// builds a `GosMiddleware` handle binding that env + serve address.
    /// The result is tagged `http::Middleware` so `lower_http_serve` serves
    /// it through `gos_rt_middleware_serve`.
    fn lower_middleware_wrap(&mut self, inner_expr: &HirExpr, span: Span) -> Option<Local> {
        let inner_local = self.lower_expr(inner_expr)?;
        let inner_serve = match self.local_runtime_kind.get(&inner_local).copied() {
            Some("http::Middleware") => "gos_rt_middleware_serve".to_string(),
            _ => {
                let inner_ty = self.locals[inner_local.0 as usize].ty;
                let struct_name = self.struct_name_of(inner_ty)?;
                self.handler_dispatch_symbol(format!("{struct_name}::serve"))
            }
        };
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let serve_addr = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(serve_addr),
            Rvalue::CallIntrinsic {
                name: "gos_fn_addr",
                args: vec![Operand::Const(ConstValue::Str(inner_serve))],
            },
            span,
        );
        let dest = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_middleware_new".to_string())),
            args: vec![
                Operand::Copy(Place::local(inner_local)),
                Operand::Copy(Place::local(serve_addr)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        self.local_runtime_kind.insert(dest, "http::Middleware");
        Some(dest)
    }

    /// Lowers `middleware::<name>(inner[, config]) -> Handler`. Same
    /// wrap-and-return shape as `tag`, with the transform selector and its
    /// configuration string bound into the `GosMiddleware` handle.
    fn lower_middleware_kind(
        &mut self,
        inner_expr: &HirExpr,
        kind: i64,
        config_expr: Option<&HirExpr>,
        numeric_config: bool,
        span: Span,
    ) -> Option<Local> {
        let inner_local = self.lower_expr(inner_expr)?;
        let inner_serve = self.middleware_inner_serve(inner_local)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let string_ty = self.tcx.string_ty();
        let serve_addr = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(serve_addr),
            Rvalue::CallIntrinsic {
                name: "gos_fn_addr",
                args: vec![Operand::Const(ConstValue::Str(inner_serve))],
            },
            span,
        );
        let kind_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(kind_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(kind)))),
            span,
        );
        let config_local = match config_expr {
            Some(expr) => {
                let raw = self.lower_expr(expr)?;
                if numeric_config {
                    let text = self.fresh(string_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_i64_to_str".to_string())),
                        args: vec![Operand::Copy(Place::local(raw))],
                        destination: Place::local(text),
                        target: Some(next),
                    });
                    self.set_current(next);
                    text
                } else {
                    raw
                }
            }
            None => {
                let empty = self.fresh(string_ty);
                self.emit_assign(
                    Place::local(empty),
                    Rvalue::Use(Operand::Const(ConstValue::Str(String::new()))),
                    span,
                );
                empty
            }
        };
        let dest = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_middleware_new_kind".to_string())),
            args: vec![
                Operand::Copy(Place::local(inner_local)),
                Operand::Copy(Place::local(serve_addr)),
                Operand::Copy(Place::local(kind_local)),
                Operand::Copy(Place::local(config_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        self.local_runtime_kind.insert(dest, "http::Middleware");
        Some(dest)
    }

    /// Serve fn-address symbol of a wrapped handler: a nested middleware
    /// serves through `gos_rt_middleware_serve`, a struct handler through
    /// its possibly ok-wrapped `{Struct}::serve`.
    fn middleware_inner_serve(&mut self, inner_local: Local) -> Option<String> {
        match self.local_runtime_kind.get(&inner_local).copied() {
            Some("http::Middleware") => Some("gos_rt_middleware_serve".to_string()),
            Some("http::Router") => Some("gos_rt_router_serve".to_string()),
            _ => {
                // A bare function is a handler in its own right, the shape the
                // router already accepts for a route. Middleware invokes its
                // inner handler through the two-argument `(env, request)` ABI,
                // so a plain fn reaches it through its synthesized
                // env-ignoring thunk - which in turn routes a bare-`Response`
                // return through `::__ok_wrap`, preserving the packed-Result
                // ABI the runtime reads back.
                if let Some(fn_name) = self.local_fn_name.get(&inner_local).cloned() {
                    return Some(crate::lower::helpers::handler_env_wrap_name(&fn_name));
                }
                if let Some(closure_name) = self.local_closure.get(&inner_local).cloned() {
                    return Some(self.handler_dispatch_symbol(closure_name));
                }
                let inner_ty = self.locals[inner_local.0 as usize].ty;
                let struct_name = self.struct_name_of(inner_ty)?;
                Some(self.handler_dispatch_symbol(format!("{struct_name}::serve")))
            }
        }
    }

    /// `Set::from(values)` where `values` is a sequence *value* rather than a
    /// literal list: the elements are only known at run time, so the runtime
    /// builds the set from the buffer in one call.
    fn lower_set_from_sequence(
        &mut self,
        arg: &HirExpr,
        span: Span,
        runtime_kind: &'static str,
    ) -> Option<Local> {
        let elem_ty = self.for_loop_elem_ty(arg)?;
        let is_i64 = matches!(
            map_key_kind_from(self.tcx, self.peel_ref_ty(elem_ty)),
            MapKeyKind::I64
        );
        let callee = match (runtime_kind, is_i64) {
            ("collections::BTreeSet", true) => "gos_rt_btree_set_from_vec_i64",
            ("collections::BTreeSet", false) => "gos_rt_btree_set_from_vec_str",
            (_, true) => "gos_rt_set_from_vec_i64",
            (_, false) => "gos_rt_set_from_vec_str",
        };
        let set_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let set = self.emit_stdlib_free_call(callee, set_ty, std::slice::from_ref(arg), span)?;
        self.local_runtime_kind.insert(set, runtime_kind);
        Some(set)
    }

    fn lower_set_from_array(
        &mut self,
        arg: &HirExpr,
        span: Span,
        runtime_kind: &'static str,
    ) -> Option<Local> {
        let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(items)) = &arg.kind else {
            return None;
        };
        let set_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let ctor = if runtime_kind == "collections::BTreeSet" {
            "gos_rt_btree_set_new"
        } else {
            "gos_rt_set_new"
        };
        let set = self.emit_stdlib_free_call(ctor, set_ty, &[], span)?;
        self.local_runtime_kind.insert(set, runtime_kind);
        let bool_ty = self.tcx.bool_ty();
        for item in items {
            let mut value = self.lower_expr(item)?;
            value = self.auto_deref_cell(value, item.span);
            let value_ty = self.peel_ref_ty(item.ty);
            let aggregate_desc = self
                .is_aggregate_key(value_ty)
                .then(|| self.key_descriptor(value_ty))
                .flatten();
            let rt = if aggregate_desc.is_some() {
                "gos_rt_set_insert_skey"
            } else if matches!(map_key_kind_from(self.tcx, value_ty), MapKeyKind::I64) {
                "gos_rt_set_insert_i64"
            } else {
                "gos_rt_set_insert"
            };
            let mut call_args = vec![
                Operand::Copy(Place::local(set)),
                Operand::Copy(Place::local(value)),
            ];
            if let Some(desc) = aggregate_desc {
                call_args.push(Operand::Const(ConstValue::Str(desc)));
            }
            let inserted = self.fresh(bool_ty);
            let next = self.new_block(item.span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(rt.to_string())),
                args: call_args,
                destination: Place::local(inserted),
                target: Some(next),
            });
            self.set_current(next);
        }
        Some(set)
    }

    /// `Map::from(seq)` / `BTreeMap::from(seq)` over a runtime sequence of
    /// key/value pairs, lowered to the constructor plus a counted insert
    /// loop - the same shape `for (k, v) in seq { m.insert(k, v) }` emits,
    /// so every key and value the insert path already stores works here.
    ///
    /// Returns `None` for anything but a materialised sequence of 2-tuples,
    /// leaving the fixed-array fast path each backend unrolls to run.
    /// `Map::from([(k, v), ...])` where `k` is a struct, tuple, or fixed
    /// array. Such a key is content-hashed through its slot descriptor, which
    /// only the `skey` entry points accept, so the literal's entries are
    /// inserted one by one here rather than through a backend's array walk.
    fn lower_map_from_aggregate_key_literal(
        &mut self,
        arg: &HirExpr,
        span: Span,
        ordered: bool,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(items)) = &arg.kind else {
            return None;
        };
        if items.is_empty() {
            return None;
        }
        let elem_ty = self.peel_ref_ty(items.first()?.ty);
        let TyKind::Tuple(fields) = self.tcx.kind_of(elem_ty).clone() else {
            return None;
        };
        let [key_ty, val_ty] = fields.as_slice() else {
            return None;
        };
        let (key_ty, val_ty) = (*key_ty, *val_ty);
        if !self.is_aggregate_key(key_ty) {
            return None;
        }
        let descriptor = self.key_descriptor(key_ty)?;
        let map_ty = self.tcx.intern(TyKind::HashMap {
            key: key_ty,
            value: val_ty,
            ordered,
        });
        let ctor = if ordered { "BTreeMap::new" } else { "Map::new" };
        let map = self.emit_stdlib_free_call(ctor, map_ty, &[], span)?;
        let prior_ty = self.option_payload_adt_ty(val_ty);
        let items = items.clone();
        for item in &items {
            let pair = self.lower_expr(item)?;
            let key = self.fresh(key_ty);
            self.emit_assign(
                Place::local(key),
                Rvalue::Use(Operand::Copy(Place {
                    local: pair,
                    projection: vec![crate::ir::Projection::Field(0)],
                })),
                span,
            );
            let value = self.fresh(val_ty);
            self.emit_assign(
                Place::local(value),
                Rvalue::Use(Operand::Copy(Place {
                    local: pair,
                    projection: vec![crate::ir::Projection::Field(1)],
                })),
                span,
            );
            let _ = self.emit_combinator_call(
                "gos_rt_map_insert_skey_opt",
                vec![
                    Operand::Copy(Place::local(map)),
                    Operand::Copy(Place::local(key)),
                    Operand::Const(ConstValue::Str(descriptor.clone())),
                    Operand::Copy(Place::local(value)),
                ],
                prior_ty,
                span,
            );
        }
        Some(map)
    }

    fn lower_map_from_sequence(
        &mut self,
        arg: &HirExpr,
        span: Span,
        ordered: bool,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        let raw_elem_ty = self.for_loop_elem_ty(arg)?;
        let elem_ty = self.peel_ref_ty(raw_elem_ty);
        let TyKind::Tuple(fields) = self.tcx.kind_of(elem_ty).clone() else {
            return None;
        };
        let [key_ty, val_ty] = fields.as_slice() else {
            return None;
        };
        let (key_ty, val_ty) = (*key_ty, *val_ty);
        // An array literal is a flat slot buffer, not a `GosVec`, so the
        // indexed walk below cannot read it; each backend lowers it directly
        // instead.
        if matches!(&arg.kind, HirExprKind::Array(_)) {
            return None;
        }
        let map_ty = self.tcx.intern(TyKind::HashMap {
            key: key_ty,
            value: val_ty,
            ordered,
        });
        // The loop walks the sequence by index, so it has to be a materialised
        // sequence rather than a cursor with no addressable elements.
        let seq = self.lower_expr(arg)?;
        let seq_ty = self.peel_ref_ty(self.locals[seq.0 as usize].ty);
        if !matches!(
            self.tcx.kind_of(seq_ty),
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
        ) {
            return None;
        }
        // An aggregate key content-hashes through its slot descriptor; every
        // other key shape reaches a typed entry point keyed on its own kind.
        let key_descriptor = self
            .is_aggregate_key(key_ty)
            .then(|| self.key_descriptor(key_ty))
            .flatten();
        if self.is_aggregate_key(key_ty) && key_descriptor.is_none() {
            return None;
        }
        let insert = match &key_descriptor {
            Some(_) => "gos_rt_map_insert_skey_opt",
            None => self.map_insert_helper(map_ty),
        };
        // An aggregate value crosses as one handle word: the backend copies
        // its slots into a reference-counted blob, and the blob owns each
        // heap child the copy names. The structural meta is what tells the
        // copy which children those are, so register it here exactly as the
        // `insert` method dispatch does.
        if let Some(value_ty) = self.hash_map_value_ty(map_ty) {
            let _ = self.ensure_aggr_struct_meta(value_ty);
            if self.is_inline_aggregate_ty(value_ty) {
                let _ = self.ensure_aggr_copy_meta(value_ty);
            }
        }

        let ctor = if ordered { "BTreeMap::new" } else { "Map::new" };
        let map = self.emit_stdlib_free_call(ctor, map_ty, &[], span)?;

        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let len = self.emit_combinator_call(
            "gos_rt_vec_len",
            vec![Operand::Copy(Place::local(seq))],
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
        let exit = self.new_block(span);
        self.terminate(Terminator::Goto { target: header });

        self.set_current(header);
        let cmp = self.fresh(bool_ty);
        self.emit_assign(
            Place::local(cmp),
            Rvalue::BinaryOp {
                op: BinOp::Lt,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Copy(Place::local(len)),
            },
            span,
        );
        self.terminate(Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::local(cmp)),
            arms: vec![(0, exit)],
            default: body_block,
        });

        self.set_current(body_block);
        // An aggregate local holds the element's address, so the pair's
        // fields project off it without per-shape offset arithmetic.
        let pair = self.emit_combinator_call(
            "gos_rt_vec_get_ptr",
            vec![
                Operand::Copy(Place::local(seq)),
                Operand::Copy(Place::local(counter)),
            ],
            elem_ty,
            span,
        );
        let key = self.fresh(key_ty);
        self.emit_assign(
            Place::local(key),
            Rvalue::Use(Operand::Copy(Place {
                local: pair,
                projection: vec![crate::ir::Projection::Field(0)],
            })),
            span,
        );
        let value = self.fresh(val_ty);
        self.emit_assign(
            Place::local(value),
            Rvalue::Use(Operand::Copy(Place {
                local: pair,
                projection: vec![crate::ir::Projection::Field(1)],
            })),
            span,
        );
        let mut args = vec![
            Operand::Copy(Place::local(map)),
            Operand::Copy(Place::local(key)),
        ];
        if let Some(desc) = key_descriptor {
            args.push(Operand::Const(ConstValue::Str(desc)));
        }
        args.push(Operand::Copy(Place::local(value)));
        let prior_ty = self.option_payload_adt_ty(val_ty);
        let _ = self.emit_combinator_call(insert, args, prior_ty, span);
        self.emit_assign(
            Place::local(counter),
            Rvalue::BinaryOp {
                op: BinOp::Add,
                lhs: Operand::Copy(Place::local(counter)),
                rhs: Operand::Const(ConstValue::Int(1)),
            },
            span,
        );
        self.terminate(Terminator::Goto { target: header });

        self.set_current(exit);
        Some(map)
    }

    fn hashmap_from_arg_is_empty(&self, arg: &HirExpr) -> bool {
        matches!(self.tcx.kind(arg.ty), Some(gossamer_types::TyKind::Unit))
            || matches!(
                &arg.kind,
                HirExprKind::Array(gossamer_hir::HirArrayExpr::List(items)) if items.is_empty()
            )
    }

    pub(crate) fn lower_stdlib_free_call(
        &mut self,
        callee: &HirExpr,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let HirExprKind::Path {
            segments,
            def: callee_def,
            ..
        } = &callee.kind
        else {
            return None;
        };
        let joined = Self::stdlib_free_path(segments);
        if matches!(
            joined.as_str(),
            "BTreeSet::new" | "collections::BTreeSet::new"
        ) && args.is_empty()
        {
            let set_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            let set = self.emit_stdlib_free_call("gos_rt_btree_set_new", set_ty, &[], span)?;
            self.local_runtime_kind.insert(set, "collections::BTreeSet");
            return Some(set);
        }
        // `HashMap::from({})` / `BTreeMap::from({})` is the typed empty-map
        // constructor. Lower it to the same zero-argument intrinsic as `new`
        // so no unit value reaches the native call ABI.
        if matches!(
            joined.as_str(),
            "Map::from"
                | "collections::Map::from"
                | "HashMap::from"
                | "collections::HashMap::from"
                | "BTreeMap::from"
                | "collections::BTreeMap::from"
        ) && matches!(args, [arg] if self.hashmap_from_arg_is_empty(arg))
        {
            let map_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
            return self.emit_stdlib_free_call("Map::new", map_ty, &[], span);
        }
        if matches!(
            joined.as_str(),
            "Map::from"
                | "collections::Map::from"
                | "HashMap::from"
                | "collections::HashMap::from"
                | "BTreeMap::from"
                | "collections::BTreeMap::from"
        ) && let [arg] = args
        {
            let ordered = joined.as_str().ends_with("BTreeMap::from");
            if let Some(map) = self.lower_map_from_aggregate_key_literal(arg, span, ordered) {
                return Some(map);
            }
            if let Some(map) = self.lower_map_from_sequence(arg, span, ordered) {
                return Some(map);
            }
        }
        if matches!(
            joined.as_str(),
            "Set::from" | "collections::Set::from" | "HashSet::from" | "collections::HashSet::from"
        ) && let [arg] = args
        {
            if let Some(set) = self.lower_set_from_array(arg, span, "collections::HashSet") {
                return Some(set);
            }
            if let Some(set) = self.lower_set_from_sequence(arg, span, "collections::HashSet") {
                return Some(set);
            }
        }
        if matches!(
            joined.as_str(),
            "BTreeSet::from" | "collections::BTreeSet::from"
        ) && let [arg] = args
        {
            if let Some(set) = self.lower_set_from_array(arg, span, "collections::BTreeSet") {
                return Some(set);
            }
            if let Some(set) = self.lower_set_from_sequence(arg, span, "collections::BTreeSet") {
                return Some(set);
            }
        }
        // A loop region proves every region-owned allocation dies at the
        // iteration boundary. Collection while the region is still active is
        // at best redundant and at worst makes the collector inspect pointers
        // which `arena_pop` is about to bulk-free. Keep the source-visible
        // collection point, but lower it immediately after that pop.
        if joined == "runtime::collect_cycles"
            && args.is_empty()
            && let Some(deferred) = self.deferred_auto_region_collections.last_mut()
        {
            *deferred = true;
            let unit = self.tcx.unit();
            let dest = self.fresh(unit);
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Const(ConstValue::Unit)),
                span,
            );
            return Some(dest);
        }
        if let ControlFlow::Break(result) = self.lower_stdlib_free_special(
            segments.len(),
            callee_def.is_some(),
            &joined,
            args,
            span,
        ) {
            return result;
        }
        let joined = joined.as_str();
        let (rt_name, ret_ty) = self.resolve_stdlib_free_call(joined, args)?;
        self.emit_stdlib_free_call(rt_name, ret_ty, args, span)
    }

    /// The canonical stdlib path a free-call callee's segments name.
    ///
    /// `use std::compress::gzip` + `gzip::encode(..)` names the same
    /// function as `compress::gzip::encode`, and the arms below key on
    /// the canonical path, so the leaf spelling folds into it here.
    pub(crate) fn stdlib_free_path(segments: &[gossamer_ast::Ident]) -> String {
        let names: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
        let strip_std = if names.first() == Some(&"std") {
            &names[1..]
        } else {
            &names[..]
        };
        let joined = strip_std.join("::");
        gossamer_resolve::canonical_stdlib_path(&joined)
            .map_or(joined, std::string::ToString::to_string)
    }

    /// A `sort::` free call over a tuple or struct element, lowered through
    /// the field-kind stream that describes the element's layout.
    ///
    /// The by-word shims read one machine word per element, which for an
    /// aggregate is the address rather than the value. `None` for a scalar
    /// element, which those shims order correctly.
    fn try_lower_aggregate_sort_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::{IntTy, TyKind};
        if !matches!(
            joined,
            "sort::sort_stable" | "sort::binary_search" | "sort::partition_point"
        ) {
            return None;
        }
        let sequence = args.first()?;
        let elem = self.vec_receiver_elem_ty(sequence.ty);
        let elem = self.peel_ref_ty(elem);
        if !matches!(
            self.tcx.kind_of(elem),
            TyKind::Tuple(_) | TyKind::Adt { .. }
        ) {
            return None;
        }
        let (count, tags) = self.tuple_element_stream(elem)?;
        let sequence_local = self.lower_expr(sequence)?;
        let mut call_args = vec![Operand::Copy(Place::local(sequence_local))];
        if joined != "sort::sort_stable" {
            let target = args.get(1)?;
            let target_local = self.lower_expr(target)?;
            call_args.push(Operand::Copy(Place::local(target_local)));
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let count_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(count_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(
                i128::try_from(count).unwrap_or(0),
            ))),
            span,
        );
        call_args.push(Operand::Copy(Place::local(count_local)));
        let tag_text: String = tags.iter().map(|&b| b as char).collect();
        let string_ty = self.tcx.string_ty();
        let tags_local = self.fresh(string_ty);
        self.emit_assign(
            Place::local(tags_local),
            Rvalue::Use(Operand::Const(ConstValue::Str(tag_text))),
            span,
        );
        call_args.push(Operand::Copy(Place::local(tags_local)));
        let (symbol, ret_ty) = match joined {
            "sort::sort_stable" => (
                "gos_rt_sort_stable_aggr",
                self.tcx.intern(TyKind::Vec(elem)),
            ),
            "sort::binary_search" => ("gos_rt_sort_binary_search_aggr", self.option_i64_adt_ty()),
            _ => ("gos_rt_sort_partition_point_aggr", i64_ty),
        };
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

    /// The runtime symbol and pinned return type a stdlib free call
    /// lowers to, without emitting it.
    ///
    /// Separate from the emission so a caller that needs only the type -
    /// a `for` loop deciding what its element shape is - can ask without
    /// lowering the call twice.
    pub(crate) fn resolve_stdlib_free_call(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        let mut resolved = self.lower_errors_regex_free(joined, args);
        resolved = resolved.or_else(|| self.lower_fs_free(joined, args));
        resolved = resolved.or_else(|| self.lower_fs_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_os_free(joined, args));
        resolved = resolved.or_else(|| self.lower_os_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_path_free(joined, args));
        resolved = resolved.or_else(|| self.lower_io_net_free(joined, args));
        resolved = resolved.or_else(|| self.lower_sort_free(joined, args));
        resolved = resolved.or_else(|| self.lower_middleware_config_free(joined, args));
        resolved = resolved.or_else(|| self.lower_hash_free(joined, args));
        resolved = resolved.or_else(|| self.lower_crypto_free(joined, args));
        resolved = resolved.or_else(|| self.lower_crypto_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_math_free(joined, args));
        resolved = resolved.or_else(|| self.lower_math_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_math_3_free(joined, args));
        resolved = resolved.or_else(|| self.lower_math_4_free(joined, args));
        resolved = resolved.or_else(|| self.lower_utf8_free(joined, args));
        resolved = resolved.or_else(|| self.lower_unicode_free(joined, args));
        resolved = resolved.or_else(|| self.lower_encoding_free(joined, args));
        resolved = resolved.or_else(|| self.lower_encoding_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_strings_free(joined, args));
        resolved = resolved.or_else(|| self.lower_strings_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_strconv_free(joined, args));
        resolved = resolved.or_else(|| self.lower_compress_free(joined, args));
        resolved = resolved.or_else(|| self.lower_codec_free(joined, args));
        resolved = resolved.or_else(|| self.lower_sql_free(joined, args));
        resolved = resolved.or_else(|| self.lower_sql_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_sql_3_free(joined, args));
        resolved = resolved.or_else(|| self.lower_sql_4_free(joined, args));
        resolved = resolved.or_else(|| self.lower_env_thread_free(joined, args));
        resolved = resolved.or_else(|| self.lower_time_free(joined, args));
        resolved = resolved.or_else(|| self.lower_id_misc_free(joined, args));
        resolved = resolved.or_else(|| self.lower_concurrency_free(joined, args));
        resolved = resolved.or_else(|| self.lower_concurrency_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_bytes_free(joined, args));
        resolved = resolved.or_else(|| self.lower_collections_free(joined, args));
        resolved = resolved.or_else(|| self.lower_collections_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_url_runtime_misc_free(joined, args));
        resolved = resolved.or_else(|| self.lower_image_free(joined, args));
        resolved = resolved.or_else(|| self.lower_http_free(joined, args));
        resolved = resolved.or_else(|| self.lower_http_2_free(joined, args));
        resolved = resolved.or_else(|| self.lower_http_3_free(joined, args));
        resolved = resolved.or_else(|| self.lower_http_4_free(joined, args));
        resolved = resolved.or_else(|| self.lower_exec_free(joined, args));
        resolved = resolved.or_else(|| self.lower_signal_flag_free(joined, args));
        resolved
    }

    /// Lowers a prelude free function by its bare name with the arguments
    /// already assembled. The scalar bound methods (`n.max(m)`) name the
    /// same operations, so they share this entry rather than duplicating
    /// the symbol and return-type selection.
    pub(crate) fn lower_stdlib_free_by_name(
        &mut self,
        name: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        let resolved = self
            .lower_math_free(name, args)
            .or_else(|| self.lower_math_2_free(name, args))
            .or_else(|| self.lower_math_3_free(name, args))
            .or_else(|| self.lower_math_4_free(name, args));
        let (rt_name, ret_ty) = resolved?;
        self.emit_stdlib_free_call(rt_name, ret_ty, args, span)
    }

    fn lower_stdlib_free_special(
        &mut self,
        seg_len: usize,
        callee_def_some: bool,
        joined: &str,
        args: &[HirExpr],
        span: Span,
    ) -> ControlFlow<Option<Local>> {
        // 0.7.0 - bare prelude names (`min`, `max`, `clamp`) shadow
        // a runtime helper only when the user hasn't defined their
        // own fn with that name. A non-None `def` here means the
        // resolver bound this path to a user fn - defer to the
        // generic user-fn dispatch below.
        if callee_def_some && seg_len == 1 && matches!(joined, "min" | "max" | "clamp") {
            return ControlFlow::Break(None);
        }
        // spawn(f) -> JoinHandle<T>: run the callable on a goroutine
        // and return a one-shot join handle. Custom-lowered because
        // the callable's code/env must be extracted before the
        // runtime call. A user-defined `fn spawn` (non-None `def`)
        // shadows the prelude builtin.
        if !callee_def_some && seg_len == 1 && joined == "spawn" && matches!(args.len(), 1 | 2) {
            return ControlFlow::Break(self.lower_spawn(&args[0], args.get(1), span));
        }
        // A tuple or struct element spans several slots, so the by-word
        // primitives have no single value to order it by. These take the
        // element's field-kind stream and walk it in declaration order.
        if let Some(local) = self.try_lower_aggregate_sort_free(joined, args, span) {
            return ControlFlow::Break(Some(local));
        }
        // `assert(cond[, msg])` / `assert_eq(a, b[, msg])` prelude
        // assertions: lower to a conditional `panic(msg)` so the same
        // abort fires on every tier (the interp uses the matching
        // `builtin_assert`). A user-defined `fn assert` (non-None `def`)
        // shadows the prelude form.
        if !callee_def_some
            && seg_len == 1
            && !args.is_empty()
            && matches!(joined, "assert" | "assert_eq")
        {
            return ControlFlow::Break(self.lower_assert(args, joined == "assert_eq", span));
        }
        // `fs::walk_dir(root, visit)` / `path::walk(root, visit)`: the
        // visitor closure must be coerced to an env-pointer value and
        // handed to the runtime walker, which calls back into it per
        // descendant - the generic stdlib-call path only forwards plain
        // operands, so this needs its own lowering ahead of that fallback.
        if !callee_def_some && args.len() == 2 && matches!(joined, "fs::walk_dir" | "path::walk") {
            return ControlFlow::Break(self.try_lower_walk_dir(args, span));
        }
        // A resolver-bound type-qualified call (`UserStruct::method`, so
        // `callee_def` is some) is a user item and must never be hijacked
        // by a stdlib bare-type alias like `Counter::new` / `Builder::new`
        // that shares the type name. Defer to the generic user-fn dispatch.
        if callee_def_some && seg_len >= 2 {
            return ControlFlow::Break(None);
        }
        // Qualified `HashMap::get/contains_key/contains/insert(m, k, …)` over a
        // struct / tuple key must content-hash the key exactly as the method
        // form (`m.insert(...)`) does. The plain qualified dispatch below only
        // distinguishes `_str` from `_i64` keys, so an aggregate key would hash
        // its pointer and never find the slot it was inserted under (a `get`
        // that returns `None` for a key that is present). Returns `None` for
        // scalar / string keys, leaving the normal qualified path to run.
        if !callee_def_some && args.len() >= 2 {
            let map_op = match joined {
                "Map::get"
                | "collections::Map::get"
                | "HashMap::get"
                | "collections::HashMap::get" => Some("get"),
                "Map::pop"
                | "collections::Map::pop"
                | "HashMap::pop"
                | "collections::HashMap::pop" => Some("pop"),
                "Map::contains_key"
                | "collections::Map::contains_key"
                | "HashMap::contains_key"
                | "collections::HashMap::contains_key" => Some("contains_key"),
                "Map::contains"
                | "collections::Map::contains"
                | "HashMap::contains"
                | "collections::HashMap::contains" => Some("contains"),
                "Map::insert"
                | "collections::Map::insert"
                | "HashMap::insert"
                | "collections::HashMap::insert" => Some("insert"),
                _ => None,
            };
            if let Some(op) = map_op
                && let Some(local) =
                    self.try_lower_struct_key_map_op(&args[0], op, &args[1..], span)
            {
                return ControlFlow::Break(Some(local));
            }
        }
        // `slog::info/warn/error/debug(msg, k1, v1, …)`: the trailing
        // key/value fields are stringified per-type and passed as a
        // `Vec<String>` so the structured fields survive the FFI on the
        // compiled tier (the generic dispatch would drop them).
        if !callee_def_some
            && matches!(
                joined,
                "slog::info" | "slog::warn" | "slog::error" | "slog::debug"
            )
        {
            let sym = match joined {
                "slog::info" => "gos_rt_slog_info",
                "slog::warn" => "gos_rt_slog_warn",
                "slog::error" => "gos_rt_slog_error",
                "slog::debug" => "gos_rt_slog_debug",
                _ => unreachable!(),
            };
            return ControlFlow::Break(self.lower_slog(sym, args, span));
        }
        // Middleware composition `middleware::tag(inner) -> Handler`:
        // custom-lowered because it must resolve the inner handler's
        // serve fn-address and bind it into a `GosMiddleware` handle,
        // rather than pass the inner value positionally.
        if !callee_def_some
            && args.len() == 1
            && matches!(joined, "middleware::tag" | "http::middleware::tag")
        {
            return ControlFlow::Break(self.lower_middleware_wrap(&args[0], span));
        }
        if !callee_def_some
            && !args.is_empty()
            && let Some(name) = joined
                .strip_prefix("http::middleware::")
                .or_else(|| joined.strip_prefix("middleware::"))
            && let Some((kind, arity, numeric_config)) = middleware_kind_of(name)
            && args.len() == arity
        {
            return ControlFlow::Break(self.lower_middleware_kind(
                &args[0],
                kind,
                args.get(1),
                numeric_config,
                span,
            ));
        }
        ControlFlow::Continue(())
    }

    /// Whether `ty` is an `errors::Error` value (possibly behind
    /// references), as opposed to the `String` message form.
    fn is_error_valued_ty(&self, ty: gossamer_types::Ty) -> bool {
        matches!(
            self.tcx.kind_of(self.peel_ref_ty(ty)),
            gossamer_types::TyKind::DynError
        )
    }

    pub(crate) fn lower_errors_regex_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // DynError (not bare I64) so a let-bound error classifies
            // as PrintKind/ConcatKind::ErrorMessage and `{}` renders
            // the message chain instead of the raw pointer value.
            "errors::new" => ("gos_rt_error_new", self.tcx.dyn_error_ty()),
            "errors::Error::from" => ("gos_rt_error_from", self.tcx.dyn_error_ty()),
            "errors::wrap" => ("gos_rt_error_wrap", self.tcx.dyn_error_ty()),
            // Returns Option<Error> as *mut GosResult (disc=0→Some, disc=1→None).
            // Takes *mut GosVec; MIR coerces the array literal before the call.
            "errors::join" => ("gos_rt_errors_join_vec", self.option_adt_ty()),
            // `errors::is(err, needle)` accepts either a message (substring
            // match down the chain) or a sentinel error value (identity
            // match). The needle's type picks the shim so the runtime never
            // has to guess what the second pointer refers to.
            "errors::is" => {
                let sentinel = args
                    .get(1)
                    .is_some_and(|arg| self.is_error_valued_ty(arg.ty));
                if sentinel {
                    ("gos_rt_error_is_sentinel", self.tcx.bool_ty())
                } else {
                    ("gos_rt_error_is", self.tcx.bool_ty())
                }
            }
            // Result-shaped so an invalid pattern lands in the Err arm
            // on the compiled tiers exactly as it does on the VM; the
            // bare-pointer `gos_rt_regex_compile` shim made every
            // compile look like Ok, with a null handle on bad input.
            // `regex::Pattern::compile(p)` is the type-qualified spelling of
            // the same call; both answer `Result<Pattern, errors::Error>`.
            "regex::compile" | "regex::Pattern::compile" | "Pattern::compile" => {
                let handle = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let ty = self.result_payload_string_error_ty(handle);
                ("gos_rt_regex_compile_result", ty)
            }
            "regex::is_match" => ("gos_rt_regex_is_match", self.tcx.bool_ty()),
            "regex::count" => (
                "gos_rt_regex_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // Returns Option<(start, end, text)> - disc=0 Some, disc=1 None.
            "regex::find" => ("gos_rt_regex_find_opt", self.option_tuple3_i64_i64_str_ty()),
            // Returns Option<Vec<String>> - disc=0 Some(caps), disc=1 None.
            "regex::captures" => ("gos_rt_regex_captures", self.option_vec_option_string_ty()),
            "regex::find_all" => {
                // The runtime returns 24-byte `(start, end, text)` tuples
                // (see `gos_rt_regex_find_all`), so a `let all = ...`
                // binding must carry the tuple element type - otherwise the
                // bound-Vec for-loop reads each element as a single 8-byte
                // slot and `hit.2` indexes past the slot.
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let s = self.tcx.string_ty();
                let tup = self
                    .tcx
                    .intern(gossamer_types::TyKind::Tuple(vec![i, i, s]));
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(tup));
                ("gos_rt_regex_find_all", v)
            }
            "regex::captures_all" => {
                // Returns `Vec<Vec<Option<String>>>` - outer per-match,
                // inner per-group. Each group is a canonical
                // `Option<String>` tagged union (`gos_rt_result_new`):
                // Some(matched text) or None for an absent optional
                // group. Pinning the element to `Option<String>` (not a
                // bare `String`) is what makes `match row[i] { Some(k)
                // => …, None => … }` read the real discriminant instead
                // of treating the value as a raw payload.
                let opt_s = self.option_string_ty();
                let inner = self.tcx.intern(gossamer_types::TyKind::Vec(opt_s));
                let outer = self.tcx.intern(gossamer_types::TyKind::Vec(inner));
                ("gos_rt_regex_captures_all", outer)
            }
            "regex::replace" => ("gos_rt_regex_replace", self.tcx.string_ty()),
            "regex::replace_all" => ("gos_rt_regex_replace_all", self.tcx.string_ty()),
            "regex::split" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_regex_split", v)
            }
            _ => return None,
        })
    }

    fn lower_fs_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "fs::sync_dir" => {
                let unit = self.tcx.unit();
                ("gos_rt_fs_sync_dir", self.result_of(unit))
            }
            // `fs::read_to_string(path) -> Result<String, errors::Error>`.
            // Routes to the Result-shaped shim (not the bare-string
            // `gos_rt_fs_read_to_string`, which returns "" on failure) so a
            // missing / unreadable path propagates `Err` like `fs::read` and
            // the VM, not a silent `Ok("")`.
            "fs::read_to_string" => {
                let s = self.tcx.string_ty();
                let e = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([s, e]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_fs_read_to_string_result", result_ty)
            }
            "fs::write" => {
                // Pick the bytes-shaped variant when the contents
                // argument is a Vec<u8> / &[u8] - the c-string-shaped
                // helper would truncate at the first NUL and corrupt
                // binary payloads (image writes, gzip bodies, etc.).
                // The typechecker often leaves `&local_vec`-shaped
                // args as `Ref<Var(_)>`, so we walk through the `&`
                // operator and consult `peek_collection_type`, which
                // recovers the actual MIR-pinned local type.
                let bytes_shaped = args.get(1).is_some_and(|a| {
                    use gossamer_types::{IntTy, TyKind};
                    if is_vec_u8_arg(self.tcx, a) {
                        return true;
                    }
                    let inner_expr = if let HirExprKind::Unary { op, operand } = &a.kind {
                        if matches!(op, HirUnaryOp::RefShared | HirUnaryOp::RefMut) {
                            operand.as_ref()
                        } else {
                            a
                        }
                    } else {
                        a
                    };
                    let probe = self
                        .peek_collection_type(inner_expr)
                        .or(Some(inner_expr.ty));
                    probe.is_some_and(|t| {
                        let mut walk = t;
                        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(walk) {
                            walk = *inner;
                        }
                        let elem = match self.tcx.kind_of(walk) {
                            TyKind::Vec(e) | TyKind::Slice(e) => *e,
                            _ => return false,
                        };
                        matches!(self.tcx.kind_of(elem), TyKind::Int(IntTy::U8))
                    })
                });
                let sym = if bytes_shaped {
                    "gos_rt_os_write_file_bytes_result"
                } else {
                    "gos_rt_os_write_file_result"
                };
                (sym, self.result_unit_error_adt_ty())
            }
            "fs::create_dir" => ("gos_rt_fs_create_dir", self.result_unit_error_adt_ty()),
            "fs::create_dir_mode" => ("gos_rt_fs_create_dir_mode", self.result_unit_error_adt_ty()),
            "fs::create_dir_all_mode" => (
                "gos_rt_fs_create_dir_all_mode",
                self.result_unit_error_adt_ty(),
            ),
            "fs::write_mode" => ("gos_rt_fs_write_mode", self.result_unit_error_adt_ty()),
            "fs::set_permissions" => ("gos_rt_fs_set_permissions", self.result_unit_error_adt_ty()),
            "fs::permissions" => {
                let mode = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let error = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([mode, error]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_fs_permissions", result_ty)
            }
            "fs::create_dir_all" => (
                "gos_rt_os_mkdir_all_result",
                self.result_unit_error_adt_ty(),
            ),
            "fs::remove_file" => (
                "gos_rt_os_remove_file_result",
                self.result_unit_error_adt_ty(),
            ),
            // Non-recursive empty-directory removal, matching the interp.
            "fs::remove_dir" => ("gos_rt_fs_remove_dir", self.result_unit_error_adt_ty()),
            "fs::remove_dir_all" => (
                "gos_rt_os_remove_dir_all_result",
                self.result_unit_error_adt_ty(),
            ),
            "fs::temp_dir" => ("gos_rt_fs_temp_dir", self.result_string_error_adt_ty()),
            "fs::temp_file" => {
                let file = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let path = self.tcx.string_ty();
                let pair = self
                    .tcx
                    .intern(gossamer_types::TyKind::Tuple(vec![file, path]));
                ("gos_rt_fs_temp_file", self.result_of(pair))
            }
            _ => return None,
        })
    }

    fn lower_fs_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "fs::read_dir" => {
                // Return type is `Result<Vec<DirInfo>, errors::Error>`.
                // Pin the dest as a Result Adt whose first generic
                // is `Vec<DirInfo>` so `.map_err(...)?` unwraps to a
                // properly-typed Vec (driving `entries[i]` through
                // the Vec dispatch with `DirInfo` element-struct
                // tag) instead of a bare i64 pointer.
                let dir_info_ty = self.dir_info_adt_ty();
                let vec_ty = self.tcx.intern(gossamer_types::TyKind::Vec(dir_info_ty));
                // The Err payload is a `*mut GosError` from
                // `gos_rt_error_new`; pinning it as a bare I64 made
                // `println!("{e}")` render the raw pointer value.
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([vec_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_fs_list_dir", result_ty)
            }
            _ => return None,
        })
    }

    fn lower_os_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "fs::read" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
                let e = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([v, e]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_fs_read_bytes_result", result_ty)
            }
            // 0.10.0 - os/fs copy + canonicalize, crypto::subtle.
            "fs::copy" => ("gos_rt_fs_copy", self.result_i64_error_adt_ty()),
            "fs::canonicalize" => ("gos_rt_fs_canonicalize", self.result_string_error_adt_ty()),
            // `os::arch()` / `os::family()` - target introspection.
            "os::arch" => ("gos_rt_os_arch", self.tcx.string_ty()),
            "os::family" => ("gos_rt_os_family", self.tcx.string_ty()),
            // `fs::rename(from, to)` -> Result<(), Error>.
            "fs::rename" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_fs_rename", result_ty)
            }
            "os::program_name" | "env::program_name" => {
                ("gos_rt_os_program_name", self.tcx.string_ty())
            }
            "env::set_current_dir" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_env_set_current_dir", result_ty)
            }
            "env::var" => ("gos_rt_os_env", self.option_string_adt_ty()),
            "fs::exists" => ("gos_rt_os_exists", self.tcx.bool_ty()),
            "fs::is_file" => ("gos_rt_os_is_file", self.tcx.bool_ty()),
            "fs::is_dir" => ("gos_rt_os_is_dir", self.tcx.bool_ty()),
            "fs::is_symlink" => ("gos_rt_os_is_symlink", self.tcx.bool_ty()),
            "fs::file_size" => (
                "gos_rt_os_file_size",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "env::current_dir" => ("gos_rt_os_cwd", self.result_string_error_adt_ty()),
            _ => return None,
        })
    }

    fn lower_os_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `env::args() -> Vec<String>`. Pinning the dest type
            // here is what teaches `args[i].len()` to dispatch
            // through `gos_rt_str_len` instead of the generic
            // `gos_rt_arr_len`. Single-file builds got
            // `Vec<String>` for free from typeck, but cross-module
            // compilation (e.g. askq, where `cli.gos` references
            // `args` and sibling modules also exist) leaves the
            // call's HIR type as a `Var(_)` and the cranelift
            // dispatch then crashes inside `gos_rt_arr_len`
            // reading a Vec header out of a `*const c_char`
            // string pointer. The runtime now hands back a real
            // `*mut GosVec` whose data pointer is `argv + 1`, so
            // index access through the standard `header.ptr + i *
            // elem_bytes` shape Just Works.
            "env::args" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_os_args", v)
            }
            // `env::set_var(name, value) -> Result<(), errors::Error>`.
            // Pin the Ok payload to unit and the Err to
            // `errors::Error` so callers' `?` shapes find the
            // right field layout. Without this binding the
            // compiled tier silently no-op'd `set_env` because
            // the generic free-call dispatch couldn't resolve
            // the symbol, and downstream `env::var` reads
            // returned the old value.
            "env::set_var" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_os_set_env", result_ty)
            }
            "env::unset_var" => ("gos_rt_os_unset_env", self.tcx.unit()),
            _ => return None,
        })
    }

    fn lower_path_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "path::join" => ("gos_rt_path_join", self.tcx.string_ty()),
            "path::split" => {
                let s = self.tcx.string_ty();
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                ("gos_rt_path_split", tup)
            }
            "path::components" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_path_components", v)
            }
            "path::prefixes" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_path_prefixes", v)
            }
            "path::unique_prefixes" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_path_unique_prefixes", v)
            }
            "path::normalize" => ("gos_rt_path_clean", self.tcx.string_ty()),
            "path::is_absolute" => ("gos_rt_path_is_absolute", self.tcx.bool_ty()),
            "path::starts_with" => ("gos_rt_path_has_prefix", self.tcx.bool_ty()),
            "path::extension" => ("gos_rt_path_ext", self.option_string_adt_ty()),
            // 0.10.0 - path Option-returning free fns. Each wraps
            // the matching `gos_rt_path_*_opt` helper which packs a
            // `*mut GosResult` (disc=0 Some(String), disc=1 None).
            "path::glob" => ("gos_rt_path_glob", self.result_vec_string_error_ty()),
            "path::matches" => ("gos_rt_path_matches", self.tcx.bool_ty()),
            "path::parent" => ("gos_rt_path_parent", self.option_string_adt_ty()),
            "path::file_stem" => ("gos_rt_path_stem", self.option_string_adt_ty()),
            "path::file_name" => ("gos_rt_path_file_name", self.option_string_adt_ty()),
            _ => return None,
        })
    }

    /// `std::sort` - the explicit stable-order and sorted-sequence search
    /// half of the sequence surface. The element type selects the shim:
    /// `Vec<i64>` compares values, every other 8-byte element compares as
    /// a `String`.
    /// `middleware::<Config>::<ctor>(..) -> String` - the configuration
    /// values the middleware wrappers consume.
    fn lower_middleware_config_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        let name = joined
            .strip_prefix("http::middleware::")
            .or_else(|| joined.strip_prefix("middleware::"))?;
        let sym = match name {
            "CorsConfig::permissive" => "gos_rt_mw_cors_permissive",
            "CorsConfig::new" => "gos_rt_mw_cors_new",
            "HstsConfig::safe_default" => "gos_rt_mw_hsts_safe_default",
            "HstsConfig::strict" => "gos_rt_mw_hsts_strict",
            "SecurityHeaders::strict" => "gos_rt_mw_security_strict",
            "SecurityHeaders::off" => "gos_rt_mw_security_off",
            "CacheControl::no_store" => "gos_rt_mw_cache_no_store",
            "CacheControl::immutable_for" => "gos_rt_mw_cache_immutable_for",
            "RateLimit::per_ip" => "gos_rt_mw_rate_limit_per_ip",
            _ => return None,
        };
        Some((sym, self.tcx.string_ty()))
    }

    fn lower_sort_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        if !joined.starts_with("sort::") || args.is_empty() {
            return None;
        }
        let elem = self.vec_receiver_elem_ty(args[0].ty);
        let (int_shim, float_shim, str_shim) = match joined {
            "sort::sort_stable" => (
                "gos_rt_sort_stable_i64",
                "gos_rt_sort_stable_f64",
                "gos_rt_sort_stable_str",
            ),
            "sort::binary_search" => (
                "gos_rt_sort_binary_search_i64",
                "gos_rt_sort_binary_search_f64",
                "gos_rt_sort_binary_search_str",
            ),
            "sort::partition_point" => (
                "gos_rt_sort_partition_point_i64",
                "gos_rt_sort_partition_point_f64",
                "gos_rt_sort_partition_point_str",
            ),
            _ => return None,
        };
        let sym = match self.tcx.kind_of(self.peel_ref_ty(elem)) {
            gossamer_types::TyKind::Int(_) | gossamer_types::TyKind::Char => int_shim,
            gossamer_types::TyKind::Float(_) => float_shim,
            _ => str_shim,
        };
        let ret = match joined {
            "sort::sort_stable" => self.tcx.intern(gossamer_types::TyKind::Vec(elem)),
            "sort::binary_search" => self.option_i64_adt_ty(),
            _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
        };
        Some((sym, ret))
    }

    fn lower_io_net_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "bufio::read_to_string" => (
                "gos_rt_bufio_read_to_string",
                self.result_string_error_adt_ty(),
            ),
            "bufio::read_lines_of" | "bufio::read_lines" => (
                "gos_rt_bufio_read_lines_of",
                self.result_vec_string_error_ty(),
            ),
            "bufio::split_whitespace" => {
                let s = self.tcx.string_ty();
                (
                    "gos_rt_str_split_whitespace",
                    self.tcx.intern(gossamer_types::TyKind::Vec(s)),
                )
            }
            // io::Copy(dst, src) / io::ReadAll(reader) - Go-shaped
            // stream helpers over the fd-tagged `*GosStream` handles.
            "io::Copy" => (
                "gos_rt_io_copy",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "io::ReadAll" => ("gos_rt_io_read_all", self.result_string_error_adt_ty()),
            // Handle-based stream adapters: a Reader / Writer is an i64
            // registry id, so the whole family composes as plain scalars
            // across the C-ABI.
            "io::string_reader" | "io::buffer_writer" | "io::limit_reader" | "io::tee_reader"
            | "io::multi_reader" | "io::write" => {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let sym = match joined {
                    "io::string_reader" => "gos_rt_io_string_reader",
                    "io::buffer_writer" => "gos_rt_io_buffer_writer",
                    "io::limit_reader" => "gos_rt_io_limit_reader",
                    "io::tee_reader" => "gos_rt_io_tee_reader",
                    "io::multi_reader" => "gos_rt_io_multi_reader",
                    _ => "gos_rt_io_write_str",
                };
                (sym, i64_ty)
            }
            "io::pipe" => {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tup = self
                    .tcx
                    .intern(gossamer_types::TyKind::Tuple(vec![i64_ty, i64_ty]));
                ("gos_rt_io_pipe", tup)
            }
            "io::copy_n" => ("gos_rt_io_copy_n", self.result_i64_error_adt_ty()),
            "io::drain" => ("gos_rt_io_drain", self.tcx.string_ty()),
            "io::contents" => ("gos_rt_io_contents", self.tcx.string_ty()),
            "io::close_writer" => ("gos_rt_io_close_writer", self.tcx.unit()),
            "net::lookup" => ("gos_rt_net_resolve", self.result_vec_string_error_ty()),
            "fs::open" | "fs::File::open" => {
                ("gos_rt_fs_file_open", self.result_i64_error_adt_ty())
            }
            "fs::create" | "fs::File::create" => {
                ("gos_rt_fs_file_create", self.result_i64_error_adt_ty())
            }
            "fs::OpenOptions::new" | "OpenOptions::new" => (
                "gos_rt_fs_open_options_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "net::ip::is_valid" => ("gos_rt_netip_is_valid", self.tcx.bool_ty()),
            "net::ip::is_v4" => ("gos_rt_netip_is_v4", self.tcx.bool_ty()),
            "net::ip::is_v6" => ("gos_rt_netip_is_v6", self.tcx.bool_ty()),
            "net::ip::is_loopback" => ("gos_rt_netip_is_loopback", self.tcx.bool_ty()),
            "net::ip::is_private" => ("gos_rt_netip_is_private", self.tcx.bool_ty()),
            "net::ip::is_multicast" => ("gos_rt_netip_is_multicast", self.tcx.bool_ty()),
            "net::ip::is_unspecified" => ("gos_rt_netip_is_unspecified", self.tcx.bool_ty()),
            "net::ip::to_string" => ("gos_rt_netip_normalize", self.tcx.string_ty()),
            "net::ip::parse" => ("gos_rt_net_ip_parse", self.result_string_error_adt_ty()),
            "net::ip::octets" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_net_ip_octets",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "net::TcpListener::bind" => {
                ("gos_rt_tcp_listener_bind", self.result_i64_error_adt_ty())
            }
            "net::TcpStream::connect" => {
                ("gos_rt_tcp_stream_connect", self.result_i64_error_adt_ty())
            }
            "net::UnixListener::bind" => {
                ("gos_rt_unix_listener_bind", self.result_i64_error_adt_ty())
            }
            "net::UnixStream::connect" => {
                ("gos_rt_unix_stream_connect", self.result_i64_error_adt_ty())
            }
            "net::UdpSocket::bind" => ("gos_rt_udp_bind", self.result_i64_error_adt_ty()),
            "bufio::Scanner::new" | "Scanner::new" => (
                "gos_rt_bufio_scanner_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "bufio::Scanner::next" | "Scanner::next" => {
                ("gos_rt_bufio_scanner_text", self.tcx.string_ty())
            }
            _ => return None,
        })
    }

    fn lower_hash_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.10.0 - hash::* checksums previously VM-only.
            "hash::crc32::checksum" => (
                "gos_rt_hash_crc32_checksum",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "hash::crc32::checksum_string" => (
                "gos_rt_hash_crc32_checksum_string",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "hash::crc32::update" => (
                "gos_rt_hash_crc32_update",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "hash::crc32::update_window" => (
                "gos_rt_hash_crc32_update_window",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "hash::adler32::checksum" => (
                "gos_rt_hash_adler32_checksum",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "hash::adler32::checksum_string" => (
                "gos_rt_hash_adler32_checksum_string",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "hash::adler32::update" => (
                "gos_rt_hash_adler32_update",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "hash::fnv::hash32" => (
                "gos_rt_hash_fnv32",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "hash::fnv::hash64" => (
                "gos_rt_hash_fnv64",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "hash::fnv::hash_string" => (
                "gos_rt_hash_fnv_string",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    fn lower_crypto_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "crypto::subtle::constant_time_eq" => {
                ("gos_rt_crypto_subtle_ct_eq", self.tcx.bool_ty())
            }
            "crypto::hmac::sha256_mac" | "hmac::sha256_mac" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_hmac_sha256_mac",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::rand::bytes" => ("gos_rt_crypto_rand_bytes", self.result_vec_u8_error_ty()),
            "crypto::password::hash" => (
                "gos_rt_crypto_password_hash",
                self.result_string_error_adt_ty(),
            ),
            "crypto::password::verify" => (
                "gos_rt_crypto_password_verify",
                self.result_bool_error_adt_ty(),
            ),
            "crypto::password::needs_rehash" => {
                ("gos_rt_crypto_password_needs_rehash", self.tcx.bool_ty())
            }
            "crypto::kdf::pbkdf2_sha256" | "kdf::pbkdf2_sha256" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
                ("gos_rt_crypto_pbkdf2_sha256", v)
            }
            "crypto::kdf::scrypt_interactive" | "kdf::scrypt_interactive" => (
                "gos_rt_crypto_scrypt_interactive",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::kdf::argon2id_hash" | "kdf::argon2id_hash" => (
                "gos_rt_crypto_argon2id_hash",
                self.result_string_error_adt_ty(),
            ),
            "crypto::kdf::argon2id_verify" | "kdf::argon2id_verify" => (
                "gos_rt_crypto_argon2id_verify",
                self.result_bool_error_adt_ty(),
            ),
            "crypto::aead::aes_256_gcm_seal" | "aead::aes_256_gcm_seal" => (
                "gos_rt_crypto_aes256gcm_seal",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::aead::aes_256_gcm_open" | "aead::aes_256_gcm_open" => (
                "gos_rt_crypto_aes256gcm_open",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::aead::chacha20_poly1305_seal" | "aead::chacha20_poly1305_seal" => (
                "gos_rt_crypto_chacha20poly1305_seal",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::aead::chacha20_poly1305_open" | "aead::chacha20_poly1305_open" => (
                "gos_rt_crypto_chacha20poly1305_open",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::x509::verify_server_certificate_with_crls"
            | "x509::verify_server_certificate_with_crls" => (
                "gos_rt_x509_verify_server_certificate_with_crls",
                self.result_unit_error_adt_ty(),
            ),
            "crypto::ed25519::keypair" | "ed25519::keypair" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let vec_u8 = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
                let tup = self
                    .tcx
                    .intern(gossamer_types::TyKind::Tuple(vec![vec_u8, vec_u8]));
                ("gos_rt_crypto_ed25519_keypair", self.result_of(tup))
            }
            "crypto::ed25519::sign" | "ed25519::sign" => {
                ("gos_rt_crypto_ed25519_sign", self.result_vec_u8_error_ty())
            }
            "crypto::ed25519::verify" | "ed25519::verify" => (
                "gos_rt_crypto_ed25519_verify",
                self.result_unit_error_adt_ty(),
            ),
            "crypto::ecdsa::keypair_pem" | "ecdsa::keypair_pem" => {
                let s = self.tcx.string_ty();
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                ("gos_rt_crypto_ecdsa_keypair_pem", self.result_of(tup))
            }
            "crypto::ecdsa::sign_pem" | "ecdsa::sign_pem" => (
                "gos_rt_crypto_ecdsa_sign_pem",
                self.result_vec_u8_error_ty(),
            ),
            _ => return None,
        })
    }

    fn lower_crypto_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "crypto::ecdsa::verify_pem" | "ecdsa::verify_pem" => (
                "gos_rt_crypto_ecdsa_verify_pem",
                self.result_unit_error_adt_ty(),
            ),
            "jwt::sign_hs" => ("gos_rt_jwt_sign_hs", self.result_string_error_adt_ty()),
            "jwt::verify_hs" => ("gos_rt_jwt_verify_hs", self.result_string_error_adt_ty()),
            "jwt::verify" => ("gos_rt_jwt_verify", self.result_string_error_adt_ty()),
            "jwt::header" => ("gos_rt_jwt_header", self.result_string_error_adt_ty()),
            "jwt::sign_es256" => ("gos_rt_jwt_sign_es256", self.result_string_error_adt_ty()),
            "jwt::verify_es256" => ("gos_rt_jwt_verify_es256", self.result_string_error_adt_ty()),
            "jwt::sign_eddsa" => ("gos_rt_jwt_sign_eddsa", self.result_string_error_adt_ty()),
            "jwt::verify_eddsa" => ("gos_rt_jwt_verify_eddsa", self.result_string_error_adt_ty()),
            "crypto::sha256::hex" | "sha256::hex" | "crypto::sha256_hex" => {
                ("gos_rt_sha256_hex", self.tcx.string_ty())
            }
            "crypto::sha256::digest" | "sha256::digest" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_sha256_digest",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::sha512::digest" | "sha512::digest" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_sha512_digest",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::blake3::digest" | "blake3::digest" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_blake3_digest",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::insecure::md5" | "insecure::md5" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_md5",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::insecure::md5_hex" | "insecure::md5_hex" => {
                ("gos_rt_crypto_md5_hex", self.tcx.string_ty())
            }
            "crypto::insecure::sha1" | "insecure::sha1" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_sha1",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::insecure::sha1_hex" | "insecure::sha1_hex" => {
                ("gos_rt_crypto_sha1_hex", self.tcx.string_ty())
            }
            "crypto::sha512::hex" | "sha512::hex" | "crypto::sha512_hex" => {
                ("gos_rt_sha512_hex", self.tcx.string_ty())
            }
            "crypto::blake3::hex" | "blake3::hex" | "crypto::blake3_hex" => {
                ("gos_rt_blake3_hex", self.tcx.string_ty())
            }
            "crypto::hmac::sha256_hex" | "hmac::sha256_hex" | "crypto::hmac_sha256_hex" => {
                ("gos_rt_hmac_sha256_hex", self.tcx.string_ty())
            }
            _ => return None,
        })
    }

    fn lower_math_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "f64::to_bits" => (
                "gos_rt_f64_to_bits",
                self.tcx.int_ty(gossamer_types::IntTy::U64),
            ),
            "f64::from_bits" => (
                "gos_rt_f64_from_bits",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "f32::to_bits" => (
                "gos_rt_f32_to_bits",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "f32::from_bits" => (
                "gos_rt_f32_from_bits",
                self.tcx.float_ty(gossamer_types::FloatTy::F32),
            ),
            // 0.10.0 - math::bits::* scalar primitives previously
            // VM-only. The carrying add/sub/mul/div (tuple returns)
            // stay on the VM until aggregate-return ABI lands.
            "math::bits::count_ones" => (
                "gos_rt_bits_count_ones",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::count_zeros" => (
                "gos_rt_bits_count_zeros",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::leading_zeros" => (
                "gos_rt_bits_leading_zeros",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::trailing_zeros" => (
                "gos_rt_bits_trailing_zeros",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::reverse_bits" => (
                "gos_rt_bits_reverse_bits",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::reverse_bytes" => (
                "gos_rt_bits_reverse_bytes",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::len" => (
                "gos_rt_bits_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::rotate_left" => (
                "gos_rt_bits_rotate_left",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::rotate_right" => (
                "gos_rt_bits_rotate_right",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // 0.10.0 - carrying primitives return (i64, i64) via the
            // by-value-aggregate ABI (heap pointer + caller memcpy).
            "math::bits::add" | "math::bits::sub" | "math::bits::div" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![i, i]));
                let sym = match joined {
                    "math::bits::add" => "gos_rt_bits_add",
                    "math::bits::sub" => "gos_rt_bits_sub",
                    _ => "gos_rt_bits_div",
                };
                (sym, tup)
            }
            "math::bits::mul" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![i, i]));
                ("gos_rt_bits_mul", tup)
            }
            // 0.10.0 - math extended trig / log / round entries.
            "math::tan" => (
                "gos_rt_math_tan",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::asin" => (
                "gos_rt_math_asin",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::acos" => (
                "gos_rt_math_acos",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::atan" => (
                "gos_rt_math_atan",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::atan2" => (
                "gos_rt_math_atan2",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::sinh" => (
                "gos_rt_math_sinh",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::cosh" => (
                "gos_rt_math_cosh",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            _ => return None,
        })
    }

    fn lower_math_2_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "math::abs" if args.len() == 1 => {
                if arg_is_float(self.tcx, &args[0]) {
                    (
                        "gos_rt_math_abs",
                        self.tcx.float_ty(gossamer_types::FloatTy::F64),
                    )
                } else {
                    (
                        "gos_rt_math_abs_i64",
                        self.tcx.int_ty(gossamer_types::IntTy::I64),
                    )
                }
            }
            "math::tanh" => (
                "gos_rt_math_tanh",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::log2" => (
                "gos_rt_math_log2",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::log10" => (
                "gos_rt_math_log10",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::cbrt" => (
                "gos_rt_math_cbrt",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::round" => (
                "gos_rt_math_round",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::exp2" => (
                "gos_rt_math_exp2",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::rem" => (
                "gos_rt_math_fmod",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::hypot" => (
                "gos_rt_math_hypot",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::copysign" => (
                "gos_rt_math_copysign",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::positive_diff" => (
                "gos_rt_math_dim",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::trunc" => (
                "gos_rt_math_trunc",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::is_nan" => ("gos_rt_math_is_nan", self.tcx.bool_ty()),
            "math::is_inf" => ("gos_rt_math_is_inf", self.tcx.bool_ty()),
            // 0.10.0 - arbitrary-precision big integers. Every value
            // is carried as a decimal `String` (matching the interp),
            // so all the arithmetic entries take/return `String`.
            "math::big::factorial" => ("gos_rt_math_big_factorial", self.tcx.string_ty()),
            _ => return None,
        })
    }

    fn lower_math_3_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "math::big::int_from_i64" => ("gos_rt_math_big_int_from_i64", self.tcx.string_ty()),
            "math::big::int_from_str" => (
                "gos_rt_math_big_int_from_str",
                self.result_string_error_adt_ty(),
            ),
            "math::big::int_to_str" => ("gos_rt_math_big_int_to_str", self.tcx.string_ty()),
            "math::big::int_to_hex" => ("gos_rt_math_big_int_to_hex", self.tcx.string_ty()),
            "math::big::int_to_i64" => ("gos_rt_math_big_int_to_i64", self.option_i64_adt_ty()),
            "math::big::int_is_zero" => ("gos_rt_math_big_int_is_zero", self.tcx.bool_ty()),
            "math::big::int_is_positive" => ("gos_rt_math_big_int_is_positive", self.tcx.bool_ty()),
            "math::big::int_is_negative" => ("gos_rt_math_big_int_is_negative", self.tcx.bool_ty()),
            "math::big::int_add" => ("gos_rt_math_big_int_add", self.tcx.string_ty()),
            "math::big::int_sub" => ("gos_rt_math_big_int_sub", self.tcx.string_ty()),
            "math::big::int_mul" => ("gos_rt_math_big_int_mul", self.tcx.string_ty()),
            "math::big::int_div" => ("gos_rt_math_big_int_div", self.result_string_error_adt_ty()),
            "math::big::int_rem" => ("gos_rt_math_big_int_rem", self.result_string_error_adt_ty()),
            "math::big::int_pow" => ("gos_rt_math_big_int_pow", self.tcx.string_ty()),
            "math::big::int_abs" => ("gos_rt_math_big_int_abs", self.tcx.string_ty()),
            "math::big::int_neg" => ("gos_rt_math_big_int_neg", self.tcx.string_ty()),
            "math::big::int_gcd" => ("gos_rt_math_big_int_gcd", self.tcx.string_ty()),
            "math::big::int_lcm" => ("gos_rt_math_big_int_lcm", self.tcx.string_ty()),
            "math::big::int_cmp" => (
                "gos_rt_math_big_int_cmp",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::big::uint_from_u64" => ("gos_rt_math_big_uint_from_u64", self.tcx.string_ty()),
            "math::big::uint_from_str" => (
                "gos_rt_math_big_uint_from_str",
                self.result_string_error_adt_ty(),
            ),
            "math::big::uint_to_str" => ("gos_rt_math_big_uint_to_str", self.tcx.string_ty()),
            "math::big::uint_to_hex" => ("gos_rt_math_big_uint_to_hex", self.tcx.string_ty()),
            "math::big::uint_to_u64" => ("gos_rt_math_big_uint_to_u64", self.option_i64_adt_ty()),
            "math::big::uint_is_zero" => ("gos_rt_math_big_uint_is_zero", self.tcx.bool_ty()),
            "math::big::uint_add" => ("gos_rt_math_big_uint_add", self.tcx.string_ty()),
            "math::big::uint_mul" => ("gos_rt_math_big_uint_mul", self.tcx.string_ty()),
            "math::big::uint_pow" => ("gos_rt_math_big_uint_pow", self.tcx.string_ty()),
            "math::big::uint_pow_mod" => ("gos_rt_math_big_uint_pow_mod", self.tcx.string_ty()),
            "math::big::uint_bit_len" => (
                "gos_rt_math_big_uint_bit_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // 0.7.0 scalar cmp prelude - `min(a, b)` / `max(a, b)`
            // / `clamp(x, lo, hi)`. Two-arg shape dispatches by
            // first-arg HIR type to the i64 or f64 variant; the
            // Vec-shaped `min(xs)` / `max(xs)` fallback hits the
            // bare-name dispatch later (single-arg shape is *not*
            // matched here).
            "min" | "math::min" if args.len() == 2 => {
                let is_f = arg_is_float(self.tcx, &args[0]);
                let sym = if is_f {
                    "gos_rt_min_f64"
                } else {
                    "gos_rt_min_i64"
                };
                let ret = if is_f {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else if arg_is_char(self.tcx, &args[0]) {
                    // Codepoint compares as i64 via `gos_rt_*_i64`, but the
                    // result is a `char` and must render as one, not its int.
                    self.tcx.char_ty()
                } else {
                    self.tcx.int_ty(gossamer_types::IntTy::I64)
                };
                (sym, ret)
            }
            "max" | "math::max" if args.len() == 2 => {
                let is_f = arg_is_float(self.tcx, &args[0]);
                let sym = if is_f {
                    "gos_rt_max_f64"
                } else {
                    "gos_rt_max_i64"
                };
                let ret = if is_f {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else if arg_is_char(self.tcx, &args[0]) {
                    // Codepoint compares as i64 via `gos_rt_*_i64`, but the
                    // result is a `char` and must render as one, not its int.
                    self.tcx.char_ty()
                } else {
                    self.tcx.int_ty(gossamer_types::IntTy::I64)
                };
                (sym, ret)
            }
            _ => return None,
        })
    }

    fn lower_math_4_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "clamp" | "math::clamp" if args.len() == 3 => {
                let is_f = arg_is_float(self.tcx, &args[0]);
                let sym = if is_f {
                    "gos_rt_clamp_f64"
                } else {
                    "gos_rt_clamp_i64"
                };
                let ret = if is_f {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else if arg_is_char(self.tcx, &args[0]) {
                    // Codepoint compares as i64 via `gos_rt_*_i64`, but the
                    // result is a `char` and must render as one, not its int.
                    self.tcx.char_ty()
                } else {
                    self.tcx.int_ty(gossamer_types::IntTy::I64)
                };
                (sym, ret)
            }
            _ => return None,
        })
    }

    fn lower_utf8_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // utf8::decode_rune family - (char, i64) by-value tuple.
            "utf8::decode_rune"
            | "utf8::decode_rune_in_string"
            | "utf8::decode_last_rune"
            | "utf8::decode_last_rune_in_string" => {
                let c = self.tcx.char_ty();
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![c, i]));
                let sym = match joined {
                    "utf8::decode_rune" => "gos_rt_utf8_decode_rune",
                    "utf8::decode_rune_in_string" => "gos_rt_utf8_decode_rune_in_string",
                    "utf8::decode_last_rune" => "gos_rt_utf8_decode_last_rune",
                    _ => "gos_rt_utf8_decode_last_rune_in_string",
                };
                (sym, tup)
            }
            "utf8::append_rune" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_utf8_append_rune",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            // ---------------------------------------------------------------
            // std::utf8 - high-value helpers. The decode_rune family
            // returns `(char, usize)` tuples and stays interp-only
            // until the Adt-by-value ABI lands.
            "utf8::rune_count_in_string" => (
                "gos_rt_utf8_rune_count_in_string",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // `rune_count`, `is_valid` and `full_rune` take `Vec<u8>`; their
            // `*_in_string` twins take a `String`. Each has to reach the shim
            // whose parameter is the pointer the call actually passes.
            "utf8::rune_count" => (
                "gos_rt_utf8_rune_count_bytes",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "utf8::rune_len" => (
                "gos_rt_utf8_rune_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "utf8::valid_rune" => ("gos_rt_utf8_valid_rune", self.tcx.bool_ty()),
            "utf8::valid_string" => ("gos_rt_utf8_valid_string", self.tcx.bool_ty()),
            "utf8::is_valid" => ("gos_rt_utf8_is_valid", self.tcx.bool_ty()),
            "utf8::full_rune_in_string" => ("gos_rt_utf8_full_rune_in_string", self.tcx.bool_ty()),
            "utf8::full_rune" => ("gos_rt_utf8_full_rune", self.tcx.bool_ty()),
            "utf8::rune_start" => ("gos_rt_utf8_rune_start", self.tcx.bool_ty()),
            _ => return None,
        })
    }

    fn lower_unicode_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // ---------------------------------------------------------------
            // std::unicode - general-category predicates, casing,
            // normalization, segmentation. Char args lower as u32,
            // string args as `*const c_char`, bool results as i64
            // (auto-truncated to i1 by the LLVM lowerer). Vec<String>
            // returns route through `gos_rt_unicode_*` helpers that
            // build a GosVec with `elem_kind = STRING`.
            "unicode::is_letter" => ("gos_rt_unicode_is_letter", self.tcx.bool_ty()),
            "unicode::is_digit" => ("gos_rt_unicode_is_digit", self.tcx.bool_ty()),
            "unicode::is_number" => ("gos_rt_unicode_is_number", self.tcx.bool_ty()),
            "unicode::is_space" => ("gos_rt_unicode_is_space", self.tcx.bool_ty()),
            "unicode::is_upper" => ("gos_rt_unicode_is_upper", self.tcx.bool_ty()),
            "unicode::is_lower" => ("gos_rt_unicode_is_lower", self.tcx.bool_ty()),
            "unicode::is_title" => ("gos_rt_unicode_is_title", self.tcx.bool_ty()),
            "unicode::is_punct" => ("gos_rt_unicode_is_punct", self.tcx.bool_ty()),
            "unicode::is_symbol" => ("gos_rt_unicode_is_symbol", self.tcx.bool_ty()),
            "unicode::is_mark" => ("gos_rt_unicode_is_mark", self.tcx.bool_ty()),
            "unicode::is_print" => ("gos_rt_unicode_is_print", self.tcx.bool_ty()),
            "unicode::is_graphic" => ("gos_rt_unicode_is_graphic", self.tcx.bool_ty()),
            "unicode::is_control" => ("gos_rt_unicode_is_control", self.tcx.bool_ty()),
            "unicode::is_assigned" => ("gos_rt_unicode_is_assigned", self.tcx.bool_ty()),
            "unicode::combining_class" => (
                "gos_rt_unicode_combining_class",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "unicode::to_upper" => ("gos_rt_unicode_to_upper", self.tcx.char_ty()),
            "unicode::to_lower" => ("gos_rt_unicode_to_lower", self.tcx.char_ty()),
            "unicode::to_title" => ("gos_rt_unicode_to_title", self.tcx.char_ty()),
            "unicode::simple_fold" => ("gos_rt_unicode_simple_fold", self.tcx.char_ty()),
            "unicode::to_upper_str" => ("gos_rt_unicode_to_upper_str", self.tcx.string_ty()),
            "unicode::to_lower_str" => ("gos_rt_unicode_to_lower_str", self.tcx.string_ty()),
            "unicode::fold_case" => ("gos_rt_unicode_fold_case", self.tcx.string_ty()),
            "unicode::nfc" => ("gos_rt_unicode_nfc", self.tcx.string_ty()),
            "unicode::nfd" => ("gos_rt_unicode_nfd", self.tcx.string_ty()),
            "unicode::nfkc" => ("gos_rt_unicode_nfkc", self.tcx.string_ty()),
            "unicode::nfkd" => ("gos_rt_unicode_nfkd", self.tcx.string_ty()),
            "unicode::is_nfc" => ("gos_rt_unicode_is_nfc", self.tcx.bool_ty()),
            "unicode::is_nfd" => ("gos_rt_unicode_is_nfd", self.tcx.bool_ty()),
            "unicode::is_nfkc" => ("gos_rt_unicode_is_nfkc", self.tcx.bool_ty()),
            "unicode::is_nfkd" => ("gos_rt_unicode_is_nfkd", self.tcx.bool_ty()),
            "unicode::graphemes" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_unicode_graphemes", v)
            }
            "unicode::grapheme_count" => (
                "gos_rt_unicode_grapheme_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "unicode::words" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_unicode_words", v)
            }
            "unicode::word_bounds" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_unicode_word_bounds", v)
            }
            "unicode::word_count" => (
                "gos_rt_unicode_word_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "unicode::sentences" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_unicode_sentences", v)
            }
            "unicode::sentence_count" => (
                "gos_rt_unicode_sentence_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    fn lower_encoding_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // encoding::utf16::* (previously VM-only).
            "encoding::utf16::is_surrogate" | "utf16::is_surrogate" => {
                ("gos_rt_utf16_is_surrogate", self.tcx.bool_ty())
            }
            "encoding::utf16::rune_len" | "utf16::rune_len" => (
                "gos_rt_utf16_rune_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "encoding::utf16::decode_surrogate_pair" | "utf16::decode_surrogate_pair" => {
                let c = self.tcx.char_ty();
                let substs = gossamer_types::Substs::from_types([c]);
                let opt = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                ("gos_rt_utf16_decode_surrogate_pair", opt)
            }
            "encoding::utf16::encode_string" | "utf16::encode_string" => {
                let u16_ty = self.tcx.int_ty(gossamer_types::IntTy::U16);
                (
                    "gos_rt_utf16_encode_string",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u16_ty)),
                )
            }
            "encoding::utf16::decode_to_string" | "utf16::decode_to_string" => {
                ("gos_rt_utf16_decode_to_string", self.tcx.string_ty())
            }
            "encoding::hex::encode" | "hex::encode" => {
                ("gos_rt_encoding_hex_encode", self.tcx.string_ty())
            }
            "encoding::hex::decode" | "hex::decode" => {
                ("gos_rt_encoding_hex_decode", self.result_vec_u8_error_ty())
            }
            "encoding::base64::encode" | "base64::encode" => {
                ("gos_rt_encoding_base64_encode", self.tcx.string_ty())
            }
            "encoding::base64::decode" | "base64::decode" => (
                "gos_rt_encoding_base64_decode",
                self.result_vec_u8_error_ty(),
            ),
            "encoding::base32::encode" | "base32::encode" => {
                ("gos_rt_encoding_base32_encode", self.tcx.string_ty())
            }
            "encoding::base32::encode_hex" | "base32::encode_hex" => {
                ("gos_rt_encoding_base32_encode_hex", self.tcx.string_ty())
            }
            "encoding::base32::decode" | "base32::decode" => (
                "gos_rt_encoding_base32_decode",
                self.result_vec_u8_error_ty(),
            ),
            "encoding::base32::decode_hex" | "base32::decode_hex" => (
                "gos_rt_encoding_base32_decode_hex",
                self.result_vec_u8_error_ty(),
            ),
            // encoding::binary - put_* return [u8]; get_* return
            // Result<i64>; uvarint/varint return Result<(i64,i64)>.
            "encoding::binary::put_u8"
            | "encoding::binary::put_u16_be"
            | "encoding::binary::put_u16_le"
            | "encoding::binary::put_u32_be"
            | "encoding::binary::put_u32_le"
            | "encoding::binary::put_u64_be"
            | "encoding::binary::put_u64_le"
            | "encoding::binary::put_uvarint"
            | "encoding::binary::put_varint" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let sym = match joined {
                    "encoding::binary::put_u8" => "gos_rt_bin_put_u8",
                    "encoding::binary::put_u16_be" => "gos_rt_bin_put_u16_be",
                    "encoding::binary::put_u16_le" => "gos_rt_bin_put_u16_le",
                    "encoding::binary::put_u32_be" => "gos_rt_bin_put_u32_be",
                    "encoding::binary::put_u32_le" => "gos_rt_bin_put_u32_le",
                    "encoding::binary::put_u64_be" => "gos_rt_bin_put_u64_be",
                    "encoding::binary::put_u64_le" => "gos_rt_bin_put_u64_le",
                    "encoding::binary::put_uvarint" => "gos_rt_bin_put_uvarint",
                    _ => "gos_rt_bin_put_varint",
                };
                (sym, self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)))
            }
            _ => return None,
        })
    }

    fn lower_encoding_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "encoding::binary::get_u8"
            | "encoding::binary::get_u16_be"
            | "encoding::binary::get_u16_le"
            | "encoding::binary::get_u32_be"
            | "encoding::binary::get_u32_le"
            | "encoding::binary::get_u64_be"
            | "encoding::binary::get_u64_le" => {
                let sym = match joined {
                    "encoding::binary::get_u8" => "gos_rt_bin_get_u8",
                    "encoding::binary::get_u16_be" => "gos_rt_bin_get_u16_be",
                    "encoding::binary::get_u16_le" => "gos_rt_bin_get_u16_le",
                    "encoding::binary::get_u32_be" => "gos_rt_bin_get_u32_be",
                    "encoding::binary::get_u32_le" => "gos_rt_bin_get_u32_le",
                    "encoding::binary::get_u64_be" => "gos_rt_bin_get_u64_be",
                    _ => "gos_rt_bin_get_u64_le",
                };
                (sym, self.result_i64_error_adt_ty())
            }
            // The `_at` family reads and writes through the caller's own
            // buffer, so it answers the natural unsigned width rather than
            // the lossy `i64` the append-and-return forms use.
            "encoding::binary::get_u16_be_at"
            | "encoding::binary::get_u16_le_at"
            | "encoding::binary::get_u32_be_at"
            | "encoding::binary::get_u32_le_at"
            | "encoding::binary::get_u64_be_at"
            | "encoding::binary::get_u64_le_at" => {
                let (sym, width) = match joined {
                    "encoding::binary::get_u16_be_at" => {
                        ("gos_rt_bin_get_u16_be_at", gossamer_types::IntTy::U16)
                    }
                    "encoding::binary::get_u16_le_at" => {
                        ("gos_rt_bin_get_u16_le_at", gossamer_types::IntTy::U16)
                    }
                    "encoding::binary::get_u32_be_at" => {
                        ("gos_rt_bin_get_u32_be_at", gossamer_types::IntTy::U32)
                    }
                    "encoding::binary::get_u32_le_at" => {
                        ("gos_rt_bin_get_u32_le_at", gossamer_types::IntTy::U32)
                    }
                    "encoding::binary::get_u64_be_at" => {
                        ("gos_rt_bin_get_u64_be_at", gossamer_types::IntTy::U64)
                    }
                    _ => ("gos_rt_bin_get_u64_le_at", gossamer_types::IntTy::U64),
                };
                let value = self.tcx.int_ty(width);
                (sym, self.result_of(value))
            }
            "encoding::binary::put_u16_be_at"
            | "encoding::binary::put_u16_le_at"
            | "encoding::binary::put_u32_be_at"
            | "encoding::binary::put_u32_le_at"
            | "encoding::binary::put_u64_be_at"
            | "encoding::binary::put_u64_le_at" => {
                let sym = match joined {
                    "encoding::binary::put_u16_be_at" => "gos_rt_bin_put_u16_be_at",
                    "encoding::binary::put_u16_le_at" => "gos_rt_bin_put_u16_le_at",
                    "encoding::binary::put_u32_be_at" => "gos_rt_bin_put_u32_be_at",
                    "encoding::binary::put_u32_le_at" => "gos_rt_bin_put_u32_le_at",
                    "encoding::binary::put_u64_be_at" => "gos_rt_bin_put_u64_be_at",
                    _ => "gos_rt_bin_put_u64_le_at",
                };
                let unit = self.tcx.unit();
                (sym, self.result_of(unit))
            }
            "encoding::binary::uvarint" => ("gos_rt_bin_uvarint", self.result_pair_i64_error_ty()),
            "encoding::binary::varint" => ("gos_rt_bin_varint", self.result_pair_i64_error_ty()),
            "encoding::csv::parse_line" | "csv::parse_line" => {
                let s = self.tcx.string_ty();
                (
                    "gos_rt_csv_parse_line",
                    self.tcx.intern(gossamer_types::TyKind::Vec(s)),
                )
            }
            "encoding::csv::read" | "csv::read" => {
                ("gos_rt_csv_read", self.result_vec_vec_string_error_ty())
            }
            "encoding::csv::write" | "csv::write" => ("gos_rt_csv_write", self.tcx.string_ty()),
            "encoding::ascii85::encode" | "ascii85::encode" => {
                ("gos_rt_encoding_ascii85_encode", self.tcx.string_ty())
            }
            "encoding::ascii85::decode" | "ascii85::decode" => (
                "gos_rt_encoding_ascii85_decode",
                self.result_vec_u8_error_ty(),
            ),
            "encoding::xml::escape" | "xml::escape" => {
                ("gos_rt_encoding_xml_escape", self.tcx.string_ty())
            }
            "encoding::xml::parse" | "xml::parse" => {
                ("gos_rt_xml_parse", self.result_i64_error_adt_ty())
            }
            "encoding::xml::encode" | "xml::encode" => ("gos_rt_xml_encode", self.tcx.string_ty()),
            "encoding::base32::encode_string" | "base32::encode_string" => {
                ("gos_rt_encoding_base32_encode_string", self.tcx.string_ty())
            }
            "encoding::base32::decode_string" | "base32::decode_string" => (
                "gos_rt_encoding_base32_decode_string",
                self.result_string_error_adt_ty(),
            ),
            _ => return None,
        })
    }

    fn lower_strings_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.7.0 stdlib wiring - string-surface free fns that
            // the VM already exposes but that lacked a compiled-tier
            // runtime entry point. Each maps a fully-qualified
            // module path to the matching `gos_rt_*` helper.
            "strings::join" => ("gos_rt_strings_join", self.tcx.string_ty()),
            "strings::split_once" | "strings::rsplit_once" => {
                let s = self.tcx.string_ty();
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                let substs = gossamer_types::Substs::from_types([tup]);
                let opt_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                let sym = if joined == "strings::split_once" {
                    "gos_rt_str_split_once"
                } else {
                    "gos_rt_str_rsplit_once"
                };
                (sym, opt_ty)
            }
            "strings::count" => (
                "gos_rt_str_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // 0.10.0 - string-surface free fns. Each routes to the
            // matching `gos_rt_str_*` runtime helper (same shim that
            // already backs the method-call form). Without these,
            // MIR emits `@strings::trim` etc. as a literal symbol and
            // LLVM `opt` fails with `use of undefined value`.
            "strings::trim" => ("gos_rt_str_trim", self.tcx.string_ty()),
            "strings::trim_start" => ("gos_rt_str_trim_start", self.tcx.string_ty()),
            "strings::trim_end" => ("gos_rt_str_trim_end", self.tcx.string_ty()),
            "strings::to_uppercase" => ("gos_rt_str_to_upper", self.tcx.string_ty()),
            "strings::to_lowercase" => ("gos_rt_str_to_lower", self.tcx.string_ty()),
            "strings::contains" => ("gos_rt_str_contains", self.tcx.bool_ty()),
            "strings::replace" => ("gos_rt_str_replace", self.tcx.string_ty()),
            "strings::starts_with" => ("gos_rt_str_starts_with", self.tcx.bool_ty()),
            "strings::ends_with" => ("gos_rt_str_ends_with", self.tcx.bool_ty()),
            "strings::repeat" => ("gos_rt_str_repeat", self.tcx.string_ty()),
            "strings::byte_len" => (
                "gos_rt_str_byte_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "strings::byte_at" => (
                "gos_rt_str_byte_at",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "strings::substring" => ("gos_rt_str_substring", self.tcx.string_ty()),
            "strings::bytes" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
                ("gos_rt_str_as_bytes", v)
            }
            "strings::chars" => {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(i64_ty));
                ("gos_rt_str_chars", v)
            }
            "strings::lines" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_str_lines", v)
            }
            "strings::split" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_str_split", v)
            }
            "strings::find" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let substs = gossamer_types::Substs::from_types([i]);
                let opt_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                ("gos_rt_str_find_opt", opt_ty)
            }
            "strings::trim_start_matches" => ("gos_rt_str_lstrip_chars", self.tcx.string_ty()),
            "strings::trim_end_matches" => ("gos_rt_str_rstrip_chars", self.tcx.string_ty()),
            "strings::center" => ("gos_rt_str_center", self.tcx.string_ty()),
            "strings::slice" => ("gos_rt_str_slice", self.result_string_error_adt_ty()),
            // 0.10.0 - remaining strings::* free fns previously
            // VM-only. Each routes to the matching gos_rt_str_*
            // runtime helper backed by gossamer_std::strings.
            "strings::splitn" => {
                let s = self.tcx.string_ty();
                (
                    "gos_rt_str_splitn",
                    self.tcx.intern(gossamer_types::TyKind::Vec(s)),
                )
            }
            "strings::split_whitespace" => {
                let s = self.tcx.string_ty();
                (
                    "gos_rt_str_split_whitespace",
                    self.tcx.intern(gossamer_types::TyKind::Vec(s)),
                )
            }
            "strings::replacen" => ("gos_rt_str_replacen", self.tcx.string_ty()),
            "strings::to_title" => ("gos_rt_str_to_title", self.tcx.string_ty()),
            "strings::trim_matches" => ("gos_rt_str_trim_matches", self.tcx.string_ty()),
            "strings::pad_left" => ("gos_rt_str_pad_left", self.tcx.string_ty()),
            "strings::pad_right" => ("gos_rt_str_pad_right", self.tcx.string_ty()),
            "strings::contains_any" => ("gos_rt_str_contains_any", self.tcx.bool_ty()),
            "strings::equal_fold" => ("gos_rt_str_equal_fold", self.tcx.bool_ty()),
            "strings::parse" => ("gos_rt_parse_i64_result", self.result_i64_error_adt_ty()),
            _ => return None,
        })
    }

    fn lower_strings_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "strings::find_any" => ("gos_rt_str_index_any", self.option_i64_adt_ty()),
            "strings::rfind_any" => ("gos_rt_str_last_index_any", self.option_i64_adt_ty()),
            "strings::strip_prefix" => ("gos_rt_str_strip_prefix", self.option_string_adt_ty()),
            "strings::strip_suffix" => ("gos_rt_str_strip_suffix", self.option_string_adt_ty()),
            "strings::to_i64" => ("gos_rt_str_to_i64_opt", self.option_i64_adt_ty()),
            "strings::to_f64" => ("gos_rt_str_to_f64_opt", self.option_f64_adt_ty()),
            "strings::to_bool" => ("gos_rt_str_to_bool_opt", self.option_bool_adt_ty()),
            // String-as-receiver `rfind` returns Option<i64>; same
            // discriminant-packed shape as `find_opt`.
            "strings::rfind" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let substs = gossamer_types::Substs::from_types([i]);
                let opt_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                ("gos_rt_str_rfind_opt", opt_ty)
            }
            _ => return None,
        })
    }

    fn lower_strconv_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.10.0 - strconv free fns. parse_* return
            // Result<T, errors::Error> packed as a *mut GosResult;
            // format_* return String.
            "strconv::parse_i64" => ("gos_rt_strconv_parse_i64", self.result_i64_error_adt_ty()),
            "strconv::parse_u64" => ("gos_rt_strconv_parse_u64", self.result_i64_error_adt_ty()),
            "strconv::parse_f64" => ("gos_rt_strconv_parse_f64", self.result_f64_error_adt_ty()),
            "strconv::parse_bool" => ("gos_rt_strconv_parse_bool", self.result_bool_error_adt_ty()),
            "strconv::parse_i64_radix" => (
                "gos_rt_strconv_parse_i64_radix",
                self.result_i64_error_adt_ty(),
            ),
            "strconv::format_i64_radix" => {
                ("gos_rt_strconv_format_i64_radix", self.tcx.string_ty())
            }
            "strconv::quote" | "__gos_strconv_quote" => {
                ("gos_rt_strconv_quote", self.tcx.string_ty())
            }
            "strconv::unquote" => ("gos_rt_strconv_unquote", self.result_string_error_adt_ty()),
            // Format-spec intrinsics from `{:spec}` expansion. `__fmt_radix`
            // and `__fmt_upper` reuse the strconv/strings shims; `__fmt_pad`
            // applies width/alignment/fill to an already-rendered string.
            "__fmt_radix" => ("gos_rt_fmt_radix_i64", self.tcx.string_ty()),
            "__fmt_upper" => ("gos_rt_str_to_upper", self.tcx.string_ty()),
            "__fmt_pad" => ("gos_rt_fmt_pad", self.tcx.string_ty()),
            "strconv::format_i64" => ("gos_rt_strconv_format_i64", self.tcx.string_ty()),
            "strconv::format_u64" => ("gos_rt_strconv_format_i64", self.tcx.string_ty()),
            "strconv::format_f64" => ("gos_rt_strconv_format_f64", self.tcx.string_ty()),
            "strconv::format_bool" => ("gos_rt_strconv_format_bool", self.tcx.string_ty()),
            _ => return None,
        })
    }

    fn lower_compress_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "compress::gzip::encode" | "gzip::encode" => {
                ("gos_rt_compress_gzip_encode", self.result_vec_u8_error_ty())
            }
            "compress::gzip::decode" | "gzip::decode" => {
                ("gos_rt_compress_gzip_decode", self.result_vec_u8_error_ty())
            }
            "compress::flate::compress" | "flate::compress" => (
                "gos_rt_compress_flate_compress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::flate::decompress" | "flate::decompress" => (
                "gos_rt_compress_flate_decompress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::bzip2::compress" | "bzip2::compress" => (
                "gos_rt_compress_bzip2_compress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::bzip2::decompress" | "bzip2::decompress" => (
                "gos_rt_compress_bzip2_decompress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::zstd::encode" | "zstd::encode" => {
                ("gos_rt_compress_zstd_encode", self.result_vec_u8_error_ty())
            }
            "compress::zstd::encode_level" | "zstd::encode_level" => (
                "gos_rt_compress_zstd_encode_level",
                self.result_vec_u8_error_ty(),
            ),
            "compress::zstd::decode" | "zstd::decode" => {
                ("gos_rt_compress_zstd_decode", self.result_vec_u8_error_ty())
            }
            "compress::zlib::compress" | "zlib::compress" => (
                "gos_rt_compress_zlib_compress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::zlib::decompress" | "zlib::decompress" => (
                "gos_rt_compress_zlib_decompress",
                self.result_vec_u8_error_ty(),
            ),
            _ => return None,
        })
    }

    fn lower_codec_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // pem leaf intrinsics (called from injected Gossamer
            // wrappers; return tuples/bytes the wrappers fold into
            // real `Block` structs).
            "__gos_pem_decode_raw" => {
                let tup = self.tuple_str_bytes_ty();
                ("gos_rt_pem_decode_raw", self.result_of(tup))
            }
            "__gos_pem_decode_all_raw" => {
                let tup = self.tuple_str_bytes_ty();
                let vec = self.tcx.intern(gossamer_types::TyKind::Vec(tup));
                ("gos_rt_pem_decode_all_raw", self.result_of(vec))
            }
            "__gos_pem_encode_raw" => ("gos_rt_pem_encode_raw", self.tcx.string_ty()),
            "__gos_x509_parse_pem_raw" => {
                let tup = self.tuple_cert_info_ty();
                ("gos_rt_x509_parse_pem_raw", self.result_of(tup))
            }
            "__gos_fs_metadata_raw" => {
                let tup = self.tuple_fs_metadata_ty();
                ("gos_rt_fs_metadata_raw", self.result_of(tup))
            }
            "__gos_tar_read_raw" | "__gos_zip_read_raw" => {
                let tup = self.tuple_entry_ty();
                let vec = self.tcx.intern(gossamer_types::TyKind::Vec(tup));
                let sym = if joined == "__gos_tar_read_raw" {
                    "gos_rt_tar_read_raw"
                } else {
                    "gos_rt_zip_read_raw"
                };
                (sym, self.result_of(vec))
            }
            // tar/zip write take `[(String,[u8])]` tuples and return
            // Result<[u8]> - no struct, so they lower directly.
            "archive::tar::write" | "tar::write" => {
                ("gos_rt_tar_write", self.result_vec_u8_error_ty())
            }
            "archive::zip::write" | "zip::write" => {
                ("gos_rt_zip_write", self.result_vec_u8_error_ty())
            }
            "html::escape" => ("gos_rt_html_escape", self.tcx.string_ty()),
            "html::unescape" => ("gos_rt_html_unescape", self.tcx.string_ty()),
            "html::template::render_json" => (
                "gos_rt_html_template_render_json",
                self.result_string_error_adt_ty(),
            ),
            _ => return None,
        })
    }

    fn lower_sql_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // database::sql leaf intrinsics (called from injected
            // Gossamer wrappers; scalar/string-shaped, sentinel
            // error convention with gos_rt_sql_last_error).
            "__gos_sql_open_raw" => (
                "gos_rt_sql_open",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_last_error_raw" => ("gos_rt_sql_last_error", self.tcx.string_ty()),
            "__gos_sql_drivers_raw" => ("gos_rt_sql_drivers", self.tcx.string_ty()),
            "__gos_sql_params_new_raw" => (
                "gos_rt_sql_params_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_null_raw" => (
                "gos_rt_sql_params_push_null",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_bool_raw" => (
                "gos_rt_sql_params_push_bool",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_int_raw" => (
                "gos_rt_sql_params_push_int",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_float_raw" => (
                "gos_rt_sql_params_push_float",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_text_raw" => (
                "gos_rt_sql_params_push_text",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_blob_raw" => (
                "gos_rt_sql_params_push_blob",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_execute_raw" => (
                "gos_rt_sql_conn_execute_params",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_query_raw" => (
                "gos_rt_sql_conn_query_params",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_begin_raw" => (
                "gos_rt_sql_conn_begin",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_begin_with_raw" => (
                "gos_rt_sql_conn_begin_with",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_ping_raw" => (
                "gos_rt_sql_conn_ping",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_set_busy_timeout_raw" => (
                "gos_rt_sql_conn_set_busy_timeout",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_interrupt_raw" => (
                "gos_rt_sql_conn_interrupt",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_close_raw" => (
                "gos_rt_sql_conn_close",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_rows_next_row_raw" => (
                "gos_rt_sql_rows_next_row",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_rows_close_raw" => (
                "gos_rt_sql_rows_close",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_rows_columns_raw" => ("gos_rt_sql_rows_columns", self.tcx.string_ty()),
            "__gos_sql_row_kind_raw" => (
                "gos_rt_sql_row_kind",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_row_get_i64_raw" => (
                "gos_rt_sql_row_get_i64",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    fn lower_sql_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "__gos_sql_row_get_f64_raw" => (
                "gos_rt_sql_row_get_f64",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "__gos_sql_row_get_bool_raw" => (
                "gos_rt_sql_row_get_bool_i64",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_row_get_text_raw" => ("gos_rt_sql_row_get_text", self.tcx.string_ty()),
            "__gos_sql_row_get_blob_raw" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_sql_row_get_blob_vec",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "__gos_sql_row_width_raw" => (
                "gos_rt_sql_row_width",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_commit_raw" => (
                "gos_rt_sql_tx_commit",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_rollback_raw" => (
                "gos_rt_sql_tx_rollback",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_execute_raw" => (
                "gos_rt_sql_tx_execute",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_savepoint_raw" => (
                "gos_rt_sql_tx_savepoint",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_release_savepoint_raw" => (
                "gos_rt_sql_tx_release_savepoint",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_rollback_to_savepoint_raw" => (
                "gos_rt_sql_tx_rollback_to_savepoint",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_execute_params_raw" => (
                "gos_rt_sql_tx_execute_params",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_query_params_raw" => (
                "gos_rt_sql_tx_query_params",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_prepare_raw" => (
                "gos_rt_sql_conn_prepare",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_stmt_execute_raw" => (
                "gos_rt_sql_stmt_execute",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_stmt_query_raw" => (
                "gos_rt_sql_stmt_query",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_stmt_close_raw" => (
                "gos_rt_sql_stmt_close",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_copy_in_raw" => (
                "gos_rt_sql_conn_copy_in",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_copy_out_run_raw" => (
                "gos_rt_sql_conn_copy_out_run",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_copy_out_take_raw" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_sql_conn_copy_out_take",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            _ => return None,
        })
    }

    fn lower_sql_3_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "__gos_sql_conn_listen_raw" => (
                "gos_rt_sql_conn_listen",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_unlisten_raw" => (
                "gos_rt_sql_conn_unlisten",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_poll_notification_raw" => (
                "gos_rt_sql_conn_poll_notification",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_notification_channel_raw" => {
                ("gos_rt_sql_notification_channel", self.tcx.string_ty())
            }
            "__gos_sql_notification_payload_raw" => {
                ("gos_rt_sql_notification_payload", self.tcx.string_ty())
            }
            "__gos_sql_notification_pid_raw" => (
                "gos_rt_sql_notification_pid",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_new_raw" => (
                "gos_rt_sql_pool_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_get_raw" => (
                "gos_rt_sql_pool_get",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_live_raw" => (
                "gos_rt_sql_pool_live",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_idle_raw" => (
                "gos_rt_sql_pool_idle",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_close_idle_raw" => (
                "gos_rt_sql_pool_close_idle",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_migrate_up_raw" => (
                "gos_rt_sql_migrate_up",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // Gossamer-native driver side-channel helpers. The `.gos`
            // driver reads inputs / writes outputs through these; the
            // writers return unit, the readers their slot field type,
            // and the value constructors / accessors traffic in
            // sql::Value handles (i64).
            "__gos_sql_native_url" => ("gos_rt_sql_native_url", self.tcx.string_ty()),
            "__gos_sql_native_sql" => ("gos_rt_sql_native_sql", self.tcx.string_ty()),
            "__gos_sql_native_parent" => (
                "gos_rt_sql_native_parent",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_out_handle" => (
                "gos_rt_sql_native_out_handle",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_iso" => (
                "gos_rt_sql_native_iso",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_timeout" => (
                "gos_rt_sql_native_timeout",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_channel" => ("gos_rt_sql_native_channel", self.tcx.string_ty()),
            "__gos_sql_native_param_count" => (
                "gos_rt_sql_native_param_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_param" => (
                "gos_rt_sql_native_param",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_data" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_sql_native_data",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "__gos_sql_native_push_column" => ("gos_rt_sql_native_push_column", self.tcx.unit()),
            _ => return None,
        })
    }

    fn lower_sql_4_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "__gos_sql_native_push_value" => ("gos_rt_sql_native_push_value", self.tcx.unit()),
            "__gos_sql_native_row_ready" => ("gos_rt_sql_native_row_ready", self.tcx.unit()),
            "__gos_sql_native_set_error" => ("gos_rt_sql_native_set_error", self.tcx.unit()),
            "__gos_sql_native_emit_bytes" => ("gos_rt_sql_native_emit_bytes", self.tcx.unit()),
            "__gos_sql_native_set_notification" => {
                ("gos_rt_sql_native_set_notification", self.tcx.unit())
            }
            "__gos_sql_native_set_handle" => ("gos_rt_sql_native_set_handle", self.tcx.unit()),
            "__gos_sql_native_handle" => (
                "gos_rt_sql_native_handle",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_null" => (
                "gos_rt_sql_native_value_null",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_bool" => (
                "gos_rt_sql_native_value_bool",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_int" => (
                "gos_rt_sql_native_value_int",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_float" => (
                "gos_rt_sql_native_value_float",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_text" => (
                "gos_rt_sql_native_value_text",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_blob" => (
                "gos_rt_sql_native_value_blob",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_kind" => (
                "gos_rt_sql_native_value_kind",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_int_of" => (
                "gos_rt_sql_native_value_int_of",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_float_of" => (
                "gos_rt_sql_native_value_float_of",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "__gos_sql_native_value_text_of" => {
                ("gos_rt_sql_native_value_text_of", self.tcx.string_ty())
            }
            "__gos_sql_native_value_blob_of" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_sql_native_value_blob_of",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            _ => return None,
        })
    }

    fn lower_env_thread_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.10.0 - env aliases (the os:: spelling is already wired
            // above; the env:: spelling matches `use std::env`).
            "env::set_var" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_os_set_env", result_ty)
            }
            "env::unset_var" => ("gos_rt_os_unset_env", self.tcx.unit()),
            "env::set_current_dir" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_env_set_current_dir", result_ty)
            }
            // `thread::yield_now()` - goroutine-aware yield (Gosched).
            "thread::yield_now" => ("gos_rt_go_yield", self.tcx.unit()),
            "thread::num_cpus" => (
                "gos_rt_thread_num_cpus",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "env::temp_dir" => ("gos_rt_env_temp_dir", self.tcx.string_ty()),
            "env::home_dir" => ("gos_rt_env_home_dir", self.option_string_adt_ty()),
            "env::vars" => {
                let string_ty = self.tcx.string_ty();
                let map_ty = self.tcx.intern(gossamer_types::TyKind::HashMap {
                    key: string_ty,
                    value: string_ty,
                    ordered: false,
                });
                ("gos_rt_env_vars", map_ty)
            }
            _ => return None,
        })
    }

    fn lower_time_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.10.0 - time::Duration helpers. Duration is represented
            // as i64 nanoseconds end-to-end through the compiled tier.
            "time::Duration::from_secs" => (
                "gos_rt_duration_from_secs",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::Duration::from_millis" => (
                "gos_rt_duration_from_millis",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::Duration::from_micros" => (
                "gos_rt_duration_from_micros",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::Duration::as_millis" => (
                "gos_rt_duration_as_millis",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::Duration::as_secs" => (
                "gos_rt_duration_as_secs",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::Duration::as_micros" => (
                "gos_rt_duration_as_micros",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_time_location_raw" => (
                "gos_rt_time_location_raw",
                self.result_string_error_adt_ty(),
            ),
            "__gos_time_fixed_location_raw" => (
                "gos_rt_time_fixed_location_raw",
                self.result_string_error_adt_ty(),
            ),
            "__gos_time_civil_raw" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tuple = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![i; 9]));
                ("gos_rt_time_civil_raw", self.result_of(tuple))
            }
            "__gos_time_resolve_raw" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tuple = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![i; 3]));
                ("gos_rt_time_resolve_raw", self.result_of(tuple))
            }
            "__gos_time_format_in_raw" => (
                "gos_rt_time_format_in_raw",
                self.result_string_error_adt_ty(),
            ),
            "__gos_time_add_date_raw" => {
                ("gos_rt_time_add_date_raw", self.result_i64_error_adt_ty())
            }
            "time::format_rfc3339" => {
                let s = self.tcx.string_ty();
                let substs = gossamer_types::Substs::from_types([s, s]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_time_format_rfc3339", result_ty)
            }
            "time::parse_rfc3339" => {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let s = self.tcx.string_ty();
                let substs = gossamer_types::Substs::from_types([i64_ty, s]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_time_parse_rfc3339", result_ty)
            }
            // 0.10.0 - time::* free fns previously VM-only. The
            // monotonic/now shims already existed in the runtime;
            // these arms route the language-level calls to them.
            "time::sleep" => ("gos_rt_sleep_ms", self.tcx.unit()),
            "time::sleep_ctx" => ("gos_rt_sleep_ms_ctx", self.tcx.bool_ty()),
            "smtp::send" => ("gos_rt_smtp_send", self.result_unit_error_adt_ty()),
            "smtp::send_auth" => ("gos_rt_smtp_send_auth", self.result_unit_error_adt_ty()),
            "time::freeze" => ("gos_rt_time_freeze", self.tcx.unit()),
            "time::advance" => (
                "gos_rt_time_advance",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::unfreeze" => ("gos_rt_time_unfreeze", self.tcx.unit()),
            "time::is_frozen" => ("gos_rt_time_is_frozen", self.tcx.bool_ty()),
            "time::now" | "time::unix_ms" => (
                "gos_rt_time_now_ms",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::now_nanos" => (
                "gos_rt_time_now_nanos",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::monotonic_ms" => (
                "gos_rt_monotonic_ms",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::monotonic_nanos" => (
                "gos_rt_monotonic_nanos",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::since_ms" => (
                "gos_rt_time_since_ms",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // `time::Instant` is a transparent `i64` of monotonic ms.
            // `Instant::now()` samples the monotonic clock; `elapsed_ms`
            // is the monotonic delta from that sample, so both route to
            // the existing monotonic helpers.
            "time::Instant::now" => (
                "gos_rt_monotonic_ms",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::Instant::elapsed_ms" => (
                "gos_rt_time_since_ms",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    fn lower_id_misc_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "uuid::v4" => ("gos_rt_uuid_v4", self.tcx.string_ty()),
            "uuid::v7" => ("gos_rt_uuid_v7", self.tcx.string_ty()),
            "uuid::is_valid" => ("gos_rt_uuid_is_valid", self.tcx.bool_ty()),
            "uuid::normalize" => ("gos_rt_uuid_normalize", self.tcx.string_ty()),
            "uuid::simple" => ("gos_rt_uuid_simple", self.tcx.string_ty()),
            "user::current_name" => ("gos_rt_os_user_current_name", self.tcx.string_ty()),
            "user::current_uid" => (
                "gos_rt_os_user_current_uid",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "user::current_gid" => (
                "gos_rt_os_user_current_gid",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "user::current_home" => ("gos_rt_os_user_current_home", self.tcx.string_ty()),
            "user::lookup_uid" => ("gos_rt_os_user_lookup_uid", self.tcx.string_ty()),
            "user::lookup_name" => (
                "gos_rt_os_user_lookup_name",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "netip::is_valid" => ("gos_rt_netip_is_valid", self.tcx.bool_ty()),
            "netip::is_v4" => ("gos_rt_netip_is_v4", self.tcx.bool_ty()),
            "netip::is_v6" => ("gos_rt_netip_is_v6", self.tcx.bool_ty()),
            "netip::is_loopback" => ("gos_rt_netip_is_loopback", self.tcx.bool_ty()),
            "netip::is_unspecified" => ("gos_rt_netip_is_unspecified", self.tcx.bool_ty()),
            "netip::is_multicast" => ("gos_rt_netip_is_multicast", self.tcx.bool_ty()),
            "netip::is_private" => ("gos_rt_netip_is_private", self.tcx.bool_ty()),
            "netip::normalize" => ("gos_rt_netip_normalize", self.tcx.string_ty()),
            "netip::host_of" => ("gos_rt_netip_host_of", self.tcx.string_ty()),
            "netip::port_of" => (
                "gos_rt_netip_port_of",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "netip::join_addr_port" => ("gos_rt_netip_join_addr_port", self.tcx.string_ty()),
            "mime::parse" => ("gos_rt_mime_parse", self.tcx.string_ty()),
            "mime::top" => ("gos_rt_mime_top", self.tcx.string_ty()),
            "mime::sub" => ("gos_rt_mime_sub", self.tcx.string_ty()),
            "mime::charset" => ("gos_rt_mime_charset", self.tcx.string_ty()),
            "mime::boundary" => ("gos_rt_mime_boundary", self.tcx.string_ty()),
            "mime::param" => ("gos_rt_mime_param", self.tcx.string_ty()),
            "mime::type_by_extension" => ("gos_rt_mime_type_by_extension", self.tcx.string_ty()),
            "mime::extension_by_type" => ("gos_rt_mime_extension_by_type", self.tcx.string_ty()),
            "mime::is_valid" => ("gos_rt_mime_is_valid", self.tcx.bool_ty()),
            "toml::to_json" | "encoding::toml::to_json" => {
                ("gos_rt_toml_to_json", self.result_string_error_adt_ty())
            }
            "toml::from_json" | "encoding::toml::from_json" => {
                ("gos_rt_toml_from_json", self.result_string_error_adt_ty())
            }
            "toml::is_valid" | "encoding::toml::is_valid" => {
                ("gos_rt_toml_is_valid", self.tcx.bool_ty())
            }
            "toml::pretty" | "encoding::toml::pretty" => {
                ("gos_rt_toml_pretty", self.result_string_error_adt_ty())
            }
            // `encoding::yaml::parse(text) -> Result<json::Value, _>`:
            // YAML projected onto the JSON value tree so the dynamic
            // document path reuses the json::Value runtime type (the VM
            // routes through the same projection).
            "yaml::parse" | "encoding::yaml::parse" => {
                ("gos_rt_yaml_parse", self.result_json_value_error_adt_ty())
            }
            "yaml::parse_all" | "encoding::yaml::parse_all" => (
                "gos_rt_yaml_parse_all",
                self.result_vec_json_value_error_ty(),
            ),
            "yaml::encode" | "encoding::yaml::encode" => {
                ("gos_rt_yaml_encode", self.result_string_error_adt_ty())
            }
            "yaml::to_json" | "encoding::yaml::to_json" => {
                ("gos_rt_yaml_to_json", self.result_string_error_adt_ty())
            }
            "yaml::from_json" | "encoding::yaml::from_json" => {
                ("gos_rt_yaml_from_json", self.result_string_error_adt_ty())
            }
            "yaml::is_valid" | "encoding::yaml::is_valid" => {
                ("gos_rt_yaml_is_valid", self.tcx.bool_ty())
            }
            _ => return None,
        })
    }

    fn lower_concurrency_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "sync::Map::new" => (
                "gos_rt_sync_map_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "sync::Map::insert" => ("gos_rt_sync_map_set", self.tcx.unit()),
            "sync::Map::remove" => ("gos_rt_sync_map_delete", self.tcx.unit()),
            "sync::Map::get" => ("gos_rt_sync_map_get", self.option_string_adt_ty()),
            "sync::Map::len" => (
                "gos_rt_sync_map_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "sync::Map::contains_key" => ("gos_rt_sync_map_contains", self.tcx.bool_ty()),
            "sync::Map::keys" => {
                let str_ty = self.tcx.string_ty();
                (
                    "gos_rt_sync_map_keys",
                    self.tcx.intern(gossamer_types::TyKind::Vec(str_ty)),
                )
            }
            // Qualified-atomic free-call spellings route to the existing
            // AtomicI64 shims (the method form already lowered).
            "sync::AtomicI64::new"
            | "AtomicI64::new"
            | "sync::AtomicU64::new"
            | "AtomicU64::new"
            | "sync::AtomicI32::new"
            | "AtomicI32::new" => (
                "gos_rt_atomic_i64_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // AtomicBool shares the i64 handle storage but mints a
            // distinct symbol so the receiver tags as `sync::AtomicBool`
            // and `load` pins to `bool` (renders `true` / `false`).
            "sync::AtomicBool::new" | "AtomicBool::new" => (
                "gos_rt_atomic_bool_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "sync::AtomicI64::load"
            | "AtomicI64::load"
            | "sync::AtomicU64::load"
            | "AtomicU64::load" => (
                "gos_rt_atomic_i64_load",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "sync::AtomicI64::store"
            | "AtomicI64::store"
            | "sync::AtomicU64::store"
            | "AtomicU64::store" => ("gos_rt_atomic_i64_store", self.tcx.unit()),
            "sync::AtomicI64::fetch_add"
            | "AtomicI64::fetch_add"
            | "sync::AtomicU64::fetch_add"
            | "AtomicU64::fetch_add" => (
                "gos_rt_atomic_i64_fetch_add",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "sync::Barrier::new" | "Barrier::new" => (
                "gos_rt_barrier_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "sync::Barrier::wait" | "Barrier::wait" => ("gos_rt_barrier_wait", self.tcx.unit()),
            "sync::Once::new" | "Once::new" => (
                "gos_rt_once_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "rand::Rng::new" | "math::rand::Rng::new" | "Rng::new" => (
                "gos_rt_math_rng_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "validate::FieldError::new" | "FieldError::new" => (
                "gos_rt_field_error_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "validate::Errors::new" | "Errors::new" => (
                "gos_rt_validate_errors_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "sync::RwLock::new" | "RwLock::new" => (
                "gos_rt_rwlock_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "sync::Shared::new" | "Shared::new" => (
                "gos_rt_shared_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "context::Context::background" | "Context::background" => (
                "gos_rt_ctx_background",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "context::Context::with_cancel" | "Context::with_cancel" => (
                "gos_rt_ctx_with_cancel",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "context::Context::with_timeout" | "Context::with_timeout" => (
                "gos_rt_ctx_with_timeout",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "metrics::Counter::new" | "Counter::new" => (
                "gos_rt_metrics_counter_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "metrics::Gauge::new" | "Gauge::new" => (
                "gos_rt_metrics_gauge_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    fn lower_concurrency_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "metrics::Histogram::new" | "Histogram::new" => (
                "gos_rt_metrics_histogram_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "metrics::Registry::new" | "Registry::new" => (
                "gos_rt_metrics_registry_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "metrics::serve_metrics" | "serve_metrics" => {
                let ty = self.result_unit_error_adt_ty();
                ("gos_rt_metrics_serve", ty)
            }
            "trace::Tracer::new" | "Tracer::new" => (
                "gos_rt_trace_tracer_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    fn lower_bytes_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "bytes::Builder::new" | "Builder::new" => (
                "gos_rt_bytes_builder_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "bytes::Builder::with_capacity" | "Builder::with_capacity" => (
                "gos_rt_bytes_builder_with_capacity",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "bytes::Buffer::new" | "Buffer::new" => (
                "gos_rt_bytes_buffer_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "bytes::Buffer::with_capacity" | "Buffer::with_capacity" => (
                "gos_rt_bytes_buffer_with_capacity",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "bytes::index_of" => ("gos_rt_bytes_index_of", self.option_i64_adt_ty()),
            // These three take and answer byte vectors, not text: a byte a
            // caller stored survives the round trip whether or not it is UTF-8.
            "bytes::split" => {
                let b = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let chunk = self.tcx.intern(gossamer_types::TyKind::Vec(b));
                (
                    "gos_rt_bytes_split",
                    self.tcx.intern(gossamer_types::TyKind::Vec(chunk)),
                )
            }
            "bytes::replace" => {
                let b = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_bytes_replace",
                    self.tcx.intern(gossamer_types::TyKind::Vec(b)),
                )
            }
            _ => return None,
        })
    }

    fn lower_collections_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // Stdlib collections beyond HashMap. The cranelift
            // intrinsic dispatch handles `HashSet::new` /
            // `BTreeMap::new` directly (no args); MIR routes the
            // call through these symbol names so the destination
            // local can be tagged with a runtime kind for method
            // dispatch.
            "Set::new" | "collections::Set::new" | "HashSet::new" | "collections::HashSet::new" => {
                (
                    "gos_rt_set_new",
                    self.tcx.int_ty(gossamer_types::IntTy::I64),
                )
            }
            "BTreeSet::new" | "collections::BTreeSet::new" => (
                "gos_rt_btree_set_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // `BTreeMap` is backed by the same map runtime as `HashMap`
            // (see the checker's `TyKind::HashMap` resolution), so its
            // constructor allocates a `GosMap`; the binding keeps its
            // `HashMap<K, V>` type and reaches the full map method surface.
            "BTreeMap::new" | "collections::BTreeMap::new" => (
                "gos_rt_map_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "Deque::new"
            | "collections::Deque::new"
            | "VecDeque::new"
            | "collections::VecDeque::new" => (
                "gos_rt_deque_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "Queue::new"
            | "collections::Queue::new"
            | "VecQueue::new"
            | "collections::VecQueue::new" => (
                "gos_rt_queue_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "Stack::new"
            | "collections::Stack::new"
            | "VecStack::new"
            | "collections::VecStack::new" => (
                "gos_rt_stack_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "BinaryHeap::new"
            | "collections::BinaryHeap::new"
            | "MaxBinaryHeap::new"
            | "collections::MaxBinaryHeap::new"
            | "MaxHeap::new"
            | "collections::MaxHeap::new" => (
                "gos_rt_bheap_max_new_i64",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "MinBinaryHeap::new"
            | "collections::MinBinaryHeap::new"
            | "MinHeap::new"
            | "collections::MinHeap::new" => (
                "gos_rt_bheap_min_new_i64",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // 0.7.0 - `HashMap::pop(m, k) -> Option<V>` free-fn shape.
            // Dispatches by the first arg's HashMap key type to the
            // string-keyed or i64-keyed runtime variant. The Option
            // payload is the previous value (i64 directly for
            // `HashMap<_, i64>`, c-string-cast-to-i64 for
            // `HashMap<_, String>`).
            "Map::pop" | "collections::Map::pop" | "HashMap::pop" | "collections::HashMap::pop"
                if !args.is_empty() =>
            {
                let key_kind = hashmap_key_kind(self.tcx, args[0].ty);
                let sym = if key_kind == VecElemKind::Str {
                    "gos_rt_map_pop_typed_str"
                } else {
                    "gos_rt_map_pop_i64"
                };
                // The Option payload is the map's value type, recovered from
                // the first argument's HashMap (peeling any `&` / `&mut`). A
                // struct-valued pop binds `p: Struct`, so `p.field` lowers to
                // a `Field` projection rather than the dynamic json accessor.
                let mut flat = args[0].ty;
                while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(flat) {
                    flat = *inner;
                }
                let value_ty =
                    if let gossamer_types::TyKind::HashMap { value, .. } = self.tcx.kind_of(flat) {
                        *value
                    } else {
                        self.tcx.int_ty(gossamer_types::IntTy::I64)
                    };
                let substs = gossamer_types::Substs::from_types([value_ty]);
                let opt_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                (sym, opt_ty)
            }
            // `HashMap::get(m, k) -> Option<V>` free-fn form, mirroring the
            // `m.get(k)` method. Without this arm the LLVM lowerer emits a
            // call to an undefined `@HashMap::get` symbol.
            "Map::get" | "collections::Map::get" | "HashMap::get" | "collections::HashMap::get"
                if !args.is_empty() =>
            {
                let key_kind = hashmap_key_kind(self.tcx, args[0].ty);
                let sym = if key_kind == VecElemKind::Str {
                    "gos_rt_map_get_typed_str_opt"
                } else {
                    "gos_rt_map_get_i64_opt"
                };
                let mut flat = args[0].ty;
                while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(flat) {
                    flat = *inner;
                }
                let value_ty =
                    if let gossamer_types::TyKind::HashMap { value, .. } = self.tcx.kind_of(flat) {
                        *value
                    } else {
                        self.tcx.int_ty(gossamer_types::IntTy::I64)
                    };
                let substs = gossamer_types::Substs::from_types([value_ty]);
                let opt_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                (sym, opt_ty)
            }
            _ => return None,
        })
    }

    fn lower_collections_2_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // Qualified Vec mutators use the same checked in-place contract
            // as method calls.
            "Vec::insert" | "collections::Vec::insert" if args.len() == 3 => {
                let unit = self.tcx.unit();
                let error = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit, error]);
                let result = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_vec_insert_safe", result)
            }
            "Vec::remove" | "collections::Vec::remove" if args.len() == 2 => {
                let elem = self.vec_receiver_elem_ty(args[0].ty);
                let error = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([elem, error]);
                let result = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_vec_remove_safe", result)
            }
            "Vec::slice" if args.len() == 3 => {
                // The slice preserves the receiver's element type: a
                // `Vec<String>` slice is `Result<Vec<String>, _>`, so the
                // unwrapped Vec indexes its elements as strings rather than
                // reading the raw String pointer back as an i64.
                let elem = self.vec_receiver_elem_ty(args[0].ty);
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(elem));
                let e = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([v, e]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_vec_slice_result", result_ty)
            }
            "String::slice" if args.len() == 3 => {
                ("gos_rt_str_slice", self.result_string_error_adt_ty())
            }
            _ => return None,
        })
    }

    fn lower_url_runtime_misc_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "url::query_escape" => ("gos_rt_url_query_escape", self.tcx.string_ty()),
            "url::path_escape" => ("gos_rt_url_path_escape", self.tcx.string_ty()),
            "url::query_unescape" => ("gos_rt_url_query_unescape", self.tcx.string_ty()),
            "url::path_unescape" => ("gos_rt_url_path_unescape", self.tcx.string_ty()),
            "runtime::collect_cycles" => ("gos_rt_collect_cycles", self.tcx.unit()),
            "runtime::cycle_collection_supported" => (
                "gos_rt_runtime_cycle_collection_supported",
                self.tcx.bool_ty(),
            ),
            "runtime::scheduler_stats_json" => {
                ("gos_rt_runtime_scheduler_stats_json", self.tcx.string_ty())
            }
            "pprof::goroutine_profile" => ("gos_rt_pprof_goroutine_profile", self.tcx.string_ty()),
            "pprof::mutex_profile" => ("gos_rt_pprof_mutex_profile", self.tcx.string_ty()),
            "pprof::block_profile" => ("gos_rt_pprof_block_profile", self.tcx.string_ty()),
            "pprof::execution_trace" => ("gos_rt_pprof_execution_trace", self.tcx.string_ty()),
            "pprof::cpu_profile" => ("gos_rt_pprof_cpu_profile", self.tcx.string_ty()),
            "pprof::heap_profile" => ("gos_rt_pprof_heap_profile", self.tcx.string_ty()),
            "pprof::route" => ("gos_rt_pprof_route", self.option_string_adt_ty()),
            // Bare `fn(String)` only: the hook is a raw code pointer the
            // runtime calls with the rendered message.
            "runtime::set_panic_hook" => ("gos_rt_set_panic_hook", self.tcx.unit()),
            "runtime::arena_push" => {
                // Locals created after this point (until the matching pop)
                // are region-owned; the drop pass skips their release.
                self.region_depth += 1;
                ("gos_rt_arena_push", self.tcx.unit())
            }
            "runtime::arena_pop" => {
                self.region_depth = self.region_depth.saturating_sub(1);
                ("gos_rt_arena_pop", self.tcx.unit())
            }
            // The three entries a `cohort { }` block desugars to, plus
            // the two a child uses to cooperate with cancellation.
            "runtime::cohort_push" => (
                "gos_rt_cohort_push",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "runtime::cohorts" => {
                let string_ty = self.tcx.string_ty();
                (
                    "gos_rt_cohorts",
                    self.tcx.intern(gossamer_types::TyKind::Vec(string_ty)),
                )
            }
            "runtime::root" => ("gos_rt_cohort_root", self.tcx.string_ty()),
            "runtime::cohort_join" => ("gos_rt_cohort_join", self.result_unit_error_adt_ty()),
            "runtime::cohort_pop" => ("gos_rt_cohort_pop", self.tcx.unit()),
            "lifecycle::ready" => ("gos_rt_lifecycle_ready", self.tcx.unit()),
            "lifecycle::set_ready" => ("gos_rt_lifecycle_set_ready", self.tcx.unit()),
            "lifecycle::is_ready" => ("gos_rt_lifecycle_is_ready", self.tcx.bool_ty()),
            "lifecycle::shutdown" => ("gos_rt_lifecycle_shutdown", self.tcx.unit()),
            "lifecycle::is_shutting_down" => {
                ("gos_rt_lifecycle_is_shutting_down", self.tcx.bool_ty())
            }
            "lifecycle::await_shutdown" => ("gos_rt_lifecycle_await_shutdown", self.tcx.unit()),
            "lifecycle::notify_status" => ("gos_rt_lifecycle_notify_status", self.tcx.unit()),
            "runtime::cohort_cancelled" => ("gos_rt_cohort_cancelled", self.tcx.bool_ty()),
            "runtime::cohort_cancel" => ("gos_rt_cohort_cancel", self.tcx.unit()),
            "testing::check" => ("gos_rt_testing_check", self.tcx.bool_ty()),
            "testing::check_eq" => ("gos_rt_testing_check_eq_i64", self.tcx.bool_ty()),
            "testing::wait_for_scheduler_idle" => {
                ("gos_rt_testing_wait_for_scheduler_idle", self.tcx.bool_ty())
            }
            "testing::check_ok" => {
                // Pass-through identity in compiled mode - assumes
                // happy path.
                ("", self.tcx.int_ty(gossamer_types::IntTy::I64))
            }
            "httptest::server" => ("gos_rt_httptest_server", self.tcx.string_ty()),
            _ => return None,
        })
    }

    fn lower_image_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        Some(match joined {
            "image::new" => ("gos_rt_image_new", i64_ty),
            "image::filled" => ("gos_rt_image_filled", i64_ty),
            "image::decode_base64" => ("gos_rt_image_decode_base64", i64_ty),
            "image::width" => ("gos_rt_image_width", i64_ty),
            "image::height" => ("gos_rt_image_height", i64_ty),
            "image::pixel" => ("gos_rt_image_pixel", i64_ty),
            "image::set_pixel" => ("gos_rt_image_set_pixel", self.tcx.bool_ty()),
            "image::encode_png_base64" => ("gos_rt_image_encode_png_base64", self.tcx.string_ty()),
            "image::encode_jpeg_base64" => {
                ("gos_rt_image_encode_jpeg_base64", self.tcx.string_ty())
            }
            _ => return None,
        })
    }

    fn lower_http_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `http::get(url, headers) -> Result<Response, errors::Error>`.
            // Pin the Ok payload to the sentinel-DefId Response Adt
            // so `r.status` / `r.body` / `r.content_type` /
            // `r.location` projections find the right field index
            // via `stdlib_struct_shapes`.
            "http::get" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_http_get", result_ty)
            }
            // One-shot client verbs sharing `http::get`'s Ok-payload
            // pinning. `head`/`options` take `(url, headers)`;
            // `post`/`put` take `(url, body, content_type)`; `delete`
            // takes `(url, body, headers)`. Each lowers to its
            // per-verb shim so the method string is fixed at the
            // runtime boundary.
            "http::head" | "http::options" | "http::post" | "http::put" | "http::delete" => {
                let result_ty = self.result_response_error_adt_ty();
                let sym = match joined {
                    "http::head" => "gos_rt_http_head",
                    "http::options" => "gos_rt_http_options",
                    "http::post" => "gos_rt_http_post",
                    "http::put" => "gos_rt_http_put",
                    _ => "gos_rt_http_delete",
                };
                (sym, result_ty)
            }
            // Bare `NativeClient` one-shot helpers. `get`/`delete` take
            // just the URL; `post`/`put` take `(url, body, content_type)`
            // (empty content type defaults to application/octet-stream in
            // the shim). Each pins the Response Ok payload like `http::get`.
            "http::native_client::get" | "native_client::get" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_nc_get", result_ty)
            }
            "http::native_client::delete" | "native_client::delete" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_nc_delete", result_ty)
            }
            "http::native_client::post" | "native_client::post" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_nc_post", result_ty)
            }
            "http::native_client::put" | "native_client::put" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_nc_put", result_ty)
            }
            // `proxy::forward(upstream_url, method, body)` one-shot
            // upstream request; `static_files::serve_file(path)` one-shot
            // file read. Both return Result<Response, errors::Error>.
            "http::proxy::forward" | "proxy::forward" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_proxy_forward_url", result_ty)
            }
            "http::static_files::serve_file" | "static_files::serve_file" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_static_serve_file", result_ty)
            }
            // `router::add(router, method, pattern)` registers a
            // handler-less pattern route; `router::lookup(router, method,
            // path) -> Option<i64>` returns the matched route index.
            "http::router::add" | "router::add" => ("gos_rt_router_add_pattern", self.tcx.unit()),
            "http::router::lookup" | "router::lookup" => {
                ("gos_rt_router_lookup", self.option_i64_adt_ty())
            }
            _ => return None,
        })
    }

    fn lower_http_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `http::request(method, url, body, headers)` and
            // `http::request_bytes(method, url, body: [u8], headers)`
            // -> Result<Response, errors::Error>. Same Ok-payload
            // pinning as `http::get`. The String-bodied form lowers
            // to `gos_rt_http_request` (body arrives as a c-string,
            // like `gos_rt_http_stream`); the byte-bodied form lowers
            // to `gos_rt_http_request_bytes` (body arrives as a byte
            // GosVec) so binary upload payloads survive intact.
            "http::request" | "http::request_bytes" => {
                let resp_def = gossamer_resolve::DefId::local(u32::MAX - 5);
                let resp_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: resp_def,
                    substs: gossamer_types::Substs::new(),
                });
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([resp_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                let sym = if joined == "http::request" {
                    "gos_rt_http_request"
                } else {
                    "gos_rt_http_request_bytes"
                };
                (sym, result_ty)
            }
            // `http::stream(method, url, body, headers) -> Result<ResponseStream, errors::Error>`.
            // Pin the Ok payload to the sentinel-DefId
            // ResponseStream Adt so `.__handle` / `.status` /
            // `.content_type` projections find the right field index
            // via `stdlib_struct_shapes`. Without this binding, the
            // call lowered to a non-existent symbol and the
            // destination held an undefined pointer the caller
            // dereferenced as a Result aggregate (askq SSE chat
            // round hung when next_line read garbage).
            "http::stream" => {
                let rs_def = gossamer_resolve::DefId::local(u32::MAX - 4);
                let rs_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: rs_def,
                    substs: gossamer_types::Substs::new(),
                });
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([rs_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_http_stream", result_ty)
            }
            "http::Client::new" => (
                "gos_rt_http_client_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::Client::builder" => (
                "gos_rt_http_client_builder_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::Response::text" => (
                "gos_rt_http_response_text_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::Response::json" => (
                "gos_rt_http_response_json_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // `Response::stream(status, content_type, rs)` - the rs
            // argument is the 3-slot ResponseStream blob pointer
            // (same ptr shape `next_line` receives as receiver).
            "http::Response::stream" | "Response::stream" => (
                "gos_rt_http_response_stream_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::serve" => {
                let ty = self.result_unit_error_adt_ty();
                ("gos_rt_http_serve", ty)
            }
            "http::serve_h2c" => {
                let ty = self.result_unit_error_adt_ty();
                ("gos_rt_http2_bind_and_run_h2c", ty)
            }
            _ => return None,
        })
    }

    fn lower_http_3_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.4.0 HTTP-module bridges (compiled tier free-fn surface).
            // Stateful types (router::new, etc.) are interp-only and not
            // listed here - calling them in compiled mode emits an
            // "unsupported call" diagnostic via the generic fallback.
            "http::chunked::encode" | "chunked::encode" => {
                ("gos_rt_chunked_encode", self.tcx.string_ty())
            }
            "http::chunked::decode" | "chunked::decode" => {
                ("gos_rt_chunked_decode", self.tcx.string_ty())
            }
            "http::sse::encode_event" | "sse::encode_event" => {
                ("gos_rt_sse_encode_event", self.tcx.string_ty())
            }
            "http::sse::encode_comment" | "sse::encode_comment" => {
                ("gos_rt_sse_encode_comment", self.tcx.string_ty())
            }
            "http::sse::encode_retry" | "sse::encode_retry" => {
                ("gos_rt_sse_encode_retry", self.tcx.string_ty())
            }
            "http::middleware::new_request_id" | "middleware::new_request_id" => {
                ("gos_rt_mw_new_request_id", self.tcx.string_ty())
            }
            "http::middleware::accepts_gzip" | "middleware::accepts_gzip" => {
                ("gos_rt_mw_accepts_gzip", self.tcx.bool_ty())
            }
            "http::middleware::decode_basic_auth" | "middleware::decode_basic_auth" => (
                "gos_rt_mw_decode_basic_auth",
                self.option_pair_string_adt_ty(),
            ),
            "http::websocket::accept_key" | "websocket::accept_key" => {
                ("gos_rt_ws_accept_key", self.tcx.string_ty())
            }
            "http::websocket::is_websocket_upgrade" | "websocket::is_websocket_upgrade" => {
                ("gos_rt_ws_is_upgrade", self.tcx.bool_ty())
            }
            "http::websocket::accept" | "websocket::accept" => {
                ("gos_rt_ws_accept", self.result_response_error_adt_ty())
            }
            "http::websocket::connect" | "websocket::connect" => {
                ("gos_rt_ws_serve_connect", self.result_i64_error_adt_ty())
            }
            "http::websocket::send_text" | "websocket::send_text" => {
                ("gos_rt_ws_send_text", self.result_unit_error_adt_ty())
            }
            "http::websocket::send_binary" | "websocket::send_binary" => {
                ("gos_rt_ws_send_binary", self.result_unit_error_adt_ty())
            }
            "http::websocket::recv" | "websocket::recv" => {
                ("gos_rt_ws_recv", self.result_string_error_adt_ty())
            }
            "http::websocket::close" | "websocket::close" => {
                ("gos_rt_ws_close", self.result_unit_error_adt_ty())
            }
            "http::cookie::parse_cookie_header" | "cookie::parse_cookie_header" => {
                ("gos_rt_http_cookie_parse_header", self.string_pair_vec_ty())
            }
            "http::cookie::serialize" | "cookie::serialize" => {
                ("gos_rt_http_cookie_serialize", self.tcx.string_ty())
            }
            "http::csrf::issue_token" | "csrf::issue_token" => (
                "gos_rt_http_csrf_issue_token",
                self.result_string_error_adt_ty(),
            ),
            "http::csrf::verify_token" | "csrf::verify_token" => (
                "gos_rt_http_csrf_verify_token",
                self.result_unit_error_adt_ty(),
            ),
            "http::session::sign" | "session::sign" => {
                ("gos_rt_http_session_sign", self.tcx.string_ty())
            }
            "http::session::verify" | "session::verify" => (
                "gos_rt_http_session_verify",
                self.result_string_error_adt_ty(),
            ),
            "http::static_files::mime_for_path" | "static_files::mime_for_path" => {
                ("gos_rt_static_mime_for_path", self.tcx.string_ty())
            }
            _ => return None,
        })
    }

    fn lower_http_4_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // Stateful constructors. The MIR call-path emits the
            // bare runtime symbol; user code does `Router::new()`
            // → constructor handle. Returns `*mut T` (Ptr) which
            // the caller treats as the receiver of subsequent
            // method calls.
            "http::router::Router::new"
            | "router::Router::new"
            | "Router::new"
            | "http::router::new"
            | "router::new" => (
                "gos_rt_router_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::websocket::ws_frame_text" | "websocket::ws_frame_text" => {
                ("gos_rt_ws_frame_text", self.tcx.string_ty())
            }
            "http::native_client::Client::new"
            | "native_client::Client::new"
            | "NativeClient::new" => (
                "gos_rt_native_client_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::static_files::FileServer::new"
            | "static_files::FileServer::new"
            | "FileServer::new" => (
                "gos_rt_file_server_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::proxy::Proxy::new" | "proxy::Proxy::new" | "Proxy::new" => (
                "gos_rt_proxy_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::ResponseStream::new" | "ResponseStream::new" => (
                "gos_rt_http_response_stream_open",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::Server::new" | "Server::new" => (
                "gos_rt_http_server_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    fn lower_exec_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `exec::run(prog, args) -> Result<Output, errors::Error>`.
            // Pin the Ok payload to the sentinel-DefId Output Adt so
            // `o.stdout` / `o.stderr` / `o.code` projections find the
            // right field index via `stdlib_struct_shapes`. Without
            // this binding, the call lowered to a non-existent
            // user-fn symbol and the destination held an undefined
            // pointer the caller then dereferenced as the Result
            // aggregate (the askq segfault).
            "exec::run" | "os::exec::run" | "process::run" => {
                let output_def = gossamer_resolve::DefId::local(u32::MAX - 3);
                let output_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: output_def,
                    substs: gossamer_types::Substs::new(),
                });
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([output_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_exec_run", result_ty)
            }
            // `process::run_in(prog, args, dir, env)` - the same
            // `Result<Output, errors::Error>` shape as `process::run`,
            // with the child's working directory and environment
            // overrides supplied by the caller.
            "process::run_in" => {
                let output_def = gossamer_resolve::DefId::local(u32::MAX - 3);
                let output_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: output_def,
                    substs: gossamer_types::Substs::new(),
                });
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([output_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_exec_run_in", result_ty)
            }
            // `exec::spawn(prog, args) -> Result<i64, errors::Error>`.
            // Non-blocking process launch - returns the child PID
            // so callers (daemon launchers, long-running tools)
            // don't block the calling goroutine. Pin the Ok
            // payload to `i64` and the Err to `errors::Error` so
            // downstream `?` / `match` shapes find the right field
            // layout.
            "exec::spawn" | "os::exec::spawn" | "process::spawn" => {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([i64_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_exec_spawn", result_ty)
            }
            // `process::spawn_piped(prog, args) -> Result<Child, errors::Error>`.
            // The Ok payload is the opaque piped-child handle (the
            // `Child` sentinel Adt), so method dispatch on the
            // extracted binding routes through the `process::Child`
            // runtime kind.
            "exec::spawn_piped" | "os::exec::spawn_piped" | "process::spawn_piped" => {
                let child_def = gossamer_resolve::DefId::local(u32::MAX - 8);
                self.tcx.register_def_name(child_def, "Child");
                let child_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: child_def,
                    substs: gossamer_types::Substs::new(),
                });
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([child_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_exec_spawn_piped", result_ty)
            }
            // `exec::kill(pid) -> bool` - best-effort SIGTERM.
            "exec::kill" | "os::exec::kill" | "process::kill" => {
                ("gos_rt_exec_kill", self.tcx.bool_ty())
            }
            // `exec::signal(pid, signum) -> bool`.
            "exec::signal" | "os::exec::signal" | "process::signal" => {
                ("gos_rt_exec_signal", self.tcx.bool_ty())
            }
            // `exec::kill_group(pid) -> bool` - kills the entire
            // process group on Unix; best-effort on Windows.
            "exec::kill_group" | "os::exec::kill_group" | "process::kill_group" => {
                ("gos_rt_exec_kill_group", self.tcx.bool_ty())
            }
            // `exec::wait_timeout(pid, ms) -> i64`. Returns the
            // child's exit code on success, -1 on timeout, -2 on
            // error (unknown pid, permission denied).
            "exec::wait_timeout" | "os::exec::wait_timeout" | "process::wait_timeout" => (
                "gos_rt_exec_wait_timeout",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // `exec::pipeline_run(cmds: Vec<String>) -> Result<Output, errors::Error>`.
            // Same Ok-shape sentinel-DefId as `exec::run` so the
            // existing `Output { stdout, stderr, code }` field
            // projection lowers identically.
            "exec::pipeline_run" | "os::exec::pipeline_run" | "process::pipeline_run" => {
                let output_def = gossamer_resolve::DefId::local(u32::MAX - 3);
                let output_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: output_def,
                    substs: gossamer_types::Substs::new(),
                });
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([output_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_exec_pipeline_run", result_ty)
            }
            _ => return None,
        })
    }

    fn lower_signal_flag_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `signal::on(sig_raw) -> i64` - registers a notifier.
            "signal::on" | "os::signal::on" => (
                "gos_rt_signal_on",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // `Notifier::wait(handle)` - blocks until signal fires.
            "signal_wait" | "Notifier::wait" | "signal::wait" | "os::signal::wait" => {
                ("gos_rt_signal_wait", self.tcx.unit())
            }
            // `Notifier::try_wait(handle) -> bool`.
            "signal_try_wait"
            | "Notifier::try_wait"
            | "signal::try_wait"
            | "os::signal::try_wait" => ("gos_rt_signal_try_wait", self.tcx.bool_ty()),
            "flag::Set::new" => ("gos_rt_flag_set_new", self.flag_set_ty()),
            _ => return None,
        })
    }

    /// The class of C-ABI return a Gossamer type is carried in, for the
    /// three distinctions that decide how a caller reads the value: a
    /// two-word carrier, a float register, and nothing at all. The
    /// integer-versus-pointer distinction is not one of them - both are
    /// one word and a Gossamer type does not choose between them.
    fn abi_return_class(&self, ty: gossamer_types::Ty) -> &'static str {
        use gossamer_types::TyKind;
        if self.is_result_or_option_adt(ty) {
            return "carrier";
        }
        match self.tcx.kind_of(ty) {
            TyKind::Unit => "void",
            TyKind::Float(_) => "float",
            _ => "word",
        }
    }

    /// The same class, read off the ABI registry's declared return type.
    fn registry_return_class(rt_name: &str) -> Option<&'static str> {
        use gossamer_abi::AbiType;
        gossamer_abi::lookup(rt_name).map(|entry| match entry.sig.ret {
            AbiType::Void => "void",
            AbiType::F64 => "float",
            AbiType::I128 => "carrier",
            _ => "word",
        })
    }

    pub(crate) fn emit_stdlib_free_call(
        &mut self,
        rt_name: &str,
        ret_ty: gossamer_types::Ty,
        args: &[HirExpr],
        span: Span,
    ) -> Option<Local> {
        // The type this lowering gives the call's destination and the
        // signature the backends emit the call with come from two
        // different places, and a disagreement between them is a
        // wrong-ABI call with no diagnostic: a carrier read as one word
        // loses its payload, a float read as an integer is nonsense.
        // Both compiled tiers build their signature from the registry,
        // so the registry is what the destination type is checked
        // against.
        debug_assert!(
            Self::registry_return_class(rt_name)
                .is_none_or(|declared| declared == self.abi_return_class(ret_ty)),
            "{rt_name}: this lowering gives the call a {} result, but the ABI \
             registry declares it returns a {} - one of the two is wrong",
            self.abi_return_class(ret_ty),
            Self::registry_return_class(rt_name).unwrap_or("?"),
        );
        if rt_name == "gos_rt_fmt_pad"
            && args.len() == 4
            && let HirExprKind::Call {
                callee,
                args: rendered_args,
            } = &args[0].kind
            && let HirExprKind::Path { segments, .. } = &callee.kind
            && segments.len() == 1
            && segments[0].name.as_str() == "__concat"
            && rendered_args.len() == 1
            && matches!(
                self.tcx.kind_of(rendered_args[0].ty),
                gossamer_types::TyKind::Int(_)
            )
        {
            let mut locals = Vec::with_capacity(4);
            locals.push(self.lower_expr(&rendered_args[0])?);
            for arg in &args[1..] {
                locals.push(self.lower_expr(arg)?);
            }
            let dest = self.fresh(ret_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_fmt_pad_i64".to_string())),
                args: locals
                    .into_iter()
                    .map(|local| Operand::Copy(Place::local(local)))
                    .collect(),
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return Some(dest);
        }
        if rt_name.is_empty() {
            // Identity passthrough for testing::check_ok and friends.
            let v = args.first().and_then(|a| self.lower_expr(a))?;
            let dest = self.fresh(ret_ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::Use(Operand::Copy(Place::local(v))),
                span,
            );
            return Some(dest);
        }
        let coerce_str_arg = Self::stdlib_arg_needs_byte_coercion(rt_name);
        let coerce_char_needle = Self::stdlib_str_needle_fn(rt_name);
        let mut arg_locals = Vec::with_capacity(args.len());
        for arg in args {
            let local = self.lower_expr(arg)?;
            let local = self.coerce_stdlib_arg(local, coerce_str_arg, span);
            // A `char` needle to a string fn (`strings::contains(s, 'x')`)
            // must be promoted to a one-char String, mirroring the method
            // form - the `gos_rt_str_*` helpers dereference their needle as a
            // c-string, so a raw `char` int would be read as a pointer.
            let local = if coerce_char_needle
                && matches!(
                    self.tcx.kind_of(self.locals[local.0 as usize].ty),
                    gossamer_types::TyKind::Char
                ) {
                self.coerce_char_arg_to_str(local, span)
            } else {
                local
            };
            arg_locals.push(local);
        }
        self.apply_pad_default(rt_name, &mut arg_locals, span);
        let ret_ty = self.adjust_stdlib_ret_ty(rt_name, ret_ty);
        let dest = self.fresh(ret_ty);
        if let Some(rk) = Self::stdlib_runtime_kind(rt_name) {
            self.local_runtime_kind.insert(dest, rk);
        }
        if rt_name == "gos_rt_fs_list_dir" {
            self.local_elem_struct.insert(dest, "DirInfo".to_string());
        }
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(rt_name.to_string())),
            args: arg_locals
                .into_iter()
                .map(|l| Operand::Copy(Place::local(l)))
                .collect(),
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// Free-form string functions whose needle/pattern argument is a `&str`
    /// and so accepts a `char` (promoted to a one-char String), matching the
    /// method form's `coerce_char_needle`.
    fn stdlib_str_needle_fn(rt_name: &str) -> bool {
        matches!(
            rt_name,
            "gos_rt_str_contains"
                | "gos_rt_str_contains_any"
                | "gos_rt_str_starts_with"
                | "gos_rt_str_ends_with"
                | "gos_rt_str_find_opt"
                | "gos_rt_str_rfind_opt"
                | "gos_rt_str_split"
                | "gos_rt_str_splitn"
                | "gos_rt_str_split_once"
                | "gos_rt_str_rsplit_once"
                | "gos_rt_str_replace"
                | "gos_rt_str_replacen"
                | "gos_rt_str_count"
        )
    }

    fn stdlib_arg_needs_byte_coercion(rt_name: &str) -> bool {
        matches!(
            rt_name,
            "gos_rt_encoding_base64_encode"
                | "gos_rt_encoding_hex_encode"
                | "gos_rt_encoding_base32_encode"
                | "gos_rt_encoding_base32_encode_hex"
                | "gos_rt_encoding_ascii85_encode"
                | "gos_rt_crypto_sha256_digest"
                | "gos_rt_crypto_sha512_digest"
                | "gos_rt_crypto_blake3_digest"
                | "gos_rt_crypto_hmac_sha256_mac"
                | "gos_rt_crypto_md5"
                | "gos_rt_crypto_sha1"
                | "gos_rt_compress_flate_compress"
                | "gos_rt_compress_zlib_compress"
                | "gos_rt_compress_gzip_encode"
                | "gos_rt_compress_zstd_encode"
                | "gos_rt_compress_zstd_encode_level"
                | "gos_rt_compress_bzip2_compress"
                | "gos_rt_crypto_pbkdf2_sha256"
                | "gos_rt_crypto_scrypt_interactive"
                | "gos_rt_crypto_argon2id_hash"
                | "gos_rt_crypto_aes256gcm_seal"
                | "gos_rt_crypto_aes256gcm_open"
                | "gos_rt_crypto_chacha20poly1305_seal"
                | "gos_rt_crypto_chacha20poly1305_open"
                | "gos_rt_crypto_ed25519_sign"
                | "gos_rt_crypto_ed25519_verify"
        )
    }

    fn coerce_stdlib_arg(&mut self, local: Local, coerce_str_arg: bool, span: Span) -> Local {
        let lt = self.locals[local.0 as usize].ty;
        if let gossamer_types::TyKind::Array { elem, len } = self.tcx.kind_of(lt).clone() {
            self.coerce_array_to_vec(local, elem, len, span)
        } else if coerce_str_arg && matches!(self.tcx.kind_of(lt), gossamer_types::TyKind::String) {
            let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
            let bytes_ty = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
            let dest = self.fresh(bytes_ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_str_as_bytes".to_string())),
                args: vec![Operand::Copy(Place::local(local))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            dest
        } else {
            local
        }
    }

    fn apply_pad_default(&mut self, rt_name: &str, arg_locals: &mut Vec<Local>, span: Span) {
        // `strings::pad_left/pad_right` carry the pad glyph as a String
        // (e.g. `"*"`) and default to a single space when the 3rd arg
        // is omitted; the shim's pad parameter is an `i64` codepoint.
        // Inject the default for the 2-arg form and fold a String pad
        // arg to its first codepoint. A `char` pad arg already lowers
        // to its codepoint, so it is left untouched.
        if matches!(rt_name, "gos_rt_str_pad_left" | "gos_rt_str_pad_right") {
            if arg_locals.len() < 3 {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let pad = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(pad),
                    Rvalue::Use(Operand::Const(ConstValue::Int(32))),
                    span,
                );
                arg_locals.push(pad);
            } else {
                let pad_ty = self.tcx.kind_of(self.locals[arg_locals[2].0 as usize].ty);
                let pad_ty = if let gossamer_types::TyKind::Ref { inner, .. } = pad_ty {
                    self.tcx.kind_of(*inner)
                } else {
                    pad_ty
                };
                if matches!(pad_ty, gossamer_types::TyKind::String) {
                    let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                    let cp = self.fresh(i64_ty);
                    let next = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_str_first_codepoint".to_string(),
                        )),
                        args: vec![Operand::Copy(Place::local(arg_locals[2]))],
                        destination: Place::local(cp),
                        target: Some(next),
                    });
                    self.set_current(next);
                    arg_locals[2] = cp;
                }
            }
        }
    }

    fn adjust_stdlib_ret_ty(
        &mut self,
        rt_name: &str,
        ret_ty: gossamer_types::Ty,
    ) -> gossamer_types::Ty {
        if rt_name == "gos_rt_http_request_send" {
            // Pin the Ok payload to the sentinel Response Adt so
            // field projections resolve, matching `http::get`.
            self.result_response_error_adt_ty()
        } else if gossamer_abi::lookup(rt_name).map(|e| e.sig.ret)
            == Some(gossamer_abi::AbiType::I128)
        {
            self.result_repr_ty(ret_ty)
        } else {
            ret_ty
        }
    }

    fn stdlib_runtime_kind(rt_name: &str) -> Option<&'static str> {
        match rt_name {
            "gos_rt_flag_set_new" => Some("flag::Set"),
            "gos_rt_signal_on" => Some("signal::Notifier"),
            "gos_rt_bufio_scanner_new" => Some("bufio::Scanner"),
            "gos_rt_http_client_new" => Some("http::Client"),
            "gos_rt_http_client_builder_new" => Some("http::ClientBuilder"),
            "gos_rt_http_client_get"
            | "gos_rt_http_client_post"
            | "gos_rt_http_client_put"
            | "gos_rt_http_client_options"
            | "gos_rt_http_client_delete"
            | "gos_rt_http_client_head" => Some("http::Request"),
            "gos_rt_http_response_text_new"
            | "gos_rt_http_response_json_new"
            | "gos_rt_http_response_stream_new" => Some("http::Response"),
            "gos_rt_error_new" | "gos_rt_error_wrap" | "gos_rt_errors_join_vec" => {
                Some("errors::Error")
            }
            "gos_rt_regex_compile" | "gos_rt_regex_compile_result" => Some("regex::Pattern"),
            "gos_rt_set_new" => Some("collections::HashSet"),
            "gos_rt_btree_set_new" => Some("collections::BTreeSet"),
            "gos_rt_deque_new" | "gos_rt_deque_from_vec_i64" => Some("collections::VecDeque"),
            "gos_rt_queue_new" | "gos_rt_queue_from_vec_i64" => Some("collections::VecQueue"),
            "gos_rt_stack_new" | "gos_rt_stack_from_vec_i64" => Some("collections::VecStack"),
            "gos_rt_bheap_max_new_i64" | "gos_rt_bheap_max_from_vec_i64" => {
                Some("collections::MaxHeap")
            }
            "gos_rt_bheap_min_new_i64" | "gos_rt_bheap_min_from_vec_i64" => {
                Some("collections::MinHeap")
            }
            "gos_rt_sync_map_new" => Some("sync::Map"),
            "gos_rt_math_rng_new" => Some("math::rand::Rng"),
            "gos_rt_field_error_new" => Some("validate::FieldError"),
            "gos_rt_validate_errors_new" => Some("validate::Errors"),
            "gos_rt_once_new" => Some("sync::Once"),
            "gos_rt_rwlock_new" => Some("sync::RwLock"),
            "gos_rt_shared_new" => Some("sync::Shared"),
            "gos_rt_atomic_bool_new" => Some("sync::AtomicBool"),
            "gos_rt_ctx_background" | "gos_rt_ctx_with_cancel" | "gos_rt_ctx_with_timeout" => {
                Some("context::Context")
            }
            "gos_rt_metrics_counter_new" => Some("metrics::Counter"),
            "gos_rt_metrics_gauge_new" => Some("metrics::Gauge"),
            "gos_rt_metrics_histogram_new" => Some("metrics::Histogram"),
            "gos_rt_metrics_registry_new" => Some("metrics::Registry"),
            "gos_rt_trace_tracer_new" => Some("trace::Tracer"),
            "gos_rt_bytes_builder_new" | "gos_rt_bytes_builder_with_capacity" => {
                Some("bytes::Builder")
            }
            "gos_rt_bytes_buffer_new" | "gos_rt_bytes_buffer_with_capacity" => {
                Some("bytes::Buffer")
            }
            "gos_rt_tcp_listener_bind" => Some("net::TcpListener"),
            "gos_rt_tcp_stream_connect" => Some("net::TcpStream"),
            "gos_rt_io_stdin" | "gos_rt_io_stdout" | "gos_rt_io_stderr" => Some("io::Stream"),
            "gos_rt_fs_file_open" | "gos_rt_fs_file_create" => Some("fs::File"),
            "gos_rt_fs_temp_file" => Some("fs::temp_file_pair"),
            "gos_rt_fs_open_options_new" => Some("fs::OpenOptions"),
            "gos_rt_unix_listener_bind" => Some("net::UnixListener"),
            "gos_rt_unix_stream_connect" => Some("net::UnixStream"),
            "gos_rt_udp_bind" => Some("net::UdpSocket"),
            // 0.4.0 stateful HTTP types.
            "gos_rt_router_new" => Some("http::Router"),
            "gos_rt_http_server_new" => Some("http::Server"),
            "gos_rt_http_response_stream_open" => Some("http::ResponseStream"),
            "gos_rt_file_server_new" => Some("http::FileServer"),
            "gos_rt_native_client_new" => Some("http::NativeClient"),
            "gos_rt_proxy_new" => Some("http::Proxy"),
            _ => None,
        }
    }
}

/// `(kind, arity, config_is_numeric)` for a `middleware::<name>` wrapper,
/// or `None` when the name is not a middleware. `arity` counts the inner
/// handler plus the optional configuration argument. The kind numbering
/// is ABI with `gossamer_runtime::c_abi::http_middleware::middleware_kind`.
fn middleware_kind_of(name: &str) -> Option<(i64, usize, bool)> {
    Some(match name {
        "request_id" => (1, 1, false),
        "cors" => (2, 2, false),
        "security_headers" => (3, 2, false),
        "etag" => (4, 1, false),
        "rate_limit" => (5, 2, false),
        "hsts" => (6, 2, false),
        "cache_control" => (7, 2, false),
        "body_limit" => (8, 2, true),
        "compress_gzip" => (9, 1, false),
        "logger" => (10, 1, false),
        "recoverer" => (11, 1, false),
        "timeout" => (12, 2, true),
        "basic_auth" => (13, 2, false),
        "bearer_auth" => (14, 2, false),
        "safe_defaults" => (15, 1, false),
        _ => return None,
    })
}
