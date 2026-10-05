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

mod crypto;
mod http;
mod math;
mod sql;
mod sys;
mod text;

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
            TyKind::Int(int) => super::int_to_str_symbol(*int),
            TyKind::Float(gossamer_types::FloatTy::F32) => "gos_rt_f32_to_str",
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
        if runtime_kind == "collections::BTreeSet" {
            self.order_set_by_element(set, self.peel_ref_ty(elem_ty), span);
        }
        Some(set)
    }

    /// The first type argument of an `Adt`, which for a collection is its
    /// element or key type.
    fn adt_first_type_argument(&self, ty: gossamer_types::Ty) -> Option<gossamer_types::Ty> {
        match self.tcx.kind(ty)? {
            gossamer_types::TyKind::Adt { substs, .. } => substs.types().first().copied(),
            _ => None,
        }
    }

    /// Seats a `BTreeSet`'s elements by the type's own `cmp` when it writes
    /// one, through the comparator the program compiled for that type. An
    /// element type the language orders needs no call: the constructor
    /// already answers a set in that order.
    fn order_set_by_element(&mut self, set: Local, elem: gossamer_types::Ty, span: Span) {
        let Some(name) = self.user_comparator_name(elem) else {
            return;
        };
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let address = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(address),
            Rvalue::CallIntrinsic {
                name: "gos_fn_addr",
                args: vec![Operand::Const(ConstValue::Str(name))],
            },
            span,
        );
        // An aggregate crosses the comparator's boundary by the address of
        // its slots, a node or a scalar by the word that names it.
        let by_address = self.tcx.is_flat_inline_aggregate(elem);
        let unit_ty = self.tcx.unit();
        let dest = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: "gos_rt_set_ordered_by",
                args: vec![
                    Operand::Copy(Place::local(set)),
                    Operand::Copy(Place::local(address)),
                    Operand::Const(ConstValue::Int(i128::from(by_address))),
                ],
            },
            span,
        );
    }

    /// The comparator the program declares for `ty`, `None` for a type the
    /// language orders on its own.
    fn user_comparator_name(&self, ty: gossamer_types::Ty) -> Option<String> {
        let gossamer_types::TyKind::Adt { def, .. } = self.tcx.kind(ty)? else {
            return None;
        };
        let registered = self.tcx.def_name(*def)?;
        let name = format!(
            "{}{}",
            gossamer_ast::USER_COMPARATOR_PREFIX,
            registered.replace("::", "__")
        );
        self.fn_ret_names.contains_key(&name).then_some(name)
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
        if runtime_kind == "collections::BTreeSet"
            && let Some(first) = items.first()
        {
            let elem = self.peel_ref_ty(first.ty);
            self.order_set_by_element(set, elem, span);
        }
        let bool_ty = self.tcx.bool_ty();
        for item in items {
            let mut value = self.lower_expr(item)?;
            value = self.auto_deref_cell(value, item.span);
            let value_ty = self.peel_ref_ty(item.ty);
            let aggregate_desc = self
                .is_aggregate_key(value_ty)
                .then(|| self.key_descriptor(value_ty))
                .flatten();
            // A user enum's value is a counted node, keyed by the same
            // discriminant-and-payload bytes `s.insert(v)` uses.
            let enum_desc = if aggregate_desc.is_none() && self.struct_name_of(value_ty).is_none() {
                self.ensure_enum_eq_desc(value_ty)
            } else {
                None
            };
            let rt = if aggregate_desc.is_some() {
                "gos_rt_set_insert_skey"
            } else if enum_desc.is_some() {
                "gos_rt_set_insert_ekey"
            } else if matches!(map_key_kind_from(self.tcx, value_ty), MapKeyKind::I64) {
                "gos_rt_set_insert_i64"
            } else {
                "gos_rt_set_insert"
            };
            let mut call_args = vec![
                Operand::Copy(Place::local(set)),
                Operand::Copy(Place::local(value)),
            ];
            if let Some(desc) = aggregate_desc.or(enum_desc) {
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
        // A struct, tuple, or array key content-hashes through its slot
        // descriptor; a user enum key hashes by discriminant and payload, as
        // `m.insert(k, v)` keys one.
        let (insert, descriptor) = if self.is_aggregate_key(key_ty) {
            ("gos_rt_map_insert_skey_opt", self.key_descriptor(key_ty)?)
        } else if self.struct_name_of(key_ty).is_none()
            && let Some(desc) = self.ensure_enum_eq_desc(key_ty)
        {
            let _ = self.ensure_aggr_struct_meta(val_ty);
            if self.is_inline_aggregate_ty(val_ty) {
                let _ = self.ensure_aggr_copy_meta(val_ty);
            }
            ("gos_rt_map_insert_ekey_opt", desc)
        } else {
            return None;
        };
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
                insert,
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
            if self.is_inline_aggregate_ty(value_ty) || self.map_value_is_carrier(map_ty) {
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
        result_ty: gossamer_types::Ty,
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
            if let Some(elem) = self.adt_first_type_argument(result_ty) {
                self.order_set_by_element(set, elem, span);
            }
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
        // Scalars and Strings order by their slot word and keep the word
        // entry points; the stream is for every element that does not.
        let (count, tags) = self.tuple_element_stream(elem)?;
        let sequence_local = self.lower_expr(sequence)?;
        let mut call_args = vec![Operand::Copy(Place::local(sequence_local))];
        if joined != "sort::sort_stable" {
            let target = args.get(1)?;
            let target_local = self.lower_expr(target)?;
            let slots = self.ordered_value_slots(target_local, elem, span);
            call_args.push(Operand::Copy(Place::local(slots)));
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
        if !callee_def_some && args.len() == 2 && joined == "__gos_fs_walk_dir_raw" {
            return ControlFlow::Break(self.try_lower_walk_dir(args, span));
        }
        // A parallel adapter's chunk runner: the leaf closure must reach the
        // runtime as an environment it can call from every worker, which the
        // generic stdlib-call path does not shape.
        if !callee_def_some && args.len() == 3 && joined == "__gos_par_run" {
            return ControlFlow::Break(self.try_lower_par_run(args, span));
        }
        // `par_chunks_mut`'s runner: the callback reaches the runtime as an
        // environment every worker calls with a chunk's window.
        if !callee_def_some && args.len() == 3 && joined == "__gos_par_chunks" {
            return ControlFlow::Break(self.try_lower_par_chunks(args, span));
        }
        // The overlap check a call passing several windows of one sequence
        // runs first; every argument is an integer.
        if !callee_def_some && args.len() == 7 && joined == "__gos_windows_disjoint" {
            return ControlFlow::Break(self.lower_windows_disjoint(args, span));
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
            // A `regex::compile` literal was validated while parsing with the
            // engine the shim compiles it with, so the handle is the answer.
            "regex::compile" => (
                "gos_rt_regex_compile",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // A pattern built at run time can be refused: the Err arm carries
            // the engine's reason on every tier. `regex::Pattern::new(p)` is
            // the type-qualified spelling of the same call.
            "regex::new" | "regex::Pattern::new" | "Pattern::new" => {
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

    fn lower_time_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `time::Duration` is a count of nanoseconds and `time::Instant`
            // a monotonic reading in nanoseconds, both an `i64` at run time.
            "time::Duration::from_nanos" => ("gos_rt_duration_from_nanos", self.tcx.duration_ty()),
            "time::Duration::from_micros" => {
                ("gos_rt_duration_from_micros", self.tcx.duration_ty())
            }
            "time::Duration::from_millis" => {
                ("gos_rt_duration_from_millis", self.tcx.duration_ty())
            }
            "time::Duration::from_secs" => ("gos_rt_duration_from_secs", self.tcx.duration_ty()),
            "time::Duration::from_secs_f64" => {
                ("gos_rt_duration_from_secs_f64", self.tcx.duration_ty())
            }
            "time::Duration::as_nanos" => (
                "gos_rt_duration_as_nanos",
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
            "time::Duration::as_secs_f64" => (
                "gos_rt_duration_as_secs_f64",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
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
            "__gos_fd_wait_raw" => ("gos_rt_fd_wait_raw", self.result_i64_error_adt_ty()),
            "ffi::last_errno" | "std::ffi::last_errno" => (
                "gos_rt_ffi_last_errno",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "ffi::last_os_error" | "std::ffi::last_os_error" => (
                "gos_rt_ffi_last_os_error",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
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
            // A wait given as a `Duration` counts nanoseconds; an integer
            // counts milliseconds.
            "time::sleep" if self.arg_is_duration(args.first()) => {
                ("gos_rt_sleep_ns", self.tcx.unit())
            }
            "time::sleep" => ("gos_rt_sleep_ms", self.tcx.unit()),
            "time::sleep_ctx" if self.arg_is_duration(args.get(1)) => {
                ("gos_rt_sleep_ns_ctx", self.tcx.bool_ty())
            }
            "time::sleep_ctx" => ("gos_rt_sleep_ms_ctx", self.tcx.bool_ty()),
            // `time::__sleep_ns` / `time::__sleep_ns_ctx` are the internal
            // shims the interpreter's lowering folds `sleep` / `sleep_ctx`
            // into when the wait is a `Duration`; the compiled tier folds the
            // same shape into the arms above, but the names are bound
            // free-function exports, so a direct call needs its own arm.
            "time::__sleep_ns" => ("gos_rt_sleep_ns", self.tcx.unit()),
            "time::__sleep_ns_ctx" => ("gos_rt_sleep_ns_ctx", self.tcx.bool_ty()),
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
            "time::Instant::now" => ("gos_rt_instant_now", self.tcx.instant_ty()),
            "time::Instant::elapsed_ms" => (
                "gos_rt_instant_elapsed_ms",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "time::Instant::elapsed" => ("gos_rt_instant_elapsed", self.tcx.duration_ty()),
            "time::Instant::duration_since" => {
                ("gos_rt_instant_duration_since", self.tcx.duration_ty())
            }
            _ => return None,
        })
    }

    /// Whether a call argument is a `time::Duration`.
    fn arg_is_duration(&self, arg: Option<&HirExpr>) -> bool {
        arg.is_some_and(|arg| matches!(self.tcx.kind_of(arg.ty), gossamer_types::TyKind::Duration))
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
                // A struct, tuple, or array element reaches the runtime as the
                // address of its slot block, as in the method form.
                let elem = self.vec_receiver_elem_ty(args[0].ty);
                let symbol = if self.is_inline_slot_block(elem) {
                    "gos_rt_vec_insert_slots_safe"
                } else {
                    "gos_rt_vec_insert_safe"
                };
                (symbol, result)
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
            "runtime::at_exit" => ("gos_rt_at_exit", self.tcx.unit()),
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
            "image::from_rgba_bytes" => ("gos_rt_image_from_rgba_bytes", i64_ty),
            "image::to_rgba_bytes" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_image_to_rgba_bytes",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "image::encode_png_base64" => ("gos_rt_image_encode_png_base64", self.tcx.string_ty()),
            "image::encode_jpeg_base64" => {
                ("gos_rt_image_encode_jpeg_base64", self.tcx.string_ty())
            }
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
            // The helper renders an `i64`; a `u64` / `usize` above
            // `i64::MAX` would print negative, so it keeps the general path,
            // which renders it unsigned.
            && matches!(
                self.tcx.kind_of(rendered_args[0].ty),
                gossamer_types::TyKind::Int(int)
                    if !matches!(int, gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize)
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
