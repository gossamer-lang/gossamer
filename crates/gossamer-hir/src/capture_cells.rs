//! Closures name the bindings they capture.
//!
//! A closure reads a captured binding's current value and its writes reach
//! the binding, so a binding a closure captures and something writes lives in
//! a shared cell: a one-element `Vec`, which every tier shares between a
//! closure and the code around it. Each use of such a binding in its scope is
//! rewritten to the cell's element. A container mutated in place is shared by
//! its handle already, so only reassigning one, or lending `&mut` to it,
//! gives it a cell.
//!
//! A spawned closure is the exception: it runs on another goroutine, so it
//! takes a snapshot of each local it captures where the `spawn` is written.
//! The checker rejects a write to a capture inside one.

use std::collections::{HashMap, HashSet};

use gossamer_ast::Ident;
use gossamer_lex::Span;
use gossamer_types::{IntTy, Ty, TyCtxt, TyKind};

use crate::ids::HirIdGenerator;
use crate::tree::{
    HirArrayExpr, HirBlock, HirExpr, HirExprKind, HirFn, HirItemKind, HirLiteral, HirMatchArm,
    HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind, HirUnaryOp, for_each_child_expr,
    for_each_child_expr_mut,
};

/// Prefix of the cell a captured, written binding is rewritten to.
const CELL_PREFIX: &str = "__cell_";
/// The runtime collections a copy shares by handle: sets, deques, queues,
/// stacks, and heaps.
const HANDLE_COLLECTION_DEFS: [u32; 7] = [
    u32::MAX - 7,
    u32::MAX - 18,
    u32::MAX - 19,
    u32::MAX - 28,
    u32::MAX - 30,
    u32::MAX - 31,
    u32::MAX - 32,
];
/// Prefix of the snapshot a spawned closure takes of a captured local.
const SNAPSHOT_PREFIX: &str = "__snapshot_";

/// Rewrites every binding a closure captures and something writes into a
/// shared cell, and gives every spawned closure snapshots of its captures.
pub(crate) fn insert_capture_cells(
    program: &mut HirProgram,
    tcx: &mut TyCtxt,
    ids: &mut HirIdGenerator,
) {
    let mut_self_methods = mut_self_method_names(program);
    let mut pass = CellPass {
        tcx,
        ids,
        next: 0,
        mut_self_methods,
    };
    for item in &mut program.items {
        match &mut item.kind {
            HirItemKind::Fn(decl) => pass.visit_fn(decl),
            HirItemKind::Impl(imp) => imp.methods.iter_mut().for_each(|m| pass.visit_fn(m)),
            HirItemKind::Trait(tr) => tr.methods.iter_mut().for_each(|m| pass.visit_fn(m)),
            HirItemKind::Const(_) | HirItemKind::Static(_) | HirItemKind::Adt(_) => {}
        }
    }
}

/// Names of the methods some `impl` or trait declares with a `&mut self`
/// receiver, whose calls write the receiver.
fn mut_self_method_names(program: &HirProgram) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut note = |decl: &HirFn| {
        // A `&mut self` receiver lowers to a `mut self` binding.
        let mut_receiver = decl.has_self
            && decl.params.first().is_some_and(|param| {
                matches!(&param.pattern.kind, HirPatKind::Binding { name, mutable: true }
                    if name.name == "self")
            });
        if mut_receiver {
            names.insert(decl.name.name.clone());
        }
    };
    for item in &program.items {
        match &item.kind {
            HirItemKind::Impl(imp) => imp.methods.iter().for_each(&mut note),
            HirItemKind::Trait(tr) => tr.methods.iter().for_each(&mut note),
            _ => {}
        }
    }
    names
}

/// The bindings a scope introduces, each with whether it is declared `mut`.
type Scope = HashMap<String, bool>;

