//! Mutating tree-walkers that mirror [`walk`](super::walk) but accept
//! `&mut` references so visitors can rewrite AST nodes in place.

#![forbid(unsafe_code)]

use super::VisitorMut;
use crate::expr::{ArrayExpr, Block, Expr, ExprKind, MatchArm, PathExpr, SelectArm, SelectOp};
use crate::items::{
    ConstDecl, EnumDecl, FnDecl, FnParam, ImplDecl, ImplItem, Item, ItemKind, ModBody, ModDecl,
    StaticDecl, StructBody, StructDecl, TraitDecl, TraitItem, TypeAliasDecl,
};
use crate::pattern::{Pattern, PatternKind};
use crate::source_file::{SourceFile, UseDecl};
use crate::stmt::{Stmt, StmtKind};
use crate::ty::{GenericArg, Type, TypeKind, TypePath};

/// Walks into every child of a [`SourceFile`] mutably.
pub fn walk_source_file_mut<V: VisitorMut + ?Sized>(visitor: &mut V, source_file: &mut SourceFile) {
    for use_decl in &mut source_file.uses {
        visitor.visit_use_decl(use_decl);
    }
    for item in &mut source_file.items {
        visitor.visit_item(item);
    }
    for stmt in &mut source_file.top_level_stmts {
        visitor.visit_stmt(stmt);
    }
}

/// Walks into every child of a [`UseDecl`] mutably.
///
/// A `use` declaration carries only identifiers and string data, so this
/// walker is deliberately a no-op.
pub fn walk_use_decl_mut<V: VisitorMut + ?Sized>(_visitor: &mut V, _use_decl: &mut UseDecl) {}

/// Walks into every child of an [`Item`] mutably.
pub fn walk_item_mut<V: VisitorMut + ?Sized>(visitor: &mut V, item: &mut Item) {
    match &mut item.kind {
        ItemKind::Fn(decl) => walk_fn_decl_mut(visitor, decl),
        ItemKind::Struct(decl) => walk_struct_decl_mut(visitor, decl),
        ItemKind::Enum(decl) => walk_enum_decl_mut(visitor, decl),
        ItemKind::Trait(decl) => walk_trait_decl_mut(visitor, decl),
        ItemKind::Impl(decl) => walk_impl_decl_mut(visitor, decl),
        ItemKind::TypeAlias(decl) => walk_type_alias_decl_mut(visitor, decl),
        ItemKind::Const(decl) => walk_const_decl_mut(visitor, decl),
        ItemKind::Static(decl) => walk_static_decl_mut(visitor, decl),
        ItemKind::Mod(decl) => walk_mod_decl_mut(visitor, decl),
        ItemKind::AttrItem(_) => {}
    }
}

fn walk_fn_decl_mut<V: VisitorMut + ?Sized>(visitor: &mut V, decl: &mut FnDecl) {
    for param in &mut decl.params {
        if let FnParam::Typed { pattern, ty, .. } = param {
            visitor.visit_pattern(pattern);
            visitor.visit_type(ty);
        }
    }
    if let Some(ret) = &mut decl.ret {
        visitor.visit_type(ret);
    }
    if let Some(body) = &mut decl.body {
        visitor.visit_expr(body);
    }
}

fn walk_struct_decl_mut<V: VisitorMut + ?Sized>(visitor: &mut V, decl: &mut StructDecl) {
    walk_struct_body_mut(visitor, &mut decl.body);
    walk_impl_items_mut(visitor, &mut decl.items);
}

fn walk_struct_body_mut<V: VisitorMut + ?Sized>(visitor: &mut V, body: &mut StructBody) {
    match body {
        StructBody::Named(fields) => {
            for field in fields {
                visitor.visit_type(&mut field.ty);
            }
        }
        StructBody::Tuple(fields) => {
            for field in fields {
                visitor.visit_type(&mut field.ty);
            }
        }
        StructBody::Unit => {}
    }
}

fn walk_enum_decl_mut<V: VisitorMut + ?Sized>(visitor: &mut V, decl: &mut EnumDecl) {
    for variant in &mut decl.variants {
        walk_struct_body_mut(visitor, &mut variant.body);
        if let Some(disc) = &mut variant.discriminant {
            visitor.visit_expr(disc);
        }
    }
    walk_impl_items_mut(visitor, &mut decl.items);
}

