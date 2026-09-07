//! Consumability analysis for move-on-last-use.
//!
//! [`consumable_locals`] returns the set of local names in a function
//! whose single use may *consume* (move out of) the source instead of
//! cloning it under the interpreter's clone-on-write value model. The
//! compiler reads this set at the consuming sites
//! (`Op::MoveConsume` / `Op::VariantFieldConsume` /
//! `Op::IndexGetConsume` / `Op::TupleIndexConsume`) to free input
//! aggregates as they are consumed.
//!
//! Soundness rests on a conservative over-approximation: a name is
//! consumable only when it is bound exactly once, read exactly once via
//! a plain single-segment value `Path`, at the same loop-nesting depth
//! as its binding, never under `&`/`&mut`, never an assignment place,
//! never a projection base, and never referenced inside a closure body.
//! Any doubt leaves the name out of the set (a non-consumed local is
//! always correct, just not optimised). The runtime `Arc::get_mut`
//! guard at each consuming op degrades a still-shared value to a safe
//! clone, so the static set need only guarantee the emptied slot is
//! never read again - which "read exactly once at binding depth"
//! provides.

use std::collections::{HashMap, HashSet};

use gossamer_hir::{
    HirArrayExpr, HirBlock, HirExpr, HirExprKind, HirFn, HirParam, HirPat, HirStmt, HirStmtKind,
    collect_pattern_names,
};
use gossamer_types::Ty;

/// One recorded `Path` read of a local.
#[derive(Clone, Copy)]
struct Occ {
    /// Loop-nesting depth at the read site.
    depth: usize,
    /// `true` when the read is inside a closure body.
    in_closure: bool,
    /// `true` when the read is a plain value read (not under a
    /// borrow / projection / assignment place).
    bare: bool,
}

/// One binding instance. Shadowing creates separate slots, so two
/// `let v` / parameter `v` / loop `v` in disjoint scopes are tracked
/// independently - a legitimate Gossamer pattern (as in Rust).
struct Slot {
    name: String,
    bind_depth: usize,
    read_count: u32,
    read: Option<Occ>,
}

#[derive(Default)]
struct Analyzer {
    /// Lexical scope stack mirroring the compiler's `push_scope` /
    /// `pop_scope`, so a read resolves to the same binding instance the
    /// compiler's `lookup_local` would. Maps name -> [`Self::slots`]
    /// index.
    scopes: Vec<HashMap<String, usize>>,
    /// Every binding instance seen, in declaration order.
    slots: Vec<Slot>,
    /// [`Self::slots`] length at the entry of each enclosing closure
    /// body. A read that resolves to a slot below the innermost mark
    /// names a binding the closure captures rather than one it declares.
    closure_marks: Vec<usize>,
    /// Captured binding names paired with the type at their reading
    /// site, in first-seen order.
    captured: Vec<(String, Ty)>,
}

