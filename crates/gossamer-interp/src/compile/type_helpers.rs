#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

const HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
const BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;

impl<'tcx> FnBuilder<'tcx> {
    /// Classifies an expression's natural result kind from its
    /// HIR `Ty`. Unknown / aggregate / polymorphic types stay in
    /// `Value`; `f64` → `F64`, all integer types → `I64`. When
    /// the type is unknown (an unresolved / error type) we default to
    /// `Value` so the generic path handles it.
    pub(crate) fn expr_kind(&self, expr: &HirExpr) -> RegKind {
        match self.tcx.kind(expr.ty) {
            Some(TyKind::Float(FloatTy::F64)) => RegKind::F64,
            Some(TyKind::Int(_)) => RegKind::I64,
            _ => RegKind::Value,
        }
    }

    /// If `ty` is a named struct whose layout is known, returns
    /// the offset of `field_name` within its declaration-order
    /// field list. Used to fold field reads/writes to the
    /// `ByOffset` op variants that skip the runtime name scan.
    pub(crate) fn resolve_struct_field_offset(
        &self,
        ty: gossamer_types::Ty,
        field_name: &str,
    ) -> Option<u16> {
        let ty = self.unwrap_ref(ty);
        let kind = self.tcx.kind(ty)?;
        let def = match kind {
            TyKind::Adt { def, .. } => *def,
            _ => return None,
        };
        let fields = self.layouts.get(&def)?;
        let idx = fields.iter().position(|f| f == field_name)?;
        u16::try_from(idx).ok()
    }

    /// Peels any `&T` / `&mut T` reference layers off a `Ty`,
    /// so type-directed optimisations work through reference
    /// binders (`fn energy(b: &[Body; 5])`).
    pub(crate) fn unwrap_ref(&self, mut ty: gossamer_types::Ty) -> gossamer_types::Ty {
        loop {
            match self.tcx.kind(ty) {
                Some(TyKind::Ref { inner, .. }) => ty = *inner,
                _ => return ty,
            }
        }
    }

