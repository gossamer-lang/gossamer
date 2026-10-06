//! Collecting item signatures and registering declarations.

use super::{
    Expectation, Expr, ExprKind, FnDecl, FnParam, FnSig, HashMap, ImplDecl, ImplItem, Item,
    ItemKind, Mutbl, NodeId, OwnGenericMethodSig, Resolution, Span, StructBody, TraitFnSelfSig,
    TraitItem, Ty, TyKind, TypeChecker, TypeError, Visibility, bare_path_type_name,
    body_value_span, bound_names, derive_rejection_hint, evaluate_const_int_from_expr,
    expr_is_static_string_value, impl_self_ty_name, is_compiler_generated, known_builtin_trait,
    qualified_type_name, render_ty, self_assoc_projection,
};

impl TypeChecker<'_> {
    pub(super) fn collect_signatures(&mut self, items: &[Item]) {
        self.collect_signatures_in(items, &mut Vec::new());
    }

    /// Registers every signature in `items`, tracking the module path so a
    /// type's identity is the name it can be reached by rather than the bare
    /// name two modules may share.
    pub(super) fn collect_signatures_in(&mut self, items: &[Item], module_path: &mut Vec<String>) {
        // First pass: index every trait name + its methods + supertraits,
        // and every user struct / enum name, so subsequent passes can
        // validate `<T: Bound>` bounds, reject name-global method
        // mis-dispatch, and detect supertrait-through-bound calls
        // regardless of declaration order relative to impl blocks.
        self.collect_trait_names_in(items, module_path);
        // Register alias targets before any type lowering so a struct
        // field / let / param naming `X` (where `type X = T`) expands to
        // `T` regardless of declaration order.
        self.collect_type_aliases(items);
        for item in items {
            self.register_must_use(item);
            if let ItemKind::Struct(_) = &item.kind
                && item.attrs.lists_argument("repr", "C")
                && let Some(def) = self.resolutions.definition_of(item.id)
            {
                self.repr_c_structs.insert(def);
            }
            if let ItemKind::Struct(_) = &item.kind
                && item.attrs.has_word(gossamer_ast::FOREIGN_TYPE_ATTR)
                && let Some(def) = self.resolutions.definition_of(item.id)
            {
                self.opaque_types.insert(def);
            }
            if let ItemKind::Static(decl) = &item.kind
                && item.attrs.has_word(gossamer_ast::FOREIGN_STATIC_ATTR)
                && let Some(def) = self.resolutions.definition_of(item.id)
            {
                self.foreign_statics.insert(def, decl.name.name.clone());
            }
            match &item.kind {
                ItemKind::Fn(decl) => self.register_fn_sig(item.id, decl, item.span),
                ItemKind::Impl(decl) => {
                    self.validate_declared_bounds(&decl.generics, &decl.where_clause, item.span);
                    self.collect_impl_signatures(decl, module_path);
                    // A `cmp` the source wrote is the type's order. The
                    // synthesized field-by-field one carries the marker, and
                    // is the order every ordering primitive already reads.
                    if !item.attrs.has_word("gos_synthesized")
                        && decl
                            .items
                            .iter()
                            .any(|it| matches!(it, ImplItem::Fn(f) if f.name.name == "cmp"))
                    {
                        self.user_ordered_types.insert(impl_self_ty_name(decl));
                    }
                }
                ItemKind::Trait(decl) => {
                    self.validate_declared_bounds(&decl.generics, &decl.where_clause, item.span);
                    self.collect_trait_signatures(decl);
                }
                ItemKind::Struct(decl) => {
                    self.validate_derives(&item.attrs, item.span);
                    self.validate_declared_bounds(&decl.generics, &decl.where_clause, item.span);
                    self.register_struct(item.id, decl, module_path);
                }
                ItemKind::Enum(decl) => {
                    self.validate_derives(&item.attrs, item.span);
                    self.validate_declared_bounds(&decl.generics, &decl.where_clause, item.span);
                    self.register_enum(item.id, decl, item.span, module_path);
                }
                ItemKind::Const(decl) => self.register_const(item.id, &decl.ty, &decl.value),
                ItemKind::Static(decl) => {
                    // A static's declared type is what a reference to it
                    // reads, exactly as a `const`'s is. Without it every use
                    // took a fresh variable and went unchecked.
                    self.register_const(item.id, &decl.ty, &decl.value);
                    if let Some(def) = self.resolutions.definition_of(item.id) {
                        self.static_mutability.insert(
                            def,
                            matches!(decl.mutability, gossamer_ast::Mutability::Mutable),
                        );
                    }
                }
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        module_path.push(decl.name.name.clone());
                        self.collect_signatures_in(inner, module_path);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
        // Every user type is registered by now, so an `impl` block can be
        // matched to the declaration its self type names regardless of the
        // order the two appear in.
        self.collect_impl_obligations_in(items, module_path);
    }

    /// Indexes one struct / enum under both the identity it is reached by
    /// (`a::Point`) and its declared name. The bare key is first-declaration
    /// wins, so a second module declaring the name never displaces the first -
    /// a reference that means the second one is written or imported through
    /// its module and resolves on the qualified key.
    pub(super) fn register_adt_name(
        &mut self,
        item_id: NodeId,
        name: &str,
        module_path: &[String],
    ) {
        self.user_type_decls.insert(name.to_string());
        let identity = qualified_type_name(module_path, name);
        self.user_type_decls.insert(identity.clone());
        if module_path.is_empty() {
            self.root_type_names.insert(name.to_string());
        }
        if let Some(def) = self.resolutions.definition_of(item_id) {
            self.adt_def_by_name.insert(identity, def);
            self.adt_def_by_name.entry(name.to_string()).or_insert(def);
        }
    }

    /// Attaches each `impl` block's generic bounds to the type it targets
    /// and verifies every trait impl supplies exactly the items its trait
    /// declares. Tracks the module path so two modules each declaring a
    /// `Point` claim distinct `(trait, type)` pairs.
    pub(super) fn collect_impl_obligations_in(
        &mut self,
        items: &[Item],
        module_path: &mut Vec<String>,
    ) {
        for item in items {
            match &item.kind {
                ItemKind::Impl(decl) => {
                    self.record_impl_param_bounds(decl);
                    self.check_trait_impl_completeness(decl, item.span);
                    self.check_trait_impl_membership(decl, item.span);
                    self.check_trait_impl_uniqueness(decl, module_path, item.span);
                    self.check_method_uniqueness(decl, module_path);
                    self.check_trait_impl_assoc_items(decl, item.span);
                    self.check_supertrait_impls(decl, module_path, item.span);
                }
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        module_path.push(decl.name.name.clone());
                        self.collect_impl_obligations_in(inner, module_path);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    /// Walks `items` recursively into inline modules and records, for
    /// later passes: every trait name (for `<T: Bound>` validation),
    /// each trait's own method names and supertrait list (for the
    /// supertrait-through-bound check), and every user struct / enum
    /// name (to tell a real user Adt receiver from a synthesized
    /// sentinel one). Idempotent - re-calling adds to the existing sets.
    /// Tracks the module path so a type registers under the identity it is
    /// reached by.
    pub(super) fn collect_trait_names_in(&mut self, items: &[Item], module_path: &mut Vec<String>) {
        for item in items {
            match &item.kind {
                ItemKind::Trait(decl) => {
                    self.declared_trait_names.insert(decl.name.name.clone());
                    let methods: std::collections::HashSet<String> = decl
                        .items
                        .iter()
                        .filter_map(|it| match it {
                            TraitItem::Fn(fn_decl) => Some(fn_decl.name.name.clone()),
                            _ => None,
                        })
                        .collect();
                    self.trait_own_methods
                        .entry(decl.name.name.clone())
                        .or_default()
                        .extend(methods);
                    let required = self
                        .trait_required_methods
                        .entry(decl.name.name.clone())
                        .or_default();
                    for it in &decl.items {
                        if let TraitItem::Fn(fn_decl) = it
                            && fn_decl.body.is_none()
                            && !required.contains(&fn_decl.name.name)
                        {
                            required.push(fn_decl.name.name.clone());
                        }
                    }
                    let declared = self
                        .trait_declared_methods
                        .entry(decl.name.name.clone())
                        .or_default();
                    for it in &decl.items {
                        if let TraitItem::Fn(fn_decl) = it
                            && !declared.contains(&fn_decl.name.name)
                        {
                            declared.push(fn_decl.name.name.clone());
                        }
                    }
                    for item in &decl.items {
                        if let TraitItem::Fn(fn_decl) = item {
                            let requires_mut = fn_decl.params.iter().any(|param| {
                                matches!(param, FnParam::Receiver(gossamer_ast::Receiver::RefMut))
                            });
                            self.trait_method_requires_mut.insert(
                                (decl.name.name.clone(), fn_decl.name.name.clone()),
                                requires_mut,
                            );
                        }
                    }
                    let supers: Vec<String> = decl
                        .supertraits
                        .iter()
                        .filter_map(|b| b.path.segments.last().map(|s| s.name.name.clone()))
                        .collect();
                    if !supers.is_empty() {
                        self.trait_supertraits
                            .insert(decl.name.name.clone(), supers);
                    }
                }
                ItemKind::Struct(decl) => {
                    self.register_adt_name(item.id, &decl.name.name, module_path);
                    self.record_derived_traits(
                        &qualified_type_name(module_path, &decl.name.name),
                        &item.attrs,
                    );
                }
                ItemKind::Enum(decl) => {
                    self.register_adt_name(item.id, &decl.name.name, module_path);
                    self.record_derived_traits(
                        &qualified_type_name(module_path, &decl.name.name),
                        &item.attrs,
                    );
                }
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        module_path.push(decl.name.name.clone());
                        self.collect_trait_names_in(inner, module_path);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    /// Records the trait names a declaration's `#[derive(..)]` attributes
    /// supply, keyed by the identity the type is reached by, so a written
    /// `impl` of one of them is reported rather than silently competing with
    /// the synthesized body.
    pub(super) fn record_derived_traits(&mut self, ty_name: &str, attrs: &gossamer_ast::Attrs) {
        for attr in &attrs.outer {
            if attr.path.segments.len() != 1 || attr.path.segments[0].name.name != "derive" {
                continue;
            }
            let Some(tokens) = &attr.tokens else {
                continue;
            };
            let entry = self.derived_traits.entry(ty_name.to_string()).or_default();
            for name in tokens.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                entry.insert(name.to_string());
            }
        }
    }

    pub(super) fn register_const(
        &mut self,
        item_id: NodeId,
        ty: &gossamer_ast::Type,
        value: &Expr,
    ) {
        let Some(def) = self.resolutions.definition_of(item_id) else {
            return;
        };
        let resolved = self.type_from_ast(ty);
        self.const_tys.insert(def, resolved);
        if let Some(literal) = evaluate_const_int_from_expr(value) {
            self.const_int_values.insert(def, literal);
        }
    }

    /// The integer a length expression names when it is a path to a `const`
    /// item holding one, or `None` for any other expression.
    pub(super) fn const_int_of_path(&mut self, expr: &Expr) -> Option<u128> {
        let ExprKind::Path(_) = &expr.kind else {
            return None;
        };
        let Resolution::Def {
            def,
            kind: gossamer_resolve::DefKind::Const,
        } = self.resolutions.get(expr.id)?
        else {
            return None;
        };
        self.const_int_values.get(&def).copied()
    }

    /// Records each `type X<..> = T` alias's type-parameter names and
    /// right-hand side by `DefId`, recursing into inline modules. A use of
    /// the alias expands to `T` (with the params substituted by the
    /// use-site arguments for a generic alias) during type lowering.
    pub(super) fn collect_type_aliases(&mut self, items: &[Item]) {
        for item in items {
            match &item.kind {
                ItemKind::TypeAlias(decl) => {
                    if let Some(def) = self.resolutions.definition_of(item.id) {
                        let params: Vec<String> = decl
                            .generics
                            .params
                            .iter()
                            .filter_map(|p| match p {
                                gossamer_ast::GenericParam::Type { name, .. } => {
                                    Some(name.name.clone())
                                }
                                _ => None,
                            })
                            .collect();
                        if decl.nominal {
                            self.nominal_aliases.insert(def);
                            // The type prints under its own name, so a
                            // mismatch against the representation names
                            // both sides distinctly.
                            self.tcx.register_def_name(def, decl.name.name.clone());
                        }
                        self.alias_targets.insert(def, (params, decl.ty.clone()));
                    }
                }
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        self.collect_type_aliases(inner);
                    }
                }
                _ => {}
            }
        }
    }

    /// Registers an enum's `DefId -> name` so `render_ty` / `adt_dispatch_name`
    /// recover "Shape" instead of the "adt#N" placeholder - needed for `==` /
    /// `{:?}` dispatch on enum values whose type resolves to the Adt.
    #[allow(
        clippy::too_many_lines,
        reason = "one pass per fact an enum declaration records"
    )]
    pub(super) fn register_enum(
        &mut self,
        item_id: NodeId,
        decl: &gossamer_ast::EnumDecl,
        span: Span,
        module_path: &[String],
    ) {
        let identity = qualified_type_name(module_path, &decl.name.name);
        if let Some(def) = self.resolutions.definition_of(item_id) {
            self.tcx.register_def_name(def, identity.as_str());
            self.user_type_defs.insert(identity.clone(), def);
            self.user_type_defs
                .entry(decl.name.name.clone())
                .or_insert(def);
            self.record_adt_param_bounds(def, &decl.generics, &decl.where_clause);
            // A generic enum instantiates its parameters per constructor call
            // and per match arm, exactly as a generic struct does. The arity
            // says how many fresh variables each of those needs.
            // Every generic position counts, so a const parameter keeps the
            // index its `[T; N]` payload names.
            let arity = decl
                .generics
                .params
                .iter()
                .filter(|p| {
                    matches!(
                        p,
                        gossamer_ast::GenericParam::Type { .. }
                            | gossamer_ast::GenericParam::Const { .. }
                    )
                })
                .count();
            if arity > 0 {
                self.struct_generic_arity.insert(def, arity);
            }
            let const_tys: Vec<Option<Ty>> = decl
                .generics
                .params
                .iter()
                .map(|param| match param {
                    gossamer_ast::GenericParam::Const { ty, .. } => Some(self.type_from_ast(ty)),
                    _ => None,
                })
                .collect();
            if const_tys.iter().any(Option::is_some) {
                self.const_generics.param_tys.insert(def, const_tys);
            }
            self.tcx.register_enum_variant_names(
                def,
                decl.variants
                    .iter()
                    .map(|variant| variant.name.name.clone())
                    .collect(),
            );
            // Payload-bearing enums are reference-counted heap
            // values; register the def eagerly so the MIR drop pass
            // sees enum-typed locals as RC-managed in every body,
            // not just bodies lowered after the enum's first
            // constructor. All-unit enums lower as bare `i64`
            // discriminants and are excluded.
            let has_payload = decl.variants.iter().any(|v| match &v.body {
                StructBody::Tuple(fields) => !fields.is_empty(),
                StructBody::Named(fields) => !fields.is_empty(),
                StructBody::Unit => false,
            });
            if has_payload {
                self.tcx.register_rc_managed_enum_def(def.local);
            }
            // Cache the enum's `Adt` type so a tuple-variant constructor call
            // resolves to it. Generic enums need per-call-site substs, so they
            // keep the fresh-variable path.
            if decl.generics.params.is_empty() {
                let adt = self.tcx.intern(TyKind::Adt {
                    def,
                    substs: crate::Substs::from_types(std::iter::empty()),
                });
                // The identity is authoritative; the bare alias is
                // first-declaration wins so a second module declaring the
                // name never displaces the first.
                self.enum_tys.insert(identity.clone(), adt);
                self.enum_tys.entry(decl.name.name.clone()).or_insert(adt);
                self.tcx.register_enum_ty_by_name(identity.as_str(), adt);
            }
        }
        for key in [identity.clone(), decl.name.name.clone()] {
            let variant_names = self.enum_variants.entry(key).or_default();
            for variant in &decl.variants {
                variant_names.insert(variant.name.name.clone());
            }
        }
        // A payload type naming a parameter (`Leaf(T)`) records a rigid
        // `TyKind::Param` slot so each constructor call and each match arm
        // substitutes it independently. Lowered outside the scope it would be
        // one shared inference variable, and the first instantiation to pin
        // it would fix the payload type for every other.
        let payload_scope = self.enter_generic_scope(&decl.generics);
        for variant in &decl.variants {
            match &variant.body {
                StructBody::Tuple(fields) => {
                    let tys: Vec<Ty> = fields.iter().map(|f| self.type_from_ast(&f.ty)).collect();
                    self.enum_variant_payloads
                        .insert((identity.clone(), variant.name.name.clone()), tys.clone());
                    self.enum_variant_payloads
                        .entry((decl.name.name.clone(), variant.name.name.clone()))
                        .or_insert(tys);
                }
                StructBody::Named(fields) => {
                    let tys: Vec<(String, Ty)> = fields
                        .iter()
                        .map(|f| (f.name.name.clone(), self.type_from_ast(&f.ty)))
                        .collect();
                    self.enum_variant_named_payloads
                        .insert((identity.clone(), variant.name.name.clone()), tys.clone());
                    self.enum_variant_named_payloads
                        .entry((decl.name.name.clone(), variant.name.name.clone()))
                        .or_insert(tys);
                }
                StructBody::Unit => {}
            }
        }
        // Per-variant field types in declaration (discriminant) order, for the
        // MIR structural-equality descriptor of heap enums.
        if let Some(def) = self.resolutions.definition_of(item_id) {
            let mut variant_tys: Vec<Vec<Ty>> = Vec::with_capacity(decl.variants.len());
            for variant in &decl.variants {
                let tys: Vec<Ty> = match &variant.body {
                    StructBody::Tuple(fields) => {
                        fields.iter().map(|f| self.type_from_ast(&f.ty)).collect()
                    }
                    StructBody::Named(fields) => {
                        fields.iter().map(|f| self.type_from_ast(&f.ty)).collect()
                    }
                    StructBody::Unit => Vec::new(),
                };
                let tys = tys
                    .into_iter()
                    .map(|t| self.const_length_carrier(t))
                    .collect();
                variant_tys.push(tys);
            }
            self.tcx.register_enum_variant_tys(def, variant_tys);
        }
        self.leave_generic_scope(payload_scope);
        // The width the declaration settles on, so layout reads one answer
        // rather than re-deriving it per back-end.
        if let Some(def) = self.resolutions.definition_of(item_id) {
            let bits = decl.repr.bits(decl.variants.len());
            if bits != gossamer_ast::EnumRepr::natural_bits(false, decl.variants.len()) {
                self.tcx.register_enum_repr_bits(def, bits);
            }
        }
        // A declared width has to hold every variant, and the natural width
        // caps the rest: the heap representation stores the discriminant in a
        // one-byte header field.
        if let Some(bits) = decl.repr.declared_bits {
            if !gossamer_ast::EnumRepr::fits(bits, decl.variants.len()) {
                self.emit(
                    TypeError::EnumReprTooNarrow {
                        name: decl.name.name.clone(),
                        bits,
                        count: decl.variants.len(),
                    },
                    span,
                );
            }
        } else if decl.variants.len() > 256 {
            self.emit(
                TypeError::TooManyVariants {
                    name: decl.name.name.clone(),
                    count: decl.variants.len(),
                },
                span,
            );
        }
    }

    /// Rejects `#[derive(...)]` names that synthesize nothing. Gossamer's
    /// value-type structs / enums compare, order, hash, and copy by value
    /// automatically, so the meaningful derives are exactly `Debug`, `Default`,
    /// `PartialEq`, `Eq`, `PartialOrd`, and `Ord` - each of which still does
    /// work for a generic type, whose rendering and comparison depend on the
    /// arguments an instantiation supplies. Every other name is either
    /// automatic (`Clone` - `let b = a` copies, `Hash`, `Copy`, `Display`,
    /// serde) or implemented with `impl Trait for T` (`From`, operators).
    /// Records `#[must_use]` on a function, struct, or enum declaration so
    /// a discarded value of it is reported (GT0064).
    pub(super) fn register_must_use(&mut self, item: &Item) {
        if !item.attrs.has_word("must_use") {
            return;
        }
        let Some(def) = self.resolutions.definition_of(item.id) else {
            return;
        };
        match &item.kind {
            ItemKind::Fn(decl) => {
                self.must_use_fns.insert(def, decl.name.name.clone());
            }
            ItemKind::Struct(decl) => {
                self.must_use_types.insert(def, decl.name.name.clone());
            }
            ItemKind::Enum(decl) => {
                self.must_use_types.insert(def, decl.name.name.clone());
            }
            _ => {}
        }
    }

    pub(super) fn validate_derives(&mut self, attrs: &gossamer_ast::Attrs, span: Span) {
        for attr in &attrs.outer {
            let is_derive =
                attr.path.segments.len() == 1 && attr.path.segments[0].name.name == "derive";
            if !is_derive {
                continue;
            }
            let Some(tokens) = &attr.tokens else {
                continue;
            };
            for tok in tokens.split(',') {
                let name = tok.trim();
                if name.is_empty()
                    || matches!(
                        name,
                        "Debug" | "Default" | "PartialEq" | "Eq" | "PartialOrd" | "Ord"
                    )
                {
                    continue;
                }
                self.emit(
                    TypeError::UnsupportedDerive {
                        name: name.to_string(),
                        hint: derive_rejection_hint(name),
                    },
                    span,
                );
            }
        }
    }

    pub(super) fn register_struct(
        &mut self,
        item_id: NodeId,
        decl: &gossamer_ast::StructDecl,
        module_path: &[String],
    ) {
        let Some(def) = self.resolutions.definition_of(item_id) else {
            return;
        };
        let identity = qualified_type_name(module_path, &decl.name.name);
        let name = identity.as_str();
        self.tcx.register_def_name(def, name);
        self.user_type_defs.insert(identity.clone(), def);
        self.user_type_defs
            .entry(decl.name.name.clone())
            .or_insert(def);
        self.record_adt_param_bounds(def, &decl.generics, &decl.where_clause);
        // Build the generic-parameter scope so `Pair<A, B> { fst:
        // A, snd: B }` field-type references resolve to the right
        // `TyKind::Param` indices.
        let prior_scope = self.enter_generic_scope(&decl.generics);
        // Record the struct's generic-parameter arity in source
        // order so struct-literal substitution at use sites knows
        // how many fresh inference variables to allocate.
        // Every generic position counts, so a const parameter keeps the index
        // its `[T; N]` field names.
        let arity = decl
            .generics
            .params
            .iter()
            .filter(|p| {
                matches!(
                    p,
                    gossamer_ast::GenericParam::Type { .. }
                        | gossamer_ast::GenericParam::Const { .. }
                )
            })
            .count();
        if arity > 0 {
            self.struct_generic_arity.insert(def, arity);
        }
        let const_tys: Vec<Option<Ty>> = decl
            .generics
            .params
            .iter()
            .map(|param| match param {
                gossamer_ast::GenericParam::Const { ty, .. } => Some(self.type_from_ast(ty)),
                _ => None,
            })
            .collect();
        if const_tys.iter().any(Option::is_some) {
            self.const_generics.param_tys.insert(def, const_tys);
        }
        // Tuple-struct fields are modelled as named fields "0".."N-1", so a
        // `Pt(a, b)` constructor (rewritten to a `Pt { 0: a, 1: b }` literal)
        // and positional access `p.0` reuse the named-field machinery.
        let list: Vec<(String, Ty)> = match &decl.body {
            StructBody::Named(fields) => fields
                .iter()
                .map(|f| (f.name.name.clone(), self.type_from_ast(&f.ty)))
                .collect(),
            StructBody::Tuple(fields) => fields
                .iter()
                .enumerate()
                .map(|(i, f)| (i.to_string(), self.type_from_ast(&f.ty)))
                .collect(),
            StructBody::Unit => Vec::new(),
        };
        // A field's visibility is declared on the field, so a `pub` struct
        // may keep private ones. Record each field's home module alongside
        // it so a reference from outside is checked like a method call.
        let visibilities: Vec<(String, Visibility)> = match &decl.body {
            StructBody::Named(fields) => fields
                .iter()
                .map(|f| (f.name.name.clone(), f.visibility))
                .collect(),
            StructBody::Tuple(fields) => fields
                .iter()
                .enumerate()
                .map(|(i, f)| (i.to_string(), f.visibility))
                .collect(),
            StructBody::Unit => Vec::new(),
        };
        for (name, visibility) in visibilities {
            self.field_homes
                .insert((def, name), (module_path.to_vec(), visibility));
        }
        // A unit struct is the zero-field shape `Unit {}` also spells, so it
        // carries the same registered (empty) layout: the tiers read its
        // fields, its slots, and its key content through one description.
        // A field whose length is a const parameter has no length until an
        // instantiation supplies one, so its storage is the runtime-length
        // sequence a const generic body holds. The checker keeps the declared
        // `[T; N]` for typing; the layout reads the carrier.
        let tys: Vec<Ty> = list
            .iter()
            .map(|(_, t)| self.const_length_carrier(*t))
            .collect();
        self.tcx.register_struct_fields(def, tys);
        self.struct_fields.insert(def, list);
        if matches!(decl.body, StructBody::Tuple(_)) {
            self.tcx.register_tuple_struct(def.local);
        }
        self.leave_generic_scope(prior_scope);
    }

    /// Resolves `receiver_ty.field_name` to the leaf field type.
    /// Auto-dereferences through `&T`/`&mut T` wrappers. Returns
    /// `None` when the receiver does not name a known struct or the
    /// field is not declared on it.
    /// Resolves field access to a type, distinguishing failure
    /// modes worth surfacing to the user:
    ///
    /// - `Err(UnknownField { opaque: true })` - the receiver is an
    ///   `Adt` whose field map isn't registered (typical of opaque
    ///   stdlib types like `json::Value`).
    /// - `Err(UnknownField { opaque: false })` - the receiver is a
    ///   known struct but the field name doesn't match any of its
    ///   fields.
    ///
    /// Non-Adt receivers (primitives, tuples, unresolved inference
    /// vars, generic params) get `Ok(fresh_var)` so the rest of the
    /// expression keeps type-checking. Catching those would either
    /// fight the trait-method machinery or block legitimate
    /// inference.
    pub(super) fn lookup_field_ty_diagnosed(
        &mut self,
        receiver_ty: Ty,
        field_name: &str,
    ) -> Result<Ty, TypeError> {
        let resolved = self.infer.resolve(self.tcx, receiver_ty);
        let mut cur = resolved;
        loop {
            match self.tcx.kind_of(cur).clone() {
                TyKind::Ref { inner, .. } => cur = inner,
                TyKind::Adt { def, substs } => {
                    let ty_name = self.render_public_ty(resolved);
                    let Some(fields) = self.struct_fields.get(&def).cloned() else {
                        return Err(TypeError::UnknownField {
                            ty: ty_name,
                            field: field_name.to_string(),
                            opaque: true,
                            declared: Vec::new(),
                            field_span: None,
                            method_of_same_name: false,
                        });
                    };
                    for (name, ty) in &fields {
                        if name == field_name {
                            // Substitute `TyKind::Param { idx }`
                            // slots in the declared field type
                            // with the matching generic argument
                            // from the receiver's `substs`. This
                            // is the dual of the substitution at
                            // struct-literal sites: literals
                            // allocate fresh vars for each
                            // parameter; field reads need to
                            // resolve `Param` back to the
                            // receiver's per-instance argument.
                            let (types, consts) = self.adt_subst_vectors(&substs);
                            return Ok(self.subst_generics_in_ty(*ty, &types, &consts));
                        }
                    }
                    return Err(TypeError::UnknownField {
                        ty: ty_name,
                        field: field_name.to_string(),
                        opaque: false,
                        declared: fields.iter().map(|(name, _)| name.clone()).collect(),
                        field_span: None,
                        method_of_same_name: self.has_method_named(cur, field_name),
                    });
                }
                // A scalar, a text value, a sequence, a map, a tuple, and a
                // range or iterator (which is iteration state) carry no named
                // fields at all, so the read has no type to answer with and
                // every tier faults on it at run time.
                TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Vec(_)
                | TyKind::Slice(_)
                | TyKind::Array { .. }
                | TyKind::HashMap { .. }
                | TyKind::Tuple(_)
                | TyKind::Range(_)
                | TyKind::Iterator(_) => {
                    return Err(TypeError::UnknownField {
                        ty: self.render_public_ty(cur),
                        field: field_name.to_string(),
                        opaque: false,
                        declared: Vec::new(),
                        field_span: None,
                        method_of_same_name: self.has_method_named(cur, field_name),
                    });
                }
                // A variable constrained to a numeric family can only
                // ever be a scalar, whatever width it settles on.
                TyKind::Var(_)
                    if self.infer.is_float_literal_var(self.tcx, cur)
                        || self.infer.is_integer_constrained_var(self.tcx, cur) =>
                {
                    return Err(TypeError::UnknownField {
                        ty: self.render_public_ty(cur),
                        field: field_name.to_string(),
                        opaque: false,
                        declared: Vec::new(),
                        field_span: None,
                        method_of_same_name: self.has_method_named(cur, field_name),
                    });
                }
                _ => return Ok(self.fresh()),
            }
        }
    }

    /// Whether `resolved` answers a method spelled `name` - the
    /// difference between a misspelled field and a call missing its
    /// parentheses.
    pub(super) fn has_method_named(&mut self, resolved: Ty, name: &str) -> bool {
        if self.known_method_names(resolved).iter().any(|m| m == name) {
            return true;
        }
        let numeric = matches!(
            self.tcx.kind(resolved),
            Some(TyKind::Int(_) | TyKind::Float(_))
        ) || self.infer.is_float_literal_var(self.tcx, resolved)
            || self.infer.is_integer_constrained_var(self.tcx, resolved);
        numeric && crate::stdlib_signatures::function_shape_for_path(&["math"], name).is_some()
    }

    /// The names an `impl` block's methods register under: the identity
    /// its self type is reached by (`lib::Point`) first, then the bare
    /// spelling. A resolved receiver carries the identity
    /// [`Self::register_struct`] recorded, so an impl inside `mod lib`
    /// that keyed only the bare name would be invisible to every
    /// receiver-typed lookup; the bare key stays for the sites that key
    /// on a written `Type::method` path instead.
    pub(super) fn impl_owner_keys(
        &mut self,
        self_ty: &gossamer_ast::Type,
        module_path: &[String],
    ) -> Option<Vec<String>> {
        let gossamer_ast::ty::TypeKind::Path(tp) = &self_ty.kind else {
            // A structural type has no path to name it by; it keys on the
            // spelling every layer shares for one.
            let lowered = self.type_from_ast(self_ty);
            let settled = self.deep_resolve(lowered);
            return crate::printer::structural_impl_owner(self.tcx, settled).map(|name| vec![name]);
        };
        let segments: Vec<&str> = tp.segments.iter().map(|s| s.name.name.as_str()).collect();
        let bare = (*segments.last()?).to_string();
        // A path written with its module (`impl lib::Point`) already
        // spells the identity; a bare one names a type the enclosing
        // module declares, or an imported one that keeps its own name.
        let mut candidates: Vec<String> = Vec::new();
        if segments.len() > 1 {
            candidates.push(segments.join("::"));
        }
        // The type may be declared in the module this `impl` sits in, or in
        // any module enclosing it: a package splits one type's methods across
        // sibling files, and consumed as a dependency every one of those sits
        // one module deeper than the type they implement.
        for level in (0..=module_path.len()).rev() {
            candidates.push(qualified_type_name(&module_path[..level], &bare));
        }
        let identity = candidates
            .into_iter()
            .find(|candidate| self.user_type_decls.contains(candidate))
            .unwrap_or_else(|| bare.clone());
        // The bare name also reaches a module's type, unless a type of that
        // very name is declared at the root, whose methods it would replace.
        if identity == bare || self.root_type_names.contains(&bare) {
            return Some(vec![identity]);
        }
        Some(vec![identity, bare])
    }

    /// Identities the owner path of an associated call could name, most
    /// specific first: the path anchored under each enclosing module, then
    /// the path as written, then its bare tail.
    ///
    /// A path is written relative to the module it appears in, while a type
    /// registers under its full module-qualified identity, so `model::Point`
    /// written inside `engine` names `pkg::model::Point` when both sit under
    /// `pkg` - which is what a package consumed as a dependency looks like.
    pub(super) fn owner_identity_candidates(&self, owner: &[&str]) -> Vec<String> {
        let written = owner.join("::");
        let mut out = Vec::new();
        for level in (1..=self.current_module.len()).rev() {
            out.push(format!(
                "{}::{written}",
                self.current_module[..level].join("::")
            ));
        }
        out.push(written);
        if let Some(bare) = owner.last() {
            out.push((*bare).to_string());
        }
        out
    }

    /// Records which module each of an impl's methods is declared in, and
    /// at what visibility, so a call from elsewhere is checked against the
    /// declaration rather than against the call site's module.
    pub(super) fn record_impl_method_homes(
        &mut self,
        decl: &ImplDecl,
        owners: &[String],
        module_path: &[String],
    ) {
        for owner in owners {
            self.collect_impl_method_owners_and_mutability(decl, owner);
        }
        // A trait impl's methods are reachable wherever the trait is: the
        // trait declares the surface, the impl only supplies it.
        let via_trait = decl.trait_ref.is_some();
        for item in &decl.items {
            if let ImplItem::Fn(fn_decl) = item {
                let visibility = if via_trait {
                    Visibility::Public
                } else {
                    fn_decl.visibility
                };
                for owner in owners {
                    self.method_homes.insert(
                        (owner.clone(), fn_decl.name.name.clone()),
                        (module_path.to_vec(), visibility),
                    );
                }
            }
        }
    }

    pub(super) fn collect_impl_signatures(&mut self, decl: &ImplDecl, module_path: &[String]) {
        // Self-type names for receiver-keyed method return types.
        // Generic impls are skipped: their returns may mention
        // `Param` slots that a bare lookup cannot substitute.
        let owner_keys = self.impl_owner_keys(&decl.self_ty, module_path);
        let self_names = if decl.generics.params.is_empty() {
            owner_keys.clone()
        } else {
            None
        };
        // The owner type names for method-ownership tracking are recorded
        // even for generic impls (`impl<T> Stack<T>`), so a method call
        // on a generic user type is not falsely flagged as belonging to
        // a different type.
        let owner_names = owner_keys;
        if let Some(owners) = &owner_names {
            self.record_impl_method_homes(decl, owners, module_path);
        }
        // Record `impl Trait for Type` so a `T: Trait` bound can be verified
        // against the concrete argument type at a generic call site.
        if let Some(trait_ref) = &decl.trait_ref
            && let Some(trait_seg) = trait_ref.path.segments.last()
            && let Some(owners) = &owner_names
        {
            self.trait_impl_types
                .entry(trait_seg.name.name.clone())
                .or_default()
                .extend(owners.iter().cloned());
        }
        // `Self` in a signature names the type being implemented, and
        // signatures are collected before any impl body is checked, so the
        // binding has to be in place here too - otherwise a `-> Self`
        // constructor records an unconstrained return type and every call
        // on its result goes unchecked.
        let self_scope = self.enter_generic_scope(&decl.generics);
        let impl_self_ty = self.type_from_ast(&decl.self_ty);
        self.leave_generic_scope(self_scope);
        let prev_self_ty = self.current_self_ty.replace(impl_self_ty);
        let prev_self_name = std::mem::replace(
            &mut self.current_self_ty_name,
            gossamer_ast::assoc::type_head_name(&decl.self_ty).map(ToString::to_string),
        );
        let prev_impl_generics = self.current_impl_generics.replace(decl.generics.clone());
        let prev_impl_where =
            std::mem::replace(&mut self.current_impl_where, decl.where_clause.clone());
        for item in &decl.items {
            if let ImplItem::Fn(fn_decl) = item {
                self.collect_impl_fn_signature(
                    decl,
                    fn_decl,
                    self_names.as_deref(),
                    owner_names.as_deref(),
                );
            }
        }
        self.current_impl_where = prev_impl_where;
        self.current_impl_generics = prev_impl_generics;
        self.current_self_ty_name = prev_self_name;
        self.current_self_ty = prev_self_ty;
    }

    /// Records one `impl` method's signature under the owner names it is
    /// reached by. `self_names` is set for a non-generic block, whose
    /// signatures are concrete; `owner_names` for every block, so a generic
    /// block's signatures are kept with their parameter slots.
    pub(super) fn collect_impl_fn_signature(
        &mut self,
        decl: &ImplDecl,
        fn_decl: &FnDecl,
        self_names: Option<&[String]>,
        owner_names: Option<&[String]>,
    ) {
        self.register_fn_sig_anonymous(fn_decl);
        self.register_method_arg_sig(fn_decl);
        // A method with its own type parameters is registered too,
        // as long as its RETURN names none of them: `fn arg<T:
        // Arg>(self, v: T) -> Cmd` answers a `Cmd` at every call
        // site, so recording it is what keeps a field read through
        // the result checked. Without this the call typed as a fresh
        // variable and `c.no_such_field` passed `gos check`.
        let method_ret_is_concrete = fn_decl.generics.params.is_empty()
            || fn_decl.ret.as_ref().is_some_and(|ty| {
                let scope = self.enter_fn_generic_scope(&fn_decl.generics);
                let resolved = self.type_from_ast(ty);
                self.leave_generic_scope(scope);
                !self.ty_mentions_generic_param(resolved)
            });
        if let Some(names) = self_names
            && method_ret_is_concrete
        {
            // A method with its own type parameters contributes its
            // RETURN only: its parameter types carry rigid `Param`
            // slots that each call site instantiates for itself, so
            // recording them would check the second `arg("two")`
            // against the first `arg(1)`'s instantiation.
            let own_generics = !fn_decl.generics.params.is_empty();
            let scope = self.enter_fn_generic_scope(&fn_decl.generics);
            let params: Vec<Ty> = fn_decl
                .params
                .iter()
                .filter(|p| matches!(p, FnParam::Typed { .. }))
                .map(|p| self.param_ty(p))
                .collect();
            let ret = match fn_decl.ret.as_ref() {
                Some(ty) => self.type_from_ast(ty),
                None => self.tcx.unit(),
            };
            self.leave_generic_scope(scope);
            let arity = params.len();
            for name in names {
                if !own_generics {
                    self.method_param_types
                        .insert((name.clone(), fn_decl.name.name.clone()), params.clone());
                }
                self.method_ret_types
                    .insert((name.clone(), fn_decl.name.name.clone(), arity), ret);
                self.method_arities
                    .insert((name.clone(), fn_decl.name.name.clone()), arity);
            }
        } else if let Some(names) = self_names
            && fn_decl
                .generics
                .params
                .iter()
                .all(|param| matches!(param, gossamer_ast::GenericParam::Type { .. }))
        {
            self.record_own_generic_method_sig(fn_decl, names);
        } else if !decl.generics.params.is_empty()
            && let Some(names) = owner_names
        {
            // Generic-impl methods (`impl<T> Add for Wrap<T>`):
            // record the return with rigid `Param` slots, resolved
            // inside the impl's generic scope. A receiver-typed use
            // site substitutes its instantiation's `substs`, and a
            // method's own type parameters take the positions after
            // the impl's, which each call instantiates from its
            // arguments.
            let scope = self.enter_generic_scope_combined(&decl.generics, &fn_decl.generics);
            let mut bindings = HashMap::new();
            Self::assoc_bindings_of(&decl.generics, &decl.where_clause, &mut bindings);
            Self::assoc_bindings_of(&fn_decl.generics, &fn_decl.where_clause, &mut bindings);
            let constraints = self.assoc_constraints_in_scope(&bindings);
            let params: Vec<Ty> = fn_decl
                .params
                .iter()
                .filter(|p| matches!(p, FnParam::Typed { .. }))
                .map(|p| self.param_ty(p))
                .collect();
            let arity = params.len();
            let ret = match fn_decl.ret.as_ref() {
                Some(ty) => self.type_from_ast(ty),
                None => self.tcx.unit(),
            };
            let impl_consts: Vec<(usize, Ty)> = decl
                .generics
                .params
                .iter()
                .enumerate()
                .filter_map(|(position, param)| match param {
                    gossamer_ast::GenericParam::Const { ty, .. } => {
                        Some((position, self.type_from_ast(ty)))
                    }
                    _ => None,
                })
                .collect();
            self.leave_generic_scope(scope);
            for name in names {
                if !impl_consts.is_empty() {
                    self.const_generics.impl_method_params.insert(
                        (name.clone(), fn_decl.name.name.clone()),
                        impl_consts.clone(),
                    );
                }
                if !constraints.is_empty() {
                    self.method_assoc_constraints.insert(
                        (name.clone(), fn_decl.name.name.clone()),
                        constraints.clone(),
                    );
                }
                self.generic_method_param_types
                    .insert((name.clone(), fn_decl.name.name.clone()), params.clone());
                self.generic_method_ret_types
                    .insert((name.clone(), fn_decl.name.name.clone(), arity), ret);
                self.method_arities
                    .insert((name.clone(), fn_decl.name.name.clone()), arity);
            }
        }
    }

    /// Records a method on a concrete user type whose own type parameters
    /// reach its return. It answers a different type at each call site, so
    /// the signature keeps those parameters rigid and every call
    /// instantiates them from its own arguments.
    pub(super) fn record_own_generic_method_sig(&mut self, fn_decl: &FnDecl, names: &[String]) {
        let scope = self.enter_fn_generic_scope(&fn_decl.generics);
        let mut bindings = HashMap::new();
        Self::assoc_bindings_of(&fn_decl.generics, &fn_decl.where_clause, &mut bindings);
        let constraints = self.assoc_constraints_in_scope(&bindings);
        let params: Vec<Ty> = fn_decl
            .params
            .iter()
            .filter(|p| matches!(p, FnParam::Typed { .. }))
            .map(|p| self.param_ty(p))
            .collect();
        let ret = match fn_decl.ret.as_ref() {
            Some(ty) => self.type_from_ast(ty),
            None => self.tcx.unit(),
        };
        self.leave_generic_scope(scope);
        let arity = params.len();
        let sig = OwnGenericMethodSig {
            generics: fn_decl.generics.params.len(),
            params,
            ret,
        };
        for name in names {
            if !constraints.is_empty() {
                self.method_assoc_constraints.insert(
                    (name.clone(), fn_decl.name.name.clone()),
                    constraints.clone(),
                );
            }
            self.own_generic_method_sigs.insert(
                (name.clone(), fn_decl.name.name.clone(), arity),
                sig.clone(),
            );
            self.method_arities
                .insert((name.clone(), fn_decl.name.name.clone()), arity);
        }
    }

    pub(super) fn collect_impl_method_owners_and_mutability(
        &mut self,
        decl: &ImplDecl,
        owner: &str,
    ) {
        let is_trait_impl = decl.trait_ref.is_some();
        for item in &decl.items {
            if let ImplItem::Fn(fn_decl) = item {
                self.user_method_owners
                    .entry(fn_decl.name.name.clone())
                    .or_default()
                    .insert(owner.to_string());
                let requires_mut = fn_decl.params.iter().any(|param| {
                    matches!(param, FnParam::Receiver(gossamer_ast::Receiver::RefMut))
                });
                let receiver_map = if is_trait_impl {
                    &mut self.trait_impl_method_requires_mut
                } else {
                    &mut self.inherent_method_requires_mut
                };
                receiver_map
                    .entry((owner.to_string(), fn_decl.name.name.clone()))
                    .and_modify(|current| *current |= requires_mut)
                    .or_insert(requires_mut);
            }
        }
        // Trait defaults are callable even when the impl does not restate
        // them, so propagate their ownership and receiver capabilities.
        let Some(trait_name) = decl
            .trait_ref
            .as_ref()
            .and_then(|trait_ref| trait_ref.path.segments.last())
            .map(|segment| segment.name.name.as_str())
        else {
            return;
        };
        if let Some(methods) = self.trait_own_methods.get(trait_name).cloned() {
            for method in methods {
                self.user_method_owners
                    .entry(method)
                    .or_default()
                    .insert(owner.to_string());
            }
        }
        for ((declaring_trait, method), requires_mut) in &self.trait_method_requires_mut {
            if declaring_trait == trait_name {
                self.trait_impl_method_requires_mut
                    .entry((owner.to_string(), method.clone()))
                    .and_modify(|current| *current |= *requires_mut)
                    .or_insert(*requires_mut);
            }
        }
    }

    pub(super) fn collect_trait_signatures(&mut self, decl: &gossamer_ast::TraitDecl) {
        let trait_name = decl.name.name.clone();
        // `Self::Item` in a trait method signature resolves through the
        // declaring trait, since no concrete self type is known yet.
        let prev_trait = self.current_trait_name.replace(trait_name.clone());
        // A function reached through a bounded type parameter (`T::zero()`)
        // reads the trait's `Self` as that parameter, so its signature is kept
        // with `Self` as a placeholder the use site substitutes. It is typed
        // before the declaration's own signature below, whose recorded node
        // types are the ones that stand.
        if decl.generics.params.is_empty() {
            let self_placeholder = self.tcx.intern(TyKind::Param {
                idx: crate::ParamIdx(0),
                name: "Self".into(),
            });
            for item in &decl.items {
                let TraitItem::Fn(fn_decl) = item else {
                    continue;
                };
                if !fn_decl.generics.params.is_empty() {
                    continue;
                }
                let prev_self = self.current_self_ty.replace(self_placeholder);
                let inputs: Vec<Ty> = fn_decl
                    .params
                    .iter()
                    .filter(|p| matches!(p, FnParam::Typed { .. }))
                    .map(|p| self.param_ty(p))
                    .collect();
                let output = match fn_decl.ret.as_ref() {
                    Some(ty) => self.type_from_ast(ty),
                    None => self.tcx.unit(),
                };
                self.current_self_ty = prev_self;
                let receiver = fn_decl.params.iter().find_map(|p| match p {
                    FnParam::Receiver(receiver) => Some(*receiver),
                    FnParam::Typed { .. } => None,
                });
                self.trait_fn_self_sigs.insert(
                    (trait_name.clone(), fn_decl.name.name.clone()),
                    TraitFnSelfSig {
                        receiver,
                        sig: FnSig { inputs, output },
                    },
                );
            }
        }
        for item in &decl.items {
            if let TraitItem::Fn(fn_decl) = item {
                self.register_fn_sig_anonymous(fn_decl);
                self.register_method_arg_sig(fn_decl);
                let ret = match fn_decl.ret.as_ref() {
                    Some(ty) => self.type_from_ast(ty),
                    None => self.tcx.unit(),
                };
                let params = fn_decl
                    .params
                    .iter()
                    .filter(|p| matches!(p, FnParam::Typed { .. }))
                    .map(|p| self.param_ty(p))
                    .collect();
                self.trait_method_params
                    .insert((trait_name.clone(), fn_decl.name.name.clone()), params);
                self.trait_method_ret
                    .insert((trait_name.clone(), fn_decl.name.name.clone()), ret);
                if let Some(assoc) = fn_decl.ret.as_ref().and_then(self_assoc_projection) {
                    self.trait_method_ret_assoc
                        .insert((trait_name.clone(), fn_decl.name.name.clone()), assoc);
                }
                let requires_mut = fn_decl.params.iter().any(|param| {
                    matches!(param, FnParam::Receiver(gossamer_ast::Receiver::RefMut))
                });
                self.trait_method_requires_mut.insert(
                    (trait_name.clone(), fn_decl.name.name.clone()),
                    requires_mut,
                );
            }
        }
        self.current_trait_name = prev_trait;
    }

    /// Records a method's non-receiver parameter types under its bare
    /// name + arity so [`Self::check_method_call`] can re-type
    /// literal arguments. Every structurally distinct signature for
    /// a key is recorded; coercion later applies only where the
    /// candidates agree (or exactly one is container-shaped), so a
    /// literal is never shaped by the wrong same-named method.
    pub(super) fn register_method_arg_sig(&mut self, decl: &FnDecl) {
        let scope = self.enter_fn_generic_scope(&decl.generics);
        let inputs: Vec<Ty> = decl
            .params
            .iter()
            .filter(|p| matches!(p, FnParam::Typed { .. }))
            .map(|p| self.param_ty(p))
            .collect();
        self.leave_generic_scope(scope);
        let key = (decl.name.name.clone(), inputs.len());
        let entry = self.method_arg_sigs.entry(key).or_default();
        let duplicate = entry.iter().any(|existing| {
            existing.len() == inputs.len()
                && existing
                    .iter()
                    .zip(&inputs)
                    .all(|(x, y)| render_ty(self.tcx, *x) == render_ty(self.tcx, *y))
        });
        if !duplicate {
            entry.push(inputs);
        }
    }

    pub(super) fn register_fn_sig(&mut self, node: NodeId, decl: &FnDecl, span: Span) {
        self.user_fn_names.insert(decl.name.name.clone());
        let sig = self.fn_sig_of(decl);
        if let Some(def) = self.resolutions.definition_of(node) {
            if decl.extern_abi.is_some() {
                self.foreign_fns.insert(def, decl.name.name.clone());
            }
            self.fn_sigs.insert(def, sig);
            // Record the generic arity and per-parameter bounds so each
            // call site can instantiate the parameters independently and
            // verify the argument types satisfy the declared bounds. The
            // arity counts every parameter position (so a const or type
            // parameter's `ParamIdx` indexes the substitution vector
            // directly); the const mask records which positions take a
            // `GenericArg::Const`.
            let has_type_or_const = decl.generics.params.iter().any(|p| {
                matches!(
                    p,
                    gossamer_ast::GenericParam::Type { .. }
                        | gossamer_ast::GenericParam::Const { .. }
                )
            });
            if has_type_or_const {
                self.fn_generic_arity
                    .insert(def, decl.generics.params.len());
                self.fn_param_bounds.insert(
                    def,
                    Self::declared_param_bounds(&decl.generics, &decl.where_clause),
                );
                let mut bindings = HashMap::new();
                Self::assoc_bindings_of(&decl.generics, &decl.where_clause, &mut bindings);
                if !bindings.is_empty() {
                    let scope = self.enter_generic_scope(&decl.generics);
                    let constraints = self.assoc_constraints_in_scope(&bindings);
                    self.leave_generic_scope(scope);
                    self.fn_assoc_constraints.insert(def, constraints);
                }
                let const_tys = decl
                    .generics
                    .params
                    .iter()
                    .map(|param| match param {
                        gossamer_ast::GenericParam::Const { ty, .. } => {
                            Some(self.type_from_ast(ty))
                        }
                        _ => None,
                    })
                    .collect();
                self.const_generics.param_tys.insert(def, const_tys);
            }
        }
        self.validate_declared_bounds(&decl.generics, &decl.where_clause, span);
    }

    /// Validates that every trait bound written on a declaration's generic
    /// parameters - in the angle brackets or in its `where` clause - names a
    /// trait this unit (or a recognised built-in) declares. Catches typos
    /// (`Hashabel` for `Hashable`) at the declaration site rather than as a
    /// "no method" error at the use site.
    pub(super) fn validate_declared_bounds(
        &mut self,
        generics: &gossamer_ast::Generics,
        where_clause: &gossamer_ast::WhereClause,
        span: Span,
    ) {
        let mut declared: Vec<(String, Vec<String>)> = generics
            .params
            .iter()
            .filter_map(|param| match param {
                gossamer_ast::GenericParam::Type { name, bounds, .. } => {
                    Some((name.name.clone(), bound_names(bounds)))
                }
                _ => None,
            })
            .collect();
        for predicate in &where_clause.predicates {
            let Some(name) = bare_path_type_name(&predicate.bounded) else {
                continue;
            };
            declared.push((name.to_string(), bound_names(&predicate.bounds)));
        }
        for (param, bounds) in declared {
            for bound in bounds {
                if bound.is_empty()
                    || self.declared_trait_names.contains(&bound)
                    || known_builtin_trait(&bound)
                {
                    continue;
                }
                self.emit(
                    TypeError::UnknownTraitBound {
                        param: param.clone(),
                        name: bound,
                    },
                    span,
                );
            }
        }
    }

    pub(super) fn register_fn_sig_anonymous(&mut self, decl: &FnDecl) {
        self.fn_sig_of(decl);
    }

    pub(super) fn fn_sig_of(&mut self, decl: &FnDecl) -> FnSig {
        // Enter the function's generic scope so a parameter / return type
        // that names a type parameter (`&T`) records a rigid `TyKind::Param`
        // slot rather than a fresh inference variable. The `Param` slots are
        // what per-call-site instantiation substitutes with fresh variables.
        let prior = self.enter_fn_generic_scope(&decl.generics);
        // A `where` predicate constrains the same parameters the angle
        // brackets introduce, so an associated-type projection written in
        // the signature resolves through either spelling.
        self.current_param_bounds = match self.current_impl_generics.clone() {
            Some(impl_generics) if !impl_generics.params.is_empty() => {
                let impl_where = self.current_impl_where.clone();
                Self::combined_param_bounds(
                    &impl_generics,
                    &impl_where,
                    &decl.generics,
                    &decl.where_clause,
                )
            }
            _ => Self::declared_param_bounds(&decl.generics, &decl.where_clause),
        };
        Self::assoc_bindings_of(
            &decl.generics,
            &decl.where_clause,
            &mut self.current_assoc_bindings,
        );
        let inputs: Vec<Ty> = decl
            .params
            .iter()
            .map(|param| self.param_ty(param))
            .collect();
        let output = match decl.ret.as_ref() {
            Some(ty) => self.type_from_ast(ty),
            None => self.tcx.unit(),
        };
        self.leave_generic_scope(prior);
        FnSig { inputs, output }
    }

    pub(super) fn param_ty(&mut self, param: &FnParam) -> Ty {
        match param {
            FnParam::Typed { ty, .. } => self.type_from_ast(ty),
            FnParam::Receiver(_) => self.fresh(),
        }
    }

    /// Checks every item of an `impl` block inside a scope where `Self`
    /// names the type being implemented, so a `-> Self` return and a
    /// `Self::Item` projection both land on it.
    pub(super) fn check_impl(&mut self, decl: &ImplDecl) {
        // The self type is lowered inside the impl's own generic scope, so
        // `impl<T: Shape> Wrapper<T>` records `Wrapper` at `Param(0)`. Field
        // reads off `self` then carry that rigid parameter, which is what
        // bound-method resolution keys on.
        let self_scope = self.enter_generic_scope(&decl.generics);
        let self_ty = self.type_from_ast(&decl.self_ty);
        self.leave_generic_scope(self_scope);
        let prev_self = self.current_self_ty.replace(self_ty);
        let prev_self_name = std::mem::replace(
            &mut self.current_self_ty_name,
            gossamer_ast::assoc::type_head_name(&decl.self_ty).map(ToString::to_string),
        );
        let prev_impl_generics = self.current_impl_generics.replace(decl.generics.clone());
        let prev_impl_where =
            std::mem::replace(&mut self.current_impl_where, decl.where_clause.clone());
        for impl_item in &decl.items {
            match impl_item {
                ImplItem::Fn(fn_decl) => {
                    self.reject_method_export(fn_decl);
                    self.check_fn(fn_decl);
                }
                ImplItem::Const { ty, value, .. } => {
                    let annotated = self.type_from_ast(ty);
                    let init = self.check_expr_expecting(value, Expectation::HasType(annotated));
                    self.unify(annotated, init, value.span);
                }
                ImplItem::Type { ty, .. } => {
                    self.type_from_ast(ty);
                }
            }
        }
        self.current_impl_generics = prev_impl_generics;
        self.current_impl_where = prev_impl_where;
        self.current_self_ty_name = prev_self_name;
        self.current_self_ty = prev_self;
    }

    /// Checks a trait's default bodies and the types of its associated
    /// declarations. `Self` stands for every implementor here, so a
    /// projection resolves through the trait rather than a concrete type.
    pub(super) fn check_trait(&mut self, decl: &gossamer_ast::TraitDecl) {
        let prev_trait = self.current_trait_name.replace(decl.name.name.clone());
        for trait_item in &decl.items {
            match trait_item {
                TraitItem::Fn(fn_decl) => {
                    self.reject_method_export(fn_decl);
                    self.check_fn(fn_decl);
                }
                TraitItem::Const { ty, default, .. } => {
                    let annotated = self.type_from_ast(ty);
                    if let Some(value) = default {
                        let init =
                            self.check_expr_expecting(value, Expectation::HasType(annotated));
                        self.unify(annotated, init, value.span);
                    }
                }
                TraitItem::Type { default, .. } => {
                    if let Some(ty) = default {
                        self.type_from_ast(ty);
                    }
                }
            }
        }
        self.current_trait_name = prev_trait;
    }

    pub(super) fn check_item(&mut self, item: &Item) {
        // SPEC §9: `#[allow(unused_result)]` on an item covers its body.
        let prior_allowed = self.unused_result_allowed;
        self.unused_result_allowed |= item.attrs.allows("unused_result");
        // An autoderive-spliced body belongs to the type it completes, and
        // reads every field of it regardless of where the splice landed.
        // The serde and reflection helpers carry the same meaning in their
        // `__`-prefixed names.
        let synthesized = item.attrs.has_word("gos_synthesized")
            || match &item.kind {
                ItemKind::Fn(decl) => is_compiler_generated(&decl.name.name),
                ItemKind::Struct(decl) => is_compiler_generated(&decl.name.name),
                ItemKind::Enum(decl) => is_compiler_generated(&decl.name.name),
                _ => false,
            };
        if synthesized {
            self.synthesized_depth += 1;
        }
        self.check_item_inner(item);
        if synthesized {
            self.synthesized_depth -= 1;
        }
        self.unused_result_allowed = prior_allowed;
    }

    pub(super) fn check_item_inner(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(decl) if decl.extern_abi.is_some() => self.check_foreign_decl(item, decl),
            ItemKind::Fn(decl) => {
                self.check_export_decl(decl);
                self.check_fn(decl);
            }
            ItemKind::Impl(decl) => self.check_impl(decl),
            ItemKind::Trait(decl) => self.check_trait(decl),
            ItemKind::Const(decl) => {
                let annotated = self.type_from_ast(&decl.ty);
                let static_string_reference = matches!(
                    self.tcx.kind_of(annotated),
                    TyKind::Ref {
                        inner,
                        mutability: Mutbl::Not,
                    } if matches!(self.tcx.kind_of(*inner), TyKind::String)
                ) && expr_is_static_string_value(&decl.value);
                if !static_string_reference {
                    self.reject_stored_reference_type(
                        annotated,
                        decl.ty.span,
                        "be stored in a constant",
                    );
                }
                let init = self.check_expr_expecting(&decl.value, Expectation::HasType(annotated));
                self.unify(annotated, init, decl.value.span);
            }
            ItemKind::Static(decl) => {
                let annotated = self.type_from_ast(&decl.ty);
                let static_string_reference = matches!(
                    self.tcx.kind_of(annotated),
                    TyKind::Ref {
                        inner,
                        mutability: Mutbl::Not,
                    } if matches!(self.tcx.kind_of(*inner), TyKind::String)
                ) && expr_is_static_string_value(&decl.value);
                if !static_string_reference {
                    self.reject_stored_reference_type(
                        annotated,
                        decl.ty.span,
                        "be stored in a static",
                    );
                }
                let init = self.check_expr_expecting(&decl.value, Expectation::HasType(annotated));
                self.unify(annotated, init, decl.value.span);
                if item.attrs.has_word(gossamer_ast::FOREIGN_STATIC_ATTR) {
                    self.check_foreign_static_decl(item, &decl.name.name, annotated, decl.ty.span);
                }
            }
            // Field types name the declaration's own generic parameters, so
            // they are read inside its scope, as its registration read them.
            ItemKind::Struct(decl) => {
                let scope = self.enter_generic_scope(&decl.generics);
                self.check_struct_body(&decl.body);
                self.leave_generic_scope(scope);
            }
            ItemKind::Enum(decl) => {
                let scope = self.enter_generic_scope(&decl.generics);
                for variant in &decl.variants {
                    self.check_struct_body(&variant.body);
                }
                self.leave_generic_scope(scope);
            }
            ItemKind::TypeAlias(decl) => {
                let _ = self.type_from_ast(&decl.ty);
            }
            ItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    self.current_module.push(decl.name.name.clone());
                    for nested in inner {
                        self.check_item(nested);
                    }
                    self.current_module.pop();
                }
            }
            ItemKind::AttrItem(_) => {}
        }
    }

    pub(super) fn check_struct_body(&mut self, body: &StructBody) {
        match body {
            StructBody::Named(fields) => {
                for field in fields {
                    let ty = self.type_from_ast(&field.ty);
                    self.reject_stored_reference_type(
                        ty,
                        field.ty.span,
                        "be stored in a struct field",
                    );
                }
            }
            StructBody::Tuple(fields) => {
                for field in fields {
                    let ty = self.type_from_ast(&field.ty);
                    self.reject_stored_reference_type(
                        ty,
                        field.ty.span,
                        "be stored in a tuple-struct field",
                    );
                }
            }
            StructBody::Unit => {}
        }
    }

    pub(super) fn check_fn(&mut self, decl: &FnDecl) {
        // Enter the function's generic scope so a parameter / return / body
        // type that names a type parameter (`&T`) records a rigid
        // `TyKind::Param`. Monomorphisation substitutes those `Param` slots,
        // and trait-method dispatch on a `T` receiver keys off them.
        // The bound table is built from the same parameter sequence as the
        // scope, so index `i` of one names index `i` of the other.
        let mut assoc_bindings = HashMap::new();
        let (prior_scope, bounds) = match self.current_impl_generics.clone() {
            Some(impl_g) if !impl_g.params.is_empty() => {
                let impl_where = self.current_impl_where.clone();
                let bounds = Self::combined_param_bounds(
                    &impl_g,
                    &impl_where,
                    &decl.generics,
                    &decl.where_clause,
                );
                Self::assoc_bindings_of(&impl_g, &impl_where, &mut assoc_bindings);
                (
                    self.enter_generic_scope_combined(&impl_g, &decl.generics),
                    bounds,
                )
            }
            _ => (
                self.enter_generic_scope(&decl.generics),
                Self::declared_param_bounds(&decl.generics, &decl.where_clause),
            ),
        };
        Self::assoc_bindings_of(&decl.generics, &decl.where_clause, &mut assoc_bindings);
        let prior_assoc_bindings =
            std::mem::replace(&mut self.current_assoc_bindings, assoc_bindings);
        let prior_bounds = std::mem::replace(&mut self.current_param_bounds, bounds);
        self.push_scope();
        for param in &decl.params {
            self.bind_fn_param(param);
            match param {
                FnParam::Typed { pattern, .. } => {
                    self.register_reference_parameter_origins(pattern);
                }
                FnParam::Receiver(
                    gossamer_ast::Receiver::RefShared | gossamer_ast::Receiver::RefMut,
                ) => {
                    if let Some(scope) = self.reference_origins.last_mut() {
                        scope.insert(Box::from("self"), Box::from("self"));
                    }
                }
                FnParam::Receiver(gossamer_ast::Receiver::Owned) => {}
            }
            if let FnParam::Typed { ty, .. } = param {
                let param_ty = self.type_from_ast(ty);
                if !matches!(
                    self.tcx.kind_of(param_ty),
                    TyKind::Ref { .. } | TyKind::FnPtr(_) | TyKind::FnTrait(_)
                ) {
                    self.reject_stored_reference_type(
                        param_ty,
                        ty.span,
                        "be nested inside an owned function parameter",
                    );
                }
            }
        }
        let declared_ret = decl.ret.as_ref().map(|ty| self.type_from_ast(ty));
        if let Some(ret) = declared_ret {
            let static_string_reference = matches!(
                self.tcx.kind_of(ret),
                TyKind::Ref {
                    inner,
                    mutability: Mutbl::Not,
                } if matches!(self.tcx.kind_of(*inner), TyKind::String)
            ) && decl
                .body
                .as_ref()
                .is_some_and(|body| expr_is_static_string_value(body));
            if !static_string_reference {
                self.reject_stored_reference_type(
                    ret,
                    decl.ret.as_ref().expect("declared return").span,
                    "escape through a function return",
                );
            }
        }
        if let Some(body) = &decl.body {
            self.check_fn_body(decl, body, declared_ret);
        }
        self.pop_scope();
        self.current_param_bounds = prior_bounds;
        self.current_assoc_bindings = prior_assoc_bindings;
        self.leave_generic_scope(prior_scope);
    }

    /// Checks one function body against the return type its signature
    /// declares, or against the unit a missing one answers.
    pub(super) fn check_fn_body(&mut self, decl: &FnDecl, body: &Expr, declared_ret: Option<Ty>) {
        let ret = declared_ret.unwrap_or_else(|| self.tcx.unit());
        self.reject_unscoped_spawns(decl, body);
        self.collect_write_arg_bindings(body);
        let prev_ret = self.current_fn_ret.replace(ret);
        // The declared return type flows into the body as its expectation
        // to constrain literals and conversions. Container identity does
        // not change: an array literal remains `[T; N]` even when `Vec<T>`
        // is expected, and unification reports the mismatch.
        let body_ty = if let Some(ret) = declared_ret {
            self.check_expr_expecting(body, Expectation::HasType(ret))
        } else {
            let body_ty = self.check_expr(body);
            if !self.unused_result_allowed && self.is_result_ty(body_ty) {
                self.emit(TypeError::DiscardedResult, body_value_span(body));
            } else {
                self.report_discarded_result(body, None);
            }
            self.check_undeclared_return(decl, body, body_ty);
            body_ty
        };
        self.finish_ffi_checks();
        self.current_fn_ret = prev_ret;
        // A declared `-> ()` says the discard is deliberate, so a body whose
        // tail computes a value is accepted and the value dropped - the same
        // shape a signature with no return type has, written out. Every
        // other declared return unifies with the body.
        let discards_tail = declared_ret.is_some_and(|declared| {
            matches!(
                self.tcx.kind(self.infer.resolve(self.tcx, declared)),
                Some(TyKind::Unit)
            )
        });
        if declared_ret.is_some() && !discards_tail {
            self.record_fn_item_coercion(body, ret);
            self.unify(ret, body_ty, body_value_span(body));
        }
    }
}

