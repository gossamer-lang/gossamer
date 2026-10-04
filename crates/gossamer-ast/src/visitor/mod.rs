//! Read-only and mutable tree-walking visitors over every AST node kind.
//! The default `walk_*` free functions recurse into every child node. A
//! [`Visitor`] or [`VisitorMut`] implementer overrides only the callbacks it
//! cares about and calls the corresponding `walk_*` helper to continue the
//! traversal.

#![forbid(unsafe_code)]

mod walk;
mod walk_mut;

pub use walk::{
    walk_block, walk_expr, walk_generic_arg, walk_item, walk_match_arm, walk_path_expr,
    walk_pattern, walk_select_arm, walk_source_file, walk_stmt, walk_type, walk_type_path,
    walk_use_decl,
};
pub use walk_mut::{
    walk_block_mut, walk_expr_mut, walk_generic_arg_mut, walk_item_mut, walk_match_arm_mut,
    walk_path_expr_mut, walk_pattern_mut, walk_select_arm_mut, walk_source_file_mut, walk_stmt_mut,
    walk_type_mut, walk_type_path_mut, walk_use_decl_mut,
};

use crate::expr::{Block, Expr, Label, Literal, MatchArm, PathExpr, SelectArm};
use crate::items::Item;
use crate::pattern::Pattern;
use crate::source_file::{SourceFile, UseDecl};
use crate::stmt::Stmt;
use crate::ty::{GenericArg, Type, TypePath};

/// Immutable AST visitor.
///
/// Every callback has a default implementation that forwards to the matching
/// `walk_*` free function. Override a callback to inspect nodes; call the
/// matching walker from inside the override to continue descending.
pub trait Visitor {
    /// Visits a source file.
    fn visit_source_file(&mut self, source_file: &SourceFile) {
        walk_source_file(self, source_file);
    }
    /// Visits a `use` declaration.
    fn visit_use_decl(&mut self, use_decl: &UseDecl) {
        walk_use_decl(self, use_decl);
    }
    /// Visits an item.
    fn visit_item(&mut self, item: &Item) {
        walk_item(self, item);
    }
    /// Visits a statement.
    fn visit_stmt(&mut self, stmt: &Stmt) {
        walk_stmt(self, stmt);
    }
    /// Visits an expression.
    fn visit_expr(&mut self, expr: &Expr) {
        walk_expr(self, expr);
    }
    /// Visits a type.
    fn visit_type(&mut self, ty: &Type) {
        walk_type(self, ty);
    }
    /// Visits a pattern.
    fn visit_pattern(&mut self, pattern: &Pattern) {
        walk_pattern(self, pattern);
    }
    /// Visits a block expression body.
    fn visit_block(&mut self, block: &Block) {
        walk_block(self, block);
    }
    /// Visits a match arm.
    fn visit_match_arm(&mut self, arm: &MatchArm) {
        walk_match_arm(self, arm);
    }
    /// Visits a select arm.
    fn visit_select_arm(&mut self, arm: &SelectArm) {
        walk_select_arm(self, arm);
    }
    /// Visits a label (no-op default).
    fn visit_label(&mut self, _label: &Label) {}
    /// Visits a literal (no-op default).
    fn visit_literal(&mut self, _literal: &Literal) {}
    /// Visits a path used in a type context.
    fn visit_type_path(&mut self, path: &TypePath) {
        walk_type_path(self, path);
    }
    /// Visits a path used in an expression context.
    fn visit_path_expr(&mut self, path: &PathExpr) {
        walk_path_expr(self, path);
    }
    /// Visits a generic argument.
    fn visit_generic_arg(&mut self, arg: &GenericArg) {
        walk_generic_arg(self, arg);
    }
}