impl Analyzer {
    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn record_bind(&mut self, name: &str, depth: usize) {
        let idx = self.slots.len();
        self.slots.push(Slot {
            name: name.to_string(),
            bind_depth: depth,
            read_count: 0,
            read: None,
        });
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), idx);
        }
    }

    fn record_bindings(&mut self, pat: &HirPat, depth: usize) {
        let mut names: HashSet<String> = HashSet::new();
        collect_pattern_names(pat, &mut names);
        for name in names {
            self.record_bind(&name, depth);
        }
    }

    /// Resolves a name to the innermost in-scope binding instance.
    fn resolve(&self, name: &str) -> Option<usize> {
        self.scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    fn enter_closure(&mut self) {
        self.closure_marks.push(self.slots.len());
    }

    fn exit_closure(&mut self) {
        self.closure_marks.pop();
    }

    /// Records a read made inside a closure body against the enclosing
    /// binding it resolves to.
    fn record_capture(&mut self, name: &str, ty: Ty) {
        let Some(mark) = self.closure_marks.last().copied() else {
            return;
        };
        if self.resolve(name).is_some_and(|idx| idx < mark) {
            self.captured.push((name.to_string(), ty));
        }
    }

    fn record_read(&mut self, name: &str, depth: usize, in_closure: bool, bare: bool) {
        if let Some(idx) = self.resolve(name) {
            let slot = &mut self.slots[idx];
            slot.read_count += 1;
            slot.read = Some(Occ {
                depth,
                in_closure,
                bare,
            });
        }
    }

    fn visit_block(&mut self, block: &HirBlock, depth: usize, in_closure: bool) {
        self.push_scope();
        for stmt in &block.stmts {
            self.visit_stmt(stmt, depth, in_closure);
        }
        if let Some(tail) = &block.tail {
            self.visit_expr(tail, depth, in_closure, false);
        }
        self.pop_scope();
    }

    fn visit_stmt(&mut self, stmt: &HirStmt, depth: usize, in_closure: bool) {
        match &stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                if let Some(init) = init {
                    // A `let` initializer is a value-consuming site
                    // (`Op::MoveConsume` of an aliasing path).
                    self.visit_expr(init, depth, in_closure, true);
                }
                self.record_bindings(pattern, depth);
            }
            HirStmtKind::Expr { expr, .. } => self.visit_expr(expr, depth, in_closure, false),
            HirStmtKind::Defer(expr) => self.visit_expr(expr, depth, in_closure, false),
            // A `go` body runs on another goroutine and may capture
            // locals; treat it like a closure boundary (never consumable).
            HirStmtKind::Item(_) => {}
        }
    }

    fn visit_expr(&mut self, expr: &HirExpr, depth: usize, in_closure: bool, bare: bool) {
        match &expr.kind {
            HirExprKind::Path { segments, .. } => {
                if let [seg] = segments.as_slice() {
                    self.record_read(&seg.name, depth, in_closure, bare);
                    if in_closure {
                        self.record_capture(&seg.name, expr.ty);
                    }
                }
            }
            HirExprKind::Call { callee, args } => {
                self.visit_expr(callee, depth, in_closure, false);
                for arg in args {
                    self.visit_expr(arg, depth, in_closure, true);
                }
            }
            HirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver, depth, in_closure, false);
                for arg in args {
                    self.visit_expr(arg, depth, in_closure, true);
                }
            }
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.visit_expr(receiver, depth, in_closure, false);
            }
            HirExprKind::Index { base, index } => {
                self.visit_expr(base, depth, in_closure, false);
                self.visit_expr(index, depth, in_closure, true);
            }
            HirExprKind::Unary { operand, .. } => {
                // `&x` / `&mut x` / `*x` / `-x` / `!x` are never a
                // consuming read; keep the operand out of the set.
                self.visit_expr(operand, depth, in_closure, false);
            }
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.visit_expr(lhs, depth, in_closure, false);
                self.visit_expr(rhs, depth, in_closure, false);
            }
            HirExprKind::Assign { place, value } => {
                // Visiting `place` records the LHS path with `bare =
                // false`, so an assignment target is never consumable.
                self.visit_expr(place, depth, in_closure, false);
                self.visit_expr(value, depth, in_closure, false);
            }
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition, depth, in_closure, false);
                self.visit_expr(then_branch, depth, in_closure, false);
                if let Some(else_branch) = else_branch {
                    self.visit_expr(else_branch, depth, in_closure, false);
                }
            }
            HirExprKind::Match { scrutinee, arms } => {
                // The scrutinee is a value-consuming site
                // (`Op::VariantFieldConsume` for a guard-free match).
                self.visit_expr(scrutinee, depth, in_closure, true);
                for arm in arms {
                    // One scope per arm, matching `compile_match`, so a
                    // binding in one arm never resolves a read in another.
                    self.push_scope();
                    self.record_bindings(&arm.pattern, depth);
                    if let Some(guard) = &arm.guard {
                        self.visit_expr(guard, depth, in_closure, false);
                    }
                    // The arm body's value is moved into the match result
                    // register, so a bare-path body (e.g. the `?` desugar's
                    // `Ok(__try_value) => __try_value`) is a consuming read.
                    self.visit_expr(&arm.body, depth, in_closure, true);
                    self.pop_scope();
                }
            }
            HirExprKind::Loop { body, .. } => {
                if let Some(forloop) = match_for_desugar(body) {
                    self.visit_for_desugar(&forloop, depth, in_closure);
                } else {
                    self.visit_expr(body, depth + 1, in_closure, false);
                }
            }
            HirExprKind::While {
                condition, body, ..
            } => {
                // Both the condition and the body re-run each iteration.
                self.visit_expr(condition, depth + 1, in_closure, false);
                self.visit_expr(body, depth + 1, in_closure, false);
            }
            HirExprKind::Block(block) => self.visit_block(block, depth, in_closure),
            HirExprKind::Closure { params, body, .. } => {
                self.push_scope();
                self.enter_closure();
                for param in params {
                    self.record_bindings(&param.pattern, depth);
                }
                // A closure may run repeatedly; nothing referenced in its
                // body is consumable.
                self.visit_expr(body, depth, true, false);
                self.exit_closure();
                self.pop_scope();
            }
            HirExprKind::LiftedClosure { captures, .. } => {
                self.enter_closure();
                for cap in captures {
                    self.visit_expr(cap, depth, true, false);
                }
                self.exit_closure();
            }
            HirExprKind::Tuple(elems) => {
                for e in elems {
                    self.visit_expr(e, depth, in_closure, true);
                }
            }
            HirExprKind::Array(arr) => match arr {
                HirArrayExpr::List(elems) => {
                    for e in elems {
                        self.visit_expr(e, depth, in_closure, true);
                    }
                }
                HirArrayExpr::Repeat { value, count } => {
                    // The repeat value is materialised N times - never a
                    // single consuming read.
                    self.visit_expr(value, depth, in_closure, false);
                    self.visit_expr(count, depth, in_closure, false);
                }
            },
            HirExprKind::Cast { value, .. } => self.visit_expr(value, depth, in_closure, false),
            HirExprKind::Range { start, end, .. } => {
                if let Some(start) = start {
                    self.visit_expr(start, depth, in_closure, false);
                }
                if let Some(end) = end {
                    self.visit_expr(end, depth, in_closure, false);
                }
            }
            HirExprKind::Return(value) => {
                if let Some(value) = value {
                    self.visit_expr(value, depth, in_closure, false);
                }
            }
            HirExprKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.visit_expr(value, depth, in_closure, false);
                }
            }
            HirExprKind::Select { arms } => {
                for arm in arms {
                    self.push_scope();
                    match &arm.op {
                        gossamer_hir::HirSelectOp::Recv { pattern, channel } => {
                            self.visit_expr(channel, depth, in_closure, false);
                            self.record_bindings(pattern, depth);
                        }
                        gossamer_hir::HirSelectOp::Send { channel, value } => {
                            self.visit_expr(channel, depth, in_closure, false);
                            self.visit_expr(value, depth, in_closure, false);
                        }
                        gossamer_hir::HirSelectOp::Default => {}
                    }
                    self.visit_expr(&arm.body, depth, in_closure, false);
                    self.pop_scope();
                }
            }
            HirExprKind::Literal(_) | HirExprKind::Continue { .. } | HirExprKind::Placeholder => {}
        }
    }

    /// Visits a recognised inline for-loop desugar. The collection is
    /// read once before the loop (outer depth, consuming), the element
    /// pattern binds inside the loop (depth + 1), and the body runs at
    /// depth + 1.
    fn visit_for_desugar(&mut self, f: &ForDesugar<'_>, depth: usize, in_closure: bool) {
        // Resolve the collection read in the scope *outside* the loop
        // body (before the element binding is introduced).
        let coll = strip_iter_chain(f.collection);
        if let HirExprKind::Path { segments, .. } = &coll.kind
            && let [seg] = segments.as_slice()
        {
            // The source collection is drained per element by
            // `Op::IndexGetConsume`; record it as a plain read at the
            // loop's outer depth so a same-depth binding qualifies.
            self.record_read(&seg.name, depth, in_closure, true);
        } else {
            self.visit_expr(coll, depth, in_closure, false);
        }
        // One scope for the loop body, matching the for-loop fast path,
        // holding the element binding at the body's depth.
        self.push_scope();
        self.record_bindings(f.some_pat, depth + 1);
        self.visit_expr(f.some_body, depth + 1, in_closure, false);
        self.pop_scope();
    }

    /// A name is consumable when every binding instance of it that is
    /// actually read is individually safe to move (read exactly once, at
    /// its binding depth, as a plain value, outside any closure). Because
    /// the consuming sites look the name up in the current scope, all
    /// reachable instances must qualify; unread instances are irrelevant
    /// (no site ever resolves to them).
    fn finish(self) -> HashSet<String> {
        let mut any_read: HashSet<String> = HashSet::new();
        let mut disqualified: HashSet<String> = HashSet::new();
        for slot in &self.slots {
            if slot.read_count == 0 {
                continue;
            }
            any_read.insert(slot.name.clone());
            let ok = slot.read_count == 1
                && slot
                    .read
                    .is_some_and(|occ| occ.bare && !occ.in_closure && occ.depth == slot.bind_depth);
            if !ok {
                disqualified.insert(slot.name.clone());
            }
        }
        any_read.retain(|name| !disqualified.contains(name));
        any_read
    }
}

