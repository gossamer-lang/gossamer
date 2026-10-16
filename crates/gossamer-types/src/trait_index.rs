//! Trait resolution.
//! Builds an [`ImplIndex`] from a resolved source file, supporting:
//! - Inherent method lookup: given a receiver type and method name,
//!   find the concrete impl item that defines it.
//! - Trait method lookup: walk impls of a trait for a given self type.
//! - Coherence: detect two impls of the same trait that apply to the
//!   same concrete self type.
//! - Vtable construction: for a concrete `impl Trait for T`, produce the
//!   ordered list of implementing function [`DefId`]s keyed by the
//!   trait's method declaration order.
//!
//! The solver is a minimal stand-in for a full Chalk-style solver: it
//! handles direct impl lookups and records obligations for later work;
//! blanket impls and supertrait propagation will arrive incrementally.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;

use gossamer_ast::{
    Expr, ExprKind, GenericArg as AstGenericArg, ImplDecl, ImplItem, ItemKind, Literal, NodeId,
    SourceFile, TraitBound, TraitDecl, TraitItem, Type as AstType, TypeKind as AstTypeKind,
    TypePath,
};
use gossamer_lex::Span;
use gossamer_resolve::{DefId, FloatWidth, IntWidth, PrimitiveTy, Resolution, Resolutions};
use thiserror::Error;

use crate::context::TyCtxt;
use crate::subst::{GenericArg, Substs};
use crate::traits::TraitRef;
use crate::ty::{FloatTy, FnSig, IntTy, Mutbl, Ty, TyKind};

/// Stable identifier for an impl entry within an [`ImplIndex`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImplId(pub u32);

/// Stable identifier for a single `fn` item inside an impl block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImplFnId(pub u32);

/// One concrete impl block collected from the source.
#[derive(Debug, Clone)]
pub struct ImplEntry {
    /// Self type the impl attaches to.
    pub self_ty: Ty,
    /// Trait being implemented, or `None` for inherent impls.
    pub trait_ref: Option<TraitRef>,
    /// Methods defined in this impl, keyed by declaration order.
    pub methods: Vec<ImplMethod>,
    /// Concrete types this impl supplies for the trait's associated types,
    /// in declaration order.
    pub assoc_types: Vec<(String, Ty)>,
    /// Declared types of the associated constants this impl supplies, in
    /// declaration order.
    pub assoc_consts: Vec<(String, Ty)>,
    /// Source range of the impl block.
    pub span: Span,
}

/// Single `fn` item inside an impl.
#[derive(Debug, Clone)]
pub struct ImplMethod {
    /// Method name as written in source.
    pub name: String,
    /// Stable id within the [`ImplIndex`].
    pub id: ImplFnId,
    /// Signature derived from the method's declared parameter and
    /// return types.
    pub sig: FnSig,
    /// `true` when the method declares a `self` receiver.
    pub has_self: bool,
}

/// Trait declaration record built alongside the impl index.
#[derive(Debug, Clone)]
pub struct TraitEntry {
    /// Defid the resolver assigned to the trait.
    pub def: DefId,
    /// Trait name.
    pub name: String,
    /// Method names in declaration order, used to compute vtable slots.
    pub methods: Vec<String>,
    /// Associated type names in declaration order, paired with the trait's
    /// default type where one is written.
    pub assoc_types: Vec<(String, Option<Ty>)>,
    /// Associated constant names in declaration order, paired with their
    /// declared type.
    pub assoc_consts: Vec<(String, Ty)>,
    /// Supertrait references extracted from the `: Bounds` clause.
    pub supertraits: Vec<TraitRef>,
    /// Source range of the trait declaration.
    pub span: Span,
}

/// Index over every impl and trait in a resolved source file.
#[derive(Debug, Default, Clone)]
pub struct ImplIndex {
    impls: Vec<ImplEntry>,
    traits: HashMap<DefId, TraitEntry>,
    trait_by_name: HashMap<String, DefId>,
    next_method_id: u32,
}

