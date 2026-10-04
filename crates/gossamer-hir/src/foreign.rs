//! The C signature of a function declared in an `unsafe extern "C"` block,
//! in the compact form every tier keys a foreign call by, and the pass that
//! routes each call to one dispatcher name.
//!
//! The signature spells each parameter, then `>`, then the result:
//!
//! | spelling | parameter |
//! |---|---|
//! | a class character | a scalar of that C class ([`gossamer_types::c_class_width`]) |
//! | `p` or `P` and a class | a slice of scalars: a pointer to its first element, read-only or written back |
//! | `r` or `R` | a `#[repr(C)]` struct or a fixed array: a pointer to a C-layout copy, read-only or written back |
//!
//! The result is a class character, or `v` for none. An aggregate
//! parameter's leaves are spelled separately by [`foreign_layouts`].
//!
//! An `ffi::Ptr`, an `Option<ffi::Ptr>`, and a C function pointer cross as
//! the address class `L`; an out-parameter (`&mut` a scalar or a pointer
//! form) crosses as `R`, a one-element array the boundary pass in
//! `foreign_boundary` fills and reads back.

use gossamer_types::{CStep, FnSig, IntTy, Mutbl, Ty, TyCtxt, TyKind};

use crate::tree::{FnOrigin, HirFn};

/// The registered name of the injected `ffi::Ptr` struct.
pub(crate) const PTR_TYPE: &str = "__gos_ffi_Ptr";
/// The registered name of the injected `ffi::Handle` struct.
pub(crate) const HANDLE_TYPE: &str = "__gos_ffi_Handle";

/// How a value crosses the C boundary.
#[derive(Debug, Clone)]
pub(crate) enum CValue {
    /// A scalar of this C class.
    Scalar(char),
    /// `ffi::Ptr<T>`: a non-null address.
    Ptr,
    /// `Option<ffi::Ptr<T>>`: an address, `None` for null.
    OptPtr,
    /// A C function pointer the program fills with a named function.
    Callback(FnSig),
}

/// The name and type arguments of the struct or enum `ty`.
fn adt_parts(tcx: &TyCtxt, ty: Ty) -> Option<(&str, Vec<Ty>)> {
    let TyKind::Adt { def, substs } = tcx.kind_of(ty) else {
        return None;
    };
    Some((tcx.def_name(*def)?, substs.types()))
}

/// The pointee of `ty` when it is `ffi::Ptr<T>`.
pub(crate) fn ptr_elem(tcx: &TyCtxt, ty: Ty) -> Option<Ty> {
    let (name, args) = adt_parts(tcx, ty)?;
    (name == PTR_TYPE).then(|| args.first().copied()).flatten()
}

/// The payload of `ty` when it is `Option<T>`.
pub(crate) fn option_payload(tcx: &TyCtxt, ty: Ty) -> Option<Ty> {
    let (name, args) = adt_parts(tcx, ty)?;
    (name == "Option").then(|| args.first().copied()).flatten()
}

/// The value type of `ty` when it is `ffi::Handle<T>`.
pub(crate) fn handle_value(tcx: &TyCtxt, ty: Ty) -> Option<Ty> {
    let (name, args) = adt_parts(tcx, ty)?;
    (name == HANDLE_TYPE)
        .then(|| args.first().copied())
        .flatten()
}

/// How a value of `ty` crosses the C boundary, when it is a scalar, a
/// pointer form, or a C function pointer.
pub(crate) fn classify_value(tcx: &TyCtxt, ty: Ty) -> Option<CValue> {
    if let Some(class) = tcx.c_scalar_class(ty) {
        return Some(CValue::Scalar(class));
    }
    if ptr_elem(tcx, ty).is_some() {
        return Some(CValue::Ptr);
    }
    if option_payload(tcx, ty)
        .and_then(|payload| ptr_elem(tcx, payload))
        .is_some()
    {
        return Some(CValue::OptPtr);
    }
    match tcx.kind_of(ty) {
        TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => Some(CValue::Callback(sig.clone())),
        _ => None,
    }
}

/// The class a value of `ty` crosses in: its scalar class, or `L` for an
/// address.
pub(crate) fn value_class(tcx: &TyCtxt, ty: Ty) -> Option<char> {
    Some(match classify_value(tcx, ty)? {
        CValue::Scalar(class) => class,
        CValue::Ptr | CValue::OptPtr | CValue::Callback(_) => 'L',
    })
}

