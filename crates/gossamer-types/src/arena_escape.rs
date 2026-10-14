//! Static enforcement of the `arena { }` escape contract.
//!
//! An `arena { }` block bump-allocates everything created while it runs and
//! frees the whole arena at the close brace. The contract is that nothing
//! allocated inside may be referenced after the block exits - violating it is
//! a use-after-free. This pass turns that contract from a documented promise
//! into a compile error: it walks every `arena { }` block, tracks which
//! values are arena-allocated (region-local), and reports a value that
//! reaches a sink able to outlive the block.
//!
//! Soundness is the goal: the analysis over-approximates region-locality and
//! under-approximates the safe shapes, so it may reject a sound program but
//! never accepts an escaping one within the sinks it models (assigning to a
//! binding that outlives the block, pushing into an outer container, sending
//! on a channel, returning, breaking out of an enclosing loop, capturing in a
//! goroutine/closure, or passing into a function that may stash the value).
//! The raw `runtime::arena_push()` / `arena_pop()` primitive is intentionally
//! left unchecked; only the `arena { }` surface (marked on the AST block)
//! carries this guarantee.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::fmt;

use gossamer_ast::{
    ArrayExpr, Block, Expr, ExprKind, FnDecl, FnParam, Item, ItemKind, Pattern, PatternKind,
    SelectOp, SourceFile, StmtKind,
};
use gossamer_lex::Span;
use gossamer_resolve::{DefId, DefKind, Resolution, Resolutions};
use thiserror::Error;

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::{Ty, TyKind};

/// Method names that mutate their receiver in place, so a region-local
/// argument can be stashed into a receiver that outlives the block.
const MUTATOR_METHODS: &[&str] = &[
    "push",
    "pop",
    "insert",
    "remove",
    "append",
    "extend",
    "swap",
    "sort",
    "sort_by",
    "retain",
    "set",
    "inc",
    "or_insert",
    "push_str",
    "drain",
];

/// Walks every `arena { }` block in `source` and reports values allocated
/// inside that escape the block.
#[must_use]
pub fn check_arena_escapes(
    source: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<ArenaEscapeDiagnostic> {
    let unsafe_fns = collect_region_unsafe_fns(source, resolutions, table, tcx);
    let mut finder = Finder {
        resolutions,
        table,
        tcx,
        unsafe_fns: &unsafe_fns,
        diags: Vec::new(),
    };
    finder.walk_items(&source.items);
    dedup(finder.diags)
}

/// Drops diagnostics that share a span and kind, which nested arena blocks
/// can produce when an inner escape is visible to both the inner and the
/// enclosing block's analysis.
fn dedup(diags: Vec<ArenaEscapeDiagnostic>) -> Vec<ArenaEscapeDiagnostic> {
    let mut seen: HashSet<(Span, ArenaEscapeKind)> = HashSet::new();
    let mut out = Vec::with_capacity(diags.len());
    for d in diags {
        if seen.insert((d.span, d.error.kind)) {
            out.push(d);
        }
    }
    out
}

/// True for types whose values carry no heap ownership - copying or dropping
/// them frees nothing, so they can leave a region freely.
fn is_copy_ty(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind(ty),
        Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Unit)
    )
}

/// Peels `&` / `&mut` / `*` wrappers to the inner place: a reference escapes
/// exactly when its referent does.
fn peel_refs(expr: &Expr) -> &Expr {
    let mut cur = expr;
    while let ExprKind::Unary { operand, .. } = &cur.kind {
        cur = operand;
    }
    cur
}

/// Root path name of a place expression (`x`, `*x`, `x.f`, `x[i]`, `&x`).
/// Root binding names an assignment writes through. A tuple on the left is a
/// destructuring target, so each element contributes its own root.
fn assign_roots(place: &Expr) -> Box<dyn Iterator<Item = &str> + '_> {
    match &place.kind {
        ExprKind::Tuple(elems) => Box::new(elems.iter().flat_map(assign_roots)),
        _ => Box::new(root_name(place).into_iter()),
    }
}

/// The path expression a place is read through, after references,
/// fields, and indexing.
fn root_expr(expr: &Expr) -> Option<&Expr> {
    match &expr.kind {
        ExprKind::Path(_) => Some(expr),
        ExprKind::Unary { operand, .. } => root_expr(operand),
        ExprKind::FieldAccess { receiver, .. } => root_expr(receiver),
        ExprKind::Index { base, .. } => root_expr(base),
        _ => None,
    }
}

fn root_name(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Path(path) => path.segments.first().map(|seg| seg.name.name.as_str()),
        ExprKind::Unary { operand, .. } => root_name(operand),
        ExprKind::FieldAccess { receiver, .. } => root_name(receiver),
        ExprKind::Index { base, .. } => root_name(base),
        _ => None,
    }
}