impl ImplIndex {
    /// Walks `source`, lowering its `impl` and `trait` items into a
    /// queryable index. Returns diagnostics for any coherence problems
    /// detected while indexing.
    #[must_use]
    pub fn build(
        source: &SourceFile,
        resolutions: &Resolutions,
        tcx: &mut TyCtxt,
    ) -> (Self, Vec<TraitDiagnostic>) {
        let mut index = Self::default();
        let trait_by_name = collect_trait_names(source, resolutions);
        index.trait_by_name.clone_from(&trait_by_name);
        for item in &source.items {
            if let ItemKind::Trait(decl) = &item.kind {
                let def = resolutions.definition_of(item.id);
                let mut lowerer = TypeLowerer {
                    tcx,
                    resolutions,
                    trait_by_name: &trait_by_name,
                };
                index.register_trait(decl, &mut lowerer, def, item.span);
            }
        }
        for item in &source.items {
            if let ItemKind::Impl(decl) = &item.kind {
                let mut lowerer = TypeLowerer {
                    tcx,
                    resolutions,
                    trait_by_name: &trait_by_name,
                };
                index.register_impl(decl, &mut lowerer, item.span);
            }
        }
        let diagnostics = index.check_coherence(tcx);
        (index, diagnostics)
    }

    /// Returns the number of impls indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.impls.len()
    }

    /// Returns `true` when no impls are indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.impls.is_empty()
    }

    /// Iterates every impl entry in insertion order.
    pub fn entries(&self) -> impl Iterator<Item = (ImplId, &ImplEntry)> {
        self.impls.iter().enumerate().map(|(idx, entry)| {
            let id = u32::try_from(idx).expect("impl index overflow");
            (ImplId(id), entry)
        })
    }

    /// Returns a single impl entry by id.
    #[must_use]
    pub fn get(&self, id: ImplId) -> Option<&ImplEntry> {
        self.impls.get(id.0 as usize)
    }

    /// Iterates every trait declaration seen in this source file.
    pub fn traits(&self) -> impl Iterator<Item = &TraitEntry> {
        self.traits.values()
    }

    /// Returns the trait entry registered for `def`, if any.
    #[must_use]
    pub fn trait_of(&self, def: DefId) -> Option<&TraitEntry> {
        self.traits.get(&def)
    }

    /// Concrete type `impl_id` supplies for the associated type `name`,
    /// falling back to the implemented trait's default.
    #[must_use]
    pub fn assoc_type(&self, impl_id: ImplId, name: &str) -> Option<Ty> {
        let entry = self.get(impl_id)?;
        if let Some((_, ty)) = entry.assoc_types.iter().find(|(n, _)| n == name) {
            return Some(*ty);
        }
        let trait_def = entry.trait_ref.as_ref()?.def;
        self.traits
            .get(&trait_def)?
            .assoc_types
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, default)| *default)
    }

    /// Finds an inherent method on `receiver` by name.
    ///
    /// Returns the first match found in impl insertion order. Trait-
    /// provided methods are not considered here; use
    /// [`Self::resolve_trait_method`] for that.
    #[must_use]
    pub fn resolve_inherent_method(&self, receiver: Ty, name: &str) -> Option<MethodResolution> {
        for (impl_id, entry) in self.entries() {
            if entry.trait_ref.is_some() || entry.self_ty != receiver {
                continue;
            }
            if let Some((idx, method)) = entry
                .methods
                .iter()
                .enumerate()
                .find(|(_, m)| m.name == name)
            {
                return Some(MethodResolution {
                    impl_id,
                    method_slot: idx,
                    method_id: method.id,
                });
            }
        }
        None
    }

    /// Finds a trait method named `name` implemented on `receiver` by
    /// any trait in scope.
    ///
    /// Walks every trait impl matching `receiver`. Returns the first
    /// match in impl insertion order.
    #[must_use]
    pub fn resolve_trait_method(&self, receiver: Ty, name: &str) -> Option<MethodResolution> {
        for (impl_id, entry) in self.entries() {
            if entry.trait_ref.is_none() || entry.self_ty != receiver {
                continue;
            }
            if let Some((idx, method)) = entry
                .methods
                .iter()
                .enumerate()
                .find(|(_, m)| m.name == name)
            {
                return Some(MethodResolution {
                    impl_id,
                    method_slot: idx,
                    method_id: method.id,
                });
            }
        }
        None
    }

    /// Combined method lookup: tries inherent methods first, then
    /// falls back to trait methods.
    #[must_use]
    pub fn resolve_method(&self, receiver: Ty, name: &str) -> Option<MethodResolution> {
        self.resolve_inherent_method(receiver, name)
            .or_else(|| self.resolve_trait_method(receiver, name))
    }

    /// Builds the vtable for `impl_id`, mapping each trait method slot
    /// to the [`ImplFnId`] of the concrete implementation.
    ///
    /// Returns `None` when the impl is inherent or when the trait is
    /// not registered in this index.
    #[must_use]
    pub fn vtable(&self, impl_id: ImplId) -> Option<Vec<ImplFnId>> {
        let entry = self.get(impl_id)?;
        let trait_def = entry.trait_ref.as_ref()?.def;
        let trait_entry = self.traits.get(&trait_def)?;
        let mut slots = Vec::with_capacity(trait_entry.methods.len());
        for method_name in &trait_entry.methods {
            let method_id = entry
                .methods
                .iter()
                .find(|m| &m.name == method_name)
                .map(|m| m.id)?;
            slots.push(method_id);
        }
        Some(slots)
    }

    fn register_trait(
        &mut self,
        decl: &TraitDecl,
        lowerer: &mut TypeLowerer<'_>,
        def: Option<DefId>,
        span: Span,
    ) {
        let Some(def) = def else {
            return;
        };
        let mut methods = Vec::new();
        let mut assoc_types = Vec::new();
        let mut assoc_consts = Vec::new();
        for item in &decl.items {
            match item {
                TraitItem::Fn(fn_decl) => methods.push(fn_decl.name.name.clone()),
                TraitItem::Type { name, default, .. } => {
                    let default = default.as_ref().map(|ty| lowerer.lower_type(ty));
                    assoc_types.push((name.name.clone(), default));
                }
                TraitItem::Const { name, ty, .. } => {
                    let ty = lowerer.lower_type(ty);
                    assoc_consts.push((name.name.clone(), ty));
                }
            }
        }
        let supertraits = decl
            .supertraits
            .iter()
            .filter_map(|bound| lowerer.lower_trait_bound(bound))
            .collect();
        self.trait_by_name.insert(decl.name.name.clone(), def);
        self.traits.insert(
            def,
            TraitEntry {
                def,
                name: decl.name.name.clone(),
                methods,
                assoc_types,
                assoc_consts,
                supertraits,
                span,
            },
        );
    }

    fn register_impl(&mut self, decl: &ImplDecl, lowerer: &mut TypeLowerer<'_>, span: Span) {
        let self_ty = lowerer.lower_type(&decl.self_ty);
        let trait_ref = decl
            .trait_ref
            .as_ref()
            .and_then(|bound| lowerer.lower_trait_bound(bound));
        let mut methods = Vec::new();
        let mut assoc_types = Vec::new();
        let mut assoc_consts = Vec::new();
        for item in &decl.items {
            match item {
                ImplItem::Fn(fn_decl) => {
                    let sig = lowerer.lower_fn_sig(fn_decl);
                    let has_self = fn_decl
                        .params
                        .iter()
                        .any(|param| matches!(param, gossamer_ast::FnParam::Receiver(_)));
                    methods.push(ImplMethod {
                        name: fn_decl.name.name.clone(),
                        id: ImplFnId(self.next_method_id),
                        sig,
                        has_self,
                    });
                    self.next_method_id = self.next_method_id.saturating_add(1);
                }
                ImplItem::Type { name, ty, .. } => {
                    let ty = lowerer.lower_type(ty);
                    assoc_types.push((name.name.clone(), ty));
                }
                ImplItem::Const { name, ty, .. } => {
                    let ty = lowerer.lower_type(ty);
                    assoc_consts.push((name.name.clone(), ty));
                }
            }
        }
        self.impls.push(ImplEntry {
            self_ty,
            trait_ref,
            methods,
            assoc_types,
            assoc_consts,
            span,
        });
    }

    fn check_coherence(&self, tcx: &TyCtxt) -> Vec<TraitDiagnostic> {
        let mut diagnostics = Vec::new();
        for (i, a) in self.impls.iter().enumerate() {
            for b in self.impls.iter().skip(i + 1) {
                if overlap(a, b, tcx) {
                    diagnostics.push(TraitDiagnostic::new(
                        TraitError::OverlappingImpls {
                            first_span: a.span,
                            second_span: b.span,
                        },
                        b.span,
                    ));
                }
            }
        }
        diagnostics
    }
}