/// Records each binding `pat` introduces in `scope`.
fn note_pattern(pat: &HirPat, scope: &mut Scope) {
    let mut names: HashSet<String> = HashSet::new();
    crate::lift::collect_pattern_names(pat, &mut names);
    let mut mutable: HashSet<String> = HashSet::new();
    crate::lift::collect_mutable_pattern_names(pat, &mut mutable);
    for name in names {
        let is_mut = mutable.contains(&name);
        scope.insert(name, is_mut);
    }
}

/// Every name `pat` binds, in a stable order.
fn pattern_bindings(pat: &HirPat, out: &mut Vec<String>) {
    let mut names: HashSet<String> = HashSet::new();
    crate::lift::collect_pattern_names(pat, &mut names);
    let mut names: Vec<String> = names.into_iter().filter(|n| !out.contains(n)).collect();
    names.sort();
    out.extend(names);
}

/// Whether `pat` binds `name`.
fn binds(pat: &HirPat, name: &str) -> bool {
    let mut names: HashSet<String> = HashSet::new();
    crate::lift::collect_pattern_names(pat, &mut names);
    names.contains(name)
}

/// Whether `expr` is the bare local `name`.
fn is_name(expr: &HirExpr, name: &str) -> bool {
    matches!(&expr.kind, HirExprKind::Path { segments, def: None }
        if segments.len() == 1 && segments[0].name == name)
}

/// The local a place expression is rooted at, through fields, tuple fields,
/// and indices.
fn place_root(place: &HirExpr) -> Option<&str> {
    match &place.kind {
        HirExprKind::Path {
            segments,
            def: None,
        } if segments.len() == 1 => Some(segments[0].name.as_str()),
        HirExprKind::Field { receiver, .. }
        | HirExprKind::TupleIndex { receiver, .. }
        | HirExprKind::Index { base: receiver, .. } => place_root(receiver),
        _ => None,
    }
}

/// Whether `expr` is a call to the prelude `spawn` whose first argument is
/// a closure.
fn is_spawn_of_closure(expr: &HirExpr) -> bool {
    let HirExprKind::Call { callee, args } = &expr.kind else {
        return false;
    };
    is_name(callee, "spawn")
        && args
            .first()
            .is_some_and(|arg| matches!(arg.kind, HirExprKind::Closure { .. }))
}

/// How a binding is used over the code its scope covers.
#[derive(Debug, Default, Clone, Copy)]
struct Uses {
    /// A closure other than a spawned one reads or writes it.
    captured: bool,
    /// Something writes it, in a way a copy would not carry back.
    written: bool,
    /// The type an occurrence of it carries.
    ty: Option<Ty>,
}

impl Uses {
    fn cell_ty(self) -> Option<Ty> {
        if self.captured && self.written {
            self.ty
        } else {
            None
        }
    }
}

struct CellPass<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    next: u32,
    mut_self_methods: HashSet<String>,
}

