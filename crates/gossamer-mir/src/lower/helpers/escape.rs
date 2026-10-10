//! Conservative escape analysis driving automatic arena regions.
//!
//! A loop body whose allocations provably do not outlive the iteration can
//! be wrapped in an arena region (`gos_rt_arena_push` .. `arena_pop`) so
//! the whole iteration's heap is reclaimed in one bulk free instead of a
//! per-node reference-count teardown. This is what lets idiomatic
//! allocation-churn code (build a tree, consume it, discard) approach a
//! tracing GC's throughput without the user writing a single annotation.
//!
//! Soundness is the entire game: regioning a loop whose allocations DO
//! escape is a use-after-free. Every check here is a conservative
//! over-approximation - when in doubt, the loop is NOT regioned.

use std::collections::{HashMap, HashSet};

use gossamer_hir::{HirBlock, HirExpr, HirExprKind, HirItemKind, HirProgram, HirStmt, HirStmtKind};
use gossamer_resolve::DefId;
use gossamer_types::{ParamIdx, Ty, TyCtxt, TyKind};

use super::effects::{
    ProgramEffects, STATELESS_STD_MODULES, StdCall, is_automatic_method, is_builtin_data, std_call,
    strands_heap_child,
};
use super::for_loop::detect_for_loop;
use gossamer_types::is_mutating_method_name;

/// True for types whose values carry no heap ownership - copying or
/// dropping them frees nothing, so they can flow out of a region freely.
pub(crate) fn is_copy_ty(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind_of(ty),
        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Unit
    )
}

/// Every identifier a pattern binds, walking tuple / variant / struct / ref /
/// `@` sub-patterns so a destructured `let a, b = …` registers both names.
pub(crate) fn pat_binding_names(pat: &gossamer_hir::HirPat, out: &mut Vec<String>) {
    use gossamer_hir::HirPatKind;
    match &pat.kind {
        HirPatKind::Binding { name, .. } => out.push(name.name.clone()),
        HirPatKind::At { name, sub, .. } => {
            out.push(name.name.clone());
            pat_binding_names(sub, out);
        }
        HirPatKind::Tuple(parts) | HirPatKind::Variant { fields: parts, .. } => {
            for p in parts {
                pat_binding_names(p, out);
            }
        }
        HirPatKind::Slice {
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
        HirPatKind::Struct { fields, .. } => {
            for f in fields {
                match &f.pattern {
                    Some(p) => pat_binding_names(p, out),
                    // Shorthand `Foo { x }` binds the field name itself.
                    None => out.push(f.name.name.clone()),
                }
            }
        }
        HirPatKind::Ref { inner, .. } => pat_binding_names(inner, out),
        HirPatKind::Or(alts) => {
            // Every arm of an or-pattern binds the same names; one arm suffices.
            if let Some(first) = alts.first() {
                pat_binding_names(first, out);
            }
        }
        HirPatKind::Wildcard
        | HirPatKind::Literal(_)
        | HirPatKind::Rest
        | HirPatKind::Range { .. } => {}
    }
}

/// Root path name of a place expression (`x`, `*x`, `x.f`, `x[i]`, `&x`).
fn place_root_name(expr: &HirExpr) -> Option<&str> {
    match &expr.kind {
        HirExprKind::Path { segments, .. } => segments.first().map(|s| s.name.as_str()),
        HirExprKind::Unary { operand, .. } => place_root_name(operand),
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            place_root_name(receiver)
        }
        HirExprKind::Index { base, .. } => place_root_name(base),
        _ => None,
    }
}

/// Why a loop body that allocates was not auto-regioned. Surfaced through
/// `GOS_ARENA_TRACE` so the otherwise-silent slow path (the value lives on
/// the per-node RC teardown instead of an O(1) bulk free) becomes
/// debuggable, and the user knows where a manual `arena { }` would pay off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionReject {
    EarlyExitOrCapture,
    NestedLoop,
    MethodCall,
    EscapingArg,
    OuterShare,
    HeapAssign,
    UnsafeCallee,
    UnresolvedCallee,
    DeferGoOrItem,
    StrandedChild,
}

impl RegionReject {
    /// One-line, user-facing explanation of the rejection.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::EarlyExitOrCapture => {
                "body leaves the loop with a heap value, or has a closure or select (would carry a region value out)"
            }
            Self::NestedLoop => {
                "body contains a nested loop that allocates without a region of its own"
            }
            Self::MethodCall => {
                "body mutates a value from outside the loop through a method, or calls a method nothing vets"
            }
            Self::EscapingArg => {
                "body hands a value from outside the loop to a call that may keep it or write into it"
            }
            Self::OuterShare => "body takes a share of a value from outside the loop out of a call",
            Self::HeapAssign => {
                "body assigns a heap value into a binding that outlives the iteration"
            }
            Self::UnsafeCallee => {
                "body calls a function that may let a value escape (a goroutine, channel, static, or closure)"
            }
            Self::UnresolvedCallee => "body calls through an unresolved/indirect callee",
            Self::DeferGoOrItem => "body contains a defer, go, or nested item",
            Self::StrandedChild => {
                "body builds a value that keeps a runtime handle (a set, deque, channel, lock, or file) inside region storage, which the bulk free would never release"
            }
        }
    }
}

/// The auto-region decision for a loop body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegionDecision {
    /// Eligible: the body allocates and provably nothing escapes.
    Region,
    /// The body allocates a heap value each iteration but was rejected - the
    /// per-iteration heap is torn down node-by-node instead of bulk-freed.
    /// The detail names the construct that decided it, when there is one.
    Reject(RegionReject, Option<String>),
    /// The body allocates nothing, so a region would be pure overhead.
    NoAlloc,
}