fn overlap(a: &ImplEntry, b: &ImplEntry, _tcx: &TyCtxt) -> bool {
    match (&a.trait_ref, &b.trait_ref) {
        (Some(lhs), Some(rhs)) if lhs.def == rhs.def => a.self_ty == b.self_ty,
        _ => false,
    }
}

fn collect_trait_names(source: &SourceFile, resolutions: &Resolutions) -> HashMap<String, DefId> {
    let mut map = HashMap::new();
    for item in &source.items {
        if let ItemKind::Trait(decl) = &item.kind {
            if let Some(def) = resolutions.definition_of(item.id) {
                map.insert(decl.name.name.clone(), def);
            }
        }
    }
    map
}

/// Result of a successful method lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MethodResolution {
    /// Impl the method belongs to.
    pub impl_id: ImplId,
    /// Zero-based slot within the impl's declaration order.
    pub method_slot: usize,
    /// Stable id for the method function.
    pub method_id: ImplFnId,
}

/// Diagnostic emitted by the trait solver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitDiagnostic {
    /// Specific error variant.
    pub error: TraitError,
    /// Primary source range.
    pub span: Span,
}

impl TraitDiagnostic {
    /// Constructs a diagnostic from its error and span.
    #[must_use]
    pub const fn new(error: TraitError, span: Span) -> Self {
        Self { error, span }
    }
}