/// Collects every identifier a pattern binds, walking sub-patterns.
fn pat_binding_names(pat: &Pattern, out: &mut Vec<String>) {
    match &pat.kind {
        PatternKind::Ident {
            name, subpattern, ..
        } => {
            out.push(name.name.clone());
            if let Some(sub) = subpattern {
                pat_binding_names(sub, out);
            }
        }
        PatternKind::Tuple(parts) | PatternKind::TupleStruct { elems: parts, .. } => {
            for p in parts {
                pat_binding_names(p, out);
            }
        }
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for p in prefix {
                pat_binding_names(p, out);
            }
            if let Some(rest) = rest {
                pat_binding_names(rest, out);
            }
            for p in suffix {
                pat_binding_names(p, out);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for f in fields {
                match &f.pattern {
                    Some(p) => pat_binding_names(p, out),
                    None => out.push(f.name.name.clone()),
                }
            }
        }
        PatternKind::Ref { inner, .. } => pat_binding_names(inner, out),
        // Every arm of an or-pattern binds the same names; one arm suffices.
        PatternKind::Or(alts) => {
            if let Some(first) = alts.first() {
                pat_binding_names(first, out);
            }
        }
        PatternKind::Wildcard
        | PatternKind::Literal(_)
        | PatternKind::Path(_)
        | PatternKind::Range { .. }
        | PatternKind::Rest
        | PatternKind::Error => {}
    }
}

/// Parameter binding names of a function (including a `self` receiver).
/// Each name a function's parameters bind, with the position of the
/// parameter that binds it.
fn param_positions(decl: &FnDecl) -> HashMap<String, usize> {
    let mut names = HashMap::new();
    for (position, param) in decl.params.iter().enumerate() {
        match param {
            FnParam::Receiver(_) => {
                names.insert("self".to_string(), position);
            }
            FnParam::Typed { pattern, .. } => {
                let mut bound = Vec::new();
                pat_binding_names(pattern, &mut bound);
                for name in bound {
                    names.insert(name, position);
                }
            }
        }
    }
    names
}

/// How a free function may let an argument outlive the call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StashSummary {
    /// It spawns, selects, writes a static, or hands an argument to a place
    /// it cannot name: any argument may escape anywhere.
    global: bool,
    /// The parameters it may store an argument into. An argument escapes
    /// through one of them only when the place passed there outlives the
    /// caller's region.
    sinks: HashSet<usize>,
}

impl StashSummary {
    fn stashes(&self) -> bool {
        self.global || !self.sinks.is_empty()
    }
}

/// Where a call argument's value lives, as the function making the call
/// sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgRoot {
    /// The function's own parameter at this position.
    Param(usize),
    /// A binding of the function's own body.
    Local,
    /// A static, or an expression with no root binding.
    Other,
}