/// The type an out-parameter `&mut T` stages its value in: `T` for a
/// scalar, `u64` for a pointer form. `None` when `ty` is not one.
pub(crate) fn out_param_word(tcx: &mut TyCtxt, ty: Ty) -> Option<(Ty, CValue)> {
    let TyKind::Ref {
        mutability: Mutbl::Mut,
        inner,
    } = tcx.kind_of(ty)
    else {
        return None;
    };
    let inner = *inner;
    match classify_value(tcx, inner)? {
        CValue::Callback(_) => None,
        CValue::Scalar(class) => Some((inner, CValue::Scalar(class))),
        value => Some((tcx.int_ty(IntTy::U64), value)),
    }
}

/// The compact signature of the foreign function `decl`, or `None` when it
/// is not a foreign function or a type has no C class (the checker has
/// reported that).
#[must_use]
pub fn foreign_signature(tcx: &TyCtxt, decl: &HirFn) -> Option<String> {
    if decl.origin != FnOrigin::Foreign {
        return None;
    }
    let mut sig = String::with_capacity(decl.params.len() * 2 + 2);
    for param in &decl.params {
        sig.push_str(&param_spelling(tcx, param.ty)?);
    }
    sig.push('>');
    match decl.ret {
        None => sig.push('v'),
        Some(ret) if matches!(tcx.kind_of(ret), TyKind::Unit) => sig.push('v'),
        Some(ret) => sig.push(value_class(tcx, ret)?),
    }
    Some(sig)
}

/// The type a parameter passes, with a `&` or `&mut` peeled, and whether its
/// writes come back.
fn peeled(tcx: &TyCtxt, ty: Ty) -> (Ty, bool) {
    match tcx.kind_of(ty) {
        TyKind::Ref { mutability, inner } => (*inner, *mutability == Mutbl::Mut),
        _ => (ty, false),
    }
}

fn param_spelling(tcx: &TyCtxt, ty: Ty) -> Option<String> {
    if let Some(class) = value_class(tcx, ty) {
        return Some(class.to_string());
    }
    let (inner, writable) = peeled(tcx, ty);
    if writable && classify_value(tcx, inner).is_some() {
        return Some("R".to_string());
    }
    match tcx.kind_of(inner) {
        TyKind::Slice(elem) => {
            let class = tcx.c_scalar_class(*elem)?;
            Some(format!("{}{class}", if writable { 'P' } else { 'p' }))
        }
        TyKind::Adt { .. } => {
            tcx.c_leaves(inner)?;
            Some(if writable { "R" } else { "r" }.to_string())
        }
        _ => None,
    }
}

