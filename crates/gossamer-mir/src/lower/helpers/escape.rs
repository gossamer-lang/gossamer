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
use gossamer_types::{Ty, TyCtxt, TyKind};

/// Method names that mutate their receiver in place (could stash an
/// argument into a caller-owned container).
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
    "clear",
    "truncate",
    "set",
    "inc",
    "or_insert",
    "reverse",
    "push_str",
    "drain",
];

/// True for types whose values carry no heap ownership - copying or
/// dropping them frees nothing, so they can flow out of a region freely.
pub(crate) fn is_copy_ty(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind_of(ty),
        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Unit
    )
}

/// Free functions that may let a value escape beyond their own return:
/// they spawn goroutines, touch channels, write a static, or stash a
/// value through a parameter. Calling one inside an auto-region is unsound.
/// Computed as a transitive closure over the static call graph.
pub fn collect_region_unsafe_fns(program: &HirProgram, tcx: &TyCtxt) -> HashSet<DefId> {
    let mut static_tys: HashMap<DefId, Ty> = HashMap::new();
    for item in &program.items {
        if let HirItemKind::Static(s) = &item.kind {
            if let Some(d) = item.def {
                static_tys.insert(d, s.ty);
            }
        }
    }

    let mut direct_unsafe: HashSet<DefId> = HashSet::new();
    let mut callees: HashMap<DefId, HashSet<DefId>> = HashMap::new();

    for item in &program.items {
        let HirItemKind::Fn(f) = &item.kind else {
            continue;
        };
        let (Some(def), Some(body)) = (item.def, &f.body) else {
            continue;
        };
        let params: HashSet<String> = f
            .params
            .iter()
            .flat_map(|p| {
                let mut names = Vec::new();
                pat_binding_names(&p.pattern, &mut names);
                names
            })
            .collect();
        let mut scan = Scan {
            tcx,
            params: &params,
            statics: &static_tys,
            unsafe_now: false,
            callees: HashSet::new(),
        };
        scan.block(&body.block);
        if scan.unsafe_now {
            direct_unsafe.insert(def);
        }
        callees.insert(def, scan.callees);
    }

    // Fixpoint: a function is unsafe if it is directly unsafe or calls an
    // unsafe function.
    let mut unsafe_set = direct_unsafe;
    loop {
        let mut changed = false;
        for (def, cs) in &callees {
            if !unsafe_set.contains(def) && cs.iter().any(|c| unsafe_set.contains(c)) {
                unsafe_set.insert(*def);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    unsafe_set
}

/// Every identifier a pattern binds, walking tuple / variant / struct / ref /
/// `@` sub-patterns so a destructured `let a, b = …` registers both names.
fn pat_binding_names(pat: &gossamer_hir::HirPat, out: &mut Vec<String>) {
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

/// Single-segment name of a path expression, if it is one.
fn path_root_name(expr: &HirExpr) -> Option<&str> {
    match &expr.kind {
        HirExprKind::Path { segments, .. } => segments.first().map(|s| s.name.as_str()),
        _ => None,
    }
}

/// Peels `&`/`&mut`/deref wrappers to the inner place expression.
fn peel_refs(expr: &HirExpr) -> &HirExpr {
    let mut cur = expr;
    loop {
        match &cur.kind {
            HirExprKind::Unary { operand, .. } => cur = operand,
            _ => return cur,
        }
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

struct Scan<'a> {
    tcx: &'a TyCtxt,
    params: &'a HashSet<String>,
    statics: &'a HashMap<DefId, Ty>,
    unsafe_now: bool,
    callees: HashSet<DefId>,
}

impl Scan<'_> {
    fn block(&mut self, b: &HirBlock) {
        for s in &b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &b.tail {
            self.expr(t);
        }
    }

    fn stmt(&mut self, s: &HirStmt) {
        match &s.kind {
            HirStmtKind::Let { init, .. } => {
                if let Some(e) = init {
                    self.expr(e);
                }
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => self.expr(expr),
            HirStmtKind::Item(_) => {}
        }
    }

    fn expr(&mut self, e: &HirExpr) {
        match &e.kind {
            HirExprKind::Select { .. } => {
                self.unsafe_now = true;
            }
            HirExprKind::Call { callee, args } => {
                if let HirExprKind::Path { def: Some(d), .. } = &callee.kind {
                    self.callees.insert(*d);
                }
                self.expr(callee);
                for a in args {
                    self.expr(a);
                }
            }
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                // A mutator on a parameter-rooted receiver may stash an
                // argument into a caller-owned structure → escape.
                if MUTATOR_METHODS.contains(&name.name.as_str())
                    && place_root_name(receiver).is_some_and(|r| self.params.contains(r))
                {
                    self.unsafe_now = true;
                }
                self.expr(receiver);
                for a in args {
                    self.expr(a);
                }
            }
            HirExprKind::Assign { place, value } => {
                // Writing heap ownership into a static, or storing a
                // non-Copy value through a parameter, escapes. Scalar
                // static cells (e.g. deterministic PRNG seeds) cannot retain
                // an arena-owned pointer, so they do not make callers
                // region-unsafe.
                if let HirExprKind::Path { def: Some(d), .. } = &place.kind {
                    if self
                        .statics
                        .get(d)
                        .is_some_and(|ty| !is_copy_ty(self.tcx, *ty))
                        || (self.statics.contains_key(d) && !is_copy_ty(self.tcx, value.ty))
                    {
                        self.unsafe_now = true;
                    }
                }
                if !is_copy_ty(self.tcx, value.ty)
                    && place_root_name(place).is_some_and(|r| self.params.contains(r))
                {
                    self.unsafe_now = true;
                }
                self.expr(place);
                self.expr(value);
            }
            // Structural recursion over everything else.
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.expr(receiver);
            }
            HirExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            HirExprKind::Unary { operand, .. } => self.expr(operand),
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            HirExprKind::If {
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
            HirExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                }
            }
            HirExprKind::Loop { body, .. } => self.expr(body),
            HirExprKind::While {
                condition, body, ..
            } => {
                self.expr(condition);
                self.expr(body);
            }
            HirExprKind::Block(b) => self.block(b),
            HirExprKind::Return(Some(e)) | HirExprKind::Break { value: Some(e), .. } => {
                self.expr(e)
            }
            HirExprKind::Tuple(items) => {
                for i in items {
                    self.expr(i);
                }
            }
            HirExprKind::Array(arr) => match arr {
                gossamer_hir::HirArrayExpr::List(items) => {
                    for i in items {
                        self.expr(i);
                    }
                }
                gossamer_hir::HirArrayExpr::Repeat { value, count } => {
                    self.expr(value);
                    self.expr(count);
                }
            },
            HirExprKind::Cast { value, .. } => self.expr(value),
            HirExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.expr(s);
                }
                if let Some(en) = end {
                    self.expr(en);
                }
            }
            // Closures capture by reference into a GC env; treat any closure
            // as opaque (its body may escape). Conservative: mark unsafe.
            HirExprKind::Closure { .. } | HirExprKind::LiftedClosure { .. } => {
                self.unsafe_now = true;
            }
            HirExprKind::Literal(_)
            | HirExprKind::Path { .. }
            | HirExprKind::Continue { .. }
            | HirExprKind::Return(None)
            | HirExprKind::Break { value: None, .. }
            | HirExprKind::Placeholder => {}
        }
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
    HeapAssign,
    UnsafeCallee,
    UnresolvedCallee,
    DeferGoOrItem,
}

impl RegionReject {
    /// One-line, user-facing explanation of the rejection.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::EarlyExitOrCapture => {
                "body has break/continue/return/go/select/closure (would bypass the region pop or capture a value)"
            }
            Self::NestedLoop => "body contains a nested loop (each loop regions itself)",
            Self::MethodCall => "body calls a method (could stash a value into an outer container)",
            Self::EscapingArg => {
                "body passes a non-Copy value created outside the loop into a call"
            }
            Self::HeapAssign => {
                "body assigns a heap value into a binding that outlives the iteration"
            }
            Self::UnsafeCallee => {
                "body calls a region-unsafe fn (spawns a goroutine, writes heap data to a static, or mutates a parameter)"
            }
            Self::UnresolvedCallee => "body calls through an unresolved/indirect callee",
            Self::DeferGoOrItem => "body contains a defer, go, or nested item",
        }
    }
}