fn walk_trait_decl_mut<V: VisitorMut + ?Sized>(visitor: &mut V, decl: &mut TraitDecl) {
    for item in &mut decl.items {
        walk_trait_item_mut(visitor, item);
    }
}

fn walk_trait_item_mut<V: VisitorMut + ?Sized>(visitor: &mut V, item: &mut TraitItem) {
    match item {
        TraitItem::Fn(decl) => walk_fn_decl_mut(visitor, decl),
        TraitItem::Type { default, .. } => {
            if let Some(ty) = default {
                visitor.visit_type(ty);
            }
        }
        TraitItem::Const { ty, default, .. } => {
            visitor.visit_type(ty);
            if let Some(expr) = default {
                visitor.visit_expr(expr);
            }
        }
    }
}

fn walk_impl_decl_mut<V: VisitorMut + ?Sized>(visitor: &mut V, decl: &mut ImplDecl) {
    visitor.visit_type(&mut decl.self_ty);
    walk_impl_items_mut(visitor, &mut decl.items);
}

/// The items of an `impl` block, or of a type's body before its items move
/// into the blocks they stand for.
fn walk_impl_items_mut<V: VisitorMut + ?Sized>(visitor: &mut V, items: &mut [ImplItem]) {
    for item in items {
        match item {
            ImplItem::Fn(fn_decl) => walk_fn_decl_mut(visitor, fn_decl),
            ImplItem::Type { ty, .. } => visitor.visit_type(ty),
            ImplItem::Const { ty, value, .. } => {
                visitor.visit_type(ty);
                visitor.visit_expr(value);
            }
        }
    }
}

fn walk_type_alias_decl_mut<V: VisitorMut + ?Sized>(visitor: &mut V, decl: &mut TypeAliasDecl) {
    visitor.visit_type(&mut decl.ty);
}

fn walk_const_decl_mut<V: VisitorMut + ?Sized>(visitor: &mut V, decl: &mut ConstDecl) {
    visitor.visit_type(&mut decl.ty);
    visitor.visit_expr(&mut decl.value);
}

fn walk_static_decl_mut<V: VisitorMut + ?Sized>(visitor: &mut V, decl: &mut StaticDecl) {
    visitor.visit_type(&mut decl.ty);
    visitor.visit_expr(&mut decl.value);
}

fn walk_mod_decl_mut<V: VisitorMut + ?Sized>(visitor: &mut V, decl: &mut ModDecl) {
    if let ModBody::Inline(items) = &mut decl.body {
        for item in items {
            visitor.visit_item(item);
        }
    }
}

/// Walks into every child of a [`Stmt`] mutably.
pub fn walk_stmt_mut<V: VisitorMut + ?Sized>(visitor: &mut V, stmt: &mut Stmt) {
    match &mut stmt.kind {
        StmtKind::Let { pattern, ty, init } => {
            visitor.visit_pattern(pattern);
            if let Some(ty) = ty {
                visitor.visit_type(ty);
            }
            if let Some(expr) = init {
                visitor.visit_expr(expr);
            }
        }
        StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => {
            visitor.visit_expr(expr);
        }
        StmtKind::Item(item) => visitor.visit_item(item),
    }
}

/// Walks into every child of an [`Expr`] mutably.
pub fn walk_expr_mut<V: VisitorMut + ?Sized>(visitor: &mut V, expr: &mut Expr) {
    match &mut expr.kind {
        ExprKind::Literal(lit) => visitor.visit_literal(lit),
        ExprKind::Path(path) => visitor.visit_path_expr(path),
        ExprKind::Block(block) | ExprKind::Unsafe(block) => visitor.visit_block(block),
        ExprKind::Try(inner) => visitor.visit_expr(inner),
        ExprKind::Unary { operand, .. }
        | ExprKind::FieldAccess {
            receiver: operand, ..
        } => visitor.visit_expr(operand),
        ExprKind::Loop { body, .. } => visitor.visit_expr(body),
        ExprKind::Tuple(items) | ExprKind::MapLiteral(items) | ExprKind::SetLiteral(items) => {
            walk_exprs_mut(visitor, items);
        }
        ExprKind::Array(array_expr) | ExprKind::FixedArray(array_expr) => {
            walk_array_expr_mut(visitor, array_expr);
        }
        ExprKind::Select(arms) => {
            for arm in arms {
                visitor.visit_select_arm(arm);
            }
        }
        ExprKind::Continue { .. } => {}
        _ => walk_expr_mut_compound(visitor, expr),
    }
}