/// Where a value inside a region body may have its storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// A `Copy` value, or one built during this iteration from nothing that
    /// outlives it: region storage that holds no share of an outer value.
    Fresh,
    /// May be, or share, storage that existed before the iteration began.
    Outer,
}

impl Origin {
    fn join(self, other: Self) -> Self {
        if self == Self::Outer || other == Self::Outer {
            Self::Outer
        } else {
            Self::Fresh
        }
    }
}

/// Walks a loop body and decides whether it is safe to wrap in an arena
/// region.
///
/// A region is bulk-freed at its pop without a per-object teardown, so two
/// things must hold. No region value may be reachable once the iteration
/// ends: nothing built here is stored into, returned to, or broken out to
/// storage that outlives it. And no region object may hold a share of an
/// outer value: the bulk free never gives that share back. Every binding the
/// body introduces is therefore classified by [`Origin`], and a value from
/// outside may be read, bound, or handed to a callee whose summary leaves it
/// untouched, but never placed inside anything the region builds.
pub(crate) struct LoopEligibility<'a> {
    tcx: &'a TyCtxt,
    effects: &'a ProgramEffects,
    /// Bindings introduced inside the body. A name absent here is from
    /// outside the body.
    locals: HashMap<String, Origin>,
    /// Labels of the loops nested inside the body, innermost last. A `break`
    /// or `continue` that targets one of them stays inside the body.
    inner_loops: Vec<Option<String>>,
    /// Whether an exit that leaves the body is admitted. Lowering pops a
    /// loop's region on every edge out of its body; a lexical block or a
    /// closure body pops only at its fall-through.
    exits_allowed: bool,
    ok: bool,
    /// True once the body is seen to allocate a heap value (a call returning a
    /// heap type, etc.). A region only pays off if there is something to arena;
    /// a purely-scalar body (a counter scan, byte stores) must NOT be wrapped,
    /// or every iteration pays two `arena_push`/`arena_pop` calls for nothing.
    allocates: bool,
    /// First rejection reason and the construct behind it, for the
    /// `GOS_ARENA_TRACE` diagnostic. Only meaningful once `ok` is false.
    reject: Option<(RegionReject, Option<String>)>,
}

impl<'a> LoopEligibility<'a> {
    pub fn new(tcx: &'a TyCtxt, effects: &'a ProgramEffects) -> Self {
        Self {
            tcx,
            effects,
            locals: HashMap::new(),
            inner_loops: Vec::new(),
            exits_allowed: false,
            ok: true,
            allocates: false,
            reject: None,
        }
    }

    /// Marks the body ineligible and records the first rejection reason.
    fn reject(&mut self, r: RegionReject) {
        self.reject_at(r, None);
    }

    fn reject_at(&mut self, r: RegionReject, detail: Option<String>) {
        self.ok = false;
        if self.reject.is_none() {
            self.reject = Some((r, detail));
        }
    }

    /// A call/expression result type that lives on the heap (so wrapping the
    /// body in an arena region can bulk-free it). Scalars / unit / refs do not.
    ///
    /// A tuple lives on the heap only if it carries a heap element: a tuple of
    /// scalars (e.g. an `lcg(s) -> (i64, i64)` result) is returned in registers
    /// / an sret slot on the compiled tiers and never allocates, so wrapping
    /// its loop in a region frees nothing and only pays two `arena_push`/
    /// `arena_pop` calls per iteration - exactly the "purely-scalar body must
    /// not be wrapped" case this analysis exists to avoid.
    fn is_alloc_ty(&self, ty: Ty) -> bool {
        match self.tcx.kind_of(ty) {
            TyKind::Adt { .. }
            | TyKind::Vec(_)
            | TyKind::Slice(_)
            | TyKind::HashMap { .. }
            | TyKind::String
            | TyKind::DynError
            | TyKind::JsonValue => true,
            TyKind::Tuple(elems) => elems.iter().any(|t| self.is_alloc_ty(*t)),
            _ => false,
        }
    }

    fn verdict(&self) -> RegionDecision {
        match (self.ok, self.allocates) {
            (true, true) => RegionDecision::Region,
            (_, false) => RegionDecision::NoAlloc,
            (false, true) => {
                let (reason, detail) = self
                    .reject
                    .clone()
                    .unwrap_or((RegionReject::EarlyExitOrCapture, None));
                RegionDecision::Reject(reason, detail)
            }
        }
    }

    /// Decides whether `body` (a loop body expression) is region-eligible,
    /// reporting *why* an allocating body was rejected so `GOS_ARENA_TRACE`
    /// can explain the perf cliff. `Region` iff the body allocates and
    /// provably nothing escapes.
    pub fn decide(mut self, body: &HirExpr) -> RegionDecision {
        // The walk visits the whole body (it does not stop at the first
        // rejection), so `allocates` is accurate even when `ok` is already
        // false - letting us tell a real perf cliff (allocates + rejected)
        // from an irrelevant scalar loop. `ok` is monotone, so visiting extra
        // nodes after a rejection cannot change the eligibility verdict.
        self.exits_allowed = true;
        self.expr(body);
        self.verdict()
    }

    /// Decides whether a nested lexical block can own one automatic region.
    /// Unlike a loop body, its tail is observed by the enclosing expression,
    /// so a non-Copy tail would let a region pointer escape through the block
    /// result. Function bodies deliberately do not use this entry point: a
    /// returned value has the same escape hazard.
    pub fn decide_lexical_block(mut self, block: &HirBlock) -> RegionDecision {
        self.block(block);
        if block
            .tail
            .as_ref()
            .is_some_and(|tail| !is_copy_ty(self.tcx, tail.ty))
        {
            self.reject(RegionReject::EarlyExitOrCapture);
        }
        self.verdict()
    }

