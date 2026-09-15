//! Post-lowering HIR rewrite that lifts non-capturing closures to
//! top-level functions.
//! A closure with no free variables is equivalent to a regular named
//! function. Lifting those closures gives the native backend a real
//! function pointer it can emit a direct call to, instead of
//! bailing out to the interpreter for every `map` / `filter` / etc.
//! Closures that genuinely capture variables are left alone and
//! continue to route through the tree-walker.

// HIR rewriter walks every expression kind; the per-kind match arms
// stay inline so the closure-classification logic is in one place.
#![allow(clippy::too_many_lines)]

use std::collections::HashSet;

use gossamer_ast::Ident;
use gossamer_lex::Span;

use crate::ids::HirIdGenerator;
use crate::tree::{
    FnOrigin, HirArrayExpr, HirBlock, HirBody, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirParam, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind,
};

/// Name prefix of every top-level function a closure lifts to. Later passes
/// recognise a lifted body by it.
pub const LIFTED_CLOSURE_PREFIX: &str = "__closure_";

/// Parameter name of the environment pointer a capturing closure's lifted
/// function receives.
const ENV_PARAM: &str = "__env";

/// Intrinsic that projects one captured value out of the env pointer.
const ENV_LOAD: &str = "gos_load";

/// True when `init` is the env projection that binds a captured variable in
/// a lifted closure's prologue. Such a binding names the value the enclosing
/// scope still owns, so lowering must alias it rather than take an owned
/// copy: a heap type mutated through the capture has to reach the original.
#[must_use]
pub fn is_capture_env_load(init: &HirExpr) -> bool {
    let HirExprKind::Call { callee, args } = &init.kind else {
        return false;
    };
    let HirExprKind::Path { segments, .. } = &callee.kind else {
        return false;
    };
    if segments.len() != 1 || segments[0].name != ENV_LOAD {
        return false;
    }
    matches!(
        args.first().map(|a| &a.kind),
        Some(HirExprKind::Path { segments, .. })
            if segments.len() == 1 && segments[0].name == ENV_PARAM
    )
}

/// Walks `program` and lifts every closure with no free variables
/// into a top-level [`HirItemKind::Fn`] item with a synthetic name,
/// replacing the original closure expression with a
/// [`HirExprKind::Path`] that points at it. Closures that capture
/// outer bindings are left untouched.
///
/// `tcx` is consulted to mint pointer-shaped types for the env
/// parameter of capturing closures - without an i64-shaped Ty,
/// the lifted body's first parameter would inherit the closure's
/// return type and the codegen would treat env as a sub-byte
/// register.
#[must_use]
pub fn lift_closures(mut program: HirProgram, tcx: &mut gossamer_types::TyCtxt) -> HirProgram {
    let env_ty = tcx.int_ty(gossamer_types::IntTy::I64);
    let scalar_tys = ScalarTys {
        unit: tcx.unit(),
        boolean: tcx.bool_ty(),
        i64: env_ty,
        f64: tcx.float_ty(gossamer_types::FloatTy::F64),
        character: tcx.char_ty(),
        string: tcx.string_ty(),
    };
    let mut lifter = Lifter {
        next_id: 0,
        lifted: Vec::new(),
        scopes: Vec::new(),
        ids: HirIdGenerator::new(),
        env_ty,
        scalar_tys,
        pending: Vec::new(),
    };
    for item in &mut program.items {
        match &mut item.kind {
            HirItemKind::Fn(decl) => {
                let params = decl.params.clone();
                if let Some(body) = &mut decl.body {
                    lifter.in_scope(&params, |l| l.visit_block(&mut body.block));
                }
            }
            // Impl methods and trait default methods are still nested
            // here at lift time (they are flattened into top-level `Fn`
            // items only during MIR lowering), so descend into them too
            // - otherwise a closure inside `impl Handler { fn serve }`
            // never lifts and the compiled tier lowers it to a null env.
            HirItemKind::Impl(imp) => {
                for method in &mut imp.methods {
                    let params = method.params.clone();
                    if let Some(body) = &mut method.body {
                        lifter.in_scope(&params, |l| l.visit_block(&mut body.block));
                    }
                }
            }
            HirItemKind::Trait(tr) => {
                for method in &mut tr.methods {
                    let params = method.params.clone();
                    if let Some(body) = &mut method.body {
                        lifter.in_scope(&params, |l| l.visit_block(&mut body.block));
                    }
                }
            }
            HirItemKind::Const(_) | HirItemKind::Static(_) | HirItemKind::Adt(_) => {}
        }
    }
    // Post-lift: pin every lifted closure param whose type is still
    // unresolved (`Var` / `Error`) to i64. The unified Fn trampoline calls a
    // lifted body with integer-register arguments, so a parameter the checker
    // never typed has to be declared in that register class too. Mirrors
    // MIR's `lower_iter_closure` input pinning at the trait coercion site.
    let i64_ty = lifter.env_ty;
    for item in &mut lifter.lifted {
        if let HirItemKind::Fn(decl) = &mut item.kind {
            // Collect param names whose body uses TupleIndex / Field
            // projections on them - those params are aggregates
            // (tuples / structs) passed by pointer, NOT i64 values.
            // Pinning them to i64 makes the lowered closure compute
            // field offsets off a junk integer; the param needs to
            // stay pointer-shaped so the runtime sort comparator can
            // hand us `(env, *a, *b)` and the projections resolve
            // correctly.
            let mut aggregate_params: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            if let Some(body) = &decl.body {
                collect_aggregate_param_uses(&body.block, &mut aggregate_params);
            }
            // A type parameter of the enclosing generic body is not
            // unresolved: monomorphisation gives each instantiation its own
            // copy of the lifted body with the parameter substituted, so it
            // keeps its `Param` type here for that copy to read.
            for param in &mut decl.params {
                let needs_pin = matches!(
                    tcx.kind_of(param.ty),
                    gossamer_types::TyKind::Var(_) | gossamer_types::TyKind::Error
                );
                if !needs_pin {
                    continue;
                }
                let used_as_aggregate = match &param.pattern.kind {
                    crate::HirPatKind::Binding { name, .. } => {
                        aggregate_params.contains(name.name.as_str())
                    }
                    _ => false,
                };
                if used_as_aggregate {
                    continue;
                }
                param.ty = i64_ty;
                param.pattern.ty = i64_ty;
            }
            // Same coercion for the lifted fn's return type so the
            // bool-returning `iter::filter` shape gets i64, not the
            // default pointer fallback.
            if let Some(ret) = decl.ret {
                if matches!(
                    tcx.kind_of(ret),
                    gossamer_types::TyKind::Var(_) | gossamer_types::TyKind::Error
                ) {
                    decl.ret = Some(i64_ty);
                }
            }
        }
    }
    let mut items = program.items;
    items.extend(lifter.lifted);
    HirProgram { items }
}

