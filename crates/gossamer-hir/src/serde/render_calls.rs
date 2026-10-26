//! `json::encode(value)` of a value whose type reaches a user enum, or holds
//! itself, writes through a generated render walk.
//!
//! The walk writes what the `json::Value` tree of the value would: a struct's
//! members in byte order of their keys, `Option` and `Result` as the value
//! they hold, every float with its fraction. A field takes its `#[rename]`d
//! key and a `#[skip]`ped one is left out, as `json::decode` reads them. A user enum is externally tagged
//! there, as `json::decode` reads it back, and a type that holds itself is
//! written by one function per type rather than unrolled at the call. Every
//! tier runs the same generated walk, so they write the same text.

use gossamer_resolve::DefId;
use gossamer_types::{Ty, TyKind};

use super::{Emit, Gen, WRITER};
use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind, HirStmtKind, HirUnaryOp,
    for_each_child_expr_mut,
};

/// What a type's values reach, for the choice of walk.
#[derive(Default)]
struct Reach {
    /// A user enum, a user type that holds itself, or a map keyed by other
    /// than strings, which the tree writes inside a struct only through it.
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
    /// rendering call whose value needs it.
    fn rendered_call(&mut self, expr: &HirExpr) -> Option<HirExpr> {
        let HirExprKind::Call { callee, args } = &expr.kind else {
            return None;
        };
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return None;
        };
        let names: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
        let (last, module) = names.split_last()?;
        if !matches!(
            module,
            ["json"] | ["encoding", "json"] | ["std", "encoding", "json"]
        ) || !matches!(*last, "encode" | "render" | "encode_pretty")
        {
            return None;
        }
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
        let span = expr.span;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let string_ty = self.tcx.string_ty();
        let mut out = Emit::default();
        let writer = self.step("wr_new", Vec::new(), i64_ty, span);
        out.stmt(self.let_stmt(WRITER, false, i64_ty, writer, span));
        let held = self.fresh("rendered");
        out.stmt(self.let_stmt(&held, false, ty, value.clone(), span));
        let held_value = self.local(&held, ty, span);
        let outer = std::mem::replace(&mut self.rendering, true);
        self.encode(&mut out, held_value, ty, span);
        self.rendering = outer;
        let w = self.local(WRITER, i64_ty, span);
        let finished = self.step("wr_finish", vec![w], string_ty, span);
        let text = self.fresh("text");
        out.stmt(self.let_stmt(&text, false, string_ty, finished, span));
        let stmts = self.finish(out, span);
        let tail = if *last == "encode_pretty" {
            // The indented form lays out the tree the compact text reads as.
            let module: Vec<String> = module.iter().map(|s| (*s).to_string()).collect();
            let parse_path: Vec<String> = module.iter().cloned().chain(["parse".into()]).collect();
            let pretty_path: Vec<String> = module
                .iter()
                .cloned()
                .chain(["encode_pretty".into()])
                .collect();
            let json_ty = self.tcx.json_value_ty();
            let error_ty = self.tcx.dyn_error_ty();
            let parsed_ty = self.result_ty(json_ty, error_ty);
            let compact = self.local(&text, string_ty, span);
            let parsed = self.call_segments(&parse_path, vec![compact], parsed_ty, span);
            let tree = self.fresh("tree");
            let tree_value = self.local(&tree, json_ty, span);
            let mut pretty_args = vec![tree_value];
            pretty_args.extend(rest.iter().cloned());
            let pretty = self.call_segments(&pretty_path, pretty_args, string_ty, span);
            let error = self.fresh("err");
            let fallback = self.local(&text, string_ty, span);
            self.match_result(
                parsed, &tree, json_ty, pretty, &error, error_ty, fallback, string_ty, span,
            )
        } else {
            self.local(&text, string_ty, span)
        };
        Some(self.block(stmts, Some(tail), string_ty, span))
    }

    /// Records in `reach` what a value of `ty` reaches. `on_walk` holds the
    /// user types being walked, so a type met again is one that holds itself.
    fn render_reach(&self, ty: Ty, on_walk: &mut Vec<DefId>, reach: &mut Reach) {
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
            TyKind::Adt { def, substs } if self.adts.contains_key(&def) => {
                if !substs.is_empty() {
                    reach.unsupported = true;
                    return;
                }
                if on_walk.contains(&def) {
                    reach.needs_walk = true;
                    return;
                }
                on_walk.push(def);
                if let Some(variants) = self.tcx.enum_variant_tys(def) {
                    reach.needs_walk = true;
                    for payload in variants.iter().flatten() {
                        self.render_reach(*payload, on_walk, reach);
                    }
                } else if let Some(fields) = self.tcx.struct_field_tys(def) {
                    // The tree writes every field under its own name, so a
                    // `#[rename]`d or `#[skip]`ped field needs the walk.
                    if self.adts[&def]
                        .serde
                        .iter()
                        .any(|field| field.key.is_some() || field.skip)
                    {
                        reach.needs_walk = true;
                    }
                    for field in fields {
                        self.render_reach(*field, on_walk, reach);
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
