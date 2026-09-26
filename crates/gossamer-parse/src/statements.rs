//! Statement parsing inside block expressions.

#![forbid(unsafe_code)]

use gossamer_ast::{
    Expr, ExprKind, Ident, MatchArm, Mutability, PathExpr, PathSegment, Pattern, PatternKind, Stmt,
    StmtKind,
};
use gossamer_lex::{Keyword, Punct, Span};

use crate::diagnostic::ParseError;
use crate::parser::Parser;
use crate::recovery::{is_item_start, is_stmt_start, is_stmt_start_keyword};

impl Parser<'_> {
    /// Parses a single statement.
    pub(crate) fn parse_stmt(&mut self) -> Stmt {
        let start_span = self.peek_span();
        let diagnostics_before = self.diagnostic_count();
        let kind = self.parse_stmt_kind();
        let consumed_semicolon = self.slice(self.last_span()) == ";";
        // A statement that already failed to parse is stopped somewhere the
        // grammar never intended, so its missing separator says nothing the
        // first diagnostic has not already said.
        if requires_statement_separator(&kind)
            && !consumed_semicolon
            && !self.at_eof()
            && !self.at_punct(Punct::RBrace)
            && !self.newline_before_peek()
            && self.diagnostic_count() == diagnostics_before
        {
            self.record(
                ParseError::unexpected("`;` or a newline between statements", self.peek_text()),
                self.peek_span(),
            );
        }
        let end_span = self.last_span();
        let span = self.join(start_span, end_span);
        let id = self.alloc_id();
        Stmt::new(id, span, kind)
    }

    fn parse_stmt_kind(&mut self) -> StmtKind {
        if self.at_keyword(Keyword::Let) {
            return self.parse_let_stmt();
        }
        if self.at_defer_stmt() {
            self.bump();
            let body = self.parse_expr();
            self.eat_statement_semicolon();
            return StmtKind::Defer(Box::new(body));
        }
        // `arena { ... }` - contextual keyword (an identifier `arena` used
        // as a value still parses as a normal expression). Every
        // allocation made while the block runs lands in a bump arena and
        // is freed wholesale when the block exits - desugars to
        // `{ runtime::arena_push(); defer runtime::arena_pop(); ... }`,
        // so the pop runs on every exit path. The matching
        // `use std::runtime` is injected after parse.
        if self.at_arena_block() {
            return self.parse_arena_block();
        }
        if is_item_start(self) {
            let item = self.parse_item();
            return StmtKind::Item(Box::new(item));
        }
        let before = self.tokens.checkpoint();
        let mut expression = self.with_empty_braces_as_blocks(Self::parse_expr);
        if self.at_punct(Punct::Comma) {
            expression = self.parse_comma_assignment(expression);
        }
        self.reject_parenthesised_assign_targets(&expression);
        if self.tokens.checkpoint() == before && !is_stmt_start(self) {
            self.recover_in_block();
        }
        let has_semi = self.eat_statement_semicolon();
        StmtKind::Expr {
            expr: Box::new(expression),
            has_semi,
        }
    }

    /// `true` at the head of a `defer expr` statement. `defer` is
    /// contextual, never reserved: what it defers has to have an effect,
    /// so the word opens the statement only when a name, a keyword, or a
    /// block follows it. Everywhere else - `defer(x)`, `defer.close()`,
    /// `defer = 1`, `defer + 1`, a trailing `defer` - it is an ordinary
    /// identifier.
    fn at_defer_stmt(&self) -> bool {
        use gossamer_lex::TokenKind;
        self.at_contextual_word("defer")
            && matches!(
                self.peek_nth(1).kind,
                TokenKind::Ident | TokenKind::Keyword(_) | TokenKind::Punct(Punct::LBrace)
            )
    }

    /// `true` at the head of an `arena` block. A bare `arena` followed by
    /// a keyword that can only begin a statement is also claimed here, so
    /// the missing `{` is reported against the block the author meant
    /// rather than against the statement after it.
    fn at_arena_block(&mut self) -> bool {
        use gossamer_lex::TokenKind;
        let cur = self.peek();
        if !matches!(cur.kind, TokenKind::Ident) || self.slice(cur.span) != "arena" {
            return false;
        }
        match self.peek_nth(1).kind {
            TokenKind::Punct(Punct::LBrace) => true,
            TokenKind::Keyword(keyword) => is_stmt_start_keyword(keyword),
            _ => false,
        }
    }

    fn parse_arena_block(&mut self) -> StmtKind {
        let start = self.peek_span();
        self.bump(); // `arena`
        if !self.expect_punct(Punct::LBrace, "to open the `arena` block") {
            let expr = Expr::new(self.alloc_id(), start, ExprKind::Error);
            return StmtKind::Expr {
                expr: Box::new(expr),
                has_semi: true,
            };
        }
        let mut block = self.parse_block_body();
        // Tag the desugared block so the front-end runs the arena-escape
        // check (GM0003) on it - the raw `runtime::arena_push/pop`
        // primitive stays unchecked.
        block.kind = gossamer_ast::BlockKind::Arena;
        let runtime_call = |p: &mut Self, fn_name: &str, span| {
            let path = Expr::new(
                p.alloc_id(),
                span,
                ExprKind::Path(PathExpr::from_names(["runtime", fn_name])),
            );
            Expr::new(
                p.alloc_id(),
                span,
                ExprKind::Call {
                    callee: Box::new(path),
                    args: Vec::new(),
                },
            )
        };
        let push_call = runtime_call(self, "arena_push", start);
        let pop_call = runtime_call(self, "arena_pop", start);
        let mut stmts = Vec::with_capacity(block.stmts.len() + 2);
        stmts.push(Stmt::new(
            self.alloc_id(),
            start,
            StmtKind::Expr {
                expr: Box::new(push_call),
                has_semi: true,
            },
        ));
        stmts.push(Stmt::new(
            self.alloc_id(),
            start,
            StmtKind::Defer(Box::new(pop_call)),
        ));
        stmts.append(&mut block.stmts);
        block.stmts = stmts;
        // An arena block is statement-position only: its value would
        // outlive the arena, so any tail expression is demoted to a
        // statement and the block yields unit.
        if let Some(tail) = block.tail.take() {
            let span = tail.span;
            block.stmts.push(Stmt::new(
                self.alloc_id(),
                span,
                StmtKind::Expr {
                    expr: tail,
                    has_semi: true,
                },
            ));
        }
        let end = self.peek_span();
        let expr = Expr::new(
            self.alloc_id(),
            self.join(start, end),
            ExprKind::Block(block),
        );
        StmtKind::Expr {
            expr: Box::new(expr),
            has_semi: true,
        }
    }

    fn parse_let_stmt(&mut self) -> StmtKind {
        self.bump();
        let pattern = self.parse_let_pattern();
        let ty = if self.eat_punct(Punct::Colon) {
            Some(self.parse_type())
        } else {
            None
        };
        // A pattern that can fail to match owns the `{` that follows its
        // initialiser: it opens the `else` block, never a struct literal.
        let refutable = is_refutable_pattern(&pattern);
        let init = if self.eat_punct(Punct::Eq) {
            if refutable {
                self.enter_no_struct();
            }
            // A `let` initialiser is not a place a block may begin, so `{}`
            // here is the empty map it spells whatever the enclosing
            // statement preferred.
            let expr = self.with_empty_braces_as_maps(Self::parse_comma_value_list);
            if refutable {
                self.leave_no_struct();
            }
            Some(Box::new(expr))
        } else {
            None
        };
        if refutable && !self.at_keyword(Keyword::Else) && self.at_punct(Punct::LBrace) {
            self.record(ParseError::RefutableLetNeedsElse, self.peek_span());
            if let Some(init) = init {
                return self.parse_let_else_block(pattern, init);
            }
        }
        // `let PAT = init else { … }` - desugar to a `match` binding so the
        // refutable pattern's bindings escape into the enclosing scope
        // (Rust/Swift semantics), reusing the match lowering on every tier.
        // The else block must diverge; that is enforced by its `!`-typed
        // position in the wildcard arm during type-checking.
        match (self.at_keyword(Keyword::Else), init) {
            (true, Some(init)) => self.desugar_let_else(pattern, init),
            (_, init) => {
                self.eat_statement_semicolon();
                StmtKind::Let { pattern, ty, init }
            }
        }
    }

    /// Completes a comma-separated assignment whose first place is already
    /// parsed: `a, b = x, y` and `a, b += x, y` write the places element-wise
    /// from the value on the right.
    fn parse_comma_assignment(&mut self, first: Expr) -> Expr {
        let start = first.span;
        let mut places = vec![first];
        while self.eat_punct(Punct::Comma) {
            places.push(self.with_empty_braces_as_blocks(Self::parse_expr_no_assign));
        }
        let places_span = self.join(start, self.last_span());
        let place = Expr::new(self.alloc_id(), places_span, ExprKind::Tuple(places));
        let Some(op) = self.eat_assign_op() else {
            self.record(
                ParseError::unexpected("an assignment operator", self.peek_text()),
                self.peek_span(),
            );
            return place;
        };
        let value = self.with_empty_braces_as_blocks(Self::parse_comma_value_list);
        let span = self.join(start, value.span);
        Expr::new(
            self.alloc_id(),
            span,
            ExprKind::Assign {
                op,
                place: Box::new(place),
                value: Box::new(value),
            },
        )
    }

    /// Parses the value side of a comma-separated binding or assignment: one
    /// expression, or several gathered into the tuple they stand for.
    pub(crate) fn parse_comma_value_list(&mut self) -> Expr {
        let first = self.parse_expr();
        if !self.at_punct(Punct::Comma) {
            return first;
        }
        let start = first.span;
        let mut elements = vec![first];
        while self.eat_punct(Punct::Comma) {
            elements.push(self.parse_expr_no_assign());
        }
        let span = self.join(start, self.last_span());
        let id = self.alloc_id();
        Expr::new(id, span, ExprKind::Tuple(elements))
    }

    /// Parses a `let` binding's target: one pattern, or several separated by
    /// commas, which bind the elements of a tuple value element-wise.
    ///
    /// The comma-separated list is the one spelling here. Parentheses group a
    /// tuple pattern where a pattern sits beside others - a `match` arm, a
    /// `for` binding, a parameter - and a `let` target has no such neighbour,
    /// so a parenthesised list at this position reports `GP0042` with the
    /// paren-free rewrite.
    fn parse_let_pattern(&mut self) -> Pattern {
        let parenthesised = self.at_punct(Punct::LParen);
        let first = self.parse_pattern();
        if !self.at_punct(Punct::Comma) {
            // A parenthesised pattern standing alone IS the target list, so
            // it takes the paren-free rewrite. One followed by a comma is a
            // nested element of that list and keeps its parentheses.
            if parenthesised && matches!(first.kind, PatternKind::Tuple(_)) {
                self.report_parenthesised_let_pattern(first.span);
            }
            return first;
        }
        let start = first.span;
        let mut elements = vec![first];
        while self.eat_punct(Punct::Comma) {
            elements.push(self.parse_pattern());
        }
        let span = self.join(start, self.last_span());
        let id = self.alloc_id();
        Pattern::new(id, span, PatternKind::Tuple(elements))
    }

    /// Reports a parenthesised assignment target list, which reads the same
    /// way a parenthesised `let` target does and takes the same rewrite.
    fn reject_parenthesised_assign_targets(&mut self, expr: &Expr) {
        let ExprKind::Assign { place, .. } = &expr.kind else {
            return;
        };
        if !matches!(place.kind, ExprKind::Tuple(_)) {
            return;
        }
        if !self.slice(place.span).starts_with('(') {
            return;
        }
        self.report_parenthesised_let_pattern(place.span);
    }

    /// Reports a parenthesised `let` target, carrying the rewrite that drops
    /// the outer parentheses.
    fn report_parenthesised_let_pattern(&mut self, span: Span) {
        let written = self.slice(span);
        let inner = written
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            .map_or(written, str::trim);
        let replacement = inner.to_string();
        self.record(ParseError::LetPatternParens { replacement }, span);
    }

    fn desugar_let_else(&mut self, pattern: Pattern, init: Box<Expr>) -> StmtKind {
        self.bump(); // consume `else`
        self.parse_let_else_block(pattern, init)
    }

    /// Parses the `{ … }` that gives a refutable `let` its diverging path
    /// and builds the `match` the binding desugars to. The `else` keyword
    /// is already consumed, or was omitted and diagnosed.
    fn parse_let_else_block(&mut self, pattern: Pattern, init: Box<Expr>) -> StmtKind {
        self.expect_punct(Punct::LBrace, "to open the `let ... else` block");
        let start = self.last_span();
        let block = self.parse_block_body();
        let else_span = self.join(start, self.last_span());
        let else_expr = Expr::new(self.alloc_id(), else_span, ExprKind::Block(block));
        self.eat_statement_semicolon();

        let mut binds: Vec<(Ident, Mutability)> = Vec::new();
        collect_pattern_bindings(&pattern, &mut binds);
        let pat_span = pattern.span;

        let success_body = self.make_var_tuple_expr(&binds, pat_span);
        let outer_pat = self.make_binding_pattern(&binds, pat_span);
        let wildcard = Pattern::new(self.alloc_id(), else_span, PatternKind::Wildcard);
        let match_expr = Expr::new(
            self.alloc_id(),
            pat_span,
            ExprKind::Match {
                scrutinee: init,
                arms: vec![
                    MatchArm {
                        pattern,
                        guard: None,
                        body: success_body,
                    },
                    MatchArm {
                        pattern: wildcard,
                        guard: None,
                        body: else_expr,
                    },
                ],
            },
        );
        StmtKind::Let {
            pattern: outer_pat,
            ty: None,
            init: Some(Box::new(match_expr)),
        }
    }

    /// Builds the success-arm body: the pattern's bindings as a tuple (single
    /// value when there is one, `()` when there are none).
    fn make_var_tuple_expr(&mut self, binds: &[(Ident, Mutability)], span: Span) -> Expr {
        let mut refs: Vec<Expr> = binds
            .iter()
            .map(|(id, _)| {
                let path = PathExpr {
                    segments: vec![PathSegment {
                        name: id.clone(),
                        generics: Vec::new(),
                    }],
                };
                Expr::new(self.alloc_id(), span, ExprKind::Path(path))
            })
            .collect();
        if refs.len() == 1 {
            refs.pop().expect("len checked")
        } else {
            Expr::new(self.alloc_id(), span, ExprKind::Tuple(refs))
        }
    }

    /// Builds the outer `let` pattern that receives the match result: the
    /// pattern's bindings as a tuple of irrefutable name bindings (single when
    /// there is one, `_` when there are none), each as mutable as the pattern
    /// wrote it.
    fn make_binding_pattern(&mut self, binds: &[(Ident, Mutability)], span: Span) -> Pattern {
        if binds.is_empty() {
            return Pattern::new(self.alloc_id(), span, PatternKind::Wildcard);
        }
        let mut pats: Vec<Pattern> = binds
            .iter()
            .map(|(id, mutability)| {
                Pattern::new(
                    self.alloc_id(),
                    span,
                    PatternKind::Ident {
                        mutability: *mutability,
                        name: id.clone(),
                        subpattern: None,
                    },
                )
            })
            .collect();
        if pats.len() == 1 {
            pats.pop().expect("len checked")
        } else {
            Pattern::new(self.alloc_id(), span, PatternKind::Tuple(pats))
        }
    }
}