/// Free functions that may let an argument escape beyond their own return:
/// they spawn goroutines, touch `select`, write a static, or stash a value
/// through a parameter, recording which parameters receive the stash.
/// Computed as a fixed point over the static call graph, so a function that
/// passes its parameter to another's receiving parameter receives through
/// it too.
/// Methods and associated functions are not in the graph (method dispatch is
/// name-global at this stage); calls through them are vetted conservatively at
/// the call site instead.
fn collect_region_unsafe_fns(
    source: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> HashMap<DefId, StashSummary> {
    let mut fns: Vec<(DefId, HashMap<String, usize>, &Expr)> = Vec::new();
    collect_fns(&source.items, resolutions, &mut fns);

    let mut summaries: HashMap<DefId, StashSummary> = HashMap::new();
    let mut calls: HashMap<DefId, Vec<(DefId, Vec<ArgRoot>)>> = HashMap::new();
    for (def, params, body) in &fns {
        let mut scan = Scan {
            resolutions,
            table,
            tcx,
            params,
            summary: StashSummary::default(),
            calls: Vec::new(),
        };
        scan.expr(body);
        summaries.insert(*def, scan.summary);
        calls.insert(*def, scan.calls);
    }

    loop {
        let mut changed = false;
        for (def, sites) in &calls {
            let mut grown = summaries.get(def).cloned().unwrap_or_default();
            for (callee, roots) in sites {
                let Some(callee_summary) = summaries.get(callee) else {
                    continue;
                };
                grown.global |= callee_summary.global;
                for &sink in &callee_summary.sinks {
                    match roots.get(sink).copied().unwrap_or(ArgRoot::Other) {
                        ArgRoot::Param(position) => {
                            grown.sinks.insert(position);
                        }
                        ArgRoot::Local => {}
                        ArgRoot::Other => grown.global = true,
                    }
                }
            }
            if summaries.get(def) != Some(&grown) {
                summaries.insert(*def, grown);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    summaries.retain(|_, summary| summary.stashes());
    summaries
}

/// Gathers every free-function body with its `DefId` and parameter names,
/// descending into inline modules.
fn collect_fns<'a>(
    items: &'a [Item],
    resolutions: &Resolutions,
    out: &mut Vec<(DefId, HashMap<String, usize>, &'a Expr)>,
) {
    for item in items {
        match &item.kind {
            ItemKind::Fn(decl) => {
                if let (Some(def), Some(body)) = (resolutions.definition_of(item.id), &decl.body) {
                    out.push((def, param_positions(decl), body));
                }
            }
            ItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    collect_fns(inner, resolutions, out);
                }
            }
            _ => {}
        }
    }
}

/// Per-function scan that summarises how a function stashes its arguments
/// and records its resolved calls with where each argument lives.
struct Scan<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    params: &'a HashMap<String, usize>,
    summary: StashSummary,
    calls: Vec<(DefId, Vec<ArgRoot>)>,
}

impl Scan<'_> {
    fn block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.expr(tail);
        }
    }

    fn stmt(&mut self, stmt: &gossamer_ast::Stmt) {
        match &stmt.kind {
            StmtKind::Let { init, .. } => {
                if let Some(init) = init {
                    self.expr(init);
                }
            }
            StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => self.expr(expr),
            StmtKind::Item(_) => {}
        }
    }

    /// Records a store into the place rooted at `root`.
    fn stash_into(&mut self, root: &str) {
        if let Some(&position) = self.params.get(root) {
            self.summary.sinks.insert(position);
        }
    }

    /// Where the value `arg` names lives.
    fn arg_root(&self, arg: &Expr) -> ArgRoot {
        let Some(root) = root_expr(arg) else {
            return ArgRoot::Other;
        };
        let ExprKind::Path(path) = &root.kind else {
            return ArgRoot::Other;
        };
        if path.segments.len() == 1
            && let Some(&position) = self.params.get(path.segments[0].name.name.as_str())
        {
            return ArgRoot::Param(position);
        }
        match self.resolutions.get(root.id) {
            Some(Resolution::Local(_)) => ArgRoot::Local,
            _ => ArgRoot::Other,
        }
    }

    fn non_copy(&self, expr: &Expr) -> bool {
        self.table
            .get(expr.id)
            .is_none_or(|ty| !is_copy_ty(self.tcx, ty))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "exhaustive per-ExprKind walker; one arm per variant, splitting obscures the dispatch"
    )]
    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Select(arms) => {
                self.summary.global = true;
                for arm in arms {
                    match &arm.op {
                        SelectOp::Recv { channel, .. } => self.expr(channel),
                        SelectOp::Send { channel, value } => {
                            self.expr(channel);
                            self.expr(value);
                        }
                        SelectOp::Default => {}
                    }
                    self.expr(&arm.body);
                }
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Path(_) = &callee.kind {
                    if let Some(Resolution::Def {
                        def,
                        kind: DefKind::Fn,
                    }) = self.resolutions.get(callee.id)
                    {
                        let roots = args.iter().map(|a| self.arg_root(a)).collect();
                        self.calls.push((def, roots));
                    }
                }
                self.expr(callee);
                for a in args {
                    self.expr(a);
                }
            }
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                if MUTATOR_METHODS.contains(&name.name.as_str())
                    && let Some(root) = root_name(receiver)
                {
                    self.stash_into(root);
                }
                self.expr(receiver);
                for a in args {
                    self.expr(a);
                }
            }
            ExprKind::Assign { place, value, .. } => {
                if let Some(Resolution::Def {
                    kind: DefKind::Static,
                    ..
                }) = self.resolutions.get(place.id)
                {
                    self.summary.global = true;
                }
                if self.non_copy(value) {
                    let roots: Vec<&str> = assign_roots(place).collect();
                    for root in roots {
                        self.stash_into(root);
                    }
                }
                self.expr(place);
                self.expr(value);
            }
            ExprKind::FieldAccess { receiver, .. } => self.expr(receiver),
            ExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Unary { operand, .. } => self.expr(operand),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Cast { value, .. } => self.expr(value),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition);
                self.expr(then_branch);
                if let Some(e) = else_branch {
                    self.expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                }
            }
            ExprKind::Loop { body, .. } | ExprKind::Closure { body, .. } => self.expr(body),
            ExprKind::While {
                condition, body, ..
            } => {
                self.expr(condition);
                self.expr(body);
            }
            ExprKind::For { iter, body, .. } => {
                self.expr(iter);
                self.expr(body);
            }
            ExprKind::Block(b) | ExprKind::Unsafe(b) => self.block(b),
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
            }
            ExprKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.expr(v);
                }
            }
            ExprKind::Tuple(items) | ExprKind::MapLiteral(items) | ExprKind::SetLiteral(items) => {
                for i in items {
                    self.expr(i);
                }
            }
            ExprKind::Array(arr) | ExprKind::FixedArray(arr) => self.array(arr),
            ExprKind::Struct { fields, base, .. } => {
                for f in fields {
                    if let Some(v) = &f.value {
                        self.expr(v);
                    }
                }
                if let Some(b) = base {
                    self.expr(b);
                }
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.expr(s);
                }
                if let Some(e) = end {
                    self.expr(e);
                }
            }
            ExprKind::Try(inner) => self.expr(inner),
            ExprKind::Literal(_)
            | ExprKind::Path(_)
            | ExprKind::Continue { .. }
            | ExprKind::Error => {}
        }
    }

    fn array(&mut self, arr: &ArrayExpr) {
        match arr {
            ArrayExpr::List(items) => {
                for i in items {
                    self.expr(i);
                }
            }
            ArrayExpr::Repeat { value, count } => {
                self.expr(value);
                self.expr(count);
            }
        }
    }
}