    fn block(&mut self, b: &HirBlock) -> Origin {
        let saved = self.locals.clone();
        for s in &b.stmts {
            match &s.kind {
                HirStmtKind::Let { pattern, init, .. } => {
                    let origin = init.as_ref().map_or(Origin::Fresh, |e| self.expr(e));
                    // A lifted closure's prologue binds each captured value
                    // through an env load. The binding names a value the
                    // enclosing scope owns.
                    let origin = if init.as_ref().is_some_and(gossamer_hir::is_capture_env_load) {
                        Origin::Outer
                    } else {
                        origin
                    };
                    self.bind(pattern, origin);
                }
                HirStmtKind::Expr { expr, .. } => {
                    self.expr(expr);
                }
                HirStmtKind::Defer(_) | HirStmtKind::Item(_) => {
                    self.reject(RegionReject::DeferGoOrItem);
                }
            }
        }
        let origin = b.tail.as_ref().map_or(Origin::Fresh, |t| self.expr(t));
        self.locals = saved;
        origin
    }

    fn bind(&mut self, pattern: &gossamer_hir::HirPat, origin: Origin) {
        let mut names = Vec::new();
        pat_binding_names(pattern, &mut names);
        for name in names {
            self.locals.insert(name, origin);
        }
    }

    /// The origin of a value with `ty` derived from parts of `origin`.
    fn of_type(&self, ty: Ty, origin: Origin) -> Origin {
        if is_copy_ty(self.tcx, ty) {
            Origin::Fresh
        } else {
            origin
        }
    }

    /// Rejects a body that builds a value of `ty` keeping a heap-only object
    /// inside region storage.
    fn reject_stranding(&mut self, ty: Ty) {
        if strands_heap_child(self.tcx, ty) {
            self.reject(RegionReject::StrandedChild);
        }
    }

    /// Requires a non-Copy value placed inside something the region builds
    /// to be fresh, so no region object ends up holding an outer share.
    fn require_fresh(&mut self, arg: &HirExpr, origin: Origin) {
        if origin == Origin::Outer && !is_copy_ty(self.tcx, arg.ty) {
            self.reject(RegionReject::EscapingArg);
        }
    }

    /// Whether `e` is a place rooted in a binding the body built fresh.
    fn fresh_place(&self, e: &HirExpr) -> bool {
        place_root_name(e).is_some_and(|root| self.locals.get(root) == Some(&Origin::Fresh))
    }

    /// Walks `e`, checking every rule, and answers the origin of its value.
    fn expr(&mut self, e: &HirExpr) -> Origin {
        let origin = self.expr_inner(e);
        self.of_type(e.ty, origin)
    }

