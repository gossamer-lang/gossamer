//! Fuses `iter::` free-form combinator pipelines over integer ranges
//! into a single streaming loop, with each stage/terminal closure
//! inlined at its use site. A chain such as
//!
//! ```text
//! iter::range_inclusive(1, n) |> |v| iter::filter(|k| k % 2 == 0, v) |> |v| iter::sum_by(|k| k * k, v)
//! ```
//!
//! otherwise materialises the whole range as a `Vec`, then a second
//! `Vec` of survivors, then reduces - three passes and two large
//! allocations. This pass rewrites it to the same tight accumulator loop
//! a hand-written `for` would produce: no intermediate `Vec`, no
//! per-element indirect closure call. Because it runs on the shared HIR
//! before closure lifting and before every backend lowers, all three
//! tiers (bytecode VM, Cranelift JIT, LLVM AOT) get the fused loop.
//!
//! Method chains get the same treatment: `xs.iter().map(f).filter(g).sum()`,
//! `(1..n).map(f).sum()`, and an eager `xs.map(f).sum()` over a sequence of
//! scalars become one index or counter loop. A `for` loop over such a chain,
//! `for (i, x) in xs.iter().enumerate()` or `for i in (0..n).rev()`, becomes
//! the same loop with its body spliced in as the terminal.
//!
//! Recognition is conservative: integer-range and scalar-sequence sources,
//! `filter` / `map` stages, and a fixed set of terminals, with inline
//! single-shape closures throughout. Anything else is left untouched and
//! lowered by the existing combinator path - correct, just unfused.

use std::collections::HashSet;

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_types::{IntTy, Ty, TyCtxt, TyKind};

use crate::ids::HirIdGenerator;
use crate::lift::collect_free_vars;
use crate::tree::{
    HirArrayExpr, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirLiteral, HirMatchArm, HirParam, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind,
    HirUnaryOp,
};

/// The binding a `for` loop over a stateful iterable keeps its cursor in.
pub(crate) const FOR_ITER: &str = "__for_iter";

/// Name a `for` loop binds a compound element under before destructuring it.
pub(crate) const FOR_ELEM: &str = "__for_elem";

/// Rewrites every recognised `iter::` range pipeline in `program` into a
/// fused loop.
pub fn fuse_iter_pipelines(program: &mut HirProgram, tcx: &mut TyCtxt, ids: &mut HirIdGenerator) {
    let mut fuser = Fuser { tcx, ids, temp: 0 };
    for item in &mut program.items {
        fuser.visit_item(item);
    }
}

struct Fuser<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    temp: u32,
}

/// A stage between the source and the terminal.
enum Stage {
    /// `filter(pred)` - keep the element when the predicate holds.
    Filter(HirExpr),
    /// `map(f)` - replace the element with `f(element)`.
    Map(HirExpr),
    /// `enumerate()` - pair the element with the number of elements that
    /// reached this stage before it.
    Enumerate,
    /// `take(n)` - end the loop before the next pull once `n` elements have
    /// passed.
    Take(i64),
    /// `skip(n)` - drop the first `n` elements that reach it.
    Skip(i64),
    /// `step_by(k)` - keep the first element that reaches it and every `k`-th
    /// one after.
    StepBy(i64),
    /// `zip(other)` - pair the element with the next element of `other`,
    /// ending the loop when `other` has none.
    Zip(Source),
    /// `take_while(pred)` - end the loop at the first element the predicate
    /// fails for.
    TakeWhile(HirExpr),
    /// `skip_while(pred)` - drop elements until the predicate first fails,
    /// then pass every element on without asking it again.
    SkipWhile(HirExpr),
    /// `filter_map(f)` - pass on the payload of each `Some` that `f` answers.
    FilterMap(HirExpr),
}

