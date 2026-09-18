//! Lowers the parallel collection adapters onto one chunk runner.
//!
//! `xs.par_map(f)` and its family become a call to [`PAR_RUN`], which runs a
//! leaf closure over index windows of the source on every worker and answers
//! the leaves' `Vec`s concatenated in index order. Each leaf is a plain index
//! loop over its window with the callback spliced in, so every element shape
//! reaches the backends through the same reads, pushes, and comparisons a
//! hand-written loop uses, on all three tiers.
//!
//! A reduction's leaf answers a one-element `Vec` holding the leaf's value,
//! and the caller combines those values in index order. The runtime cuts a
//! reduction into fixed-width leaves, so the combining tree is the same shape
//! whatever the machine's worker count.

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_types::{FloatTy, FnSig, IntTy, Ty, TyCtxt, TyKind};

use crate::fuse::inline_safe;
use crate::ids::HirIdGenerator;
use crate::tree::{
    HirArrayExpr, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirLiteral, HirParam, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind,
};

/// The runtime entry every adapter lowers to: `__gos_par_run(len, mode, leaf)`.
pub const PAR_RUN: &str = "__gos_par_run";

/// Mode code for an elementwise adapter, whose leaf width may follow the pool.
const MODE_ELEMENTWISE: i64 = 0;
/// Mode code for a reduction, whose leaves are fixed-width.
const MODE_REDUCTION: i64 = 1;

/// Rewrites every parallel adapter call in `program` onto [`PAR_RUN`].
pub(crate) fn desugar_parallel_adapters(
    program: &mut HirProgram,
    tcx: &mut TyCtxt,
    ids: &mut HirIdGenerator,
) {
    let mut pass = Desugarer { tcx, ids, temp: 0 };
    for item in &mut program.items {
        pass.visit_item(item);
    }
}

/// What an adapter call does with its elements.
enum Adapter {
    Map(HirExpr),
    Filter(HirExpr),
    Reduce { init: HirExpr, combine: HirExpr },
    Sum,
    Extreme { max: bool, by: Option<HirExpr> },
}

/// How a leaf reads the element at an index.
enum Source {
    /// `base[i]` over a sequence binding.
    Indexed { base: String, base_ty: Ty },
    /// `start + i` over an integer range.
    Counted { start: String },
}

/// A callback as the leaves apply it.
enum Callback {
    /// A closure literal's parameters and body, spliced in at each use.
    Inline {
        params: Vec<HirParam>,
        body: HirExpr,
    },
    /// A callable value called at each use.
    Call(HirExpr),
}

struct Desugarer<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    temp: u32,
}