/// The C layout of each struct or array parameter of `decl`, `|`-separated
/// in parameter order, empty for a parameter that is neither. A layout is
/// `size;leaf,leaf,..`, each leaf `steps@offset:class` with steps such as
/// `f2.i3` (field 2, then element 3). An out-parameter is laid out as the
/// one-element array it is staged in.
#[must_use]
pub fn foreign_layouts(tcx: &TyCtxt, decl: &HirFn) -> String {
    decl.params
        .iter()
        .map(|param| {
            let (inner, writable) = peeled(tcx, param.ty);
            if writable && let Some(value) = classify_value(tcx, inner) {
                let class = match value {
                    CValue::Scalar(class) => class,
                    _ => 'L',
                };
                let width = gossamer_types::c_class_width(class).unwrap_or(8);
                return format!("{width};i0@0:{class}");
            }
            if !matches!(tcx.kind_of(inner), TyKind::Adt { .. }) {
                return String::new();
            }
            aggregate_layout(tcx, inner).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// The C layout of the plain-data struct or fixed array `ty`, spelled as
/// [`foreign_layouts`] spells one parameter's.
pub(crate) fn aggregate_layout(tcx: &TyCtxt, ty: Ty) -> Option<String> {
    let (size, leaves) = tcx.c_leaves(ty)?;
    let leaves: Vec<String> = leaves
        .iter()
        .map(|leaf| {
            let steps: Vec<String> = leaf
                .steps
                .iter()
                .map(|step| match step {
                    CStep::Field(i) => format!("f{i}"),
                    CStep::Index(i) => format!("i{i}"),
                })
                .collect();
            format!("{}@{}:{}", steps.join("."), leaf.offset, leaf.class)
        })
        .collect();
    Some(format!("{size};{}", leaves.join(",")))
}

/// The name every routed foreign call reaches: a native builtin on the
/// bytecode tier, an intrinsic in MIR.
pub const FOREIGN_DISPATCHER: &str = "__gos_ffi_call";

/// A copy of `program` in which each foreign `fn` item is dropped and each
/// call to one becomes
/// `__gos_ffi_call(symbol, signature, library, layouts, args..)`, with the
/// library `""` when none is named. `None` when the program declares no
/// foreign function.
#[must_use]
pub fn route_foreign_calls(
    program: &crate::tree::HirProgram,
    tcx: &mut TyCtxt,
) -> Option<crate::tree::HirProgram> {
    use crate::tree::HirItemKind;
    let mut foreign = std::collections::HashMap::new();
    for item in &program.items {
        if let HirItemKind::Fn(decl) = &item.kind
            && let (Some(def), Some(sig)) = (item.def, foreign_signature(tcx, decl))
        {
            let link = decl.foreign_link.clone().unwrap_or_default();
            let layouts = foreign_layouts(tcx, decl);
            let symbol = decl
                .foreign_symbol
                .clone()
                .unwrap_or_else(|| decl.name.name.clone());
            foreign.insert(def, [symbol, sig, link, layouts]);
        }
    }
    if foreign.is_empty() {
        return None;
    }
    let mut routed = program.clone();
    routed.items.retain(
        |item| !matches!(&item.kind, HirItemKind::Fn(decl) if decl.origin == FnOrigin::Foreign),
    );
    let string = tcx.string_ty();
    let mut rewriter = Router {
        foreign: &foreign,
        string,
    };
    for item in &mut routed.items {
        rewriter.item(item);
    }
    Some(routed)
}

type ForeignTable = std::collections::HashMap<gossamer_resolve::DefId, [String; 4]>;

struct Router<'a> {
    foreign: &'a ForeignTable,
    string: Ty,
}

impl Router<'_> {
    fn item(&mut self, item: &mut crate::tree::HirItem) {
        use crate::tree::HirItemKind;
        match &mut item.kind {
            HirItemKind::Fn(f) => self.function(f),
            HirItemKind::Impl(imp) => imp.methods.iter_mut().for_each(|f| self.function(f)),
            HirItemKind::Trait(t) => t.methods.iter_mut().for_each(|f| self.function(f)),
            HirItemKind::Const(c) => self.expr(&mut c.value),
            HirItemKind::Static(s) => self.expr(&mut s.value),
            HirItemKind::Adt(_) => {}
        }
    }

    fn function(&mut self, f: &mut HirFn) {
        if let Some(body) = &mut f.body {
            self.block(&mut body.block);
        }
    }

    fn block(&mut self, block: &mut crate::tree::HirBlock) {
        use crate::tree::HirStmtKind;
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init: Some(e), .. }
                | HirStmtKind::Expr { expr: e, .. }
                | HirStmtKind::Defer(e) => self.expr(e),
                HirStmtKind::Item(item) => self.item(item),
                HirStmtKind::Let { init: None, .. } => {}
            }
        }
        if let Some(tail) = &mut block.tail {
            self.expr(tail);
        }
    }

    fn expr(&mut self, expr: &mut crate::tree::HirExpr) {
        use crate::tree::{HirExpr, HirExprKind, HirLiteral};
        if let HirExprKind::Block(block) = &mut expr.kind {
            self.block(block);
            return;
        }
        crate::tree::for_each_child_expr_mut(expr, &mut |child| self.expr(child));
        let HirExprKind::Call { callee, args } = &mut expr.kind else {
            return;
        };
        let HirExprKind::Path { def: Some(def), .. } = &callee.kind else {
            return;
        };
        let Some(literals) = self.foreign.get(def) else {
            return;
        };
        let literal = |text: &str, like: &HirExpr| HirExpr {
            id: like.id,
            span: like.span,
            ty: self.string,
            kind: HirExprKind::Literal(HirLiteral::String(text.to_string())),
        };
        let mut routed_args: Vec<HirExpr> =
            literals.iter().map(|text| literal(text, callee)).collect();
        routed_args.append(args);
        *args = routed_args;
        callee.kind = HirExprKind::Path {
            segments: vec![gossamer_ast::Ident::new(FOREIGN_DISPATCHER)],
            def: None,
        };
    }
}
