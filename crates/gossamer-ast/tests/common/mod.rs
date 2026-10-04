//! Shared helpers for AST integration tests.

#![forbid(unsafe_code)]
#![allow(dead_code)]

use gossamer_ast::{
    Attrs, Block, Expr, ExprKind, FieldSelector, FnDecl, FnParam, Generics, Ident, Item, ItemKind,
    Literal, NodeId, PathExpr, Pattern, PatternKind, SourceFile, Stmt, StmtKind, Type, TypeKind,
    TypePath, UseDecl, UseTarget, Visibility, WhereClause,
};
use gossamer_ast::{BinaryOp, ModulePath, Mutability, UseListEntry};
use gossamer_lex::{FileId, SourceMap, Span};

pub(crate) fn make_file() -> FileId {
    let mut map = SourceMap::new();
    map.add_file("integration-test", "")
}

pub(crate) fn dummy_span() -> Span {
    Span::new(make_file(), 0, 0)
}

pub(crate) fn ident(name: &str) -> Ident {
    Ident::new(name)
}

pub(crate) fn literal_int(value: &str) -> Expr {
    Expr::new(
        NodeId::DUMMY,
        dummy_span(),
        ExprKind::Literal(Literal::Int(value.into())),
    )
}

pub(crate) fn literal_string(value: &str) -> Expr {
    Expr::new(
        NodeId::DUMMY,
        dummy_span(),
        ExprKind::Literal(Literal::String(value.into())),
    )
}

pub(crate) fn path_expr(segments: &[&str]) -> Expr {
    Expr::new(
        NodeId::DUMMY,
        dummy_span(),
        ExprKind::Path(PathExpr::from_names(segments.iter().copied())),
    )
}

pub(crate) fn path_value(segments: &[&str]) -> PathExpr {
    PathExpr::from_names(segments.iter().copied())
}

pub(crate) fn type_path(segments: &[&str]) -> Type {
    let segments = segments
        .iter()
        .copied()
        .map(gossamer_ast::TypePathSegment::new)
        .collect();
    Type::new(
        NodeId::DUMMY,
        dummy_span(),
        TypeKind::Path(TypePath { segments }),
    )
}

pub(crate) fn call_expr(callee: Expr, args: Vec<Expr>) -> Expr {
    Expr::new(
        NodeId::DUMMY,
        dummy_span(),
        ExprKind::Call {
            callee: Box::new(callee),
            args,
        },
    )
}

pub(crate) fn method_call_expr(receiver: Expr, name: &str, args: Vec<Expr>) -> Expr {
    Expr::new(
        NodeId::DUMMY,
        dummy_span(),
        ExprKind::MethodCall {
            receiver: Box::new(receiver),
            name: Ident::new(name),
            name_span: dummy_span(),
            desugared_from: None,
            generics: Vec::new(),
            args,
        },
    )
}

pub(crate) fn field_access(receiver: Expr, field: &str) -> Expr {
    Expr::new(
        NodeId::DUMMY,
        dummy_span(),
        ExprKind::FieldAccess {
            receiver: Box::new(receiver),
            field: FieldSelector::Named(Ident::new(field)),
        },
    )
}

pub(crate) fn binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::new(
        NodeId::DUMMY,
        dummy_span(),
        ExprKind::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        },
    )
}

pub(crate) fn block_expr(stmts: Vec<Stmt>, tail: Option<Expr>) -> Expr {
    Expr::new(
        NodeId::DUMMY,
        dummy_span(),
        ExprKind::Block(Block {
            stmts,
            tail: tail.map(Box::new),
            synthetic: false,
            kind: gossamer_ast::BlockKind::Plain,
        }),
    )
}

pub(crate) fn expr_stmt(expr: Expr, has_semi: bool) -> Stmt {
    Stmt::new(
        NodeId::DUMMY,
        dummy_span(),
        StmtKind::Expr {
            expr: Box::new(expr),
            has_semi,
        },
    )
}

pub(crate) fn let_stmt(name: &str, value: Expr) -> Stmt {
    let pattern = Pattern::new(
        NodeId::DUMMY,
        dummy_span(),
        PatternKind::Ident {
            mutability: Mutability::Immutable,
            name: Ident::new(name),
            subpattern: None,
        },
    );
    Stmt::new(
        NodeId::DUMMY,
        dummy_span(),
        StmtKind::Let {
            pattern,
            ty: None,
            init: Some(Box::new(value)),
        },
    )
}

pub(crate) fn fn_item(name: &str, body: Expr) -> Item {
    fn_item_with_ret(name, body, None)
}

pub(crate) fn fn_item_with_ret(name: &str, body: Expr, ret: Option<Type>) -> Item {
    let decl = FnDecl {
        attrs: Attrs::default(),
        span: dummy_span(),
        is_unsafe: false,
        is_comptime: false,
        visibility: gossamer_ast::Visibility::Inherited,
        name: Ident::new(name),
        generics: Generics::default(),
        params: Vec::<FnParam>::new(),
        ret,
        where_clause: WhereClause::default(),
        body: Some(Box::new(body)),
        extern_abi: None,
    };
    Item::new(
        NodeId::DUMMY,
        dummy_span(),
        Attrs::default(),
        Visibility::Inherited,
        ItemKind::Fn(decl),
    )
}

pub(crate) fn use_decl_module(segments: &[&str]) -> UseDecl {
    UseDecl::simple(
        NodeId::DUMMY,
        dummy_span(),
        UseTarget::Module(ModulePath::from_names(segments.iter().copied())),
    )
}

pub(crate) fn use_decl_module_with_list(segments: &[&str], list: Vec<UseListEntry>) -> UseDecl {
    UseDecl {
        id: NodeId::DUMMY,
        span: dummy_span(),
        target: UseTarget::Module(ModulePath::from_names(segments.iter().copied())),
        alias: None,
        list: Some(list),
        module: Vec::new(),
    }
}

pub(crate) fn source_file(uses: Vec<UseDecl>, items: Vec<Item>) -> SourceFile {
    SourceFile::new(make_file(), uses, items)
}