/// Finds `arena { }` blocks anywhere in the program and runs the escape
/// analysis on each.
struct Finder<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    unsafe_fns: &'a HashMap<DefId, StashSummary>,
    diags: Vec<ArenaEscapeDiagnostic>,
}

impl Finder<'_> {
    fn walk_items(&mut self, items: &[Item]) {
        for item in items {
            match &item.kind {
                ItemKind::Fn(decl) => {
                    if let Some(body) = &decl.body {
                        self.walk_expr(body);
                    }
                }
                ItemKind::Impl(decl) => {
                    for impl_item in &decl.items {
                        if let gossamer_ast::ImplItem::Fn(fn_decl) = impl_item {
                            if let Some(body) = &fn_decl.body {
                                self.walk_expr(body);
                            }
                        }
                    }
                }
                ItemKind::Trait(decl) => {
                    for trait_item in &decl.items {
                        if let gossamer_ast::TraitItem::Fn(fn_decl) = trait_item {
                            if let Some(body) = &fn_decl.body {
                                self.walk_expr(body);
                            }
                        }
                    }
                }
                ItemKind::Const(decl) => self.walk_expr(&decl.value),
                ItemKind::Static(decl) => self.walk_expr(&decl.value),
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        self.walk_items(inner);
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_block(&mut self, block: &Block) {
        if block.is_arena() {
            let mut analyzer = Analyzer {
                resolutions: self.resolutions,
                table: self.table,
                tcx: self.tcx,
                unsafe_fns: self.unsafe_fns,
                block_locals: HashSet::new(),
                tainted: HashSet::new(),
                loop_depth: 0,
                diags: Vec::new(),
            };
            analyzer.block(block);
            self.diags.append(&mut analyzer.diags);
        }
        for stmt in &block.stmts {
            self.walk_stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.walk_expr(tail);
        }
    }

    fn walk_stmt(&mut self, stmt: &gossamer_ast::Stmt) {
        match &stmt.kind {
            StmtKind::Let { init, .. } => {
                if let Some(init) = init {
                    self.walk_expr(init);
                }
            }
            StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => {
                self.walk_expr(expr);
            }
            StmtKind::Item(item) => self.walk_items(std::slice::from_ref(item)),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "exhaustive per-ExprKind walker; one arm per variant, splitting obscures the dispatch"
    )]
    fn walk_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Block(block) | ExprKind::Unsafe(block) => self.walk_block(block),
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee);
                self.walk_exprs(args);
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.walk_expr(receiver);
                self.walk_exprs(args);
            }
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::Assign {
                place: lhs,
                value: rhs,
                ..
            }
            | ExprKind::Index {
                base: lhs,
                index: rhs,
            } => {
                self.walk_expr(lhs);
                self.walk_expr(rhs);
            }
            ExprKind::Unary { operand, .. }
            | ExprKind::FieldAccess {
                receiver: operand, ..
            }
            | ExprKind::Try(operand)
            | ExprKind::Cast { value: operand, .. } => self.walk_expr(operand),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.walk_expr(condition);
                self.walk_expr(then_branch);
                if let Some(e) = else_branch {
                    self.walk_expr(e);
                }
            }
            ExprKind::Loop { body, .. } | ExprKind::Closure { body, .. } => self.walk_expr(body),
            ExprKind::While {
                condition, body, ..
            } => {
                self.walk_expr(condition);
                self.walk_expr(body);
            }
            ExprKind::For { iter, body, .. } => {
                self.walk_expr(iter);
                self.walk_expr(body);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.walk_expr(g);
                    }
                    self.walk_expr(&arm.body);
                }
            }
            ExprKind::Return(v) | ExprKind::Break { value: v, .. } => {
                if let Some(v) = v {
                    self.walk_expr(v);
                }
            }
            ExprKind::Tuple(items) | ExprKind::MapLiteral(items) | ExprKind::SetLiteral(items) => {
                self.walk_exprs(items);
            }
            ExprKind::Array(arr) | ExprKind::FixedArray(arr) => match arr {
                ArrayExpr::List(items) => self.walk_exprs(items),
                ArrayExpr::Repeat { value, count } => {
                    self.walk_expr(value);
                    self.walk_expr(count);
                }
            },
            ExprKind::Struct { fields, base, .. } => {
                for f in fields {
                    if let Some(v) = &f.value {
                        self.walk_expr(v);
                    }
                }
                if let Some(b) = base {
                    self.walk_expr(b);
                }
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_expr(s);
                }
                if let Some(e) = end {
                    self.walk_expr(e);
                }
            }
            ExprKind::Select(arms) => {
                for arm in arms {
                    match &arm.op {
                        SelectOp::Recv { channel, .. } => self.walk_expr(channel),
                        SelectOp::Send { channel, value } => {
                            self.walk_expr(channel);
                            self.walk_expr(value);
                        }
                        SelectOp::Default => {}
                    }
                    self.walk_expr(&arm.body);
                }
            }
            ExprKind::Literal(_)
            | ExprKind::Path(_)
            | ExprKind::Continue { .. }
            | ExprKind::Error => {}
        }
    }

    fn walk_exprs(&mut self, exprs: &[Expr]) {
        for expr in exprs {
            self.walk_expr(expr);
        }
    }
}

