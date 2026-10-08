//! Calls: argument checking against signatures, constructors, and generic instantiation.

use super::{
    AstGenericArg, DefId, DeferredStructural, DeferredStructuralKind, Expectation, Expr, ExprKind,
    FnSig, IntTy, LiteralConsts, Mutbl, REVERSE_DEF_LOCAL, Resolution, Span, StrArgShape,
    StringParamMeta, Ty, TyKind, TypeChecker, TypeError, TypePath, UnaryOp, argument_value_display,
    builtin_type_constructors, callee_display_name, combinator_module_name,
    core_type_own_method_names, is_catalog_type_param, is_definitely_not_callable_value,
    string_argument_found_type, strings_fn_arity, strings_fn_char_params, strings_fn_int_params,
    strings_fn_param_metadata, strings_fn_str_params,
};

impl TypeChecker<'_> {
    /// The parameter types of a generic stdlib free function taking `args`, in
    /// the order the arguments arrive, with each catalogue type parameter left
    /// as a `Param` the other arguments bind. `None` when the function
    /// declares none, or a closure argument's slot is not one the catalogue
    /// can spell.
    pub(super) fn generic_stdlib_callee_inputs(
        &mut self,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Vec<Ty>> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let names = self.resolved_value_path_names(callee.id, path);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (module, last) = names.split_at(names.len().saturating_sub(1));
        let name = last.first().copied()?;
        let shape = crate::stdlib_signatures::internal_shape_for_path(module, name)?;
        if shape.params.len() != args.len()
            || !shape.params.iter().any(|param| {
                crate::stdlib_signatures::split_type_words(param.ty).any(is_catalog_type_param)
            })
        {
            return None;
        }
        let prior = std::mem::replace(&mut self.catalog_params_as_params, true);
        let templates: Vec<Option<Ty>> = shape
            .params
            .iter()
            .map(|param| self.stdlib_signature_ty(param.ty))
            .collect();
        self.catalog_params_as_params = prior;
        let error = self.tcx.error_ty();
        let mut inputs = Vec::with_capacity(args.len());
        for (template, arg) in templates.into_iter().zip(args) {
            let closure = matches!(arg.kind, ExprKind::Closure { .. });
            match template {
                // A bare `T` says nothing of a callable's shape; the slot's own
                // expectation (a middleware's handler) is what types it.
                Some(ty) if closure && matches!(self.tcx.kind(ty), Some(TyKind::Param { .. })) => {
                    return None;
                }
                Some(ty) => inputs.push(ty),
                None if closure => return None,
                None => inputs.push(error),
            }
        }
        Some(inputs)
    }

    /// The type arguments of `ty` by position when it is an instantiation of
    /// the user type `def` (through references), `None` at a const position.
    /// Empty for any other type.
    pub(super) fn receiver_type_args(&self, def: DefId, ty: Ty) -> Vec<Option<Ty>> {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def: found, substs }) if *found == def => substs
                .as_slice()
                .iter()
                .map(|arg| match arg {
                    crate::GenericArg::Type(ty) => Some(*ty),
                    crate::GenericArg::Const(_) | crate::GenericArg::ConstParam(_) => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Checks each closure in `args` against its parameter type in
    /// `templates`. `bound` starts with what is already known of each type
    /// parameter position; the other arguments pin more, and each position
    /// still open becomes a fresh variable. `tys` holds the other arguments'
    /// types and receives the closures'.
    pub(super) fn check_closures_against_params(
        &mut self,
        templates: &[Ty],
        mut bound: Vec<Option<Ty>>,
        args: &[Expr],
        tys: &mut [Ty],
    ) {
        let is_closure = |arg: &Expr| matches!(arg.kind, ExprKind::Closure { .. });
        for ((template, arg), ty) in templates.iter().zip(args).zip(tys.iter()) {
            if !is_closure(arg) {
                self.bind_type_params(*template, *ty, &mut bound);
            }
        }
        let substs: Vec<Ty> = bound
            .into_iter()
            .map(|ty| ty.unwrap_or_else(|| self.fresh()))
            .collect();
        for (i, (template, arg)) in templates.iter().zip(args).enumerate() {
            if is_closure(arg) {
                let want = self.subst_params_in_ty(*template, &substs);
                tys[i] = self.check_expr_expecting(arg, Expectation::HasType(want));
            }
        }
    }

    /// One past the highest type parameter position `ty` names.
    pub(super) fn param_slots(&self, ty: Ty) -> usize {
        match self.tcx.kind_of(ty) {
            TyKind::Param { idx, .. } => idx.0 as usize + 1,
            TyKind::Ref { inner, .. }
            | TyKind::Vec(inner)
            | TyKind::Slice(inner)
            | TyKind::Iterator(inner)
            | TyKind::Range(inner)
            | TyKind::Sender(inner)
            | TyKind::Receiver(inner)
            | TyKind::JoinHandle(inner)
            | TyKind::Array { elem: inner, .. } => self.param_slots(*inner),
            TyKind::Tuple(elems) => elems
                .iter()
                .map(|t| self.param_slots(*t))
                .max()
                .unwrap_or(0),
            TyKind::HashMap { key, value, .. } => {
                self.param_slots(*key).max(self.param_slots(*value))
            }
            TyKind::Adt { substs, .. } | TyKind::Alias { substs, .. } => substs
                .types()
                .iter()
                .map(|t| self.param_slots(*t))
                .max()
                .unwrap_or(0),
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => sig
                .inputs
                .iter()
                .map(|t| self.param_slots(*t))
                .max()
                .unwrap_or(0)
                .max(self.param_slots(sig.output)),
            _ => 0,
        }
    }

    /// A user method's declared non-receiver parameter types for a call with
    /// `n_args` arguments on `receiver_ty`: the receiver's type arguments
    /// substituted, and the method's own type parameters left in place.
    pub(super) fn user_method_param_templates(
        &mut self,
        receiver_ty: Ty,
        method: &str,
        n_args: usize,
    ) -> Option<Vec<Ty>> {
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let params = self.user_method_params_for(resolved, method).or_else(|| {
            let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
                return None;
            };
            let name = self.tcx.def_name(*def)?;
            self.own_generic_method_sigs
                .get(&(name.to_string(), method.to_string(), n_args))
                .map(|sig| sig.params.clone())
        })?;
        (params.len() == n_args).then_some(params)
    }

    /// The declared parameter types of a generic function callee that takes
    /// `n_args` arguments, with how many generic parameters it declares.
    pub(super) fn generic_callee_inputs(
        &self,
        callee_ty: Ty,
        n_args: usize,
    ) -> Option<(Vec<Ty>, usize)> {
        let resolved = self.infer.resolve(self.tcx, callee_ty);
        let Some(TyKind::FnDef { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let arity = self.fn_generic_arity.get(def).copied()?;
        let sig = self.fn_sigs.get(def)?;
        (sig.inputs.len() == n_args).then(|| (sig.inputs.clone(), arity))
    }

    /// The declared parameter types of `Type::function` when `callee` names a
    /// function of a user type's generic `impl` block that takes `n_args`
    /// arguments, with how many type parameter positions they name. A method
    /// called in this form takes its receiver as the first argument, which
    /// has no declared type here and is named by the type's definition.
    pub(super) fn generic_assoc_callee_inputs(
        &mut self,
        callee: &Expr,
        n_args: usize,
    ) -> Option<(Vec<Ty>, usize, Option<DefId>)> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let segments: Vec<&str> = path
            .segments
            .iter()
            .map(|seg| seg.name.name.as_str())
            .collect();
        let [owner @ .., fn_name] = segments.as_slice() else {
            return None;
        };
        if owner.is_empty() {
            return None;
        }
        let type_name = self
            .owner_identity_candidates(owner)
            .into_iter()
            .find(|candidate| self.user_type_decls.contains(candidate))?;
        let def = self.user_type_defs.get(&type_name).copied()?;
        let method = (*fn_name).to_string();
        let params = match self
            .generic_method_param_types
            .get(&(type_name.clone(), method.clone()))
        {
            Some(params) => params.clone(),
            None => [Some(n_args), n_args.checked_sub(1)]
                .into_iter()
                .flatten()
                .find_map(|arity| {
                    self.own_generic_method_sigs
                        .get(&(type_name.clone(), method.clone(), arity))
                })
                .map(|sig| sig.params.clone())?,
        };
        let slots = params
            .iter()
            .map(|param| self.param_slots(*param))
            .max()
            .unwrap_or(0);
        if params.len() == n_args {
            Some((params, slots, None))
        } else if params.len() + 1 == n_args {
            let receiver = self.tcx.error_ty();
            let inputs = std::iter::once(receiver)
                .chain(params.iter().copied())
                .collect();
            Some((inputs, slots, Some(def)))
        } else {
            None
        }
    }

    /// Records, for each type parameter `template` names, the type `actual`
    /// holds in its place, walking the two types together.
    pub(super) fn bind_type_params(&self, template: Ty, actual: Ty, out: &mut [Option<Ty>]) {
        let actual = self.infer.resolve(self.tcx, actual);
        match (self.tcx.kind_of(template), self.tcx.kind_of(actual)) {
            (TyKind::Param { .. }, TyKind::Var(_) | TyKind::Error) => {}
            (TyKind::Param { idx, .. }, _) => {
                if let Some(slot) = out.get_mut(idx.0 as usize) {
                    slot.get_or_insert(actual);
                }
            }
            (TyKind::Ref { inner: t, .. }, TyKind::Ref { inner: a, .. }) => {
                self.bind_type_params(*t, *a, out);
            }
            (TyKind::Ref { inner: t, .. }, _) => self.bind_type_params(*t, actual, out),
            (
                TyKind::Vec(t)
                | TyKind::Slice(t)
                | TyKind::Iterator(t)
                | TyKind::Range(t)
                | TyKind::Sender(t)
                | TyKind::Receiver(t)
                | TyKind::JoinHandle(t),
                TyKind::Vec(a)
                | TyKind::Slice(a)
                | TyKind::Iterator(a)
                | TyKind::Range(a)
                | TyKind::Sender(a)
                | TyKind::Receiver(a)
                | TyKind::JoinHandle(a)
                | TyKind::Array { elem: a, .. },
            )
            | (TyKind::Array { elem: t, .. }, TyKind::Array { elem: a, .. }) => {
                self.bind_type_params(*t, *a, out);
            }
            (TyKind::Tuple(ts), TyKind::Tuple(actuals)) if ts.len() == actuals.len() => {
                for (t, a) in ts.iter().zip(actuals) {
                    self.bind_type_params(*t, *a, out);
                }
            }
            (
                TyKind::HashMap {
                    key: tk, value: tv, ..
                },
                TyKind::HashMap {
                    key: ak, value: av, ..
                },
            ) => {
                self.bind_type_params(*tk, *ak, out);
                self.bind_type_params(*tv, *av, out);
            }
            (
                TyKind::Adt {
                    def: td,
                    substs: ts,
                },
                TyKind::Adt {
                    def: ad,
                    substs: actuals,
                },
            ) if td == ad => {
                for (t, a) in ts.types().iter().zip(actuals.types().iter()) {
                    self.bind_type_params(*t, *a, out);
                }
            }
            (
                TyKind::FnPtr(ts) | TyKind::FnTrait(ts),
                TyKind::FnPtr(actuals) | TyKind::FnTrait(actuals),
            ) if ts.inputs.len() == actuals.inputs.len() => {
                for (t, a) in ts.inputs.iter().zip(&actuals.inputs) {
                    self.bind_type_params(*t, *a, out);
                }
                self.bind_type_params(ts.output, actuals.output, out);
            }
            _ => {}
        }
    }

    pub(super) fn call_arg_expectations(
        &mut self,
        callee: &Expr,
        callee_ty: Ty,
        n_args: usize,
        expected: Expectation,
    ) -> Option<Vec<Expectation>> {
        let resolved = self.infer.resolve(self.tcx, callee_ty);
        // A generic function's parameter types carry rigid `Param` slots;
        // shaping the arguments against them would bind the shared `Param`
        // at the first call and reject every later call with a different
        // concrete type. Leave such arguments unshaped - `check_call_inner`
        // instantiates the signature with fresh variables per call site.
        if let Some(TyKind::FnDef { def, .. }) = self.tcx.kind(resolved)
            && self.fn_generic_arity.contains_key(def)
        {
            return None;
        }
        let sig: Option<FnSig> = match self.tcx.kind(resolved).cloned() {
            Some(TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) => Some(sig),
            Some(TyKind::FnDef { def, .. }) => self.fn_sigs.get(&def).cloned(),
            _ => None,
        };
        if let Some(sig) = sig {
            if sig.inputs.len() == n_args {
                return Some(
                    sig.inputs
                        .iter()
                        .map(|t| Expectation::HasType(*t))
                        .collect(),
                );
            }
            return None;
        }
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let n = path.segments.len();
        if n >= 2 {
            let key = (
                path.segments[n - 2].name.name.clone(),
                path.segments[n - 1].name.name.clone(),
            );
            if let Some(payloads) = self.enum_variant_payloads.get(&key).cloned()
                && payloads.len() == n_args
            {
                // A generic enum's declared payloads carry `Param` slots;
                // this call site's own instantiation is what they shape
                // against.
                let payloads = match self.variant_ctor_instantiation(callee.id, &key.0) {
                    Some((_, substs)) => payloads
                        .iter()
                        .map(|t| self.subst_params_in_ty(*t, &substs))
                        .collect(),
                    None => payloads,
                };
                // Coerce-only: variant payload registration is keyed
                // by (enum, variant) name and a same-named pair from
                // another scope must not unify into this call.
                return Some(payloads.iter().map(|t| Expectation::Coerce(*t)).collect());
            }
        }
        let last = path.segments[n - 1].name.name.as_str();
        if last == "write"
            && n >= 2
            && matches!(path.segments[n - 2].name.name.as_str(), "tar" | "zip")
            && n_args == 1
        {
            let entries = self.archive_entry_vec_ty();
            return Some(vec![Expectation::Coerce(entries)]);
        }
        if n == 1 && n_args == 1 {
            if last == "Reverse"
                && let Some(target) = self.expectation_target(expected)
                && let Some(TyKind::Adt { def, substs }) = self.tcx.kind(target)
                && def.local == REVERSE_DEF_LOCAL
                && let Some(payload) = substs.types().first().copied()
            {
                return Some(vec![expected.rewrap(payload)]);
            }
            // `Some(x)` / `Ok(x)` / `Err(e)`: thread the expected
            // `Option<T>` / `Result<T, E>` payload slot into the
            // argument so `Some([1, 2])` against `Option<Vec<i64>>`
            // lays a heap Vec into the payload.
            let payload_slot = match last {
                "Some" | "Ok" => Some(0),
                "Err" => Some(1),
                _ => None,
            };
            if let Some(slot) = payload_slot
                && let Some(target) = self.expectation_target(expected)
                && let Some(TyKind::Adt { def, substs }) = self.tcx.kind(target)
            {
                let name_ok = match last {
                    "Some" => self.tcx.def_name(*def) == Some("Option"),
                    _ => self.tcx.def_name(*def) == Some("Result"),
                };
                if name_ok && let Some(payload) = substs.types().get(slot).copied() {
                    return Some(vec![expected.rewrap(payload)]);
                }
            }
        }
        self.stdlib_signature_arg_expectations(callee.id, path, n_args)
    }

    /// Rejects `json::render` / `json::encode` of an enum value
    /// (`Result` / `Option` / user enum). The encoder is polymorphic
    /// over `json::Value`, scalars, arrays, and structs, but an enum
    /// has no JSON form and is almost always a `json::parse(..)` whose
    /// `?` was forgotten. The VM tolerated the misuse (emitting the
    /// `Ok` payload) while a native build silently emitted `""`, so the
    /// checker rejects it uniformly with a `?`-pointing diagnostic.
    pub(super) fn reject_json_enum_arg(
        &mut self,
        op: &str,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        let Some(&first_ty) = arg_tys.first() else {
            return;
        };
        let mut peeled = self.infer.resolve(self.tcx, first_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled).cloned() {
            peeled = self.infer.resolve(self.tcx, inner);
        }
        let adt_def = match self.tcx.kind(peeled) {
            Some(TyKind::Adt { def, .. }) => Some(*def),
            _ => None,
        };
        if let Some(def) = adt_def
            && self.tcx.struct_field_tys(def).is_none()
        {
            let span = args.first().map_or(callee.span, |a| a.span);
            let ty = self.render_public_ty(peeled);
            self.emit(
                TypeError::JsonNotSerializable {
                    op: op.to_string(),
                    ty,
                },
                span,
            );
        }
    }

    /// Substitutes a generic function's signature for one call site:
    /// type parameters become the fresh inference variables in `vars`,
    /// and const parameters are inferred from the array arguments whose
    /// lengths name them. Re-records the callee as a `FnDef` carrying the
    /// resolved substitution so MIR monomorphisation reads the concrete
    /// instantiation.
    pub(super) fn instantiate_generic_sig(
        &mut self,
        callee: &Expr,
        def: gossamer_resolve::DefId,
        vars: &[Ty],
        explicit_substs: &crate::Substs,
        sig: FnSig,
        arg_tys: &[Ty],
    ) -> FnSig {
        let n = vars.len();
        let const_mask = self.fn_generic_const_mask_of(def);
        // Infer each const generic from the array argument whose length
        // names it (`sum_arr([1, 2, 3])` => N = 3), so the substituted
        // `[T; N]` carries the concrete count. Inside another generic body the
        // argument's length may be that body's own const parameter, which the
        // call forwards.
        let mut const_substs: Vec<Option<i128>> = (0..n)
            .map(|i| match explicit_substs.as_slice().get(i) {
                Some(crate::GenericArg::Const(value)) => Some(*value),
                _ => None,
            })
            .collect();
        let mut forwarded: Vec<Option<crate::ParamIdx>> = vec![None; n];
        // An explicit `f::<N>(..)` argument is authoritative: inference fills
        // only the positions the call site left open, so an argument of a
        // different length reports a mismatch against the written `N`.
        for (param, arg_ty) in sig.inputs.iter().zip(arg_tys.iter()) {
            let mut found = Vec::new();
            self.infer_const_args(*param, *arg_ty, &mut found);
            for (idx, len) in found {
                if idx >= n
                    || matches!(
                        explicit_substs.as_slice().get(idx),
                        Some(crate::GenericArg::Const(_))
                    )
                {
                    continue;
                }
                match len {
                    crate::ArrayLen::Concrete(value) => {
                        const_substs[idx].get_or_insert(value as i128);
                    }
                    crate::ArrayLen::Param(forwarded_from) => {
                        forwarded[idx].get_or_insert(forwarded_from);
                    }
                }
            }
        }
        let output = self.subst_generics_in_ty(sig.output, vars, &const_substs);
        let output = self.forward_const_params(output, &forwarded);
        let inputs: Vec<Ty> = sig
            .inputs
            .iter()
            .map(|t| self.subst_generics_in_ty(*t, vars, &const_substs))
            .collect();
        let inputs = inputs
            .into_iter()
            .map(|t| self.forward_const_params(t, &forwarded))
            .collect();
        let new_sig = FnSig { inputs, output };
        let const_tys = self
            .const_generics
            .param_tys
            .get(&def)
            .cloned()
            .unwrap_or_default();
        let mut const_args = Vec::new();
        for i in 0..n {
            if !const_mask.get(i).copied().unwrap_or(false) {
                continue;
            }
            let ty = const_tys
                .get(i)
                .copied()
                .flatten()
                .unwrap_or_else(|| self.tcx.int_ty(crate::IntTy::Usize));
            if let Some(value) = const_substs[i] {
                const_args.push(crate::ConstGenericArg::Value { value, ty });
            } else if let Some(name) = forwarded[i].and_then(|at| self.const_generic_param_name(at))
            {
                const_args.push(crate::ConstGenericArg::Param { name, ty });
            } else {
                self.emit(
                    TypeError::ConstGenericNotInferred {
                        callee: callee_display_name(callee),
                        literal_ty: None,
                        assoc_owner: None,
                    },
                    callee.span,
                );
            }
        }
        if !const_args.is_empty() {
            self.table.insert_const_generic_args(callee.id, const_args);
        }
        // Const positions carry the inferred value; every other position
        // carries its fresh type variable (pinned by argument unification).
        let subst_args: Vec<crate::GenericArg> = (0..n)
            .map(|i| {
                if const_mask.get(i).copied().unwrap_or(false) {
                    crate::GenericArg::Const(const_substs[i].unwrap_or(0))
                } else {
                    crate::GenericArg::Type(vars[i])
                }
            })
            .collect();
        let fndef = self.tcx.intern(TyKind::FnDef {
            def,
            substs: crate::Substs::from_args(subst_args),
        });
        self.record(callee.id, fndef);
        new_sig
    }

    #[allow(
        clippy::cognitive_complexity,
        clippy::too_many_lines,
        reason = "sequential callee-shape dispatch: signature, variant constructor, then stdlib fallbacks"
    )]
    /// Return type for a qualified stdlib path call (`Vec::from`,
    /// `strings::parse`, `String::slice`, …). These have no `FnSig` to unify
    /// against, so each family validates its own argument slots and reports
    /// the type it produces. `None` leaves the call to the generic fallback.
    pub(super) fn check_qualified_path_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        expected: Expectation,
        resolved: Ty,
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let names = self.resolved_value_path_names(callee.id, path);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (module, last) = names.split_at(names.len().saturating_sub(1));
        let Some(last) = last.first().copied() else {
            return Some(self.fresh());
        };
        if matches!(module, ["Vec"] | ["std", "Vec"])
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) = self.check_qualified_vec_call(last, args, arg_tys, callee.span)
        {
            return Some(ret);
        }
        if matches!(module, ["String"] | ["std", "String"])
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) = self.check_qualified_string_call(last, args, arg_tys, callee.span)
        {
            return Some(ret);
        }
        if !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) =
                self.check_qualified_bytes_handle_call(module, last, args, arg_tys, callee.span)
        {
            return Some(ret);
        }
        let is_strings_call = matches!(module, ["strings"] | ["std", "strings"]);
        let has_specialized_combinator_sig = combinator_module_name(module)
            .is_some_and(|m| Self::std_combinator_arity(m, last).is_some());
        if !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. })) {
            self.check_stdlib_signature_arity(
                module,
                last,
                args.len(),
                usize::from(self.pipe_stage_callees.contains(&callee.id)),
                callee.span,
            );
            // `strings::*` has a dedicated validator below. Running the
            // generic signature catalogue too reports the same bad slot
            // twice, once without its parameter name.
            if !is_strings_call && !has_specialized_combinator_sig {
                self.check_stdlib_signature_args(module, last, args, arg_tys);
            }
        }
        // `strings::` free functions have no `FnSig` to unify
        // against, so validate their string-typed argument slots
        // here. Skipped when the callee resolves to a user `FnDef`
        // (a user module named `strings` keeps its own typing) or
        // when the value is piped in (`|>` appends the data argument
        // during lowering, shifting the positions this table keys
        // on).
        if is_strings_call
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && !self.pipe_stage_callees.contains(&callee.id)
        {
            self.check_strings_free_call_args(last, args, arg_tys, callee.span);
        }
        if is_strings_call
            && last == "parse"
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
        {
            // The free form is the same operation as the method, so it takes
            // the same one parse surface.
            let suggestion = self.string_parse_replacement(expected);
            self.emit(TypeError::StringParseRetired { suggestion }, callee.span);
            let generics = path
                .segments
                .last()
                .map_or(&[][..], |segment| segment.generics.as_slice());
            return Some(self.string_parse_ret("strings::parse", generics, expected, callee.span));
        }
        if matches!(module, ["String"] | ["std", "String"])
            && last == "slice"
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
        {
            let s = self.tcx.string_ty();
            let err = self.tcx.dyn_error_ty();
            return Some(self.result_adt_ty(s, err));
        }
        if matches!(module, ["String"] | ["std", "String"])
            && matches!(last, "from" | "new" | "with_capacity")
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
        {
            return Some(self.tcx.string_ty());
        }
        if module.is_empty()
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) = self.raw_stdlib_helper_ret(last)
        {
            return Some(ret);
        }
        // Data-last std combinators (`result::map_err(f, r)`,
        // `iter::map(f, xs)`, ...): the signature table pins
        // closure params to the data payload type. Gated on the
        // callee not being a resolved user `FnDef` so a user
        // module that happens to be named `iter` / `result` /
        // `option` keeps its own typing.
        if !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(ret) = self.check_std_combinator_free_call(
                callee,
                args,
                arg_tys,
                combinator_module_name(module),
                last,
            )
        {
            return Some(ret);
        }
        // `archive::tar::write` / `archive::zip::write` take
        // `[(String, [u8])]` and return `Result<[u8], Error>`.
        // These are stdlib (no `fn_sig`), so re-type the literal
        // argument against the synthesized parameter type so a
        // `[("a", [1, 2, 3])]` literal builds heap Vecs at every
        // level on the compiled tier.
        if last == "write"
            && matches!(module.last().copied(), Some("tar" | "zip"))
            && args.len() == 1
        {
            // The `[(String, [u8])]` argument shape flowed in via
            // `call_arg_expectations`; only the return type is
            // synthesized here.
            let u8_ty = self.tcx.int_ty(IntTy::U8);
            let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
            let e = self.tcx.dyn_error_ty();
            return Some(self.result_adt_ty(vec_u8, e));
        }
        let user_callee = matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }));
        if !user_callee && let Some(item) = self.external_binding_item(module, last) {
            let callee_name = if module.is_empty() {
                last.to_string()
            } else {
                format!("{}::{last}", module.join("::"))
            };
            for (param, (arg_ty, arg)) in item.params.iter().zip(arg_tys.iter().zip(args)) {
                if matches!(param, gossamer_resolve::BindingType::Callback(..)) {
                    // A named function crosses as a callable value, so the
                    // argument carries the function's signature rather than
                    // the item itself.
                    let arg_ty = match self.fn_item_value_ty(*arg_ty) {
                        Some(callable) => {
                            self.record(arg.id, callable);
                            callable
                        }
                        None => *arg_ty,
                    };
                    self.deferred_binding_callbacks
                        .push((arg_ty, callee_name.clone(), arg.span));
                }
            }
            if let Some(ty) = self.binding_ty(&item.ret) {
                return Some(ty);
            }
        }
        if let Some(ty) = self.check_stdlib_module_ret_ty(
            (module, last, user_callee),
            callee,
            args,
            arg_tys,
            expected,
        ) {
            return Some(ty);
        }
        if let Some(ty) = self.stdlib_signature_return_ty(module, last) {
            return Some(ty);
        }
        // The IEEE-754 reinterpretations are associated functions on a
        // primitive rather than module members, so they carry their
        // contract here: the bit pattern is the unsigned integer of the
        // float's own width, in both directions.
        if let Some(ty) = self.float_bits_assoc_ret(module, last) {
            return Some(ty);
        }
        // `String::from_utf8` is an associated function on a primitive
        // rather than a module member, so it has no catalogue row; pin
        // its `Result` here or `?` sees an unresolved variable.
        if module == ["String"] && last == "from_utf8" {
            let string_ty = self.tcx.string_ty();
            let err = self.tcx.dyn_error_ty();
            return Some(self.result_adt_ty(string_ty, err));
        }
        // Built-in intrinsics emitted by the parser's macro
        // expansion (`format!` only - `println!` / `print!` /
        // `eprintln!` / `eprint!` etc. expand to a call to the
        // outer name with the format-built string as the single
        // argument, and pinning `println` to Unit broke
        // generic-monomorph paths that route through user-named
        // functions called `println`). Pinning `__concat` and
        // `__fmt_prec` to `String` is safe: they're synthetic
        // names the parser injects and no user code can
        // shadow them.
        if module.is_empty()
            && let Some(ty) = self.check_bare_intrinsic_call(last, arg_tys, callee.span)
        {
            return Some(ty);
        }
        // `String::name(..)` on a built-in type names one of its constructors
        // or, written qualified, one of its methods; anything else has no
        // definition on any tier.
        if let [owner] | ["std", owner] = module
            && !matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. }))
            && let Some(constructors) = builtin_type_constructors(owner)
            && let Some(methods) = core_type_own_method_names(owner)
            && !constructors.contains(&last)
            && !methods.contains(&last)
            && !self.user_methods_for_owner(owner).iter().any(|m| m == last)
        {
            let mut declared: Vec<String> = constructors
                .iter()
                .chain(methods.iter())
                .map(|name| (*name).to_string())
                .collect();
            declared.dedup();
            self.emit(
                TypeError::UnknownAssocItem {
                    base: (*owner).to_string(),
                    name: last.to_string(),
                    declared,
                },
                callee.span,
            );
            return Some(self.tcx.error_ty());
        }
        None
    }

    /// Checks one call against the callee's known signature: instantiates a
    /// generic function's rigid parameter slots per call site, unifies each
    /// argument, and reports an arity mismatch. Returns the call's type when
    /// the signature determines it.
    pub(super) fn check_call_against_sig(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        mut sig: FnSig,
        callee_item: Option<(gossamer_resolve::DefId, crate::Substs)>,
    ) -> Option<Ty> {
        // Per-call-site instantiation of a generic function: replace
        // the signature's rigid `Param` slots with one fresh inference
        // variable each, so independent call sites bind the parameters
        // independently (without this, the second call with a different
        // concrete type fails to unify against the first's binding).
        let inst: Option<(gossamer_resolve::DefId, Vec<Ty>, crate::Substs)> =
            callee_item.and_then(|(def, explicit)| {
                let n = self.fn_generic_arity.get(&def).copied()?;
                if n == 0 {
                    return None;
                }
                if !explicit.is_empty() && explicit.len() != n {
                    self.emit(
                        TypeError::CallArityMismatch {
                            callee: format!("{} generic arguments", callee_display_name(callee)),
                            expected: n,
                            found: explicit.len(),
                        },
                        callee.span,
                    );
                }
                let const_mask = self.fn_generic_const_mask_of(def);
                let vars = (0..n)
                    .map(|i| {
                        if const_mask.get(i).copied().unwrap_or(false) {
                            self.fresh()
                        } else {
                            match explicit.as_slice().get(i) {
                                Some(crate::GenericArg::Type(ty)) => *ty,
                                _ => self.fresh(),
                            }
                        }
                    })
                    .collect();
                Some((def, vars, explicit))
            });
        if let Some((def, vars, explicit)) = &inst {
            sig = self.instantiate_generic_sig(callee, *def, vars, explicit, sig, arg_tys);
        }
        let pipe_extra = usize::from(self.pipe_stage_callees.contains(&callee.id));
        let effective = arg_tys.len() + pipe_extra;
        if effective == sig.inputs.len() {
            for (param, (arg_ty, arg_expr)) in sig.inputs.iter().zip(arg_tys.iter().zip(args)) {
                self.check_sig_param_arg(*param, *arg_ty, arg_expr);
            }
            if let Some((def, vars, _)) = &inst {
                self.check_trait_bounds(*def, vars, callee.span);
                self.check_descriptor_arguments(*def, args);
            }
            return Some(sig.output);
        }
        // A known callee signature whose declared arity does not
        // match the call: the VM aborts (`CallArityMismatch` in the
        // MIR verifier) and the native backend silently drops or
        // zero-fills the surplus/missing arguments. Reject it
        // statically so `check` is never looser than the tiers. A
        // call on the right of `|>` receives the piped value as an
        // implicit trailing argument, so count it toward the arity.
        if effective != sig.inputs.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee_display_name(callee),
                    expected: sig.inputs.len(),
                    found: effective,
                },
                callee.span,
            );
        }
        // Fall through to the existing stdlib / fresh handling so a
        // pipe-stage call keeps its current return typing.
        None
    }

    /// Return type for the call shapes that name a user item rather than a
    /// function value: an `impl`'s associated function, a reverse
    /// constructor, a tuple or named struct literal, and an enum variant.
    /// The first shape that recognises the callee decides the type.
    pub(super) fn check_constructor_like_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        self.check_user_assoc_fn_call(callee, args, arg_tys)
            .or_else(|| self.check_reverse_ctor_call(callee, args, arg_tys))
            .or_else(|| self.check_tuple_struct_ctor_call(callee, args, arg_tys))
            .or_else(|| self.check_named_struct_ctor_call(callee, args))
            .or_else(|| self.check_enum_variant_ctor_call(callee, args, arg_tys))
    }

    /// Return type of `Type::assoc(..)` for a user `impl`'s associated
    /// function. Without it the call's result is a fresh variable, so a
    /// `-> Self` constructor produces an untyped value and every method
    /// call on it - including its argument types - goes unchecked.
    pub(super) fn check_user_assoc_fn_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let segments: Vec<&str> = path
            .segments
            .iter()
            .map(|seg| seg.name.name.as_str())
            .collect();
        let [owner @ .., fn_name] = segments.as_slice() else {
            return None;
        };
        if owner.is_empty() {
            return None;
        }
        // A type reached through its module (`lib::Point::new`) is keyed
        // by the identity it registers under, so try the written path
        // before the bare name two modules could share.
        let type_name = self
            .owner_identity_candidates(owner)
            .into_iter()
            .find(|candidate| self.user_type_decls.contains(candidate))?;
        // An associated function carries its own visibility, the same as a
        // method: `Type::helper()` reached from outside the module the
        // `impl` was written in is private unless it says `pub`.
        self.reject_private_method(&type_name, fn_name, callee.span);
        if let Some(ret) = self
            .method_ret_types
            .get(&(type_name.clone(), (*fn_name).to_string(), args.len()))
            .copied()
        {
            return Some(ret);
        }
        if let Some(ret) =
            self.check_generic_assoc_fn_call(callee, path, &type_name, fn_name, args, arg_tys)
        {
            return Some(ret);
        }
        // A method with type parameters of its own on a concrete type,
        // written `Type::method(value, ..)` or as an associated function.
        let method = (*fn_name).to_string();
        let Some((sig, receiver_form)) = [(args.len(), false), (args.len().wrapping_sub(1), true)]
            .into_iter()
            .find_map(|(arity, receiver_form)| {
                self.own_generic_method_sigs
                    .get(&(type_name.clone(), method.clone(), arity))
                    .cloned()
                    .map(|sig| (sig, receiver_form))
            })
        else {
            return self.reject_unknown_struct_assoc_fn(&type_name, &method, callee.span);
        };
        let skip = usize::from(receiver_form);
        let explicit = path
            .segments
            .last()
            .map(|segment| self.turbofish_types(&segment.generics))
            .unwrap_or_default();
        Some(self.instantiate_own_generic_call(
            &sig,
            (&type_name, &method, callee.span),
            (&args[skip..], &arg_tys[skip..]),
            &explicit,
        ))
    }

    /// Reports `Type::name(..)` on a struct no `impl` gives a function of
    /// that name, which would otherwise type as an unknown value and fail
    /// only when the program runs. An enum is left to the variant check.
    pub(super) fn reject_unknown_struct_assoc_fn(
        &mut self,
        type_name: &str,
        fn_name: &str,
        span: Span,
    ) -> Option<Ty> {
        let def = self.user_type_defs.get(type_name).copied()?;
        if self.tcx.enum_variant_tys(def).is_some()
            || self
                .method_arities
                .contains_key(&(type_name.to_string(), fn_name.to_string()))
            || self
                .user_method_owners
                .get(fn_name)
                .is_some_and(|owners| owners.contains(type_name))
        {
            return None;
        }
        let mut declared: Vec<String> = self
            .method_arities
            .keys()
            .filter(|(owner, _)| owner == type_name)
            .map(|(_, name)| name.clone())
            .filter(|name| !name.starts_with("__"))
            .collect();
        declared.sort();
        declared.dedup();
        let base =
            crate::printer::public_type_name(type_name.rsplit("::").next().unwrap_or(type_name))
                .into_owned();
        self.emit(
            TypeError::UnknownAssocItem {
                base,
                name: fn_name.to_string(),
                declared,
            },
            span,
        );
        Some(self.tcx.error_ty())
    }

    /// Return type of `Type::assoc(..)` for an associated function of a
    /// generic `impl<T> Type<T>` block.
    ///
    /// The declared signature names the block's parameters, and no receiver
    /// supplies an instantiation for them. Each call site gives them fresh
    /// variables of its own, which the arguments pin, so `Bag::of(2.5)`
    /// answers `Bag<f64>`. A result typed by the declaration alone would
    /// carry the block's rigid parameters into a caller that has none.
    pub(super) fn check_generic_assoc_fn_call(
        &mut self,
        callee: &Expr,
        path: &gossamer_ast::PathExpr,
        type_name: &str,
        fn_name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let key = (type_name.to_string(), fn_name.to_string());
        // A method reached as `Type::method(value, ..)` carries its receiver
        // as one argument more than the parameters it declares.
        let ret = [Some(args.len()), args.len().checked_sub(1)]
            .into_iter()
            .flatten()
            .find_map(|arity| {
                self.generic_method_ret_types
                    .get(&(key.0.clone(), key.1.clone(), arity))
                    .copied()
            })?;
        let params = self.generic_method_param_types.get(&key).cloned()?;
        let def = self.user_type_defs.get(type_name).copied()?;
        let arity = self.struct_generic_arity.get(&def).copied().unwrap_or(0);
        let const_mask = self.fn_generic_const_mask_of(def);
        // A const position is filled by its own inference; only a type
        // position takes a variable here.
        let placeholder = self.tcx.error_ty();
        // A method's own type parameters take the positions after the
        // block's, and each takes a variable of its own as well.
        let slots = params
            .iter()
            .chain(std::iter::once(&ret))
            .map(|ty| self.param_slots(*ty))
            .max()
            .unwrap_or(0)
            .max(arity);
        // `Type::method(value, ..)` hands the receiver over first, and its
        // type arguments fill the block's positions.
        let receiver_form = params.len() + 1 == arg_tys.len();
        let receiver_args = match arg_tys.first() {
            Some(receiver) if receiver_form => self.receiver_type_args(def, *receiver),
            _ => Vec::new(),
        };
        // `Type::<A>::function(..)` names the block's arguments on the type
        // segment.
        let owner_args: Vec<Option<Ty>> = match path
            .segments
            .len()
            .checked_sub(2)
            .and_then(|i| path.segments.get(i))
        {
            Some(owner) => owner
                .generics
                .clone()
                .iter()
                .map(|arg| match arg {
                    gossamer_ast::GenericArg::Type(t) => Some(self.type_from_ast(t)),
                    gossamer_ast::GenericArg::Const(_) => None,
                })
                .collect(),
            None => Vec::new(),
        };
        let vars: Vec<Ty> = (0..slots)
            .map(|i| {
                if const_mask.get(i).copied().unwrap_or(false) {
                    placeholder
                } else {
                    receiver_args
                        .get(i)
                        .copied()
                        .flatten()
                        .or_else(|| owner_args.get(i).copied().flatten())
                        .unwrap_or_else(|| self.fresh())
                }
            })
            .collect();
        let const_values = self.assoc_call_const_values(path, def, arg_tys.first(), receiver_form);
        // A call that names no value for a const parameter has been reported,
        // and its result has no type to check against what it meets.
        if !receiver_form
            && !self.record_assoc_const_generic_args(callee, def, type_name, fn_name, &const_values)
        {
            return Some(self.tcx.error_ty());
        }
        let const_substs: Vec<Option<i128>> = const_values
            .iter()
            .map(|value| match value {
                Some(crate::GenericArg::Const(value)) => Some(*value),
                _ => None,
            })
            .collect();
        let (args, arg_tys) = if receiver_form {
            (&args[1..], &arg_tys[1..])
        } else {
            (args, arg_tys)
        };
        if params.len() == arg_tys.len() {
            for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
                let param = self.subst_params_in_ty(*param, &vars);
                let param = self.subst_generics_in_ty(param, &[], &const_substs);
                self.check_sig_param_arg(param, *arg_ty, arg);
            }
        }
        let ret = self.subst_params_in_ty(ret, &vars);
        Some(self.subst_generics_in_ty(ret, &[], &const_substs))
    }

    /// The value each const position of a generic `impl` block takes at an
    /// associated call: from the turbofish on the type the call names
    /// (`Ring::<3>::blank()`), or from the receiver's own type when the call
    /// hands one over (`Ring::snapshot(r)`).
    pub(super) fn assoc_call_const_values(
        &mut self,
        path: &gossamer_ast::PathExpr,
        def: gossamer_resolve::DefId,
        receiver: Option<&Ty>,
        receiver_form: bool,
    ) -> Vec<Option<crate::GenericArg>> {
        let mask = self.fn_generic_const_mask_of(def);
        if receiver_form {
            let resolved =
                receiver.map(|receiver| self.peel_refs(self.infer.resolve(self.tcx, *receiver)));
            let substs = match resolved.and_then(|ty| self.tcx.kind(ty).cloned()) {
                Some(TyKind::Adt { substs, .. }) => substs.as_slice().to_vec(),
                _ => Vec::new(),
            };
            return mask
                .iter()
                .enumerate()
                .map(|(position, is_const)| {
                    substs
                        .get(position)
                        .filter(|_| *is_const)
                        .filter(|arg| {
                            matches!(
                                arg,
                                crate::GenericArg::Const(_) | crate::GenericArg::ConstParam(_)
                            )
                        })
                        .cloned()
                })
                .collect();
        }
        let owner_generics = match path.segments.as_slice() {
            [.., owner, _] => owner.generics.clone(),
            _ => Vec::new(),
        };
        mask.iter()
            .enumerate()
            .map(|(position, is_const)| {
                if !*is_const {
                    return None;
                }
                match owner_generics.get(position)? {
                    AstGenericArg::Const(expr) => Some(crate::GenericArg::Const(
                        self.evaluate_generic_const_arg(expr),
                    )),
                    AstGenericArg::Type(ast_ty) => self
                        .const_generic_type_arg(ast_ty)
                        .map(crate::GenericArg::ConstParam),
                }
            })
            .collect()
    }

    /// Records the values an associated call with no receiver hands its
    /// `impl` block's const parameters, which the function receives as
    /// trailing parameters. Answers `false` after reporting a parameter
    /// nothing names.
    pub(super) fn record_assoc_const_generic_args(
        &mut self,
        callee: &Expr,
        def: gossamer_resolve::DefId,
        type_name: &str,
        fn_name: &str,
        values: &[Option<crate::GenericArg>],
    ) -> bool {
        let Some(owner) = self.tcx.def_name(def).map(ToString::to_string) else {
            return true;
        };
        let Some(params) = self
            .const_generics
            .impl_method_params
            .get(&(owner, fn_name.to_string()))
            .cloned()
        else {
            return true;
        };
        let mut args = Vec::with_capacity(params.len());
        for (position, ty) in params {
            match values.get(position).cloned().flatten() {
                Some(crate::GenericArg::Const(value)) => {
                    args.push(crate::ConstGenericArg::Value { value, ty });
                }
                Some(crate::GenericArg::ConstParam(idx)) => {
                    let Some(name) = self.const_generic_param_name(idx) else {
                        return true;
                    };
                    args.push(crate::ConstGenericArg::Param { name, ty });
                }
                _ => {
                    self.emit(
                        TypeError::ConstGenericNotInferred {
                            callee: fn_name.to_string(),
                            literal_ty: None,
                            assoc_owner: Some(type_name.to_string()),
                        },
                        callee.span,
                    );
                    return false;
                }
            }
        }
        if !args.is_empty() {
            self.table.insert_const_generic_args(callee.id, args);
        }
        true
    }

    pub(super) fn check_call_inner(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        callee_ty: Ty,
        arg_tys: &[Ty],
        expected: Expectation,
    ) -> Ty {
        let resolved = self.infer.resolve(self.tcx, callee_ty);
        let kind = self.tcx.kind(resolved).cloned();
        // Recognised callee shapes: `FnPtr` (anonymous or first-class
        // closure pointer) and `FnDef { def, .. }` (named function
        // resolved to a definition). Looking the def up in
        // `fn_sigs` lets cross-function call sites pin both args and
        // return type to the callee's signature instead of returning
        // a fresh inference variable that never gets bound.
        let callee_item = match self.tcx.kind(resolved) {
            Some(TyKind::FnDef { def, substs }) => Some((*def, substs.clone())),
            _ => None,
        };
        let sig_lookup: Option<FnSig> = match kind {
            Some(TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) => Some(sig),
            Some(TyKind::FnDef { def, .. }) => self.fn_sigs.get(&def).cloned(),
            _ => None,
        };
        if let Some(sig) = sig_lookup
            && let Some(ty) = self.check_call_against_sig(callee, args, arg_tys, sig, callee_item)
        {
            return ty;
        }
        if let Some(ty) = self.check_constructor_like_call(callee, args, arg_tys) {
            return ty;
        }
        // Fallback: known stdlib free functions whose signatures are
        // not present in `fn_sigs` (because they live outside user
        // source). Returning a real type instead of a fresh variable
        // lets the type checker catch mismatches such as returning
        // `Result<json::Value, String>` from a function declared
        // `Result<ComicResponse, String>`.
        if let Some(ty) = self.check_qualified_path_call(callee, args, arg_tys, expected, resolved)
        {
            return ty;
        }
        if let Some(ty) = self.reject_unknown_stdlib_type_member(callee, arg_tys, resolved) {
            return ty;
        }
        self.reject_noncallable_callee(callee, callee_ty);
        self.fresh()
    }

    /// Reports `module::Type::member(..)` naming a member the stdlib type
    /// does not answer. The runtime binds no such item, and the call is not a
    /// method written qualified, which hands over a value of the type first,
    /// so no tier has a definition for it.
    pub(super) fn reject_unknown_stdlib_type_member(
        &mut self,
        callee: &Expr,
        arg_tys: &[Ty],
        resolved: Ty,
    ) -> Option<Ty> {
        if matches!(self.tcx.kind(resolved), Some(TyKind::FnDef { .. })) {
            return None;
        }
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let names = self.resolved_value_path_names(callee.id, path);
        let [.., module, ty, member] = names.as_slice() else {
            return None;
        };
        if !ty.starts_with(|c: char| c.is_ascii_uppercase())
            || gossamer_resolve::STDLIB_MODULES
                .binary_search(&module.as_str())
                .is_err()
            || !gossamer_resolve::is_stdlib_type_path(module, ty)
            || gossamer_resolve::is_stdlib_item_path(&format!("{module}::{ty}::{member}"))
        {
            return None;
        }
        if let Some(first) = arg_tys.first() {
            let first = self.infer.resolve(self.tcx, *first);
            let rendered = crate::printer::render_public_ty(self.tcx, first);
            let names_the_type = rendered == *ty || rendered.ends_with(&format!("::{ty}"));
            // A numeric literal is never a stdlib handle, so only an unknown
            // type leaves the call possibly a qualified method.
            let unknown = match self.tcx.kind(first) {
                Some(TyKind::Var(vid)) => {
                    let vid = *vid;
                    !self.infer.is_unresolved_integer_var(vid)
                        && !self.infer.is_unresolved_float_var(vid)
                }
                None => true,
                Some(_) => false,
            };
            if names_the_type || unknown {
                return None;
            }
        }
        let declared = gossamer_resolve::stdlib_type_member_names(module, ty)
            .into_iter()
            .map(ToString::to_string)
            .collect();
        self.emit(
            TypeError::UnknownAssocItem {
                base: format!("{module}::{ty}"),
                name: member.clone(),
                declared,
            },
            callee.span,
        );
        Some(self.tcx.error_ty())
    }

    /// Validates Rust-style associated String mutators such as
    /// `String::push(&mut s, ch)`. These calls do not pass through method-call
    /// checking and are not stdlib module functions, so without this table a
    /// malformed argument can reach a permissive runtime builtin and become a
    /// silent no-op.
    pub(super) fn check_qualified_string_call(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let string = self.tcx.string_ty();
        let receiver = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner: string,
        });
        let params = match method {
            "clear" => vec![receiver],
            "push" | "push_char" => {
                vec![receiver, self.tcx.intern(TyKind::Char)]
            }
            "push_str" => vec![receiver, string],
            "push_byte" | "truncate" => {
                vec![receiver, self.tcx.int_ty(IntTy::I64)]
            }
            "push_utf8" | "push_json_quoted" => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                let bytes = self.tcx.intern(TyKind::Vec(u8_ty));
                let idx = self.tcx.int_ty(IntTy::I64);
                vec![receiver, bytes, idx, idx]
            }
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("String::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(self.tcx.unit())
    }

    /// Qualified Vec counterpart of method-call checking. Rust-style UFCS
    /// calls bypass `vec_method_ret`, so validate the complete receiver and
    /// argument contract here instead of allowing runtime builtins to accept
    /// mixed element types.
    pub(super) fn check_qualified_vec_call(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let actual_receiver = arg_tys.first().copied();
        let elem = actual_receiver
            .map(|ty| self.infer.resolve(self.tcx, ty))
            .map(|ty| self.peel_refs(ty))
            .and_then(|ty| match self.tcx.kind(ty) {
                Some(TyKind::Vec(elem)) => Some(*elem),
                _ => None,
            })
            .unwrap_or_else(|| self.fresh());
        let vec_ty = self.tcx.intern(TyKind::Vec(elem));
        let shared = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Not,
            inner: vec_ty,
        });
        let mutable = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner: vec_ty,
        });
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let error_ty = self.tcx.dyn_error_ty();
        let unit_ty = self.tcx.unit();
        let vec_result_ty = self.tcx.intern(TyKind::Vec(elem));
        let (params, ret) = match method {
            "push" => (vec![mutable, elem], unit_ty),
            "insert" => (
                vec![mutable, i64_ty, elem],
                self.result_adt_ty(unit_ty, error_ty),
            ),
            "remove" => (vec![mutable, i64_ty], self.result_adt_ty(elem, error_ty)),
            "sort" | "reverse" => (vec![mutable], self.tcx.unit()),
            "fill" => (vec![mutable, elem], self.tcx.unit()),
            "swap" => (vec![mutable, i64_ty, i64_ty], self.tcx.unit()),
            "slice" => (
                vec![shared, i64_ty, i64_ty],
                self.result_adt_ty(vec_result_ty, error_ty),
            ),
            "first" | "last" => (vec![shared], self.option_adt_ty(elem)),
            "rev" => (vec![shared], self.tcx.intern(TyKind::Vec(elem))),
            "index_of" => (vec![shared, elem], self.option_adt_ty(i64_ty)),
            "count_of" => (vec![shared, elem], i64_ty),
            "contains" => (vec![shared, elem], self.tcx.bool_ty()),
            "len" => (vec![shared], i64_ty),
            // Closure typing is handled by the combinator path for method
            // syntax. Still enforce the qualified form's receiver and arity.
            "sort_by" => (vec![mutable, self.fresh()], self.tcx.unit()),
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("Vec::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    pub(super) fn check_qualified_bytes_handle_call(
        &mut self,
        module: &[&str],
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let owner = match module {
            ["Buffer"] | ["bytes", "Buffer"] | ["std", "bytes", "Buffer"] => "bytes::Buffer",
            ["Builder"] | ["bytes", "Builder"] | ["std", "bytes", "Builder"] => "bytes::Builder",
            _ => return None,
        };
        let handle = self.bytes_handle_ty(owner);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let string = self.tcx.string_ty();
        let mutable = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner: handle,
        });
        let shared = self.tcx.intern(TyKind::Ref {
            mutability: Mutbl::Not,
            inner: handle,
        });
        let (params, ret) = match (owner, method) {
            (_, "new") => (vec![], handle),
            (_, "with_capacity") => (vec![i64_ty], handle),
            ("bytes::Buffer", "push") => (vec![mutable, u8_ty], self.tcx.unit()),
            ("bytes::Buffer", "write_str") => (vec![mutable, string], self.tcx.unit()),
            ("bytes::Buffer", "clear") => (vec![mutable], self.tcx.unit()),
            ("bytes::Buffer", "len") => (vec![shared], i64_ty),
            ("bytes::Buffer", "is_empty") => (vec![shared], self.tcx.bool_ty()),
            ("bytes::Buffer", "to_string") => (vec![shared], string),
            ("bytes::Builder", "write") => (vec![mutable, string], self.tcx.unit()),
            ("bytes::Builder", "write_char") => (
                vec![mutable, self.tcx.intern(TyKind::Char)],
                self.tcx.unit(),
            ),
            ("bytes::Builder", "len") => (vec![shared], i64_ty),
            ("bytes::Builder", "build" | "as_str") => (vec![shared], string),
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    pub(super) fn check_tuple_struct_ctor_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let Some(Resolution::Def {
            def,
            kind: gossamer_resolve::DefKind::Struct,
        }) = self.resolutions.get(callee.id)
        else {
            return None;
        };
        if !self.tcx.is_tuple_struct(def.local) {
            return None;
        }
        let called_name = path.segments.last()?.name.name.as_str();
        // A type's registered name carries the modules containing it, so
        // compare the written leaf against the identity's leaf.
        let identity = self.tcx.def_name(def)?;
        if identity.rsplit("::").next() != Some(called_name) {
            return None;
        }
        let fields = self.struct_fields.get(&def)?.clone();
        let arity = self.struct_generic_arity.get(&def).copied().unwrap_or(0);
        let substs: Vec<Ty> = (0..arity).map(|_| self.fresh()).collect();
        self.defer_adt_bounds(def, &substs, callee.span);
        if fields.len() == arg_tys.len() {
            for ((_, field_ty), (arg_ty, arg_expr)) in fields.iter().zip(arg_tys.iter().zip(args)) {
                let field_ty = self.subst_params_in_ty(*field_ty, &substs);
                self.check_sig_param_arg(field_ty, *arg_ty, arg_expr);
            }
        } else {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee_display_name(callee),
                    expected: fields.len(),
                    found: arg_tys.len(),
                },
                callee.span,
            );
        }
        Some(self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::from_types(substs.iter().copied()),
        }))
    }

    pub(super) fn check_named_struct_ctor_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let Some(Resolution::Def {
            def,
            kind: gossamer_resolve::DefKind::Struct,
        }) = self.resolutions.get(callee.id)
        else {
            return None;
        };
        if self.tcx.is_tuple_struct(def.local) {
            return None;
        }
        let called_name = path.segments.last()?.name.name.as_str();
        // A type's registered name carries the modules containing it, so
        // compare the written leaf against the identity's leaf.
        if self.tcx.def_name(def)?.rsplit("::").next() != Some(called_name) {
            return None;
        }
        let arity = self.struct_generic_arity.get(&def).copied().unwrap_or(0);
        let substs: Vec<Ty> = (0..arity).map(|_| self.fresh()).collect();
        let name = self
            .tcx
            .def_name(def)
            .map_or_else(|| called_name.to_string(), ToString::to_string);
        self.emit(
            TypeError::StructConstructorBracesRequired { name },
            callee.span,
        );
        for arg in args {
            self.check_expr(arg);
        }
        Some(self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::from_types(substs.iter().copied()),
        }))
    }

    pub(super) fn check_reverse_ctor_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        if path.segments.len() != 1 || path.segments[0].name.name != "Reverse" {
            return None;
        }
        if args.len() != 1 {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: "Reverse".to_string(),
                    expected: 1,
                    found: args.len(),
                },
                callee.span,
            );
            return Some(self.tcx.error_ty());
        }
        let elem = arg_tys.first().copied().unwrap_or_else(|| self.fresh());
        Some(self.reverse_ty(elem))
    }

    /// Validates a user enum's tuple-variant constructor. Payload expectations
    /// shape collection literals before this point, but shaping alone is not a
    /// type check. Every supplied payload must still unify with its declared
    /// slot, and the constructor's result remains the nominal enum type.
    pub(super) fn check_enum_variant_ctor_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let n = path.segments.len();
        if n < 2 {
            return None;
        }
        let enum_name = path.segments[n - 2].name.name.clone();
        let variant_name = path.segments[n - 1].name.name.clone();
        let payloads = self
            .enum_variant_payloads
            .get(&(enum_name.clone(), variant_name))?
            .clone();
        // A generic enum's parameters are instantiated once per call site;
        // the payload checks below and the type this call produces read the
        // same variables, so an argument pins the enum's own arguments.
        let instantiation = self.variant_ctor_instantiation(callee.id, &enum_name);
        let mut ctor_consts = LiteralConsts::new(
            instantiation
                .as_ref()
                .map(|(def, _)| self.fn_generic_const_mask_of(*def))
                .unwrap_or_default(),
        );
        let payloads: Vec<Ty> = match &instantiation {
            Some((_, substs)) => {
                let declared: Vec<Ty> = payloads
                    .iter()
                    .map(|t| self.subst_params_in_ty(*t, substs))
                    .collect();
                // An array payload whose length is a const parameter takes
                // the length its argument has.
                for (payload, arg_ty) in declared.iter().zip(arg_tys.iter()) {
                    if ctor_consts.is_const_array_field(self.tcx, *payload) {
                        ctor_consts.infer_from_field(self, *payload, *arg_ty);
                    }
                }
                declared
                    .iter()
                    .map(|t| ctor_consts.apply(self, *t, substs))
                    .collect()
            }
            None => payloads,
        };
        if payloads.len() == arg_tys.len() {
            for (param, (arg_ty, arg)) in payloads.iter().zip(arg_tys.iter().zip(args)) {
                self.check_sig_param_arg(*param, *arg_ty, arg);
            }
        } else {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee_display_name(callee),
                    expected: payloads.len(),
                    found: arg_tys.len(),
                },
                callee.span,
            );
        }
        if let Some((def, substs)) = instantiation {
            let ty = self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::from_types(substs.iter().copied()),
            });
            if !ctor_consts.has_const_positions() {
                return Some(ty);
            }
            return Some(self.finish_struct_literal_consts(
                ty,
                &ctor_consts,
                Expectation::None,
                path,
                callee.span,
            ));
        }
        Some(
            self.enum_tys
                .get(&enum_name)
                .copied()
                .unwrap_or_else(|| self.fresh()),
        )
    }

    /// After a call failed every resolution path: if the callee is a
    /// concrete, fully-known value that can never be a function or an ADT
    /// constructor, reject it (`GT0022`) - the compiled tier would emit a
    /// call through a non-function symbol. A qualified path callee
    /// (`String::new`) types loosely as the receiving type and is never
    /// flagged; an inference-var callee (e.g. an unsuffixed `let x = 5`)
    /// is deferred for re-check after defaulting.
    pub(super) fn reject_noncallable_callee(&mut self, callee: &Expr, callee_ty: Ty) {
        let resolved_callee = self.infer.resolve(self.tcx, callee_ty);
        let callee_kind = self.tcx.kind_of(resolved_callee).clone();
        let qualified_path_callee =
            matches!(&callee.kind, ExprKind::Path(p) if p.segments.len() >= 2);
        if matches!(callee_kind, TyKind::Var(_)) {
            self.deferred_structural.push(DeferredStructural {
                ty: resolved_callee,
                span: callee.span,
                kind: DeferredStructuralKind::Call,
                result: None,
            });
        } else if is_definitely_not_callable_value(&callee_kind) && !qualified_path_callee {
            let ty = self.render_public_ty(resolved_callee);
            self.emit(TypeError::NotCallable { ty }, callee.span);
        }
    }

    /// Concrete return type of a stdlib `json` / `errors` / `fs` / `os`
    /// free call whose signature lives outside user source. Returning a
    /// real type (rather than a fresh inference var) lets the checker
    /// catch mismatches - e.g. matching `fs::file_size(p)`'s bare `i64`
    /// against a `Result` pattern. The json accessors return `Option<T>`
    /// at runtime (the interp emits `Some`/`None`), so they are typed as
    /// such; `json::get(v, k).unwrap()` and the autoderive `Some`/`None`
    /// matches both rely on it.
    /// Return type of a qualified `HashMap::pop(m, k)` / `HashMap::get(m, k)`
    /// free-fn call. The method form (`m.pop(k)`) is typed by
    /// `check_method_call` from the receiver's static type; the qualified form
    /// must recover the value type from the first argument's map type so the
    /// `Option<V>` payload binding is the concrete value rather than an
    /// unresolved var - a struct field read on an unresolved payload lowers to
    /// the dynamic json accessor and faults at runtime.
    pub(super) fn check_qualified_map_accessor_ret(
        &mut self,
        module: &[&str],
        last: &str,
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        if !matches!(
            module,
            ["Map"] | ["collections", "Map"] | ["std", "collections", "Map"]
        ) || !matches!(last, "pop" | "get" | "insert" | "remove")
        {
            return None;
        }
        let value = arg_tys.first().and_then(|t| {
            let resolved = self.infer.resolve(self.tcx, *t);
            let peeled = match self.tcx.kind(resolved) {
                Some(TyKind::Ref { inner, .. }) => self.infer.resolve(self.tcx, *inner),
                _ => resolved,
            };
            match self.tcx.kind(peeled) {
                Some(TyKind::HashMap { value, .. }) => Some(*value),
                _ => None,
            }
        });
        value.map(|v| self.option_adt_ty(v))
    }

    /// Rejects a non-string argument in a string-typed parameter slot
    /// of a `strings::` free-function call. The stdlib free path has no
    /// `FnSig` to unify against, so without this the checker accepts an
    /// integer where a `String` is expected and the compiled string
    /// shims dereference it as a pointer (a SIGSEGV the VM masks).
    pub(super) fn check_strings_free_call_args(
        &mut self,
        name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        _callee_span: Span,
    ) {
        let Some(shapes) = strings_fn_str_params(name) else {
            return;
        };
        for &(idx, shape) in shapes {
            let (Some(arg), Some(&arg_ty)) = (args.get(idx), arg_tys.get(idx)) else {
                continue;
            };
            let meta = strings_fn_param_metadata(name, idx, shape);
            self.check_str_param_arg(
                shape,
                arg,
                arg_ty,
                arg.span,
                &format!("strings::{name}"),
                meta,
            );
        }
        self.check_strings_int_args(name, args, arg_tys, 0);
        self.check_strings_char_args(name, args, arg_tys, 0);
    }

    /// Unifies one call argument against its declared parameter type.
    /// Shared references retain the language's read-only convenience
    /// coercion, but `&mut T` is never created implicitly: mutation must
    /// be visible as `&mut place` or arrive through an existing `&mut T`.
    pub(super) fn check_sig_param_arg(&mut self, param: Ty, arg_ty: Ty, arg: &Expr) {
        let param = self.infer.resolve(self.tcx, param);
        let arg_ty = self.infer.resolve(self.tcx, arg_ty);
        let param_ref = match self.tcx.kind(param) {
            Some(TyKind::Ref { inner, mutability }) => Some((*inner, *mutability)),
            _ => None,
        };
        let arg_ref = match self.tcx.kind(arg_ty) {
            Some(TyKind::Ref { inner, mutability }) => Some((*inner, *mutability)),
            _ => None,
        };
        let (lhs, rhs) = match (param_ref, arg_ref) {
            (Some((_p, Mutbl::Mut)), Some((_a, Mutbl::Mut))) => (param, arg_ty),
            (Some((_p, Mutbl::Mut)), Some((a, Mutbl::Not))) => (
                param,
                self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Not,
                    inner: a,
                }),
            ),
            (Some((p, Mutbl::Mut)), None) => {
                // An argument whose own type already failed carries no
                // evidence about how it was passed; a second report here
                // would describe the wrong thing.
                if !matches!(self.tcx.kind(arg_ty), Some(TyKind::Error)) {
                    self.emit(
                        TypeError::MutableArgumentRequiresReference {
                            argument: Self::place_display(arg),
                        },
                        arg.span,
                    );
                }
                (p, arg_ty)
            }
            // A `[T]` view parameter keeps its reference here: the sequence
            // reaching it is an array, a `Vec`, or another view, and the
            // unsizing that accepts all three is decided against the view.
            (Some((p, Mutbl::Not)), None) => {
                if matches!(self.tcx.kind(p), Some(TyKind::Slice(_))) {
                    (param, arg_ty)
                } else {
                    (p, arg_ty)
                }
            }
            // A reference reaching a value parameter is the value it names,
            // but nothing at the call says so: the compiled tiers hand the
            // callee the address while the bytecode VM hands it the value.
            // The deref is written.
            (None, Some((a, _))) => {
                if !matches!(
                    self.tcx.kind(param),
                    Some(TyKind::Error | TyKind::Var(_) | TyKind::Param { .. })
                ) && !matches!(self.tcx.kind(arg_ty), Some(TyKind::Error))
                    && !matches!(
                        arg.kind,
                        ExprKind::Unary {
                            op: UnaryOp::RefMut,
                            ..
                        }
                    )
                {
                    self.emit(
                        TypeError::ReferenceArgumentNeedsDeref {
                            argument: Self::place_display(arg),
                        },
                        arg.span,
                    );
                }
                (param, a)
            }
            _ => (param, arg_ty),
        };
        // Render an unsuffixed float literal as its default source type in a
        // String slot. The unifier also rejects the constraint, but this
        // parameter-specific path identifies the bad argument.
        if matches!(
            self.tcx.kind(self.infer.resolve(self.tcx, lhs)),
            Some(TyKind::String)
        ) && self.infer.is_float_literal_var(self.tcx, rhs)
        {
            self.emit_str_slot_mismatch("f64", arg.span);
        } else {
            self.unify(lhs, rhs, arg.span);
        }
    }

    /// Validates the string-typed arguments of a `String` method call
    /// (`s.contains(x)`). The method dispatches to the same `strings::`
    /// shim as the free function with the receiver as the implicit first
    /// argument, so the explicit args occupy parameter positions 1..
    pub(super) fn check_strings_method_call_args(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        receiver_span: Span,
    ) {
        self.check_strings_arity(method, args.len(), 1, receiver_span);
        let Some(shapes) = strings_fn_str_params(method) else {
            return;
        };
        for &(pos, shape) in shapes {
            // Position 0 is the receiver, already known to be a `String`.
            let Some(idx) = pos.checked_sub(1) else {
                continue;
            };
            let (Some(arg), Some(&arg_ty)) = (args.get(idx), arg_tys.get(idx)) else {
                continue;
            };
            let meta = strings_fn_param_metadata(method, pos, shape);
            self.check_str_param_arg(
                shape,
                arg,
                arg_ty,
                arg.span,
                &format!("String::{method}"),
                meta,
            );
        }
        self.check_strings_int_args(method, args, arg_tys, 1);
        self.check_strings_char_args(method, args, arg_tys, 1);
    }

    /// Validates the complete fixed arity of a known string operation. The
    /// runtime shims historically treated omitted arguments as zero/empty,
    /// which turned a source error such as `s.slice(1)` into a misleading
    /// range operation. Keep this beside the string-slot checks so free and
    /// method syntax share exactly one contract.
    pub(super) fn check_strings_arity(
        &mut self,
        name: &str,
        supplied: usize,
        implicit_receiver: usize,
        span: Span,
    ) {
        let Some(total) = strings_fn_arity(name) else {
            return;
        };
        let expected = total.saturating_sub(implicit_receiver);
        if supplied != expected {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("strings::{name}"),
                    expected,
                    found: supplied,
                },
                span,
            );
        }
    }

    /// Validates integer slots in the same string operation catalogue. Unlike
    /// string slots, these are all specified as `i64`; this catches ranges,
    /// strings, and floats rather than letting runtime helpers coerce them to
    /// zero.
    pub(super) fn check_strings_int_args(
        &mut self,
        name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        implicit_receiver: usize,
    ) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        for &position in strings_fn_int_params(name) {
            let Some(index) = position.checked_sub(implicit_receiver) else {
                continue;
            };
            let (Some(arg), Some(&arg_ty)) = (args.get(index), arg_tys.get(index)) else {
                continue;
            };
            self.check_sig_param_arg(i64_ty, arg_ty, arg);
        }
    }

    /// Validates char-only slots such as `pad_left(text, width, fill)`.
    pub(super) fn check_strings_char_args(
        &mut self,
        name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        implicit_receiver: usize,
    ) {
        let char_ty = self.tcx.char_ty();
        for &position in strings_fn_char_params(name) {
            let Some(index) = position.checked_sub(implicit_receiver) else {
                continue;
            };
            let (Some(arg), Some(&arg_ty)) = (args.get(index), arg_tys.get(index)) else {
                continue;
            };
            self.check_sig_param_arg(char_ty, arg_ty, arg);
        }
    }

    /// Validates one argument against a string-shaped parameter slot.
    pub(super) fn check_str_param_arg(
        &mut self,
        shape: StrArgShape,
        arg: &Expr,
        arg_ty: Ty,
        span: Span,
        callee: &str,
        param: StringParamMeta,
    ) {
        // `&"hi"` (a `Ref<String>`) is layout-transparent to its inner
        // `String` at every call boundary; validate the referent.
        let resolved = self.infer.resolve(self.tcx, arg_ty);
        let inner = match self.tcx.kind(resolved) {
            Some(TyKind::Ref { inner, .. }) => *inner,
            _ => resolved,
        };
        // Catch unsuffixed numeric literals up front in either slot shape so
        // a `5` / `1.5` in a string position is rejected with the same
        // `i64` / `f64` spelling used by every other type diagnostic.
        if self.infer.is_integer_constrained_var(self.tcx, inner) {
            self.emit_named_str_slot_mismatch(callee, param.name, param.expected, "i64", arg, span);
            return;
        }
        if self.infer.is_float_literal_var(self.tcx, inner) {
            self.emit_named_str_slot_mismatch(callee, param.name, param.expected, "f64", arg, span);
            return;
        }
        match shape {
            StrArgShape::Str => {
                // A `String` slot admits only a real string. Keep an
                // unresolved inference variable unifiable for valid generic
                // expressions, but report every concrete wrong shape with
                // the parameter name instead of a context-free mismatch.
                let r = self.infer.resolve(self.tcx, inner);
                if matches!(self.tcx.kind(r), Some(TyKind::String)) {
                    return;
                }
                if matches!(self.tcx.kind(r), Some(TyKind::Var(_))) {
                    let s = self.tcx.string_ty();
                    self.unify(s, inner, span);
                } else if self.tcx.kind(r).is_some() {
                    self.emit_named_str_slot_mismatch(
                        callee,
                        param.name,
                        param.expected,
                        &string_argument_found_type(arg, self.tcx, r),
                        arg,
                        span,
                    );
                } else {
                    let s = self.tcx.string_ty();
                    self.unify(s, inner, span);
                }
            }
            StrArgShape::StrOrChar => {
                // A pattern slot also admits a `char`, so the
                // unifier (single expected type) is too strict. Report every
                // non-string / non-char shape through the named argument path.
                let r = self.infer.resolve(self.tcx, inner);
                if matches!(self.tcx.kind(r), Some(TyKind::Var(_))) {
                    return;
                }
                if !matches!(self.tcx.kind(r), Some(TyKind::String | TyKind::Char)) {
                    self.emit_named_str_slot_mismatch(
                        callee,
                        param.name,
                        param.expected,
                        &string_argument_found_type(arg, self.tcx, r),
                        arg,
                        span,
                    );
                }
            }
        }
    }

    pub(super) fn emit_str_slot_mismatch(&mut self, found: &str, span: Span) {
        self.emit(
            TypeError::TypeMismatch {
                expected: "String".to_string(),
                found: found.to_string(),
            },
            span,
        );
    }

    pub(super) fn emit_named_str_slot_mismatch(
        &mut self,
        callee: &str,
        parameter: &str,
        expected: &str,
        found: &str,
        arg: &Expr,
        span: Span,
    ) {
        self.emit(
            TypeError::ArgumentTypeMismatch {
                callee: callee.to_string(),
                parameter: parameter.to_string(),
                expected: expected.to_string(),
                found: found.to_string(),
                actual: argument_value_display(arg),
            },
            span,
        );
    }

    /// Result type of an `fs::` / `os::` free call, or `None` for the
    /// unlisted surface. Typed reads keep the `?`-unwrapped payload
    /// concrete (`fs::read_to_string(p)?.to_lowercase()` stays `String`
    /// into codegen).
    pub(super) fn fs_call_ret_ty(&mut self, last: &str) -> Option<Ty> {
        match last {
            "file_size" => Some(self.tcx.int_ty(IntTy::I64)),
            "exists" | "is_file" | "is_dir" | "is_symlink" => Some(self.tcx.bool_ty()),
            "read_to_string" | "read_file_to_string" => {
                let s = self.tcx.string_ty();
                let e = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(s, e))
            }
            "read" | "read_file" => {
                let u8_ty = self.tcx.int_ty(IntTy::U8);
                let v = self.tcx.intern(TyKind::Vec(u8_ty));
                let e = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(v, e))
            }
            _ => None,
        }
    }

    /// Result type of a `Collection::new()` constructor call, or `None`
    /// for a non-collection path. An unannotated `let m = HashMap::new()`
    /// grounds to a real `TyKind::HashMap` (generics pinned by the first
    /// `insert` / `get`, see `map_method_ret`) so method dispatch reaches
    /// the properly keyed runtime symbol on every tier; `VecDeque` /
    /// `HashSet` ground to the same sentinel Adts their annotations
    /// resolve to.
    pub(super) fn collection_ctor_ty(&mut self, module: &[&str]) -> Option<Ty> {
        let tail = match module {
            [t] | ["collections", t] | ["std", "collections", t] => *t,
            _ => return None,
        };
        match tail {
            "Vec" => {
                let elem = self.fresh();
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            "Deque" => {
                let elem = self.fresh();
                Some(self.vecdeque_ty(elem))
            }
            "Queue" => {
                let elem = self.fresh();
                Some(self.vecqueue_ty(elem))
            }
            "Stack" => {
                let elem = self.fresh();
                Some(self.vecstack_ty(elem))
            }
            "MaxHeap" => {
                let elem = self.fresh();
                Some(self.binary_heap_ty(elem))
            }
            "MinHeap" => {
                let elem = self.fresh();
                Some(self.min_heap_ty(elem))
            }
            "Map" => {
                let key = self.fresh();
                let value = self.fresh();
                Some(self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: false,
                }))
            }
            "BTreeMap" => {
                let key = self.fresh();
                let value = self.fresh();
                Some(self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: true,
                }))
            }
            "Set" | "BTreeSet" => {
                let elem = self.fresh();
                Some(self.set_ty(tail, elem))
            }
            _ => None,
        }
    }

    pub(super) fn collection_from_ty(
        &mut self,
        module: &[&str],
        source: Ty,
        expected: Expectation,
        span: Span,
    ) -> Option<Ty> {
        let owner = match module {
            [owner] | ["collections", owner] | ["std", "collections", owner] => *owner,
            _ => return None,
        };
        let source = self.infer.resolve(self.tcx, source);
        let array_source_elem = |checker: &mut Self| {
            if let Some(TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem)) =
                checker.tcx.kind(source)
            {
                *elem
            } else {
                let found = checker.render_public_ty(source);
                checker.emit(
                    TypeError::TypeMismatch {
                        expected: "array, slice, or Vec".to_string(),
                        found,
                    },
                    span,
                );
                checker.fresh()
            }
        };
        match owner {
            "Vec" => {
                let elem = array_source_elem(self);
                Some(self.tcx.intern(TyKind::Vec(elem)))
            }
            "Set" | "BTreeSet" => {
                let elem = array_source_elem(self);
                Some(self.set_ty(owner, elem))
            }
            "Deque" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.vecdeque_ty(elem))
            }
            "Queue" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.vecqueue_ty(elem))
            }
            "Stack" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.vecstack_ty(elem))
            }
            "MaxHeap" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.binary_heap_ty(elem))
            }
            "MinHeap" => {
                let elem = array_source_elem(self);
                let elem = self.require_slot_collection_elem(elem, owner, span);
                Some(self.min_heap_ty(elem))
            }
            "Map" | "BTreeMap" => {
                let (key, value) = if let Some(
                    TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem),
                ) = self.tcx.kind(source)
                {
                    match self.tcx.kind(*elem) {
                        Some(TyKind::Tuple(parts)) if parts.len() == 2 => (parts[0], parts[1]),
                        Some(TyKind::Var(_)) => {
                            let target = self.expectation_target(expected)?;
                            match self.tcx.kind(target) {
                                Some(TyKind::HashMap { key, value, .. }) => (*key, *value),
                                _ => return None,
                            }
                        }
                        _ => return None,
                    }
                } else {
                    let found = self.render_public_ty(source);
                    self.emit(
                        TypeError::TypeMismatch {
                            expected: "fixed array of key-value tuples".to_string(),
                            found,
                        },
                        span,
                    );
                    return Some(self.fresh());
                };
                Some(self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: owner == "BTreeMap",
                }))
            }
            _ => None,
        }
    }

    /// `Simd<T, N>` / `Mask<N>` from a written type path.
    pub(super) fn simd_type_from_path(&mut self, head: &str, path: &TypePath, span: Span) -> Ty {
        let substs = self.substs_from_ast(path);
        let args = substs.as_slice();
        let (elem, lanes_arg) = if head == "Mask" {
            (Some(self.tcx.bool_ty()), args.first())
        } else {
            let elem = args.first().and_then(|arg| match arg {
                crate::GenericArg::Type(ty) => Some(*ty),
                _ => None,
            });
            (elem, args.get(1))
        };
        let lanes = match lanes_arg {
            Some(crate::GenericArg::Const(value)) => {
                usize::try_from(*value).ok().map(crate::ArrayLen::Concrete)
            }
            Some(crate::GenericArg::ConstParam(idx)) => Some(crate::ArrayLen::Param(*idx)),
            _ => None,
        };
        let (Some(elem), Some(lanes)) = (elem, lanes) else {
            self.emit(
                TypeError::SimdShape {
                    reason: format!("`{head}` names its lanes, as in `Simd<f64, 4>` or `Mask<4>`"),
                },
                span,
            );
            return self.tcx.error_ty();
        };
        self.checked_simd_ty(elem, lanes, span)
    }
}