/// A stage with the bindings it keeps across turns of the fused loop.
enum Step<'p> {
    Filter(&'p HirExpr),
    Map(&'p HirExpr),
    Enumerate {
        counter: String,
    },
    Take {
        counter: String,
        limit: i64,
    },
    Skip {
        counter: String,
        count: i64,
    },
    Stride {
        counter: String,
        step: i64,
    },
    Zip {
        position: String,
        end: String,
        inclusive: bool,
        read: ElemRead,
    },
    TakeWhile(&'p HirExpr),
    SkipWhile {
        pred: &'p HirExpr,
        skipping: String,
    },
    FilterMap(&'p HirExpr),
}

/// How the fused loop reads the element at its counter.
enum ElemRead {
    /// A range: the counter is the element.
    Counter,
    /// `base[counter]` over a sequence.
    Index(HirExpr, Ty),
    /// `base.byte_at(counter) as u8` over a `String`.
    Byte(HirExpr),
}

/// The parts of a `Map` walk the loop builder consumes.
struct MapEntries {
    base: HirExpr,
    key_ty: Ty,
    value_ty: Ty,
    part: MapPart,
}

/// How the fused loop's counter walks its source.
struct LoopShape<'s> {
    counter: &'s str,
    bound: &'s str,
    inclusive: bool,
    /// Down from one past the last element, stepping before each read.
    reversed: bool,
}

/// The reducing operation that ends a pipeline.
enum Terminal {
    Sum,
    SumBy(HirExpr),
    Count,
    Product,
    ProductBy(HirExpr),
    Fold(HirExpr, HirExpr),
    ForEach(HirExpr),
    Any(HirExpr),
    All(HirExpr),
    /// `count(pred)` - the number of elements the predicate holds for.
    CountBy(HirExpr),
    /// `find(pred)` - the first element the predicate holds for.
    Find(HirExpr),
    /// `position(pred)` - how many elements came before the first one the
    /// predicate holds for.
    Position(HirExpr),
    /// `min()` - the first smallest element.
    Min,
    /// `max()` - the last largest element.
    Max,
    /// `collect()` - a `Vec` of every element, in order.
    Collect,
    /// The body of a `for` loop over the chain, run with its pattern bound to
    /// each element. `label` is the loop's own, so a labelled `break` or
    /// `continue` in the body reaches the fused loop.
    Loop {
        pat: HirPat,
        body: HirExpr,
        label: Option<String>,
    },
}

/// Where a fused loop's elements come from.
enum Source {
    /// An `i64` range `start..end` / `start..=end`.
    Range {
        start: HirExpr,
        end: HirExpr,
        inclusive: bool,
        reversed: bool,
    },
    /// The elements of a sequence binding, read by index in order.
    Indexed {
        base: HirExpr,
        elem_ty: Ty,
        reversed: bool,
    },
    /// The UTF-8 bytes of a `String` binding.
    Bytes { base: HirExpr, reversed: bool },
}

/// Which part of each `Map` entry a walk hands on.
#[derive(Clone, Copy)]
enum MapPart {
    /// `m.iter()` - the `(key, value)` pair.
    Pairs,
    /// `m.keys()`.
    Keys,
    /// `m.values()`.
    Values,
}

/// What drives a fused loop.
enum Walk {
    /// A counter over a range, a sequence, or a `String`'s bytes.
    Counted(Source),
    /// The entries of a `Map` binding, in the order its own `for` loop and
    /// `iter` / `keys` / `values` answer them.
    Map {
        base: HirExpr,
        key_ty: Ty,
        value_ty: Ty,
        part: MapPart,
    },
}

/// A recognised, fusable pipeline. Every `HirExpr` is a clone owned by
/// the plan so the builder can move it into the loop.
struct Plan {
    walk: Walk,
    stages: Vec<Stage>,
    terminal: Terminal,
    /// Accumulator / result type of the whole pipeline.
    result_ty: Ty,
}

impl Fuser<'_> {
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
            self.walk_block(&mut body.block);
        }
    }

    /// Post-order walk: fuse nested pipelines (including inside closure
    /// bodies) first, then attempt to fuse this node.
    fn visit_expr(&mut self, expr: &mut HirExpr) {
        self.walk_children(expr);
        unwrap_pipe_step_block(expr);
        if let Some(plan) = self.plan(expr) {
            let span = expr.span;
            *expr = self.build(plan, span);
        }
    }

    // One arm per HIR expression variant; the length is the variant count.
    // Splitting it would scatter the exhaustive-visitor structure that keeps
    // every child edge visible in one place.
    #[allow(clippy::too_many_lines)]
    fn walk_children(&mut self, expr: &mut HirExpr) {
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
            HirExprKind::Block(block) => self.walk_block(block),
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

    fn walk_block(&mut self, block: &mut HirBlock) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init, .. } => {
                    if let Some(e) = init {
                        self.visit_expr(e);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                    self.visit_expr(expr);
                }
                HirStmtKind::Item(item) => self.visit_item(item),
            }
        }
        if let Some(tail) = &mut block.tail {
            self.visit_expr(tail);
        }
    }

    // ----- recognition -----

    fn plan(&mut self, expr: &HirExpr) -> Option<Plan> {
        if let Some(plan) = self.plan_for_loop(expr) {
            return Some(plan);
        }
        if let Some(plan) = self.plan_sequence_call(expr) {
            return Some(plan);
        }
        let Some((name, args)) = as_iter_call(expr) else {
            return self.plan_method_chain(expr);
        };
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let unit_ty = self.tcx.unit_interned()?;

        // Terminal + its source arg (always the last argument - the
        // combinators are data-last).
        let (terminal, source) = match (name, args.len()) {
            ("sum", 1) => (Terminal::Sum, &args[0]),
            ("count", 1) => (Terminal::Count, &args[0]),
            ("product", 1) => (Terminal::Product, &args[0]),
            ("sum_by", 2) => (Terminal::SumBy(one_closure(&args[0])?.clone()), &args[1]),
            ("product_by", 2) => (
                Terminal::ProductBy(one_closure(&args[0])?.clone()),
                &args[1],
            ),
            ("for_each", 2) => (Terminal::ForEach(one_closure(&args[0])?.clone()), &args[1]),
            ("any", 2) => (Terminal::Any(one_closure(&args[0])?.clone()), &args[1]),
            ("all", 2) => (Terminal::All(one_closure(&args[0])?.clone()), &args[1]),
            ("fold", 3) => (
                Terminal::Fold(args[0].clone(), n_closure(&args[1], 2)?.clone()),
                &args[2],
            ),
            _ => return None,
        };

        // The result type fixes the accumulator's type; only the i64 /
        // bool / unit shapes are fused.
        let result_ty = match &terminal {
            Terminal::Any(_) | Terminal::All(_) => bool_ty,
            Terminal::ForEach(_) => unit_ty,
            _ => {
                if !is_i64(self.tcx, expr.ty) {
                    return None;
                }
                i64_ty
            }
        };

        // Peel `filter` / `map` stages down to the range source.
        let mut stages_rev = Vec::new();
        let mut cur = source;
        let (start, end, inclusive) = loop {
            let (sname, sargs) = as_iter_call(cur)?;
            match (sname, sargs.len()) {
                ("filter", 2) => {
                    stages_rev.push(Stage::Filter(one_closure(&sargs[0])?.clone()));
                    cur = &sargs[1];
                }
                ("map", 2) => {
                    if !map_closure_returns_i64(self.tcx, &sargs[0]) {
                        return None;
                    }
                    stages_rev.push(Stage::Map(one_closure(&sargs[0])?.clone()));
                    cur = &sargs[1];
                }
                ("range", 2) => break (sargs[0].clone(), sargs[1].clone(), false),
                ("range_inclusive", 2) => break (sargs[0].clone(), sargs[1].clone(), true),
                _ => return None,
            }
        };

        if !is_i64(self.tcx, start.ty) || !is_i64(self.tcx, end.ty) {
            return None;
        }

        stages_rev.reverse();
        Some(Plan {
            walk: Walk::Counted(Source::Range {
                start,
                end,
                inclusive,
                reversed: false,
            }),
            stages: stages_rev,
            terminal,
            result_ty,
        })
    }

    /// `iter::sum(xs)` and the other one-argument terminals over a sequence
    /// binding, which walk it the way `xs.sum()` does.
    fn plan_sequence_call(&mut self, expr: &HirExpr) -> Option<Plan> {
        let (name, [seq]) = as_iter_call(expr)? else {
            return None;
        };
        if !matches!(name, "sum" | "product" | "count" | "min" | "max") {
            return None;
        }
        let terminal = chain_terminal(name, &[])?;
        let source = self.indexed_source(seq)?;
        let elem_ty = self.source_elem_ty(&source);
        let result_ty = self.chain_result_ty(&terminal, expr.ty, elem_ty)?;
        Some(Plan {
            walk: Walk::Counted(source),
            stages: Vec::new(),
            terminal,
            result_ty,
        })
    }

    /// The source a method chain walks, its stages innermost first, and
    /// whether the source is a sequence that eager stages walk.
    fn chain_source<'a>(&mut self, receiver: &'a HirExpr) -> Option<(Walk, Vec<Stage>, bool)> {
        let mut stages_rev = Vec::new();
        let mut reversed = false;
        let mut cur: &'a HirExpr = receiver;
        let (source, eager) = loop {
            match &cur.kind {
                HirExprKind::MethodCall {
                    receiver: inner,
                    name,
                    args,
                    ..
                } => {
                    let call = (name.name.as_str(), args.len());
                    // `rev` walks the source backwards, so it sits right on it.
                    if reversed && call != ("iter", 0) && call != ("bytes", 0) {
                        return None;
                    }
                    if let Some(walk) = self.map_walk(inner, call) {
                        // A `Map` has no reverse walk; `keys` and `values`
                        // answer a `Vec` its eager stages walk.
                        if reversed {
                            return None;
                        }
                        let eager = !matches!(
                            walk,
                            Walk::Map {
                                part: MapPart::Pairs,
                                ..
                            }
                        );
                        stages_rev.reverse();
                        return Some((walk, stages_rev, eager));
                    }
                    match call {
                        ("map", 1) => stages_rev.push(Stage::Map(one_closure(&args[0])?.clone())),
                        ("filter", 1) => {
                            stages_rev.push(Stage::Filter(one_closure(&args[0])?.clone()));
                        }
                        ("enumerate", 0) => stages_rev.push(Stage::Enumerate),
                        ("take", 1) => stages_rev.push(Stage::Take(count_literal(&args[0], 0)?)),
                        ("skip", 1) => stages_rev.push(Stage::Skip(count_literal(&args[0], 0)?)),
                        ("step_by", 1) => {
                            stages_rev.push(Stage::StepBy(count_literal(&args[0], 1)?));
                        }
                        ("zip", 1) => stages_rev.push(Stage::Zip(self.zip_source(&args[0])?)),
                        ("take_while", 1) => {
                            stages_rev.push(Stage::TakeWhile(one_closure(&args[0])?.clone()));
                        }
                        ("skip_while", 1) => {
                            stages_rev.push(Stage::SkipWhile(one_closure(&args[0])?.clone()));
                        }
                        ("filter_map", 1) => {
                            stages_rev.push(Stage::FilterMap(one_closure(&args[0])?.clone()));
                        }
                        ("rev", 0) => reversed = true,
                        ("iter", 0) => break (self.indexed_source(inner)?, false),
                        // `bytes` builds its `Vec<u8>` with no closure, so
                        // reading the text in place walks the same bytes.
                        ("bytes", 0) => break (self.bytes_source(inner)?, false),
                        _ => return None,
                    }
                    cur = inner;
                }
                HirExprKind::Range { .. } => break (range_source(self.tcx, cur)?, false),
                // A terminal or an eager stage on a sequence walks the same
                // elements in the same order.
                _ => break (self.indexed_source(cur)?, true),
            }
        };
        let source = match source {
            // A reversed inclusive range would step below its start.
            Source::Range {
                inclusive: true, ..
            } if reversed => return None,
            Source::Range {
                start,
                end,
                inclusive,
                ..
            } => Source::Range {
                start,
                end,
                inclusive,
                reversed,
            },
            Source::Indexed { base, elem_ty, .. } => Source::Indexed {
                base,
                elem_ty,
                reversed,
            },
            Source::Bytes { base, .. } => Source::Bytes { base, reversed },
        };
        stages_rev.reverse();
        Some((Walk::Counted(source), stages_rev, eager))
    }

    /// `m.iter()`, `m.keys()`, or `m.values()` over a `Map` binding of scalar
    /// keys and values.
    fn map_walk(&self, base: &HirExpr, call: (&str, usize)) -> Option<Walk> {
        let part = match call {
            ("iter", 0) => MapPart::Pairs,
            ("keys", 0) => MapPart::Keys,
            ("values", 0) => MapPart::Values,
            _ => return None,
        };
        let HirExprKind::Path { segments, .. } = &base.kind else {
            return None;
        };
        if segments.len() != 1 {
            return None;
        }
        let TyKind::HashMap { key, value, .. } = self.tcx.kind_of(base.ty) else {
            return None;
        };
        if !is_walked_elem(self.tcx, *key) || !is_walked_elem(self.tcx, *value) {
            return None;
        }
        Some(Walk::Map {
            base: base.clone(),
            key_ty: *key,
            value_ty: *value,
            part,
        })
    }

    fn walk_elem_ty(&mut self, walk: &Walk) -> Ty {
        match walk {
            Walk::Counted(source) => self.source_elem_ty(source),
            Walk::Map {
                key_ty,
                value_ty,
                part,
                ..
            } => match part {
                MapPart::Pairs => self.tcx.intern(TyKind::Tuple(vec![*key_ty, *value_ty])),
                MapPart::Keys => *key_ty,
                MapPart::Values => *value_ty,
            },
        }
    }

    /// The second source of a `zip`: a range or `other.iter()` over a binding.
    fn zip_source(&self, arg: &HirExpr) -> Option<Source> {
        match &arg.kind {
            HirExprKind::Range { .. } => range_source(self.tcx, arg),
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if args.is_empty() => match name.name.as_str() {
                "iter" => self.indexed_source(receiver),
                "bytes" => self.bytes_source(receiver),
                _ => None,
            },
            _ => None,
        }
    }

    fn source_elem_ty(&mut self, source: &Source) -> Ty {
        match source {
            Source::Range { .. } => self.tcx.int_ty(IntTy::I64),
            Source::Indexed { elem_ty, .. } => *elem_ty,
            Source::Bytes { .. } => self.tcx.int_ty(IntTy::U8),
        }
    }

    /// The bytes of a `String` binding.
    fn bytes_source(&self, base: &HirExpr) -> Option<Source> {
        let HirExprKind::Path { segments, .. } = &base.kind else {
            return None;
        };
        if segments.len() != 1 || !matches!(self.tcx.kind_of(base.ty), TyKind::String) {
            return None;
        }
        Some(Source::Bytes {
            base: base.clone(),
            reversed: false,
        })
    }

    /// The type a fused chain answers, or `None` when the terminal's result
    /// is not the one a plain loop computes.
    fn chain_result_ty(&mut self, terminal: &Terminal, expr_ty: Ty, elem_ty: Ty) -> Option<Ty> {
        Some(match terminal {
            Terminal::Any(_) | Terminal::All(_) => self.tcx.bool_ty(),
            Terminal::ForEach(_) => self.tcx.unit_interned()?,
            Terminal::Count | Terminal::CountBy(_) => {
                if !is_i64(self.tcx, expr_ty) {
                    return None;
                }
                expr_ty
            }
            // The accumulator is the element type itself, so a sum answers
            // exactly what adding the elements with `+` from zero answers.
            Terminal::Sum | Terminal::Product => {
                if !is_number(self.tcx, expr_ty) || expr_ty != elem_ty {
                    return None;
                }
                expr_ty
            }
            Terminal::Fold(init, _) => {
                if !is_scalar(self.tcx, expr_ty) || init.ty != expr_ty {
                    return None;
                }
                expr_ty
            }
            Terminal::Find(_) => {
                if option_payload(self.tcx, expr_ty)? != elem_ty {
                    return None;
                }
                expr_ty
            }
            Terminal::Position(_) => {
                if !is_i64(self.tcx, option_payload(self.tcx, expr_ty)?) {
                    return None;
                }
                expr_ty
            }
            // Only an integer's order is its `<`; a float's `min` / `max`
            // follows the runtime's IEEE rule.
            Terminal::Min | Terminal::Max => {
                if option_payload(self.tcx, expr_ty)? != elem_ty
                    || !matches!(self.tcx.kind_of(elem_ty), TyKind::Int(_))
                {
                    return None;
                }
                expr_ty
            }
            Terminal::Collect => {
                if !matches!(self.tcx.kind_of(expr_ty), TyKind::Vec(elem) if *elem == elem_ty) {
                    return None;
                }
                expr_ty
            }
            // The pattern binds the element as the loop's own `next()` hands it.
            Terminal::Loop { pat, .. } => {
                if pat.ty != elem_ty {
                    return None;
                }
                self.tcx.unit_interned()?
            }
            Terminal::SumBy(_) | Terminal::ProductBy(_) => return None,
        })
    }

    /// Recognises a method chain ending in a terminal whose receiver root is an
    /// `i64` range, `xs.iter()`, or a sequence binding an eager stage walks.
    fn plan_method_chain(&mut self, expr: &HirExpr) -> Option<Plan> {
        let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        let terminal = chain_terminal(name.name.as_str(), args)?;
        self.plan_chain(receiver, terminal, expr.ty)
    }

    /// Recognises `for pat in chain { body }`, as the `__for_iter` binding and
    /// `next()` loop it lowers to, over a chain whose source is an `i64` range,
    /// `xs.iter()`, or a sequence binding.
    fn plan_for_loop(&mut self, expr: &HirExpr) -> Option<Plan> {
        let HirExprKind::Block(block) = &expr.kind else {
            return None;
        };
        let ([iter_let], Some(tail)) = (block.stmts.as_slice(), block.tail.as_deref()) else {
            return None;
        };
        let HirStmtKind::Let {
            pattern,
            init: Some(chain),
            ..
        } = &iter_let.kind
        else {
            return None;
        };
        if !matches!(&pattern.kind, HirPatKind::Binding { name, .. } if name.name == FOR_ITER) {
            return None;
        }
        let HirExprKind::Loop { body, label } = &tail.kind else {
            return None;
        };
        let HirExprKind::Block(loop_block) = &body.kind else {
            return None;
        };
        let (true, Some(turn)) = (loop_block.stmts.is_empty(), loop_block.tail.as_deref()) else {
            return None;
        };
        let HirExprKind::Match { scrutinee, arms } = &turn.kind else {
            return None;
        };
        let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &scrutinee.kind
        else {
            return None;
        };
        let HirExprKind::Unary {
            op: HirUnaryOp::RefMut,
            operand,
        } = &receiver.kind
        else {
            return None;
        };
        if name.name != "next" || !args.is_empty() || !is_binding_path(operand, FOR_ITER) {
            return None;
        }
        let [some_arm, none_arm] = arms.as_slice() else {
            return None;
        };
        let HirPatKind::Variant {
            name: some_name,
            fields,
        } = &some_arm.pattern.kind
        else {
            return None;
        };
        let [pat] = fields.as_slice() else {
            return None;
        };
        let none_breaks = matches!(
            &none_arm.body.kind,
            HirExprKind::Break {
                value: None,
                label: None
            }
        );
        if some_name.name != "Some"
            || some_arm.guard.is_some()
            || none_arm.guard.is_some()
            || !matches!(&none_arm.pattern.kind, HirPatKind::Variant { name, fields }
                if name.name == "None" && fields.is_empty())
            || !none_breaks
        {
            return None;
        }
        let terminal = Terminal::Loop {
            pat: pat.clone(),
            body: some_arm.body.clone(),
            label: label.clone(),
        };
        let unit_ty = self.tcx.unit_interned()?;
        let plan = self.plan_chain(chain, terminal, unit_ty)?;
        // A map's own `for` loop already walks its entries in place.
        if matches!(plan.walk, Walk::Map { .. }) {
            return None;
        }
        Some(plan)
    }

    /// The plan for `terminal` over the chain `receiver`, answering `expr_ty`.
    fn plan_chain(&mut self, receiver: &HirExpr, terminal: Terminal, expr_ty: Ty) -> Option<Plan> {
        let (walk, stages_rev, eager) = self.chain_source(receiver)?;
        if eager && !eager_order_kept(&stages_rev, &terminal) {
            return None;
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let mut elem_ty = self.walk_elem_ty(&walk);
        for stage in &stages_rev {
            elem_ty = match stage {
                Stage::Map(f) => {
                    let HirExprKind::Closure { body, .. } = &f.kind else {
                        return None;
                    };
                    body.ty
                }
                Stage::Enumerate => self.tcx.intern(TyKind::Tuple(vec![i64_ty, elem_ty])),
                Stage::Zip(other) => {
                    let other_ty = self.source_elem_ty(other);
                    self.tcx.intern(TyKind::Tuple(vec![elem_ty, other_ty]))
                }
                Stage::FilterMap(f) => {
                    let HirExprKind::Closure { body, .. } = &f.kind else {
                        return None;
                    };
                    option_payload(self.tcx, body.ty)?
                }
                Stage::Filter(_)
                | Stage::Take(_)
                | Stage::Skip(_)
                | Stage::StepBy(_)
                | Stage::TakeWhile(_)
                | Stage::SkipWhile(_) => elem_ty,
            };
            if !is_walked_elem(self.tcx, elem_ty) {
                return None;
            }
        }
        if !is_walked_elem(self.tcx, elem_ty) {
            return None;
        }
        let result_ty = self.chain_result_ty(&terminal, expr_ty, elem_ty)?;
        // A closure that names a walked binding could change it mid-walk.
        let walked_bases = stages_rev
            .iter()
            .filter_map(|stage| match stage {
                Stage::Zip(Source::Indexed { base, .. }) => Some(base),
                _ => None,
            })
            .chain(match &walk {
                Walk::Counted(Source::Indexed { base, .. }) | Walk::Map { base, .. } => Some(base),
                Walk::Counted(_) => None,
            });
        for base in walked_bases {
            if closures_name_base(base, &stages_rev, &terminal)? {
                return None;
            }
        }
        Some(Plan {
            walk,
            stages: stages_rev,
            terminal,
            result_ty,
        })
    }

    /// A sequence binding of scalar elements, the only shape an index loop
    /// reads with no share taken per element.
    fn indexed_source(&self, base: &HirExpr) -> Option<Source> {
        let HirExprKind::Path { segments, .. } = &base.kind else {
            return None;
        };
        if segments.len() != 1 {
            return None;
        }
        // A sequence parameter is a reference to its caller's value; indexing
        // and `len` read through it.
        let mut seq_ty = base.ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(seq_ty) {
            seq_ty = *inner;
        }
        let elem_ty = match self.tcx.kind_of(seq_ty) {
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => *elem,
            _ => return None,
        };
        if !is_walked_elem(self.tcx, elem_ty) {
            return None;
        }
        Some(Source::Indexed {
            base: base.clone(),
            elem_ty,
            reversed: false,
        })
    }

    // ----- construction -----

    fn build(&mut self, plan: Plan, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let counter = self.temp_name("i");
        let end_name = self.temp_name("end");
        let acc = self.temp_name("acc");
        let has_acc = !matches!(plan.terminal, Terminal::ForEach(_) | Terminal::Loop { .. });

        let Plan {
            walk,
            stages,
            terminal,
            result_ty,
        } = plan;

        let mut stmts: Vec<HirStmt> = Vec::new();
        if has_acc {
            let init = self.acc_init(&terminal, result_ty, span);
            let s = self.let_stmt(&acc, true, result_ty, init, span);
            stmts.push(s);
        }
        let source = match walk {
            Walk::Counted(source) => source,
            Walk::Map {
                base,
                key_ty,
                value_ty,
                part,
            } => {
                self.terminal_bindings(&terminal, &acc, result_ty, &mut stmts, span);
                let steps = self.stage_steps(&stages, &mut stmts, span);
                let entries = MapEntries {
                    base,
                    key_ty,
                    value_ty,
                    part,
                };
                let loop_expr =
                    self.build_map_loop(entries, &steps, &terminal, &acc, result_ty, span);
                let s = self.expr_stmt(loop_expr, span);
                stmts.push(s);
                return self.finish(stmts, has_acc, &acc, result_ty, span);
            }
        };
        let (start, end, inclusive, reversed, read) = match source {
            Source::Range {
                start,
                end,
                inclusive,
                reversed,
            } => (start, end, inclusive, reversed, ElemRead::Counter),
            Source::Indexed {
                base,
                elem_ty,
                reversed,
            } => {
                let zero = self.int_lit(0, i64_ty, span);
                let len = self.len_call(&base, span);
                (zero, len, false, reversed, ElemRead::Index(base, elem_ty))
            }
            Source::Bytes { base, reversed } => {
                let zero = self.int_lit(0, i64_ty, span);
                let len = self.method0(&base, "byte_len", i64_ty, span);
                (zero, len, false, reversed, ElemRead::Byte(base))
            }
        };
        // Both walks evaluate the start before the end.
        if reversed {
            let s = self.let_stmt(&end_name, false, i64_ty, start, span);
            stmts.push(s);
            let s = self.let_stmt(&counter, true, i64_ty, end, span);
            stmts.push(s);
        } else {
            let s = self.let_stmt(&counter, true, i64_ty, start, span);
            stmts.push(s);
            let s = self.let_stmt(&end_name, false, i64_ty, end, span);
            stmts.push(s);
        }
        self.terminal_bindings(&terminal, &acc, result_ty, &mut stmts, span);
        let steps = self.stage_steps(&stages, &mut stmts, span);
        let shape = LoopShape {
            counter: &counter,
            bound: &end_name,
            inclusive,
            reversed,
        };
        let while_expr = self.build_while(&shape, read, &steps, &terminal, &acc, result_ty, span);
        let s = self.expr_stmt(while_expr, span);
        stmts.push(s);
        self.finish(stmts, has_acc, &acc, result_ty, span)
    }

    /// The pipeline's block: its statements, then the accumulator when the
    /// terminal answers one.
    fn finish(
        &mut self,
        stmts: Vec<HirStmt>,
        has_acc: bool,
        acc: &str,
        result_ty: Ty,
        span: Span,
    ) -> HirExpr {
        if has_acc {
            let tail = self.path(acc, result_ty, span);
            self.block(stmts, Some(tail), result_ty, span)
        } else {
            let unit_ty = self.tcx.unit();
            self.block(stmts, None, unit_ty, span)
        }
    }

    /// `for (key, value) in base { .. }`, spelled as the `loop` / `match` on
    /// `next()` a `for` lowers to, so the walk is the one the map's own `for`
    /// loop takes. Only the parts the pipeline reads are bound.
    fn build_map_loop(
        &mut self,
        entries: MapEntries,
        steps: &[Step<'_>],
        terminal: &Terminal,
        acc: &str,
        result_ty: Ty,
        span: Span,
    ) -> HirExpr {
        let MapEntries {
            base,
            key_ty,
            value_ty,
            part,
        } = entries;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit_ty = self.tcx.unit();
        let bool_ty = self.tcx.bool_ty();

        let mut body_stmts = Vec::new();
        for step in steps {
            if let Step::Take { counter, limit } = step {
                let taken = self.path(counter, i64_ty, span);
                let limit = self.int_lit(*limit, i64_ty, span);
                let done = self.binary(HirBinaryOp::Ge, taken, limit, bool_ty, span);
                let stop = self.break_if(done, span);
                body_stmts.push(stop);
            }
        }
        let key_name = self.temp_name("k");
        let value_name = self.temp_name("mv");
        let reads_key = !matches!(part, MapPart::Values);
        let reads_value = !matches!(part, MapPart::Keys);
        let key_pat = self.entry_pat(reads_key.then_some(key_name.as_str()), key_ty, span);
        let value_pat = self.entry_pat(reads_value.then_some(value_name.as_str()), value_ty, span);
        let pair_ty = self.tcx.intern(TyKind::Tuple(vec![key_ty, value_ty]));
        let elem = match part {
            MapPart::Pairs => {
                let key = self.path(&key_name, key_ty, span);
                let value = self.path(&value_name, value_ty, span);
                self.expr(pair_ty, span, HirExprKind::Tuple(vec![key, value]))
            }
            MapPart::Keys => self.path(&key_name, key_ty, span),
            MapPart::Values => self.path(&value_name, value_ty, span),
        };
        let elem = if matches!(part, MapPart::Pairs) {
            let elem_name = self.temp_name("e");
            let bind = self.let_stmt(&elem_name, false, pair_ty, elem, span);
            body_stmts.push(bind);
            self.path(&elem_name, pair_ty, span)
        } else {
            elem
        };
        body_stmts.extend(self.build_body(steps, 0, elem, terminal, acc, result_ty, span));
        let body = self.block(body_stmts, None, unit_ty, span);

        let entry_pat = HirPat {
            id: self.ids.next(),
            span,
            ty: pair_ty,
            kind: HirPatKind::Tuple(vec![key_pat, value_pat]),
        };
        let (some_pat, none_pat) = self.some_none_pats(entry_pat, pair_ty, span);
        let never = self.tcx.never();
        let brk = self.expr(
            never,
            span,
            HirExprKind::Break {
                value: None,
                label: None,
            },
        );
        let next_call = self.expr(
            pair_ty,
            span,
            HirExprKind::MethodCall {
                receiver: Box::new(base),
                name: Ident::new("next"),
                args: Vec::new(),
                owner: None,
            },
        );
        let match_expr = self.match_some(next_call, (some_pat, body), (none_pat, brk), span);
        let loop_body = self.block(Vec::new(), Some(match_expr), unit_ty, span);
        self.expr(
            unit_ty,
            span,
            HirExprKind::Loop {
                body: Box::new(loop_body),
                label: None,
            },
        )
    }

    /// A binding for an entry part the pipeline reads, `_` for one it does not.
    fn entry_pat(&mut self, name: Option<&str>, ty: Ty, span: Span) -> HirPat {
        let kind = match name {
            Some(name) => HirPatKind::Binding {
                name: Ident::new(name),
                mutable: false,
            },
            None => HirPatKind::Wildcard,
        };
        HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind,
        }
    }

    /// The `Some(<payload>)` and `None` patterns of a match whose scrutinee
    /// has type `ty`.
    fn some_none_pats(&mut self, payload: HirPat, ty: Ty, span: Span) -> (HirPat, HirPat) {
        let some_pat = HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind: HirPatKind::Variant {
                name: Ident::new("Some"),
                fields: vec![payload],
            },
        };
        let none_pat = HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind: HirPatKind::Variant {
                name: Ident::new("None"),
                fields: Vec::new(),
            },
        };
        (some_pat, none_pat)
    }

    /// `match scrutinee { Some(..) => <body>, None => <body> }` as a unit.
    fn match_some(
        &mut self,
        scrutinee: HirExpr,
        some: (HirPat, HirExpr),
        none: (HirPat, HirExpr),
        span: Span,
    ) -> HirExpr {
        let unit_ty = self.tcx.unit();
        let arm = |(pattern, body)| HirMatchArm {
            pattern,
            guard: None,
            body,
        };
        self.expr(
            unit_ty,
            span,
            HirExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![arm(some), arm(none)],
            },
        )
    }

    /// `if cond { then } else { otherwise }` as an expression of type `ty`.
    fn if_expr(
        &mut self,
        cond: HirExpr,
        then: HirExpr,
        otherwise: Option<HirExpr>,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::If {
                condition: Box::new(cond),
                then_branch: Box::new(then),
                else_branch: otherwise.map(Box::new),
            },
        )
    }

    /// `if cond { then } else { otherwise }` as a statement.
    fn if_stmt(
        &mut self,
        cond: HirExpr,
        then: Vec<HirStmt>,
        otherwise: Option<Vec<HirStmt>>,
        span: Span,
    ) -> HirStmt {
        let unit_ty = self.tcx.unit();
        let then = self.block(then, None, unit_ty, span);
        let otherwise = otherwise.map(|stmts| self.block(stmts, None, unit_ty, span));
        let if_expr = self.if_expr(cond, then, otherwise, unit_ty, span);
        self.expr_stmt(if_expr, span)
    }

    /// Binds, ahead of the loop, the state each stage keeps across turns.
    fn stage_steps<'p>(
        &mut self,
        stages: &'p [Stage],
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) -> Vec<Step<'p>> {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let mut steps = Vec::with_capacity(stages.len());
        for stage in stages {
            let step = match stage {
                Stage::Filter(pred) => Step::Filter(pred),
                Stage::Map(f) => Step::Map(f),
                Stage::TakeWhile(pred) => Step::TakeWhile(pred),
                Stage::FilterMap(f) => Step::FilterMap(f),
                Stage::SkipWhile(pred) => {
                    let skipping = self.temp_name("skipping");
                    let yes = self.bool_lit(true, span);
                    let bool_ty = self.tcx.bool_ty();
                    let s = self.let_stmt(&skipping, true, bool_ty, yes, span);
                    stmts.push(s);
                    Step::SkipWhile { pred, skipping }
                }
                Stage::Enumerate | Stage::Take(_) | Stage::Skip(_) | Stage::StepBy(_) => {
                    let counter = self.temp_name("n");
                    let zero = self.int_lit(0, i64_ty, span);
                    let s = self.let_stmt(&counter, true, i64_ty, zero, span);
                    stmts.push(s);
                    match stage {
                        Stage::Take(limit) => Step::Take {
                            counter,
                            limit: *limit,
                        },
                        Stage::Skip(count) => Step::Skip {
                            counter,
                            count: *count,
                        },
                        Stage::StepBy(step) => Step::Stride {
                            counter,
                            step: *step,
                        },
                        _ => Step::Enumerate { counter },
                    }
                }
                Stage::Zip(other) => {
                    let position = self.temp_name("z");
                    let end = self.temp_name("zend");
                    let (start, end_expr, inclusive, read) = match other {
                        Source::Range {
                            start,
                            end,
                            inclusive,
                            ..
                        } => (
                            self.reclone(start),
                            self.reclone(end),
                            *inclusive,
                            ElemRead::Counter,
                        ),
                        Source::Indexed { base, elem_ty, .. } => {
                            let zero = self.int_lit(0, i64_ty, span);
                            let len = self.len_call(base, span);
                            (
                                zero,
                                len,
                                false,
                                ElemRead::Index(self.reclone(base), *elem_ty),
                            )
                        }
                        Source::Bytes { base, .. } => {
                            let zero = self.int_lit(0, i64_ty, span);
                            let len = self.method0(base, "byte_len", i64_ty, span);
                            (zero, len, false, ElemRead::Byte(self.reclone(base)))
                        }
                    };
                    let s = self.let_stmt(&position, true, i64_ty, start, span);
                    stmts.push(s);
                    let s = self.let_stmt(&end, false, i64_ty, end_expr, span);
                    stmts.push(s);
                    Step::Zip {
                        position,
                        end,
                        inclusive,
                        read,
                    }
                }
            };
            steps.push(step);
        }
        steps
    }

    /// Builds the loop over the source: `take` checks, the element read, the
    /// stages and terminal, and the counter step.
    /// With an indexed source the element is `base[counter]`, bound once
    /// per turn before the stages read it.
    // The loop's shape is the product of every part a plan names; grouping
    // them into a struct would only restate the plan.
    #[allow(clippy::too_many_arguments)]
    fn build_while(
        &mut self,
        shape: &LoopShape<'_>,
        read: ElemRead,
        steps: &[Step<'_>],
        terminal: &Terminal,
        acc: &str,
        result_ty: Ty,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit_ty = self.tcx.unit();
        let bool_ty = self.tcx.bool_ty();

        let mut body_stmts = Vec::new();
        // A `take` whose count has passed ends the loop before the next pull.
        for step in steps {
            if let Step::Take { counter, limit } = step {
                let taken = self.path(counter, i64_ty, span);
                let limit = self.int_lit(*limit, i64_ty, span);
                let done = self.binary(HirBinaryOp::Ge, taken, limit, bool_ty, span);
                let stop = self.break_if(done, span);
                body_stmts.push(stop);
            }
        }
        if shape.reversed {
            let down = self.step_stmt(shape.counter, HirBinaryOp::Sub, span);
            body_stmts.push(down);
        }
        // A loop body may `continue`, which skips everything after it in the
        // turn, so the counter steps before the body runs. The element is
        // read into a binding first, since a range's element is the counter.
        let (step_first, label) = match terminal {
            Terminal::Loop { label, .. } => (true, label.clone()),
            _ => (false, None),
        };
        let value = self.elem_value(&read, shape.counter, span);
        let elem_ty = value.ty;
        let elem = if matches!(read, ElemRead::Counter) && !step_first {
            value
        } else {
            let elem_name = self.temp_name("e");
            let bind = self.let_stmt(&elem_name, false, elem_ty, value, span);
            body_stmts.push(bind);
            self.path(&elem_name, elem_ty, span)
        };
        if step_first && !shape.reversed {
            let up = self.step_stmt(shape.counter, HirBinaryOp::Add, span);
            body_stmts.push(up);
        }
        body_stmts.extend(self.build_body(steps, 0, elem, terminal, acc, result_ty, span));
        if !shape.reversed && !step_first {
            let up = self.step_stmt(shape.counter, HirBinaryOp::Add, span);
            body_stmts.push(up);
        }
        let body_block = self.block(body_stmts, None, unit_ty, span);

        let cmp_op = match (shape.reversed, shape.inclusive) {
            (true, _) => HirBinaryOp::Gt,
            (false, true) => HirBinaryOp::Le,
            (false, false) => HirBinaryOp::Lt,
        };
        let lhs = self.path(shape.counter, i64_ty, span);
        let rhs = self.path(shape.bound, i64_ty, span);
        let cond = self.binary(cmp_op, lhs, rhs, bool_ty, span);
        self.expr(
            unit_ty,
            span,
            HirExprKind::While {
                condition: Box::new(cond),
                body: Box::new(body_block),
                label,
            },
        )
    }

    // Each argument threads one piece of the recursion's state.
    #[allow(clippy::too_many_arguments)]
    fn build_body(
        &mut self,
        steps: &[Step<'_>],
        i: usize,
        elem: HirExpr,
        terminal: &Terminal,
        acc: &str,
        result_ty: Ty,
        span: Span,
    ) -> Vec<HirStmt> {
        if i == steps.len() {
            return self.terminal_stmts(terminal, elem, acc, result_ty, span);
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit_ty = self.tcx.unit();
        let bool_ty = self.tcx.bool_ty();
        match &steps[i] {
            Step::Filter(pred) => {
                let elem_for_pred = self.reclone(&elem);
                let cond = self.inline1(pred, elem_for_pred, span);
                let rest = self.build_body(steps, i + 1, elem, terminal, acc, result_ty, span);
                vec![self.if_stmt(cond, rest, None, span)]
            }
            Step::Map(f) => {
                let vname = self.temp_name("v");
                let mapped = self.inline1(f, elem, span);
                let mapped_ty = mapped.ty;
                let mut out = vec![self.let_stmt(&vname, false, mapped_ty, mapped, span)];
                let next = self.path(&vname, mapped_ty, span);
                out.extend(self.build_body(steps, i + 1, next, terminal, acc, result_ty, span));
                out
            }
            Step::Enumerate { counter } => {
                let index = self.path(counter, i64_ty, span);
                let (bind, pair) = self.bind_pair(index, elem, span);
                let up = self.step_stmt(counter, HirBinaryOp::Add, span);
                let mut out = vec![bind, up];
                out.extend(self.build_body(steps, i + 1, pair, terminal, acc, result_ty, span));
                out
            }
            Step::Take { counter, .. } => {
                let up = self.step_stmt(counter, HirBinaryOp::Add, span);
                let mut out = vec![up];
                out.extend(self.build_body(steps, i + 1, elem, terminal, acc, result_ty, span));
                out
            }
            Step::Skip { counter, count } => {
                let seen = self.path(counter, i64_ty, span);
                let limit = self.int_lit(*count, i64_ty, span);
                let skipping = self.binary(HirBinaryOp::Lt, seen, limit, bool_ty, span);
                let up = self.step_stmt(counter, HirBinaryOp::Add, span);
                let rest = self.build_body(steps, i + 1, elem, terminal, acc, result_ty, span);
                vec![self.if_stmt(skipping, vec![up], Some(rest), span)]
            }
            Step::Stride { counter, step } => {
                let (mut out, hit) = self.stride_hit(counter, *step, span);
                let rest = self.build_body(steps, i + 1, elem, terminal, acc, result_ty, span);
                out.push(self.if_stmt(hit, rest, None, span));
                out
            }
            Step::Zip {
                position,
                end,
                inclusive,
                read,
            } => {
                let (mut out, pair) = self.zip_pair(position, end, *inclusive, read, elem, span);
                out.extend(self.build_body(steps, i + 1, pair, terminal, acc, result_ty, span));
                out
            }
            Step::TakeWhile(pred) => {
                let elem_for_pred = self.reclone(&elem);
                let holds = self.inline1(pred, elem_for_pred, span);
                let fails = self.not(holds, span);
                let mut out = vec![self.break_if(fails, span)];
                out.extend(self.build_body(steps, i + 1, elem, terminal, acc, result_ty, span));
                out
            }
            Step::SkipWhile { pred, skipping } => {
                let (bind, dropping, passed) = self.skip_while_gate(pred, skipping, &elem, span);
                let mut rest = vec![passed];
                rest.extend(self.build_body(steps, i + 1, elem, terminal, acc, result_ty, span));
                vec![bind, self.if_stmt(dropping, Vec::new(), Some(rest), span)]
            }
            Step::FilterMap(f) => {
                let mapped = self.inline1(f, elem, span);
                let option_ty = mapped.ty;
                let Some(payload_ty) = option_payload(self.tcx, option_ty) else {
                    return Vec::new();
                };
                let vname = self.temp_name("v");
                let binding = self.entry_pat(Some(&vname), payload_ty, span);
                let (some_pat, none_pat) = self.some_none_pats(binding, option_ty, span);
                let payload = self.path(&vname, payload_ty, span);
                let rest = self.build_body(steps, i + 1, payload, terminal, acc, result_ty, span);
                let some_body = self.block(rest, None, unit_ty, span);
                let none_body = self.block(Vec::new(), None, unit_ty, span);
                let matched =
                    self.match_some(mapped, (some_pat, some_body), (none_pat, none_body), span);
                vec![self.expr_stmt(matched, span)]
            }
        }
    }

    /// Binds whether this turn is one a `step_by` keeps and advances its
    /// counter, answering those statements and the condition that reads the
    /// binding.
    fn stride_hit(&mut self, counter: &str, step: i64, span: Span) -> (Vec<HirStmt>, HirExpr) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let seen = self.path(counter, i64_ty, span);
        let every = self.int_lit(step, i64_ty, span);
        let offset = self.binary(HirBinaryOp::Rem, seen, every, i64_ty, span);
        let zero = self.int_lit(0, i64_ty, span);
        let hit = self.binary(HirBinaryOp::Eq, offset, zero, bool_ty, span);
        let hit_name = self.temp_name("hit");
        let bind = self.let_stmt(&hit_name, false, bool_ty, hit, span);
        let up = self.step_stmt(counter, HirBinaryOp::Add, span);
        let cond = self.path(&hit_name, bool_ty, span);
        (vec![bind, up], cond)
    }

    /// Ends the loop once the zipped sequence runs out, pairs the element with
    /// that sequence's, and advances its position.
    fn zip_pair(
        &mut self,
        position: &str,
        end: &str,
        inclusive: bool,
        read: &ElemRead,
        elem: HirExpr,
        span: Span,
    ) -> (Vec<HirStmt>, HirExpr) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let at = self.path(position, i64_ty, span);
        let bound = self.path(end, i64_ty, span);
        let op = if inclusive {
            HirBinaryOp::Gt
        } else {
            HirBinaryOp::Ge
        };
        let done = self.binary(op, at, bound, bool_ty, span);
        let stop = self.break_if(done, span);
        let other = self.elem_value(read, position, span);
        let (bind, pair) = self.bind_pair(elem, other, span);
        let up = self.step_stmt(position, HirBinaryOp::Add, span);
        (vec![stop, bind, up], pair)
    }

    /// What a `skip_while` decides each turn: the binding of whether this
    /// element is still dropped, the condition that reads it, and the store
    /// that ends the skipping once an element passes.
    fn skip_while_gate(
        &mut self,
        pred: &HirExpr,
        skipping: &str,
        elem: &HirExpr,
        span: Span,
    ) -> (HirStmt, HirExpr, HirStmt) {
        let bool_ty = self.tcx.bool_ty();
        let elem_for_pred = self.reclone(elem);
        let holds = self.inline1(pred, elem_for_pred, span);
        let still = self.path(skipping, bool_ty, span);
        let no = self.bool_lit(false, span);
        let asked = self.block(Vec::new(), Some(holds), bool_ty, span);
        let not_asked = self.block(Vec::new(), Some(no), bool_ty, span);
        let dropping = self.if_expr(still, asked, Some(not_asked), bool_ty, span);
        let dropping_name = self.temp_name("dropping");
        let bind = self.let_stmt(&dropping_name, false, bool_ty, dropping, span);
        let place = self.path(skipping, bool_ty, span);
        let stop_skipping = self.bool_lit(false, span);
        let passed = self.assign_stmt(place, stop_skipping, span);
        let cond = self.path(&dropping_name, bool_ty, span);
        (bind, cond, passed)
    }

    /// The element a read answers at `counter`: the counter itself for a
    /// range, `base[counter]` for a sequence, `base.byte_at(counter) as u8`
    /// for the bytes of a `String`.
    fn elem_value(&mut self, read: &ElemRead, counter: &str, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let index = self.path(counter, i64_ty, span);
        match read {
            ElemRead::Counter => index,
            ElemRead::Index(base, elem_ty) => {
                let base = self.reclone(base);
                self.expr(
                    *elem_ty,
                    span,
                    HirExprKind::Index {
                        base: Box::new(base),
                        index: Box::new(index),
                    },
                )
            }
            ElemRead::Byte(base) => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                let receiver = self.reclone(base);
                let byte = self.expr(
                    i64_ty,
                    span,
                    HirExprKind::MethodCall {
                        receiver: Box::new(receiver),
                        name: Ident::new("byte_at"),
                        args: vec![index],
                        owner: None,
                    },
                );
                self.expr(
                    u8_ty,
                    span,
                    HirExprKind::Cast {
                        value: Box::new(byte),
                        ty: u8_ty,
                    },
                )
            }
        }
    }

    /// `!value`
    fn not(&mut self, value: HirExpr, span: Span) -> HirExpr {
        let bool_ty = self.tcx.bool_ty();
        self.expr(
            bool_ty,
            span,
            HirExprKind::Unary {
                op: HirUnaryOp::Not,
                operand: Box::new(value),
            },
        )
    }

    /// `receiver.<name>()`
    fn method0(&mut self, receiver: &HirExpr, name: &str, ty: Ty, span: Span) -> HirExpr {
        let receiver = self.reclone(receiver);
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

    /// `Some(value)` of the option type `ty`.
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

    /// The bindings a terminal keeps beside its accumulator: the position a
    /// `position` counts, and the best element a `min` / `max` holds.
    fn terminal_bindings(
        &mut self,
        terminal: &Terminal,
        acc: &str,
        result_ty: Ty,
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        match terminal {
            Terminal::Position(_) => {
                let zero = self.int_lit(0, i64_ty, span);
                let s = self.let_stmt(&format!("{acc}_pos"), true, i64_ty, zero, span);
                stmts.push(s);
            }
            Terminal::Min | Terminal::Max => {
                let Some(elem_ty) = option_payload(self.tcx, result_ty) else {
                    return;
                };
                let zero = self.int_lit(0, elem_ty, span);
                let s = self.let_stmt(&format!("{acc}_best"), true, elem_ty, zero, span);
                stmts.push(s);
                let bool_ty = self.tcx.bool_ty();
                let no = self.bool_lit(false, span);
                let s = self.let_stmt(&format!("{acc}_any"), true, bool_ty, no, span);
                stmts.push(s);
            }
            _ => {}
        }
    }

    /// `let v = (first, second)`, answering the statement and a read of `v`.
    fn bind_pair(&mut self, first: HirExpr, second: HirExpr, span: Span) -> (HirStmt, HirExpr) {
        let ty = self.tcx.intern(TyKind::Tuple(vec![first.ty, second.ty]));
        let pair = self.expr(ty, span, HirExprKind::Tuple(vec![first, second]));
        let name = self.temp_name("v");
        let stmt = self.let_stmt(&name, false, ty, pair, span);
        (stmt, self.path(&name, ty, span))
    }

    /// `name = name <op> 1`
    fn step_stmt(&mut self, name: &str, op: HirBinaryOp, span: Span) -> HirStmt {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let read = self.path(name, i64_ty, span);
        let one = self.int_lit(1, i64_ty, span);
        let value = self.binary(op, read, one, i64_ty, span);
        let place = self.path(name, i64_ty, span);
        self.assign_stmt(place, value, span)
    }

    /// `if cond { break }` on the fused loop.
    fn break_if(&mut self, cond: HirExpr, span: Span) -> HirStmt {
        let unit_ty = self.tcx.unit();
        let brk = self.expr(
            unit_ty,
            span,
            HirExprKind::Break {
                value: None,
                label: None,
            },
        );
        let brk = self.expr_stmt(brk, span);
        let then = self.block(vec![brk], None, unit_ty, span);
        let if_expr = self.expr(
            unit_ty,
            span,
            HirExprKind::If {
                condition: Box::new(cond),
                then_branch: Box::new(then),
                else_branch: None,
            },
        );
        self.expr_stmt(if_expr, span)
    }

    /// `base.len()`
    fn len_call(&mut self, base: &HirExpr, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let receiver = self.reclone(base);
        self.expr(
            i64_ty,
            span,
            HirExprKind::MethodCall {
                receiver: Box::new(receiver),
                name: Ident::new("len"),
                args: Vec::new(),
                owner: None,
            },
        )
    }

    fn terminal_stmts(
        &mut self,
        terminal: &Terminal,
        elem: HirExpr,
        acc: &str,
        result_ty: Ty,
        span: Span,
    ) -> Vec<HirStmt> {
        let ty = result_ty;
        match terminal {
            Terminal::Sum => vec![self.acc_op(acc, elem, HirBinaryOp::Add, ty, span)],
            Terminal::Product => vec![self.acc_op(acc, elem, HirBinaryOp::Mul, ty, span)],
            Terminal::Count => {
                let one = self.int_lit(1, ty, span);
                vec![self.acc_op(acc, one, HirBinaryOp::Add, ty, span)]
            }
            Terminal::CountBy(pred) => {
                let cond = self.inline1(pred, elem, span);
                let one = self.int_lit(1, ty, span);
                let bump = self.acc_op(acc, one, HirBinaryOp::Add, ty, span);
                vec![self.if_stmt(cond, vec![bump], None, span)]
            }
            Terminal::SumBy(f) => {
                let mapped = self.inline1(f, elem, span);
                vec![self.acc_op(acc, mapped, HirBinaryOp::Add, ty, span)]
            }
            Terminal::ProductBy(f) => {
                let mapped = self.inline1(f, elem, span);
                vec![self.acc_op(acc, mapped, HirBinaryOp::Mul, ty, span)]
            }
            Terminal::Fold(_, f) => {
                let acc_expr = self.path(acc, ty, span);
                let folded = self.inline2(f, acc_expr, elem, span);
                let place = self.path(acc, ty, span);
                vec![self.assign_stmt(place, folded, span)]
            }
            Terminal::ForEach(f) => {
                let call = self.inline1(f, elem, span);
                vec![self.expr_stmt(call, span)]
            }
            Terminal::Any(p) => self.short_circuit(p, elem, acc, true, span),
            Terminal::All(p) => self.short_circuit(p, elem, acc, false, span),
            Terminal::Collect => {
                let unit_ty = self.tcx.unit();
                let receiver = self.path(acc, ty, span);
                let push = self.expr(
                    unit_ty,
                    span,
                    HirExprKind::MethodCall {
                        receiver: Box::new(receiver),
                        name: Ident::new("push"),
                        args: vec![elem],
                        owner: None,
                    },
                );
                vec![self.expr_stmt(push, span)]
            }
            Terminal::Find(p) => {
                let elem_for_pred = self.reclone(&elem);
                let holds = self.inline1(p, elem_for_pred, span);
                let found = self.some(elem, ty, span);
                let place = self.path(acc, ty, span);
                let set = self.assign_stmt(place, found, span);
                vec![self.set_and_break(holds, set, span)]
            }
            Terminal::Position(p) => {
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                let counter = format!("{acc}_pos");
                let holds = self.inline1(p, elem, span);
                let at = self.path(&counter, i64_ty, span);
                let found = self.some(at, ty, span);
                let place = self.path(acc, ty, span);
                let set = self.assign_stmt(place, found, span);
                let stop = self.set_and_break(holds, set, span);
                let up = self.step_stmt(&counter, HirBinaryOp::Add, span);
                vec![stop, up]
            }
            Terminal::Min | Terminal::Max => {
                self.extreme_stmts(matches!(terminal, Terminal::Min), elem, acc, ty, span)
            }
            Terminal::Loop { pat, body, .. } => {
                let bind = HirStmt {
                    id: self.ids.next(),
                    span,
                    kind: HirStmtKind::Let {
                        pattern: pat.clone(),
                        ty: pat.ty,
                        init: Some(elem),
                    },
                };
                let body = self.expr_stmt(body.clone(), span);
                let unit_ty = self.tcx.unit();
                let turn = self.block(vec![bind, body], None, unit_ty, span);
                vec![self.expr_stmt(turn, span)]
            }
        }
    }

    /// `min` / `max`: keeps the best element seen so far and answers its `Some`.
    fn extreme_stmts(
        &mut self,
        min: bool,
        elem: HirExpr,
        acc: &str,
        ty: Ty,
        span: Span,
    ) -> Vec<HirStmt> {
        let bool_ty = self.tcx.bool_ty();
        let elem_ty = elem.ty;
        let best = format!("{acc}_best");
        let any = format!("{acc}_any");
        // The first smallest and the last largest win, as the runtime's.
        let op = if min {
            HirBinaryOp::Lt
        } else {
            HirBinaryOp::Ge
        };
        let candidate = self.reclone(&elem);
        let held = self.path(&best, elem_ty, span);
        let better = self.binary(op, candidate, held, bool_ty, span);
        let seen = self.path(&any, bool_ty, span);
        let yes = self.bool_lit(true, span);
        let compared = self.block(Vec::new(), Some(better), bool_ty, span);
        let first = self.block(Vec::new(), Some(yes), bool_ty, span);
        let wins = self.if_expr(seen, compared, Some(first), bool_ty, span);
        let best_place = self.path(&best, elem_ty, span);
        let keep = self.reclone(&elem);
        let set_best = self.assign_stmt(best_place, keep, span);
        let any_place = self.path(&any, bool_ty, span);
        let yes = self.bool_lit(true, span);
        let set_any = self.assign_stmt(any_place, yes, span);
        let found = self.some(elem, ty, span);
        let acc_place = self.path(acc, ty, span);
        let set_acc = self.assign_stmt(acc_place, found, span);
        vec![self.if_stmt(wins, vec![set_best, set_any, set_acc], None, span)]
    }

    /// `if cond { <set>; break }`
    fn set_and_break(&mut self, cond: HirExpr, set: HirStmt, span: Span) -> HirStmt {
        let unit_ty = self.tcx.unit();
        let brk = self.expr(
            unit_ty,
            span,
            HirExprKind::Break {
                value: None,
                label: None,
            },
        );
        let brk = self.expr_stmt(brk, span);
        let then = self.block(vec![set, brk], None, unit_ty, span);
        let if_expr = self.expr(
            unit_ty,
            span,
            HirExprKind::If {
                condition: Box::new(cond),
                then_branch: Box::new(then),
                else_branch: None,
            },
        );
        self.expr_stmt(if_expr, span)
    }

    /// `if [!]pred(elem) { acc = <target>; break }` for `any` / `all`.
    fn short_circuit(
        &mut self,
        pred: &HirExpr,
        elem: HirExpr,
        acc: &str,
        is_any: bool,
        span: Span,
    ) -> Vec<HirStmt> {
        let bool_ty = self.tcx.bool_ty();
        let unit_ty = self.tcx.unit();
        let mut cond = self.inline1(pred, elem, span);
        if !is_any {
            cond = self.expr(
                bool_ty,
                span,
                HirExprKind::Unary {
                    op: HirUnaryOp::Not,
                    operand: Box::new(cond),
                },
            );
        }
        let target = self.bool_lit(is_any, span);
        let place = self.path(acc, bool_ty, span);
        let set = self.assign_stmt(place, target, span);
        let brk_expr = self.expr(
            unit_ty,
            span,
            HirExprKind::Break {
                value: None,
                label: None,
            },
        );
        let brk = self.expr_stmt(brk_expr, span);
        let then = self.block(vec![set, brk], None, unit_ty, span);
        let if_expr = self.expr(
            unit_ty,
            span,
            HirExprKind::If {
                condition: Box::new(cond),
                then_branch: Box::new(then),
                else_branch: None,
            },
        );
        vec![self.expr_stmt(if_expr, span)]
    }

    fn acc_init(&mut self, terminal: &Terminal, ty: Ty, span: Span) -> HirExpr {
        let float = matches!(self.tcx.kind_of(ty), TyKind::Float(_));
        match terminal {
            Terminal::Any(_) => self.bool_lit(false, span),
            Terminal::All(_) => self.bool_lit(true, span),
            Terminal::Product | Terminal::ProductBy(_) if float => self.float_lit("1.0", ty, span),
            Terminal::Product | Terminal::ProductBy(_) => self.int_lit(1, ty, span),
            Terminal::Fold(init, _) => init.clone(),
            Terminal::Find(_) | Terminal::Position(_) | Terminal::Min | Terminal::Max => {
                self.path("None", ty, span)
            }
            Terminal::Collect => {
                self.expr(ty, span, HirExprKind::Array(HirArrayExpr::List(Vec::new())))
            }
            _ if float => self.float_lit("0.0", ty, span),
            _ => self.int_lit(0, ty, span),
        }
    }

    /// `acc = acc <op> value`
    fn acc_op(
        &mut self,
        acc: &str,
        value: HirExpr,
        op: HirBinaryOp,
        ty: Ty,
        span: Span,
    ) -> HirStmt {
        let acc_read = self.path(acc, ty, span);
        let new = self.binary(op, acc_read, value, ty, span);
        let place = self.path(acc, ty, span);
        self.assign_stmt(place, new, span)
    }

    /// Inlines a single-argument closure applied to `arg` as
    /// `{ let <param> = arg; <body> }`. `lower_path` resolves locals by
    /// name, so the cloned body's references to the parameter bind to the
    /// `let`; captured names stay resolved in the enclosing scope.
    fn inline1(&mut self, closure: &HirExpr, arg: HirExpr, span: Span) -> HirExpr {
        let HirExprKind::Closure { params, body, .. } = &closure.kind else {
            return arg;
        };
        let body = (**body).clone();
        let ty = body.ty;
        let stmt = self.param_let(&params[0], arg, span);
        self.block(vec![stmt], Some(body), ty, span)
    }

    fn inline2(&mut self, closure: &HirExpr, a: HirExpr, b: HirExpr, span: Span) -> HirExpr {
        let HirExprKind::Closure { params, body, .. } = &closure.kind else {
            return b;
        };
        let body = (**body).clone();
        let ty = body.ty;
        let s0 = self.param_let(&params[0], a, span);
        let s1 = self.param_let(&params[1], b, span);
        self.block(vec![s0, s1], Some(body), ty, span)
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

    // ----- node constructors -----

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

    fn float_lit(&mut self, text: &str, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Literal(HirLiteral::Float(text.to_string())),
        )
    }

    fn bool_lit(&mut self, v: bool, span: Span) -> HirExpr {
        let ty = self.tcx.bool_ty();
        self.expr(ty, span, HirExprKind::Literal(HirLiteral::Bool(v)))
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
        format!("__fuse_{tag}_{n}")
    }

    /// A fresh clone of an element path so it can be used at more than one
    /// site.
    fn reclone(&mut self, e: &HirExpr) -> HirExpr {
        let mut c = e.clone();
        c.id = self.ids.next();
        c
    }
}