impl CellPass<'_> {
    fn visit_fn(&mut self, decl: &mut HirFn) {
        let Some(body) = &mut decl.body else {
            return;
        };
        let mut bindings = Vec::new();
        for param in &decl.params {
            pattern_bindings(&param.pattern, &mut bindings);
        }
        let prologue = self.cells_for(&bindings, &mut ScopeMut::Block(&mut body.block, 0));
        if !prologue.is_empty() {
            let mut stmts = prologue;
            stmts.append(&mut body.block.stmts);
            body.block.stmts = stmts;
        }
        self.visit_block(&mut body.block);
        let mut params = Scope::new();
        for param in &decl.params {
            note_pattern(&param.pattern, &mut params);
        }
        let mut locals: Vec<Scope> = vec![params];
        self.snapshot_block(&mut body.block, &mut locals);
    }

    /// The `let` statements creating cells for the `bindings` a scope
    /// introduces that need one, after rewriting the scope's uses of them.
    fn cells_for(&mut self, bindings: &[String], scope: &mut ScopeMut<'_>) -> Vec<HirStmt> {
        let mut stmts = Vec::new();
        for name in bindings {
            if name == "self" {
                continue;
            }
            let Some(ty) = self.scan_scope(scope, name).cell_ty() else {
                continue;
            };
            let span = scope.span();
            let cell_name = format!("{CELL_PREFIX}{name}_{}", self.next);
            self.next += 1;
            let cell_ty = self.tcx.intern(TyKind::Vec(ty));
            self.rename_scope(scope, name, &cell_name, cell_ty);
            let value = self.path(name, ty, span);
            let init = self.vec_of(value, cell_ty, span);
            stmts.push(self.let_stmt(&cell_name, cell_ty, init, span));
        }
        stmts
    }

    fn path(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        }
    }

    fn vec_of(&mut self, value: HirExpr, vec_ty: Ty, span: Span) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty: vec_ty,
            kind: HirExprKind::Array(HirArrayExpr::List(vec![value])),
        }
    }

    fn let_stmt(&mut self, name: &str, ty: Ty, init: HirExpr, span: Span) -> HirStmt {
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Let {
                pattern: HirPat {
                    id: self.ids.next(),
                    span,
                    ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new(name),
                        mutable: true,
                    },
                },
                ty,
                init: Some(init),
            },
        }
    }

    /// Rewrites the bindings of `block` and of the expressions in it that
    /// need cells.
    fn visit_block(&mut self, block: &mut HirBlock) {
        let mut idx = 0;
        while idx < block.stmts.len() {
            let bindings = match &block.stmts[idx].kind {
                HirStmtKind::Let { pattern, .. } => {
                    let mut out = Vec::new();
                    pattern_bindings(pattern, &mut out);
                    out
                }
                _ => Vec::new(),
            };
            if !bindings.is_empty() {
                let cells = self.cells_for(&bindings, &mut ScopeMut::Block(block, idx + 1));
                let count = cells.len();
                for (offset, stmt) in cells.into_iter().enumerate() {
                    block.stmts.insert(idx + 1 + offset, stmt);
                }
                idx += count;
            }
            idx += 1;
        }
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let {
                    init: Some(init), ..
                } => self.visit_expr(init),
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => self.visit_expr(expr),
                HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
            }
        }
        if let Some(tail) = &mut block.tail {
            self.visit_expr(tail);
        }
    }

    fn visit_expr(&mut self, expr: &mut HirExpr) {
        match &mut expr.kind {
            HirExprKind::Block(block) => self.visit_block(block),
            HirExprKind::Closure { params, body, .. } => {
                let mut bindings = Vec::new();
                for param in params.iter() {
                    pattern_bindings(&param.pattern, &mut bindings);
                }
                let prologue = self.cells_for(&bindings, &mut ScopeMut::Expr(body));
                self.prepend(body, prologue);
                self.visit_expr(body);
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.visit_expr(scrutinee);
                for arm in arms.iter_mut() {
                    self.visit_arm(arm);
                }
            }
            _ => for_each_child_expr_mut(expr, &mut |child| self.visit_expr(child)),
        }
    }

    fn visit_arm(&mut self, arm: &mut HirMatchArm) {
        let mut bindings = Vec::new();
        pattern_bindings(&arm.pattern, &mut bindings);
        let prologue = self.cells_for(&bindings, &mut ScopeMut::Arm(arm));
        self.prepend(&mut arm.body, prologue);
        if let Some(guard) = &mut arm.guard {
            self.visit_expr(guard);
        }
        self.visit_expr(&mut arm.body);
    }

    /// Runs `stmts` before `expr`, which keeps its value.
    fn prepend(&mut self, expr: &mut HirExpr, stmts: Vec<HirStmt>) {
        if stmts.is_empty() {
            return;
        }
        let (span, ty) = (expr.span, expr.ty);
        let placeholder = HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Placeholder,
        };
        let inner = std::mem::replace(expr, placeholder);
        *expr = HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.ids.next(),
                span,
                stmts,
                tail: Some(Box::new(inner)),
                ty,
                is_comptime: false,
            }),
        };
    }

    /// Whether `ty` is a container a copy shares by its handle, so writing
    /// into it in place reaches every holder.
    fn shared_by_handle(&self, ty: Ty) -> bool {
        let mut ty = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        match self.tcx.kind_of(ty) {
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::HashMap { .. } => true,
            TyKind::Adt { def, .. } => HANDLE_COLLECTION_DEFS.contains(&def.local),
            _ => false,
        }
    }

    /// How the binding `name` of type `ty` is used over `scope`.
    fn scan_scope(&self, scope: &ScopeMut<'_>, name: &str) -> Uses {
        let mut occurrences: Vec<(&HirExpr, u32)> = Vec::new();
        let mut on = |expr, closure_depth| occurrences.push((expr, closure_depth));
        match scope {
            ScopeMut::Block(block, from) => scan_stmts(
                &block.stmts[*from..],
                block.tail.as_deref(),
                name,
                0,
                &mut on,
            ),
            ScopeMut::Expr(expr) => scan_expr(expr, name, 0, &mut on),
            ScopeMut::Arm(arm) => {
                if let Some(guard) = &arm.guard {
                    scan_expr(guard, name, 0, &mut on);
                }
                scan_expr(&arm.body, name, 0, &mut on);
            }
        }
        let mut uses = Uses {
            ty: occurrences
                .iter()
                .find(|(expr, _)| is_name(expr, name))
                .map(|(expr, _)| expr.ty),
            ..Uses::default()
        };
        let handle = uses.ty.is_some_and(|ty| self.shared_by_handle(ty));
        for (expr, closure_depth) in occurrences {
            self.note_use(expr, name, handle, closure_depth, &mut uses);
        }
        uses
    }

    /// Records what `expr`, an expression `name` is in scope for at
    /// `closure_depth` closures deep, does to the binding.
    fn note_use(
        &self,
        expr: &HirExpr,
        name: &str,
        handle: bool,
        closure_depth: u32,
        uses: &mut Uses,
    ) {
        if closure_depth > 0 && is_name(expr, name) {
            uses.captured = true;
        }
        // A container is shared by its handle, so only rebinding it, or
        // lending `&mut` to it, is a write a closure must see.
        let rooted = |place: &HirExpr| {
            if handle {
                is_name(place, name)
            } else {
                place_root(place) == Some(name)
            }
        };
        let written = match &expr.kind {
            HirExprKind::Assign { place, .. } => rooted(place),
            HirExprKind::Unary {
                op: HirUnaryOp::RefMut,
                operand,
            } => rooted(operand),
            HirExprKind::MethodCall {
                receiver,
                name: method,
                ..
            } => {
                !handle
                    && place_root(receiver) == Some(name)
                    && (gossamer_types::is_mutating_method_name(&method.name)
                        || self.mut_self_methods.contains(&method.name))
            }
            _ => false,
        };
        uses.written |= written;
    }

    /// Rewrites each use of `name` over `scope` to `cell_name[0]`.
    fn rename_scope(&mut self, scope: &mut ScopeMut<'_>, name: &str, cell_name: &str, cell_ty: Ty) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let ids = &mut *self.ids;
        let mut rewrite = |expr: &mut HirExpr| {
            if !is_name(expr, name) {
                return;
            }
            let span = expr.span;
            let base = HirExpr {
                id: ids.next(),
                span,
                ty: cell_ty,
                kind: HirExprKind::Path {
                    segments: vec![Ident::new(cell_name)],
                    def: None,
                },
            };
            let index = HirExpr {
                id: ids.next(),
                span,
                ty: i64_ty,
                kind: HirExprKind::Literal(HirLiteral::Int("0".to_string())),
            };
            expr.kind = HirExprKind::Index {
                base: Box::new(base),
                index: Box::new(index),
            };
        };
        match scope {
            ScopeMut::Block(block, from) => {
                let from = *from;
                rename_stmts(
                    &mut block.stmts[from..],
                    block.tail.as_deref_mut(),
                    name,
                    &mut rewrite,
                );
            }
            ScopeMut::Expr(expr) => rename_expr(expr, name, &mut rewrite),
            ScopeMut::Arm(arm) => {
                if let Some(guard) = &mut arm.guard {
                    rename_expr(guard, name, &mut rewrite);
                }
                rename_expr(&mut arm.body, name, &mut rewrite);
            }
        }
    }

    /// Gives each spawned closure in `block` snapshots of the locals it
    /// captures. `locals` holds the bindings around it.
    fn snapshot_block(&mut self, block: &mut HirBlock, locals: &mut Vec<Scope>) {
        locals.push(Scope::new());
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { pattern, init, .. } => {
                    if let Some(init) = init {
                        self.snapshot_expr(init, locals);
                    }
                    if let Some(scope) = locals.last_mut() {
                        note_pattern(pattern, scope);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                    self.snapshot_expr(expr, locals);
                }
                HirStmtKind::Item(_) => {}
            }
        }
        if let Some(tail) = &mut block.tail {
            self.snapshot_expr(tail, locals);
        }
        locals.pop();
    }

    fn snapshot_expr(&mut self, expr: &mut HirExpr, locals: &mut Vec<Scope>) {
        if is_spawn_of_closure(expr) {
            self.snapshot_spawn(expr, locals);
            return;
        }
        match &mut expr.kind {
            HirExprKind::Block(block) => self.snapshot_block(block, locals),
            HirExprKind::Closure { params, body, .. } => {
                let mut scope = Scope::new();
                for param in params.iter() {
                    note_pattern(&param.pattern, &mut scope);
                }
                locals.push(scope);
                self.snapshot_expr(body, locals);
                locals.pop();
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.snapshot_expr(scrutinee, locals);
                for arm in arms.iter_mut() {
                    let mut scope = Scope::new();
                    note_pattern(&arm.pattern, &mut scope);
                    locals.push(scope);
                    if let Some(guard) = &mut arm.guard {
                        self.snapshot_expr(guard, locals);
                    }
                    self.snapshot_expr(&mut arm.body, locals);
                    locals.pop();
                }
            }
            _ => for_each_child_expr_mut(expr, &mut |child| self.snapshot_expr(child, locals)),
        }
    }

    /// Rewrites `spawn(|| body, ..)` to bind a snapshot of each local the
    /// closure captures, as the closure's own, ahead of the call.
    fn snapshot_spawn(&mut self, expr: &mut HirExpr, locals: &mut Vec<Scope>) {
        let HirExprKind::Call { args, .. } = &mut expr.kind else {
            return;
        };
        for arg in args.iter_mut().skip(1) {
            self.snapshot_expr(arg, locals);
        }
        let Some(closure) = args.first_mut() else {
            return;
        };
        let HirExprKind::Closure { params, body, .. } = &mut closure.kind else {
            return;
        };
        let mut params_scope = Scope::new();
        for param in params.iter() {
            note_pattern(&param.pattern, &mut params_scope);
        }
        let bound: HashSet<String> = params_scope.keys().cloned().collect();
        let mut captured: Vec<(String, Ty)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut on = |occurrence: &HirExpr, _depth: u32| {
            if let HirExprKind::Path {
                segments,
                def: None,
            } = &occurrence.kind
                && segments.len() == 1
            {
                let name = &segments[0].name;
                // A binding nothing can change after the spawn reads the same
                // value on the goroutine as a snapshot of it would, so the
                // goroutine shares it. A `&mut` reaches storage another
                // holder writes.
                let changeable = locals
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(name).copied());
                let snapshot = changeable.is_some_and(|mutable| {
                    mutable || matches!(self.tcx.kind_of(occurrence.ty), TyKind::Ref { .. })
                });
                if snapshot && !bound.contains(name) && seen.insert(name.clone()) {
                    captured.push((name.clone(), occurrence.ty));
                }
            }
        };
        scan_free(body, &bound, &mut on);
        // A spawned closure inside the spawned one takes its snapshots from
        // this closure's own.
        let mut inner_locals = locals.clone();
        inner_locals.push(params_scope);
        self.snapshot_expr(body, &mut inner_locals);
        if captured.is_empty() {
            return;
        }
        let span = expr.span;
        let mut stmts = Vec::new();
        for (name, ty) in &captured {
            let snapshot = format!("{SNAPSHOT_PREFIX}{name}_{}", self.next);
            self.next += 1;
            let value = self.path(name, *ty, span);
            stmts.push(self.let_stmt(&snapshot, *ty, value, span));
            let mut rewrite = |occurrence: &mut HirExpr| {
                if is_name(occurrence, name) {
                    occurrence.kind = HirExprKind::Path {
                        segments: vec![Ident::new(&snapshot)],
                        def: None,
                    };
                }
            };
            let HirExprKind::Call { args, .. } = &mut expr.kind else {
                return;
            };
            if let Some(HirExpr {
                kind: HirExprKind::Closure { body, .. },
                ..
            }) = args.first_mut()
            {
                rename_expr(body, name, &mut rewrite);
            }
        }
        let call = std::mem::replace(
            expr,
            HirExpr {
                id: self.ids.next(),
                span,
                ty: expr.ty,
                kind: HirExprKind::Placeholder,
            },
        );
        let ty = call.ty;
        *expr = HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.ids.next(),
                span,
                stmts,
                tail: Some(Box::new(call)),
                ty,
                is_comptime: false,
            }),
        };
    }
}