/// Walks `block` recursively and records every identifier that
/// appears as the receiver of a `TupleIndex`, `Field`, or `Index`
/// expression. The lift-pass's i64-default pinning skips these so
/// closure params that hold aggregates (tuples / structs / arrays)
/// stay pointer-shaped - the runtime sort / iter comparators hand
/// the body raw element pointers and the projections walk off them.
fn collect_aggregate_param_uses(block: &HirBlock, out: &mut std::collections::HashSet<String>) {
    for stmt in &block.stmts {
        match &stmt.kind {
            HirStmtKind::Let {
                init: Some(value), ..
            }
            | HirStmtKind::Expr { expr: value, .. }
            | HirStmtKind::Defer(value) => {
                collect_aggregate_in_expr(value, out);
            }
            HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &block.tail {
        collect_aggregate_in_expr(tail, out);
    }
}

fn collect_aggregate_in_expr(expr: &HirExpr, out: &mut std::collections::HashSet<String>) {
    match &expr.kind {
        HirExprKind::TupleIndex { receiver, .. } | HirExprKind::Field { receiver, .. } => {
            note_path_receiver(receiver, out);
            collect_aggregate_in_expr(receiver, out);
        }
        HirExprKind::Index { base, index } => {
            note_path_receiver(base, out);
            collect_aggregate_in_expr(base, out);
            collect_aggregate_in_expr(index, out);
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            // `param.clone()` / `param.len()` etc. - the receiver
            // is used as an aggregate handle whose methods walk
            // off a pointer, same as a field projection.
            note_path_receiver(receiver, out);
            collect_aggregate_in_expr(receiver, out);
            for a in args {
                collect_aggregate_in_expr(a, out);
            }
        }
        HirExprKind::Call { callee, args } => {
            collect_aggregate_in_expr(callee, out);
            for a in args {
                collect_aggregate_in_expr(a, out);
            }
        }
        HirExprKind::Unary { operand, .. } => {
            collect_aggregate_in_expr(operand, out);
        }
        HirExprKind::Binary { lhs, rhs, .. } => {
            collect_aggregate_in_expr(lhs, out);
            collect_aggregate_in_expr(rhs, out);
        }
        HirExprKind::Assign { place, value } => {
            collect_aggregate_in_expr(place, out);
            collect_aggregate_in_expr(value, out);
        }
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_aggregate_in_expr(condition, out);
            collect_aggregate_in_expr(then_branch, out);
            if let Some(else_b) = else_branch {
                collect_aggregate_in_expr(else_b, out);
            }
        }
        HirExprKind::Block(block) => {
            collect_aggregate_param_uses(block, out);
        }
        _ => {}
    }
}

fn note_path_receiver(expr: &HirExpr, out: &mut std::collections::HashSet<String>) {
    if let HirExprKind::Path { segments, .. } = &expr.kind {
        if segments.len() == 1 {
            out.insert(segments[0].name.clone());
        }
    }
}

/// Names the HIR lowerer emits for global helpers rather than for a binding:
/// the format-macro entry points and the lowerer's own synthetic calls. A
/// closure never captures one, because the named-callee dispatch is what
/// resolves them to runtime helpers, while an env slot holds only raw bits.
const SYNTHETIC_GLOBAL_NAMES: &[&str] = &[
    "__concat",
    "__debug",
    "__struct",
    "__fmt_prec",
    "__update",
    "format",
    "println",
    "print",
    "eprintln",
    "eprint",
    "panic",
];

/// The global-helper names `is_bound` reports as bindings, in the shape
/// [`collect_free_vars`] and its typed counterpart expect.
#[must_use]
pub fn shadowed_global_names(is_bound: impl Fn(&str) -> bool) -> HashSet<String> {
    SYNTHETIC_GLOBAL_NAMES
        .iter()
        .filter(|name| is_bound(name))
        .map(|name| (*name).to_string())
        .collect()
}

/// Whether `name` reaches a global helper rather than a binding. `shadowed`
/// carries the enclosing scope's bindings, so a parameter or local named
/// `format` is a capture like any other name.
fn is_synthetic_global<H: std::hash::BuildHasher>(
    name: &str,
    shadowed: &HashSet<String, H>,
) -> bool {
    if shadowed.contains(name) {
        return false;
    }
    SYNTHETIC_GLOBAL_NAMES.contains(&name) || name.starts_with(LIFTED_CLOSURE_PREFIX)
}

struct Lifter {
    next_id: u32,
    lifted: Vec<HirItem>,
    ids: HirIdGenerator,
    /// Names bound by the scopes enclosing the expression being visited, so a
    /// binding that shares a global helper's name still captures.
    scopes: Vec<HashSet<String>>,
    /// Ty handle for an i64 - used as the env parameter type
    /// of capturing closures so the lifted body sees env as a
    /// pointer-sized register, not a byte / sub-word.
    env_ty: gossamer_types::Ty,
    /// Scalar Ty handles for eta-expanding `[rust-bindings]`
    /// references; minted once so the visitor does not need `tcx`.
    scalar_tys: ScalarTys,
    /// `let` bindings a `spawn` hoisted out of its call, waiting for
    /// [`Lifter::visit_block`] to splice them in ahead of the statement
    /// they came from.
    pending: Vec<HirStmt>,
}

/// Pre-minted Ty handles for the binding-declared scalar types.
struct ScalarTys {
    unit: gossamer_types::Ty,
    boolean: gossamer_types::Ty,
    i64: gossamer_types::Ty,
    f64: gossamer_types::Ty,
    character: gossamer_types::Ty,
    string: gossamer_types::Ty,
}

impl Lifter {
    /// Runs `body` with a fresh scope holding `params`' bindings.
    fn in_scope(&mut self, params: &[HirParam], body: impl FnOnce(&mut Self)) {
        let mut scope = HashSet::new();
        for param in params {
            collect_pattern_names(&param.pattern, &mut scope);
        }
        self.scopes.push(scope);
        body(self);
        self.scopes.pop();
    }

    /// Binds `pattern`'s names in the innermost scope.
    fn bind(&mut self, pattern: &HirPat) {
        if let Some(scope) = self.scopes.last_mut() {
            collect_pattern_names(pattern, scope);
        }
    }

    /// The global-helper names an enclosing binding shadows, which a closure
    /// captures like any other name.
    fn shadowed_globals(&self) -> HashSet<String> {
        shadowed_global_names(|name| self.scopes.iter().any(|scope| scope.contains(name)))
    }

    fn fresh_name(&mut self) -> Ident {
        let idx = self.next_id;
        self.next_id += 1;
        Ident::new(format!("{LIFTED_CLOSURE_PREFIX}{idx}"))
    }

    fn visit_block(&mut self, block: &mut HirBlock) {
        let mut stmts = Vec::with_capacity(block.stmts.len());
        for mut stmt in block.stmts.drain(..) {
            self.visit_stmt(&mut stmt);
            // A spawn's hoisted operands are evaluated where the spawn is
            // written, so they precede the statement that produced them.
            stmts.append(&mut self.pending);
            stmts.push(stmt);
        }
        block.stmts = stmts;
        if let Some(tail) = &mut block.tail {
            self.visit_expr(tail);
            block.stmts.append(&mut self.pending);
        }
    }

    fn visit_stmt(&mut self, stmt: &mut HirStmt) {
        match &mut stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                if let Some(expr) = init {
                    self.visit_expr(expr);
                }
                let pattern = pattern.clone();
                self.bind(&pattern);
            }
            HirStmtKind::Expr { expr, .. } => self.visit_expr(expr),
            HirStmtKind::Defer(inner) => self.visit_expr(inner),
            HirStmtKind::Item(_) => {}
        }
    }

    fn visit_expr(&mut self, expr: &mut HirExpr) {
        // Recurse into children first so inner closures are lifted
        // before we process the outer one.
        match &mut expr.kind {
            HirExprKind::Call { callee, args } => {
                // A path used as the direct callee stays a direct
                // call - only value-position references eta-expand,
                // so skip the callee when it is a bare path (its
                // visit was a no-op before eta-expansion existed).
                if !matches!(callee.kind, HirExprKind::Path { .. }) {
                    self.visit_expr(callee);
                }
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            HirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            HirExprKind::Field { receiver, .. } => self.visit_expr(receiver),
            HirExprKind::TupleIndex { receiver, .. } => self.visit_expr(receiver),
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
                if let Some(else_branch) = else_branch {
                    self.visit_expr(else_branch);
                }
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    let mut scope = HashSet::new();
                    collect_pattern_names(&arm.pattern, &mut scope);
                    self.scopes.push(scope);
                    if let Some(guard) = &mut arm.guard {
                        self.visit_expr(guard);
                    }
                    self.visit_expr(&mut arm.body);
                    self.scopes.pop();
                }
            }
            HirExprKind::Loop { body, .. } | HirExprKind::While { body, .. } => {
                self.visit_expr(body);
            }
            HirExprKind::Block(block) => self.visit_block(block),
            HirExprKind::Return(Some(inner))
            | HirExprKind::Break {
                value: Some(inner), ..
            }
            | HirExprKind::Cast { value: inner, .. } => self.visit_expr(inner),
            HirExprKind::Return(None) | HirExprKind::Break { value: None, .. } => {}
            HirExprKind::Tuple(elems) => {
                for e in elems {
                    self.visit_expr(e);
                }
            }
            HirExprKind::Array(HirArrayExpr::List(elems)) => {
                for e in elems {
                    self.visit_expr(e);
                }
            }
            HirExprKind::Array(HirArrayExpr::Repeat { value, count }) => {
                self.visit_expr(value);
                self.visit_expr(count);
            }
            HirExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.visit_expr(s);
                }
                if let Some(e) = end {
                    self.visit_expr(e);
                }
            }
            HirExprKind::Closure { params, body, .. } => {
                let params = params.clone();
                self.in_scope(&params, |lifter| lifter.visit_expr(body));
            }
            HirExprKind::LiftedClosure { captures, .. } => {
                for c in captures {
                    self.visit_expr(c);
                }
            }
            HirExprKind::Select { arms } => {
                for arm in arms {
                    match &mut arm.op {
                        crate::tree::HirSelectOp::Recv { channel, .. } => {
                            self.visit_expr(channel);
                        }
                        crate::tree::HirSelectOp::Send { channel, value } => {
                            self.visit_expr(channel);
                            self.visit_expr(value);
                        }
                        crate::tree::HirSelectOp::Default => {}
                    }
                    self.visit_expr(&mut arm.body);
                }
            }
            HirExprKind::Literal(_)
            | HirExprKind::Path { .. }
            | HirExprKind::Continue { .. }
            | HirExprKind::Placeholder => {}
        }

        // A value-position reference to a `[rust-bindings]` function
        // becomes an equivalent closure (`set_text` →
        // `|a0| set_text(a0)`) so the closed-closure lift below gives
        // it a real callable body on every tier; the inner call then
        // flows through the standard binding-call lowering with its
        // argument / return conversions, which a raw symbol
        // reference would skip.
        self.eta_expand_binding_ref(expr);

        if let HirExprKind::Closure { params, ret, body } = &expr.kind {
            let mut bound: HashSet<String> = HashSet::new();
            for param in params {
                collect_pattern_names(&param.pattern, &mut bound);
            }
            let shadowed = self.shadowed_globals();
            if is_closed(body, &bound, &shadowed) {
                let lifted_name = self.lift_closed(params, *ret, body, expr.span);
                expr.kind = HirExprKind::Path {
                    segments: vec![lifted_name],
                    def: None,
                };
            } else {
                // Capturing closure: collect free vars, generate a
                // `__closure_N(env, params…)` lifted function that
                // reads captures via `gos_load`, and rewrite the
                // closure expression into a `LiftedClosure` node
                // that the MIR lowerer expands into the heap-alloc
                // sequence.
                let captures = collect_free_vars_typed(body, &bound, &shadowed);
                if !captures.is_empty() {
                    let (name, capture_exprs) =
                        self.lift_capturing(params, *ret, body, &captures, expr.span);
                    expr.kind = HirExprKind::LiftedClosure {
                        name,
                        captures: capture_exprs,
                    };
                }
            }
        }
    }

    /// Ty for a binding-declared parameter / return. Scalars map to
    /// their real types; tagged unions and handles flow as the same
    /// ptr-sized i64 the MIR binding layer uses.
    fn binding_ty(&self, t: &gossamer_resolve::BindingType) -> gossamer_types::Ty {
        use gossamer_resolve::BindingType as B;
        match t {
            B::Unit => self.scalar_tys.unit,
            B::Bool => self.scalar_tys.boolean,
            B::I64 => self.scalar_tys.i64,
            B::F64 => self.scalar_tys.f64,
            B::Char => self.scalar_tys.character,
            B::String => self.scalar_tys.string,
            _ => self.env_ty,
        }
    }

    /// Rewrites a value-position path that names a `[rust-bindings]`
    /// function into the closure `|a0, ..| f(a0, ..)`. Direct callees
    /// never reach here (the `Call` arm skips path callees), so only
    /// genuine value uses pay the wrapper.
    fn eta_expand_binding_ref(&mut self, expr: &mut HirExpr) {
        let HirExprKind::Path { segments, .. } = &expr.kind else {
            return;
        };
        // The AST→HIR lowering expands use-imported binding names to
        // their full `module::item` spelling, so a binding reference
        // always has 2+ segments here.
        if segments.len() < 2 {
            return;
        }
        let qualified = segments
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        let Some(item) = gossamer_resolve::lookup_external_item(&qualified) else {
            return;
        };
        let span = expr.span;
        let mut params = Vec::with_capacity(item.params.len());
        let mut args = Vec::with_capacity(item.params.len());
        for (index, binding_param) in item.params.iter().enumerate() {
            let ty = self.binding_ty(binding_param);
            let name = Ident::new(format!("__binding_arg{index}"));
            params.push(HirParam {
                pattern: HirPat {
                    id: self.ids.next(),
                    span,
                    ty,
                    kind: HirPatKind::Binding {
                        name: name.clone(),
                        mutable: false,
                    },
                },
                ty,
                is_comptime: false,
            });
            args.push(HirExpr {
                id: self.ids.next(),
                span,
                ty,
                kind: HirExprKind::Path {
                    segments: vec![name],
                    def: None,
                },
            });
        }
        let ret_ty = self.binding_ty(&item.ret);
        let callee = HirExpr {
            id: self.ids.next(),
            span,
            ty: expr.ty,
            kind: expr.kind.clone(),
        };
        let body = HirExpr {
            id: self.ids.next(),
            span,
            ty: ret_ty,
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        };
        expr.kind = HirExprKind::Closure {
            params,
            ret: Some(ret_ty),
            body: Box::new(body),
        };
    }

    fn lift_closed(
        &mut self,
        params: &[HirParam],
        ret: Option<gossamer_types::Ty>,
        body: &HirExpr,
        span: Span,
    ) -> Ident {
        let name = self.fresh_name();
        let hir_body = HirBody {
            block: HirBlock {
                id: self.ids.next(),
                span,
                stmts: Vec::new(),
                tail: Some(Box::new(body.clone())),
                ty: body.ty,
                is_comptime: false,
            },
        };
        // When the closure has no explicit return annotation, fall
        // back to the body's typeck-inferred type so the lifted
        // fn's MIR signature matches the actual return shape.
        // Without this, the MIR builder defaults the return type
        // to `unit` and downstream callers (Fn(...) dispatch sites)
        // mismatch on the calling convention - bool / f64 closures
        // segfault on return because the dispatcher reads the
        // wrong register width.
        let ret = ret.or(Some(body.ty));
        let decl = HirFn {
            name: name.clone(),
            params: params.to_vec(),
            ret,
            body: Some(hir_body),
            is_unsafe: false,
            is_comptime: false,
            has_self: false,
            origin: FnOrigin::LiftedClosure,
        };
        self.lifted.push(HirItem {
            id: self.ids.next(),
            span,
            def: None,
            module_path: Vec::new(),
            kind: HirItemKind::Fn(decl),
        });
        name
    }

    fn lift_capturing(
        &mut self,
        params: &[HirParam],
        ret: Option<gossamer_types::Ty>,
        body: &HirExpr,
        captures: &[(String, gossamer_types::Ty)],
        span: Span,
    ) -> (Ident, Vec<HirExpr>) {
        let name = self.fresh_name();
        // Each capture carries the type of the occurrence that made it
        // free, so the env slot is laid out and reference-counted as
        // the value it actually holds. A capture typed by anything else
        // - the closure's return type, say - hands an `i64`'s bits to a
        // `String` slot, and the compiled tiers then retain the integer
        // as a pointer.
        let capture_types: Vec<gossamer_types::Ty> = captures.iter().map(|(_, ty)| *ty).collect();
        // The lifted function's body wraps the original body in a
        // block that first pulls each capture out of the env pointer
        // via `gos_load(env, offset)`, binds it to a local of the
        // same name, then evaluates the original body.
        let mut stmts: Vec<HirStmt> = Vec::with_capacity(captures.len());
        for (i, (cap, _)) in captures.iter().enumerate() {
            let offset = (i as i64 + 1) * 8;
            let cap_ty = capture_types[i];
            let load_call = self.make_env_load(ENV_PARAM, offset, body.span, cap_ty);
            stmts.push(HirStmt {
                id: self.ids.next(),
                span: body.span,
                kind: HirStmtKind::Let {
                    pattern: HirPat {
                        id: self.ids.next(),
                        span: body.span,
                        ty: cap_ty,
                        kind: HirPatKind::Binding {
                            name: Ident::new(cap),
                            mutable: false,
                        },
                    },
                    ty: cap_ty,
                    init: Some(load_call),
                },
            });
        }
        let wrapper_block = HirBlock {
            id: self.ids.next(),
            span,
            stmts,
            tail: Some(Box::new(body.clone())),
            ty: body.ty,
            is_comptime: false,
        };
        let env_param = HirParam {
            pattern: HirPat {
                id: self.ids.next(),
                span,
                ty: self.env_ty,
                kind: HirPatKind::Binding {
                    name: Ident::new(ENV_PARAM),
                    mutable: false,
                },
            },
            ty: self.env_ty,
            is_comptime: false,
        };
        let mut new_params = vec![env_param];
        new_params.extend(params.iter().cloned());
        // Fill in the lifted fn's return type from the body's
        // typeck-inferred type when the closure had no explicit
        // annotation. See the matching comment in `lift_closed`.
        let ret = ret.or(Some(body.ty));
        let decl = HirFn {
            name: name.clone(),
            params: new_params,
            ret,
            body: Some(HirBody {
                block: wrapper_block,
            }),
            is_unsafe: false,
            is_comptime: false,
            has_self: false,
            origin: FnOrigin::LiftedClosure,
        };
        self.lifted.push(HirItem {
            id: self.ids.next(),
            span,
            def: None,
            module_path: Vec::new(),
            kind: HirItemKind::Fn(decl),
        });
        let capture_exprs: Vec<HirExpr> = captures
            .iter()
            .enumerate()
            .map(|(i, (n, _))| HirExpr {
                id: self.ids.next(),
                span,
                ty: capture_types[i],
                kind: HirExprKind::Path {
                    segments: vec![Ident::new(n)],
                    def: None,
                },
            })
            .collect();
        (name, capture_exprs)
    }

    /// Builds `gos_load(env, offset)` as a HIR call expression.
    fn make_env_load(
        &mut self,
        env_name: &str,
        offset: i64,
        span: Span,
        ty: gossamer_types::Ty,
    ) -> HirExpr {
        let env_ref = HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(env_name)],
                def: None,
            },
        };
        let offset_lit = HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Literal(crate::tree::HirLiteral::Int(offset.to_string())),
        };
        let callee = HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(ENV_LOAD)],
                def: None,
            },
        };
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args: vec![env_ref, offset_lit],
            },
        }
    }
}