/// Rewrites the block a closure `|>` step lowers to back into the call it
/// stands for: `{ let v = piped; iter::name(a.., v) }` becomes
/// `iter::name(a.., piped)`.
///
/// Recognition below reads one shape - a nest of `iter::` calls - and a
/// closure step reaches the same pipeline through a binding. Only a step
/// whose binding is read exactly once, as the data argument, is rewritten:
/// the other arguments move out of the block, so they must not mention the
/// binding.
fn unwrap_pipe_step_block(expr: &mut HirExpr) {
    let HirExprKind::Block(block) = &expr.kind else {
        return;
    };
    let ([binding], Some(tail)) = (block.stmts.as_slice(), block.tail.as_ref()) else {
        return;
    };
    let HirStmtKind::Let {
        pattern,
        init: Some(_),
        ..
    } = &binding.kind
    else {
        return;
    };
    let HirPatKind::Binding { name, .. } = &pattern.kind else {
        return;
    };
    let HirExprKind::Call { callee, args } = &tail.kind else {
        return;
    };
    let Some((data, lead)) = args.split_last() else {
        return;
    };
    if as_iter_call(tail).is_none() || !is_binding_path(data, &name.name) {
        return;
    }
    let bound: HashSet<String> = HashSet::new();
    let shadowed: HashSet<String> = HashSet::new();
    if lead
        .iter()
        .any(|arg| collect_free_vars(arg, &bound, &shadowed).contains(&name.name))
    {
        return;
    }
    let callee = callee.clone();
    let mut new_args = lead.to_vec();
    let HirExprKind::Block(block) = &mut expr.kind else {
        return;
    };
    let HirStmtKind::Let { init, .. } = &mut block.stmts[0].kind else {
        return;
    };
    let Some(piped) = init.take() else {
        return;
    };
    new_args.push(piped);
    expr.kind = HirExprKind::Call {
        callee,
        args: new_args,
    };
}