impl Desugarer<'_> {
    fn visit_item(&mut self, item: &mut HirItem) {
        match &mut item.kind {
            HirItemKind::Fn(f) => self.visit_fn(f),
            HirItemKind::Impl(imp) => {
                for m in &mut imp.methods {
                    self.visit_fn(m);
                }
            }
            HirItemKind::Trait(t) => {
                for m in &mut t.methods {
                    self.visit_fn(m);
                }
            }
            HirItemKind::Const(c) => self.visit_expr(&mut c.value),
            HirItemKind::Static(s) => self.visit_expr(&mut s.value),
            HirItemKind::Adt(_) => {}
        }
    }

    fn visit_fn(&mut self, f: &mut HirFn) {
        if let Some(body) = &mut f.body {
            self.visit_block(&mut body.block);
        }
    }

    fn visit_block(&mut self, block: &mut HirBlock) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init, .. } => {
                    if let Some(e) = init {
                        self.visit_expr(e);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => self.visit_expr(expr),
                HirStmtKind::Item(item) => self.visit_item(item),
            }
        }
        if let Some(tail) = &mut block.tail {
            self.visit_expr(tail);
        }
    }

    /// Post-order, so an adapter call inside a callback is lowered before the
    /// callback is spliced into its caller's leaves.
    fn visit_expr(&mut self, expr: &mut HirExpr) {
        self.visit_children(expr);
        if let Some(lowered) = self.lower_adapter(expr) {
            *expr = lowered;
        }
    }

    // One arm per HIR expression variant; the length is the variant count.
    // Splitting it would scatter the exhaustive-visitor structure that keeps
    // every child edge visible in one place.
    #[allow(clippy::too_many_lines)]
    fn visit_children(&mut self, expr: &mut HirExpr) {
        match &mut expr.kind {
            HirExprKind::Literal(_)
            | HirExprKind::Path { .. }
            | HirExprKind::LiftedClosure { .. }
            | HirExprKind::Continue { .. }
            | HirExprKind::Placeholder => {}
            HirExprKind::Call { callee, args } => {
                self.visit_expr(callee);
                for a in args {
                    self.visit_expr(a);
                }
            }
            HirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver);
                for a in args {
                    self.visit_expr(a);
                }
            }
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.visit_expr(receiver);
            }
            HirExprKind::Index { base, index } => {
                self.visit_expr(base);
                self.visit_expr(index);
            }
            HirExprKind::Unary { operand, .. } => self.visit_expr(operand),
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.visit_expr(lhs);
                self.visit_expr(rhs);
            }
            HirExprKind::Assign { place, value } => {
                self.visit_expr(place);
                self.visit_expr(value);
            }
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition);
                self.visit_expr(then_branch);
                if let Some(e) = else_branch {
                    self.visit_expr(e);
                }
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &mut arm.guard {
                        self.visit_expr(g);
                    }
                    self.visit_expr(&mut arm.body);
                }
            }
            HirExprKind::Loop { body, .. } => self.visit_expr(body),
            HirExprKind::While {
                condition, body, ..
            } => {
                self.visit_expr(condition);
                self.visit_expr(body);
            }
            HirExprKind::Block(block) => self.visit_block(block),
            HirExprKind::Closure { body, .. } => self.visit_expr(body),
            HirExprKind::Select { arms } => {
                for arm in arms {
                    self.visit_expr(&mut arm.body);
                }
            }
            HirExprKind::Return(v) => {
                if let Some(e) = v {
                    self.visit_expr(e);
                }
            }
            HirExprKind::Break { value, .. } => {
                if let Some(e) = value {
                    self.visit_expr(e);
                }
            }
            HirExprKind::Tuple(items) => {
                for e in items {
                    self.visit_expr(e);
                }
            }
            HirExprKind::Array(arr) => match arr {
                HirArrayExpr::List(items) => {
                    for e in items {
                        self.visit_expr(e);
                    }
                }
                HirArrayExpr::Repeat { value, count } => {
                    self.visit_expr(value);
                    self.visit_expr(count);
                }
            },
            HirExprKind::Cast { value, .. } => self.visit_expr(value),
            HirExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.visit_expr(s);
                }
                if let Some(e) = end {
                    self.visit_expr(e);
                }
            }
        }
    }

    // ----- recognition -----

    fn lower_adapter(&mut self, expr: &HirExpr) -> Option<HirExpr> {
        let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        let adapter = match (name.name.as_str(), args.as_slice()) {
            ("par_map", [f]) => Adapter::Map(f.clone()),
            ("par_filter", [f]) => Adapter::Filter(f.clone()),
            ("par_reduce", [init, combine]) => Adapter::Reduce {
                init: init.clone(),
                combine: combine.clone(),
            },
            ("par_sum", []) => Adapter::Sum,
            ("par_min", []) => Adapter::Extreme {
                max: false,
                by: None,
            },
            ("par_max", []) => Adapter::Extreme {
                max: true,
                by: None,
            },
            ("par_min_by", [cmp]) => Adapter::Extreme {
                max: false,
                by: Some(cmp.clone()),
            },
            ("par_max_by", [cmp]) => Adapter::Extreme {
                max: true,
                by: Some(cmp.clone()),
            },
            _ => return None,
        };
        let elem_ty = self.source_elem_ty(receiver.ty)?;
        Some(self.build(receiver, elem_ty, adapter, expr.ty, expr.span))
    }

    /// The element type a sequence or integer range receiver walks. A range
    /// reaches the HIR typed as the iterator it is; the checker admits no
    /// other iterator as an adapter's receiver.
    fn source_elem_ty(&self, ty: Ty) -> Option<Ty> {
        let mut peeled = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        match self.tcx.kind_of(peeled) {
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => Some(*elem),
            TyKind::Range(elem) | TyKind::Iterator(elem)
                if matches!(self.tcx.kind_of(*elem), TyKind::Int(_)) =>
            {
                Some(*elem)
            }
            _ => None,
        }
    }

    // ----- construction -----

    fn build(
        &mut self,
        receiver: &HirExpr,
        elem_ty: Ty,
        adapter: Adapter,
        result_ty: Ty,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let mut stmts = Vec::new();
        let (source, len) = self.bind_source(receiver, elem_ty, &mut stmts, span);
        let len_name = self.temp_name("len");
        let bind = self.let_stmt(&len_name, false, i64_ty, len, span);
        stmts.push(bind);
        let tail = match adapter {
            Adapter::Map(f) => {
                let callback = self.callback(f, &mut stmts, span);
                let leaf = self.map_leaf(&source, elem_ty, &callback, result_ty, span);
                self.run_call(&len_name, MODE_ELEMENTWISE, leaf, result_ty, span)
            }
            Adapter::Filter(f) => {
                let callback = self.callback(f, &mut stmts, span);
                let leaf = self.filter_leaf(&source, elem_ty, &callback, result_ty, span);
                self.run_call(&len_name, MODE_ELEMENTWISE, leaf, result_ty, span)
            }
            Adapter::Reduce { init, combine } => {
                let init_name = self.temp_name("init");
                let bind = self.let_stmt(&init_name, false, elem_ty, init, span);
                stmts.push(bind);
                let callback = self.callback(combine, &mut stmts, span);
                self.reduce(&source, elem_ty, &callback, &len_name, &init_name, span)
            }
            Adapter::Sum => self.sum(&source, elem_ty, &len_name, span),
            Adapter::Extreme { max, by } => {
                let by = by.map(|cmp| self.callback(cmp, &mut stmts, span));
                self.extreme(
                    &source,
                    elem_ty,
                    max,
                    by.as_ref(),
                    &len_name,
                    result_ty,
                    span,
                )
            }
        };
        self.block(stmts, Some(tail), result_ty, span)
    }

    /// Binds what the leaves read, and answers the element count.
    fn bind_source(
        &mut self,
        receiver: &HirExpr,
        elem_ty: Ty,
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) -> (Source, HirExpr) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        if let HirExprKind::Range {
            start: Some(start),
            end: Some(end),
            inclusive,
        } = &receiver.kind
        {
            let start_name = self.temp_name("start");
            let end_name = self.temp_name("end");
            let start_value = self.widen(start.as_ref().clone(), i64_ty, span);
            let end_value = self.widen(end.as_ref().clone(), i64_ty, span);
            let s = self.let_stmt(&start_name, false, i64_ty, start_value, span);
            stmts.push(s);
            let e = self.let_stmt(&end_name, false, i64_ty, end_value, span);
            stmts.push(e);
            // The count a range walks: `end - start` (one more when the end is
            // included), and nothing when the range is empty.
            let bool_ty = self.tcx.bool_ty();
            let end_read = self.path(&end_name, i64_ty, span);
            let start_read = self.path(&start_name, i64_ty, span);
            let span_len = self.binary(HirBinaryOp::Sub, end_read, start_read, i64_ty, span);
            let span_len = if *inclusive {
                let one = self.int_lit(1, i64_ty, span);
                self.binary(HirBinaryOp::Add, span_len, one, i64_ty, span)
            } else {
                span_len
            };
            let zero = self.int_lit(0, i64_ty, span);
            let end_read = self.path(&end_name, i64_ty, span);
            let start_read = self.path(&start_name, i64_ty, span);
            let nonempty_op = if *inclusive {
                HirBinaryOp::Ge
            } else {
                HirBinaryOp::Gt
            };
            let nonempty = self.binary(nonempty_op, end_read, start_read, bool_ty, span);
            let len = self.if_else(nonempty, span_len, zero, i64_ty, span);
            return (Source::Counted { start: start_name }, len);
        }
        let (base, base_ty) = if matches!(
            self.tcx.kind_of(receiver.ty),
            TyKind::Range(_) | TyKind::Iterator(_)
        ) {
            // A range value rather than a literal: its bounds are not in
            // reach, so the leaves read the elements it collects to.
            let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
            let collected = self.expr(
                vec_ty,
                span,
                HirExprKind::MethodCall {
                    receiver: Box::new(receiver.clone()),
                    name: Ident::new("collect"),
                    args: Vec::new(),
                    owner: None,
                },
            );
            (collected, vec_ty)
        } else {
            (receiver.clone(), receiver.ty)
        };
        let name = if let Some(name) = simple_binding(&base) {
            name
        } else {
            let name = self.temp_name("src");
            let bind = self.let_stmt(&name, false, base_ty, base, span);
            stmts.push(bind);
            name
        };
        let read = self.path(&name, base_ty, span);
        let len = self.method0(read, "len", i64_ty, span);
        (
            Source::Indexed {
                base: name,
                base_ty,
            },
            len,
        )
    }

    /// Prepares `f` for application in the leaves: a closure literal whose
    /// body can be spliced is spliced; anything else is bound once and called.
    fn callback(&mut self, f: HirExpr, stmts: &mut Vec<HirStmt>, span: Span) -> Callback {
        if let HirExprKind::Closure { params, body, .. } = &f.kind
            && params.iter().all(|p| simple_param(&p.pattern))
            && inline_safe(body, 0)
        {
            return Callback::Inline {
                params: params.clone(),
                body: (**body).clone(),
            };
        }
        if let HirExprKind::Path { .. } = &f.kind {
            // A named function called directly is typed as the function it
            // is; the callable-value form is what a call through an
            // environment reads.
            let mut f = f;
            if let TyKind::FnTrait(sig) = self.tcx.kind_of(f.ty).clone() {
                f.ty = self.tcx.intern(TyKind::FnPtr(sig));
            }
            return Callback::Call(f);
        }
        let name = self.temp_name("f");
        let ty = f.ty;
        let bind = self.let_stmt(&name, false, ty, f, span);
        stmts.push(bind);
        let callee = self.path(&name, ty, span);
        Callback::Call(callee)
    }

    /// `callback(args..)`, typed `ty`.
    fn apply(&mut self, callback: &Callback, args: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        match callback {
            Callback::Inline { params, body } => {
                let body = body.clone();
                let stmts: Vec<HirStmt> = params
                    .iter()
                    .zip(args)
                    .map(|(param, arg)| self.param_let(param, arg, span))
                    .collect();
                self.block(stmts, Some(body), ty, span)
            }
            Callback::Call(callee) => {
                let callee = self.reclone(callee);
                self.expr(
                    ty,
                    span,
                    HirExprKind::Call {
                        callee: Box::new(callee),
                        args,
                    },
                )
            }
        }
    }

    /// The element at `index`.
    fn elem(&mut self, source: &Source, elem_ty: Ty, index: HirExpr, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        match source {
            Source::Indexed { base, base_ty } => {
                let base = self.path(base, *base_ty, span);
                self.expr(
                    elem_ty,
                    span,
                    HirExprKind::Index {
                        base: Box::new(base),
                        index: Box::new(index),
                    },
                )
            }
            Source::Counted { start } => {
                let start = self.path(start, i64_ty, span);
                let value = self.binary(HirBinaryOp::Add, start, index, i64_ty, span);
                if elem_ty == i64_ty {
                    value
                } else {
                    self.expr(
                        elem_ty,
                        span,
                        HirExprKind::Cast {
                            value: Box::new(value),
                            ty: elem_ty,
                        },
                    )
                }
            }
        }
    }

    /// `|lo, hi| -> ret { stmts; tail }`.
    fn leaf_closure(&mut self, lo: &str, hi: &str, body: HirExpr, ret: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let params = vec![self.param(lo, i64_ty, span), self.param(hi, i64_ty, span)];
        let ty = self.tcx.intern(TyKind::FnPtr(FnSig {
            inputs: vec![i64_ty, i64_ty],
            output: ret,
        }));
        self.expr(
            ty,
            span,
            HirExprKind::Closure {
                params,
                ret: Some(ret),
                body: Box::new(body),
            },
        )
    }

    /// `__gos_par_run(len, mode, leaf)`, typed as the leaf's result.
    fn run_call(&mut self, len: &str, mode: i64, leaf: HirExpr, ty: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let callee_ty = self.tcx.intern(TyKind::FnPtr(FnSig {
            inputs: vec![i64_ty, i64_ty, leaf.ty],
            output: ty,
        }));
        let callee = self.path(PAR_RUN, callee_ty, span);
        let len = self.path(len, i64_ty, span);
        let mode = self.int_lit(mode, i64_ty, span);
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![len, mode, leaf],
            },
        )
    }

    /// A counted walk over `[from, hi)`: `let mut i = from; while i < hi {
    /// body(i); i += 1 }`, answering the statements.
    fn window_loop(
        &mut self,
        from: HirExpr,
        hi: &str,
        body: impl FnOnce(&mut Self, HirExpr) -> Vec<HirStmt>,
        span: Span,
    ) -> Vec<HirStmt> {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let unit_ty = self.tcx.unit();
        let index = self.temp_name("i");
        let init = self.let_stmt(&index, true, i64_ty, from, span);
        let read = self.path(&index, i64_ty, span);
        let mut body_stmts = body(self, read);
        let current = self.path(&index, i64_ty, span);
        let one = self.int_lit(1, i64_ty, span);
        let next = self.binary(HirBinaryOp::Add, current, one, i64_ty, span);
        let place = self.path(&index, i64_ty, span);
        body_stmts.push(self.assign_stmt(place, next, span));
        let body_block = self.block(body_stmts, None, unit_ty, span);
        let at = self.path(&index, i64_ty, span);
        let end = self.path(hi, i64_ty, span);
        let condition = self.binary(HirBinaryOp::Lt, at, end, bool_ty, span);
        let walk = self.expr(
            unit_ty,
            span,
            HirExprKind::While {
                condition: Box::new(condition),
                body: Box::new(body_block),
                label: None,
            },
        );
        vec![init, self.expr_stmt(walk, span)]
    }

    /// A leaf that pushes each mapped element onto its own `Vec`.
    fn map_leaf(
        &mut self,
        source: &Source,
        elem_ty: Ty,
        callback: &Callback,
        out_ty: Ty,
        span: Span,
    ) -> HirExpr {
        let out_elem = match self.tcx.kind_of(out_ty) {
            TyKind::Vec(elem) => *elem,
            _ => elem_ty,
        };
        let (lo, hi) = (self.temp_name("lo"), self.temp_name("hi"));
        let out = self.temp_name("out");
        // A map answers one element per input, so the leaf's output is sized
        // once rather than grown.
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let hi_read = self.path(&hi, i64_ty, span);
        let lo_read = self.path(&lo, i64_ty, span);
        let count = self.binary(HirBinaryOp::Sub, hi_read, lo_read, i64_ty, span);
        let callee = self.expr(
            out_ty,
            span,
            HirExprKind::Path {
                segments: vec![Ident::new("Vec"), Ident::new("with_capacity")],
                def: None,
            },
        );
        let sized = self.expr(
            out_ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![count],
            },
        );
        let mut stmts = vec![self.let_stmt(&out, true, out_ty, sized, span)];
        let from = self.path(&lo, i64_ty, span);
        stmts.extend(self.window_loop(
            from,
            &hi,
            |this, index| {
                let value = this.elem(source, elem_ty, index, span);
                let mapped = this.apply(callback, vec![value], out_elem, span);
                vec![this.push_stmt(&out, out_ty, mapped, span)]
            },
            span,
        ));
        let tail = self.path(&out, out_ty, span);
        let body = self.block(stmts, Some(tail), out_ty, span);
        self.leaf_closure(&lo, &hi, body, out_ty, span)
    }

    /// A leaf that pushes each element its predicate keeps.
    fn filter_leaf(
        &mut self,
        source: &Source,
        elem_ty: Ty,
        callback: &Callback,
        out_ty: Ty,
        span: Span,
    ) -> HirExpr {
        let bool_ty = self.tcx.bool_ty();
        let (lo, hi) = (self.temp_name("lo"), self.temp_name("hi"));
        let out = self.temp_name("out");
        let mut stmts = vec![self.empty_vec(&out, out_ty, span)];
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let from = self.path(&lo, i64_ty, span);
        stmts.extend(self.window_loop(
            from,
            &hi,
            |this, index| {
                let value = this.elem(source, elem_ty, index, span);
                let elem = this.temp_name("e");
                let bind = this.let_stmt(&elem, false, elem_ty, value, span);
                let arg = this.path(&elem, elem_ty, span);
                let keep = this.apply(callback, vec![arg], bool_ty, span);
                let kept = this.path(&elem, elem_ty, span);
                let push = this.push_stmt(&out, out_ty, kept, span);
                let push = this.if_stmt(keep, vec![push], span);
                vec![bind, push]
            },
            span,
        ));
        let tail = self.path(&out, out_ty, span);
        let body = self.block(stmts, Some(tail), out_ty, span);
        self.leaf_closure(&lo, &hi, body, out_ty, span)
    }

    /// A leaf folding its non-empty window with `step`, starting from its
    /// first element, and answering `#[acc]`; an empty window answers `#[]`.
    fn fold_leaf(
        &mut self,
        source: &Source,
        elem_ty: Ty,
        step: &mut dyn FnMut(&mut Self, &str, HirExpr) -> Vec<HirStmt>,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let (lo, hi) = (self.temp_name("lo"), self.temp_name("hi"));
        let acc = self.temp_name("acc");
        let first_index = self.path(&lo, i64_ty, span);
        let first = self.elem(source, elem_ty, first_index, span);
        let mut stmts = vec![self.let_stmt(&acc, true, elem_ty, first, span)];
        let lo_read = self.path(&lo, i64_ty, span);
        let one = self.int_lit(1, i64_ty, span);
        let from = self.binary(HirBinaryOp::Add, lo_read, one, i64_ty, span);
        stmts.extend(self.window_loop(
            from,
            &hi,
            |this, index| {
                let value = this.elem(source, elem_ty, index, span);
                step(this, &acc, value)
            },
            span,
        ));
        let acc_read = self.path(&acc, elem_ty, span);
        let one_elem = self.vec_of(vec![acc_read], vec_ty, span);
        let nonempty_body = self.block(stmts, Some(one_elem), vec_ty, span);
        let empty = self.vec_of(Vec::new(), vec_ty, span);
        let hi_read = self.path(&hi, i64_ty, span);
        let lo_read = self.path(&lo, i64_ty, span);
        let nonempty = self.binary(HirBinaryOp::Gt, hi_read, lo_read, bool_ty, span);
        let body = self.if_else(nonempty, nonempty_body, empty, vec_ty, span);
        self.leaf_closure(&lo, &hi, body, vec_ty, span)
    }

    /// Folds `values` (a `Vec<T>` binding) in index order with `step`,
    /// answering the value, or `empty` when it holds nothing.
    fn fold_values(
        &mut self,
        values: &str,
        elem_ty: Ty,
        step: &mut dyn FnMut(&mut Self, &str, HirExpr) -> Vec<HirStmt>,
        empty: HirExpr,
        result: impl FnOnce(&mut Self, HirExpr) -> HirExpr,
        span: Span,
    ) -> HirExpr {
        let result_ty = empty.ty;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let source = Source::Indexed {
            base: values.to_string(),
            base_ty: vec_ty,
        };
        let count = self.temp_name("count");
        let values_read = self.path(values, vec_ty, span);
        let len = self.method0(values_read, "len", i64_ty, span);
        let count_bind = self.let_stmt(&count, false, i64_ty, len, span);
        let acc = self.temp_name("acc");
        let zero = self.int_lit(0, i64_ty, span);
        let first = self.elem(&source, elem_ty, zero, span);
        let mut stmts = vec![self.let_stmt(&acc, true, elem_ty, first, span)];
        let one = self.int_lit(1, i64_ty, span);
        stmts.extend(self.window_loop(
            one,
            &count,
            |this, index| {
                let value = this.elem(&source, elem_ty, index, span);
                step(this, &acc, value)
            },
            span,
        ));
        let acc_read = self.path(&acc, elem_ty, span);
        let answer = result(self, acc_read);
        let nonempty_body = self.block(stmts, Some(answer), result_ty, span);
        let count_read = self.path(&count, i64_ty, span);
        let zero = self.int_lit(0, i64_ty, span);
        let nonempty = self.binary(HirBinaryOp::Gt, count_read, zero, bool_ty, span);
        let choice = self.if_else(nonempty, nonempty_body, empty, result_ty, span);
        self.block(vec![count_bind], Some(choice), result_ty, span)
    }

    /// `acc = combine(acc, value)`.
    fn combine_step(
        &mut self,
        callback: &Callback,
        acc: &str,
        value: HirExpr,
        elem_ty: Ty,
        span: Span,
    ) -> Vec<HirStmt> {
        let acc_read = self.path(acc, elem_ty, span);
        let combined = self.apply(callback, vec![acc_read, value], elem_ty, span);
        let place = self.path(acc, elem_ty, span);
        vec![self.assign_stmt(place, combined, span)]
    }

    fn reduce(
        &mut self,
        source: &Source,
        elem_ty: Ty,
        callback: &Callback,
        len: &str,
        init: &str,
        span: Span,
    ) -> HirExpr {
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let mut step = |this: &mut Self, acc: &str, value: HirExpr| {
            this.combine_step(callback, acc, value, elem_ty, span)
        };
        let leaf = self.fold_leaf(source, elem_ty, &mut step, span);
        let run = self.run_call(len, MODE_REDUCTION, leaf, vec_ty, span);
        let values = self.temp_name("leaves");
        let bind = self.let_stmt(&values, false, vec_ty, run, span);
        let empty = self.path(init, elem_ty, span);
        let combined = self.fold_values(&values, elem_ty, &mut step, empty, |_, acc| acc, span);
        self.block(vec![bind], Some(combined), elem_ty, span)
    }

    fn sum(&mut self, source: &Source, elem_ty: Ty, len: &str, span: Span) -> HirExpr {
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let mut step = |this: &mut Self, acc: &str, value: HirExpr| {
            let acc_read = this.path(acc, elem_ty, span);
            let added = this.binary(HirBinaryOp::Add, acc_read, value, elem_ty, span);
            let place = this.path(acc, elem_ty, span);
            vec![this.assign_stmt(place, added, span)]
        };
        // A leaf's sum starts from zero, as the sequential sum does, so a
        // negative zero element reads the same both ways.
        let leaf = self.sum_leaf(source, elem_ty, &mut step, span);
        let run = self.run_call(len, MODE_REDUCTION, leaf, vec_ty, span);
        let values = self.temp_name("leaves");
        let bind = self.let_stmt(&values, false, vec_ty, run, span);
        let zero = self.zero(elem_ty, span);
        let total = self.fold_values(&values, elem_ty, &mut step, zero, |_, acc| acc, span);
        self.block(vec![bind], Some(total), elem_ty, span)
    }

    /// A reduction leaf folding its whole window into `acc` from zero.
    fn sum_leaf(
        &mut self,
        source: &Source,
        elem_ty: Ty,
        step: &mut dyn FnMut(&mut Self, &str, HirExpr) -> Vec<HirStmt>,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let (lo, hi) = (self.temp_name("lo"), self.temp_name("hi"));
        let acc = self.temp_name("acc");
        let zero = self.zero(elem_ty, span);
        let mut stmts = vec![self.let_stmt(&acc, true, elem_ty, zero, span)];
        let from = self.path(&lo, i64_ty, span);
        stmts.extend(self.window_loop(
            from,
            &hi,
            |this, index| {
                let value = this.elem(source, elem_ty, index, span);
                step(this, &acc, value)
            },
            span,
        ));
        let acc_read = self.path(&acc, elem_ty, span);
        let one_elem = self.vec_of(vec![acc_read], vec_ty, span);
        let body = self.block(stmts, Some(one_elem), vec_ty, span);
        self.leaf_closure(&lo, &hi, body, vec_ty, span)
    }

    /// `par_min` / `par_max`: the first smallest or the last largest element,
    /// by the element's own order, the total order for a float, or `by`.
    #[allow(clippy::too_many_arguments)]
    fn extreme(
        &mut self,
        source: &Source,
        elem_ty: Ty,
        max: bool,
        by: Option<&Callback>,
        len: &str,
        result_ty: Ty,
        span: Span,
    ) -> HirExpr {
        let vec_ty = self.tcx.intern(TyKind::Vec(elem_ty));
        let mut step = |this: &mut Self, acc: &str, value: HirExpr| {
            let candidate = this.temp_name("v");
            let bind = this.let_stmt(&candidate, false, elem_ty, value, span);
            let cand_read = this.path(&candidate, elem_ty, span);
            let best_read = this.path(acc, elem_ty, span);
            let better = this.better(cand_read, best_read, elem_ty, max, by, span);
            let cand_read = this.path(&candidate, elem_ty, span);
            let place = this.path(acc, elem_ty, span);
            let replace = this.assign_stmt(place, cand_read, span);
            let update = this.if_stmt(better, vec![replace], span);
            vec![bind, update]
        };
        let leaf = self.fold_leaf(source, elem_ty, &mut step, span);
        let run = self.run_call(len, MODE_REDUCTION, leaf, vec_ty, span);
        let values = self.temp_name("leaves");
        let bind = self.let_stmt(&values, false, vec_ty, run, span);
        let none = self.path("None", result_ty, span);
        let best = self.fold_values(
            &values,
            elem_ty,
            &mut step,
            none,
            |this, acc| this.some(acc, result_ty, span),
            span,
        );
        self.block(vec![bind], Some(best), result_ty, span)
    }

    /// Whether `candidate` replaces `best`: strictly smaller for a minimum and
    /// strictly larger for a maximum, so the first of equal elements stays, as
    /// it does for the sequential walk.
    fn better(
        &mut self,
        candidate: HirExpr,
        best: HirExpr,
        elem_ty: Ty,
        max: bool,
        by: Option<&Callback>,
        span: Span,
    ) -> HirExpr {
        let bool_ty = self.tcx.bool_ty();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let op = if max {
            HirBinaryOp::Gt
        } else {
            HirBinaryOp::Lt
        };
        if let Some(by) = by {
            let ordering = self.apply(by, vec![candidate, best], i64_ty, span);
            let zero = self.int_lit(0, i64_ty, span);
            return self.binary(op, ordering, zero, bool_ty, span);
        }
        if let TyKind::Float(width) = *self.tcx.kind_of(elem_ty) {
            let candidate = self.total_order_key(candidate, width, span);
            let best = self.total_order_key(best, width, span);
            return self.binary(op, candidate, best, bool_ty, span);
        }
        self.binary(op, candidate, best, bool_ty, span)
    }

    /// A signed integer ordered as `total_cmp` orders the float: a negative
    /// value's magnitude bits are flipped so larger magnitudes sort lower.
    fn total_order_key(&mut self, value: HirExpr, width: FloatTy, span: Span) -> HirExpr {
        let (owner, bits_ty, signed_ty, unsigned_ty, top) = match width {
            FloatTy::F32 => (
                "f32",
                self.tcx.int_ty(IntTy::U32),
                self.tcx.int_ty(IntTy::I32),
                self.tcx.int_ty(IntTy::U32),
                31,
            ),
            FloatTy::F64 => (
                "f64",
                self.tcx.int_ty(IntTy::U64),
                self.tcx.int_ty(IntTy::I64),
                self.tcx.int_ty(IntTy::U64),
                63,
            ),
        };
        // The associated spelling is the one every tier lowers.
        let callee = self.expr(
            bits_ty,
            span,
            HirExprKind::Path {
                segments: vec![Ident::new(owner), Ident::new("to_bits")],
                def: None,
            },
        );
        let bits = self.expr(
            bits_ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![value],
            },
        );
        let signed = self.cast(bits, signed_ty, span);
        let name = self.temp_name("bits");
        let bind = self.let_stmt(&name, false, signed_ty, signed, span);
        let read = self.path(&name, signed_ty, span);
        let shift = self.int_lit(top, signed_ty, span);
        let sign = self.binary(HirBinaryOp::Shr, read, shift, signed_ty, span);
        let sign = self.cast(sign, unsigned_ty, span);
        let one = self.int_lit(1, unsigned_ty, span);
        let mask = self.binary(HirBinaryOp::Shr, sign, one, unsigned_ty, span);
        let mask = self.cast(mask, signed_ty, span);
        let read = self.path(&name, signed_ty, span);
        let key = self.binary(HirBinaryOp::BitXor, read, mask, signed_ty, span);
        self.block(vec![bind], Some(key), signed_ty, span)
    }

    // ----- node constructors -----

    fn zero(&mut self, ty: Ty, span: Span) -> HirExpr {
        if matches!(self.tcx.kind_of(ty), TyKind::Float(_)) {
            self.expr(
                ty,
                span,
                HirExprKind::Literal(HirLiteral::Float("0.0".to_string())),
            )
        } else {
            self.int_lit(0, ty, span)
        }
    }

    /// Widens an integer bound to `i64`, the width leaves count in.
    fn widen(&mut self, value: HirExpr, i64_ty: Ty, span: Span) -> HirExpr {
        if value.ty == i64_ty {
            value
        } else {
            self.cast(value, i64_ty, span)
        }
    }

    fn cast(&mut self, value: HirExpr, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Cast {
                value: Box::new(value),
                ty,
            },
        )
    }

    fn empty_vec(&mut self, name: &str, ty: Ty, span: Span) -> HirStmt {
        let empty = self.vec_of(Vec::new(), ty, span);
        self.let_stmt(name, true, ty, empty, span)
    }

    fn vec_of(&mut self, items: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        self.expr(ty, span, HirExprKind::Array(HirArrayExpr::List(items)))
    }

    fn push_stmt(&mut self, vec: &str, vec_ty: Ty, value: HirExpr, span: Span) -> HirStmt {
        let unit_ty = self.tcx.unit();
        let receiver = self.path(vec, vec_ty, span);
        let push = self.expr(
            unit_ty,
            span,
            HirExprKind::MethodCall {
                receiver: Box::new(receiver),
                name: Ident::new("push"),
                args: vec![value],
                owner: None,
            },
        );
        self.expr_stmt(push, span)
    }

    fn method0(&mut self, receiver: HirExpr, name: &str, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::MethodCall {
                receiver: Box::new(receiver),
                name: Ident::new(name),
                args: Vec::new(),
                owner: None,
            },
        )
    }

    fn some(&mut self, value: HirExpr, ty: Ty, span: Span) -> HirExpr {
        let callee = self.path("Some", ty, span);
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![value],
            },
        )
    }

    fn if_else(
        &mut self,
        condition: HirExpr,
        then: HirExpr,
        otherwise: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(then),
                else_branch: Some(Box::new(otherwise)),
            },
        )
    }

    fn if_stmt(&mut self, condition: HirExpr, then: Vec<HirStmt>, span: Span) -> HirStmt {
        let unit_ty = self.tcx.unit();
        let then = self.block(then, None, unit_ty, span);
        let if_expr = self.expr(
            unit_ty,
            span,
            HirExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(then),
                else_branch: None,
            },
        );
        self.expr_stmt(if_expr, span)
    }

    fn param(&mut self, name: &str, ty: Ty, span: Span) -> HirParam {
        HirParam {
            pattern: HirPat {
                id: self.ids.next(),
                span,
                ty,
                kind: HirPatKind::Binding {
                    name: Ident::new(name),
                    mutable: false,
                },
            },
            ty,
            is_comptime: false,
        }
    }

    fn param_let(&mut self, param: &HirParam, init: HirExpr, span: Span) -> HirStmt {
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Let {
                pattern: param.pattern.clone(),
                ty: param.ty,
                init: Some(init),
            },
        }
    }

    fn expr(&mut self, ty: Ty, span: Span, kind: HirExprKind) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind,
        }
    }

    fn int_lit(&mut self, v: i64, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Literal(HirLiteral::Int(v.to_string())),
        )
    }

    fn path(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        )
    }

    fn binary(
        &mut self,
        op: HirBinaryOp,
        lhs: HirExpr,
        rhs: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        )
    }

    fn assign_stmt(&mut self, place: HirExpr, value: HirExpr, span: Span) -> HirStmt {
        let unit = self.tcx.unit();
        let assign = self.expr(
            unit,
            span,
            HirExprKind::Assign {
                place: Box::new(place),
                value: Box::new(value),
            },
        );
        self.expr_stmt(assign, span)
    }

    fn expr_stmt(&mut self, expr: HirExpr, span: Span) -> HirStmt {
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Expr {
                expr,
                has_semi: true,
            },
        }
    }

    fn let_stmt(
        &mut self,
        name: &str,
        mutable: bool,
        ty: Ty,
        init: HirExpr,
        span: Span,
    ) -> HirStmt {
        let pat = HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind: HirPatKind::Binding {
                name: Ident::new(name),
                mutable,
            },
        };
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Let {
                pattern: pat,
                ty,
                init: Some(init),
            },
        }
    }

    fn block(&mut self, stmts: Vec<HirStmt>, tail: Option<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        let block = HirBlock {
            id: self.ids.next(),
            span,
            stmts,
            tail: tail.map(Box::new),
            ty,
            is_comptime: false,
        };
        self.expr(ty, span, HirExprKind::Block(block))
    }

    fn temp_name(&mut self, tag: &str) -> String {
        let n = self.temp;
        self.temp += 1;
        format!("__par_{tag}_{n}")
    }

    fn reclone(&mut self, e: &HirExpr) -> HirExpr {
        let mut c = e.clone();
        c.id = self.ids.next();
        c
    }
}

/// The name of a single-segment path to a local binding, which the leaves
/// can read directly rather than through a copy.
fn simple_binding(expr: &HirExpr) -> Option<String> {
    let HirExprKind::Path { segments, def } = &expr.kind else {
        return None;
    };
    (segments.len() == 1 && def.is_none()).then(|| segments[0].name.clone())
}

/// A parameter pattern a spliced `let` binds the same way the call would.
fn simple_param(pat: &HirPat) -> bool {
    match &pat.kind {
        HirPatKind::Binding { .. } | HirPatKind::Wildcard => true,
        HirPatKind::Tuple(items) => items.iter().all(simple_param),
        _ => false,
    }
}