/// The code a binding introduced by a statement, a closure parameter, or a
/// match arm is in scope over.
enum ScopeMut<'a> {
    /// The statements of a block from an index on, and its tail.
    Block(&'a mut HirBlock, usize),
    /// A closure body.
    Expr(&'a mut HirExpr),
    /// A match arm's guard and body.
    Arm(&'a mut HirMatchArm),
}

impl ScopeMut<'_> {
    fn span(&self) -> Span {
        match self {
            Self::Block(block, _) => block.span,
            Self::Expr(expr) => expr.span,
            Self::Arm(arm) => arm.body.span,
        }
    }
}

/// Calls `on` with every expression `name` is in scope for in `stmts` and
/// `tail`, with how many closures deep it sits, stopping where a `let`
/// shadows the name.
fn scan_stmts<'a>(
    stmts: &'a [HirStmt],
    tail: Option<&'a HirExpr>,
    name: &str,
    depth: u32,
    on: &mut impl FnMut(&'a HirExpr, u32),
) {
    for stmt in stmts {
        match &stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                if let Some(init) = init {
                    scan_expr(init, name, depth, on);
                }
                if binds(pattern, name) {
                    return;
                }
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                scan_expr(expr, name, depth, on);
            }
            HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = tail {
        scan_expr(tail, name, depth, on);
    }
}

/// [`scan_stmts`] for one expression. A spawned closure is not walked: it
/// captures a snapshot rather than the binding.
fn scan_expr<'a>(expr: &'a HirExpr, name: &str, depth: u32, on: &mut impl FnMut(&'a HirExpr, u32)) {
    on(expr, depth);
    match &expr.kind {
        HirExprKind::Block(block) => {
            scan_stmts(&block.stmts, block.tail.as_deref(), name, depth, on);
        }
        HirExprKind::Closure { params, body, .. } => {
            if params.iter().any(|param| binds(&param.pattern, name)) {
                return;
            }
            scan_expr(body, name, depth + 1, on);
        }
        HirExprKind::Match { scrutinee, arms } => {
            scan_expr(scrutinee, name, depth, on);
            for arm in arms {
                if binds(&arm.pattern, name) {
                    continue;
                }
                if let Some(guard) = &arm.guard {
                    scan_expr(guard, name, depth, on);
                }
                scan_expr(&arm.body, name, depth, on);
            }
        }
        _ if is_spawn_of_closure(expr) => {
            let HirExprKind::Call { args, .. } = &expr.kind else {
                return;
            };
            for arg in args.iter().skip(1) {
                scan_expr(arg, name, depth, on);
            }
        }
        _ => for_each_child_expr(expr, &mut |child| scan_expr(child, name, depth, on)),
    }
}

/// Calls `on` with every expression in `expr` whose free names are not
/// bound in it: each bound set of `bound`, or by a `let`, parameter, or
/// pattern inside it.
fn scan_free(expr: &HirExpr, bound: &HashSet<String>, on: &mut impl FnMut(&HirExpr, u32)) {
    if let HirExprKind::Path {
        segments,
        def: None,
    } = &expr.kind
        && segments.len() == 1
        && !bound.contains(&segments[0].name)
    {
        on(expr, 0);
    }
    match &expr.kind {
        HirExprKind::Block(block) => {
            let mut inner = bound.clone();
            for stmt in &block.stmts {
                match &stmt.kind {
                    HirStmtKind::Let { pattern, init, .. } => {
                        if let Some(init) = init {
                            scan_free(init, &inner, on);
                        }
                        crate::lift::collect_pattern_names(pattern, &mut inner);
                    }
                    HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                        scan_free(expr, &inner, on);
                    }
                    HirStmtKind::Item(_) => {}
                }
            }
            if let Some(tail) = &block.tail {
                scan_free(tail, &inner, on);
            }
        }
        HirExprKind::Closure { params, body, .. } => {
            let mut inner = bound.clone();
            for param in params {
                crate::lift::collect_pattern_names(&param.pattern, &mut inner);
            }
            scan_free(body, &inner, on);
        }
        HirExprKind::Match { scrutinee, arms } => {
            scan_free(scrutinee, bound, on);
            for arm in arms {
                let mut inner = bound.clone();
                crate::lift::collect_pattern_names(&arm.pattern, &mut inner);
                if let Some(guard) = &arm.guard {
                    scan_free(guard, &inner, on);
                }
                scan_free(&arm.body, &inner, on);
            }
        }
        _ => for_each_child_expr(expr, &mut |child| scan_free(child, bound, on)),
    }
}