/// Whether `expr` is a bare reference to the binding `name`.
fn is_binding_path(expr: &HirExpr, name: &str) -> bool {
    let HirExprKind::Path { segments, .. } = &expr.kind else {
        return false;
    };
    segments.len() == 1 && segments[0].name == name
}

/// Matches `iter::<name>(args...)`, returning the trailing name and args.
fn as_iter_call(expr: &HirExpr) -> Option<(&str, &[HirExpr])> {
    let HirExprKind::Call { callee, args } = &expr.kind else {
        return None;
    };
    let HirExprKind::Path { segments, .. } = &callee.kind else {
        return None;
    };
    if segments.len() != 2 || segments[0].name != "iter" {
        return None;
    }
    Some((segments[1].name.as_str(), args.as_slice()))
}

/// Accepts an inline single-parameter closure with a plain binding
/// parameter, the only shape the inliner can splice.
fn one_closure(expr: &HirExpr) -> Option<&HirExpr> {
    n_closure(expr, 1)
}

fn n_closure(expr: &HirExpr, n: usize) -> Option<&HirExpr> {
    let HirExprKind::Closure { params, body, .. } = &expr.kind else {
        return None;
    };
    if params.len() != n {
        return None;
    }
    let simple = |p: &HirPat| matches!(p.kind, HirPatKind::Binding { .. } | HirPatKind::Wildcard);
    if params.iter().any(|p| {
        !(simple(&p.pattern)
            || matches!(&p.pattern.kind, HirPatKind::Tuple(items) if items.iter().all(simple)))
    }) {
        return None;
    }
    // Splicing the body into the loop must not move control flow that
    // targets the closure out to the enclosing function or the fused
    // loop: a `return` (also what `?` desugars to) or a loop-level
    // `break` / `continue` disqualifies it.
    if !inline_safe(body, 0) {
        return None;
    }
    Some(expr)
}