/// Collects every binding name introduced by `pat` into `out`.
///
/// Used by free-variable analysis: a name is "bound" if it's
/// introduced by an enclosing pattern (parameter, `let`, match arm),
/// so collecting pattern names builds the bound-set passed into
/// [`collect_free_vars`].
pub fn collect_pattern_names<S: std::hash::BuildHasher + Clone>(
    pat: &HirPat,
    out: &mut HashSet<String, S>,
) {
    match &pat.kind {
        HirPatKind::Binding { name, .. } => {
            out.insert(name.name.clone());
        }
        HirPatKind::Tuple(subs) | HirPatKind::Variant { fields: subs, .. } => {
            for sub in subs {
                collect_pattern_names(sub, out);
            }
        }
        HirPatKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for sub in prefix {
                collect_pattern_names(sub, out);
            }
            if let Some(rest) = rest {
                collect_pattern_names(rest, out);
            }
            for sub in suffix {
                collect_pattern_names(sub, out);
            }
        }
        HirPatKind::Struct { fields, .. } => {
            for f in fields {
                if let Some(sub) = &f.pattern {
                    collect_pattern_names(sub, out);
                } else {
                    out.insert(f.name.name.clone());
                }
            }
        }
        HirPatKind::Or(alts) => {
            for alt in alts {
                collect_pattern_names(alt, out);
            }
        }
        HirPatKind::Ref { inner, .. } => collect_pattern_names(inner, out),
        HirPatKind::At { name, sub, .. } => {
            out.insert(name.name.clone());
            collect_pattern_names(sub, out);
        }
        HirPatKind::Literal(_)
        | HirPatKind::Wildcard
        | HirPatKind::Rest
        | HirPatKind::Range { .. } => {}
    }
}