impl fmt::Display for TraitDiagnostic {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}", self.error)
    }
}

/// Every failure mode the trait solver can report.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TraitError {
    /// Two impls of the same trait target the same concrete self type.
    #[error("conflicting implementations of trait")]
    OverlappingImpls {
        /// First impl's span.
        first_span: Span,
        /// Second impl's span (also the primary span).
        second_span: Span,
    },
    /// A method lookup failed for the given receiver/name pair.
    #[error("no method named `{name}` found for this receiver")]
    UnresolvedMethod {
        /// Method name that was searched.
        name: String,
    },
}

impl TraitError {
    /// Returns a short stable tag useful for snapshot tests.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::OverlappingImpls { .. } => "overlapping-impls",
            Self::UnresolvedMethod { .. } => "unresolved-method",
        }
    }
}

/// Lowers AST types into [`Ty`] handles without going through the
/// inference machinery. Unresolved references fall back to the error
/// type so that later diagnostics remain suppressible.
struct TypeLowerer<'a> {
    tcx: &'a mut TyCtxt,
    resolutions: &'a Resolutions,
    trait_by_name: &'a HashMap<String, DefId>,
}

impl TypeLowerer<'_> {
    fn lower_type(&mut self, ast_ty: &AstType) -> Ty {
        match &ast_ty.kind {
            AstTypeKind::Unit => self.tcx.unit(),
            AstTypeKind::Never => self.tcx.never(),
            AstTypeKind::Infer => self.tcx.error_ty(),
            AstTypeKind::Path(path) => self.lower_type_path(ast_ty.id, path),
            AstTypeKind::Tuple(elems) => {
                let tys: Vec<Ty> = elems.iter().map(|e| self.lower_type(e)).collect();
                self.tcx.intern(TyKind::Tuple(tys))
            }
            AstTypeKind::Array { elem, len } => {
                let elem_ty = self.lower_type(elem);
                let count = evaluate_const_int(len).unwrap_or(0);
                self.tcx.intern(TyKind::Array {
                    elem: elem_ty,
                    len: crate::ArrayLen::Concrete(count),
                })
            }
            AstTypeKind::Slice(inner) => {
                let inner_ty = self.lower_type(inner);
                self.tcx.intern(TyKind::Slice(inner_ty))
            }
            AstTypeKind::Ref { mutability, inner } => {
                let inner_ty = self.lower_type(inner);
                let mutability = match mutability {
                    gossamer_ast::Mutability::Immutable => Mutbl::Not,
                    gossamer_ast::Mutability::Mutable => Mutbl::Mut,
                };
                self.tcx.intern(TyKind::Ref {
                    mutability,
                    inner: inner_ty,
                })
            }
            AstTypeKind::Fn { params, ret, .. } => {
                let inputs: Vec<Ty> = params.iter().map(|p| self.lower_type(p)).collect();
                let output = match ret.as_ref() {
                    Some(ty) => self.lower_type(ty),
                    None => self.tcx.unit(),
                };
                self.tcx.intern(TyKind::FnPtr(FnSig { inputs, output }))
            }
        }
    }

    fn lower_type_path(&mut self, node: NodeId, path: &TypePath) -> Ty {
        let head_name = path
            .segments
            .first()
            .map_or("", |seg| seg.name.name.as_str());
        if let Some(prim) = primitive_from_name(head_name) {
            return prim_to_ty(self.tcx, prim);
        }
        if let Some(resolution) = self.resolutions.get(node) {
            match resolution {
                Resolution::Primitive(prim) => return prim_to_ty(self.tcx, prim),
                Resolution::Def { def, .. } => {
                    let substs = self.lower_substs(path);
                    return self.tcx.intern(TyKind::Adt { def, substs });
                }
                // `use a::Point` then `impl Point`: the import names the type.
                Resolution::Import { .. } => {
                    if let Some(def) = self.resolutions.import_def(node) {
                        let substs = self.lower_substs(path);
                        return self.tcx.intern(TyKind::Adt { def, substs });
                    }
                }
                Resolution::Err | Resolution::Local(_) => {}
            }
        }
        self.tcx.error_ty()
    }

    fn lower_substs(&mut self, path: &TypePath) -> Substs {
        let mut args = Vec::new();
        for segment in &path.segments {
            for arg in &segment.generics {
                match arg {
                    AstGenericArg::Type(ast_ty) => {
                        args.push(GenericArg::Type(self.lower_type(ast_ty)));
                    }
                    AstGenericArg::Const(expr) => {
                        let raw = evaluate_const_int_from_expr(expr).unwrap_or(0);
                        let value = i128::try_from(raw).unwrap_or(0);
                        args.push(GenericArg::Const(value));
                    }
                }
            }
        }
        Substs::from_args(args)
    }

    fn lower_trait_bound(&mut self, bound: &TraitBound) -> Option<TraitRef> {
        let tail = bound.path.segments.last()?;
        let def = *self.trait_by_name.get(&tail.name.name)?;
        let substs = self.lower_substs(&bound.path);
        Some(TraitRef::new(def, substs))
    }

    fn lower_fn_sig(&mut self, decl: &gossamer_ast::FnDecl) -> FnSig {
        let mut inputs = Vec::new();
        for param in &decl.params {
            if let gossamer_ast::FnParam::Typed { ty, .. } = param {
                inputs.push(self.lower_type(ty));
            }
        }
        let output = match decl.ret.as_ref() {
            Some(ty) => self.lower_type(ty),
            None => self.tcx.unit(),
        };
        FnSig { inputs, output }
    }
}