/// Whether `expr` can be spliced inline without changing which construct
/// its control flow targets. `loop_depth` counts loops entered *within*
/// the body, so a `break` inside a nested loop is local and safe. Nested
/// closures are opaque boundaries - their control flow stays local.
pub(crate) fn inline_safe(expr: &HirExpr, loop_depth: u32) -> bool {
    match &expr.kind {
        HirExprKind::Return(_) => false,
        HirExprKind::Break { .. } | HirExprKind::Continue { .. } => loop_depth > 0,
        HirExprKind::Closure { .. } | HirExprKind::LiftedClosure { .. } => true,
        HirExprKind::Literal(_) | HirExprKind::Path { .. } | HirExprKind::Placeholder => true,
        HirExprKind::Call { callee, args } => {
            inline_safe(callee, loop_depth) && args.iter().all(|a| inline_safe(a, loop_depth))
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            inline_safe(receiver, loop_depth) && args.iter().all(|a| inline_safe(a, loop_depth))
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            inline_safe(receiver, loop_depth)
        }
        HirExprKind::Index { base, index } => {
            inline_safe(base, loop_depth) && inline_safe(index, loop_depth)
        }
        HirExprKind::Unary { operand, .. } => inline_safe(operand, loop_depth),
        HirExprKind::Binary { lhs, rhs, .. } => {
            inline_safe(lhs, loop_depth) && inline_safe(rhs, loop_depth)
        }
        HirExprKind::Assign { place, value } => {
            inline_safe(place, loop_depth) && inline_safe(value, loop_depth)
        }
        HirExprKind::Cast { value, .. } => inline_safe(value, loop_depth),
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            inline_safe(condition, loop_depth)
                && inline_safe(then_branch, loop_depth)
                && else_branch
                    .as_ref()
                    .is_none_or(|e| inline_safe(e, loop_depth))
        }
        HirExprKind::Match { scrutinee, arms } => {
            inline_safe(scrutinee, loop_depth)
                && arms.iter().all(|a| {
                    a.guard.as_ref().is_none_or(|g| inline_safe(g, loop_depth))
                        && inline_safe(&a.body, loop_depth)
                })
        }
        HirExprKind::Loop { body, .. } => inline_safe(body, loop_depth + 1),
        HirExprKind::While {
            condition, body, ..
        } => inline_safe(condition, loop_depth) && inline_safe(body, loop_depth + 1),
        HirExprKind::Block(b) => {
            b.stmts.iter().all(|s| match &s.kind {
                HirStmtKind::Let { init, .. } => {
                    init.as_ref().is_none_or(|e| inline_safe(e, loop_depth))
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                    inline_safe(expr, loop_depth)
                }
                HirStmtKind::Item(_) => true,
            }) && b.tail.as_ref().is_none_or(|t| inline_safe(t, loop_depth))
        }
        HirExprKind::Tuple(items) => items.iter().all(|e| inline_safe(e, loop_depth)),
        HirExprKind::Array(HirArrayExpr::List(items)) => {
            items.iter().all(|e| inline_safe(e, loop_depth))
        }
        HirExprKind::Array(HirArrayExpr::Repeat { value, count }) => {
            inline_safe(value, loop_depth) && inline_safe(count, loop_depth)
        }
        HirExprKind::Range { start, end, .. } => {
            start.as_ref().is_none_or(|e| inline_safe(e, loop_depth))
                && end.as_ref().is_none_or(|e| inline_safe(e, loop_depth))
        }
        // Spawning / selecting inside a fused inline body is outside the
        // shapes this pass reasons about; be conservative.
        HirExprKind::Select { .. } => false,
    }
}