    fn expr_inner(&mut self, e: &HirExpr) -> Origin {
        match &e.kind {
            HirExprKind::Literal(_) | HirExprKind::Placeholder => Origin::Fresh,
            HirExprKind::Path { segments, .. } => match segments.as_slice() {
                [only] => self
                    .locals
                    .get(&only.name)
                    .copied()
                    .unwrap_or(Origin::Outer),
                _ => Origin::Outer,
            },
            HirExprKind::Return(value) => {
                let value_origin = value.as_ref().map(|v| (self.expr(v), v.ty));
                self.exit(value_origin);
                Origin::Fresh
            }
            HirExprKind::Break { value, label } => {
                let value_origin = value.as_ref().map(|v| (self.expr(v), v.ty));
                if !self.targets_inner_loop(label.as_deref()) {
                    self.exit(value_origin);
                }
                Origin::Fresh
            }
            HirExprKind::Continue { label } => {
                if !self.targets_inner_loop(label.as_deref()) {
                    self.exit(None);
                }
                Origin::Fresh
            }
            HirExprKind::Select { .. }
            | HirExprKind::Closure { .. }
            | HirExprKind::LiftedClosure { .. } => {
                self.reject(RegionReject::EarlyExitOrCapture);
                Origin::Outer
            }
            HirExprKind::Loop { body, label } => {
                let shape = detect_for_loop(body);
                let inner_body = shape.as_ref().map_or(&**body, |s| s.body);
                self.nested_loop(inner_body, label.clone(), |this| {
                    if let Some(shape) = &shape {
                        let iter = this.expr(shape.iter_expr);
                        this.bind(shape.loop_pat, iter);
                    }
                    this.expr(inner_body);
                });
                Origin::Outer
            }
            HirExprKind::While {
                condition,
                body,
                label,
            } => {
                self.nested_loop(body, label.clone(), |this| {
                    this.expr(condition);
                    this.expr(body);
                });
                Origin::Fresh
            }
            // Projecting a captured value out of the closure environment is a
            // load: it allocates nothing and cannot retain a region pointer.
            HirExprKind::Call { .. } if gossamer_hir::is_capture_env_load(e) => Origin::Outer,
            HirExprKind::Call { callee, args } => self.call(e, callee, args),
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => self.method_call(e, receiver, &name.name, args),
            HirExprKind::Assign { place, value } => {
                let value_origin = self.expr(value);
                self.expr(place);
                if !is_copy_ty(self.tcx, place.ty)
                    && !(self.fresh_place(place) && value_origin == Origin::Fresh)
                {
                    self.reject(RegionReject::HeapAssign);
                }
                Origin::Fresh
            }
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.expr(receiver)
            }
            HirExprKind::Index { base, index } => {
                self.expr(index);
                self.expr(base)
            }
            HirExprKind::Unary { operand, .. } => self.expr(operand),
            HirExprKind::Cast { value, .. } => self.expr(value),
            HirExprKind::Binary { lhs, rhs, .. } => {
                let l = self.expr(lhs);
                let r = self.expr(rhs);
                l.join(r)
            }
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition);
                let t = self.expr(then_branch);
                let f = else_branch.as_ref().map_or(Origin::Fresh, |b| self.expr(b));
                t.join(f)
            }
            HirExprKind::Match { scrutinee, arms } => {
                let scrutinee_origin = self.expr(scrutinee);
                let mut origin = Origin::Fresh;
                for arm in arms {
                    let saved = self.locals.clone();
                    self.bind(&arm.pattern, scrutinee_origin);
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    origin = origin.join(self.expr(&arm.body));
                    self.locals = saved;
                }
                origin
            }
            HirExprKind::Block(b) => self.block(b),
            HirExprKind::Tuple(items) => items
                .iter()
                .fold(Origin::Fresh, |acc, i| acc.join(self.expr(i))),
            HirExprKind::Array(arr) => {
                if self.is_alloc_ty(e.ty) {
                    self.allocates = true;
                }
                self.reject_stranding(e.ty);
                let heap = matches!(self.tcx.kind_of(e.ty), TyKind::Vec(_) | TyKind::Slice(_));
                let items: Vec<&HirExpr> = match arr {
                    gossamer_hir::HirArrayExpr::List(items) => items.iter().collect(),
                    gossamer_hir::HirArrayExpr::Repeat { value, count } => {
                        self.expr(count);
                        vec![&**value]
                    }
                };
                let mut origin = Origin::Fresh;
                for item in items {
                    let o = self.expr(item);
                    if heap {
                        self.require_fresh(item, o);
                    }
                    origin = origin.join(o);
                }
                origin
            }
            HirExprKind::Range { start, end, .. } => {
                let s = start.as_ref().map_or(Origin::Fresh, |s| self.expr(s));
                let en = end.as_ref().map_or(Origin::Fresh, |en| self.expr(en));
                s.join(en)
            }
        }
    }

    /// A `break`, `continue`, or `return` leaving the body. Lowering pops the
    /// region on the edge, so the edge is admitted when the body is a loop
    /// body and the value it carries out holds no region storage.
    fn exit(&mut self, value: Option<(Origin, Ty)>) {
        if !self.exits_allowed {
            self.reject(RegionReject::EarlyExitOrCapture);
            return;
        }
        if let Some((_, ty)) = value
            && !is_copy_ty(self.tcx, ty)
        {
            self.reject(RegionReject::EarlyExitOrCapture);
        }
    }

    fn targets_inner_loop(&self, label: Option<&str>) -> bool {
        match label {
            None => !self.inner_loops.is_empty(),
            Some(l) => self
                .inner_loops
                .iter()
                .any(|inner| inner.as_deref() == Some(l)),
        }
    }

    /// A loop nested in the body. It is admitted when its own body is
    /// regioned or allocates nothing - so each inner iteration's temporaries
    /// are freed as they would be without the outer region - and when it
    /// keeps every rule of the outer body too.
    fn nested_loop(
        &mut self,
        inner_body: &HirExpr,
        label: Option<String>,
        walk: impl FnOnce(&mut Self),
    ) {
        let inner = LoopEligibility::new(self.tcx, self.effects).decide(inner_body);
        if matches!(inner, RegionDecision::Reject(..)) {
            self.reject(RegionReject::NestedLoop);
        }
        // What the inner loop allocates lands in its own region, so it does
        // not make the outer body worth a region of its own.
        let allocates = self.allocates;
        let saved = self.locals.clone();
        self.inner_loops.push(label);
        walk(self);
        self.inner_loops.pop();
        self.locals = saved;
        self.allocates = allocates;
    }

    fn call(&mut self, e: &HirExpr, callee: &HirExpr, args: &[HirExpr]) -> Origin {
        if self.is_alloc_ty(e.ty) {
            self.allocates = true;
        }
        self.reject_stranding(e.ty);
        let origins: Vec<Origin> = args.iter().map(|a| self.expr(a)).collect();
        match &callee.kind {
            HirExprKind::Path {
                def: Some(d),
                segments,
            } => {
                if let Some(summary) = self.effects.of_fn(*d) {
                    if summary.escapes {
                        self.reject_at(RegionReject::UnsafeCallee, Some(path_text(segments)));
                    }
                    if summary.strands {
                        self.reject_at(RegionReject::StrandedChild, Some(path_text(segments)));
                    }
                    for (pos, (arg, origin)) in args.iter().zip(&origins).enumerate() {
                        if *origin == Origin::Outer
                            && !is_copy_ty(self.tcx, arg.ty)
                            && !summary.param(pos).leaves_untouched()
                        {
                            self.reject_at(RegionReject::EscapingArg, Some(path_text(segments)));
                        }
                    }
                } else {
                    // A variant or tuple-struct constructor builds a value
                    // holding its arguments.
                    for (arg, origin) in args.iter().zip(&origins) {
                        self.require_fresh(arg, *origin);
                    }
                }
                Origin::Fresh
            }
            HirExprKind::Path {
                def: None,
                segments,
            } => match std_call(segments) {
                StdCall::Reads => Origin::Fresh,
                StdCall::Projects => {
                    let origin = origins.iter().fold(Origin::Fresh, |a, o| a.join(*o));
                    if origin == Origin::Outer && !is_copy_ty(self.tcx, e.ty) {
                        self.reject_at(RegionReject::OuterShare, Some(path_text(segments)));
                    }
                    origin
                }
                StdCall::Builds => {
                    for (arg, origin) in args.iter().zip(&origins) {
                        self.require_fresh(arg, *origin);
                    }
                    Origin::Fresh
                }
                StdCall::Unknown => {
                    self.reject_at(RegionReject::UnresolvedCallee, Some(path_text(segments)));
                    Origin::Outer
                }
            },
            _ => {
                self.expr(callee);
                self.reject(RegionReject::UnresolvedCallee);
                Origin::Outer
            }
        }
    }

    fn method_call(
        &mut self,
        e: &HirExpr,
        receiver: &HirExpr,
        name: &str,
        args: &[HirExpr],
    ) -> Origin {
        if self.is_alloc_ty(e.ty) {
            self.allocates = true;
        }
        self.reject_stranding(e.ty);
        let recv_origin = self.expr(receiver);
        let origins: Vec<Origin> = args.iter().map(|a| self.expr(a)).collect();
        let arity = args.len() + 1;
        let builtin = is_builtin_data(self.tcx, receiver.ty) || is_automatic_method(name);
        let detail = || Some(format!(".{name}()"));
        if let Some(summary) = self
            .effects
            .of_method_call(self.tcx, receiver.ty, name, arity)
        {
            if summary.escapes {
                self.reject_at(RegionReject::UnsafeCallee, detail());
            }
            if summary.strands {
                self.reject_at(RegionReject::StrandedChild, detail());
            }
            let operands = std::iter::once((receiver, recv_origin))
                .chain(args.iter().zip(origins.iter().copied()));
            for (pos, (operand, origin)) in operands.enumerate() {
                if origin == Origin::Outer
                    && !is_copy_ty(self.tcx, operand.ty)
                    && !summary.param(pos).leaves_untouched()
                {
                    self.reject_at(RegionReject::EscapingArg, detail());
                }
            }
        } else if !builtin {
            self.reject_at(RegionReject::MethodCall, detail());
            return Origin::Outer;
        }
        if !builtin {
            return Origin::Fresh;
        }
        if is_mutating_method_name(name) {
            // Growing an outer container allocates its new storage in the
            // region, and an argument stored into a fresh one must not hold
            // an outer share.
            if !self.fresh_place(receiver) && !is_copy_ty(self.tcx, receiver.ty) {
                self.reject_at(RegionReject::MethodCall, detail());
            }
            for (arg, origin) in args.iter().zip(&origins) {
                self.require_fresh(arg, *origin);
            }
            return Origin::Fresh;
        }
        if cannot_carry(self.tcx, e.ty) && !matches!(self.tcx.kind_of(e.ty), TyKind::String) {
            return Origin::Fresh;
        }
        let origin = origins.iter().fold(recv_origin, |a, o| a.join(*o));
        if origin == Origin::Outer && !is_copy_ty(self.tcx, e.ty) {
            self.reject_at(RegionReject::OuterShare, detail());
        }
        origin
    }
}

