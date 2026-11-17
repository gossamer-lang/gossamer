//! `json::encode(value)` of a value whose type reaches a user struct or enum
//! writes through a generated render walk, and so do `yaml::encode` and
//! `toml::encode` of any value other than a `json::Value`.
//!
//! The walk writes what `json::encode::<T>` writes: a struct's members in
//! declaration order under their serde keys (by key under `sort: true`),
//! `Option` and `Result` as the value they hold, every float with its
//! fraction, and the shapes only `json::encode` takes (a map keyed by other
//! than strings, a `Result`). A type that holds itself is written by one
//! function per type rather than unrolled at the call. Every tier runs the
//! same generated walk, so they write the same text.

use gossamer_types::{Ty, TyKind};

use super::{Emit, Gen, Mode, WRITER, is_plain_place};
use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind, HirLiteral, HirStmtKind,
    HirUnaryOp, for_each_child_expr_mut,
};

/// What a type's values reach, for the choice of walk.
#[derive(Default)]
struct Reach {
    /// A user struct or enum, or a map keyed by other than strings, which the
    /// tree writes inside a struct only through it.
    needs_walk: bool,
    /// A shape the generated walk writes differently from the tree: a
    /// generic type, a library type, or a map keyed by other than strings.
    unsupported: bool,
}