fn is_i64(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(tcx.kind_of(ty), TyKind::Int(IntTy::I64))
}

/// Integers and floats: the element types a sum or product accumulates.
fn is_number(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(tcx.kind_of(ty), TyKind::Int(_) | TyKind::Float(_))
}

/// Values a fused loop moves without taking a share: numbers, `bool`, `char`.
fn is_scalar(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind_of(ty),
        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
    )
}

/// The payload type of an `Option`.
fn option_payload(tcx: &TyCtxt, ty: Ty) -> Option<Ty> {
    match tcx.kind_of(ty) {
        TyKind::Adt { def, substs } if def.local == u32::MAX - 1 => substs.types().first().copied(),
        _ => None,
    }
}

/// Elements a fused loop may carry: plain values, and a type parameter. A
/// generic body's fused loop is instantiated like the rest of the body, so
/// each instantiation reads and hands on its elements with the ownership
/// the concrete type takes, exactly as the loop written by hand would.
fn is_walked_elem(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind_of(ty) {
        TyKind::Param { .. } => true,
        TyKind::Tuple(items) => items.iter().all(|item| is_walked_elem(tcx, *item)),
        _ => is_scalar(tcx, ty),
    }
}

/// An `i64` range with both ends, walked forwards.
fn range_source(tcx: &TyCtxt, expr: &HirExpr) -> Option<Source> {
    let HirExprKind::Range {
        start: Some(start),
        end: Some(end),
        inclusive,
    } = &expr.kind
    else {
        return None;
    };
    if !is_i64(tcx, start.ty) || !is_i64(tcx, end.ty) {
        return None;
    }
    Some(Source::Range {
        start: (**start).clone(),
        end: (**end).clone(),
        inclusive: *inclusive,
        reversed: false,
    })
}