fn is_closed<S: std::hash::BuildHasher + Clone, H: std::hash::BuildHasher>(
    expr: &HirExpr,
    bound: &HashSet<String, S>,
    shadowed: &HashSet<String, H>,
) -> bool {
    match &expr.kind {
        HirExprKind::Path { segments, def, .. } => {
            // Fully-qualified paths and resolved DefIds point to top-
            // level items - treat those as "closed" (not captures).
            if def.is_some() || segments.len() > 1 {
                return true;
            }
            if let Some(first) = segments.first() {
                // Synthetic builtin names (mirror of `walk_free`'s
                // exclusion set) - must never be treated as free
                // captures, so the closure stays liftable. Without
                // this match the closure body `Pair { ... }` -
                // which the HIR lowerer rewrites into
                // `__struct(...)` - appears unbound and the
                // closure neither lifts as closed nor lifts as
                // capturing, leaving the original `Closure` HIR
                // node for MIR to mishandle.
                // Lifted closure bodies (`__closure_N`) and synthetic
                // builtins are global items - never free variables.
                if is_synthetic_global(&first.name, shadowed) {
                    return true;
                }
                return bound.contains(&first.name);
            }
            true
        }
        HirExprKind::Literal(_) | HirExprKind::Continue { .. } | HirExprKind::Placeholder => true,
        HirExprKind::Return(inner) => inner.as_ref().is_none_or(|e| is_closed(e, bound, shadowed)),
        HirExprKind::Break { value, .. } => {
            value.as_ref().is_none_or(|e| is_closed(e, bound, shadowed))
        }
        HirExprKind::Call { callee, args } => {
            is_closed(callee, bound, shadowed) && args.iter().all(|a| is_closed(a, bound, shadowed))
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            is_closed(receiver, bound, shadowed)
                && args.iter().all(|a| is_closed(a, bound, shadowed))
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            is_closed(receiver, bound, shadowed)
        }
        HirExprKind::Index { base, index } => {
            is_closed(base, bound, shadowed) && is_closed(index, bound, shadowed)
        }
        HirExprKind::Unary { operand, .. } => is_closed(operand, bound, shadowed),
        HirExprKind::Binary { lhs, rhs, .. } => {
            is_closed(lhs, bound, shadowed) && is_closed(rhs, bound, shadowed)
        }
        HirExprKind::Assign { place, value } => {
            is_closed(place, bound, shadowed) && is_closed(value, bound, shadowed)
        }
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            is_closed(condition, bound, shadowed)
                && is_closed(then_branch, bound, shadowed)
                && else_branch
                    .as_ref()
                    .is_none_or(|e| is_closed(e, bound, shadowed))
        }
        HirExprKind::Match { scrutinee, arms } => {
            if !is_closed(scrutinee, bound, shadowed) {
                return false;
            }
            for arm in arms {
                let mut arm_bound = bound.clone();
                collect_pattern_names(&arm.pattern, &mut arm_bound);
                if let Some(guard) = &arm.guard {
                    if !is_closed(guard, &arm_bound, shadowed) {
                        return false;
                    }
                }
                if !is_closed(&arm.body, &arm_bound, shadowed) {
                    return false;
                }
            }
            true
        }
        HirExprKind::Loop { body, .. } => is_closed(body, bound, shadowed),
        HirExprKind::While {
            condition, body, ..
        } => is_closed(condition, bound, shadowed) && is_closed(body, bound, shadowed),
        HirExprKind::Block(block) => is_closed_block(block, bound, shadowed),
        HirExprKind::Closure { params, body, .. } => {
            let mut inner_bound = bound.clone();
            for param in params {
                collect_pattern_names(&param.pattern, &mut inner_bound);
            }
            is_closed(body, &inner_bound, shadowed)
        }
        HirExprKind::LiftedClosure { captures, .. } => {
            captures.iter().all(|c| is_closed(c, bound, shadowed))
        }
        HirExprKind::Select { arms } => arms.iter().all(|arm| {
            let ops_closed = match &arm.op {
                crate::tree::HirSelectOp::Recv { channel, .. } => {
                    is_closed(channel, bound, shadowed)
                }
                crate::tree::HirSelectOp::Send { channel, value } => {
                    is_closed(channel, bound, shadowed) && is_closed(value, bound, shadowed)
                }
                crate::tree::HirSelectOp::Default => true,
            };
            ops_closed && is_closed(&arm.body, bound, shadowed)
        }),
        HirExprKind::Tuple(elems) => elems.iter().all(|e| is_closed(e, bound, shadowed)),
        HirExprKind::Array(HirArrayExpr::List(elems)) => {
            elems.iter().all(|e| is_closed(e, bound, shadowed))
        }
        HirExprKind::Array(HirArrayExpr::Repeat { value, count }) => {
            is_closed(value, bound, shadowed) && is_closed(count, bound, shadowed)
        }
        HirExprKind::Cast { value, .. } => is_closed(value, bound, shadowed),
        HirExprKind::Range { start, end, .. } => {
            start.as_ref().is_none_or(|s| is_closed(s, bound, shadowed))
                && end.as_ref().is_none_or(|e| is_closed(e, bound, shadowed))
        }
    }
}