/// The auto-region decision for a loop body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionDecision {
    /// Eligible: the body allocates and provably nothing escapes.
    Region,
    /// The body allocates a heap value each iteration but was rejected - the
    /// per-iteration heap is torn down node-by-node instead of bulk-freed.
    Reject(RegionReject),
    /// The body allocates nothing, so a region would be pure overhead.
    NoAlloc,
}

/// Walks a loop body and decides whether it is safe to wrap in an arena
/// region: no control flow escapes the region without a pop, no value
/// created in the body outlives the iteration, and every callee is
/// region-safe. `outer_local_ty` resolves a name visible before the loop
/// to its type (so an outer non-Copy value passed into a call is rejected).
pub(crate) struct LoopEligibility<'a> {
    pub tcx: &'a TyCtxt,
    pub unsafe_fns: &'a HashSet<DefId>,
    /// Names declared inside the loop body so far (let-bindings); these die
    /// at the iteration boundary and are safe to pass around.
    in_body: HashSet<String>,
    ok: bool,
    /// True once the body is seen to allocate a heap value (a call returning a
    /// heap type, etc.). A region only pays off if there is something to arena;
    /// a purely-scalar body (a counter scan, byte stores) must NOT be wrapped,
    /// or every iteration pays two `arena_push`/`arena_pop` calls for nothing.
    allocates: bool,
    /// First rejection reason, for the `GOS_ARENA_TRACE` diagnostic. Only
    /// meaningful once `ok` is false.
    reject: Option<RegionReject>,
}