/// An integer literal count of at least `min`. A computed count keeps the
/// runtime adapter, which checks it when the chain is built and panics on one
/// out of range.
fn count_literal(expr: &HirExpr, min: i64) -> Option<i64> {
    let HirExprKind::Literal(HirLiteral::Int(text)) = &expr.kind else {
        return None;
    };
    let value: i64 = text.replace('_', "").parse().ok()?;
    (value >= min).then_some(value)
}

/// Whether a fused eager chain calls its closures as the eager stages do.
/// Each eager stage runs over every element before the next stage starts, so
/// a second closure would interleave with the first, and a stage or terminal
/// that stops early would skip calls a closure stage before it still makes.
fn eager_order_kept(stages: &[Stage], terminal: &Terminal) -> bool {
    let has_closure = |stage: &&Stage| {
        matches!(
            stage,
            Stage::Map(_)
                | Stage::Filter(_)
                | Stage::TakeWhile(_)
                | Stage::SkipWhile(_)
                | Stage::FilterMap(_)
        )
    };
    let closure_at = stages.iter().position(|stage| has_closure(&stage));
    let stage_closures = stages.iter().filter(has_closure).count();
    let terminal_closure = !matches!(
        terminal,
        Terminal::Sum
            | Terminal::Product
            | Terminal::Count
            | Terminal::Min
            | Terminal::Max
            | Terminal::Collect
    );
    if stage_closures + usize::from(terminal_closure) > 1 {
        return false;
    }
    let Some(at) = closure_at else {
        return true;
    };
    let stops_later = stages[at + 1..]
        .iter()
        .any(|stage| matches!(stage, Stage::Take(_) | Stage::Zip(_) | Stage::TakeWhile(_)));
    !stops_later
        && !matches!(
            terminal,
            Terminal::Any(_) | Terminal::All(_) | Terminal::Find(_) | Terminal::Position(_)
        )
}