/// Collects the free variables referenced by `expr` that are not in
/// `bound`. Variables appear in first-use order (each distinct name
/// shows up exactly once). Used by the lifter to produce a stable
/// capture ordering, and by the tree-walking interpreter so closures
/// capture only the bindings they actually reference (instead of
/// the full enclosing scope).
#[must_use]
pub fn collect_free_vars<S: std::hash::BuildHasher + Clone, H: std::hash::BuildHasher>(
    expr: &HirExpr,
    bound: &HashSet<String, S>,
    shadowed: &HashSet<String, H>,
) -> Vec<String> {
    collect_free_vars_typed(expr, bound, shadowed)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Every free variable of `expr` paired with the type its first
/// occurrence carries.
///
/// The type comes from the same traversal that finds the name, so a
/// capture can never be found without one: a closure's environment
/// slot is laid out and reference-counted by this type, and inferring
/// it from a second, separately-written walk of the tree let the two
/// disagree - a name the type walk did not reach took the closure's
/// return type instead, and the compiled tiers then retained an `i64`
/// as if it were a `String`.
#[must_use]
pub(crate) fn collect_free_vars_typed<
    S: std::hash::BuildHasher + Clone,
    H: std::hash::BuildHasher,
>(
    expr: &HirExpr,
    bound: &HashSet<String, S>,
    shadowed: &HashSet<String, H>,
) -> Vec<(String, gossamer_types::Ty)> {
    let mut out: Vec<(String, gossamer_types::Ty)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    walk_free(expr, bound, shadowed, &mut out, &mut seen);
    out
}

fn walk_free<S: std::hash::BuildHasher + Clone, H: std::hash::BuildHasher>(
    expr: &HirExpr,
    bound: &HashSet<String, S>,
    shadowed: &HashSet<String, H>,
    out: &mut Vec<(String, gossamer_types::Ty)>,
    seen: &mut HashSet<String>,
) {
    match &expr.kind {
        HirExprKind::Path { segments, def, .. } => {
            if def.is_some() || segments.len() > 1 {
                return;
            }
            if let Some(first) = segments.first() {
                // Synthetic builtin names introduced by the HIR
                // lowerer (`__concat`, `__struct`, `__fmt_prec`,
                // and the bare format-macro helpers `format`,
                // `println`, `print`, `eprintln`, `eprint`,
                // `panic`) must never be captured as free
                // variables. Capturing them would route the call
                // through env-load, but the env stores only `i64`
                // bits and the named callee dispatch is what
                // actually resolves these to runtime helpers.
                // Lifted closure bodies (`__closure_N`) and synthetic
                // builtins are global items, never free variables.
                if is_synthetic_global(&first.name, shadowed) {
                    return;
                }
                if !bound.contains(&first.name) && seen.insert(first.name.clone()) {
                    out.push((first.name.clone(), expr.ty));
                }
            }
        }
        HirExprKind::Literal(_) | HirExprKind::Continue { .. } | HirExprKind::Placeholder => {}
        HirExprKind::Return(inner) => {
            if let Some(e) = inner {
                walk_free(e, bound, shadowed, out, seen);
            }
        }
        HirExprKind::Break { value, .. } => {
            if let Some(e) = value {
                walk_free(e, bound, shadowed, out, seen);
            }
        }
        HirExprKind::Call { callee, args } => {
            walk_free(callee, bound, shadowed, out, seen);
            for a in args {
                walk_free(a, bound, shadowed, out, seen);
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            walk_free(receiver, bound, shadowed, out, seen);
            for a in args {
                walk_free(a, bound, shadowed, out, seen);
            }
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            walk_free(receiver, bound, shadowed, out, seen);
        }
        HirExprKind::Index { base, index } => {
            walk_free(base, bound, shadowed, out, seen);
            walk_free(index, bound, shadowed, out, seen);
        }
        HirExprKind::Unary { operand, .. } => walk_free(operand, bound, shadowed, out, seen),
        HirExprKind::Binary { lhs, rhs, .. } => {
            walk_free(lhs, bound, shadowed, out, seen);
            walk_free(rhs, bound, shadowed, out, seen);
        }
        HirExprKind::Assign { place, value } => {
            walk_free(place, bound, shadowed, out, seen);
            walk_free(value, bound, shadowed, out, seen);
        }
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_free(condition, bound, shadowed, out, seen);
            walk_free(then_branch, bound, shadowed, out, seen);
            if let Some(e) = else_branch {
                walk_free(e, bound, shadowed, out, seen);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            walk_free(scrutinee, bound, shadowed, out, seen);
            for arm in arms {
                let mut arm_bound = bound.clone();
                collect_pattern_names(&arm.pattern, &mut arm_bound);
                if let Some(g) = &arm.guard {
                    walk_free_with(g, &arm_bound, shadowed, out, seen);
                }
                walk_free_with(&arm.body, &arm_bound, shadowed, out, seen);
            }
        }
        HirExprKind::Loop { body, .. } => {
            walk_free(body, bound, shadowed, out, seen);
        }
        HirExprKind::While {
            condition, body, ..
        } => {
            walk_free(condition, bound, shadowed, out, seen);
            walk_free(body, bound, shadowed, out, seen);
        }
        HirExprKind::Block(block) => walk_free_block(block, bound, shadowed, out, seen),
        HirExprKind::Closure { params, body, .. } => {
            let mut inner_bound = bound.clone();
            for p in params {
                collect_pattern_names(&p.pattern, &mut inner_bound);
            }
            walk_free_with(body, &inner_bound, shadowed, out, seen);
        }
        HirExprKind::LiftedClosure { captures, .. } => {
            for c in captures {
                walk_free(c, bound, shadowed, out, seen);
            }
        }
        HirExprKind::Select { arms } => {
            for arm in arms {
                match &arm.op {
                    crate::tree::HirSelectOp::Recv { channel, .. } => {
                        walk_free(channel, bound, shadowed, out, seen);
                    }
                    crate::tree::HirSelectOp::Send { channel, value } => {
                        walk_free(channel, bound, shadowed, out, seen);
                        walk_free(value, bound, shadowed, out, seen);
                    }
                    crate::tree::HirSelectOp::Default => {}
                }
                walk_free(&arm.body, bound, shadowed, out, seen);
            }
        }
        HirExprKind::Tuple(elems) | HirExprKind::Array(HirArrayExpr::List(elems)) => {
            for e in elems {
                walk_free(e, bound, shadowed, out, seen);
            }
        }
        HirExprKind::Array(HirArrayExpr::Repeat { value, count }) => {
            walk_free(value, bound, shadowed, out, seen);
            walk_free(count, bound, shadowed, out, seen);
        }
        HirExprKind::Cast { value, .. } => walk_free(value, bound, shadowed, out, seen),
        HirExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                walk_free(s, bound, shadowed, out, seen);
            }
            if let Some(e) = end {
                walk_free(e, bound, shadowed, out, seen);
            }
        }
    }
}