/// Classification of a call's callee for region-escape purposes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CalleeClass<'s> {
    /// A vetted free function, a constructor, or a builtin/stdlib call.
    Safe,
    /// A free function that may stash an argument, and how.
    Unsafe(&'s StashSummary),
    /// An indirect call (through a local, static, or non-path callee) that
    /// cannot be vetted statically.
    Indirect,
}

/// Per-`arena`-block escape analysis.
struct Analyzer<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    unsafe_fns: &'a HashMap<DefId, StashSummary>,
    /// Names `let`-bound lexically within this arena block.
    block_locals: HashSet<String>,
    /// Subset of `block_locals` whose value is arena-allocated.
    tainted: HashSet<String>,
    /// Loops opened within the block, so a `break` value is judged against the
    /// loop it actually leaves.
    loop_depth: u32,
    diags: Vec<ArenaEscapeDiagnostic>,
}

impl<'a> Analyzer<'a> {
    fn flag(&mut self, span: Span, kind: ArenaEscapeKind) {
        self.diags
            .push(ArenaEscapeDiagnostic::new(ArenaEscapeError { kind }, span));
    }

    fn block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.expr(tail);
        }
    }

    fn stmt(&mut self, stmt: &gossamer_ast::Stmt) {
        match &stmt.kind {
            StmtKind::Let { pattern, init, .. } => {
                let region_local = if let Some(init) = init {
                    self.expr(init);
                    self.is_region_local(init)
                } else {
                    false
                };
                let mut names = Vec::new();
                pat_binding_names(pattern, &mut names);
                for name in names {
                    if region_local {
                        self.tainted.insert(name.clone());
                    }
                    self.block_locals.insert(name);
                }
            }
            StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => self.expr(expr),
            StmtKind::Item(_) => {}
        }
    }

    /// True when `expr` may evaluate to a value bump-allocated inside this
    /// arena block (an over-approximation; `false` is always safe).
    fn is_region_local(&self, expr: &Expr) -> bool {
        let inner = peel_refs(expr);
        if let Some(ty) = self.table.get(inner.id) {
            if is_copy_ty(self.tcx, ty) {
                return false;
            }
        }
        match &inner.kind {
            ExprKind::Path(path) => path
                .segments
                .first()
                .is_some_and(|seg| self.tainted.contains(&seg.name.name)),
            ExprKind::Literal(_) => false,
            ExprKind::FieldAccess { receiver, .. } => self.is_region_local(receiver),
            ExprKind::Index { base, .. } => self.is_region_local(base),
            ExprKind::Unary { operand, .. } | ExprKind::Cast { value: operand, .. } => {
                self.is_region_local(operand)
            }
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.is_region_local(then_branch)
                    || else_branch
                        .as_ref()
                        .is_some_and(|e| self.is_region_local(e))
            }
            ExprKind::Match { arms, .. } => arms.iter().any(|arm| self.is_region_local(&arm.body)),
            ExprKind::Block(b) => b.tail.as_ref().is_some_and(|t| self.is_region_local(t)),
            // Constructors, calls, and aggregate literals freshly allocate in
            // the arena; remaining non-Copy shapes are treated conservatively.
            _ => true,
        }
    }

    /// True when the place's root binding outlives this arena block, so
    /// storing an arena-allocated value into it escapes.
    fn escapes_outer(&self, place: &Expr) -> bool {
        root_name(place).is_none_or(|root| !self.block_locals.contains(root))
    }

    fn classify_callee(&self, callee: &Expr) -> CalleeClass<'a> {
        let ExprKind::Path(_) = &callee.kind else {
            return CalleeClass::Indirect;
        };
        match self.resolutions.get(callee.id) {
            Some(Resolution::Def {
                def,
                kind: DefKind::Fn,
            }) => match self.unsafe_fns.get(&def) {
                Some(summary) => CalleeClass::Unsafe(summary),
                None => CalleeClass::Safe,
            },
            Some(Resolution::Def {
                kind: DefKind::Variant | DefKind::Struct | DefKind::Const,
                ..
            }) => CalleeClass::Safe,
            Some(
                Resolution::Local(_)
                | Resolution::Def {
                    kind: DefKind::Static,
                    ..
                },
            ) => CalleeClass::Indirect,
            // None / Import / Primitive: a builtin, stdlib, or constructor
            // (`Box::new`, ...) - assumed not to stash its argument.
            _ => CalleeClass::Safe,
        }
    }

    /// Flags a goroutine/closure body that captures an arena-allocated value.
    fn check_capture(&mut self, body: &Expr, span: Span) {
        if self.references_tainted(body) {
            self.flag(span, ArenaEscapeKind::Capture);
        }
    }

    fn references_tainted(&self, expr: &Expr) -> bool {
        let mut found = false;
        let mut visit = |e: &Expr| {
            if let ExprKind::Path(path) = &e.kind {
                if path
                    .segments
                    .first()
                    .is_some_and(|seg| self.tainted.contains(&seg.name.name))
                {
                    found = true;
                }
            }
        };
        walk_paths(expr, &mut visit);
        found
    }

    fn sink_assign(&mut self, place: &Expr, value: &Expr) {
        self.expr(place);
        self.expr(value);
        self.check_assign_escape(place, value);
    }

    /// Flags an arena-local value written into a place that outlives the
    /// region. Tuples on both sides pair element-wise, so a destructuring
    /// assignment is judged per target rather than as one opaque write.
    fn check_assign_escape(&mut self, place: &Expr, value: &Expr) {
        if let (ExprKind::Tuple(targets), ExprKind::Tuple(values)) = (&place.kind, &value.kind)
            && targets.len() == values.len()
        {
            for (target, element) in targets.iter().zip(values) {
                self.check_assign_escape(target, element);
            }
            return;
        }
        if self.target_escapes_outer(place) && self.is_region_local(value) {
            self.flag(value.span, ArenaEscapeKind::OuterAssign);
        }
    }

    /// Whether an assignment target reaches a place outliving the region.
    /// A destructuring tuple does when any of its elements does.
    fn target_escapes_outer(&self, place: &Expr) -> bool {
        match &place.kind {
            ExprKind::Tuple(elems) => elems.iter().any(|e| self.target_escapes_outer(e)),
            _ => self.escapes_outer(place),
        }
    }

    fn sink_call(&mut self, callee: &Expr, args: &[Expr]) {
        self.expr(callee);
        for a in args {
            self.expr(a);
        }
        let kind = match self.classify_callee(callee) {
            CalleeClass::Safe => return,
            // A stash into an argument this block owns dies with the block.
            CalleeClass::Unsafe(summary)
                if !summary.global
                    && summary
                        .sinks
                        .iter()
                        .all(|&sink| args.get(sink).is_some_and(|a| !self.escapes_outer(a))) =>
            {
                return;
            }
            CalleeClass::Unsafe(_) => ArenaEscapeKind::UnsafeCallee,
            CalleeClass::Indirect => ArenaEscapeKind::IndirectCall,
        };
        for a in args {
            if self.is_region_local(a) {
                self.flag(a.span, kind);
            }
        }
    }

    fn sink_method_call(&mut self, receiver: &Expr, method: &str, args: &[Expr]) {
        self.expr(receiver);
        for a in args {
            self.expr(a);
        }
        let kind = if method == "send" {
            ArenaEscapeKind::ChannelSend
        } else if MUTATOR_METHODS.contains(&method) && self.escapes_outer(receiver) {
            ArenaEscapeKind::OuterContainer
        } else {
            return;
        };
        for a in args {
            if self.is_region_local(a) {
                self.flag(a.span, kind);
            }
        }
    }

    fn sink_select(&mut self, arms: &[gossamer_ast::SelectArm]) {
        for arm in arms {
            match &arm.op {
                SelectOp::Recv { channel, .. } => self.expr(channel),
                SelectOp::Send { channel, value } => {
                    self.expr(channel);
                    self.expr(value);
                    if self.is_region_local(value) {
                        self.flag(value.span, ArenaEscapeKind::ChannelSend);
                    }
                }
                SelectOp::Default => {}
            }
            self.expr(&arm.body);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "exhaustive per-ExprKind walker; one arm per variant, splitting obscures the dispatch"
    )]
    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Assign { place, value, .. } => self.sink_assign(place, value),
            ExprKind::Call { callee, args } => self.sink_call(callee, args),
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => self.sink_method_call(receiver, name.name.as_str(), args),
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    self.expr(v);
                    if self.is_region_local(v) {
                        self.flag(v.span, ArenaEscapeKind::Return);
                    }
                }
            }
            ExprKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.expr(v);
                    if self.loop_depth == 0 && self.is_region_local(v) {
                        self.flag(v.span, ArenaEscapeKind::Break);
                    }
                }
            }
            ExprKind::Closure { body, .. } => self.check_capture(body, expr.span),
            ExprKind::Select(arms) => self.sink_select(arms),
            ExprKind::While {
                condition, body, ..
            } => {
                self.expr(condition);
                self.loop_depth += 1;
                self.expr(body);
                self.loop_depth -= 1;
            }
            ExprKind::Loop { body, .. } => {
                self.loop_depth += 1;
                self.expr(body);
                self.loop_depth -= 1;
            }
            ExprKind::For { iter, body, .. } => {
                self.expr(iter);
                self.loop_depth += 1;
                self.expr(body);
                self.loop_depth -= 1;
            }
            ExprKind::Block(b) | ExprKind::Unsafe(b) => self.block(b),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition);
                self.expr(then_branch);
                if let Some(e) = else_branch {
                    self.expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Unary { operand, .. } | ExprKind::Cast { value: operand, .. } => {
                self.expr(operand);
            }
            ExprKind::FieldAccess { receiver, .. } => self.expr(receiver),
            ExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Try(inner) => self.expr(inner),
            ExprKind::Tuple(items) | ExprKind::MapLiteral(items) | ExprKind::SetLiteral(items) => {
                for i in items {
                    self.expr(i);
                }
            }
            ExprKind::Array(arr) | ExprKind::FixedArray(arr) => match arr {
                ArrayExpr::List(items) => {
                    for i in items {
                        self.expr(i);
                    }
                }
                ArrayExpr::Repeat { value, count } => {
                    self.expr(value);
                    self.expr(count);
                }
            },
            ExprKind::Struct { fields, base, .. } => {
                for f in fields {
                    if let Some(v) = &f.value {
                        self.expr(v);
                    }
                }
                if let Some(b) = base {
                    self.expr(b);
                }
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.expr(s);
                }
                if let Some(e) = end {
                    self.expr(e);
                }
            }
            ExprKind::Literal(_)
            | ExprKind::Path(_)
            | ExprKind::Continue { .. }
            | ExprKind::Error => {}
        }
    }
}