fn map_closure_returns_i64(tcx: &TyCtxt, closure: &HirExpr) -> bool {
    if let HirExprKind::Closure { body, .. } = &closure.kind {
        is_i64(tcx, body.ty)
    } else {
        false
    }
}

/// The terminal a chain ends in, for the method name and arguments that name one.
fn chain_terminal(name: &str, args: &[HirExpr]) -> Option<Terminal> {
    Some(match (name, args.len()) {
        ("sum", 0) => Terminal::Sum,
        ("product", 0) => Terminal::Product,
        ("count", 0) => Terminal::Count,
        ("count", 1) => Terminal::CountBy(one_closure(&args[0])?.clone()),
        ("fold", 2) => Terminal::Fold(args[0].clone(), n_closure(&args[1], 2)?.clone()),
        ("for_each", 1) => Terminal::ForEach(one_closure(&args[0])?.clone()),
        ("any", 1) => Terminal::Any(one_closure(&args[0])?.clone()),
        ("all", 1) => Terminal::All(one_closure(&args[0])?.clone()),
        ("find", 1) => Terminal::Find(one_closure(&args[0])?.clone()),
        ("position", 1) => Terminal::Position(one_closure(&args[0])?.clone()),
        ("min", 0) => Terminal::Min,
        ("max", 0) => Terminal::Max,
        ("collect", 0) => Terminal::Collect,
        _ => return None,
    })
}

/// Whether a closure of the chain names the sequence it walks. Such a closure
/// could change the sequence while the loop reads it; the runtime iterator
/// reports that and a plain index loop would not, so the chain keeps the
/// runtime path. `None` when the base is not a binding.
fn closures_name_base(base: &HirExpr, stages: &[Stage], terminal: &Terminal) -> Option<bool> {
    let HirExprKind::Path { segments, .. } = &base.kind else {
        return None;
    };
    let base_name = segments[0].name.clone();
    let bound: HashSet<String> = HashSet::new();
    let shadowed: HashSet<String> = HashSet::new();
    let mut closures: Vec<&HirExpr> = stages
        .iter()
        .filter_map(|stage| match stage {
            Stage::Filter(c)
            | Stage::Map(c)
            | Stage::TakeWhile(c)
            | Stage::SkipWhile(c)
            | Stage::FilterMap(c) => Some(c),
            _ => None,
        })
        .collect();
    match terminal {
        Terminal::Fold(init, c) => {
            closures.push(init);
            closures.push(c);
        }
        Terminal::ForEach(c)
        | Terminal::Any(c)
        | Terminal::All(c)
        | Terminal::CountBy(c)
        | Terminal::SumBy(c)
        | Terminal::ProductBy(c)
        | Terminal::Find(c)
        | Terminal::Position(c)
        | Terminal::Loop { body: c, .. } => closures.push(c),
        Terminal::Sum
        | Terminal::Product
        | Terminal::Count
        | Terminal::Min
        | Terminal::Max
        | Terminal::Collect => {}
    }
    Some(
        closures
            .iter()
            .any(|c| collect_free_vars(c, &bound, &shadowed).contains(&base_name)),
    )
}