fn primitive_from_name(name: &str) -> Option<PrimitiveTy> {
    Some(match name {
        "bool" => PrimitiveTy::Bool,
        "char" => PrimitiveTy::Char,
        "String" => PrimitiveTy::String,
        "i8" => PrimitiveTy::Int(IntWidth::W8),
        "i16" => PrimitiveTy::Int(IntWidth::W16),
        "i32" => PrimitiveTy::Int(IntWidth::W32),
        "i64" => PrimitiveTy::Int(IntWidth::W64),
        "i128" => PrimitiveTy::Int(IntWidth::W128),
        "isize" => PrimitiveTy::Int(IntWidth::Size),
        "u8" => PrimitiveTy::UInt(IntWidth::W8),
        "u16" => PrimitiveTy::UInt(IntWidth::W16),
        "u32" => PrimitiveTy::UInt(IntWidth::W32),
        "u64" => PrimitiveTy::UInt(IntWidth::W64),
        "u128" => PrimitiveTy::UInt(IntWidth::W128),
        "usize" => PrimitiveTy::UInt(IntWidth::Size),
        "f32" => PrimitiveTy::Float(FloatWidth::W32),
        "f64" => PrimitiveTy::Float(FloatWidth::W64),
        _ => return None,
    })
}

fn prim_to_ty(tcx: &mut TyCtxt, prim: PrimitiveTy) -> Ty {
    match prim {
        PrimitiveTy::Bool => tcx.bool_ty(),
        PrimitiveTy::Char => tcx.char_ty(),
        PrimitiveTy::String => tcx.string_ty(),
        PrimitiveTy::Int(width) => tcx.int_ty(int_ty_from_width(width, true)),
        PrimitiveTy::UInt(width) => tcx.int_ty(int_ty_from_width(width, false)),
        PrimitiveTy::Float(FloatWidth::W32) => tcx.float_ty(FloatTy::F32),
        PrimitiveTy::Float(FloatWidth::W64) => tcx.float_ty(FloatTy::F64),
        PrimitiveTy::Never => tcx.never(),
        PrimitiveTy::Unit => tcx.unit(),
    }
}