impl<'a> LoopEligibility<'a> {
    pub fn new(tcx: &'a TyCtxt, unsafe_fns: &'a HashSet<DefId>) -> Self {
        Self {
            tcx,
            unsafe_fns,
            in_body: HashSet::new(),
            ok: true,
            allocates: false,
            reject: None,
        }
    }

    /// Marks the body ineligible and records the first rejection reason.
    fn reject(&mut self, r: RegionReject) {
        self.ok = false;
        if self.reject.is_none() {
            self.reject = Some(r);
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
        use gossamer_types::TyKind;
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
        self.expr(body, true);
        match (self.ok, self.allocates) {
            (true, true) => RegionDecision::Region,
            (_, false) => RegionDecision::NoAlloc,
            (false, true) => {
                RegionDecision::Reject(self.reject.unwrap_or(RegionReject::EarlyExitOrCapture))
            }
        }
    }

    /// Decides whether a nested lexical block can own one automatic region.
    /// Unlike a loop body, its tail is observed by the enclosing expression,
    /// so a non-Copy tail would let a region pointer escape through the block
    /// result. Function bodies deliberately do not use this entry point: a
    /// returned value has the same escape hazard.
    pub fn decide_lexical_block(mut self, block: &HirBlock) -> RegionDecision {
        self.block(block, true);
        if block
            .tail
            .as_ref()
            .is_some_and(|tail| !is_copy_ty(self.tcx, tail.ty))
        {
            self.reject(RegionReject::EarlyExitOrCapture);
        }
        match (self.ok, self.allocates) {
            (true, true) => RegionDecision::Region,
            (_, false) => RegionDecision::NoAlloc,
            (false, true) => {
                RegionDecision::Reject(self.reject.unwrap_or(RegionReject::EarlyExitOrCapture))
            }
        }
    }

    fn block(&mut self, b: &HirBlock, top: bool) {
        for s in &b.stmts {
            match &s.kind {
                HirStmtKind::Let { pattern, init, .. } => {
                    if let Some(e) = init {
                        self.expr(e, false);
                    }
                    // A lifted closure's prologue binds each captured value
                    // through an env load. The binding names a value the
                    // enclosing scope owns, so it stays out of `in_body` and
                    // is judged by the same rule as any other outer name.
                    if init.as_ref().is_some_and(gossamer_hir::is_capture_env_load) {
                        continue;
                    }
                    let mut names = Vec::new();
                    pat_binding_names(pattern, &mut names);
                    self.in_body.extend(names);
                }
                HirStmtKind::Expr { expr, .. } => self.expr(expr, false),
                HirStmtKind::Defer(_) | HirStmtKind::Item(_) => {
                    self.reject(RegionReject::DeferGoOrItem);
                }
            }
        }
        if let Some(t) = &b.tail {
            self.expr(t, top);
        }
    }

    /// Checks a call argument: it must be Copy, or created inside the body
    /// (an in-body let or a fresh call result), never an outer non-Copy.
    /// References are peeled first so `&mut seed` is judged by the referent
    /// (`seed: i64`, Copy - safe), not the reference type.
    fn check_arg(&mut self, arg: &HirExpr) {
        let inner = peel_refs(arg);
        if is_copy_ty(self.tcx, inner.ty) {
            return;
        }
        match &inner.kind {
            // Fresh value produced in the body - dies with the region.
            HirExprKind::Call { .. }
            | HirExprKind::Literal(_)
            | HirExprKind::Tuple(_)
            | HirExprKind::Array(_) => {}
            HirExprKind::Path { .. } => {
                // A non-Copy value passed into a call is safe only if it was
                // created inside the body (dies with the region). An outer
                // local, parameter, or global is rejected conservatively.
                match path_root_name(inner) {
                    Some(root) if self.in_body.contains(root) => {}
                    _ => self.reject(RegionReject::EscapingArg),
                }
            }
            _ => {
                // Field/index/etc of something - conservatively reject if
                // it is non-Copy and not obviously in-body.
                if !place_root_name(inner).is_some_and(|r| self.in_body.contains(r)) {
                    self.reject(RegionReject::EscapingArg);
                }
            }
        }
    }

    fn expr(&mut self, e: &HirExpr, _top: bool) {
        match &e.kind {
            // Control flow that would skip the region pop, or escape values.
            // Any break/continue/return can bypass the pop emitted at the
            // body's fall-through exit, leaving the region open.
            HirExprKind::Return(_)
            | HirExprKind::Break { .. }
            | HirExprKind::Continue { .. }
            | HirExprKind::Select { .. }
            | HirExprKind::Closure { .. }
            | HirExprKind::LiftedClosure { .. } => {
                self.reject(RegionReject::EarlyExitOrCapture);
            }
            // Nested loops are analyzed (and regioned) on their own.
            HirExprKind::Loop { .. } | HirExprKind::While { .. } => {
                self.reject(RegionReject::NestedLoop);
            }
            // Projecting a captured value out of the closure environment is a
            // load: it allocates nothing and cannot retain a region pointer.
            // The value it yields is an outer one, which `check_arg` judges
            // wherever the body goes on to use it.
            HirExprKind::Call { .. } if gossamer_hir::is_capture_env_load(e) => {}
            HirExprKind::Call { callee, args } => {
                if self.is_alloc_ty(e.ty) {
                    self.allocates = true;
                }
                match &callee.kind {
                    HirExprKind::Path { def: Some(d), .. } => {
                        if self.unsafe_fns.contains(d) {
                            self.reject(RegionReject::UnsafeCallee);
                        }
                    }
                    // Standard-library paths do not have a user-function
                    // DefId. Whitelist only the no-argument collector: it
                    // cannot retain a region pointer, and lowering moves it
                    // to immediately after the matching `arena_pop`.
                    HirExprKind::Path {
                        def: None,
                        segments,
                    } => {
                        let names: Vec<&str> = segments
                            .iter()
                            .map(|segment| segment.name.as_str())
                            .collect();
                        let names = names.strip_prefix(&["std"]).unwrap_or(&names);
                        if !(args.is_empty() && names == ["runtime", "collect_cycles"]) {
                            self.reject(RegionReject::UnresolvedCallee);
                        }
                    }
                    // Unresolved / non-path callee - cannot vet it.
                    _ => self.reject(RegionReject::UnresolvedCallee),
                }
                for a in args {
                    self.check_arg(a);
                    self.expr(a, false);
                }
            }
            // A method call could mutate an outer container. The audited
            // exceptions below read only Copy-typed data and cannot retain a
            // region pointer.
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                owner: None,
            } if name.name == "len" && args.is_empty() => self.expr(receiver, false),
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                owner: None,
            } if matches!(name.name.as_str(), "wrapping_add" | "wrapping_mul")
                && is_copy_ty(self.tcx, receiver.ty)
                && args.iter().all(|arg| is_copy_ty(self.tcx, arg.ty)) =>
            {
                self.expr(receiver, false);
                for arg in args {
                    self.expr(arg, false);
                }
            }
            HirExprKind::MethodCall { .. } => self.reject(RegionReject::MethodCall),
            HirExprKind::Assign { place, value } => {
                // Only Copy-typed places (i64 accumulators, loop counters)
                // may be assigned; storing a heap value into any binding
                // could let it outlive the iteration.
                if !is_copy_ty(self.tcx, place.ty) {
                    self.reject(RegionReject::HeapAssign);
                }
                self.expr(value, false);
            }
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                self.expr(receiver, false);
            }
            HirExprKind::Index { base, index } => {
                self.expr(base, false);
                self.expr(index, false);
            }
            HirExprKind::Unary { operand, .. } => self.expr(operand, false),
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, false);
                self.expr(rhs, false);
            }
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition, false);
                self.expr(then_branch, false);
                if let Some(el) = else_branch {
                    self.expr(el, false);
                }
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, false);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.expr(g, false);
                    }
                    self.expr(&arm.body, false);
                }
            }
            HirExprKind::Block(b) => self.block(b, false),
            HirExprKind::Tuple(items) => {
                for i in items {
                    self.expr(i, false);
                }
            }
            HirExprKind::Array(arr) => match arr {
                gossamer_hir::HirArrayExpr::List(items) => {
                    for i in items {
                        self.expr(i, false);
                    }
                }
                gossamer_hir::HirArrayExpr::Repeat { value, count } => {
                    self.expr(value, false);
                    self.expr(count, false);
                }
            },
            HirExprKind::Cast { value, .. } => self.expr(value, false),
            HirExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.expr(s, false);
                }
                if let Some(en) = end {
                    self.expr(en, false);
                }
            }
            HirExprKind::Literal(_) | HirExprKind::Path { .. } | HirExprKind::Placeholder => {}
        }
    }
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
pub fn collect_shareable_params(program: &HirProgram, tcx: &TyCtxt) -> HashMap<DefId, Vec<bool>> {
    let mut out = HashMap::new();
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
        let mut flags: Vec<bool> = Vec::with_capacity(f.params.len());
        let mut param_forwards: Vec<Vec<(DefId, usize)>> = Vec::with_capacity(f.params.len());
        for p in &f.params {
            let mut forwards = Vec::new();
            let shareable = 'param: {
                let gossamer_hir::HirPatKind::Binding {
                    name,
                    mutable: false,
                } = &p.pattern.kind
                else {
                    break 'param false;
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
                    break 'param false;
                }
                let mut scan = ShareScan {
                    tcx,
                    names: vec![name.name.as_str().to_string()],
                    escaped: false,
                    ret_carries,
                    forwards: Vec::new(),
                };
                scan.block(&body.block, false, ret_carries);
                forwards = scan.forwards;
                !scan.escaped
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
                if !out[def][idx] {
                    continue;
                }
                let reaches_unshareable = targets.iter().any(|(callee, pos)| {
                    out.get(callee)
                        .is_none_or(|flags| !flags.get(*pos).copied().unwrap_or(false))
                });
                if reaches_unshareable {
                    out.get_mut(def).expect("summary present")[idx] = false;
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
    "push_str",
    "push_utf8",
    "starts_with",
];

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
}

impl ShareScan<'_> {
    fn block(&mut self, b: &HirBlock, place: bool, returned: bool) {
        for s in &b.stmts {
            match &s.kind {
                HirStmtKind::Let { pattern, init, .. } => {
                    if let Some(e) = init {
                        // A binding taken straight out of the parameter names
                        // the same storage, so it joins the tracked set rather
                        // than counting as a use: the walk then judges what the
                        // body does with it.
                        if let gossamer_hir::HirPatKind::Binding {
                            name,
                            mutable: false,
                        } = &pattern.kind
                            && self.projection_of_tracked(e)
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
            && !is_copy_ty(self.tcx, e.ty)
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
                let scalar = is_copy_ty(self.tcx, e.ty);
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
                let scalar = is_copy_ty(self.tcx, e.ty);
                self.expr(base, true, returned && !scalar);
                self.expr(index, false, false);
            }
            HirExprKind::Call { callee, args } => {
                self.expr(callee, false, false);
                let target = match &callee.kind {
                    HirExprKind::Path { def: Some(d), .. } => Some(*d),
                    _ => None,
                };
                for (idx, a) in args.iter().enumerate() {
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
                let scalar = matches!(
                    self.tcx.kind_of(e.ty),
                    TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Unit
                );
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
                self.expr(scrutinee, false, false);
                for arm in arms {
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