/// Visits every expression in `expr`, invoking `visit` on each (preorder).
#[allow(
    clippy::too_many_lines,
    reason = "exhaustive per-ExprKind walker; one arm per variant, splitting obscures the dispatch"
)]
fn walk_paths(expr: &Expr, visit: &mut impl FnMut(&Expr)) {
    visit(expr);
    match &expr.kind {
        ExprKind::Call { callee, args } => {
            walk_paths(callee, visit);
            for a in args {
                walk_paths(a, visit);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            walk_paths(receiver, visit);
            for a in args {
                walk_paths(a, visit);
            }
        }
        ExprKind::Binary { lhs, rhs, .. }
        | ExprKind::Assign {
            place: lhs,
            value: rhs,
            ..
        }
        | ExprKind::Index {
            base: lhs,
            index: rhs,
        } => {
            walk_paths(lhs, visit);
            walk_paths(rhs, visit);
        }
        ExprKind::Unary { operand, .. }
        | ExprKind::FieldAccess {
            receiver: operand, ..
        }
        | ExprKind::Try(operand)
        | ExprKind::Cast { value: operand, .. } => walk_paths(operand, visit),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_paths(condition, visit);
            walk_paths(then_branch, visit);
            if let Some(e) = else_branch {
                walk_paths(e, visit);
            }
        }
        ExprKind::Loop { body, .. } | ExprKind::Closure { body, .. } => walk_paths(body, visit),
        ExprKind::While {
            condition, body, ..
        } => {
            walk_paths(condition, visit);
            walk_paths(body, visit);
        }
        ExprKind::For { iter, body, .. } => {
            walk_paths(iter, visit);
            walk_paths(body, visit);
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_paths(scrutinee, visit);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    walk_paths(g, visit);
                }
                walk_paths(&arm.body, visit);
            }
        }
        ExprKind::Block(b) | ExprKind::Unsafe(b) => {
            for stmt in &b.stmts {
                match &stmt.kind {
                    StmtKind::Let { init, .. } => {
                        if let Some(init) = init {
                            walk_paths(init, visit);
                        }
                    }
                    StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => {
                        walk_paths(expr, visit);
                    }
                    StmtKind::Item(_) => {}
                }
            }
            if let Some(tail) = &b.tail {
                walk_paths(tail, visit);
            }
        }
        ExprKind::Return(v) | ExprKind::Break { value: v, .. } => {
            if let Some(v) = v {
                walk_paths(v, visit);
            }
        }
        ExprKind::Tuple(items) | ExprKind::MapLiteral(items) | ExprKind::SetLiteral(items) => {
            for i in items {
                walk_paths(i, visit);
            }
        }
        ExprKind::Array(arr) | ExprKind::FixedArray(arr) => match arr {
            ArrayExpr::List(items) => {
                for i in items {
                    walk_paths(i, visit);
                }
            }
            ArrayExpr::Repeat { value, count } => {
                walk_paths(value, visit);
                walk_paths(count, visit);
            }
        },
        ExprKind::Struct { fields, base, .. } => {
            for f in fields {
                if let Some(v) = &f.value {
                    walk_paths(v, visit);
                }
            }
            if let Some(b) = base {
                walk_paths(b, visit);
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                walk_paths(s, visit);
            }
            if let Some(e) = end {
                walk_paths(e, visit);
            }
        }
        ExprKind::Select(arms) => {
            for arm in arms {
                match &arm.op {
                    SelectOp::Recv { channel, .. } => walk_paths(channel, visit),
                    SelectOp::Send { channel, value } => {
                        walk_paths(channel, visit);
                        walk_paths(value, visit);
                    }
                    SelectOp::Default => {}
                }
                walk_paths(&arm.body, visit);
            }
        }
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Continue { .. } | ExprKind::Error => {}
    }
}