impl Gen<'_> {
    /// Rewrites every `json::encode` / `render` / `encode_pretty` call under
    /// `item` whose value needs the generated walk.
    pub(super) fn render_json_calls(&mut self, item: &mut HirItem) {
        match &mut item.kind {
            HirItemKind::Fn(f) => self.render_in_fn(f),
            HirItemKind::Impl(imp) => {
                for m in &mut imp.methods {
                    self.render_in_fn(m);
                }
            }
            HirItemKind::Trait(t) => {
                for m in &mut t.methods {
                    self.render_in_fn(m);
                }
            }
            HirItemKind::Const(c) => self.render_in_expr(&mut c.value),
            HirItemKind::Static(s) => self.render_in_expr(&mut s.value),
            HirItemKind::Adt(_) => {}
        }
    }

    fn render_in_fn(&mut self, f: &mut HirFn) {
        if let Some(body) = &mut f.body {
            self.render_in_block(&mut body.block);
        }
    }

    fn render_in_block(&mut self, block: &mut HirBlock) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init, .. } => {
                    if let Some(e) = init {
                        self.render_in_expr(e);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                    self.render_in_expr(expr);
                }
                HirStmtKind::Item(item) => self.render_json_calls(item),
            }
        }
        if let Some(tail) = &mut block.tail {
            self.render_in_expr(tail);
        }
    }

    fn render_in_expr(&mut self, expr: &mut HirExpr) {
        if let HirExprKind::Block(block) = &mut expr.kind {
            self.render_in_block(block);
        } else {
            for_each_child_expr_mut(expr, &mut |child| self.render_in_expr(child));
        }
        if let Some(rendered) = self.rendered_call(expr) {
            *expr = rendered;
        }
    }

    /// The generated walk standing in for `expr`, when `expr` is a JSON
    /// rendering call whose value needs it, or a YAML or TOML encode of a
    /// value other than a `json::Value`.
    fn rendered_call(&mut self, expr: &HirExpr) -> Option<HirExpr> {
        let HirExprKind::Call { callee, args } = &expr.kind else {
            return None;
        };
        // A path the resolver bound to an item names the program's own
        // function, whatever its module is called.
        let HirExprKind::Path {
            segments,
            def: None,
        } = &callee.kind
        else {
            return None;
        };
        let names: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
        let (last, module) = names.split_last()?;
        let format = match module {
            [format] | ["encoding", format] | ["std", "encoding", format] => *format,
            _ => return None,
        };
        if *last == "encode" && matches!(format, "yaml" | "toml") {
            return self.format_encode(expr, args, format == "toml");
        }
        if format != "json" || !matches!(*last, "encode" | "encode_pretty" | "to_value") {
            return None;
        }
        if *last == "to_value" {
            // The value `json::parse` reads from the text `encode` writes.
            let mut encode = expr.clone();
            if let HirExprKind::Call { callee, .. } = &mut encode.kind
                && let HirExprKind::Path { segments, .. } = &mut callee.kind
                && let Some(name) = segments.last_mut()
            {
                name.name = "encode".to_string();
            }
            let text = self.rendered_text(&encode, args, "wr_finish", Mode::default())?;
            let mut parse_path: Vec<String> = module.iter().map(|s| (*s).to_string()).collect();
            parse_path.push("parse".to_string());
            return Some(self.call_segments(&parse_path, vec![text], expr.ty, expr.span));
        }
        let finish = if *last == "encode_pretty" {
            "wr_finish_pretty"
        } else {
            "wr_finish"
        };
        let text = self.rendered_text(expr, args, finish, Mode::default())?;
        // Every encode answers a `Result`: writing a value with a JSON form
        // cannot fail, so the text is always the `Ok`.
        Some(self.ctor("Ok", vec![text], expr.ty, expr.span))
    }

    /// `yaml::encode(value)` / `toml::encode(value)` of a value other than a
    /// `json::Value`: the JSON text `json::encode` writes, converted to the
    /// format with the members in the order written. TOML has no null, so
    /// its walk leaves out an `Option` member that is `None`.
    fn format_encode(&mut self, expr: &HirExpr, args: &[HirExpr], toml: bool) -> Option<HirExpr> {
        let value = args.first()?;
        if matches!(self.tcx.kind_of(self.peel(value.ty)), TyKind::JsonValue) {
            return None;
        }
        let finish = if toml {
            "wr_finish_toml"
        } else {
            "wr_finish_yaml"
        };
        let mode = Mode {
            sorted: false,
            omit_none: toml,
        };
        let span = expr.span;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let unit = self.tcx.unit();
        // A walk finishes in the format's own step; text the tree writes
        // goes through a writer to the same step.
        if let Some(walk) = self.rendered_walk(args, finish, expr.ty, mode, span) {
            return Some(walk);
        }
        let string_ty = self.tcx.string_ty();
        let text = self.call_path(&["json", "encode"], args.to_vec(), string_ty, span);
        let mut out = Emit::default();
        let writer = self.step("wr_new", Vec::new(), i64_ty, span);
        out.stmt(self.let_stmt(WRITER, false, i64_ty, writer, span));
        let w = self.local(WRITER, i64_ty, span);
        let raw = self.step("wr_raw", vec![w, text], unit, span);
        out.stmt(self.expr_stmt(raw, span));
        let w = self.local(WRITER, i64_ty, span);
        let finished = self.step(finish, vec![w], expr.ty, span);
        let stmts = self.finish(out, span);
        Some(self.block(stmts, Some(finished), expr.ty, span))
    }

    /// The text the call `expr` writes: the generated walk for a value of a
    /// user type, finished by the step `finish`, or the call itself as the
    /// text the tree renders.
    fn rendered_text(
        &mut self,
        expr: &HirExpr,
        args: &[HirExpr],
        finish: &str,
        mode: Mode,
    ) -> Option<HirExpr> {
        let string_ty = self.tcx.string_ty();
        if let Some(walk) = self.rendered_walk(args, finish, string_ty, mode, expr.span) {
            return Some(walk);
        }
        args.first()?;
        let mut call = expr.clone();
        call.ty = string_ty;
        Some(call)
    }

    /// The generated walk writing the value `args` opens with, answering what
    /// the step `finish` (of type `finished_ty`) answers. `None` for a value
    /// the tree writes as the walk would, or one the walk has no form for.
    fn rendered_walk(
        &mut self,
        args: &[HirExpr],
        finish: &str,
        finished_ty: Ty,
        mode: Mode,
        span: gossamer_lex::Span,
    ) -> Option<HirExpr> {
        let (first, rest) = args.split_first()?;
        let mut value = first;
        while let HirExprKind::Unary {
            op: HirUnaryOp::RefShared | HirUnaryOp::RefMut,
            operand,
        } = &value.kind
        {
            value = operand;
        }
        let ty = value.ty;
        // A map keyed by other than strings is the one shape the tree writes
        // whole, so a call naming one directly keeps its own rendering.
        if let TyKind::HashMap { key, .. } = self.tcx.kind_of(self.peel(ty))
            && !matches!(self.tcx.kind_of(self.peel(*key)), TyKind::String)
        {
            return None;
        }
        let mut reach = Reach::default();
        self.render_reach(ty, &mut Vec::new(), &mut reach);
        if reach.unsupported || !reach.needs_walk {
            return None;
        }
        // The walk only reads the value, so a place it names is walked where
        // it lies; any other value is computed once into a binding of its own.
        let mut stmts = Vec::new();
        let held = if is_plain_place(value) {
            value.clone()
        } else {
            let name = self.fresh("rendered");
            stmts.push(self.let_stmt(&name, false, ty, value.clone(), span));
            self.local(&name, ty, span)
        };
        let sorted = Mode {
            sorted: true,
            ..mode
        };
        // `sort:` is the call's second argument when it is written.
        let walk = match rest.first().map(|e| &e.kind) {
            None | Some(HirExprKind::Literal(HirLiteral::Bool(false))) => {
                self.render_walk(held, ty, mode, finish, finished_ty, span)
            }
            Some(HirExprKind::Literal(HirLiteral::Bool(true))) => {
                self.render_walk(held, ty, sorted, finish, finished_ty, span)
            }
            Some(_) => {
                let flag = self.fresh("sort");
                let bool_ty = self.tcx.bool_ty();
                let sort = rest[0].clone();
                stmts.push(self.let_stmt(&flag, false, bool_ty, sort, span));
                let by_key = self.render_walk(held.clone(), ty, sorted, finish, finished_ty, span);
                let declared = self.render_walk(held, ty, mode, finish, finished_ty, span);
                let cond = self.local(&flag, bool_ty, span);
                self.if_else(cond, by_key, declared, finished_ty, span)
            }
        };
        Some(self.block(stmts, Some(walk), finished_ty, span))
    }

    /// The walk writing `held_value` of type `ty` in `mode`, answering what
    /// the step `finish` answers.
    fn render_walk(
        &mut self,
        held_value: HirExpr,
        ty: Ty,
        mode: Mode,
        finish: &str,
        finished_ty: Ty,
        span: gossamer_lex::Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let mut out = Emit::default();
        let writer = self.step("wr_new", Vec::new(), i64_ty, span);
        out.stmt(self.let_stmt(WRITER, false, i64_ty, writer, span));
        let outer = std::mem::replace(&mut self.rendering, true);
        let outer_mode = std::mem::replace(&mut self.mode, mode);
        self.encode(&mut out, held_value, ty, span);
        self.rendering = outer;
        self.mode = outer_mode;
        let w = self.local(WRITER, i64_ty, span);
        let finished = self.step(finish, vec![w], finished_ty, span);
        let stmts = self.finish(out, span);
        self.block(stmts, Some(finished), finished_ty, span)
    }

    /// Records in `reach` what a value of `ty` reaches. `on_walk` holds the
    /// user types being walked, so a type met again is one that holds itself.
    fn render_reach(&mut self, ty: Ty, on_walk: &mut Vec<Ty>, reach: &mut Reach) {
        let peeled = self.peel(ty);
        match self.tcx.kind_of(peeled).clone() {
            TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Bool
            | TyKind::Char
            | TyKind::String
            | TyKind::JsonValue
            | TyKind::Unit
            | TyKind::Never => {}
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => {
                self.render_reach(elem, on_walk, reach);
            }
            TyKind::Tuple(elems) => {
                for elem in elems {
                    self.render_reach(elem, on_walk, reach);
                }
            }
            TyKind::HashMap { key, value, .. } => {
                if matches!(self.tcx.kind_of(self.peel(key)), TyKind::String) {
                    self.render_reach(value, on_walk, reach);
                    return;
                }
                // Such a map is written whole by `json::encode`, which takes
                // its values as the tree does only while none needs the walk.
                let mut values = Reach::default();
                self.render_reach(value, on_walk, &mut values);
                reach.needs_walk = true;
                reach.unsupported |= values.unsupported || values.needs_walk;
            }
            TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                for payload in substs.types() {
                    self.render_reach(payload, on_walk, reach);
                }
            }
            TyKind::Adt { def, .. } if self.adts.contains_key(&def) => {
                if on_walk.contains(&peeled) {
                    reach.needs_walk = true;
                    return;
                }
                on_walk.push(peeled);
                if let Some(variants) = self.variant_fields_of(def, peeled) {
                    reach.needs_walk = true;
                    for payload in variants.iter().flatten() {
                        self.render_reach(*payload, on_walk, reach);
                    }
                } else if let Some(fields) = self.struct_fields_of(def, peeled) {
                    // The tree orders members by key and knows no serde
                    // attributes, so a struct is always written by the walk.
                    reach.needs_walk = true;
                    for field in fields {
                        self.render_reach(field, on_walk, reach);
                    }
                }
                on_walk.pop();
            }
            TyKind::Adt { .. } | TyKind::Var(_) | TyKind::Param { .. } | TyKind::Error => {
                reach.unsupported = true;
            }
            _ => {}
        }
    }

    /// Writes the shapes only a rendering takes - a `char`, a fixed array, a
    /// `Result` - and answers whether `ty` was one of them.
    pub(super) fn encode_rendered_only(
        &mut self,
        out: &mut Emit,
        value: &HirExpr,
        ty: Ty,
        span: gossamer_lex::Span,
    ) -> bool {
        let peeled = self.peel(ty);
        let unit = self.tcx.unit();
        match self.tcx.kind_of(peeled).clone() {
            TyKind::Char => {
                let string_ty = self.tcx.string_ty();
                let text = self.method(value.clone(), "to_string", Vec::new(), string_ty, span);
                self.write_step(out, "wr_str", text, span);
                true
            }
            TyKind::Array { elem, .. } => {
                self.encode_seq(out, value.clone(), ty, elem, span);
                true
            }
            TyKind::HashMap { key, .. }
                if !matches!(self.tcx.kind_of(self.peel(key)), TyKind::String) =>
            {
                let string_ty = self.tcx.string_ty();
                let text =
                    self.call_path(&["json", "encode"], vec![value.clone()], string_ty, span);
                self.write_step(out, "wr_raw", text, span);
                true
            }
            TyKind::Adt { def, substs } if def.local == u32::MAX => {
                let types = substs.types();
                let (Some(&ok_ty), Some(&err_ty)) = (types.first(), types.get(1)) else {
                    return false;
                };
                let ok = self.fresh("ok");
                let mut on_ok = Emit::default();
                let ok_value = self.local(&ok, ok_ty, span);
                self.encode(&mut on_ok, ok_value, ok_ty, span);
                let ok_stmts = self.finish(on_ok, span);
                let ok_body = self.block(ok_stmts, None, unit, span);
                let err = self.fresh("err");
                let mut on_err = Emit::default();
                let err_value = self.local(&err, err_ty, span);
                self.encode(&mut on_err, err_value, err_ty, span);
                let err_stmts = self.finish(on_err, span);
                let err_body = self.block(err_stmts, None, unit, span);
                let matched = self.match_result(
                    value.clone(),
                    &ok,
                    ok_ty,
                    ok_body,
                    &err,
                    err_ty,
                    err_body,
                    unit,
                    span,
                );
                out.stmt(self.expr_stmt(matched, span));
                true
            }
            _ => false,
        }
    }
}
