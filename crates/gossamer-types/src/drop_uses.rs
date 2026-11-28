//! Refusals for types with `impl Drop`.
//!
//! A drop type is a shared handle: `let b = a` shares one value, and `drop`
//! runs once, when the last reference goes. Its fields live in one record, so
//! the type is a struct whose field types are known where it is declared; and
//! `clone()` would make a second value owning the same resource, which `drop`
//! would then release twice.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use gossamer_ast::visitor::{Visitor, walk_expr, walk_item};
use gossamer_ast::{
    Expr, ExprKind, Item, ItemKind, SourceFile, StructDecl, TraitBound, Type, TypeKind,
};

use crate::{TyCtxt, TyKind, TypeDiagnostic, TypeError, TypeTable};

/// The refusals of `impl Drop` on a type that cannot hold it, and of
/// `clone()` on a drop value.
#[must_use]
pub fn check_drop_uses(sf: &SourceFile, table: &TypeTable, tcx: &TyCtxt) -> Vec<TypeDiagnostic> {
    let mut decls = Decls::default();
    decls.visit_source_file(sf);
    let mut out = Vec::new();
    for (name, span, reason) in &decls.refused {
        out.push(TypeDiagnostic {
            error: TypeError::DropTypeShape {
                ty: name.clone(),
                reason: (*reason).to_string(),
            },
            span: *span,
        });
    }
    let mut walk = Clones {
        table,
        tcx,
        drop_types: &decls.drop_types,
        out: Vec::new(),
    };
    walk.visit_source_file(sf);
    out.extend(walk.out);
    out.into_iter()
        .filter(|diag| sf.is_program_text(diag.span))
        .collect()
}

fn is_drop_bound(bound: &TraitBound) -> bool {
    bound
        .path
        .segments
        .last()
        .is_some_and(|segment| segment.name.name == "Drop")
}

/// The name a path type ends in, and whether it carries generic arguments.
fn type_name(ty: &Type) -> Option<(String, bool)> {
    let TypeKind::Path(path) = &ty.kind else {
        return None;
    };
    let last = path.segments.last()?;
    Some((last.name.name.clone(), !last.generics.is_empty()))
}

#[derive(Default)]
struct Decls {
    structs: Vec<(String, bool)>,
    enums: Vec<String>,
    drop_types: HashSet<String>,
    refused: Vec<(String, gossamer_lex::Span, &'static str)>,
    impls: Vec<(String, bool, gossamer_lex::Span)>,
}

impl Decls {
    fn note_struct(&mut self, decl: &StructDecl, span: gossamer_lex::Span) {
        let generic = !decl.generics.params.is_empty();
        self.structs.push((decl.name.name.clone(), generic));
        if decl.traits.iter().any(is_drop_bound) {
            self.impls.push((decl.name.name.clone(), generic, span));
        }
    }
}

impl Visitor for Decls {
    fn visit_source_file(&mut self, sf: &SourceFile) {
        for item in &sf.items {
            self.visit_item(item);
        }
        let impls = std::mem::take(&mut self.impls);
        for (name, generic, span) in impls {
            if self.enums.contains(&name) {
                self.refused.push((name, span, "an enum"));
            } else if generic || self.structs.iter().any(|(s, g)| *s == name && *g) {
                self.refused.push((name, span, "generic"));
            } else {
                self.drop_types.insert(name);
            }
        }
    }

    fn visit_item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Struct(decl) => self.note_struct(decl, item.span),
            ItemKind::Enum(decl) => {
                self.enums.push(decl.name.name.clone());
                if decl.traits.iter().any(is_drop_bound) {
                    self.impls.push((decl.name.name.clone(), false, item.span));
                }
            }
            ItemKind::Impl(decl) => {
                if decl.trait_ref.as_ref().is_some_and(is_drop_bound)
                    && let Some((name, has_args)) = type_name(&decl.self_ty)
                {
                    let generic = has_args || !decl.generics.params.is_empty();
                    self.impls.push((name, generic, item.span));
                }
            }
            _ => {}
        }
        walk_item(self, item);
    }
}

struct Clones<'a> {
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    drop_types: &'a HashSet<String>,
    out: Vec<TypeDiagnostic>,
}

impl Visitor for Clones<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &expr.kind
            && name.name == "clone"
            && args.is_empty()
            && let Some(mut ty) = self.table.get(receiver.id)
        {
            while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
                ty = *inner;
            }
            if let Some(TyKind::Adt { def, .. }) = self.tcx.kind(ty)
                && let Some(type_name) = self.tcx.def_name(*def)
                && let Some(short) = type_name.rsplit("::").next()
                && self.drop_types.contains(short)
            {
                self.out.push(TypeDiagnostic {
                    error: TypeError::DropClone {
                        ty: short.to_string(),
                    },
                    span: expr.span,
                });
            }
        }
        walk_expr(self, expr);
    }
}