fn walk_expr_mut_compound<V: VisitorMut + ?Sized>(visitor: &mut V, expr: &mut Expr) {
    match &mut expr.kind {
        ExprKind::Call { callee, args } => {
            visitor.visit_expr(callee);
            walk_exprs_mut(visitor, args);
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            visitor.visit_expr(receiver);
            walk_exprs_mut(visitor, args);
        }
        ExprKind::Index { base, index } => {
            visitor.visit_expr(base);
            visitor.visit_expr(index);
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            visitor.visit_expr(lhs);
            visitor.visit_expr(rhs);
        }
        ExprKind::Assign { place, value, .. } => {
            visitor.visit_expr(place);
            visitor.visit_expr(value);
        }
        ExprKind::Cast { value, ty, .. } => {
            visitor.visit_expr(value);
            visitor.visit_type(ty);
        }
        _ => walk_expr_mut_control(visitor, expr),
    }
}

fn walk_expr_mut_control<V: VisitorMut + ?Sized>(visitor: &mut V, expr: &mut Expr) {
    match &mut expr.kind {
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            visitor.visit_expr(condition);
            visitor.visit_expr(then_branch);
            if let Some(else_branch) = else_branch {
                visitor.visit_expr(else_branch);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            visitor.visit_expr(scrutinee);
            for arm in arms {
                visitor.visit_match_arm(arm);
            }
        }
        ExprKind::While {
            condition, body, ..
        } => {
            visitor.visit_expr(condition);
            visitor.visit_expr(body);
        }
        ExprKind::For {
            pattern,
            iter,
            body,
            ..
        } => {
            visitor.visit_pattern(pattern);
            visitor.visit_expr(iter);
            visitor.visit_expr(body);
        }
        _ => walk_expr_mut_other(visitor, expr),
    }
}