/// Mutable AST visitor - identical surface to [`Visitor`] with `&mut` nodes.
pub trait VisitorMut {
    /// Visits a source file.
    fn visit_source_file(&mut self, source_file: &mut SourceFile) {
        walk_source_file_mut(self, source_file);
    }
    /// Visits a `use` declaration.
    fn visit_use_decl(&mut self, use_decl: &mut UseDecl) {
        walk_use_decl_mut(self, use_decl);
    }
    /// Visits an item.
    fn visit_item(&mut self, item: &mut Item) {
        walk_item_mut(self, item);
    }
    /// Visits a statement.
    fn visit_stmt(&mut self, stmt: &mut Stmt) {
        walk_stmt_mut(self, stmt);
    }
    /// Visits an expression.
    fn visit_expr(&mut self, expr: &mut Expr) {
        walk_expr_mut(self, expr);
    }
    /// Visits a type.
    fn visit_type(&mut self, ty: &mut Type) {
        walk_type_mut(self, ty);
    }
    /// Visits a pattern.
    fn visit_pattern(&mut self, pattern: &mut Pattern) {
        walk_pattern_mut(self, pattern);
    }
    /// Visits a block expression body.
    fn visit_block(&mut self, block: &mut Block) {
        walk_block_mut(self, block);
    }
    /// Visits a match arm.
    fn visit_match_arm(&mut self, arm: &mut MatchArm) {
        walk_match_arm_mut(self, arm);
    }
    /// Visits a select arm.
    fn visit_select_arm(&mut self, arm: &mut SelectArm) {
        walk_select_arm_mut(self, arm);
    }
    /// Visits a label (no-op default).
    fn visit_label(&mut self, _label: &mut Label) {}
    /// Visits a literal (no-op default).
    fn visit_literal(&mut self, _literal: &mut Literal) {}
    /// Visits a path used in a type context.
    fn visit_type_path(&mut self, path: &mut TypePath) {
        walk_type_path_mut(self, path);
    }
    /// Visits a path used in an expression context.
    fn visit_path_expr(&mut self, path: &mut PathExpr) {
        walk_path_expr_mut(self, path);
    }
    /// Visits a generic argument.
    fn visit_generic_arg(&mut self, arg: &mut GenericArg) {
        walk_generic_arg_mut(self, arg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{BinaryOp, Ident, Mutability, Visibility};
    use crate::expr::{Block, Expr, ExprKind, Literal, PathExpr};
    use crate::items::{Attrs, FnDecl, FnParam, Generics, Item, ItemKind, WhereClause};
    use crate::node_id::NodeId;
    use crate::pattern::{Pattern, PatternKind};
    use crate::source_file::SourceFile;
    use crate::stmt::{Stmt, StmtKind};
    use crate::ty::{Type, TypeKind, TypePath};
    use gossamer_lex::{FileId, SourceMap, Span};

    struct ExprCounter {
        count: u32,
    }

    impl Visitor for ExprCounter {
        fn visit_expr(&mut self, expr: &Expr) {
            self.count += 1;
            walk_expr(self, expr);
        }
    }

    struct LetVisits {
        patterns: u32,
        types: u32,
        exprs: u32,
    }

    impl Visitor for LetVisits {
        fn visit_pattern(&mut self, pattern: &Pattern) {
            self.patterns += 1;
            walk_pattern(self, pattern);
        }
        fn visit_type(&mut self, ty: &Type) {
            self.types += 1;
            walk_type(self, ty);
        }
        fn visit_expr(&mut self, expr: &Expr) {
            self.exprs += 1;
            walk_expr(self, expr);
        }
    }

    struct LiteralRewriter;

    impl VisitorMut for LiteralRewriter {
        fn visit_literal(&mut self, literal: &mut Literal) {
            if let Literal::Int(raw) = literal {
                *raw = "99".into();
            }
        }
    }

    struct SegmentCounter {
        segments: u32,
    }

    impl Visitor for SegmentCounter {
        fn visit_path_expr(&mut self, path: &PathExpr) {
            self.segments += u32::try_from(path.segments.len()).unwrap_or(u32::MAX);
            walk_path_expr(self, path);
        }
    }

    fn fake_file() -> FileId {
        let mut map = SourceMap::new();
        map.add_file("visitor_test", "")
    }

    fn fake_span() -> Span {
        Span::new(fake_file(), 0, 0)
    }

    fn int(value: &str) -> Expr {
        Expr::new(
            NodeId::DUMMY,
            fake_span(),
            ExprKind::Literal(Literal::Int(value.into())),
        )
    }

    #[test]
    fn default_walker_visits_every_expression_in_a_nested_tree() {
        let lhs = int("1");
        let rhs = int("2");
        let sum = Expr::new(
            NodeId::DUMMY,
            fake_span(),
            ExprKind::Binary {
                op: BinaryOp::Add,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        );
        let mut counter = ExprCounter { count: 0 };
        counter.visit_expr(&sum);
        assert_eq!(counter.count, 3);
    }

    #[test]
    fn visitor_walks_let_statement_pattern_and_initializer() {
        let pattern = Pattern::new(
            NodeId::DUMMY,
            fake_span(),
            PatternKind::Ident {
                mutability: Mutability::Immutable,
                name: Ident::new("answer"),
                subpattern: None,
            },
        );
        let init = int("42");
        let ty = Type::new(
            NodeId::DUMMY,
            fake_span(),
            TypeKind::Path(TypePath::single("i32")),
        );
        let let_stmt = Stmt::new(
            NodeId::DUMMY,
            fake_span(),
            StmtKind::Let {
                pattern,
                ty: Some(ty),
                init: Some(Box::new(init)),
            },
        );
        let mut visits = LetVisits {
            patterns: 0,
            types: 0,
            exprs: 0,
        };
        visits.visit_stmt(&let_stmt);
        assert_eq!(visits.patterns, 1);
        assert_eq!(visits.types, 1);
        assert_eq!(visits.exprs, 1);
    }

    #[test]
    fn visitor_walks_fn_item_body() {
        let body = Expr::new(
            NodeId::DUMMY,
            fake_span(),
            ExprKind::Block(Block {
                stmts: vec![],
                tail: Some(Box::new(int("0"))),
                synthetic: false,
                kind: crate::BlockKind::Plain,
            }),
        );
        let fn_decl = FnDecl {
            attrs: Attrs::default(),
            span: fake_span(),
            is_unsafe: false,
            is_comptime: false,
            visibility: crate::Visibility::Inherited,
            name: Ident::new("main"),
            generics: Generics::default(),
            params: Vec::<FnParam>::new(),
            ret: None,
            where_clause: WhereClause::default(),
            body: Some(Box::new(body)),
            extern_abi: None,
        };
        let item = Item::new(
            NodeId::DUMMY,
            fake_span(),
            Attrs::default(),
            Visibility::Inherited,
            ItemKind::Fn(fn_decl),
        );
        let source = SourceFile::new(fake_file(), vec![], vec![item]);
        let mut counter = ExprCounter { count: 0 };
        counter.visit_source_file(&source);
        assert_eq!(counter.count, 2);
    }

    #[test]
    fn visitor_walks_top_level_statements() {
        let stmt = Stmt::new(
            NodeId::DUMMY,
            fake_span(),
            StmtKind::Expr {
                expr: Box::new(int("7")),
                has_semi: false,
            },
        );
        let mut source = SourceFile::new(fake_file(), vec![], vec![]);
        source.top_level_stmts.push(stmt);
        let mut counter = ExprCounter { count: 0 };
        counter.visit_source_file(&source);
        assert_eq!(counter.count, 1);

        LiteralRewriter.visit_source_file(&mut source);
        let StmtKind::Expr { expr, .. } = &source.top_level_stmts[0].kind else {
            panic!("expected expression statement");
        };
        assert!(matches!(&expr.kind, ExprKind::Literal(Literal::Int(raw)) if raw == "99"));
    }

    #[test]
    fn visitor_mut_rewrites_int_literal_text() {
        let mut expr = int("1");
        LiteralRewriter.visit_expr(&mut expr);
        if let ExprKind::Literal(Literal::Int(raw)) = &expr.kind {
            assert_eq!(raw, "99");
        } else {
            panic!("expected int literal expression");
        }
    }

    #[test]
    fn path_expr_walker_visits_each_segment() {
        let path = PathExpr::from_names(["a", "b", "c"]);
        let mut counter = SegmentCounter { segments: 0 };
        counter.visit_path_expr(&path);
        assert_eq!(counter.segments, 3);
    }
}