impl TypeChecker<'_> {
    /// GT0113 for an `#[export]` on a method, which C has no receiver for.
    fn reject_method_export(&mut self, decl: &FnDecl) {
        if decl.attrs.export_symbol(&decl.name.name).is_some() {
            self.emit_export_error(decl, "a method has no C symbol; export a free function");
        }
    }

    /// Checks an `#[export]` function: a free, non-generic function under a
    /// C identifier no other export uses, whose parameters and result the C
    /// ABI carries.
    fn check_export_decl(&mut self, decl: &FnDecl) {
        let name = decl.name.name.clone();
        let Some(symbol) = decl.attrs.export_symbol(&name) else {
            return;
        };
        if gossamer_resolve::cfg_target_family() == "wasm" {
            self.emit(
                TypeError::Foreign(crate::ForeignError::OnWasm { name: name.clone() }),
                decl.span,
            );
            return;
        }
        let is_identifier = symbol
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && symbol
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !is_identifier {
            let why = format!("the symbol `{symbol}` is not a C identifier");
            self.emit_export_error(decl, &why);
            return;
        }
        if symbol == "main" {
            self.emit_export_error(decl, "the symbol `main` is the C program's entry");
            return;
        }
        if !decl.generics.params.is_empty() {
            self.emit_export_error(decl, "a generic function has no single C entry");
            return;
        }
        if decl.is_comptime {
            self.emit_export_error(decl, "a `comptime fn` runs while compiling");
            return;
        }
        if let Some(other) = self.export_symbols.get(&symbol).cloned() {
            let why = format!("`{other}` already exports the symbol `{symbol}`");
            self.emit_export_error(decl, &why);
            return;
        }
        self.export_symbols.insert(symbol, name);
        for param in &decl.params {
            let FnParam::Typed { ty, .. } = param else {
                self.emit_export_error(decl, "a method has no C symbol; export a free function");
                return;
            };
            let lowered = self.type_from_ast(ty);
            if !self.c_value_ok(lowered) {
                let why = format!(
                    "the parameter type `{}` has no C representation",
                    self.render_public_ty(lowered)
                );
                self.emit_export_error(decl, &why);
            }
        }
        if let Some(ret) = &decl.ret {
            let lowered = self.type_from_ast(ret);
            if !matches!(self.tcx.kind_of(lowered), TyKind::Unit) && !self.c_value_ok(lowered) {
                let why = format!(
                    "the result type `{}` has no C representation",
                    self.render_public_ty(lowered)
                );
                self.emit_export_error(decl, &why);
            }
        }
    }

    fn emit_export_error(&mut self, decl: &FnDecl, why: &str) {
        self.emit(
            TypeError::Foreign(crate::ForeignError::Export {
                name: decl.name.name.clone(),
                why: why.to_string(),
            }),
            decl.span,
        );
    }

    /// Checks a C global declared `static NAME: T` in an extern block: `T`
    /// has a C layout, and the declaration is not active on wasm32.
    fn check_foreign_static_decl(
        &mut self,
        item: &Item,
        name: &str,
        ty: Ty,
        ty_span: gossamer_lex::Span,
    ) {
        if gossamer_resolve::cfg_target_family() == "wasm" {
            self.emit(
                TypeError::Foreign(crate::ForeignError::OnWasm {
                    name: name.to_string(),
                }),
                item.span,
            );
        }
        if let Some(pointee) = self.pointee_problem(ty) {
            self.emit(
                TypeError::Foreign(crate::ForeignError::PointerTarget {
                    ty: pointee,
                    context: format!("the foreign static `{name}` cannot be reached through it"),
                }),
                ty_span,
            );
        }
    }

    /// Checks a function declared in an `unsafe extern "C"` block: every
    /// parameter and the return type must have a C representation, and no
    /// effect attribute may claim anything about the native body.
    fn check_foreign_decl(&mut self, item: &Item, decl: &FnDecl) {
        let name = decl.name.name.clone();
        if gossamer_resolve::cfg_target_family() == "wasm" {
            self.emit(
                TypeError::Foreign(crate::ForeignError::OnWasm { name: name.clone() }),
                item.span,
            );
        }
        for attr in &item.attrs.outer {
            let Some(word) = attr.path.segments.last().map(|s| s.name.name.clone()) else {
                continue;
            };
            if matches!(
                word.as_str(),
                "pure" | "readonly" | "effect" | "blocking" | "nonblocking"
            ) {
                self.emit(
                    TypeError::Foreign(crate::ForeignError::EffectAttribute {
                        name: name.clone(),
                        attr: word,
                    }),
                    item.span,
                );
            }
        }
        let Some(def) = self.resolutions.definition_of(item.id) else {
            return;
        };
        let Some(sig) = self.fn_sigs.get(&def).cloned() else {
            return;
        };
        self.check_foreign_params(item, decl, &name, &sig.inputs);
        self.check_foreign_return(item, decl, name, sig.output);
    }

    /// GT0098, GT0104, GT0105, and GT0107 for the foreign function `name`'s
    /// parameters.
    fn check_foreign_params(&mut self, item: &Item, decl: &FnDecl, name: &str, inputs: &[Ty]) {
        for (index, (param, ty)) in decl.params.iter().zip(inputs.iter()).enumerate() {
            let span = match param {
                gossamer_ast::FnParam::Typed { ty, .. } => ty.span,
                gossamer_ast::FnParam::Receiver(_) => item.span,
            };
            if let Some(opaque) = self.foreign_opaque_by_value(*ty) {
                self.emit(
                    TypeError::Foreign(crate::ForeignError::OpaqueByValue {
                        name: opaque,
                        context: format!("parameter {} of `{name}` takes one by value", index + 1),
                    }),
                    span,
                );
                continue;
            }
            let peeled = self.peel_mut_ref(*ty);
            if let Some(Err(pointee)) = self.foreign_pointer_form(peeled) {
                self.emit(
                    TypeError::Foreign(crate::ForeignError::PointerTarget {
                        ty: pointee,
                        context: format!("parameter {} of `{name}` cannot point at it", index + 1),
                    }),
                    span,
                );
                continue;
            }
            if let Some(why) = self.callback_problem(*ty) {
                let callback = self.render_public_ty(*ty);
                self.emit(
                    TypeError::Foreign(crate::ForeignError::CallbackSignature {
                        name: name.to_string(),
                        ty: callback,
                        why,
                    }),
                    span,
                );
                continue;
            }
            if let Some(why) = self.foreign_param_problem(*ty) {
                let ty = self.render_public_ty(*ty);
                self.emit(
                    TypeError::Foreign(crate::ForeignError::SignatureType {
                        name: name.to_string(),
                        position: format!("parameter {}", index + 1),
                        ty,
                        why,
                    }),
                    span,
                );
            }
        }
    }

    /// GT0098, GT0104, and GT0105 for the foreign function `name`'s result.
    fn check_foreign_return(&mut self, item: &Item, decl: &FnDecl, name: String, output: Ty) {
        let ret = self.infer.resolve(self.tcx, output);
        let span = decl.ret.as_ref().map_or(item.span, |ty| ty.span);
        if let Some(opaque) = self.foreign_opaque_by_value(ret) {
            self.emit(
                TypeError::Foreign(crate::ForeignError::OpaqueByValue {
                    name: opaque,
                    context: format!("`{name}` answers one by value"),
                }),
                span,
            );
            return;
        }
        match self.foreign_pointer_form(ret) {
            Some(Ok(())) => return,
            Some(Err(ty)) => {
                self.emit(
                    TypeError::Foreign(crate::ForeignError::PointerTarget {
                        ty,
                        context: format!("`{name}` cannot answer a pointer to it"),
                    }),
                    span,
                );
                return;
            }
            None => {}
        }
        let plain_struct = match self.tcx.kind(ret).cloned() {
            Some(TyKind::Adt { def, substs }) => {
                (self.repr_c_structs.contains(&def)
                    || self.tcx.union_members(def, &substs).is_some())
                    && self.tcx.c_leaves(ret).is_some()
            }
            _ => false,
        };
        if !matches!(self.tcx.kind(ret), Some(TyKind::Unit))
            && !self.foreign_scalar(ret)
            && !plain_struct
        {
            let ty = self.render_public_ty(ret);
            self.emit(
                TypeError::Foreign(crate::ForeignError::SignatureType {
                    name,
                    position: "the return type".to_string(),
                    ty,
                    why: "a foreign function answers `()`, a scalar (an integer up to 64 \
                          bits, `bool`, `f32`, or `f64`), an `ffi::Ptr`, an \
                          `Option<ffi::Ptr>`, or a `#[repr(C)]` plain-data struct"
                        .to_string(),
                }),
                span,
            );
        }
    }

    /// The foreign type `ty` passes by value, directly or behind `&mut`.
    fn foreign_opaque_by_value(&mut self, ty: Ty) -> Option<String> {
        let ty = self.infer.resolve(self.tcx, ty);
        let inner = match self.tcx.kind(ty) {
            Some(TyKind::Ref { inner, .. }) => *inner,
            _ => ty,
        };
        let (def, name) = match self.tcx.kind(self.infer.resolve(self.tcx, inner)).cloned() {
            Some(TyKind::Adt { def, .. }) => (def, self.tcx.def_name(def)?.to_string()),
            _ => return None,
        };
        self.opaque_types
            .contains(&def)
            .then(|| name.rsplit("::").next().unwrap_or(&name).to_string())
    }

    /// Whether `ty` crosses the C boundary as a scalar in a register.
    pub(super) fn foreign_scalar(&self, ty: Ty) -> bool {
        self.tcx.c_scalar_class(ty).is_some()
    }

    /// `ty` with one `&mut` peeled.
    fn peel_mut_ref(&mut self, ty: Ty) -> Ty {
        let ty = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(ty) {
            Some(TyKind::Ref {
                mutability: Mutbl::Mut,
                inner,
            }) => *inner,
            _ => ty,
        }
    }

    /// Why `ty` cannot be a foreign function's parameter, or `None` when it
    /// can.
    fn foreign_param_problem(&mut self, ty: Ty) -> Option<String> {
        let ty = self.infer.resolve(self.tcx, ty);
        if self.foreign_scalar(ty)
            || self.foreign_pointer_form(ty).is_some()
            || matches!(
                self.tcx.kind(ty),
                Some(TyKind::FnPtr(_) | TyKind::FnTrait(_))
            )
        {
            return None;
        }
        // `&mut` a scalar or a pointer form is an out-parameter: `int *`,
        // `T **`.
        if let Some(TyKind::Ref {
            mutability: Mutbl::Mut,
            inner,
        }) = self.tcx.kind(ty).cloned()
        {
            let inner = self.infer.resolve(self.tcx, inner);
            if self.foreign_scalar(inner) || self.foreign_pointer_form(inner).is_some() {
                return None;
            }
        }
        // A `[T]` view parameter is carried as a shared reference.
        let (inner, mutable) = match self.tcx.kind(ty) {
            Some(TyKind::Ref { mutability, inner }) => (
                self.infer.resolve(self.tcx, *inner),
                *mutability == Mutbl::Mut,
            ),
            _ => (ty, false),
        };
        match self.tcx.kind(inner).cloned() {
            Some(TyKind::Slice(elem)) => {
                let elem = self.infer.resolve(self.tcx, elem);
                let plain_struct = match self.tcx.kind(elem).cloned() {
                    Some(TyKind::Adt { def, substs }) => {
                        (self.repr_c_structs.contains(&def)
                            || self.tcx.union_members(def, &substs).is_some())
                            && self.tcx.c_leaves(elem).is_some()
                    }
                    _ => false,
                };
                if self.foreign_scalar(elem) || plain_struct {
                    None
                } else {
                    Some(
                        "a slice crosses the C boundary as a pointer to its first element, so \
                         its elements are scalars (integers up to 64 bits, `bool`, `f32`, or \
                         `f64`) or `#[repr(C)]` plain-data structs"
                            .to_string(),
                    )
                }
            }
            Some(TyKind::Adt { def, .. }) if self.repr_c_structs.contains(&def) => {
                if self.tcx.c_leaves(inner).is_some() {
                    None
                } else {
                    Some(
                        "a struct crosses the C boundary only when every field is plain data \
                         with a C type: integers up to 64 bits, `bool`, `f32`, `f64`, \
                         `ffi::Ptr` (`Ptr::null()` where C expects NULL), `ffi::Union`, fixed \
                         arrays of those, and other such `#[repr(C)]` structs"
                            .to_string(),
                    )
                }
            }
            // A union crosses by value as C passes one: classified over
            // every member's bytes.
            Some(TyKind::Adt { def, substs })
                if self.tcx.union_members(def, &substs).is_some()
                    && self.tcx.c_leaves(inner).is_some() =>
            {
                None
            }
            Some(TyKind::Adt { .. }) => {
                Some("declare the struct `#[repr(C)]` so its fields take the C layout".to_string())
            }
            _ if mutable => Some(
                "a `&mut` parameter is `&mut` a scalar, an `ffi::Ptr`, or an \
                 `Option<ffi::Ptr>` (an out-parameter), `&mut [T]` of scalars or structs, or \
                 `&mut` a `#[repr(C)]` plain-data struct (a pointer whose writes come back)"
                    .to_string(),
            ),
            _ => Some(
                "a parameter is a scalar (an integer up to 64 bits, `bool`, `f32`, `f64`), an \
                 `ffi::Ptr` or `Option<ffi::Ptr>`, a C function type `fn(..) -> R`, a slice \
                 `[T]` of scalars or `#[repr(C)]` structs, a `#[repr(C)]` plain-data struct \
                 (passed by value), or `&mut` one of these"
                    .to_string(),
            ),
        }
    }

    /// Rejects a call to a foreign function outside an `unsafe { }` block.
    pub(super) fn check_foreign_call_site(&mut self, callee: &Expr) {
        if self.unsafe_depth > 0 {
            return;
        }
        let ExprKind::Path(_) = &callee.kind else {
            return;
        };
        let Some(Resolution::Def { def, .. }) = self.resolutions.get(callee.id) else {
            return;
        };
        if let Some(name) = self.foreign_fns.get(&def).cloned() {
            self.emit(
                TypeError::Foreign(crate::ForeignError::CallOutsideUnsafe { name }),
                callee.span,
            );
        }
    }
}