/// `true` when draining the scrutinee during this arm's pattern test is
/// safe: once the arm's top constructor tag matches (a `VariantIs` that
/// runs *before* any field is extracted), no further refutable test can
/// fail and fall through to a later arm that would re-read the emptied
/// scrutinee. Only a bare binding / wildcard, or a single-variant
/// pattern whose fields are all bindings / wildcards, qualifies. Any
/// nested refutable test - a literal, range, nested variant, struct, or
/// slice sub-pattern - extracts a field and can then fall through, so
/// such an arm is cloned (consume `false`) instead.
pub(crate) fn pattern_consume_safe(pat: &HirPat) -> bool {
    use gossamer_hir::HirPatKind;
    match &pat.kind {
        HirPatKind::Wildcard | HirPatKind::Rest | HirPatKind::Binding { .. } => true,
        HirPatKind::Variant { fields, .. } => fields.iter().all(|f| {
            matches!(
                f.kind,
                HirPatKind::Wildcard | HirPatKind::Rest | HirPatKind::Binding { .. }
            )
        }),
        _ => false,
    }
}

/// A matched inline for-loop desugar (`loop { match <coll>.next() {
/// Some(pat) => body, None => break } }`).
struct ForDesugar<'a> {
    collection: &'a HirExpr,
    some_pat: &'a HirPat,
    some_body: &'a HirExpr,
}