fn walk_expr_mut_other<V: VisitorMut + ?Sized>(visitor: &mut V, expr: &mut Expr) {
    match &mut expr.kind {
        ExprKind::Closure {
            params, ret, body, ..
        } => {
            for param in params {
                visitor.visit_pattern(&mut param.pattern);
                if let Some(ty) = &mut param.ty {
                    visitor.visit_type(ty);
                }
            }
            if let Some(ty) = ret {
                visitor.visit_type(ty);
            }
            visitor.visit_expr(body);
        }
        ExprKind::Return(value) | ExprKind::Break { value, .. } => {
            if let Some(expr) = value {
                visitor.visit_expr(expr);
            }
        }
        ExprKind::Struct { fields, base, .. } => {
            for field in fields {
                if let Some(value) = &mut field.value {
                    visitor.visit_expr(value);
                }
            }
            if let Some(base) = base {
                visitor.visit_expr(base);
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(expr) = start {
                visitor.visit_expr(expr);
            }
            if let Some(expr) = end {
                visitor.visit_expr(expr);
            }
        }
        _ => {}
    }
}

fn walk_exprs_mut<V: VisitorMut + ?Sized>(visitor: &mut V, exprs: &mut [Expr]) {
    for expr in exprs {
        visitor.visit_expr(expr);
    }
}

fn walk_array_expr_mut<V: VisitorMut + ?Sized>(visitor: &mut V, array: &mut ArrayExpr) {
    match array {
        ArrayExpr::List(items) => walk_exprs_mut(visitor, items),
        ArrayExpr::Repeat { value, count } => {
            visitor.visit_expr(value);
            visitor.visit_expr(count);
        }
    }
}

/// Walks into every child of a [`Type`] mutably.
pub fn walk_type_mut<V: VisitorMut + ?Sized>(visitor: &mut V, ty: &mut Type) {
    match &mut ty.kind {
        TypeKind::Unit | TypeKind::Never | TypeKind::Infer => {}
        TypeKind::Path(path) => visitor.visit_type_path(path),
        TypeKind::Tuple(items) => {
            for item in items {
                visitor.visit_type(item);
            }
        }
        TypeKind::Array { elem, len } => {
            visitor.visit_type(elem);
            visitor.visit_expr(len);
        }
        TypeKind::Slice(elem) => visitor.visit_type(elem),
        TypeKind::Ref { inner, .. } => visitor.visit_type(inner),
        TypeKind::Fn { params, ret, .. } => {
            for param in params {
                visitor.visit_type(param);
            }
            if let Some(ret) = ret {
                visitor.visit_type(ret);
            }
        }
    }
}

/// Walks into every child of a [`Pattern`] mutably.
pub fn walk_pattern_mut<V: VisitorMut + ?Sized>(visitor: &mut V, pattern: &mut Pattern) {
    match &mut pattern.kind {
        PatternKind::Wildcard | PatternKind::Rest | PatternKind::Error => {}
        PatternKind::Literal(lit) => visitor.visit_literal(lit),
        PatternKind::Ident { subpattern, .. } => {
            if let Some(sub) = subpattern {
                visitor.visit_pattern(sub);
            }
        }
        PatternKind::Path(path) => visitor.visit_type_path(path),
        PatternKind::Tuple(items) | PatternKind::Or(items) => {
            for item in items {
                visitor.visit_pattern(item);
            }
        }
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for item in prefix {
                visitor.visit_pattern(item);
            }
            if let Some(rest) = rest {
                visitor.visit_pattern(rest);
            }
            for item in suffix {
                visitor.visit_pattern(item);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for field in fields {
                if let Some(pattern) = &mut field.pattern {
                    visitor.visit_pattern(pattern);
                }
            }
        }
        PatternKind::TupleStruct { elems, .. } => {
            for elem in elems {
                visitor.visit_pattern(elem);
            }
        }
        PatternKind::Range { lo, hi, .. } => {
            if let Some(lo) = lo {
                visitor.visit_literal(lo);
            }
            if let Some(hi) = hi {
                visitor.visit_literal(hi);
            }
        }
        PatternKind::Ref { inner, .. } => visitor.visit_pattern(inner),
    }
}

/// Walks into every child of a [`Block`] mutably.
pub fn walk_block_mut<V: VisitorMut + ?Sized>(visitor: &mut V, block: &mut Block) {
    for stmt in &mut block.stmts {
        visitor.visit_stmt(stmt);
    }
    if let Some(tail) = &mut block.tail {
        visitor.visit_expr(tail);
    }
}

/// Walks into every child of a [`MatchArm`] mutably.
pub fn walk_match_arm_mut<V: VisitorMut + ?Sized>(visitor: &mut V, arm: &mut MatchArm) {
    visitor.visit_pattern(&mut arm.pattern);
    if let Some(guard) = &mut arm.guard {
        visitor.visit_expr(guard);
    }
    visitor.visit_expr(&mut arm.body);
}

/// Walks into every child of a [`SelectArm`] mutably.
pub fn walk_select_arm_mut<V: VisitorMut + ?Sized>(visitor: &mut V, arm: &mut SelectArm) {
    match &mut arm.op {
        SelectOp::Recv { pattern, channel } => {
            visitor.visit_pattern(pattern);
            visitor.visit_expr(channel);
        }
        SelectOp::Send { channel, value } => {
            visitor.visit_expr(channel);
            visitor.visit_expr(value);
        }
        SelectOp::Default => {}
    }
    visitor.visit_expr(&mut arm.body);
}

/// Walks into every child of a [`TypePath`] mutably.
pub fn walk_type_path_mut<V: VisitorMut + ?Sized>(visitor: &mut V, path: &mut TypePath) {
    for segment in &mut path.segments {
        for arg in &mut segment.generics {
            visitor.visit_generic_arg(arg);
        }
    }
}

/// Walks into every child of a [`PathExpr`] mutably.
pub fn walk_path_expr_mut<V: VisitorMut + ?Sized>(visitor: &mut V, path: &mut PathExpr) {
    for segment in &mut path.segments {
        for arg in &mut segment.generics {
            visitor.visit_generic_arg(arg);
        }
    }
}

/// Walks into every child of a [`GenericArg`] mutably.
pub fn walk_generic_arg_mut<V: VisitorMut + ?Sized>(visitor: &mut V, arg: &mut GenericArg) {
    match arg {
        GenericArg::Type(ty) => visitor.visit_type(ty),
        GenericArg::Const(expr) => visitor.visit_expr(expr),
    }
}