/// The way an arena-allocated value leaves its block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArenaEscapeKind {
    /// Stored into a binding declared outside the block.
    OuterAssign,
    /// Pushed into a container whose root outlives the block.
    OuterContainer,
    /// Sent on a channel.
    ChannelSend,
    /// Returned out of the enclosing function.
    Return,
    /// Carried by `break` out of a loop that encloses the block.
    Break,
    /// Captured by a goroutine or closure that may outlive the block.
    Capture,
    /// Passed to a function that may stash it where it outlives the block.
    UnsafeCallee,
    /// Passed to an indirect call that cannot be vetted.
    IndirectCall,
}

impl ArenaEscapeKind {
    /// Message for the primary label pointing at the escaping value.
    const fn label(self) -> &'static str {
        match self {
            Self::OuterAssign => {
                "this value is allocated in the arena but stored into a binding that outlives the block"
            }
            Self::OuterContainer => {
                "this value is allocated in the arena but pushed into a container that outlives the block"
            }
            Self::ChannelSend => {
                "this value is allocated in the arena but sent on a channel that may outlive the block"
            }
            Self::Return => "this value is allocated in the arena but returned out of the block",
            Self::Break => {
                "this value is allocated in the arena but carried out of an enclosing loop"
            }
            Self::Capture => {
                "this goroutine/closure captures a value allocated in the arena and may outlive the block"
            }
            Self::UnsafeCallee => {
                "this arena-allocated value is passed to a function that may stash it past the block"
            }
            Self::IndirectCall => {
                "this arena-allocated value is passed to an indirect call that cannot be verified safe"
            }
        }
    }
}

