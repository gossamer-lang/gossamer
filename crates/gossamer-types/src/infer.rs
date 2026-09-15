//! Hindley-Milner unification over the [`crate::TyCtxt`] interner.
//! The [`InferCtxt`] hands out fresh [`TyVid`]s and maintains a
//! union-find mapping from variables to either another variable (a
//! parent link) or to a concrete resolved [`Ty`]. The `unify` method
//! implements standard structural unification with an occurs check,
//! walking through the interner to look at `TyKind` payloads.

#![forbid(unsafe_code)]

use thiserror::Error;

use crate::context::TyCtxt;
use crate::subst::{GenericArg, Substs};
use crate::traits::TraitRef;
use crate::ty::{FloatTy, FnSig, IntTy, Mutbl, Ty, TyKind, TyVid};

/// One slot in the union-find table maintained by [`InferCtxt`].
#[derive(Debug, Clone, Copy)]
enum VarSlot {
    /// Unresolved variable whose parent is another variable id (root
    /// when `parent == self`).
    Parent(u32),
    /// Variable bound to a concrete type handle.
    Resolved(Ty),
}

/// Inference context: owns fresh-var allocation and the union-find
/// substitution table.
///
/// Variables come in two flavours:
/// * **Plain** - produced by [`InferCtxt::fresh_var`]; unifies with
///   any type.
/// * **Integer-constrained** - produced by
///   [`InferCtxt::fresh_int_var`]; only unifies with concrete integer
///   types. Used by the typechecker to give unsuffixed integer
///   literals (`42`, `0`, `0x2a`) Go-style "untyped constant"
///   semantics: the literal takes the integer type required by its
///   use site, falls back to `i64` when no constraint resolves it
///   ([`InferCtxt::default_unresolved_int_vars`]), and is rejected
///   when forced into a non-integer position.
#[derive(Debug, Clone)]
pub struct InferCtxt {
    slots: Vec<VarSlot>,
    /// Per-variable flag. `true` at index `i` means the var with id
    /// `i` was minted as integer-constrained. The flag is meaningful
    /// only on root variables (consumers should call [`Self::root_of`]
    /// before reading), and unions propagate the constraint through
    /// the union-find merge in [`Self::bind_var`].
    integer_constrained: Vec<bool>,
    /// Per-variable flag mirroring [`Self::integer_constrained`] for
    /// unsuffixed float literals (`3.0`, `1.5`). A float-literal var
    /// takes the float type its use site requires (`f32` / `f64`) and
    /// falls back to `f64` via [`Self::default_unresolved_float_vars`]
    /// when nothing constrains it - without this an unsuffixed float
    /// fed into a generic position (`Triple { third: 3.0 }`) leaks an
    /// unresolved `Var` into lowering, which then prints the value's
    /// bit pattern as an integer.
    float_literal: Vec<bool>,
    /// Direct lookup from `TyVid` index to the interned `Ty` handle
    /// that wraps it. The previous implementation walked the entire
    /// interner on every `walk_var` lookup (an O(N) scan that turned
    /// `unify`/`resolve`/`occurs` into O(N²) on programs with many
    /// inference variables). Populating this side-table at
    /// `alloc_var` time keeps the lookup O(1).
    var_to_ty: Vec<Ty>,
}