/// Recognises the inline for-loop desugar shape produced by HIR
/// lowering for built-in iterables, returning the collection
/// expression and the `Some` arm's pattern / body.
fn match_for_desugar(body: &HirExpr) -> Option<ForDesugar<'_>> {
    let HirExprKind::Block(block) = &body.kind else {
        return None;
    };
    if !block.stmts.is_empty() {
        return None;
    }
    let tail = block.tail.as_deref()?;
    let HirExprKind::Match { scrutinee, arms } = &tail.kind else {
        return None;
    };
    if arms.len() != 2 {
        return None;
    }
    let HirExprKind::MethodCall {
        receiver,
        name,
        args,
        owner: None,
    } = &scrutinee.kind
    else {
        return None;
    };
    if name.name != "next" || !args.is_empty() {
        return None;
    }
    let some_arm = &arms[0];
    let none_arm = &arms[1];
    if some_arm.guard.is_some() || none_arm.guard.is_some() {
        return None;
    }
    let gossamer_hir::HirPatKind::Variant {
        name: some_name,
        fields,
    } = &some_arm.pattern.kind
    else {
        return None;
    };
    if some_name.name != "Some" || fields.len() != 1 {
        return None;
    }
    Some(ForDesugar {
        collection: receiver,
        some_pat: &some_arm.pattern,
        some_body: &some_arm.body,
    })
}

/// Strips trailing `.iter()` / `.enumerate()` calls to reach the base
/// collection an inline for-loop drives by index. Leaves `&mut` /
/// other receiver shapes intact (a `(&mut __for_iter).next()` stateful
/// iterator must not be treated as a drainable collection).
fn strip_iter_chain(expr: &HirExpr) -> &HirExpr {
    let mut cur = expr;
    loop {
        match &cur.kind {
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                owner: None,
            } if args.is_empty() && (name.name == "iter" || name.name == "enumerate") => {
                cur = receiver;
            }
            _ => return cur,
        }
    }
}

