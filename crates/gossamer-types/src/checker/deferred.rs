//! Checks that wait for the whole program: spawn scopes, deferred reference storage, and expectations.

use super::{
    CATALOG_TYPE_PARAM_SLOTS, DeferredStructural, DeferredStructuralKind, Expectation, Expr,
    ExprKind, FnDecl, FnParam, FnSig, HashMap, HashSet, INT_SUFFIXES, IntTy, Literal,
    LiteralConsts, MethodCallSite, Mutbl, Resolution, Span, SpawnScopeScan, Ty, TyKind,
    TypeChecker, TypeError, UnaryOp, WriteArgPathCollector, body_value_span, builtin_trait_methods,
    combinator_module_name, field_name_span, int_literal_fits, is_compiler_generated,
    is_soft_for_structural_use, op_trait_name, struct_literal_positional_index, with_field_span,
};

impl TypeChecker<'_> {
    /// Reports a `spawn` that no `cohort { }` in the same body encloses.
    ///
    /// `spawn` attaches its child to the cohort the goroutine is inside at
    /// that moment, and a function is not a cohort. A function that spawns
    /// without opening one hands its children to whatever cohort its caller
    /// happens to be in - and, when the program has none, to the root cohort,
    /// whose extent is the process. Neither side of the call says so: the
    /// callee's signature does not mention spawning and the caller cannot see
    /// what it took on. Requiring the block lexically is what makes the block
    /// that joins a child the block a reader can point at.
    ///
    /// `main` is the exemption, and the only one. The root cohort's extent IS
    /// main's extent, so a child started there does not outlive the scope that
    /// owns it - the one place ambient attachment tells the truth. An entry
    /// file's top-level statements are a synthesized `fn main` and ride the
    /// same exemption; a method named `main` in an `impl` block does not.
    ///
    /// Containment is the whole test, so a closure does not break it: a
    /// closure written inside a cohort block runs where it is written.
    pub(super) fn reject_unscoped_spawns(&mut self, decl: &FnDecl, body: &Expr) {
        let name = decl.name.name.as_str();
        if (name == "main" && self.current_self_ty_name.is_none()) || is_compiler_generated(name) {
            return;
        }
        let mut scan = SpawnScopeScan::default();
        gossamer_ast::visitor::Visitor::visit_expr(&mut scan, body);
        if scan.spawns.is_empty() {
            return;
        }
        for spawn in scan.spawns {
            let scoped = scan.cohorts.iter().any(|cohort| {
                cohort.file == spawn.file && cohort.start <= spawn.start && spawn.end <= cohort.end
            });
            if !scoped {
                self.emit(
                    TypeError::SpawnOutsideCohort {
                        function: name.to_string(),
                    },
                    spawn,
                );
            }
        }
    }

    /// Reports a body whose tail answers a value through a signature that
    /// declares no return type. A missing return type is a unit, so the
    /// value the tail computed is discarded; the report says how to return
    /// it and how to mark the discard deliberate.
    pub(super) fn check_undeclared_return(&mut self, decl: &FnDecl, body: &Expr, body_ty: Ty) {
        // A wrapper the front end synthesized around an expression - the
        // REPL's per-input entry point, the binding-type probe - answers that
        // expression by construction, and its caller reads the value back
        // rather than the signature.
        if is_compiler_generated(&decl.name.name) {
            return;
        }
        // A body with no tail expression answers a unit whatever its
        // statements compute, so only a tail can hand a value back.
        if !matches!(&body.kind, ExprKind::Block(block) if block.tail.is_some()) {
            return;
        }
        let resolved = self.infer.resolve(self.tcx, body_ty);
        if !self.ty_is_returnable_value(resolved) {
            return;
        }
        let found = self.render_public_ty(resolved);
        self.emit(
            TypeError::UndeclaredReturnValue {
                name: decl.name.name.clone(),
                found,
            },
            body_value_span(body),
        );
    }

    /// Whether a body's answer is a value a caller could read back, as
    /// opposed to a unit, a diverging path, or a type inference never
    /// settled (which already has its own diagnostic).
    pub(super) fn ty_is_returnable_value(&self, ty: Ty) -> bool {
        !matches!(
            self.tcx.kind(ty),
            None | Some(TyKind::Unit | TyKind::Never | TyKind::Error | TyKind::Var(_))
        )
    }

    pub(super) fn ty_contains_reference(&self, ty: Ty) -> bool {
        match self.tcx.kind_of(ty) {
            TyKind::Ref { .. } => true,
            TyKind::Array { elem, .. }
            | TyKind::Slice(elem)
            | TyKind::Vec(elem)
            | TyKind::Sender(elem)
            | TyKind::Receiver(elem)
            | TyKind::JoinHandle(elem) => self.ty_contains_reference(*elem),
            TyKind::Tuple(items) => items.iter().any(|item| self.ty_contains_reference(*item)),
            TyKind::HashMap { key, value, .. } => {
                self.ty_contains_reference(*key) || self.ty_contains_reference(*value)
            }
            TyKind::Adt { substs, .. } | TyKind::FnDef { substs, .. } => substs
                .types()
                .iter()
                .any(|item| self.ty_contains_reference(*item)),
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                self.ty_contains_reference(sig.output)
                    || sig
                        .inputs
                        .iter()
                        .any(|item| self.ty_contains_reference(*item))
            }
            _ => false,
        }
    }

    pub(super) fn ty_contains_nested_vec(&self, ty: Ty) -> bool {
        fn walk(checker: &TypeChecker<'_>, ty: Ty, seen: &mut HashSet<Ty>) -> bool {
            let ty = checker.infer.resolve(checker.tcx, ty);
            if !seen.insert(ty) {
                return false;
            }
            match checker.tcx.kind_of(ty) {
                TyKind::Vec(_) => true,
                TyKind::Array { elem, .. } | TyKind::Slice(elem) => walk(checker, *elem, seen),
                TyKind::Tuple(items) => items.iter().any(|item| walk(checker, *item, seen)),
                TyKind::Adt { def, substs } => checker
                    .tcx
                    .adt_field_tys(*def, substs)
                    .is_some_and(|fields| fields.iter().any(|field| walk(checker, *field, seen))),
                _ => false,
            }
        }

        walk(self, ty, &mut HashSet::new())
    }

    pub(super) fn reject_stored_reference_type(&mut self, ty: Ty, span: Span, context: &str) {
        if self.ty_contains_reference(ty) {
            self.emit(
                TypeError::ReferenceEscapeUnsupported {
                    context: context.to_string(),
                },
                span,
            );
        }
    }

    pub(super) fn check_deferred_reference_storage(&mut self) {
        let pending = std::mem::take(&mut self.deferred_reference_storage);
        for (ty, span, context) in pending {
            let ty = self.infer.resolve(self.tcx, ty);
            if !matches!(self.tcx.kind_of(ty), TyKind::Ref { .. }) && self.ty_contains_reference(ty)
            {
                self.emit(
                    TypeError::ReferenceEscapeUnsupported {
                        context: context.to_string(),
                    },
                    span,
                );
            }
        }
    }

    /// Return type of a method called on a bound type-parameter receiver
    /// (`s.method()` where `s: &T` and `T: Trait`): look the method up in
    /// each of the parameter's bound traits. `None` when the receiver is
    /// not a type parameter or no bound declares the method.
    /// Resolves `ty` and strips any chain of `&` / `&mut` wrappers,
    /// returning the underlying type. References are layout-transparent
    /// in Gossamer (the runtime owns memory), so this is used wherever a
    /// value-vs-reference distinction must not produce a diagnostic.
    pub(super) fn peel_refs(&mut self, ty: Ty) -> Ty {
        let mut cur = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(cur) {
            cur = self.infer.resolve(self.tcx, *inner);
        }
        cur
    }

    pub(super) fn param_method_sig(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        span: Span,
    ) -> Option<(Ty, Vec<Ty>)> {
        let mut t = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(t) {
            t = self.infer.resolve(self.tcx, *inner);
        }
        let TyKind::Param { idx, name } = self.tcx.kind(t)? else {
            return None;
        };
        let param_name = name.to_string();
        let bounds = self.current_param_bounds.get(idx.0 as usize)?.clone();
        for bound in bounds {
            let key = (bound, method.to_string());
            if let Some(ret) = self.trait_method_ret.get(&key).copied() {
                let params = self
                    .trait_method_params
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                // A `-> Self::Item` return is concrete only once the
                // receiver is known, so resolve it against this
                // parameter's own bound rather than the trait's
                // declaration-time placeholder.
                let ret = match self.trait_method_ret_assoc.get(&key).cloned() {
                    Some(assoc) => self.resolve_assoc_type_projection_inner(
                        &param_name,
                        &assoc,
                        true,
                        false,
                        span,
                        false,
                    ),
                    None => ret,
                };
                return Some((ret, params));
            }
        }
        None
    }

    /// Rejects a binary operator whose left or right operand is a generic
    /// parameter with no bound licensing it. Returns `true` when a
    /// diagnostic was emitted.
    pub(super) fn reject_operands_off_bound(
        &mut self,
        lhs_ty: Ty,
        rhs_ty: Ty,
        op: &str,
        method: &str,
        span: Span,
    ) -> bool {
        self.reject_operator_off_bound(lhs_ty, op, method, span)
            || self.reject_operator_off_bound(rhs_ty, op, method, span)
    }

    /// Rejects an operator applied to a generic-parameter operand whose
    /// bounds do not license it.
    ///
    /// A parameter stands for every type a caller may supply, so only a
    /// bound declaring the operator's method guarantees each instantiation
    /// can perform the operation. Returns `true` when a diagnostic was
    /// emitted.
    pub(super) fn reject_operator_off_bound(
        &mut self,
        operand_ty: Ty,
        op: &str,
        method: &str,
        span: Span,
    ) -> bool {
        let peeled = self.peel_refs(operand_ty);
        let Some(TyKind::Param { idx, name }) = self.tcx.kind(peeled) else {
            return false;
        };
        let param = name.to_string();
        let bounds = self
            .current_param_bounds
            .get(idx.0 as usize)
            .cloned()
            .unwrap_or_default();
        let trait_name = op_trait_name(method);
        // A bound licenses the operator when it is the operator's own trait,
        // when it (or one of its supertraits) declares the operator method,
        // or when its method surface is unknown and so cannot rule the
        // operation out.
        let licensed = bounds.iter().any(|bound| {
            bound == trait_name
                || self
                    .trait_own_methods
                    .get(bound)
                    .is_some_and(|methods| methods.contains(method))
                || self.supertrait_owning_method(bound, method).is_some()
                || (!self.declared_trait_names.contains(bound)
                    && builtin_trait_methods(bound).is_none())
        });
        if licensed {
            return false;
        }
        self.emit(
            TypeError::OperatorNotOnBound {
                param,
                op: op.to_string(),
                trait_name: trait_name.to_string(),
                method: method.to_string(),
                bounds,
            },
            span,
        );
        true
    }

    /// Rejects a method on a bound type-parameter receiver that resolves
    /// only through a *supertrait* of one of the parameter's bounds
    /// (P0-5: `fn describe<T: Pet>(p: &T)` calling `p.name()` where
    /// `name` is declared on `Animal` and `trait Pet: Animal`). The
    /// compiled tiers cannot lower supertrait-through-bound dispatch
    /// (SPEC §3.8); it runs right on the VM but miscompiles native, so it
    /// is rejected uniformly. Returns `true` when a diagnostic was
    /// emitted.
    pub(super) fn reject_supertrait_method_through_bound(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        span: Span,
    ) -> bool {
        let mut t = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(t) {
            t = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Param { idx, .. }) = self.tcx.kind(t) else {
            return false;
        };
        let idx = *idx;
        let Some(bounds) = self.current_param_bounds.get(idx.0 as usize).cloned() else {
            return false;
        };
        // If the method is declared directly on any bound, it is a normal
        // generic-bound call (handled elsewhere), not a supertrait leak.
        for bound in &bounds {
            if self
                .trait_own_methods
                .get(bound)
                .is_some_and(|m| m.contains(method))
            {
                return false;
            }
        }
        for bound in &bounds {
            if let Some(supertrait) = self.supertrait_owning_method(bound, method) {
                let param = self
                    .current_generic_scope
                    .iter()
                    .find(|(_, (pidx, _))| *pidx == idx)
                    .map_or_else(|| "T".to_string(), |(name, _)| name.clone());
                self.emit(
                    TypeError::SupertraitMethodThroughBound {
                        param,
                        method: method.to_string(),
                        bound: bound.clone(),
                        supertrait,
                    },
                    span,
                );
                return true;
            }
        }
        false
    }

    /// Walks the supertrait graph of `trait_name` (transitively) and
    /// returns the first supertrait that declares `method`, or `None`.
    pub(super) fn supertrait_owning_method(
        &self,
        trait_name: &str,
        method: &str,
    ) -> Option<String> {
        let mut stack: Vec<String> = self
            .trait_supertraits
            .get(trait_name)
            .cloned()
            .unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if self
                .trait_own_methods
                .get(&name)
                .is_some_and(|m| m.contains(method))
            {
                return Some(name);
            }
            if let Some(supers) = self.trait_supertraits.get(&name) {
                stack.extend(supers.iter().cloned());
            }
        }
        None
    }

    /// Pre-scans a function body for `archive::{tar,zip}::write(arg)`
    /// calls whose single argument is a path to a local binding, and
    /// records that binding's node so its literal initializer is later
    /// re-typed to the `[(String, [u8])]` parameter.
    pub(super) fn collect_write_arg_bindings(&mut self, body: &Expr) {
        let mut collector = WriteArgPathCollector {
            arg_paths: Vec::new(),
        };
        gossamer_ast::visitor::Visitor::visit_expr(&mut collector, body);
        if collector.arg_paths.is_empty() {
            return;
        }
        let vec_pair = self.archive_entry_vec_ty();
        for path_node in collector.arg_paths {
            if let Some(Resolution::Local(binding)) = self.resolutions.get(path_node) {
                self.write_arg_bindings.insert(binding, vec_pair);
            }
        }
    }

    pub(super) fn bind_fn_param(&mut self, param: &FnParam) {
        match param {
            FnParam::Typed { pattern, ty, .. } => {
                let param_ty = self.type_from_ast(ty);
                self.check_param_reference_pattern(pattern, param_ty);
                self.bind_pattern(pattern, param_ty);
            }
            FnParam::Receiver(recv) => {
                // Bind `self` to the enclosing `impl`'s `Self` type so
                // `self.field` accesses resolve; fall back to a fresh var
                // only outside an impl context (defensive). `self` and
                // `&self` name the same value - a receiver is reached
                // without copying either way - so only `&mut self`, which
                // writes back, carries a reference in the type.
                let ty = match self.current_self_ty {
                    Some(self_ty) => match recv {
                        gossamer_ast::Receiver::Owned | gossamer_ast::Receiver::RefShared => {
                            self_ty
                        }
                        gossamer_ast::Receiver::RefMut => self.tcx.intern(TyKind::Ref {
                            mutability: Mutbl::Mut,
                            inner: self_ty,
                        }),
                    },
                    None => self.fresh(),
                };
                self.bind_local("self", ty);
                // Receiver syntax controls referent capability, not whether
                // the local `self` slot may be rebound.
                self.bind_local_mutability("self", false);
            }
        }
    }

    pub(super) fn check_expr(&mut self, expr: &Expr) -> Ty {
        self.check_expr_expecting(expr, Expectation::None)
    }

    pub(super) fn check_expr_expecting(&mut self, expr: &Expr, expected: Expectation) -> Ty {
        if self.enter_recursion(expr.span).is_err() {
            let err = self.tcx.error_ty();
            return self.record(expr.id, err);
        }
        let ty = self.check_expr_kind(expr, expected);
        self.check_ffi_expr(expr, ty);
        self.check_expected_integer_literal_range(expr, expected, ty);
        self.leave_recursion();
        self.record(expr.id, ty)
    }

    pub(super) fn check_expected_integer_literal_range(
        &mut self,
        expr: &Expr,
        expected: Expectation,
        actual: Ty,
    ) {
        let (text, has_suffix) = match &expr.kind {
            ExprKind::Literal(Literal::Int(text)) => (
                text.clone(),
                INT_SUFFIXES
                    .iter()
                    .any(|(suffix, _)| text.ends_with(suffix)),
            ),
            ExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } => match &operand.kind {
                ExprKind::Literal(Literal::Int(text)) => (
                    format!("-{text}"),
                    INT_SUFFIXES
                        .iter()
                        .any(|(suffix, _)| text.ends_with(suffix)),
                ),
                _ => return,
            },
            _ => return,
        };
        if matches!(self.tcx.kind(actual), Some(TyKind::Error)) || has_suffix {
            return;
        }
        let Some(expected) = self.expectation_target(expected) else {
            return;
        };
        let Some(TyKind::Int(int_ty)) = self.tcx.kind(expected).cloned() else {
            return;
        };
        if !int_literal_fits(&text, int_ty) {
            self.emit(
                TypeError::IntLiteralOverflow {
                    literal: text,
                    ty: int_ty.as_str().to_string(),
                },
                expr.span,
            );
        }
    }

    /// Resolves the expectation to the structural type it imposes,
    /// peeling one `Ref` - a `&[T]` parameter shapes a bare `[..]`
    /// literal exactly like `[T]` (the borrow is transparent at the
    /// layout level).
    pub(super) fn expectation_target(&mut self, expected: Expectation) -> Option<Ty> {
        let ty = expected.ty()?;
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Ref { inner, .. }) => Some(self.infer.resolve(self.tcx, *inner)),
            Some(_) => Some(resolved),
            None => None,
        }
    }

    /// Type of a `loop` expression: the unified type of its value-carrying
    /// breaks (`let x = loop { break v }` => `x: typeof(v)`); a value-less
    /// loop keeps the divergent `never` type.
    pub(super) fn check_loop(&mut self, body: &Expr) -> Ty {
        let break_ty = self.fresh();
        self.loop_break_tys.push((break_ty, false));
        self.check_discarded_expr(body);
        self.report_discarded_result(body, None);
        let (break_ty, used) = self.loop_break_tys.pop().expect("loop stack");
        if used {
            self.infer.resolve(self.tcx, break_ty)
        } else {
            self.tcx.never()
        }
    }

    /// Type-checks a `return value` / `break value`; both diverge (`never`)
    /// but thread their value into the enclosing function return type or the
    /// loop break-type var respectively.
    pub(super) fn check_return_or_break(&mut self, expr: &Expr, value: Option<&Expr>) -> Ty {
        if let Some(value) = value {
            // `return [..]` carries the declared return shape the same way the
            // block-tail path does, so an explicit `return []` in a `-> [T]` fn
            // is shaped as a Vec rather than a fixed `[T; 0]`.
            let value_expected = match (&expr.kind, self.current_fn_ret) {
                (ExprKind::Return(_), Some(ret)) => Expectation::HasType(ret),
                _ => Expectation::None,
            };
            let got = self.check_expr_expecting(value, value_expected);
            // The expectation only shapes literal containers; unify the checked
            // value against the declared return type so a non-literal mismatch
            // is reported the same way a block tail is.
            if let (ExprKind::Return(_), Some(ret)) = (&expr.kind, self.current_fn_ret) {
                self.record_fn_item_coercion(value, ret);
                self.unify(ret, got, value.span);
            }
            // `break value` unifies its value with the enclosing loop's
            // break-type var and marks the loop as value-yielding.
            if matches!(expr.kind, ExprKind::Break { .. }) {
                let break_ty = self.loop_break_tys.last_mut().map(|last| {
                    last.1 = true;
                    last.0
                });
                if let Some(break_ty) = break_ty {
                    self.unify(break_ty, got, value.span);
                }
            }
        } else if matches!(expr.kind, ExprKind::Return(_))
            && let Some(ret) = self.current_fn_ret
        {
            let unit = self.tcx.unit();
            self.unify(ret, unit, expr.span);
        }
        self.tcx.never()
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "expression dispatch - arms map 1:1 to ExprKind variants; splitting hides the dispatch table"
    )]
    pub(super) fn check_expr_kind(&mut self, expr: &Expr, expected: Expectation) -> Ty {
        match &expr.kind {
            ExprKind::Literal(lit) => self.type_of_literal(lit, expr.span),
            ExprKind::Path(path) => self.check_path_expr(expr.id, path, expr.span, expected),
            ExprKind::Call { callee, args } => {
                let ty = self.check_call(callee, args, expected);
                if let ExprKind::Path(path) = &callee.kind
                    && let Some(last) = path.segments.last()
                {
                    // The prelude `spawn`, not a module's own (`exec::spawn`).
                    if last.name.name == "spawn"
                        && path.segments.len() == 1
                        && let Some(arg) = args.first()
                    {
                        // An aggregate written inline at the spawned call has
                        // no owner on either side of the boundary. A reference
                        // the closure body creates and consumes never crosses
                        // it, so only a captured one is rejected, which
                        // `reject_unshareable_goroutine_captures` covers.
                        self.reject_spawn_inline_aggregate_args(arg);
                        self.reject_unshareable_goroutine_captures(arg);
                    }
                    // The guarded slot is one word that every tier reads
                    // back as an integer. A payload without that agreement
                    // is refused here rather than compiling on one tier and
                    // failing to lower on another.
                    if last.name.name == "new"
                        && path
                            .segments
                            .iter()
                            .any(|segment| segment.name.name == "Shared")
                        && let Some(arg) = args.first()
                        && let Some(arg_ty) = self.table.get(arg.id)
                    {
                        // Judged once inference has settled: a numeric
                        // literal is still an open variable here, and
                        // whether it lands on an integer decides the answer.
                        self.deferred_shared_payloads.push((arg_ty, arg.span));
                    }
                }
                // A non-generic tuple-variant constructor call is its
                // enum: unify so bindings (`let p = Sign::Pos(7)`) carry
                // the nominal type operator dispatch resolves against.
                // Generic enums are absent from `enum_tys` and keep the
                // fresh-var path.
                if let Some(e) = self.variant_ctor_enum_ty(expr) {
                    let r = self.infer.resolve(self.tcx, ty);
                    if matches!(self.tcx.kind(r), Some(TyKind::Var(_))) {
                        self.unify(ty, e, expr.span);
                    }
                }
                ty
            }
            ExprKind::MethodCall {
                receiver,
                name,
                name_span,
                desugared_from,
                generics,
                args,
            } => {
                let written = desugared_from
                    .as_ref()
                    .map_or(name.name.as_str(), |i| &i.name);
                // Scoped to this call: a method call nested in the receiver or
                // an argument sets its own spelling and restores this one.
                let outer = std::mem::replace(&mut self.written_method, written.to_string());
                let ty = self.check_method_call(
                    MethodCallSite {
                        call_id: expr.id,
                        call_span: expr.span,
                        method: &name.name,
                        name_span: *name_span,
                        generics,
                    },
                    receiver,
                    args,
                    expected,
                );
                self.written_method = outer;
                ty
            }
            ExprKind::FieldAccess { receiver, field } => {
                let receiver_ty = self.check_expr(receiver);
                match field {
                    gossamer_ast::FieldSelector::Named(name) => {
                        self.reject_private_field(receiver_ty, &name.name, expr.span);
                        match self.lookup_field_ty_diagnosed(receiver_ty, &name.name) {
                            Ok(ty) => ty,
                            Err(err) => {
                                self.emit(
                                    with_field_span(err, field_name_span(expr, &name.name)),
                                    expr.span,
                                );
                                self.fresh()
                            }
                        }
                    }
                    gossamer_ast::FieldSelector::Index(idx) => {
                        self.check_tuple_field(receiver_ty, *idx, expr.span)
                    }
                }
            }
            ExprKind::Unary { op, operand } => self.check_unary(*op, operand, expr.span, expected),
            ExprKind::Index { base, index } => self.check_index_expr(base, index, expr.span),
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(*op, lhs, rhs, expr.span),
            ExprKind::Assign { place, value, op } => self.check_assign(place, value, *op),
            ExprKind::Cast { value, ty } => {
                let from = self.check_expr(value);
                let to = self.type_from_ast(ty);
                self.check_cast(from, to, expr.span);
                to
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.check_if(condition, then_branch, else_branch.as_deref(), expected),
            ExprKind::Match { scrutinee, arms } => self.check_match(scrutinee, arms, expected),
            ExprKind::Loop { body, .. } => self.check_loop(body),
            ExprKind::While {
                condition, body, ..
            } => {
                let bool_ty = self.tcx.bool_ty();
                let cond_ty = self.check_expr(condition);
                self.unify(bool_ty, cond_ty, condition.span);
                self.check_discarded_expr(body);
                self.report_discarded_result(body, None);
                self.tcx.unit()
            }
            ExprKind::For {
                pattern,
                iter,
                body,
                ..
            } => self.check_for(pattern, iter, body),
            ExprKind::Block(block) => self.check_block(block, expected),
            ExprKind::Unsafe(block) => {
                self.unsafe_depth += 1;
                let ty = self.check_block(block, expected);
                self.unsafe_depth -= 1;
                ty
            }
            ExprKind::Closure { params, ret, body } => {
                self.check_closure(params, ret.as_ref(), body, expected)
            }
            ExprKind::Return(value) | ExprKind::Break { value, .. } => {
                self.check_return_or_break(expr, value.as_deref())
            }
            ExprKind::Continue { .. } => self.tcx.never(),
            ExprKind::Tuple(elems) => {
                let want: Option<Vec<Ty>> = match self.expectation_target(expected) {
                    Some(target) => match self.tcx.kind(target) {
                        Some(TyKind::Tuple(tys)) if tys.len() == elems.len() => Some(tys.clone()),
                        _ => None,
                    },
                    None => None,
                };
                if let Some(want) = want {
                    for (elem, want_ty) in elems.iter().zip(&want) {
                        let got = self.check_expr_expecting(elem, expected.rewrap(*want_ty));
                        if expected.unifies() {
                            self.unify(*want_ty, got, elem.span);
                        }
                    }
                    return self.tcx.intern(TyKind::Tuple(want));
                }
                let tys: Vec<Ty> = elems.iter().map(|e| self.check_expr(e)).collect();
                self.tcx.intern(TyKind::Tuple(tys))
            }
            ExprKind::MapLiteral(entries) => self.check_map_literal(entries, expected),
            ExprKind::SetLiteral(entries) => self.check_set_literal(entries, expected),
            ExprKind::Struct {
                path,
                fields,
                base,
                syntax,
            } => {
                // Resolve the header path to an Adt type. Unifying
                // named field values with the declared field
                // types lets downstream field-access nodes see
                // concrete leaf types.
                //
                // For a generic struct (`Pair<A, B>`), the
                // declared field types carry `TyKind::Param`
                // slots. We allocate one fresh inference variable
                // per generic parameter and substitute those into
                // each field type before unifying with the
                // literal's value type - that lets the inferencer
                // pin `A` and `B` from the field values.
                let head_node = expr.id;
                // A `use`d type keeps its opaque `Import` resolution; the
                // definition it names is what the literal is built from.
                let head_res = self.resolutions.get(head_node).map(|res| match res {
                    Resolution::Import { .. } => self
                        .resolutions
                        .import_def(head_node)
                        .and_then(|def| {
                            self.resolutions
                                .kind_of(def)
                                .map(|kind| Resolution::Def { def, kind })
                        })
                        .unwrap_or(res),
                    other => other,
                });
                let (struct_ty, substs_table) = if let Some(res) = head_res {
                    match res {
                        Resolution::Def {
                            def,
                            kind:
                                gossamer_resolve::DefKind::Struct | gossamer_resolve::DefKind::Enum,
                        } => {
                            let arity = self.struct_generic_arity.get(&def).copied().unwrap_or(0);
                            let const_mask = self.fn_generic_const_mask_of(def);
                            let placeholder = self.tcx.error_ty();
                            let substs: Vec<Ty> = (0..arity)
                                .map(|i| {
                                    if const_mask.get(i).copied().unwrap_or(false) {
                                        placeholder
                                    } else {
                                        self.fresh()
                                    }
                                })
                                .collect();
                            let substs_obj = crate::Substs::from_types(substs.iter().copied());
                            self.defer_adt_bounds(def, &substs, expr.span);
                            (
                                self.tcx.intern(TyKind::Adt {
                                    def,
                                    substs: substs_obj,
                                }),
                                substs,
                            )
                        }
                        _ => (self.fresh(), Vec::new()),
                    }
                } else {
                    (self.fresh(), Vec::new())
                };
                // `http::Response { … }` - no resolver entry (stdlib
                // opaque type). Pin the literal to the sentinel
                // Response Adt and check the known field shapes so a
                // wrong-typed field reports a clean type mismatch
                // instead of slipping through as an inference
                // variable. `body` stays unchecked: the runtime
                // accepts both String and `[u8]` bodies.
                let resolved_probe = self.infer.resolve(self.tcx, struct_ty);
                let path_tail = path.segments.last().map(|s| s.name.name.as_str());
                let (struct_ty, http_response_fields) =
                    if matches!(self.tcx.kind_of(resolved_probe), TyKind::Var(_))
                        && path_tail == Some("Response")
                    {
                        let def = gossamer_resolve::DefId::local(u32::MAX - 5);
                        let response_ty = self.tcx.intern(TyKind::Adt {
                            def,
                            substs: crate::Substs::new(),
                        });
                        let s = self.tcx.string_ty();
                        let pair = self.tcx.intern(TyKind::Tuple(vec![s, s]));
                        let headers_ty = self.tcx.intern(TyKind::Vec(pair));
                        let fields: Vec<(String, Ty)> = vec![
                            ("status".to_string(), self.tcx.int_ty(IntTy::I64)),
                            ("body".to_string(), self.fresh()),
                            ("content_type".to_string(), s),
                            ("headers".to_string(), headers_ty),
                        ];
                        (response_ty, Some(fields))
                    } else {
                        (struct_ty, None)
                    };
                let resolved = self.infer.resolve(self.tcx, struct_ty);
                let http_response_literal = http_response_fields.is_some();
                let declared: Option<Vec<(String, Ty)>> = match self.tcx.kind_of(resolved) {
                    // The literal-specific Response list takes priority
                    // over the stdlib layout: the layout declares
                    // `body: String`, but literal bodies may also be
                    // `[u8]` byte arrays (interp parity).
                    TyKind::Adt { def, .. } => {
                        http_response_fields.or_else(|| self.struct_fields.get(def).cloned())
                    }
                    _ => None,
                };
                let tuple_struct_literal = if let TyKind::Adt { def, .. } =
                    self.tcx.kind_of(resolved).clone()
                {
                    let is_tuple = self.tcx.is_tuple_struct(def.local);
                    if matches!(syntax, gossamer_ast::expr::StructExprSyntax::Braced) && is_tuple {
                        let name = self
                            .tcx
                            .def_name(def)
                            .map_or_else(|| "<struct>".to_string(), ToString::to_string);
                        self.emit(
                            TypeError::TupleStructConstructorParenthesesRequired { name },
                            expr.span,
                        );
                    }
                    is_tuple
                } else {
                    false
                };
                let require_all_fields = !http_response_literal;
                if !tuple_struct_literal
                    && fields
                        .iter()
                        .any(|field| struct_literal_positional_index(&field.name.name).is_some())
                {
                    let name = path.segments.last().map_or_else(
                        || "<struct>".to_string(),
                        |segment| segment.name.name.clone(),
                    );
                    self.emit(TypeError::NamedStructFieldsRequired { name }, expr.span);
                }
                let resolved_literal_fields =
                    if !tuple_struct_literal && let Some(declared_fields) = declared.as_ref() {
                        Some(self.resolve_struct_literal_fields(
                            path,
                            fields,
                            base.is_some(),
                            declared_fields,
                            require_all_fields,
                            expr.span,
                        ))
                    } else {
                        None
                    };
                // Naming a field in a literal is a reference to it, so the
                // same visibility rule applies: a struct with a private
                // field cannot be built from outside its declaring module.
                // A `..base` spread carries every field the literal does not
                // name, so it references all of them.
                if let TyKind::Adt { def, .. } = self.tcx.kind_of(resolved).clone()
                    && let (Some(literal_fields), Some(declared_fields)) =
                        (resolved_literal_fields.as_ref(), declared.as_ref())
                {
                    let referenced: Vec<String> = if base.is_some() {
                        declared_fields
                            .iter()
                            .map(|(field_name, _)| field_name.clone())
                            .collect()
                    } else {
                        literal_fields
                            .values()
                            .filter_map(|&idx| declared_fields.get(idx))
                            .map(|(field_name, _)| field_name.clone())
                            .collect()
                    };
                    for field_name in referenced {
                        self.reject_private_field_of(def, &field_name, expr.span);
                    }
                }
                let mut literal_consts =
                    LiteralConsts::new(self.fn_generic_const_mask_of_ty(struct_ty));
                for (field_idx, field) in fields.iter().enumerate() {
                    if let Some(value) = &field.value {
                        // Substitute `Param { idx }` slots with the
                        // fresh inference vars allocated above so
                        // unification can drive `A`, `B`, ... from
                        // each literal's value type. Checking the
                        // value against the declared field type lets
                        // `S { xs: ["a", "b"] }` lay a heap Vec, not
                        // a fixed `[T; N]`, into a Vec-typed field.
                        let dty_sub = declared.as_ref().and_then(|declared_fields| {
                            resolved_literal_fields
                                .as_ref()
                                .and_then(|resolved| resolved.get(&field_idx).copied())
                                .and_then(|decl_idx| declared_fields.get(decl_idx))
                                .or_else(|| {
                                    declared_fields.iter().find(|(n, _)| n == &field.name.name)
                                })
                                .map(|(_, dty)| *dty)
                        });
                        let dty_sub =
                            dty_sub.map(|dty| self.subst_params_in_ty(dty, &substs_table));
                        // An array field whose length is a const parameter
                        // takes the length the value has; only the element
                        // type is expected of the value.
                        let infers_const = dty_sub
                            .is_some_and(|dty| literal_consts.is_const_array_field(self.tcx, dty));
                        let field_expected = match dty_sub {
                            Some(_) if infers_const => Expectation::None,
                            Some(dty) => Expectation::HasType(dty),
                            None => Expectation::None,
                        };
                        let val_ty = self.check_expr_expecting(value, field_expected);
                        if let Some(dty) = dty_sub {
                            let dty = if infers_const {
                                literal_consts.infer_from_field(self, dty, val_ty);
                                literal_consts.apply(self, dty, &substs_table)
                            } else {
                                dty
                            };
                            self.unify(dty, val_ty, value.span);
                        }
                    }
                }
                if let Some(base) = base {
                    self.check_expr(base);
                }
                if literal_consts.has_const_positions() {
                    self.finish_struct_literal_consts(
                        struct_ty,
                        &literal_consts,
                        expected,
                        path,
                        expr.span,
                    )
                } else {
                    struct_ty
                }
            }
            ExprKind::Array(arr) => {
                let target = self.expectation_target(expected);
                let wants_growable = target.is_some_and(|target| {
                    matches!(
                        self.tcx.kind(target),
                        Some(TyKind::Vec(_) | TyKind::Slice(_))
                    )
                });
                let wants_array = target.is_some_and(|target| {
                    matches!(self.tcx.kind(target), Some(TyKind::Array { .. }))
                });
                let _ = wants_growable;
                if wants_array {
                    self.check_array(arr, expected)
                } else {
                    self.check_vec_literal(arr, expected)
                }
            }
            ExprKind::FixedArray(arr) => self.check_array(arr, expected),
            ExprKind::Range { start, end, .. } => {
                // Rust-style ranges are lazy values. Index and for-loop
                // positions still consume their bounds syntactically.
                let start_ty = start.as_ref().map(|bound| self.check_expr(bound));
                let end_ty = end.as_ref().map(|bound| self.check_expr(bound));
                let elem = start_ty
                    .filter(|ty| self.is_integer(*ty))
                    .or_else(|| end_ty.filter(|ty| self.is_integer(*ty)))
                    .unwrap_or_else(|| self.tcx.int_ty(IntTy::I64));
                if let (Some(bound), Some(ty)) = (start, start_ty) {
                    self.unify(elem, ty, bound.span);
                }
                if let (Some(bound), Some(ty)) = (end, end_ty) {
                    self.unify(elem, ty, bound.span);
                }
                self.tcx.range_ty(elem)
            }
            ExprKind::Try(inner) => {
                let inner_expectation = match self.expectation_target(expected) {
                    Some(ok) => {
                        let err = self.tcx.dyn_error_ty();
                        Expectation::HasType(self.result_adt_ty(ok, err))
                    }
                    None => Expectation::None,
                };
                let inner_ty = self.check_expr_expecting(inner, inner_expectation);
                self.check_question_mark(inner_ty, inner.span)
            }
            ExprKind::Select(arms) => self.check_select(arms),
            ExprKind::Error => self.fresh(),
        }
    }

    /// Returns the type of field `idx` when `ty` is a tuple struct (an
    /// `Adt` whose `idx`-th field is the positional name "idx"), else
    /// `None`. Tuple-struct fields are modelled as named fields "0".."N-1",
    /// so `p.0` positional access reads field "0".
    pub(super) fn tuple_struct_field_ty(&self, ty: Ty, idx: u32) -> Option<Ty> {
        let TyKind::Adt { def, substs } = self.tcx.kind_of(ty).clone() else {
            return None;
        };
        let is_positional = self
            .struct_fields
            .get(&def)
            .and_then(|list| list.get(idx as usize))
            .is_some_and(|(name, _)| *name == idx.to_string());
        if !is_positional {
            return None;
        }
        self.tcx
            .adt_field_tys(def, &substs)
            .and_then(|tys| tys.get(idx as usize).copied())
    }

    pub(super) fn resolve_struct_literal_fields(
        &mut self,
        path: &gossamer_ast::PathExpr,
        fields: &[gossamer_ast::StructExprField],
        has_base: bool,
        declared_fields: &[(String, Ty)],
        require_all_fields: bool,
        span: Span,
    ) -> HashMap<usize, usize> {
        let name = path.segments.last().map_or_else(
            || "<struct>".to_string(),
            |segment| segment.name.name.clone(),
        );
        let declared_by_name: HashMap<&str, usize> = declared_fields
            .iter()
            .enumerate()
            .map(|(idx, (field_name, _))| (field_name.as_str(), idx))
            .collect();
        let mut resolved = HashMap::new();
        let mut filled = HashSet::new();
        let mut keyed_seen = HashSet::new();
        for (field_idx, field) in fields.iter().enumerate() {
            if struct_literal_positional_index(&field.name.name).is_some() {
                continue;
            }
            let Some(&decl_idx) = declared_by_name.get(field.name.name.as_str()) else {
                self.emit(
                    TypeError::UnknownField {
                        ty: name.clone(),
                        field: field.name.name.clone(),
                        opaque: false,
                        declared: declared_fields
                            .iter()
                            .map(|(field_name, _)| field_name.clone())
                            .collect(),
                        field_span: None,
                        method_of_same_name: false,
                    },
                    span,
                );
                continue;
            };
            if !keyed_seen.insert(field.name.name.as_str()) {
                self.emit(
                    TypeError::DuplicateStructField {
                        name: name.clone(),
                        field: field.name.name.clone(),
                    },
                    span,
                );
            }
            filled.insert(decl_idx);
            resolved.insert(field_idx, decl_idx);
        }

        let mut next_pos = 0usize;
        for (field_idx, field) in fields.iter().enumerate() {
            if struct_literal_positional_index(&field.name.name).is_none() {
                continue;
            }
            while next_pos < declared_fields.len() && filled.contains(&next_pos) {
                next_pos += 1;
            }
            if next_pos >= declared_fields.len() {
                self.emit(
                    TypeError::TooManyStructFields {
                        name: name.clone(),
                        expected: declared_fields.len(),
                        found: fields.len(),
                    },
                    span,
                );
                continue;
            }
            filled.insert(next_pos);
            resolved.insert(field_idx, next_pos);
        }

        if require_all_fields && !has_base {
            for (idx, (field_name, _)) in declared_fields.iter().enumerate() {
                if !filled.contains(&idx) {
                    self.emit(
                        TypeError::MissingStructField {
                            name: name.clone(),
                            field: field_name.clone(),
                        },
                        span,
                    );
                }
            }
        }
        resolved
    }

    /// Type of `value.N` positional access. Rejects access on a concrete
    /// non-tuple receiver and out-of-range indices (GT0023); a still-
    /// unresolved receiver is deferred for re-check after defaulting.
    pub(super) fn check_tuple_field(&mut self, receiver_ty: Ty, idx: u32, span: Span) -> Ty {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(resolved).clone() {
            resolved = self.infer.resolve(self.tcx, inner);
        }
        if let Some(fty) = self.tuple_struct_field_ty(resolved, idx) {
            return fty;
        }
        match self.tcx.kind_of(resolved).clone() {
            TyKind::Tuple(elems) => elems.get(idx as usize).copied().unwrap_or_else(|| {
                let ty = self.render_public_ty(resolved);
                self.emit(
                    TypeError::NoTupleField {
                        ty,
                        index: u64::from(idx),
                    },
                    span,
                );
                self.fresh()
            }),
            TyKind::Var(_) => {
                let result = self.fresh();
                self.deferred_structural.push(DeferredStructural {
                    ty: resolved,
                    span,
                    kind: DeferredStructuralKind::TupleField(u64::from(idx)),
                    result: Some(result),
                });
                result
            }
            other => {
                if !is_soft_for_structural_use(&other) {
                    let ty = self.render_public_ty(resolved);
                    self.emit(
                        TypeError::NoTupleField {
                            ty,
                            index: u64::from(idx),
                        },
                        span,
                    );
                }
                self.fresh()
            }
        }
    }

    /// Element type of `base[index]`. Rejects indexing a concrete
    /// non-indexable receiver (GT0021); a still-unresolved receiver is
    /// deferred for re-check after defaulting.
    pub(super) fn check_index_expr(&mut self, base: &Expr, index: &Expr, span: Span) -> Ty {
        let base_ty = self.check_expr(base);
        let index_ty = self.check_expr(index);
        let range_index = matches!(index.kind, ExprKind::Range { .. });
        let mut cur = self.infer.resolve(self.tcx, base_ty);
        loop {
            match self.tcx.kind_of(cur).clone() {
                TyKind::Ref { inner, .. } => cur = inner,
                // `m[k]` reads the value stored under `k`; a key the map does
                // not hold panics, as an index past a vector's end does.
                TyKind::HashMap { key, value, .. } if !range_index => {
                    self.unify(key, index_ty, index.span);
                    return value;
                }
                TyKind::Simd { elem, .. } if !range_index => return elem,
                TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => {
                    if range_index {
                        return self.tcx.intern(TyKind::Vec(elem));
                    }
                    return elem;
                }
                TyKind::String => {
                    if range_index {
                        return self.tcx.string_ty();
                    }
                    return self.tcx.char_ty();
                }
                TyKind::Var(_) => {
                    let result = self.fresh();
                    self.deferred_structural.push(DeferredStructural {
                        ty: cur,
                        span,
                        kind: DeferredStructuralKind::Index,
                        result: Some(result),
                    });
                    return result;
                }
                other => {
                    // `a[i]` on a user struct / enum routes to its `index` impl
                    // method (one argument); the element type is that method's
                    // return type. The base node is anchored to its resolved
                    // nominal type so tier lowering dispatches the call.
                    if matches!(other, TyKind::Adt { .. }) && self.adt_name_of(cur).is_some() {
                        self.record(base.id, cur);
                        if let Some(ret) = self.adt_op_method_ret(cur, "index", 1) {
                            return ret;
                        }
                    }
                    if !is_soft_for_structural_use(&other) {
                        let ty = self.render_public_ty(cur);
                        self.emit(TypeError::NotIndexable { ty }, span);
                    }
                    return self.fresh();
                }
            }
        }
    }

    pub(super) fn check_call(&mut self, callee: &Expr, args: &[Expr], expected: Expectation) -> Ty {
        if let (ExprKind::Path(path), [source]) = (&callee.kind, args)
            && path.segments.len() == 1
            && path.segments[0].name.name == "__gos_codegen"
        {
            // `codegen(src)` is replaced by the code `src` spells before the
            // program is checked for real, so its type is whatever that code
            // answers where it is spliced; only the source text is checked
            // here.
            let string = self.tcx.string_ty();
            let got = self.check_expr_expecting(source, Expectation::HasType(string));
            self.unify(string, got, source.span);
            return self.fresh();
        }
        self.check_overlapping_mutable_call_args(args);
        self.check_foreign_call_site(callee);
        self.note_addr_of_argument(callee, args);
        if matches!(callee.kind, ExprKind::Path(_)) {
            self.callee_path_nodes.insert(callee.id);
        }
        let callee_ty = self.check_expr(callee);
        let arg_expectations = self.call_arg_expectations(callee, callee_ty, args.len(), expected);
        let arg_tys: Vec<Ty> = match self.data_last_combinator_arg_tys(callee, args) {
            Some(tys) => tys,
            None => self.check_call_args(callee, callee_ty, args, arg_expectations.as_deref()),
        };
        self.check_mutating_qualified_call(callee, args);
        self.check_by_value_argument_aliases(args);
        self.record_qualified_method_const_generic_args(callee, &arg_tys);
        self.check_exponent_placeholder(callee, args);
        self.check_callback_arguments(callee, args, &arg_tys);
        let ret = self.check_call_inner(callee, args, callee_ty, &arg_tys, expected);
        self.check_ffi_operation(callee, args, &arg_tys, ret);
        ret
    }

    /// `{:e}` renders a number in scientific notation. The placeholder
    /// expands to `__gos_fmt_exp(__concat(value), ..)`, so the value is the
    /// operand of the inner rendering call.
    pub(super) fn check_exponent_placeholder(&mut self, callee: &Expr, args: &[Expr]) {
        let ExprKind::Path(path) = &callee.kind else {
            return;
        };
        if path
            .segments
            .last()
            .is_none_or(|segment| segment.name.name != "__gos_fmt_exp")
        {
            return;
        }
        let Some(ExprKind::Call { args: rendered, .. }) = args.first().map(|arg| &arg.kind) else {
            return;
        };
        let Some(value) = rendered.first() else {
            return;
        };
        let Some(ty) = self.table.get(value.id) else {
            return;
        };
        let resolved = self.peel_refs(self.infer.resolve(self.tcx, ty));
        let numeric = matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Int(_) | TyKind::Float(_))
        ) || self.infer.is_integer_constrained_var(self.tcx, resolved)
            || self.infer.is_float_literal_var(self.tcx, resolved);
        if !numeric {
            let found = self.render_public_ty(resolved);
            self.emit(
                TypeError::TypeMismatch {
                    expected: "an integer or float".to_string(),
                    found,
                },
                value.span,
            );
        }
    }

    /// Argument types for a data-last `iter::` combinator, checked with the
    /// sequence argument first. The element type it yields binds the leading
    /// closure's parameter, so a projection out of that parameter resolves
    /// while the closure body is checked. Returns `None` for every other
    /// call, which keeps source-order checking.
    pub(super) fn data_last_combinator_arg_tys(
        &mut self,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Vec<Ty>> {
        if args.len() < 2 {
            return None;
        }
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let names = self.resolved_value_path_names(callee.id, path);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (module, last) = names.split_at(names.len().saturating_sub(1));
        let name = last.first().copied()?;
        if combinator_module_name(module)? != "iter"
            || Self::std_combinator_arity("iter", name)? != args.len()
        {
            return None;
        }
        let data_index = args.len() - 1;
        let data_ty = self.check_expr_expecting(&args[data_index], Expectation::None);
        let elem = match self.tcx.kind(self.infer.resolve(self.tcx, data_ty)) {
            Some(
                TyKind::Vec(elem)
                | TyKind::Slice(elem)
                | TyKind::Array { elem, .. }
                | TyKind::Iterator(elem),
            ) => *elem,
            _ => return None,
        };
        let mut arg_tys = vec![data_ty; args.len()];
        for (i, arg) in args.iter().enumerate().take(data_index) {
            let expectation = match &arg.kind {
                ExprKind::Closure { params, .. } if params.len() == 1 => {
                    let output = self.fresh();
                    let sig = FnSig {
                        inputs: vec![elem],
                        output,
                    };
                    Expectation::HasType(self.tcx.intern(TyKind::FnPtr(sig)))
                }
                _ => Expectation::None,
            };
            arg_tys[i] = self.check_expr_expecting(arg, expectation);
        }
        Some(arg_tys)
    }

    /// Per-argument expectations for a call, derived (in priority
    /// order) from the callee's known signature, a variant
    /// constructor's declared payload types (`Value::Blob([1, 2, 3])`
    /// shapes its payload as a heap `[u8]`, not a fixed `[i64; 3]`),
    /// the stdlib archive-write parameter, or - for the bare `Some` /
    /// `Ok` / `Err` constructors - the call's own expected type.
    /// Checks a call's arguments in order, except that a closure handed to a
    /// generic function is checked after the other arguments.
    ///
    /// The other arguments say which types the function's parameters stand
    /// for at this call, so the closure's unannotated parameters take their
    /// types from them before its body is checked, the way a closure handed to
    /// a concrete function takes them from the declared signature.
    pub(super) fn check_call_args(
        &mut self,
        callee: &Expr,
        callee_ty: Ty,
        args: &[Expr],
        expectations: Option<&[Expectation]>,
    ) -> Vec<Ty> {
        let is_closure = |arg: &Expr| matches!(arg.kind, ExprKind::Closure { .. });
        let generic = if args.iter().any(is_closure) {
            self.generic_callee_inputs(callee_ty, args.len())
                .map(|(inputs, arity)| (inputs, arity, None))
                .or_else(|| self.generic_assoc_callee_inputs(callee, args.len()))
                .or_else(|| {
                    self.generic_stdlib_callee_inputs(callee, args)
                        .map(|inputs| (inputs, CATALOG_TYPE_PARAM_SLOTS, None))
                })
        } else {
            None
        };
        let error_ty = self.tcx.error_ty();
        let mut tys = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if generic.is_some() && is_closure(arg) {
                tys.push(error_ty);
                continue;
            }
            let exp = expectations
                .and_then(|exps| exps.get(i).copied())
                .unwrap_or(Expectation::None);
            tys.push(self.check_expr_expecting(arg, exp));
        }
        if let Some((inputs, slots, receiver_def)) = generic {
            let mut bound = vec![None; slots];
            if let (Some(def), Some(receiver)) = (receiver_def, tys.first().copied()) {
                for (slot, ty) in bound.iter_mut().zip(self.receiver_type_args(def, receiver)) {
                    *slot = ty;
                }
            }
            self.check_closures_against_params(&inputs, bound, args, &mut tys);
        }
        tys
    }
}
