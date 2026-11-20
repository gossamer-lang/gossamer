//! Immutable tree-walkers. Each `walk_*` function matches the corresponding
//! [`Visitor`] callback and is deliberately a thin, branching
//! recursion so implementers can override any callback without losing the
//! default descent behaviour.

#![forbid(unsafe_code)]

use super::Visitor;
use crate::expr::{
    ArrayExpr, Block, ClosureParam, Expr, ExprKind, FieldSelector, Label, MatchArm, PathExpr,
    PathSegment, SelectArm, SelectOp, StructExprField,
};
use crate::items::{
    Attrs, ConstDecl, EnumDecl, EnumVariant, FnDecl, FnParam, GenericParam, Generics, ImplDecl,
    ImplItem, Item, ItemKind, ModBody, ModDecl, StaticDecl, StructBody, StructDecl, StructField,
    TraitBound, TraitDecl, TraitItem, TupleField, TypeAliasDecl, WhereClause,
};
use crate::pattern::{FieldPattern, Pattern, PatternKind};
use crate::source_file::{SourceFile, UseDecl};
use crate::stmt::{Stmt, StmtKind};
use crate::ty::{GenericArg, Type, TypeKind, TypePath, TypePathSegment};

/// Walks into every child of a [`SourceFile`].
pub fn walk_source_file<V: Visitor + ?Sized>(visitor: &mut V, source_file: &SourceFile) {
    for use_decl in &source_file.uses {
        visitor.visit_use_decl(use_decl);
    }
    for item in &source_file.items {
        visitor.visit_item(item);
    }
    for stmt in &source_file.top_level_stmts {
        visitor.visit_stmt(stmt);
    }
}

/// Walks into every child of a [`UseDecl`].
///
/// A `use` declaration carries only identifiers and string data, so this
/// walker is deliberately a no-op and exists solely to satisfy the default
/// [`Visitor::visit_use_decl`] implementation.
pub fn walk_use_decl<V: Visitor + ?Sized>(_visitor: &mut V, _use_decl: &UseDecl) {}

/// Walks into every child of an [`Item`].
pub fn walk_item<V: Visitor + ?Sized>(visitor: &mut V, item: &Item) {
    walk_attrs(visitor, &item.attrs);
    match &item.kind {
        ItemKind::Fn(decl) => walk_fn_decl(visitor, decl),
        ItemKind::Struct(decl) => walk_struct_decl(visitor, decl),
        ItemKind::Enum(decl) => walk_enum_decl(visitor, decl),
        ItemKind::Trait(decl) => walk_trait_decl(visitor, decl),
        ItemKind::Impl(decl) => walk_impl_decl(visitor, decl),
        ItemKind::TypeAlias(decl) => walk_type_alias_decl(visitor, decl),
        ItemKind::Const(decl) => walk_const_decl(visitor, decl),
        ItemKind::Static(decl) => walk_static_decl(visitor, decl),
        ItemKind::Mod(decl) => walk_mod_decl(visitor, decl),
        ItemKind::AttrItem(_) => {}
    }
}

fn walk_attrs<V: Visitor + ?Sized>(_visitor: &mut V, _attrs: &Attrs) {}

fn walk_fn_decl<V: Visitor + ?Sized>(visitor: &mut V, decl: &FnDecl) {
    walk_generics(visitor, &decl.generics);
    for param in &decl.params {
        walk_fn_param(visitor, param);
    }
    if let Some(ret) = &decl.ret {
        visitor.visit_type(ret);
    }
    walk_where_clause(visitor, &decl.where_clause);
    if let Some(body) = &decl.body {
        visitor.visit_expr(body);
    }
}

fn walk_fn_param<V: Visitor + ?Sized>(visitor: &mut V, param: &FnParam) {
    match param {
        FnParam::Receiver(_) => {}
        FnParam::Typed { pattern, ty, .. } => {
            visitor.visit_pattern(pattern);
            visitor.visit_type(ty);
        }
    }
}

fn walk_generics<V: Visitor + ?Sized>(visitor: &mut V, generics: &Generics) {
    for param in &generics.params {
        walk_generic_param(visitor, param);
    }
}

fn walk_generic_param<V: Visitor + ?Sized>(visitor: &mut V, param: &GenericParam) {
    match param {
        GenericParam::Lifetime { .. } => {}
        GenericParam::Type {
            bounds, default, ..
        } => {
            for bound in bounds {
                walk_trait_bound(visitor, bound);
            }
            if let Some(ty) = default {
                visitor.visit_type(ty);
            }
        }
        GenericParam::Const { ty, default, .. } => {
            visitor.visit_type(ty);
            if let Some(expr) = default {
                visitor.visit_expr(expr);
            }
        }
    }
}