/// Calls `rewrite` on every expression `name` is in scope for in `stmts`
/// and `tail`, stopping where a `let` shadows the name.
fn rename_stmts(
    stmts: &mut [HirStmt],
    tail: Option<&mut HirExpr>,
    name: &str,
    rewrite: &mut impl FnMut(&mut HirExpr),
) {
    for stmt in stmts {
        match &mut stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                if let Some(init) = init {
                    rename_expr(init, name, rewrite);
                }
                if binds(pattern, name) {
                    return;
                }
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                rename_expr(expr, name, rewrite);
            }
            HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = tail {
        rename_expr(tail, name, rewrite);
    }
}

/// [`rename_stmts`] for one expression.
fn rename_expr(expr: &mut HirExpr, name: &str, rewrite: &mut impl FnMut(&mut HirExpr)) {
    rewrite(expr);
    match &mut expr.kind {
        HirExprKind::Block(block) => {
            rename_stmts(&mut block.stmts, block.tail.as_deref_mut(), name, rewrite);
        }
        HirExprKind::Closure { params, body, .. } => {
            if params.iter().any(|param| binds(&param.pattern, name)) {
                return;
            }
            rename_expr(body, name, rewrite);
        }
        HirExprKind::Match { scrutinee, arms } => {
            rename_expr(scrutinee, name, rewrite);
            for arm in arms.iter_mut() {
                if binds(&arm.pattern, name) {
                    continue;
                }
                if let Some(guard) = &mut arm.guard {
                    rename_expr(guard, name, rewrite);
                }
                rename_expr(&mut arm.body, name, rewrite);
            }
        }
        _ => for_each_child_expr_mut(expr, &mut |child| rename_expr(child, name, rewrite)),
    }
}