fn int_ty_from_width(width: IntWidth, signed: bool) -> IntTy {
    match (signed, width) {
        (true, IntWidth::W8) => IntTy::I8,
        (true, IntWidth::W16) => IntTy::I16,
        (true, IntWidth::W32) => IntTy::I32,
        (true, IntWidth::W64) => IntTy::I64,
        (true, IntWidth::W128) => IntTy::I128,
        (true, IntWidth::Size) => IntTy::Isize,
        (false, IntWidth::W8) => IntTy::U8,
        (false, IntWidth::W16) => IntTy::U16,
        (false, IntWidth::W32) => IntTy::U32,
        (false, IntWidth::W64) => IntTy::U64,
        (false, IntWidth::W128) => IntTy::U128,
        (false, IntWidth::Size) => IntTy::Usize,
    }
}

fn evaluate_const_int(expr: &Expr) -> Option<usize> {
    evaluate_const_int_from_expr(expr).and_then(|v| usize::try_from(v).ok())
}

fn evaluate_const_int_from_expr(expr: &Expr) -> Option<u128> {
    if let ExprKind::Literal(Literal::Int(text)) = &expr.kind {
        let cleaned = strip_int_suffix(text).replace('_', "");
        return parse_int(&cleaned);
    }
    None
}

fn parse_int(text: &str) -> Option<u128> {
    if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u128::from_str_radix(rest, 16).ok();
    }
    if let Some(rest) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
        return u128::from_str_radix(rest, 2).ok();
    }
    if let Some(rest) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O")) {
        return u128::from_str_radix(rest, 8).ok();
    }
    text.parse::<u128>().ok()
}

fn strip_int_suffix(text: &str) -> String {
    const SUFFIXES: &[&str] = &[
        "i128", "u128", "isize", "usize", "i64", "u64", "i32", "u32", "i16", "u16", "i8", "u8",
        "f32", "f64",
    ];
    for suffix in SUFFIXES {
        if let Some(stripped) = text.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    text.to_string()
}