/// A path as written, for a diagnostic.
fn path_text(segments: &[gossamer_ast::Ident]) -> String {
    segments
        .iter()
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join("::")
}

/// Per-parameter "the callee only reads this, and nothing derived from it
/// outlives the call" summary, keyed by callee.
///
/// A by-value container argument, and every growable field an aggregate one
/// carries, is cloned at the call site so the callee's value is its own. That
/// clone is observable only when the callee can write the parameter or let it
/// escape; when it can do neither, the argument may cross as the handle and
/// the copy is pure cost - which is quadratic when the caller passes a large
/// collection inside a loop, and scales with the field rather than with the
/// work when it passes a struct that holds one.
///
/// Conservative on every axis: the parameter must be an immutable binding of a
/// container or aggregate type, the callee must answer a value that cannot
/// carry it, and every mention of the name in the body must sit in a read-only
/// place position. Anything else - a mention as an argument, in the tail
/// expression, on either side of an assignment, inside a literal - keeps the
/// clone. A field read is the one projection that stays read-only: it reaches
/// the parameter's storage only when the field's own type can carry it.
/// Whether a parameter stays inside its call is a property of that parameter,
/// so it is answered per parameter: a helper that writes through one `&mut`
/// parameter still only reads the collection handed to another.
pub fn collect_shareable_params(
    program: &HirProgram,
    tcx: &TyCtxt,
) -> HashMap<DefId, Vec<ParamShare>> {
    let mut out: HashMap<DefId, Vec<ParamShare>> = HashMap::new();
    let mut pending: HashMap<DefId, Vec<Vec<(DefId, usize)>>> = HashMap::new();
    for item in &program.items {
        let HirItemKind::Fn(f) = &item.kind else {
            continue;
        };
        let (Some(def), Some(body)) = (item.def, &f.body) else {
            continue;
        };
        // Whether the answer's type can carry the parameter's storage out of
        // the call: a scalar or a `String` cannot, anything else can. It
        // decides what a mention in a returned expression means. A function
        // that answers a container still only reads a parameter it never
        // returns, which is the shape a worker that builds a fresh collection
        // from one it reads has.
        let ret_carries = !f.ret.is_none_or(|ret| {
            matches!(
                tcx.kind_of(ret),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Unit
            ) || matches!(tcx.kind_of(ret), TyKind::String)
        });
        let mut flags: Vec<ParamShare> = Vec::with_capacity(f.params.len());
        let mut param_forwards: Vec<Vec<(DefId, usize)>> = Vec::with_capacity(f.params.len());
        for p in &f.params {
            let mut forwards = Vec::new();
            let shareable = 'param: {
                let gossamer_hir::HirPatKind::Binding {
                    name,
                    mutable: false,
                } = &p.pattern.kind
                else {
                    break 'param ParamShare::Never;
                };
                if !matches!(
                    tcx.kind_of(p.ty),
                    TyKind::Vec(_)
                        | TyKind::Slice(_)
                        | TyKind::Array { .. }
                        | TyKind::HashMap { .. }
                        | TyKind::Adt { .. }
                        | TyKind::Tuple(_)
                ) {
                    break 'param ParamShare::Never;
                }
                let mut scan = ShareScan {
                    tcx,
                    names: vec![name.name.as_str().to_string()],
                    escaped: false,
                    ret_carries,
                    forwards: Vec::new(),
                    copy_params: Vec::new(),
                };
                scan.block(&body.block, false, ret_carries);
                forwards = scan.forwards;
                if scan.escaped {
                    ParamShare::Never
                } else {
                    ParamShare::WhenCopy(scan.copy_params)
                }
            };
            flags.push(shareable);
            param_forwards.push(forwards);
        }
        out.insert(def, flags);
        pending.insert(def, param_forwards);
    }
    // A parameter that is only forwarded is shareable exactly when every
    // position it reaches is. The pass starts from each body's own answer and
    // withdraws one whose target has been withdrawn, until nothing changes -
    // so a chain of read-only helpers stays shareable the whole way down, and
    // mutual recursion that only forwards settles rather than falsifying
    // itself.
    loop {
        let mut changed = false;
        for (def, forwards) in &pending {
            for (idx, targets) in forwards.iter().enumerate() {
                if out[def][idx] == ParamShare::Never {
                    continue;
                }
                // A forward keeps the parameter shareable only through a
                // target that shares unconditionally: a target's own type
                // parameters are not this body's, so its conditions do not
                // translate into conditions here.
                let reaches_unshareable = targets.iter().any(|(callee, pos)| {
                    out.get(callee).is_none_or(|flags| {
                        flags.get(*pos) != Some(&ParamShare::WhenCopy(Vec::new()))
                    })
                });
                if reaches_unshareable {
                    out.get_mut(def).expect("summary present")[idx] = ParamShare::Never;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    out
}

/// Whether a call reads its container arguments without keeping them: the
/// callee is a standard-library function of a stateless module, and the value
/// it answers has no field that could hold one of them.
fn std_call_reads_args(tcx: &TyCtxt, callee: &HirExpr, answer: Ty) -> bool {
    let HirExprKind::Path {
        segments,
        def: None,
    } = &callee.kind
    else {
        return false;
    };
    let [.., module, _] = segments.as_slice() else {
        return false;
    };
    STATELESS_STD_MODULES.contains(&module.name.as_str()) && cannot_carry(tcx, answer)
}

/// Whether a value of `ty` has nowhere to hold a container: a scalar, a
/// `String`, or an `Option`, `Result`, or tuple built only from those.
pub(crate) fn cannot_carry(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind_of(ty) {
        TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::Bool
        | TyKind::Char
        | TyKind::Unit
        | TyKind::String
        | TyKind::DynError => true,
        TyKind::Tuple(elems) => elems.iter().all(|t| cannot_carry(tcx, *t)),
        TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
            substs.types().iter().all(|t| cannot_carry(tcx, *t))
        }
        _ => false,
    }
}

/// Whether a parameter's storage stays inside its call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamShare {
    /// The callee may write the parameter or let it outlive the call.
    Never,
    /// The callee only reads the parameter once each listed type parameter is
    /// instantiated with a copy type; an empty list holds for every call.
    WhenCopy(Vec<ParamIdx>),
}

impl ParamShare {
    /// Whether the parameter is shared at a call instantiating the callee's
    /// type parameters with `type_args`, indexed by parameter position.
    #[must_use]
    pub fn holds(&self, tcx: &TyCtxt, type_args: &[Option<Ty>]) -> bool {
        match self {
            Self::Never => false,
            Self::WhenCopy(params) => params.iter().all(|idx| {
                type_args
                    .get(idx.0 as usize)
                    .copied()
                    .flatten()
                    .is_some_and(|ty| is_copy_ty(tcx, ty))
            }),
        }
    }
}

/// Methods whose arguments they only read: the argument's storage stays the
/// caller's, so handing a parameter to one is a read of that parameter rather
/// than a use that could keep it.
///
/// Every entry is a load-bearing claim in the same sense the non-capturing
/// runtime list is: a method that stored an argument, or answered something
/// that reaches it, would let a callee observe the caller's later writes.
const READ_ONLY_ARG_METHODS: &[&str] = &[
    "contains",
    "ends_with",
    "extend",
    "index_of",
    "push_json_quoted",
    "push_str",
    "push_utf8",
    "starts_with",
];

/// Sequence methods that answer the elements they read in a vector of its own.
const FRESH_COPY_METHODS: &[&str] = &["clone", "slice", "to_vec"];

/// Walks a body looking for any mention of one parameter outside a read-only
/// place position.
struct ShareScan<'a> {
    tcx: &'a TyCtxt,
    /// The parameter's name, plus every binding taken from a projection of it:
    /// `let row = grid[i]` names the parameter's own element, so a use of
    /// `row` reaches the parameter's storage exactly as `grid[i]` does.
    names: Vec<String>,
    escaped: bool,
    /// Whether this function's return type can carry the parameter's storage:
    /// a scalar or a `String` answer cannot, anything else can. It decides
    /// what a mention in a returned expression means, and nothing else - a
    /// mention elsewhere in the body is judged by the same read-only place
    /// rule whatever the function answers.
    ret_carries: bool,
    /// `(callee, parameter index)` for every use that is the whole argument
    /// of a direct call. Reading the parameter through a callee that only
    /// reads its own is still only reading, so the use is answered by that
    /// callee's summary rather than by giving up here - which is what lets a
    /// helper hand its collection to another helper without the caller
    /// copying it first.
    forwards: Vec<(DefId, usize)>,
    /// Type parameters the walk read as copies. A generic body is summarised
    /// once for all its instantiations, and a value typed by one of these is a
    /// copy exactly in the instantiations that choose a copy type for it, so
    /// the answer holds for those.
    copy_params: Vec<ParamIdx>,
}