    /// True when `expr` has a set sentinel `Adt` type, seeing through
    /// references.
    /// Used by the for-loop fast path to snapshot a bare set to a Vec.
    pub(crate) fn expr_is_hashset(&self, expr: &HirExpr) -> bool {
        matches!(
            self.tcx.kind(self.unwrap_ref(expr.ty)),
            Some(TyKind::Adt { def, .. })
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL)
        )
    }

    /// True when `expr` is a `Deque`, `Queue`, `Stack`, `MinHeap`, or
    /// `MaxHeap`. Each reaches its
    /// elements through a registry handle, so a binding taken from one has to
    /// copy the elements rather than the handle - exactly as a `Set` does.
    pub(crate) fn expr_is_slot_container(&self, expr: &HirExpr) -> bool {
        const VEC_DEQUE_DEF_LOCAL: u32 = u32::MAX - 19;
        const BINARY_HEAP_DEF_LOCAL: u32 = u32::MAX - 28;
        const MIN_HEAP_DEF_LOCAL: u32 = u32::MAX - 30;
        const VEC_QUEUE_DEF_LOCAL: u32 = u32::MAX - 31;
        const VEC_STACK_DEF_LOCAL: u32 = u32::MAX - 32;
        matches!(
            self.tcx.kind(self.unwrap_ref(expr.ty)),
            Some(TyKind::Adt { def, .. })
                if matches!(
                    def.local,
                    VEC_DEQUE_DEF_LOCAL
                        | VEC_QUEUE_DEF_LOCAL
                        | VEC_STACK_DEF_LOCAL
                        | BINARY_HEAP_DEF_LOCAL
                        | MIN_HEAP_DEF_LOCAL
                )
        )
    }

    /// True when `expr` is a `HashMap` or `BTreeMap`. Both surface types
    /// share `TyKind::HashMap` and the same runtime storage.
    pub(crate) fn expr_is_map(&self, expr: &HirExpr) -> bool {
        matches!(
            self.tcx.kind(self.unwrap_ref(expr.ty)),
            Some(TyKind::HashMap { .. })
        )
    }

    /// Whether `expr` is a by-value struct or tuple holding, at any nesting
    /// depth, a field that reaches its storage through a handle carrying no
    /// count of its holders: a `Map`, a `Set` / `BTreeSet`, a `Deque` /
    /// `Queue` / `Stack`, or a `MinHeap` / `MaxHeap`. A plain register copy of
    /// such a value would share that field's storage between the binding and
    /// its source, so the binding takes a deep clone the way a bare container
    /// binding does.
    pub(crate) fn expr_is_aggregate_with_container(&self, expr: &HirExpr) -> bool {
        self.ty_holds_shared_container(self.unwrap_ref(expr.ty), 0)
    }

    /// The register a function answers with, cloned when the answer is a map
    /// read straight out of an aggregate's field.
    ///
    /// The caller keeps the struct the field belongs to, so the value handed
    /// back has to be one of its own - exactly what a `let` binding of the
    /// same field takes. A bare local or parameter is not one: its own copy
    /// discipline already ran where it was bound.
    pub(crate) fn cloned_returned_field_container(
        &mut self,
        expr: &gossamer_hir::HirExpr,
        reg: Reg,
    ) -> Reg {
        use gossamer_hir::HirExprKind;
        if !matches!(
            expr.kind,
            HirExprKind::Field { .. } | HirExprKind::TupleIndex { .. }
        ) {
            return reg;
        }
        if !self.expr_is_map(expr) {
            return reg;
        }
        let dst = self.alloc_reg();
        self.emit(crate::bytecode::Op::CloneMapLike { dst, src: reg });
        dst
    }

    fn ty_holds_shared_container(&self, ty: gossamer_types::Ty, depth: u32) -> bool {
        const HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
        const BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;
        const VEC_DEQUE_DEF_LOCAL: u32 = u32::MAX - 19;
        const BINARY_HEAP_DEF_LOCAL: u32 = u32::MAX - 28;
        const MIN_HEAP_DEF_LOCAL: u32 = u32::MAX - 30;
        const VEC_QUEUE_DEF_LOCAL: u32 = u32::MAX - 31;
        const VEC_STACK_DEF_LOCAL: u32 = u32::MAX - 32;
        if depth > 8 {
            return false;
        }
        match self.tcx.kind(ty) {
            Some(TyKind::HashMap { .. }) => true,
            Some(TyKind::Adt { def, .. })
                if matches!(
                    def.local,
                    HASH_SET_DEF_LOCAL
                        | BTREE_SET_DEF_LOCAL
                        | VEC_DEQUE_DEF_LOCAL
                        | VEC_QUEUE_DEF_LOCAL
                        | VEC_STACK_DEF_LOCAL
                        | BINARY_HEAP_DEF_LOCAL
                        | MIN_HEAP_DEF_LOCAL
                ) =>
            {
                true
            }
            Some(TyKind::Adt { def, .. }) => {
                self.tcx.struct_field_tys(*def).is_some_and(|fields| {
                    fields
                        .to_vec()
                        .iter()
                        .any(|f| self.ty_holds_shared_container(*f, depth + 1))
                })
            }
            Some(TyKind::Tuple(elems)) => elems
                .clone()
                .iter()
                .any(|e| self.ty_holds_shared_container(*e, depth + 1)),
            // A fixed array or vector reaches its elements the way a tuple
            // reaches its own, so one holding a table needs the same deep
            // copy - the compiled tiers give a cloned vector's map elements
            // tables of their own for the same reason.
            Some(TyKind::Array { elem, .. } | TyKind::Vec(elem)) => {
                self.ty_holds_shared_container(*elem, depth + 1)
            }
            _ => false,
        }
    }

    /// Bare type name to dispatch a struct `==` / `!=` through its
    /// derived `<Type>::eq` method, seeing through `&` / `&mut`. Returns
    /// `Some` only for a *struct* whose layout the compiler knows (an
    /// entry in `layouts`); enums return `None` so they compare
    /// structurally via the native `Op::Eq` (only `Value::Struct` routes
    /// through `Type::eq`). Mirrors the MIR builder's `adt_dispatch_name`
    /// for the struct case.
    pub(crate) fn struct_eq_dispatch_name(&self, ty: gossamer_types::Ty) -> Option<String> {
        let ty = self.unwrap_ref(ty);
        let TyKind::Adt { def, .. } = self.tcx.kind(ty)? else {
            return None;
        };
        if !self.layouts.contains_key(def) {
            return None;
        }
        // The synthesized `eq` registers under the type's identity, which
        // carries the modules containing it, so two modules may each declare
        // the name.
        if let Some(registered) = self.tcx.def_name(*def)
            && !registered.is_empty()
            && !registered.starts_with("adt#")
        {
            return Some(registered.to_string());
        }
        let rendered = gossamer_types::printer::render_ty(self.tcx, ty);
        let bare = rendered.rsplit("::").next().unwrap_or(&rendered);
        // Drop any generic-argument suffix (`Wrap<i64>` -> `Wrap`).
        let bare = bare.split('<').next().unwrap_or(bare);
        if bare.is_empty() || bare.starts_with("adt#") {
            return None;
        }
        Some(bare.to_string())
    }

    /// `true` when `ty` resolves (through `&` / `&mut` layers) to a
    /// nominal `Adt` (struct or enum). Used to confirm a `&mut self`
    /// receiver is an aggregate before marking it for the write-back
    /// cell protocol.
    pub(crate) fn is_adt_ref(&self, ty: gossamer_types::Ty) -> bool {
        matches!(self.tcx.kind(self.unwrap_ref(ty)), Some(TyKind::Adt { .. }))
    }

    /// Bare nominal name of an `Adt` value's type (`Counter`,
    /// `Stack` for a `Stack<i64>`), seeing through `&` / `&mut`. Used to
    /// reconstruct the `Type::method` key a `&mut self` call dispatches
    /// to, so the call site can consult [`FnBuilder::method_muts`].
    /// Returns `None` for non-`Adt` receivers (primitives, collections)
    /// and synthesized anonymous types.
    pub(crate) fn adt_type_name(&self, ty: gossamer_types::Ty) -> Option<String> {
        let ty = self.unwrap_ref(ty);
        let TyKind::Adt { .. } = self.tcx.kind(ty)? else {
            return None;
        };
        let rendered = gossamer_types::printer::render_ty(self.tcx, ty);
        let bare = rendered.rsplit("::").next().unwrap_or(&rendered);
        let bare = bare.split('<').next().unwrap_or(bare);
        if bare.is_empty() || bare.starts_with("adt#") {
            return None;
        }
        Some(bare.to_string())
    }

    /// Bare name an `impl` block would spell for `ty`'s self type, seeing
    /// through `&` / `&mut`: `Counter` for a user struct, `i64` for an
    /// `impl Trait for i64`, `Vec` for `impl Trait for Vec<T>`. Paired
    /// with a method name it reconstructs the `Type::method` key both
    /// [`FnBuilder::method_muts`] and the global table use, so dispatch
    /// resolves against the receiver's own type rather than by name alone.
    pub(crate) fn impl_target_name(&self, ty: gossamer_types::Ty) -> Option<String> {
        self.impl_target_names(ty).into_iter().next()
    }

    /// Every name an `impl` for `ty` may be keyed under, most qualified
    /// first: the type's full module-qualified identity, then its bare
    /// name.
    ///
    /// A type declared in a module is identified by its qualified name, so
    /// an `impl` inside that module keys its methods there. Reading only the
    /// bare tail would miss them, and a `&mut self` method that is missed is
    /// called without the write-back protocol - its mutation of `self` never
    /// reaches the caller.
    pub(crate) fn impl_target_names(&self, ty: gossamer_types::Ty) -> Vec<String> {
        let ty = self.unwrap_ref(ty);
        if self.tcx.kind(ty).is_none() {
            return Vec::new();
        }
        let rendered = gossamer_types::printer::render_ty(self.tcx, ty);
        let unparameterized = rendered.split('<').next().unwrap_or(&rendered);
        let bare = unparameterized
            .rsplit("::")
            .next()
            .unwrap_or(unparameterized);
        if bare.is_empty() || bare.starts_with("adt#") {
            return Vec::new();
        }
        if unparameterized == bare {
            return vec![bare.to_string()];
        }
        vec![unparameterized.to_string(), bare.to_string()]
    }

    /// `true` when `ty` names no concrete type here - an inference variable,
    /// a generic parameter, or an error - so a method call on it cannot be
    /// resolved by the receiver's own surface and the one uniquely-named
    /// candidate is all there is to bind to.
    pub(crate) fn ty_is_unresolved(&self, ty: gossamer_types::Ty) -> bool {
        let ty = self.unwrap_ref(ty);
        match self.tcx.kind(ty) {
            None => true,
            Some(kind) => matches!(kind, TyKind::Var(_) | TyKind::Param { .. } | TyKind::Error),
        }
    }

    /// Bare name of the `Ok` payload type `B` of a `Result<B, E>` (the result
    /// type of `x.try_into()`), so the call can route to `B::try_from(x)`.
    pub(crate) fn result_ok_adt_name(&self, ty: gossamer_types::Ty) -> Option<String> {
        let ty = self.unwrap_ref(ty);
        let TyKind::Adt { substs, .. } = self.tcx.kind(ty)? else {
            return None;
        };
        let b_ty = substs.types().first().copied()?;
        self.adt_type_name(b_ty)
    }

    /// `true` when `ty` (through `&` / `&mut` layers) is an array, vec,
    /// slice, or tuple - a collection the for-loop fast path can drive by
    /// index via `len()` + `IndexGet`. User `impl Iterator` types (`Adt`)
    /// are excluded so their stateful `next()` keeps its own desugar.
    /// `true` when `expr` has type `&mut [T]` / `&mut Vec<T>` - a
    /// reference whose elements are written through to their source.
    pub(crate) fn expr_is_mut_ref_collection(&self, expr: &HirExpr) -> bool {
        let Some(TyKind::Ref { mutability, inner }) = self.tcx.kind(expr.ty) else {
            return false;
        };
        *mutability == gossamer_types::Mutbl::Mut && self.is_indexable_collection_ty(*inner)
    }

    pub(crate) fn is_indexable_collection_ty(&self, ty: gossamer_types::Ty) -> bool {
        let ty = self.unwrap_ref(ty);
        matches!(
            self.tcx.kind(ty),
            Some(TyKind::Array { .. } | TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Tuple(_))
        )
    }

    /// Collects the names a pattern binds to an array / vec / slice
    /// value, deriving each binding's type from the resolved `scrut_ty`
    /// (the type of the value the pattern matches). The for-loop fast path
    /// uses the resulting `collection_locals` tags to drive `for x in
    /// <binding>` by index even when the binding's own HIR type stayed an
    /// inference var.
    pub(crate) fn collect_collection_binding_names(
        &self,
        pat: &HirPat,
        scrut_ty: Option<gossamer_types::Ty>,
        out: &mut Vec<String>,
    ) {
        match &pat.kind {
            HirPatKind::Binding { name, .. } => {
                if scrut_ty.is_some_and(|t| self.is_indexable_collection_ty(t)) {
                    out.push(name.name.clone());
                }
            }
            HirPatKind::At { name, sub, .. } => {
                if scrut_ty.is_some_and(|t| self.is_indexable_collection_ty(t)) {
                    out.push(name.name.clone());
                }
                self.collect_collection_binding_names(sub, scrut_ty, out);
            }
            HirPatKind::Variant { name, fields } => {
                for (i, fp) in fields.iter().enumerate() {
                    let fty = self.variant_payload_ty(scrut_ty, name.name.as_str(), i);
                    self.collect_collection_binding_names(fp, fty, out);
                }
            }
            HirPatKind::Struct { fields, .. } => {
                for f in fields {
                    let fty = self.struct_pat_field_ty(scrut_ty, f.name.name.as_str());
                    match &f.pattern {
                        Some(p) => self.collect_collection_binding_names(p, fty, out),
                        None => {
                            if fty.is_some_and(|t| self.is_indexable_collection_ty(t)) {
                                out.push(f.name.name.clone());
                            }
                        }
                    }
                }
            }
            HirPatKind::Tuple(parts) => {
                for (i, p) in parts.iter().enumerate() {
                    let ety = self.tuple_elem_ty(scrut_ty, i);
                    self.collect_collection_binding_names(p, ety, out);
                }
            }
            HirPatKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let ety = scrut_ty.and_then(|t| self.array_elem_ty(t));
                for p in prefix {
                    self.collect_collection_binding_names(p, ety, out);
                }
                if let Some(rest) = rest {
                    // The `..rest` sub-slice has the same indexable
                    // collection type as the scrutinee.
                    self.collect_collection_binding_names(rest, scrut_ty, out);
                }
                for p in suffix {
                    self.collect_collection_binding_names(p, ety, out);
                }
            }
            HirPatKind::Ref { inner, .. } => {
                let inner_ty = scrut_ty.map(|t| self.unwrap_ref(t));
                self.collect_collection_binding_names(inner, inner_ty, out);
            }
            HirPatKind::Or(alts) => {
                for alt in alts {
                    self.collect_collection_binding_names(alt, scrut_ty, out);
                }
            }
            HirPatKind::Wildcard
            | HirPatKind::Rest
            | HirPatKind::Literal(_)
            | HirPatKind::Range { .. } => {}
        }
    }

    /// Payload type of a matched `Option` / `Result` variant field. Only
    /// these two generic enums are resolved here (their substitution maps
    /// directly to the variant payload); other enums return `None`.
    fn variant_payload_ty(
        &self,
        scrut_ty: Option<gossamer_types::Ty>,
        variant: &str,
        idx: usize,
    ) -> Option<gossamer_types::Ty> {
        let ty = self.unwrap_ref(scrut_ty?);
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        let types = substs.types();
        // `Option<T>` (sentinel def `u32::MAX - 1`): `Some(T)` → `types[0]`.
        if def.local == u32::MAX - 1 {
            return types.get(idx).copied();
        }
        // `Result<T, E>` (sentinel def `u32::MAX`): `Ok` → `T`, `Err` → `E`.
        if def.local == u32::MAX {
            return match variant {
                "Ok" => types.first().copied(),
                "Err" => types.get(1).copied(),
                _ => None,
            };
        }
        None
    }

    /// Element type at `idx` of a tuple-typed value, if known.
    fn tuple_elem_ty(
        &self,
        scrut_ty: Option<gossamer_types::Ty>,
        idx: usize,
    ) -> Option<gossamer_types::Ty> {
        let ty = self.unwrap_ref(scrut_ty?);
        let TyKind::Tuple(elems) = self.tcx.kind(ty)? else {
            return None;
        };
        elems.get(idx).copied()
    }

    /// Declared type of struct field `field_name` on a struct-typed value,
    /// if the struct's layout is known (non-generic resolution).
    fn struct_pat_field_ty(
        &self,
        scrut_ty: Option<gossamer_types::Ty>,
        field_name: &str,
    ) -> Option<gossamer_types::Ty> {
        let ty = self.unwrap_ref(scrut_ty?);
        let TyKind::Adt { def, substs } = self.tcx.kind(ty)? else {
            return None;
        };
        let names = self.layouts.get(def)?;
        let idx = names.iter().position(|f| f == field_name)?;
        self.tcx.adt_field_tys(*def, substs)?.get(idx).copied()
    }

    /// `true` when the `for`-loop receiver `expr` is an indexable
    /// collection - either by its resolved type or, for a pattern-bound
    /// local whose inferred type stayed an unresolved var, by the
    /// `collection_locals` tag recorded at bind time.
    /// True when `expr` names the `__for_iter` state binding the `for`
    /// desugar introduces for an iterable with no home of its own.
    pub(crate) fn is_for_iter_state_path(expr: &HirExpr) -> bool {
        matches!(
            &expr.kind,
            HirExprKind::Path { segments, .. }
                if matches!(segments.as_slice(), [seg] if seg.name == "__for_iter")
        )
    }

    pub(crate) fn receiver_is_collection(&self, expr: &HirExpr) -> bool {
        if self.is_indexable_collection_ty(expr.ty) {
            return true;
        }
        if let HirExprKind::Path { segments, .. } = &expr.kind {
            if segments.len() == 1 {
                if let Some(tr) = self.lookup_local(&segments[0].name) {
                    return self.collection_locals.contains(&tr.reg);
                }
            }
        }
        false
    }

    /// Returns whether `expr` is a stateful, single-pass `Iterator<T>`, using
    /// binding provenance when the expression's HIR type remained unresolved.
    pub(crate) fn receiver_is_lazy_iterator(&self, expr: &HirExpr) -> bool {
        if matches!(
            self.tcx.kind(self.unwrap_ref(expr.ty)),
            Some(TyKind::Iterator(_))
        ) {
            return true;
        }
        if let HirExprKind::Path { segments, .. } = &expr.kind
            && let [segment] = segments.as_slice()
            && let Some(tr) = self.lookup_local(&segment.name)
        {
            return self.lazy_iterator_locals.contains(&tr.reg);
        }
        false
    }

    /// Returns the element type of an array / vec / slice / tuple,
    /// peeling reference layers first.
    pub(crate) fn array_elem_ty(&self, ty: gossamer_types::Ty) -> Option<gossamer_types::Ty> {
        let ty = self.unwrap_ref(ty);
        match self.tcx.kind(ty) {
            Some(TyKind::Array { elem, .. } | TyKind::Vec(elem) | TyKind::Slice(elem)) => {
                Some(*elem)
            }
            Some(TyKind::Tuple(elems)) => elems.first().copied(),
            _ => None,
        }
    }

    /// Returns `true` when `ty` resolves to `HashMap<i64, i64>`,
    /// the typed shape that rides through `Value::IntMap`. The
    /// resolver may already have erased one or both of the
    /// generic args when the inference variable couldn't be
    /// pinned; callers fall back to the boxed `Value::Map` in
    /// that case rather than risk a typed op crashing on a
    /// non-`i64` payload.
    pub(crate) fn is_int_map_ty(&self, ty: gossamer_types::Ty) -> bool {
        let ty = self.unwrap_ref(ty);
        let Some(TyKind::HashMap { key, value, .. }) = self.tcx.kind(ty) else {
            return false;
        };
        let key_is_i64 = matches!(
            self.tcx.kind(*key),
            Some(TyKind::Int(IntTy::I64 | IntTy::Isize | IntTy::Usize))
        );
        let value_is_i64 = matches!(
            self.tcx.kind(*value),
            Some(TyKind::Int(IntTy::I64 | IntTy::Isize | IntTy::Usize))
        );
        key_is_i64 && value_is_i64
    }

    /// Returns `true` when `ty` resolves to `HashMap<String, i64>`,
    /// the typed shape that rides through `Value::StrIntMap`. Like
    /// [`Self::is_int_map_ty`], a partially-erased generic falls back
    /// to the boxed `Value::Map` rather than risk a typed op on a
    /// non-matching payload.
    pub(crate) fn is_str_int_map_ty(&self, ty: gossamer_types::Ty) -> bool {
        let ty = self.unwrap_ref(ty);
        let Some(TyKind::HashMap { key, value, .. }) = self.tcx.kind(ty) else {
            return false;
        };
        let key_is_string = matches!(self.tcx.kind(*key), Some(TyKind::String));
        let value_is_i64 = matches!(
            self.tcx.kind(*value),
            Some(TyKind::Int(IntTy::I64 | IntTy::Isize | IntTy::Usize))
        );
        key_is_string && value_is_i64
    }
    /// Whether `body` writes through `name`: reassigning it, writing a field
    /// or element of it, or calling a method on it that mutates its receiver.
    pub(crate) fn name_is_written(&self, body: &HirExpr, name: &str) -> bool {
        let mut found = false;
        walk_hir_expr(body, &mut |expr| match &expr.kind {
            HirExprKind::Assign { place, .. } if hir_place_root_is(place, name) => found = true,
            HirExprKind::MethodCall {
                receiver,
                name: method,
                ..
            } if hir_place_root_is(receiver, name)
                && self.method_writes_receiver(receiver, method) =>
            {
                found = true;
            }
            _ => {}
        });
        found
    }

    /// Whether a call to `method` on `receiver` writes through its receiver: a
    /// container mutator, or a user method declared `&mut self`.
    fn method_writes_receiver(&self, receiver: &HirExpr, method: &Ident) -> bool {
        if gossamer_types::is_mutating_method_name(method.name.as_str()) {
            return true;
        }
        self.impl_target_names(receiver.ty)
            .into_iter()
            .any(|target| {
                self.method_muts
                    .contains(&format!("{target}::{}", method.name))
            })
    }
}

