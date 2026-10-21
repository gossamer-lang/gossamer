//! Reads a field directly where a call names a method that only answers it.
//!
//! `fn bytes(&self) -> Vec<u8> { self.data }` answers the value of the field,
//! so `face.bytes()` and `face.data` denote the same value. Spelling the call
//! as the field read lets every later decision - whether a binding shares or
//! copies, whether a parameter stays read-only - judge it as the projection it
//! is, instead of as a call answering a fresh copy of the field.

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_resolve::DefId;
use gossamer_types::{TyCtxt, TyKind};

use crate::ids::HirIdGenerator;
use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirFn, HirItemKind, HirProgram, HirStmtKind,
    for_each_child_expr_mut,
};

/// Rewrites every call of a field accessor in `program` into the field read.
pub(crate) fn inline_field_accessors(
    program: &mut HirProgram,
    tcx: &TyCtxt,
    ids: &mut HirIdGenerator,
) {
    let mut accessors: HashMap<(DefId, String), Ident> = HashMap::new();
    for item in &program.items {
        let HirItemKind::Impl(imp) = &item.kind else {
            continue;
        };
        let TyKind::Adt { def, .. } = tcx.kind_of(imp.self_ty) else {
            continue;
        };
        for method in &imp.methods {
            if let Some(field) = accessed_field(method) {
                accessors.insert((*def, method.name.name.clone()), field.clone());
            }
        }
    }
    if accessors.is_empty() {
        return;
    }
    let mut rewriter = Rewriter {
        tcx,
        ids,
        accessors: &accessors,
    };
    for item in &mut program.items {
        match &mut item.kind {
            HirItemKind::Fn(f) => rewriter.visit_fn(f),
            HirItemKind::Impl(imp) => imp.methods.iter_mut().for_each(|f| rewriter.visit_fn(f)),
            HirItemKind::Trait(t) => t.methods.iter_mut().for_each(|f| rewriter.visit_fn(f)),
            HirItemKind::Const(_) | HirItemKind::Static(_) | HirItemKind::Adt(_) => {}
        }
    }
}

/// The field `method` answers when its whole body is `self.<field>` and it
/// takes nothing but the receiver.
fn accessed_field(method: &HirFn) -> Option<&Ident> {
    if !method.has_self || method.params.len() != 1 || method.is_comptime || method.is_unsafe {
        return None;
    }
    let body = method.body.as_ref()?;
    let HirBlock { stmts, tail, .. } = &body.block;
    if !stmts.is_empty() {
        return None;
    }
    let tail = tail.as_deref()?;
    if method.ret != Some(tail.ty) {
        return None;
    }
    let HirExprKind::Field { receiver, name } = &tail.kind else {
        return None;
    };
    let HirExprKind::Path { segments, .. } = &receiver.kind else {
        return None;
    };
    matches!(segments.as_slice(), [s] if s.name == "self").then_some(name)
}

struct Rewriter<'a> {
    tcx: &'a TyCtxt,
    ids: &'a mut HirIdGenerator,
    accessors: &'a HashMap<(DefId, String), Ident>,
}

impl Rewriter<'_> {
    fn visit_fn(&mut self, f: &mut HirFn) {
        if let Some(body) = &mut f.body {
            self.visit_block(&mut body.block);
        }
    }

    fn visit_block(&mut self, block: &mut HirBlock) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                HirStmtKind::Let { init: Some(e), .. }
                | HirStmtKind::Expr { expr: e, .. }
                | HirStmtKind::Defer(e) => self.visit_expr(e),
                HirStmtKind::Let { init: None, .. } | HirStmtKind::Item(_) => {}
            }
        }
        if let Some(tail) = &mut block.tail {
            self.visit_expr(tail);
        }
    }

    fn visit_expr(&mut self, expr: &mut HirExpr) {
        for_each_child_expr_mut(expr, &mut |child| self.visit_expr(child));
        let HirExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &mut expr.kind
        else {
            return;
        };
        if !args.is_empty() {
            return;
        }
        let mut receiver_ty = receiver.ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(receiver_ty) {
            receiver_ty = *inner;
        }
        let TyKind::Adt { def, .. } = self.tcx.kind_of(receiver_ty) else {
            return;
        };
        let Some(field) = self.accessors.get(&(*def, name.name.clone())) else {
            return;
        };
        let receiver = std::mem::replace(
            receiver,
            Box::new(HirExpr {
                id: self.ids.next(),
                span: expr.span,
                ty: expr.ty,
                kind: HirExprKind::Placeholder,
            }),
        );
        expr.id = self.ids.next();
        expr.kind = HirExprKind::Field {
            receiver,
            name: field.clone(),
        };
    }
}