fn walk_free_with<S: std::hash::BuildHasher + Clone, H: std::hash::BuildHasher>(
    expr: &HirExpr,
    bound: &HashSet<String, S>,
    shadowed: &HashSet<String, H>,
    out: &mut Vec<(String, gossamer_types::Ty)>,
    seen: &mut HashSet<String>,
) {
    walk_free(expr, bound, shadowed, out, seen);
}

fn walk_free_block<S: std::hash::BuildHasher + Clone, H: std::hash::BuildHasher>(
    block: &HirBlock,
    bound: &HashSet<String, S>,
    shadowed: &HashSet<String, H>,
    out: &mut Vec<(String, gossamer_types::Ty)>,
    seen: &mut HashSet<String>,
) {
    let mut local = bound.clone();
    for stmt in &block.stmts {
        match &stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                if let Some(e) = init {
                    walk_free(e, &local, shadowed, out, seen);
                }
                collect_pattern_names(pattern, &mut local);
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                walk_free(expr, &local, shadowed, out, seen);
            }
            HirStmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &block.tail {
        walk_free(tail, &local, shadowed, out, seen);
    }
}

fn is_closed_block<S: std::hash::BuildHasher + Clone, H: std::hash::BuildHasher>(
    block: &HirBlock,
    bound: &HashSet<String, S>,
    shadowed: &HashSet<String, H>,
) -> bool {
    let mut local = bound.clone();
    for stmt in &block.stmts {
        match &stmt.kind {
            HirStmtKind::Let { pattern, init, .. } => {
                if let Some(init) = init {
                    if !is_closed(init, &local, shadowed) {
                        return false;
                    }
                }
                collect_pattern_names(pattern, &mut local);
            }
            HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                if !is_closed(expr, &local, shadowed) {
                    return false;
                }
            }
            HirStmtKind::Item(_) => {}
        }
    }
    block
        .tail
        .as_ref()
        .is_none_or(|tail| is_closed(tail, &local, shadowed))
}