fn walk_where_clause<V: Visitor + ?Sized>(visitor: &mut V, where_clause: &WhereClause) {
    for predicate in &where_clause.predicates {
        visitor.visit_type(&predicate.bounded);
        for bound in &predicate.bounds {
            walk_trait_bound(visitor, bound);
        }
    }
}

fn walk_trait_bound<V: Visitor + ?Sized>(visitor: &mut V, bound: &TraitBound) {
    visitor.visit_type_path(&bound.path);
    for binding in &bound.bindings {
        visitor.visit_type(&binding.ty);
    }
}

fn walk_struct_decl<V: Visitor + ?Sized>(visitor: &mut V, decl: &StructDecl) {
    walk_generics(visitor, &decl.generics);
    walk_where_clause(visitor, &decl.where_clause);
    walk_struct_body(visitor, &decl.body);
    walk_type_body(visitor, &decl.traits, &decl.items);
}

/// The traits a type lists after its name and the items in its body, before
/// those items move into the `impl` blocks they stand for.
fn walk_type_body<V: Visitor + ?Sized>(visitor: &mut V, traits: &[TraitBound], items: &[ImplItem]) {
    for bound in traits {
        walk_trait_bound(visitor, bound);
    }
    for item in items {
        walk_impl_item(visitor, item);
    }
}

fn walk_struct_body<V: Visitor + ?Sized>(visitor: &mut V, body: &StructBody) {
    match body {
        StructBody::Named(fields) => {
            for field in fields {
                walk_struct_field(visitor, field);
            }
        }
        StructBody::Tuple(fields) => {
            for field in fields {
                walk_tuple_field(visitor, field);
            }
        }
        StructBody::Unit => {}
    }
}

fn walk_struct_field<V: Visitor + ?Sized>(visitor: &mut V, field: &StructField) {
    visitor.visit_type(&field.ty);
}

fn walk_tuple_field<V: Visitor + ?Sized>(visitor: &mut V, field: &TupleField) {
    visitor.visit_type(&field.ty);
}

fn walk_enum_decl<V: Visitor + ?Sized>(visitor: &mut V, decl: &EnumDecl) {
    walk_generics(visitor, &decl.generics);
    walk_where_clause(visitor, &decl.where_clause);
    for variant in &decl.variants {
        walk_enum_variant(visitor, variant);
    }
    walk_type_body(visitor, &decl.traits, &decl.items);
}

fn walk_enum_variant<V: Visitor + ?Sized>(visitor: &mut V, variant: &EnumVariant) {
    walk_struct_body(visitor, &variant.body);
    if let Some(disc) = &variant.discriminant {
        visitor.visit_expr(disc);
    }
}

fn walk_trait_decl<V: Visitor + ?Sized>(visitor: &mut V, decl: &TraitDecl) {
    walk_generics(visitor, &decl.generics);
    for bound in &decl.supertraits {
        walk_trait_bound(visitor, bound);
    }
    walk_where_clause(visitor, &decl.where_clause);
    for item in &decl.items {
        walk_trait_item(visitor, item);
    }
}