impl ShareScan<'_> {
    /// Whether a value of `ty` is a copy, reading a type parameter as one and
    /// recording it as a condition of the answer.
    fn copy(&mut self, ty: Ty) -> bool {
        if let TyKind::Param { idx, .. } = self.tcx.kind_of(ty) {
            if !self.copy_params.contains(idx) {
                self.copy_params.push(*idx);
            }
            return true;
        }
        is_copy_ty(self.tcx, ty)
    }

    /// Whether `receiver.method(..)` answers elements copied out of the
    /// receiver into storage of their own, sharing nothing with it: a copy of
    /// a sequence whose elements are scalars or strings, or one such element
    /// through `next`, the walk a `for` loop over a sequence drives.
    fn answers_fresh_copy(&mut self, receiver: &HirExpr, method: &str) -> bool {
        if !FRESH_COPY_METHODS.contains(&method) && method != "next" {
            return false;
        }
        let elem = match self.tcx.kind_of(receiver.ty) {
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => *elem,
            _ => return false,
        };
        matches!(self.tcx.kind_of(elem), TyKind::String) || self.copy(elem)
    }

    fn block(&mut self, b: &HirBlock, place: bool, returned: bool) {
        for s in &b.stmts {
            match &s.kind {
                HirStmtKind::Let { pattern, init, .. } => {
                    if let Some(e) = init {
                        // A binding taken straight out of the parameter names
                        // the same storage, so it joins the tracked set rather
                        // than counting as a use: the walk then judges what the
                        // body does with it. A scalar element is the exception
                        // - `let byte = buf[i]` binds a copy of the byte, which
                        // reaches none of the parameter's storage - so it is
                        // walked as the read it is.
                        if let gossamer_hir::HirPatKind::Binding {
                            name,
                            mutable: false,
                        } = &pattern.kind
                            && self.projection_of_tracked(e)
                            && !self.copy(e.ty)
                        {
                            self.walk_projection_indices(e);
                            self.names.push(name.name.as_str().to_string());
                            continue;
                        }
                        self.expr(e, false, false);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                    self.expr(expr, false, false);
                }
                HirStmtKind::Item(_) => {}
            }
        }
        if let Some(t) = &b.tail {
            // A tail expression is the returned value.
            self.expr(t, place, returned);
        }
    }

    /// Walks `e`. `place` says the value sits where reading the parameter
    /// keeps its storage inside the call; `returned` says the value leaves the
    /// call as the answer, where a read of the parameter's storage hands that
    /// storage to the caller and so is an escape however it was projected.
    fn expr(&mut self, e: &HirExpr, place: bool, returned: bool) {
        if self.escaped {
            return;
        }
        // A field or element read off the parameter yields something that
        // still reaches its storage whenever the read's own type can hold a
        // reference, so it is judged where it sits - exactly as a bare mention
        // is. A scalar read is a copy and falls through to the walk below.
        if !matches!(e.kind, HirExprKind::Path { .. })
            && self.projection_of_tracked(e)
            && !self.copy(e.ty)
        {
            if !place || returned {
                self.escaped = true;
                return;
            }
            self.walk_projection_indices(e);
            return;
        }
        match &e.kind {
            HirExprKind::Path { segments, .. } => {
                if (!place || returned) && self.is_tracked(segments) {
                    self.escaped = true;
                }
            }
            // Reading through the parameter keeps its storage inside the call.
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                // A method answering a scalar answers a copy, so the receiver's
                // storage stays inside the call however the answer is used.
                // Any other answer may reach that storage - a cursor over it,
                // a view of it - and is read-only only where the whole call
                // already sits in a read-only place. This is the rule the
                // field projection below follows, for the same reason.
                // A method answering fresh storage copied out of a sequence of
                // scalars or strings answers nothing that reaches the receiver.
                let scalar = self.copy(e.ty) || self.answers_fresh_copy(receiver, &name.name);
                self.expr(receiver, place || scalar, returned && !scalar);
                // A method that only reads the argument it is handed leaves
                // the argument's storage inside the call, exactly as a field
                // read does, so a parameter passed to one is still only read.
                let reads_args = READ_ONLY_ARG_METHODS.contains(&name.name.as_str());
                for a in args {
                    self.expr(a, reads_args, false);
                }
            }
            HirExprKind::Index { base, index } => {
                let scalar = self.copy(e.ty);
                self.expr(base, true, returned && !scalar);
                self.expr(index, false, false);
            }
            HirExprKind::Call { callee, args } => {
                self.expr(callee, false, false);
                let target = match &callee.kind {
                    HirExprKind::Path { def: Some(d), .. } => Some(*d),
                    _ => None,
                };
                // A standard-library function that keeps no state and answers
                // a value unable to hold its argument can only read it.
                let std_reads = std_call_reads_args(self.tcx, callee, e.ty);
                for (idx, a) in args.iter().enumerate() {
                    if std_reads
                        && let HirExprKind::Path { segments, .. } = &a.kind
                        && self.is_tracked(segments)
                    {
                        continue;
                    }
                    // The argument that is exactly the parameter's name is a
                    // forward: whether it stays inside the call is the
                    // callee's own answer for that position.
                    if let Some(def) = target
                        && let HirExprKind::Path { segments, .. } = &a.kind
                        && self.is_tracked(segments)
                    {
                        self.forwards.push((def, idx));
                        continue;
                    }
                    self.expr(a, false, false);
                }
            }
            HirExprKind::Assign { place: lhs, value } => {
                self.expr(lhs, false, false);
                self.expr(value, false, false);
            }
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                // A scalar read out of the parameter is a copy, so the storage
                // stays inside the call however the value is used. Any other
                // field type yields something that still reaches the
                // parameter's storage, and is read-only only where the whole
                // projection already sits in a read-only place.
                let scalar = self.copy(e.ty);
                self.expr(receiver, place || scalar, returned && !scalar);
            }
            HirExprKind::Unary { operand, .. } => self.expr(operand, false, false),
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, false, false);
                self.expr(rhs, false, false);
            }
            HirExprKind::Cast { value, .. } => self.expr(value, false, false),
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition, false, false);
                self.expr(then_branch, place, returned);
                if let Some(e) = else_branch {
                    self.expr(e, place, returned);
                }
            }
            HirExprKind::Match { scrutinee, arms } => {
                // `for x in grid[i]` walks the parameter's own elements: the
                // loop binding names storage inside the parameter exactly as
                // `let row = grid[i]` does, so it joins the tracked names.
                let walked = self.walked_elements(scrutinee);
                if walked {
                    if let HirExprKind::MethodCall { receiver, .. } = &scrutinee.kind {
                        self.walk_projection_indices(receiver);
                    }
                } else {
                    self.expr(scrutinee, false, false);
                }
                for arm in arms {
                    if walked {
                        self.track_some_binding(&arm.pattern);
                    }
                    if let Some(g) = &arm.guard {
                        self.expr(g, false, false);
                    }
                    self.expr(&arm.body, place, returned);
                }
            }
            HirExprKind::Loop { body, .. } => self.expr(body, false, false),
            HirExprKind::While {
                condition, body, ..
            } => {
                self.expr(condition, false, false);
                self.expr(body, false, false);
            }
            HirExprKind::Block(b) => self.block(b, place, returned),
            HirExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.expr(s, false, false);
                }
                if let Some(t) = end {
                    self.expr(t, false, false);
                }
            }
            HirExprKind::Tuple(items) => {
                for i in items {
                    self.expr(i, false, false);
                }
            }
            // An early exit answers the function, and a loop's break value
            // can become the body's tail, so both carry the parameter out
            // wherever the answer's type can hold it.
            HirExprKind::Return(Some(e)) | HirExprKind::Break { value: Some(e), .. } => {
                self.expr(e, false, self.ret_carries);
            }
            // Everything not named above may carry the value somewhere this
            // walk does not model, so any mention inside it is an escape.
            _ => self.any_mention(e),
        }
    }

    /// Whether the name is mentioned anywhere in a subtree this walk does not
    /// model precisely - a closure body it was lifted into, a `select` arm.
    /// The whole subtree counts, not just its root: a capture list or an arm
    /// body reaches the name several nodes down.
    fn any_mention(&mut self, e: &HirExpr) {
        if self.escaped {
            return;
        }
        if let HirExprKind::Path { segments, .. } = &e.kind
            && self.is_tracked(segments)
        {
            self.escaped = true;
            return;
        }
        gossamer_hir::for_each_child_expr(e, &mut |child| self.any_mention(child));
    }

    /// Whether `scrutinee` is `seq.next()` over a sequence the parameter
    /// holds whose elements are not copies: the element walk a `for` loop
    /// drives, handing the body the parameter's own elements.
    fn walked_elements(&mut self, scrutinee: &HirExpr) -> bool {
        let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &scrutinee.kind
        else {
            return false;
        };
        if name.name != "next" || !args.is_empty() || !self.projection_of_tracked(receiver) {
            return false;
        }
        let elem = match self.tcx.kind_of(receiver.ty) {
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => *elem,
            _ => return false,
        };
        !self.copy(elem)
    }

    /// Tracks the name a `Some(name)` arm binds; any other shape binding a
    /// walked element is an escape, since this walk does not follow it.
    fn track_some_binding(&mut self, pattern: &gossamer_hir::HirPat) {
        use gossamer_hir::HirPatKind;
        match &pattern.kind {
            HirPatKind::Variant { fields, .. } => match fields.as_slice() {
                [] => {}
                [field] => match &field.kind {
                    HirPatKind::Binding {
                        name,
                        mutable: false,
                    } => self.names.push(name.name.as_str().to_string()),
                    HirPatKind::Wildcard => {}
                    _ => self.escaped = true,
                },
                _ => self.escaped = true,
            },
            HirPatKind::Wildcard => {}
            _ => self.escaped = true,
        }
    }

    /// Whether a one-segment path names the parameter or one of its aliases.
    fn is_tracked(&self, segments: &[gossamer_ast::Ident]) -> bool {
        segments.len() == 1
            && self
                .names
                .iter()
                .any(|tracked| tracked == segments[0].name.as_str())
    }

    /// Whether `e` is a chain of field / element reads rooted at a tracked
    /// name, so the value it yields lives inside the parameter's storage.
    fn projection_of_tracked(&self, e: &HirExpr) -> bool {
        match &e.kind {
            HirExprKind::Path { segments, .. } => self.is_tracked(segments),
            HirExprKind::Field { receiver, .. }
            | HirExprKind::TupleIndex { receiver, .. }
            | HirExprKind::Index { base: receiver, .. } => self.projection_of_tracked(receiver),
            _ => false,
        }
    }

    /// Walks the index expressions of a projection chain, which are ordinary
    /// uses of whatever they name.
    fn walk_projection_indices(&mut self, e: &HirExpr) {
        match &e.kind {
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.walk_projection_indices(receiver);
            }
            HirExprKind::Index { base, index } => {
                self.walk_projection_indices(base);
                self.expr(index, false, false);
            }
            _ => {}
        }
    }
}