/// One arena-escape diagnostic with its primary span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArenaEscapeDiagnostic {
    /// The specific escape that was detected.
    pub error: ArenaEscapeError,
    /// Where the escaping value appears.
    pub span: Span,
}

impl ArenaEscapeDiagnostic {
    /// Constructs a diagnostic from its error and span.
    #[must_use]
    pub const fn new(error: ArenaEscapeError, span: Span) -> Self {
        Self { error, span }
    }

    /// Renders this diagnostic as a structured
    /// [`gossamer_diagnostics::Diagnostic`].
    #[must_use]
    pub fn to_diagnostic(&self) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location};
        let location = Location::new(self.span.file, self.span);
        Diagnostic::error(
            Code(self.error.code()),
            "value allocated in an `arena { }` block escapes the block",
        )
        .with_primary(location, self.error.kind.label())
        .with_note(
            "an `arena { }` block frees its memory in one shot at the closing brace, so using this value afterward would be a use-after-free",
        )
        .with_help(
            "compute a scalar or already-outside summary inside the block and keep only that, or allocate the value before the block",
        )
    }
}

impl fmt::Display for ArenaEscapeDiagnostic {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}", self.error)
    }
}

/// The arena-escape error, carrying which kind of escape occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("value allocated in an `arena {{ }}` block escapes the block")]
pub struct ArenaEscapeError {
    /// Which escape sink the value reached.
    pub kind: ArenaEscapeKind,
}

impl ArenaEscapeError {
    /// Stable diagnostic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "GM0003"
    }
}