/// Whether the place `expr` names is rooted at the binding `name`.
fn hir_place_root_is(expr: &HirExpr, name: &str) -> bool {
    let mut cur = expr;
    loop {
        match &cur.kind {
            HirExprKind::Path { segments, .. } => {
                return matches!(segments.as_slice(), [seg] if seg.name == name);
            }
            HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
                cur = receiver;
            }
            HirExprKind::Index { base, .. } => cur = base,
            HirExprKind::Unary { operand, .. } => cur = operand,
            _ => return false,
        }
    }
}

/// Applies `f` to `expr` and every expression nested inside it.
fn walk_hir_expr(expr: &HirExpr, f: &mut impl FnMut(&HirExpr)) {
    use gossamer_hir::{HirArrayExpr, HirSelectOp, HirStmtKind};
    f(expr);
    match &expr.kind {
        HirExprKind::Literal(_)
        | HirExprKind::Path { .. }
        | HirExprKind::Placeholder
        | HirExprKind::Continue { .. } => {}
        HirExprKind::Call { callee, args } => {
            walk_hir_expr(callee, f);
            for a in args {
                walk_hir_expr(a, f);
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            walk_hir_expr(receiver, f);
            for a in args {
                walk_hir_expr(a, f);
            }
        }
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            walk_hir_expr(receiver, f);
        }
        HirExprKind::Index { base, index } => {
            walk_hir_expr(base, f);
            walk_hir_expr(index, f);
        }
        HirExprKind::Unary { operand, .. } => walk_hir_expr(operand, f),
        HirExprKind::Binary { lhs, rhs, .. } => {
            walk_hir_expr(lhs, f);
            walk_hir_expr(rhs, f);
        }
        HirExprKind::Assign { place, value } => {
            walk_hir_expr(place, f);
            walk_hir_expr(value, f);
        }
        HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_hir_expr(condition, f);
            walk_hir_expr(then_branch, f);
            if let Some(e) = else_branch {
                walk_hir_expr(e, f);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            walk_hir_expr(scrutinee, f);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    walk_hir_expr(g, f);
                }
                walk_hir_expr(&arm.body, f);
            }
        }
        HirExprKind::Loop { body, .. } => walk_hir_expr(body, f),
        HirExprKind::While {
            condition, body, ..
        } => {
            walk_hir_expr(condition, f);
            walk_hir_expr(body, f);
        }
        HirExprKind::Block(block) => walk_hir_block(block, f),
        HirExprKind::Closure { body, .. } => walk_hir_expr(body, f),
        HirExprKind::LiftedClosure { captures, .. } => {
            for c in captures {
                walk_hir_expr(c, f);
            }
        }
        HirExprKind::Select { arms } => {
            for arm in arms {
                match &arm.op {
                    HirSelectOp::Recv { channel, .. } => walk_hir_expr(channel, f),
                    HirSelectOp::Send { channel, value } => {
                        walk_hir_expr(channel, f);
                        walk_hir_expr(value, f);
                    }
                    HirSelectOp::Default => {}
                }
                walk_hir_expr(&arm.body, f);
            }
        }
        HirExprKind::Return(value) => {
            if let Some(v) = value {
                walk_hir_expr(v, f);
            }
        }
        HirExprKind::Break { value, .. } => {
            if let Some(v) = value {
                walk_hir_expr(v, f);
            }
        }
        HirExprKind::Tuple(elems) => {
            for e in elems {
                walk_hir_expr(e, f);
            }
        }
        HirExprKind::Array(arr) => match arr {
            HirArrayExpr::List(elems) => {
                for e in elems {
                    walk_hir_expr(e, f);
                }
            }
            HirArrayExpr::Repeat { value, count } => {
                walk_hir_expr(value, f);
                walk_hir_expr(count, f);
            }
        },
        HirExprKind::Cast { value, .. } => walk_hir_expr(value, f),
        HirExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                walk_hir_expr(s, f);
            }
            if let Some(e) = end {
                walk_hir_expr(e, f);
            }
        }
    }
    fn walk_hir_block(block: &gossamer_hir::HirBlock, f: &mut impl FnMut(&HirExpr)) {
        for stmt in &block.stmts {
            match &stmt.kind {
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                    walk_hir_expr(expr, f);
                }
                HirStmtKind::Let { init, .. } => {
                    if let Some(e) = init {
                        walk_hir_expr(e, f);
                    }
                }
                HirStmtKind::Item(_) => {}
            }
        }
        if let Some(tail) = &block.tail {
            walk_hir_expr(tail, f);
        }
    }
}