impl super::FnBuilder<'_> {
    /// Returns the local name when `expr` is a single-segment path to a
    /// consumable local (one the analysis proved is read exactly once
    /// here, so its value may be moved instead of cloned). A direct
    /// reference binding and its source share one register, so neither name
    /// may consume that register while the other name remains accessible.
    /// A `&mut` parameter is read once more than the body shows: every
    /// return path publishes its register back to the caller's write-back
    /// cell, so its value is never the last reader's to take.
    pub(crate) fn consumable_path<'a>(&self, expr: &'a HirExpr) -> Option<&'a str> {
        if let HirExprKind::Path { segments, .. } = &expr.kind
            && let [seg] = segments.as_slice()
            && self.consumable.contains(seg.name.as_str())
            && let Some(home) = self.lookup_local(&seg.name)
            && !self.reference_alias_regs.contains(&home.reg)
            && !self.mut_ref_params.contains(&home.reg)
        {
            Some(seg.name.as_str())
        } else {
            None
        }
    }

    /// `true` when `expr`'s compiled value register is safe to move out
    /// of (its last use is here). A single-segment path is safe only when
    /// the analysis proved the local is read exactly once here. The
    /// whitelisted expression forms always compile to a fresh result
    /// register that nothing else reads. `Block` / `if` / `match` / loop
    /// can forward an inner register that aliases a live local
    /// (`compile_block` returns its tail's register), so they are
    /// excluded.
    pub(crate) fn value_consumable_here(&self, expr: &HirExpr) -> bool {
        match &expr.kind {
            HirExprKind::Path { .. } => self.consumable_path(expr).is_some(),
            HirExprKind::Call { .. }
            | HirExprKind::MethodCall { .. }
            | HirExprKind::Binary { .. }
            | HirExprKind::Field { .. }
            | HirExprKind::TupleIndex { .. }
            | HirExprKind::Index { .. }
            | HirExprKind::Tuple(_)
            | HirExprKind::Array(_)
            | HirExprKind::Cast { .. } => true,
            _ => false,
        }
    }
}

/// Returns every enclosing binding a closure inside `body` reads, paired
/// with the type at the reading site. A name appears once per reading
/// site; the caller filters by type and collects the names it stores in
/// capture cells.
pub(crate) fn closure_captured_locals(params: &[HirParam], body: &HirBlock) -> Vec<(String, Ty)> {
    let mut a = Analyzer::default();
    a.push_scope();
    for param in params {
        a.record_bindings(&param.pattern, 0);
    }
    a.visit_block(body, 0, false);
    a.captured
}

/// [`closure_captured_locals`] for a closure body, which is a bare
/// expression rather than a block.
pub(crate) fn closure_captured_locals_in_expr(
    params: &[HirParam],
    body: &HirExpr,
) -> Vec<(String, Ty)> {
    let mut a = Analyzer::default();
    a.push_scope();
    for param in params {
        a.record_bindings(&param.pattern, 0);
    }
    a.visit_expr(body, 0, false, false);
    a.captured
}

/// Returns the set of local names that are safe to move at their single
/// use within `decl`'s body. Empty for bodyless declarations.
pub(crate) fn consumable_locals(decl: &HirFn) -> HashSet<String> {
    let Some(body) = decl.body.as_ref() else {
        return HashSet::new();
    };
    let mut a = Analyzer::default();
    // Outer scope for the function parameters; `visit_block` pushes its
    // own scope for the body's bindings.
    a.push_scope();
    for param in &decl.params {
        a.record_bindings(&param.pattern, 0);
    }
    a.visit_block(&body.block, 0, false);
    a.finish()
}

#[derive(Default)]
struct BlockLastUse {
    locals: HashSet<String>,
    duplicate_or_shadowed: HashSet<String>,
    captured: HashSet<String>,
    last_stmt: HashMap<String, usize>,
}

impl BlockLastUse {
    fn new(block: &HirBlock) -> Self {
        let mut this = Self::default();
        for stmt in &block.stmts {
            if let HirStmtKind::Let { pattern, .. } = &stmt.kind {
                let mut names = HashSet::new();
                collect_pattern_names(pattern, &mut names);
                for name in names {
                    if !this.locals.insert(name.clone()) {
                        this.duplicate_or_shadowed.insert(name);
                    }
                }
            }
        }
        this
    }

    fn record_path(
        &mut self,
        name: &str,
        stmt_idx: usize,
        shadowed: &HashSet<String>,
        captured: bool,
    ) {
        if !self.locals.contains(name) || shadowed.contains(name) {
            return;
        }
        if captured {
            self.captured.insert(name.to_string());
        } else {
            self.last_stmt.insert(name.to_string(), stmt_idx);
        }
    }