impl InferCtxt {
    /// Creates an empty inference context.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            integer_constrained: Vec::new(),
            float_literal: Vec::new(),
            var_to_ty: Vec::new(),
        }
    }

    /// Allocates a fresh unresolved inference variable that unifies
    /// with any type.
    pub fn fresh_var(&mut self, tcx: &mut TyCtxt) -> Ty {
        self.alloc_var(tcx, false, false)
    }

    /// Allocates a fresh inference variable constrained to integer
    /// types. See the [`InferCtxt`] doc comment for the model.
    pub fn fresh_int_var(&mut self, tcx: &mut TyCtxt) -> Ty {
        self.alloc_var(tcx, true, false)
    }

    /// Allocates a fresh inference variable for an unsuffixed float
    /// literal. Defaults to `f64` when unconstrained - see the
    /// `float_literal` field doc.
    pub fn fresh_float_var(&mut self, tcx: &mut TyCtxt) -> Ty {
        self.alloc_var(tcx, false, true)
    }

    fn alloc_var(
        &mut self,
        tcx: &mut TyCtxt,
        integer_constrained: bool,
        float_literal: bool,
    ) -> Ty {
        let idx = u32::try_from(self.slots.len()).expect("too many inference vars");
        self.slots.push(VarSlot::Parent(idx));
        self.integer_constrained.push(integer_constrained);
        self.float_literal.push(float_literal);
        let ty = tcx.intern(TyKind::Var(TyVid(idx)));
        // Side-table: O(1) lookup for `walk_var` to avoid a full
        // interner scan on every resolve / unify / occurs.
        self.var_to_ty.push(ty);
        ty
    }

    /// Defaults every integer-constrained variable that is still
    /// unresolved to `i64`. Called once at the end of typechecking
    /// so unsuffixed literals with no use-site constraint pick a
    /// concrete type instead of leaking through to lowering as a
    /// raw `Var`.
    pub fn default_unresolved_int_vars(&mut self, tcx: &mut TyCtxt) {
        let i64_ty = tcx.int_ty(IntTy::I64);
        let count = self.slots.len();
        for idx in 0..count {
            if !self.integer_constrained.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let root = self.root_of(TyVid(idx as u32));
            if matches!(self.slots[root as usize], VarSlot::Parent(_))
                && self
                    .integer_constrained
                    .get(root as usize)
                    .copied()
                    .unwrap_or(false)
            {
                self.slots[root as usize] = VarSlot::Resolved(i64_ty);
            }
        }
    }

    /// Defaults every unsuffixed-float-literal variable that is still
    /// unresolved to `f64`. Mirror of
    /// [`Self::default_unresolved_int_vars`] for the float side.
    /// Called once at the end of typechecking.
    pub fn default_unresolved_float_vars(&mut self, tcx: &mut TyCtxt) {
        let f64_ty = tcx.float_ty(FloatTy::F64);
        let count = self.slots.len();
        for idx in 0..count {
            if !self.float_literal.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let root = self.root_of(TyVid(idx as u32));
            if matches!(self.slots[root as usize], VarSlot::Parent(_))
                && self
                    .float_literal
                    .get(root as usize)
                    .copied()
                    .unwrap_or(false)
            {
                self.slots[root as usize] = VarSlot::Resolved(f64_ty);
            }
        }
    }

    /// Fixes unconstrained numeric literals reachable from an inferred
    /// binding to their language defaults before later statements are checked.
    pub fn default_numeric_vars_in_ty(&mut self, tcx: &mut TyCtxt, ty: Ty) {
        let resolved = self.resolve(tcx, ty);
        match tcx.kind_of(resolved).clone() {
            TyKind::Var(vid) => {
                let root = self.root_of(vid);
                if !matches!(self.slots[root as usize], VarSlot::Parent(_)) {
                    return;
                }
                if self
                    .integer_constrained
                    .get(root as usize)
                    .copied()
                    .unwrap_or(false)
                {
                    self.slots[root as usize] = VarSlot::Resolved(tcx.int_ty(IntTy::I64));
                } else if self
                    .float_literal
                    .get(root as usize)
                    .copied()
                    .unwrap_or(false)
                {
                    self.slots[root as usize] = VarSlot::Resolved(tcx.float_ty(FloatTy::F64));
                }
            }
            TyKind::Tuple(types) => {
                for ty in types {
                    self.default_numeric_vars_in_ty(tcx, ty);
                }
            }
            TyKind::Array { elem, .. }
            | TyKind::Simd { elem, .. }
            | TyKind::Slice(elem)
            | TyKind::Vec(elem)
            | TyKind::Iterator(elem)
            | TyKind::Sender(elem)
            | TyKind::Receiver(elem)
            | TyKind::JoinHandle(elem)
            | TyKind::Ref { inner: elem, .. } => self.default_numeric_vars_in_ty(tcx, elem),
            TyKind::HashMap { key, value, .. } => {
                self.default_numeric_vars_in_ty(tcx, key);
                self.default_numeric_vars_in_ty(tcx, value);
            }
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                for input in sig.inputs {
                    self.default_numeric_vars_in_ty(tcx, input);
                }
                self.default_numeric_vars_in_ty(tcx, sig.output);
            }
            TyKind::FnDef { substs, .. }
            | TyKind::Closure { substs, .. }
            | TyKind::Adt { substs, .. }
            | TyKind::Alias { substs, .. } => {
                for ty in substs.types() {
                    self.default_numeric_vars_in_ty(tcx, ty);
                }
            }
            TyKind::Dyn(trait_ref) => {
                for ty in trait_ref.substs.types() {
                    self.default_numeric_vars_in_ty(tcx, ty);
                }
            }
            _ => {}
        }
    }

    /// Returns true when `vid` is still unresolved and belongs to an
    /// unsuffixed integer literal that will default to `i64`.
    #[must_use]
    pub fn is_unresolved_integer_var(&mut self, vid: TyVid) -> bool {
        let root = self.root_of(vid);
        matches!(self.slots[root as usize], VarSlot::Parent(_))
            && self
                .integer_constrained
                .get(root as usize)
                .copied()
                .unwrap_or(false)
    }

    /// Returns true when `vid` is still unresolved and belongs to an
    /// unsuffixed float literal that will default to `f64`.
    #[must_use]
    pub fn is_unresolved_float_var(&mut self, vid: TyVid) -> bool {
        let root = self.root_of(vid);
        matches!(self.slots[root as usize], VarSlot::Parent(_))
            && self
                .float_literal
                .get(root as usize)
                .copied()
                .unwrap_or(false)
    }

    /// Returns the current representative of a type handle. Inference
    /// variables are walked transitively to a non-variable type (or to
    /// the root variable if still unresolved).
    #[must_use]
    pub fn resolve(&self, tcx: &TyCtxt, ty: Ty) -> Ty {
        let mut current = ty;
        loop {
            let Some(TyKind::Var(vid)) = tcx.kind(current) else {
                return current;
            };
            let Some(next) = self.walk_var(tcx, *vid) else {
                return current;
            };
            if next == current {
                return current;
            }
            current = next;
        }
    }

    /// Whether `ty` resolves to an as-yet-unbound integer-constrained
    /// inference variable - an unsuffixed integer literal whose width
    /// has not been pinned. Such a value is laid out as a machine
    /// integer on every tier, so routing it into a `String` parameter
    /// makes the compiled string shims dereference it as a pointer.
    #[must_use]
    pub fn is_integer_constrained_var(&mut self, tcx: &TyCtxt, ty: Ty) -> bool {
        let resolved = self.resolve(tcx, ty);
        let vid = match tcx.kind(resolved) {
            Some(TyKind::Var(vid)) => *vid,
            _ => return false,
        };
        let root = self.root_of(vid);
        self.integer_constrained
            .get(root as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Whether `ty` resolves to an as-yet-unbound float-default
    /// inference variable - an unsuffixed float literal. Callers use this to
    /// render the default `f64` spelling in diagnostics.
    #[must_use]
    pub fn is_float_literal_var(&mut self, tcx: &TyCtxt, ty: Ty) -> bool {
        let resolved = self.resolve(tcx, ty);
        let vid = match tcx.kind(resolved) {
            Some(TyKind::Var(vid)) => *vid,
            _ => return false,
        };
        let root = self.root_of(vid);
        self.float_literal
            .get(root as usize)
            .copied()
            .unwrap_or(false)
    }

    fn walk_var(&self, tcx: &TyCtxt, vid: TyVid) -> Option<Ty> {
        let mut idx = vid.0;
        loop {
            match self.slots.get(idx as usize)? {
                VarSlot::Parent(parent) if *parent == idx => {
                    // Side-table lookup: O(1) instead of the
                    // historical full-interner scan in `lookup_var`.
                    // Falls back to the scan for the impossible case
                    // where a var slot exists without a side-table
                    // entry - keeps the function total under any
                    // future churn.
                    return Some(
                        self.var_to_ty
                            .get(idx as usize)
                            .copied()
                            .unwrap_or_else(|| lookup_var(tcx, idx)),
                    );
                }
                VarSlot::Parent(parent) => idx = *parent,
                VarSlot::Resolved(resolved) => return Some(*resolved),
            }
        }
    }

    fn root_of(&mut self, vid: TyVid) -> u32 {
        let mut idx = vid.0;
        while let Some(VarSlot::Parent(parent)) = self.slots.get(idx as usize).copied() {
            if parent == idx {
                return idx;
            }
            idx = parent;
        }
        idx
    }

    fn bind(&mut self, vid: TyVid, ty: Ty) {
        let root = self.root_of(vid);
        self.slots[root as usize] = VarSlot::Resolved(ty);
    }

    /// Unifies two types, mutating the substitution table on success.
    pub fn unify(&mut self, tcx: &mut TyCtxt, lhs: Ty, rhs: Ty) -> Result<(), UnifyError> {
        let lhs = self.resolve(tcx, lhs);
        let rhs = self.resolve(tcx, rhs);
        if lhs == rhs {
            return Ok(());
        }
        let lhs_kind = tcx.kind_of(lhs).clone();
        let rhs_kind = tcx.kind_of(rhs).clone();
        self.unify_kinds(tcx, lhs, rhs, &lhs_kind, &rhs_kind)
    }

    fn unify_kinds(
        &mut self,
        tcx: &mut TyCtxt,
        lhs: Ty,
        rhs: Ty,
        lhs_kind: &TyKind,
        rhs_kind: &TyKind,
    ) -> Result<(), UnifyError> {
        match (lhs_kind, rhs_kind) {
            // Never / Error short-circuit BEFORE Var-binding so a
            // match arm that diverges (`return ...`, `panic!`)
            // doesn't pin its sibling arm's inference variable to
            // `Never`. Without this, `let x = match r { Ok(v) =>
            // v, Err(_) => return Err(...) }` infers `x: Never`
            // and downstream `x.field` fails the struct lookup.
            (TyKind::Never | TyKind::Error, _) | (_, TyKind::Never | TyKind::Error) => Ok(()),
            (TyKind::Var(vid), _) => self.bind_var(tcx, *vid, rhs),
            (_, TyKind::Var(vid)) => self.bind_var(tcx, *vid, lhs),
            // `time::Duration` is a transparent `i64` newtype: it unifies
            // freely with any integer kind so passing a Duration to an
            // `i64` parameter, comparing it, or assigning it to an
            // annotated `i64` binding all type-check.
            (TyKind::Duration, TyKind::Int(_)) | (TyKind::Int(_), TyKind::Duration) => Ok(()),
            // `time::Instant` is likewise a transparent `i64` newtype.
            (TyKind::Instant, TyKind::Int(_)) | (TyKind::Int(_), TyKind::Instant) => Ok(()),
            _ if lhs_kind == rhs_kind => Ok(()),
            _ => self.unify_structural(tcx, lhs_kind, rhs_kind),
        }
    }

    fn bind_var(&mut self, tcx: &mut TyCtxt, vid: TyVid, ty: Ty) -> Result<(), UnifyError> {
        if occurs(self, tcx, vid, ty) {
            return Err(UnifyError::Occurs { var: vid });
        }
        let root = self.root_of(vid);
        let needs_int = self
            .integer_constrained
            .get(root as usize)
            .copied()
            .unwrap_or(false);
        if needs_int {
            let resolved = self.resolve(tcx, ty);
            match tcx.kind(resolved).cloned() {
                Some(TyKind::Int(_)) => {}
                Some(TyKind::Var(other_vid)) => {
                    // Propagate the integer constraint to the target
                    // var's root so the merged equivalence class
                    // remains integer-only.
                    let other_root = self.root_of(other_vid);
                    if other_root as usize >= self.integer_constrained.len() {
                        self.integer_constrained
                            .resize((other_root as usize) + 1, false);
                    }
                    self.integer_constrained[other_root as usize] = true;
                }
                _ => return Err(UnifyError::IntegerConstraint),
            }
        }
        // Mirror the integer constraint for unsuffixed float literals.
        // They may select f32 or f64 from context, but must never satisfy an
        // integer, boolean, string, function, or aggregate slot.
        let needs_float = self
            .float_literal
            .get(root as usize)
            .copied()
            .unwrap_or(false);
        if needs_float {
            let resolved = self.resolve(tcx, ty);
            match tcx.kind(resolved).cloned() {
                Some(TyKind::Float(_)) => {}
                Some(TyKind::Var(other_vid)) => {
                    let other_root = self.root_of(other_vid);
                    if other_root as usize >= self.float_literal.len() {
                        self.float_literal.resize((other_root as usize) + 1, false);
                    }
                    self.float_literal[other_root as usize] = true;
                }
                _ => return Err(UnifyError::FloatConstraint),
            }
        }
        self.bind(vid, ty);
        Ok(())
    }

    /// Unifies two maps. The two spellings name distinct types, so an
    /// ordered map only unifies with another ordered map.
    fn unify_maps(
        &mut self,
        tcx: &mut TyCtxt,
        lhs_kind: &TyKind,
        rhs_kind: &TyKind,
    ) -> Result<(), UnifyError> {
        let (
            TyKind::HashMap {
                key: ak,
                value: av,
                ordered: ao,
            },
            TyKind::HashMap {
                key: bk,
                value: bv,
                ordered: bo,
            },
        ) = (lhs_kind, rhs_kind)
        else {
            return Err(UnifyError::Mismatch);
        };
        if ao != bo {
            return Err(UnifyError::Mismatch);
        }
        self.unify(tcx, *ak, *bk)?;
        self.unify(tcx, *av, *bv)
    }

    fn unify_structural(
        &mut self,
        tcx: &mut TyCtxt,
        lhs_kind: &TyKind,
        rhs_kind: &TyKind,
    ) -> Result<(), UnifyError> {
        match (lhs_kind, rhs_kind) {
            (TyKind::Tuple(a), TyKind::Tuple(b)) => self.unify_seq(tcx, a, b),
            (TyKind::Array { elem: ae, len: al }, TyKind::Array { elem: be, len: bl })
            | (
                TyKind::Simd {
                    elem: ae,
                    lanes: al,
                },
                TyKind::Simd {
                    elem: be,
                    lanes: bl,
                },
            ) if al == bl => self.unify(tcx, *ae, *be),
            (TyKind::Slice(a), TyKind::Slice(b))
            | (TyKind::Vec(a), TyKind::Vec(b))
            | (TyKind::Sender(a), TyKind::Sender(b))
            | (TyKind::Receiver(a), TyKind::Receiver(b))
            | (TyKind::Range(a), TyKind::Range(b))
            | (TyKind::Iterator(a), TyKind::Iterator(b))
            | (TyKind::JoinHandle(a), TyKind::JoinHandle(b)) => self.unify(tcx, *a, *b),
            // A range converts to the iterator it advances through, and only
            // in that direction: an adapter chain is not a range, so an
            // expected `Range<T>` keeps rejecting a plain `Iterator<T>`.
            (TyKind::Iterator(a), TyKind::Range(b)) => self.unify(tcx, *a, *b),
            (TyKind::HashMap { .. }, TyKind::HashMap { .. }) => {
                self.unify_maps(tcx, lhs_kind, rhs_kind)
            }
            // Rust-like unsizing coercions are directional: an expected
            // slice reference accepts an array or Vec reference with the
            // same mutability and element type. Owned arrays, slices, and
            // Vecs remain distinct.
            (
                TyKind::Ref {
                    mutability: am,
                    inner: ai,
                },
                TyKind::Ref {
                    mutability: bm,
                    inner: bi,
                },
            ) if (*am == Mutbl::Not || *bm == Mutbl::Mut)
                && matches!(tcx.kind(self.resolve(tcx, *ai)), Some(TyKind::Slice(_))) =>
            {
                self.unify_ref_to_slice(tcx, *ai, *bi)
            }
            // A reference reborrows as a shared one, and only in that
            // direction: an expected `&T` accepts a `&mut T`, so a function
            // that only reads keeps its `&T` signature and a caller holding
            // a `&mut T` can still hand it over. An expected `&mut T` keeps
            // rejecting a `&T`, which would grant a write the source place
            // never authorized.
            (
                TyKind::Ref {
                    mutability: am,
                    inner: ai,
                },
                TyKind::Ref {
                    mutability: bm,
                    inner: bi,
                },
            ) if am == bm || (*am == Mutbl::Not && *bm == Mutbl::Mut) => self.unify(tcx, *ai, *bi),
            // String literals and owned strings coerce to `&str`. Gossamer's
            // text references are layout-transparent, but the rule remains
            // directional so an arbitrary bare value cannot satisfy `&T`.
            (
                TyKind::Ref {
                    mutability: Mutbl::Not,
                    inner,
                },
                TyKind::String,
            ) if matches!(tcx.kind(*inner), Some(TyKind::String)) => Ok(()),
            (TyKind::FnPtr(a), TyKind::FnPtr(b)) | (TyKind::FnTrait(a), TyKind::FnTrait(b)) => {
                self.unify_fn_sig(tcx, a, b)
            }
            // `Fn(args) -> ret` accepts any callable with a
            // matching signature: bare `fn`, named `fn item`, or a
            // closure (capturing or not). The MIR coercion site
            // wraps the source value in the env+code shape the
            // codegen expects. The reverse direction (`Fn(_)`
            // assigned to a bare `fn(_)`) is rejected - capturing
            // closures need an env that bare `fn` can't carry. See
            // closure_fn_trait_plan.md.
            (TyKind::FnTrait(t), TyKind::FnPtr(s)) | (TyKind::FnPtr(s), TyKind::FnTrait(t)) => {
                self.unify_fn_sig(tcx, t, s)
            }
            // A named function item's signature lives in the checker-owned
            // definition table, not in `TyKind::FnDef`. The checker resolves
            // and validates this coercion before entering the raw unifier. A
            // nested or otherwise unchecked function item must fail closed.
            (TyKind::FnTrait(_), TyKind::FnDef { .. })
            | (TyKind::FnDef { .. }, TyKind::FnTrait(_)) => Err(UnifyError::Mismatch),
            (TyKind::FnTrait(_), TyKind::Closure { .. })
            | (TyKind::Closure { .. }, TyKind::FnTrait(_)) => {
                // Closures are tied to a synthesised def id; their
                // signature flows through the closure-body item.
                // Accept the coercion; the MIR `lift_closures`
                // pass guarantees the arities match.
                Ok(())
            }
            (
                TyKind::Adt {
                    def: ad,
                    substs: asu,
                },
                TyKind::Adt {
                    def: bd,
                    substs: bsu,
                },
            )
            | (
                TyKind::Alias {
                    def: ad,
                    substs: asu,
                },
                TyKind::Alias {
                    def: bd,
                    substs: bsu,
                },
            )
            | (
                TyKind::FnDef {
                    def: ad,
                    substs: asu,
                },
                TyKind::FnDef {
                    def: bd,
                    substs: bsu,
                },
            ) if ad == bd => self.unify_substs(tcx, asu, bsu),
            (TyKind::Dyn(a), TyKind::Dyn(b)) => self.unify_trait_ref(tcx, a, b),
            _ => Err(UnifyError::Mismatch),
        }
    }

    fn unify_ref_to_slice(
        &mut self,
        tcx: &mut TyCtxt,
        expected: Ty,
        found: Ty,
    ) -> Result<(), UnifyError> {
        let expected = self.resolve(tcx, expected);
        let found = self.resolve(tcx, found);
        let Some(TyKind::Slice(expected_elem)) = tcx.kind(expected).cloned() else {
            unreachable!();
        };
        match tcx.kind(found).cloned() {
            Some(
                TyKind::Slice(found_elem)
                | TyKind::Vec(found_elem)
                | TyKind::Array {
                    elem: found_elem, ..
                },
            ) => self.unify(tcx, expected_elem, found_elem),
            _ => Err(UnifyError::Mismatch),
        }
    }

    fn unify_seq(&mut self, tcx: &mut TyCtxt, a: &[Ty], b: &[Ty]) -> Result<(), UnifyError> {
        if a.len() != b.len() {
            return Err(UnifyError::Mismatch);
        }
        for (x, y) in a.iter().zip(b) {
            self.unify(tcx, *x, *y)?;
        }
        Ok(())
    }

    fn unify_fn_sig(&mut self, tcx: &mut TyCtxt, a: &FnSig, b: &FnSig) -> Result<(), UnifyError> {
        self.unify_seq(tcx, &a.inputs, &b.inputs)?;
        self.unify(tcx, a.output, b.output)
    }

    fn unify_substs(&mut self, tcx: &mut TyCtxt, a: &Substs, b: &Substs) -> Result<(), UnifyError> {
        let a_args = a.as_slice();
        let b_args = b.as_slice();
        if a_args.len() != b_args.len() {
            return Err(UnifyError::Mismatch);
        }
        for (x, y) in a_args.iter().zip(b_args) {
            self.unify_arg(tcx, x, y)?;
        }
        Ok(())
    }

    fn unify_arg(
        &mut self,
        tcx: &mut TyCtxt,
        a: &GenericArg,
        b: &GenericArg,
    ) -> Result<(), UnifyError> {
        match (a, b) {
            (GenericArg::Type(x), GenericArg::Type(y)) => self.unify(tcx, *x, *y),
            (GenericArg::Const(x), GenericArg::Const(y)) if x == y => Ok(()),
            (GenericArg::ConstParam(x), GenericArg::ConstParam(y)) if x == y => Ok(()),
            _ => Err(UnifyError::Mismatch),
        }
    }

    fn unify_trait_ref(
        &mut self,
        tcx: &mut TyCtxt,
        a: &TraitRef,
        b: &TraitRef,
    ) -> Result<(), UnifyError> {
        if a.def != b.def {
            return Err(UnifyError::Mismatch);
        }
        self.unify_substs(tcx, &a.substs, &b.substs)
    }
}

impl Default for InferCtxt {
    fn default() -> Self {
        Self::new()
    }
}

fn lookup_var(tcx: &TyCtxt, idx: u32) -> Ty {
    let kind = TyKind::Var(TyVid(idx));
    let len = u32::try_from(tcx.len()).unwrap_or(u32::MAX);
    for handle_idx in 0..len {
        let handle = Ty(handle_idx);
        if tcx.kind(handle) == Some(&kind) {
            return handle;
        }
    }
    // Safety net: a var that hasn't been interned should be
    // impossible because `alloc_var` interns at allocation time.
    // If we ever see one (e.g. a unification side-effect we
    // didn't anticipate), fall back to `Error` rather than
    // crashing the compiler with valid user input.
    Ty(0)
}

fn occurs(infer: &InferCtxt, tcx: &TyCtxt, vid: TyVid, ty: Ty) -> bool {
    let resolved = infer.resolve(tcx, ty);
    match tcx.kind(resolved) {
        Some(TyKind::Var(other)) => other.0 == vid.0,
        Some(kind) => occurs_in_kind(infer, tcx, vid, kind),
        None => false,
    }
}

fn occurs_in_kind(infer: &InferCtxt, tcx: &TyCtxt, vid: TyVid, kind: &TyKind) -> bool {
    match kind {
        TyKind::Tuple(parts) => parts.iter().any(|t| occurs(infer, tcx, vid, *t)),
        TyKind::Array { elem, .. }
        | TyKind::Simd { elem, .. }
        | TyKind::Slice(elem)
        | TyKind::Vec(elem)
        | TyKind::Iterator(elem)
        | TyKind::Range(elem) => occurs(infer, tcx, vid, *elem),
        TyKind::HashMap { key, value, .. } => {
            occurs(infer, tcx, vid, *key) || occurs(infer, tcx, vid, *value)
        }
        TyKind::Sender(pointee)
        | TyKind::Receiver(pointee)
        | TyKind::JoinHandle(pointee)
        | TyKind::Nominal { repr: pointee, .. }
        | TyKind::Ref { inner: pointee, .. } => occurs(infer, tcx, vid, *pointee),
        TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
            sig.inputs.iter().any(|t| occurs(infer, tcx, vid, *t))
                || occurs(infer, tcx, vid, sig.output)
        }
        TyKind::FnDef { substs, .. }
        | TyKind::Adt { substs, .. }
        | TyKind::Alias { substs, .. }
        | TyKind::Closure { substs, .. } => occurs_in_substs(infer, tcx, vid, substs),
        TyKind::Dyn(trait_ref) => occurs_in_substs(infer, tcx, vid, &trait_ref.substs),
        TyKind::Bool
        | TyKind::Char
        | TyKind::String
        | TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::Unit
        | TyKind::Never
        | TyKind::Duration
        | TyKind::Instant
        | TyKind::JsonValue
        | TyKind::DynValue
        | TyKind::DynError
        | TyKind::Var(_)
        | TyKind::Param { .. }
        | TyKind::Error => false,
    }
}

fn occurs_in_substs(infer: &InferCtxt, tcx: &TyCtxt, vid: TyVid, substs: &Substs) -> bool {
    substs.as_slice().iter().any(|arg| match arg {
        GenericArg::Type(ty) => occurs(infer, tcx, vid, *ty),
        GenericArg::Const(_) | GenericArg::ConstParam(_) => false,
    })
}

/// Unification failure reported by [`InferCtxt::unify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UnifyError {
    /// Types have incompatible shapes.
    #[error("type mismatch")]
    Mismatch,
    /// Binding the variable would produce an infinite type.
    #[error("occurs check: variable {var:?} appears in the type it's being unified with")]
    Occurs {
        /// The variable that would be self-referential.
        var: TyVid,
    },
    /// An integer-constrained inference variable (introduced by an
    /// unsuffixed integer literal) was forced into a non-integer
    /// position.
    #[error("integer literal cannot satisfy non-integer type constraint")]
    IntegerConstraint,
    /// An unsuffixed float literal was forced into a non-float position.
    #[error("float literal cannot satisfy non-float type constraint")]
    FloatConstraint,
}