/// Returns whether another statement on the same line needs an explicit `;`.
/// Like Rust, block-shaped expressions delimit themselves. Every other
/// expression or declaration needs either a semicolon or an authored newline,
/// so `println value` cannot silently become two expression statements.
fn requires_statement_separator(kind: &StmtKind) -> bool {
    match kind {
        StmtKind::Let { .. } | StmtKind::Defer(_) => true,
        StmtKind::Item(_) => false,
        StmtKind::Expr { has_semi: true, .. } => false,
        StmtKind::Expr {
            expr,
            has_semi: false,
        } => !matches!(
            expr.kind,
            ExprKind::Block(_)
                | ExprKind::Unsafe(_)
                | ExprKind::If { .. }
                | ExprKind::Match { .. }
                | ExprKind::Loop { .. }
                | ExprKind::While { .. }
                | ExprKind::For { .. }
                | ExprKind::Select(_)
        ),
    }
}

/// Collects the binding identifiers introduced by a pattern, with the
/// mutability each was written with, in source order, so `let ... else` can
/// thread them out of the desugared `match`.
fn collect_pattern_bindings(pat: &Pattern, out: &mut Vec<(Ident, Mutability)>) {
    match &pat.kind {
        PatternKind::Ident {
            name,
            subpattern,
            mutability,
        } => {
            out.push((name.clone(), *mutability));
            if let Some(sub) = subpattern {
                collect_pattern_bindings(sub, out);
            }
        }
        PatternKind::Tuple(ps) => {
            for p in ps {
                collect_pattern_bindings(p, out);
            }
        }
        PatternKind::TupleStruct { elems, .. } => {
            for p in elems {
                collect_pattern_bindings(p, out);
            }
        }
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for p in prefix.iter().chain(rest.as_deref()).chain(suffix) {
                collect_pattern_bindings(p, out);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for f in fields {
                match &f.pattern {
                    Some(p) => collect_pattern_bindings(p, out),
                    None => out.push((f.name.clone(), Mutability::Immutable)),
                }
            }
        }
        // Every alternative of an or-pattern binds the same names; take the first.
        PatternKind::Or(ps) => {
            if let Some(first) = ps.first() {
                collect_pattern_bindings(first, out);
            }
        }
        PatternKind::Ref { inner, .. } => collect_pattern_bindings(inner, out),
        PatternKind::Wildcard
        | PatternKind::Literal(_)
        | PatternKind::Path(_)
        | PatternKind::Range { .. }
        | PatternKind::Rest
        | PatternKind::Error => {}
    }
}

/// `true` for a pattern whose match can fail on some value of its type,
/// as far as syntax alone can tell. Struct patterns are excluded: a
/// braced pattern is written the same way for an irrefutable struct
/// destructure as for an enum variant.
fn is_refutable_pattern(pattern: &Pattern) -> bool {
    match &pattern.kind {
        PatternKind::Literal(_) | PatternKind::Range { .. } | PatternKind::TupleStruct { .. } => {
            true
        }
        PatternKind::Path(path) => path.segments.len() > 1,
        PatternKind::Or(alternatives) => alternatives.iter().any(is_refutable_pattern),
        PatternKind::Ref { inner, .. } => is_refutable_pattern(inner),
        _ => false,
    }
}