    fn visit_stmt(
        &mut self,
        stmt: &HirStmt,
        stmt_idx: usize,
        shadowed: &mut HashSet<String>,
        captured: bool,
    ) {
        match &stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                if let Some(init) = init {
                    self.visit_expr(init, stmt_idx, shadowed, captured);
                }
                self.shadow_pattern(pattern, shadowed);
            }
            HirStmtKind::Expr { expr, .. } => self.visit_expr(expr, stmt_idx, shadowed, captured),
            HirStmtKind::Defer(expr) => self.visit_expr(expr, stmt_idx, shadowed, true),
            HirStmtKind::Item(_) => {}
        }
    }

    fn visit_top_stmt(&mut self, stmt: &HirStmt, stmt_idx: usize) {
        let mut shadowed = HashSet::new();
        match &stmt.kind {
            HirStmtKind::Let { init, .. } => {
                if let Some(init) = init {
                    self.visit_expr(init, stmt_idx, &mut shadowed, false);
                }
            }
            HirStmtKind::Expr { expr, .. } => {
                self.visit_expr(expr, stmt_idx, &mut shadowed, false);
            }
            HirStmtKind::Defer(expr) => self.visit_expr(expr, stmt_idx, &mut shadowed, true),
            HirStmtKind::Item(_) => {}
        }
    }

    fn visit_block(
        &mut self,
        block: &HirBlock,
        stmt_idx: usize,
        shadowed: &mut HashSet<String>,
        captured: bool,
    ) {
        let snapshot = shadowed.clone();
        for stmt in &block.stmts {
            self.visit_stmt(stmt, stmt_idx, shadowed, captured);
        }
        if let Some(tail) = &block.tail {
            self.visit_expr(tail, stmt_idx, shadowed, captured);
        }
        *shadowed = snapshot;
    }

    fn shadow_pattern(&mut self, pattern: &HirPat, shadowed: &mut HashSet<String>) {
        let mut names = HashSet::new();
        collect_pattern_names(pattern, &mut names);
        for name in names {
            if self.locals.contains(&name) {
                shadowed.insert(name.clone());
                self.duplicate_or_shadowed.insert(name);
            }
        }
    }

    fn visit_expr(
        &mut self,
        expr: &HirExpr,
        stmt_idx: usize,
        shadowed: &mut HashSet<String>,
        captured: bool,
    ) {
        match &expr.kind {
            HirExprKind::Path { segments, .. } => {
                if let [seg] = segments.as_slice() {
                    self.record_path(&seg.name, stmt_idx, shadowed, captured);
                }
            }
            HirExprKind::Call { callee, args } => {
                self.visit_expr(callee, stmt_idx, shadowed, captured);
                for arg in args {
                    self.visit_expr(arg, stmt_idx, shadowed, captured);
                }
            }
            HirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver, stmt_idx, shadowed, captured);
                for arg in args {
                    self.visit_expr(arg, stmt_idx, shadowed, captured);
                }
            }
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.visit_expr(receiver, stmt_idx, shadowed, captured);
            }
            HirExprKind::Index { base, index } => {
                self.visit_expr(base, stmt_idx, shadowed, captured);
                self.visit_expr(index, stmt_idx, shadowed, captured);
            }
            HirExprKind::Unary { operand, .. } | HirExprKind::Cast { value: operand, .. } => {
                self.visit_expr(operand, stmt_idx, shadowed, captured);
            }
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.visit_expr(lhs, stmt_idx, shadowed, captured);
                self.visit_expr(rhs, stmt_idx, shadowed, captured);
            }
            HirExprKind::Assign { place, value } => {
                self.visit_expr(place, stmt_idx, shadowed, captured);
                self.visit_expr(value, stmt_idx, shadowed, captured);
            }
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition, stmt_idx, shadowed, captured);
                self.visit_expr(then_branch, stmt_idx, shadowed, captured);
                if let Some(else_branch) = else_branch {
                    self.visit_expr(else_branch, stmt_idx, shadowed, captured);
                }
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.visit_expr(scrutinee, stmt_idx, shadowed, captured);
                for arm in arms {
                    let snapshot = shadowed.clone();
                    self.shadow_pattern(&arm.pattern, shadowed);
                    if let Some(guard) = &arm.guard {
                        self.visit_expr(guard, stmt_idx, shadowed, captured);
                    }
                    self.visit_expr(&arm.body, stmt_idx, shadowed, captured);
                    *shadowed = snapshot;
                }
            }
            HirExprKind::Loop { body, .. } => self.visit_expr(body, stmt_idx, shadowed, captured),
            HirExprKind::While {
                condition, body, ..
            } => {
                self.visit_expr(condition, stmt_idx, shadowed, captured);
                self.visit_expr(body, stmt_idx, shadowed, captured);
            }
            HirExprKind::Block(block) => self.visit_block(block, stmt_idx, shadowed, captured),
            HirExprKind::Closure { params, body, .. } => {
                let snapshot = shadowed.clone();
                for param in params {
                    self.shadow_pattern(&param.pattern, shadowed);
                }
                self.visit_expr(body, stmt_idx, shadowed, true);
                *shadowed = snapshot;
            }
            HirExprKind::LiftedClosure { captures, .. } => {
                for cap in captures {
                    self.visit_expr(cap, stmt_idx, shadowed, true);
                }
            }
            HirExprKind::Tuple(elems) => {
                for elem in elems {
                    self.visit_expr(elem, stmt_idx, shadowed, captured);
                }
            }
            HirExprKind::Array(arr) => match arr {
                HirArrayExpr::List(elems) => {
                    for elem in elems {
                        self.visit_expr(elem, stmt_idx, shadowed, captured);
                    }
                }
                HirArrayExpr::Repeat { value, count } => {
                    self.visit_expr(value, stmt_idx, shadowed, captured);
                    self.visit_expr(count, stmt_idx, shadowed, captured);
                }
            },
            HirExprKind::Range { start, end, .. } => {
                if let Some(start) = start {
                    self.visit_expr(start, stmt_idx, shadowed, captured);
                }
                if let Some(end) = end {
                    self.visit_expr(end, stmt_idx, shadowed, captured);
                }
            }
            HirExprKind::Return(value) | HirExprKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.visit_expr(value, stmt_idx, shadowed, captured);
                }
            }
            HirExprKind::Select { arms } => {
                for arm in arms {
                    let snapshot = shadowed.clone();
                    match &arm.op {
                        gossamer_hir::HirSelectOp::Recv { pattern, channel } => {
                            self.visit_expr(channel, stmt_idx, shadowed, captured);
                            self.shadow_pattern(pattern, shadowed);
                        }
                        gossamer_hir::HirSelectOp::Send { channel, value } => {
                            self.visit_expr(channel, stmt_idx, shadowed, captured);
                            self.visit_expr(value, stmt_idx, shadowed, captured);
                        }
                        gossamer_hir::HirSelectOp::Default => {}
                    }
                    self.visit_expr(&arm.body, stmt_idx, shadowed, captured);
                    *shadowed = snapshot;
                }
            }
            HirExprKind::Literal(_) | HirExprKind::Continue { .. } | HirExprKind::Placeholder => {}
        }
    }

    fn finish(mut self, block: &HirBlock) -> Vec<Vec<String>> {
        if let Some(tail) = &block.tail {
            let mut shadowed = HashSet::new();
            self.visit_expr(tail, block.stmts.len(), &mut shadowed, false);
            for name in self.locals.clone() {
                if self.last_stmt.get(&name).copied() == Some(block.stmts.len()) {
                    self.captured.insert(name);
                }
            }
        }
        let mut clears = vec![Vec::new(); block.stmts.len()];
        for (name, idx) in self.last_stmt {
            if self.duplicate_or_shadowed.contains(&name) || self.captured.contains(&name) {
                continue;
            }
            if let Some(slot) = clears.get_mut(idx) {
                slot.push(name);
            }
        }
        clears
    }
}

/// For each statement in `block`, returns value-local names that can be cleared
/// immediately after that statement because their final read in the block has
/// completed. This is deliberately conservative: shadowing, captures, defers,
/// and tail-expression uses keep a name live until normal scope exit.
pub(crate) fn block_last_use_clears(block: &HirBlock) -> Vec<Vec<String>> {
    let mut a = BlockLastUse::new(block);
    for (idx, stmt) in block.stmts.iter().enumerate() {
        a.visit_top_stmt(stmt, idx);
    }
    a.finish(block)
}