fn walk_trait_item<V: Visitor + ?Sized>(visitor: &mut V, item: &TraitItem) {
    match item {
        TraitItem::Fn(decl) => walk_fn_decl(visitor, decl),
        TraitItem::Type {
            bounds, default, ..
        } => {
            for bound in bounds {
                walk_trait_bound(visitor, bound);
            }
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

fn walk_impl_decl<V: Visitor + ?Sized>(visitor: &mut V, decl: &ImplDecl) {
    walk_generics(visitor, &decl.generics);
    if let Some(trait_ref) = &decl.trait_ref {
        walk_trait_bound(visitor, trait_ref);
    }
    visitor.visit_type(&decl.self_ty);
    walk_where_clause(visitor, &decl.where_clause);
    for item in &decl.items {
        walk_impl_item(visitor, item);
    }
}

fn walk_impl_item<V: Visitor + ?Sized>(visitor: &mut V, item: &ImplItem) {
    match item {
        ImplItem::Fn(decl) => walk_fn_decl(visitor, decl),
        ImplItem::Type { ty, .. } => visitor.visit_type(ty),
        ImplItem::Const { ty, value, .. } => {
            visitor.visit_type(ty);
            visitor.visit_expr(value);
        }
    }
}

fn walk_type_alias_decl<V: Visitor + ?Sized>(visitor: &mut V, decl: &TypeAliasDecl) {
    walk_generics(visitor, &decl.generics);
    visitor.visit_type(&decl.ty);
}

fn walk_const_decl<V: Visitor + ?Sized>(visitor: &mut V, decl: &ConstDecl) {
    visitor.visit_type(&decl.ty);
    visitor.visit_expr(&decl.value);
}

fn walk_static_decl<V: Visitor + ?Sized>(visitor: &mut V, decl: &StaticDecl) {
    visitor.visit_type(&decl.ty);
    visitor.visit_expr(&decl.value);
}

fn walk_mod_decl<V: Visitor + ?Sized>(visitor: &mut V, decl: &ModDecl) {
    if let ModBody::Inline(items) = &decl.body {
        for item in items {
            visitor.visit_item(item);
        }
    }
}

/// Walks into every child of a [`Stmt`].
pub fn walk_stmt<V: Visitor + ?Sized>(visitor: &mut V, stmt: &Stmt) {
    match &stmt.kind {
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

/// Walks into every child of an [`Expr`].
pub fn walk_expr<V: Visitor + ?Sized>(visitor: &mut V, expr: &Expr) {
    match &expr.kind {
        ExprKind::Literal(lit) => visitor.visit_literal(lit),
        ExprKind::Path(path) => visitor.visit_path_expr(path),
        ExprKind::Block(block) | ExprKind::Unsafe(block) => visitor.visit_block(block),
        ExprKind::Try(inner) => visitor.visit_expr(inner),
        ExprKind::Unary { operand, .. } => visitor.visit_expr(operand),
        ExprKind::Array(array_expr) | ExprKind::FixedArray(array_expr) => {
            walk_array_expr(visitor, array_expr);
        }
        ExprKind::Tuple(items) | ExprKind::MapLiteral(items) | ExprKind::SetLiteral(items) => {
            walk_exprs(visitor, items);
        }
        ExprKind::Select(arms) => {
            for arm in arms {
                visitor.visit_select_arm(arm);
            }
        }
        ExprKind::Continue { label } => walk_optional_label(visitor, label.as_ref()),
        _ => walk_expr_compound(visitor, expr),
    }
}

fn walk_expr_compound<V: Visitor + ?Sized>(visitor: &mut V, expr: &Expr) {
    match &expr.kind {
        ExprKind::Call { callee, args } => {
            visitor.visit_expr(callee);
            walk_exprs(visitor, args);
        }
        ExprKind::MethodCall {
            receiver,
            generics,
            args,
            ..
        } => {
            visitor.visit_expr(receiver);
            for arg in generics {
                visitor.visit_generic_arg(arg);
            }
            walk_exprs(visitor, args);
        }
        ExprKind::FieldAccess { receiver, field } => {
            visitor.visit_expr(receiver);
            walk_field_selector(visitor, field);
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
        _ => walk_expr_control(visitor, expr),
    }
}

fn walk_expr_control<V: Visitor + ?Sized>(visitor: &mut V, expr: &Expr) {
    match &expr.kind {
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
        ExprKind::Loop { label, body } => {
            walk_optional_label(visitor, label.as_ref());
            visitor.visit_expr(body);
        }
        ExprKind::While {
            label,
            condition,
            body,
        } => {
            walk_optional_label(visitor, label.as_ref());
            visitor.visit_expr(condition);
            visitor.visit_expr(body);
        }
        ExprKind::For {
            label,
            pattern,
            iter,
            body,
        } => {
            walk_optional_label(visitor, label.as_ref());
            visitor.visit_pattern(pattern);
            visitor.visit_expr(iter);
            visitor.visit_expr(body);
        }
        _ => walk_expr_other(visitor, expr),
    }
}

fn walk_expr_other<V: Visitor + ?Sized>(visitor: &mut V, expr: &Expr) {
    match &expr.kind {
        ExprKind::Closure {
            params, ret, body, ..
        } => {
            for param in params {
                walk_closure_param(visitor, param);
            }
            if let Some(ty) = ret {
                visitor.visit_type(ty);
            }
            visitor.visit_expr(body);
        }
        ExprKind::Return(Some(inner)) => visitor.visit_expr(inner),
        ExprKind::Break { label, value } => {
            walk_optional_label(visitor, label.as_ref());
            if let Some(expr) = value {
                visitor.visit_expr(expr);
            }
        }
        ExprKind::Struct {
            path, fields, base, ..
        } => {
            visitor.visit_path_expr(path);
            for field in fields {
                walk_struct_expr_field(visitor, field);
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

fn walk_exprs<V: Visitor + ?Sized>(visitor: &mut V, exprs: &[Expr]) {
    for expr in exprs {
        visitor.visit_expr(expr);
    }
}

fn walk_optional_label<V: Visitor + ?Sized>(visitor: &mut V, label: Option<&Label>) {
    if let Some(label) = label {
        visitor.visit_label(label);
    }
}

fn walk_field_selector<V: Visitor + ?Sized>(_visitor: &mut V, _selector: &FieldSelector) {}

fn walk_closure_param<V: Visitor + ?Sized>(visitor: &mut V, param: &ClosureParam) {
    visitor.visit_pattern(&param.pattern);
    if let Some(ty) = &param.ty {
        visitor.visit_type(ty);
    }
}

fn walk_struct_expr_field<V: Visitor + ?Sized>(visitor: &mut V, field: &StructExprField) {
    if let Some(value) = &field.value {
        visitor.visit_expr(value);
    }
}

fn walk_array_expr<V: Visitor + ?Sized>(visitor: &mut V, array: &ArrayExpr) {
    match array {
        ArrayExpr::List(items) => {
            for item in items {
                visitor.visit_expr(item);
            }
        }
        ArrayExpr::Repeat { value, count } => {
            visitor.visit_expr(value);
            visitor.visit_expr(count);
        }
    }
}

/// Walks into every child of a [`Type`].
pub fn walk_type<V: Visitor + ?Sized>(visitor: &mut V, ty: &Type) {
    match &ty.kind {
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

/// Walks into every child of a [`Pattern`].
pub fn walk_pattern<V: Visitor + ?Sized>(visitor: &mut V, pattern: &Pattern) {
    match &pattern.kind {
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
        PatternKind::Struct { path, fields, .. } => {
            visitor.visit_type_path(path);
            for field in fields {
                walk_field_pattern(visitor, field);
            }
        }
        PatternKind::TupleStruct { path, elems } => {
            visitor.visit_type_path(path);
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

fn walk_field_pattern<V: Visitor + ?Sized>(visitor: &mut V, field: &FieldPattern) {
    if let Some(pattern) = &field.pattern {
        visitor.visit_pattern(pattern);
    }
}

/// Walks into every child of a [`Block`].
pub fn walk_block<V: Visitor + ?Sized>(visitor: &mut V, block: &Block) {
    for stmt in &block.stmts {
        visitor.visit_stmt(stmt);
    }
    if let Some(tail) = &block.tail {
        visitor.visit_expr(tail);
    }
}

/// Walks into every child of a [`MatchArm`].
pub fn walk_match_arm<V: Visitor + ?Sized>(visitor: &mut V, arm: &MatchArm) {
    visitor.visit_pattern(&arm.pattern);
    if let Some(guard) = &arm.guard {
        visitor.visit_expr(guard);
    }
    visitor.visit_expr(&arm.body);
}

/// Walks into every child of a [`SelectArm`].
pub fn walk_select_arm<V: Visitor + ?Sized>(visitor: &mut V, arm: &SelectArm) {
    match &arm.op {
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
    visitor.visit_expr(&arm.body);
}

/// Walks into every child of a [`TypePath`].
pub fn walk_type_path<V: Visitor + ?Sized>(visitor: &mut V, path: &TypePath) {
    for segment in &path.segments {
        walk_type_path_segment(visitor, segment);
    }
}

fn walk_type_path_segment<V: Visitor + ?Sized>(visitor: &mut V, segment: &TypePathSegment) {
    for arg in &segment.generics {
        visitor.visit_generic_arg(arg);
    }
}

/// Walks into every child of a [`PathExpr`].
pub fn walk_path_expr<V: Visitor + ?Sized>(visitor: &mut V, path: &PathExpr) {
    for segment in &path.segments {
        walk_path_expr_segment(visitor, segment);
    }
}

fn walk_path_expr_segment<V: Visitor + ?Sized>(visitor: &mut V, segment: &PathSegment) {
    for arg in &segment.generics {
        visitor.visit_generic_arg(arg);
    }
}

/// Walks into every child of a [`GenericArg`].
pub fn walk_generic_arg<V: Visitor + ?Sized>(visitor: &mut V, arg: &GenericArg) {
    match arg {
        GenericArg::Type(ty) => visitor.visit_type(ty),
        GenericArg::Const(expr) => visitor.visit_expr(expr),
    }
}
