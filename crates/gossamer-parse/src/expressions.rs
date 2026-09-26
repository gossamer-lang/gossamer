//! Expression parsing - Pratt-style precedence climbing driven by
//! SPEC §4.7 plus hand-written prefix and postfix handlers.

#![forbid(unsafe_code)]

use gossamer_ast::visitor::{
    VisitorMut, walk_expr_mut, walk_item_mut, walk_pattern_mut, walk_stmt_mut, walk_type_mut,
};
use gossamer_ast::{
    ArrayExpr, AssignOp, BinaryOp, Block, ClosureParam, Expr, ExprKind, FieldSelector, Ident, Item,
    Label, Literal, MatchArm, Mutability, NodeIdGenerator, PathExpr, PathSegment, Pattern,
    PatternKind, RangeKind, Stmt, StmtKind, StructExprField, Type, UnaryOp,
};
use gossamer_ast::{
    PAD_REQUEST_CENTER, PAD_REQUEST_DEFAULT, PAD_REQUEST_LEFT, PAD_REQUEST_RIGHT,
    PAD_REQUEST_ZERO_FLAG,
};
use gossamer_lex::{Keyword, Punct, Span, TokenKind};

use crate::builtin_macros::{is_desugar_macro, is_format_macro};
use crate::diagnostic::ParseError;
use crate::parser::Parser;
use crate::patterns::{
    byte_literal_value, byte_string_literal_value, char_literal_value, string_literal_value,
};

/// Precedence level strictly stronger than any binary operator.
const PREC_BELOW_ASSIGN: u8 = 17;

/// Range (`..` / `..=`) precedence: looser than every arithmetic and
/// logical operator (`i * i..n` is `(i * i)..n`), tighter than `|>`
/// (`0..n |> f` pipes the whole range).
const RANGE_PREC: u8 = 14;

/// Maximum precedence for a single clause inside an `if`/`while`
/// condition chain. Equal to `&&`'s precedence so a clause stops at
/// the next `&&` separator (and rejects an unparenthesised `||`,
/// which binds looser than `&&`).
const COND_CLAUSE_PREC: u8 = BinaryOp::And.precedence();

/// A single clause in an `if`/`while` condition chain.
enum CondClause {
    /// `let PAT = SCRUTINEE`; the binding is in scope for later
    /// clauses and the branch body.
    Let {
        /// Pattern that must match for the clause to succeed.
        pattern: Pattern,
        /// Value tested against the pattern.
        scrutinee: Expr,
    },
    /// A boolean sub-expression that must evaluate to `true`.
    Bool(Expr),
}

/// The parsed head of an `if`/`while`: either an ordinary boolean
/// expression or a chain of `&&`-joined clauses with at least one
/// `let` binding.
enum Condition {
    /// No `let` clause; a normal boolean expression.
    Plain(Expr),
    /// One or more `&&`-joined clauses, at least one of which binds.
    Chain(Vec<CondClause>),
}

impl Parser<'_> {
    /// Parses a full expression, including assignment at statement position.
    pub(crate) fn parse_expr(&mut self) -> Expr {
        self.parse_expr_with_prec(PREC_BELOW_ASSIGN, true)
    }

    /// Parses a const generic argument: a literal, or a braced block for a
    /// computed value.
    ///
    /// Deliberately narrower than an expression. A generic argument list ends
    /// at `>`, which is also a binary operator, so parsing the argument as a
    /// full expression makes `f::<3>(xs)` read as the comparison `3 > (xs)`
    /// and the list never closes.
    pub(crate) fn parse_const_generic_arg(&mut self) -> Expr {
        let start = self.peek_span();
        if self.at_punct(Punct::LBrace) {
            return self.parse_primary();
        }
        if let Some(literal) = self.try_parse_literal() {
            let span = self.join(start, self.last_span());
            let id = self.alloc_id();
            return Expr::new(id, span, ExprKind::Literal(literal));
        }
        self.record(
            ParseError::unexpected("a literal or `{ ... }` const argument", self.peek_text()),
            start,
        );
        let id = self.alloc_id();
        Expr::new(id, start, ExprKind::Error)
    }

    /// Parses an expression that is not allowed to bind assignment at
    /// its top level (e.g. argument positions).
    pub(crate) fn parse_expr_no_assign(&mut self) -> Expr {
        self.parse_expr_with_prec(PREC_BELOW_ASSIGN, false)
    }

    /// Precedence-climbing core used by `parse_expr`.
    fn parse_expr_with_prec(&mut self, max_prec: u8, allow_assign: bool) -> Expr {
        let entry_span = self.peek_span();
        if self.enter_recursion(entry_span).is_err() {
            let id = self.alloc_id();
            // Skip one token so the outer driver cannot loop on the same
            // input forever after the limit fires.
            if !self.at_eof() {
                self.bump();
            }
            return Expr::new(id, entry_span, ExprKind::Error);
        }
        let result = self.parse_expr_with_prec_inner(max_prec, allow_assign);
        self.leave_recursion();
        result
    }

    fn parse_expr_with_prec_inner(&mut self, max_prec: u8, allow_assign: bool) -> Expr {
        let lhs = self.parse_prefix();
        self.continue_binary(lhs, max_prec, allow_assign)
    }

    /// Resumes Pratt-style precedence climbing from an already-parsed
    /// left-hand side. Used both by the core driver and by condition
    /// parsing, where a `&&`-chain is parsed clause-by-clause and then
    /// rejoined into a normal expression when no `let` clause appears.
    fn continue_binary(&mut self, mut lhs: Expr, max_prec: u8, allow_assign: bool) -> Expr {
        loop {
            if allow_assign && self.peek_assign_op().is_some() {
                lhs = self.parse_assignment(lhs);
                break;
            }
            if let Some(op) = self.peek_binary_op() {
                let precedence = op.precedence();
                if precedence >= max_prec {
                    break;
                }
                if op == BinaryOp::BitOr && self.in_pattern_pipe() {
                    break;
                }
                if is_unary_startable(op) && self.newline_before_peek() && !self.in_paren_group {
                    break;
                }
                self.bump();
                if is_non_associative_compare(op)
                    && self.peek_matches_compare_after_parse(op, precedence)
                {
                    self.record(
                        ParseError::NonAssociativeCompare {
                            op: op.as_str().to_string(),
                        },
                        lhs.span,
                    );
                }
                let rhs_amp = self.peek_span();
                let rhs = if op == BinaryOp::PipeGt {
                    self.with_pipe_step(|p| p.parse_expr_with_prec(precedence, false))
                } else {
                    self.parse_expr_with_prec(precedence, false)
                };
                // `a + &b` is the same operand as `a + b`: there is no
                // shared-reference type for the operator to see, and an
                // operand is read without copying either way. A migration
                // that only rewrote argument position leaves these
                // behind, and nothing else reports them.
                let rhs = self.strip_shared_operand_reference(rhs, rhs_amp);
                if op == BinaryOp::PipeGt {
                    lhs = self.validate_pipe_rhs(lhs, rhs);
                    continue;
                }
                let span = self.join(lhs.span, rhs.span);
                let id = self.alloc_id();
                lhs = Expr::new(
                    id,
                    span,
                    ExprKind::Binary {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                );
                continue;
            }
            if self.peek_range_op().is_some() {
                // Range binds looser than every arithmetic / logical
                // operator and tighter than `|>` (SPEC §4.7), so
                // `i * i..n` is `(i * i)..n` and `0..n |> f` pipes the
                // whole range. Inside a tighter operand parse the `..`
                // belongs to the enclosing level.
                if RANGE_PREC >= max_prec {
                    break;
                }
                if self.at_match_arm_open_start_range_boundary() {
                    break;
                }
                let range = self.parse_range_infix(lhs);
                lhs = range;
                continue;
            }
            if self.at_keyword(Keyword::As) {
                self.bump();
                let ty = self.parse_type();
                let span = self.join(lhs.span, ty.span);
                let id = self.alloc_id();
                lhs = Expr::new(
                    id,
                    span,
                    ExprKind::Cast {
                        value: Box::new(lhs),
                        ty: Box::new(ty),
                    },
                );
                continue;
            }
            break;
        }
        lhs
    }

    fn peek_matches_compare_after_parse(&self, _op: BinaryOp, _precedence: u8) -> bool {
        false
    }

    fn peek_binary_op(&self) -> Option<BinaryOp> {
        use Punct::{
            Amp, AmpAmp, Caret, EqEq, Gt, GtEq, Lt, LtEq, Minus, MinusPercent, NotEq, Percent,
            Pipe, PipeGt, PipePipe, Plus, PlusPercent, ShiftL, ShiftR, Slash, Star, StarPercent,
        };
        let TokenKind::Punct(punct) = self.peek().kind else {
            return None;
        };
        Some(match punct {
            Star => BinaryOp::Mul,
            Slash => BinaryOp::Div,
            Percent => BinaryOp::Rem,
            StarPercent => BinaryOp::WrappingMul,
            PlusPercent => BinaryOp::WrappingAdd,
            MinusPercent => BinaryOp::WrappingSub,
            Plus => BinaryOp::Add,
            Minus => BinaryOp::Sub,
            ShiftL => BinaryOp::Shl,
            ShiftR => BinaryOp::Shr,
            Amp => BinaryOp::BitAnd,
            Caret => BinaryOp::BitXor,
            Pipe => BinaryOp::BitOr,
            EqEq => BinaryOp::Eq,
            NotEq => BinaryOp::Ne,
            Lt => BinaryOp::Lt,
            LtEq => BinaryOp::Le,
            Gt => BinaryOp::Gt,
            GtEq => BinaryOp::Ge,
            AmpAmp => BinaryOp::And,
            PipePipe => BinaryOp::Or,
            PipeGt => BinaryOp::PipeGt,
            _ => return None,
        })
    }

    fn peek_range_op(&self) -> Option<RangeKind> {
        if self.at_punct(Punct::DotDotEq) {
            return Some(RangeKind::Inclusive);
        }
        if self.at_punct(Punct::DotDot) {
            return Some(RangeKind::Exclusive);
        }
        None
    }

    fn at_match_arm_open_start_range_boundary(&self) -> bool {
        if !self.in_match_arm_body() || !self.newline_before_peek() {
            return false;
        }
        let inclusive = match self.peek().kind {
            TokenKind::Punct(Punct::DotDot) => false,
            TokenKind::Punct(Punct::DotDotEq) => true,
            _ => return false,
        };
        let mut after_pattern = 1;
        match self.peek_nth(after_pattern).kind {
            TokenKind::IntLit
            | TokenKind::FloatLit
            | TokenKind::StringLit
            | TokenKind::RawStringLit { .. }
            | TokenKind::TripleStringLit
            | TokenKind::CharLit
            | TokenKind::ByteLit
            | TokenKind::Keyword(Keyword::True | Keyword::False) => {
                after_pattern += 1;
            }
            TokenKind::Punct(Punct::Minus)
                if matches!(
                    self.peek_nth(after_pattern + 1).kind,
                    TokenKind::IntLit | TokenKind::FloatLit
                ) =>
            {
                after_pattern += 2;
            }
            _ if !inclusive => {}
            _ => return false,
        }
        matches!(
            self.peek_nth(after_pattern).kind,
            TokenKind::Punct(Punct::FatArrow) | TokenKind::Keyword(Keyword::If)
        )
    }

    fn parse_range_infix(&mut self, lhs: Expr) -> Expr {
        let kind = if self.eat_punct(Punct::DotDotEq) {
            RangeKind::Inclusive
        } else {
            self.bump();
            RangeKind::Exclusive
        };
        let end = if self.range_upper_bound_starts_here() {
            Some(Box::new(self.parse_expr_with_prec(RANGE_PREC, false)))
        } else {
            None
        };
        if kind == RangeKind::Inclusive && end.is_none() {
            self.record(ParseError::InclusiveRangeMissingEnd, self.last_span());
        }
        let end_span = end.as_ref().map_or(self.last_span(), |expr| expr.span);
        let span = self.join(lhs.span, end_span);
        let id = self.alloc_id();
        Expr::new(
            id,
            span,
            ExprKind::Range {
                start: Some(Box::new(lhs)),
                end,
                kind,
            },
        )
    }

    /// Consumes an assignment operator when one is next.
    pub(crate) fn eat_assign_op(&mut self) -> Option<AssignOp> {
        let op = self.peek_assign_op()?;
        self.bump();
        Some(op)
    }

    fn peek_assign_op(&self) -> Option<AssignOp> {
        let TokenKind::Punct(punct) = self.peek().kind else {
            return None;
        };
        Some(match punct {
            Punct::Eq => AssignOp::Assign,
            Punct::PlusEq => AssignOp::AddAssign,
            Punct::MinusEq => AssignOp::SubAssign,
            Punct::StarEq => AssignOp::MulAssign,
            Punct::SlashEq => AssignOp::DivAssign,
            Punct::PercentEq => AssignOp::RemAssign,
            Punct::AmpEq => AssignOp::BitAndAssign,
            Punct::PipeEq => AssignOp::BitOrAssign,
            Punct::CaretEq => AssignOp::BitXorAssign,
            Punct::ShiftLEq => AssignOp::ShlAssign,
            Punct::ShiftREq => AssignOp::ShrAssign,
            Punct::PlusPercentEq => AssignOp::WrappingAddAssign,
            Punct::MinusPercentEq => AssignOp::WrappingSubAssign,
            Punct::StarPercentEq => AssignOp::WrappingMulAssign,
            _ => return None,
        })
    }

    fn parse_assignment(&mut self, place: Expr) -> Expr {
        let Some(op) = self.peek_assign_op() else {
            return place;
        };
        self.bump();
        let value_amp = self.peek_span();
        let value = self.parse_expr_with_prec(PREC_BELOW_ASSIGN, false);
        // `acc += &frag` reads the same operand `acc += frag` does. A
        // plain `=` is not the same question: its right side is a value,
        // and a binding may hold a reference, so `reference = &b` rebinds
        // one and keeps its sigil.
        let value = if op == AssignOp::Assign {
            value
        } else {
            self.strip_shared_operand_reference(value, value_amp)
        };
        let span = self.join(place.span, value.span);
        let id = self.alloc_id();
        Expr::new(
            id,
            span,
            ExprKind::Assign {
                op,
                place: Box::new(place),
                value: Box::new(value),
            },
        )
    }

    /// Reduces one `|>` step.
    ///
    /// The pipe composes free functions, which have no receiver to chain
    /// from. A step is either a bare callable (`x |> f`), which takes the
    /// value as its only argument, or a closure (`x |> |v| f(a, v)`), whose
    /// parameter is the slot the value fills. A step that writes its own
    /// arguments has no slot left for the piped value and is rejected in
    /// favour of the closure that names one.
    fn validate_pipe_rhs(&mut self, lhs: Expr, rhs: Expr) -> Expr {
        self.report_pipe_step_shape(&rhs);
        let span = self.join(lhs.span, rhs.span);
        let id = self.alloc_id();
        Expr::new(
            id,
            span,
            ExprKind::Binary {
                op: BinaryOp::PipeGt,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        )
    }

    /// Reports what a `|>` step's shape lacks.
    ///
    /// A step built out of an expression the parser already rejected reports
    /// nothing further: the shape it would be judged on is the error's, not
    /// the author's.
    fn report_pipe_step_shape(&mut self, rhs: &Expr) {
        if step_contains_parse_error(rhs) {
            return;
        }
        let callable = matches!(
            rhs.kind,
            ExprKind::Path(_)
                | ExprKind::Call { .. }
                | ExprKind::MethodCall { .. }
                | ExprKind::Closure { .. }
        );
        if !callable {
            self.record(ParseError::PipeRhsInvalid, rhs.span);
        } else if pipe_step_has_arguments(rhs) {
            let replacement = self.pipe_step_closure_rewrite(rhs.span);
            self.record(ParseError::PipeStepNeedsClosure { replacement }, rhs.span);
        }
    }

    /// Parses a prefix (primary + unary) expression.
    fn parse_prefix(&mut self) -> Expr {
        let entry_span = self.peek_span();
        if self.enter_recursion(entry_span).is_err() {
            let id = self.alloc_id();
            if !self.at_eof() {
                self.bump();
            }
            return Expr::new(id, entry_span, ExprKind::Error);
        }
        let result = self.parse_prefix_inner();
        self.leave_recursion();
        result
    }

    fn parse_prefix_inner(&mut self) -> Expr {
        if self.peek_range_op().is_some() {
            return self.parse_open_range_prefix();
        }
        if let Some(prefix_op) = self.peek_unary_op() {
            let op_span = self.peek_span();
            self.bump();
            let mutability_consumed =
                prefix_op == UnaryOp::RefShared && self.eat_keyword(Keyword::Mut);
            let actual_op = if mutability_consumed {
                UnaryOp::RefMut
            } else {
                prefix_op
            };
            let operand = self.parse_prefix();
            let span = self.join(op_span, operand.span);
            let id = self.alloc_id();
            return Expr::new(
                id,
                span,
                ExprKind::Unary {
                    op: actual_op,
                    operand: Box::new(operand),
                },
            );
        }
        self.parse_postfix()
    }

    /// Parses a range with an omitted lower bound: `..end`, `..=end`, or
    /// `..`. The normal infix path handles an omitted upper bound.
    fn parse_open_range_prefix(&mut self) -> Expr {
        let start_span = self.peek_span();
        let kind = if self.eat_punct(Punct::DotDotEq) {
            RangeKind::Inclusive
        } else {
            self.bump();
            RangeKind::Exclusive
        };
        let end = if self.range_upper_bound_starts_here() {
            Some(Box::new(self.parse_expr_with_prec(RANGE_PREC, false)))
        } else {
            None
        };
        if kind == RangeKind::Inclusive && end.is_none() {
            self.record(ParseError::InclusiveRangeMissingEnd, self.last_span());
        }
        let end_span = end.as_ref().map_or(self.last_span(), |expr| expr.span);
        let id = self.alloc_id();
        Expr::new(
            id,
            self.join(start_span, end_span),
            ExprKind::Range {
                start: None,
                end,
                kind,
            },
        )
    }

    /// Whether the token after a range operator begins its upper bound.
    ///
    /// A newline ends an open-ended range before a following expression.
    /// Operators such as `|>` are parsed by the enclosing expression loop,
    /// so multiline iterator pipelines remain valid. In condition-like
    /// positions, `{` belongs to the surrounding `for`/`if`/`while` body,
    /// rather than serving as a block-valued range bound.
    fn range_upper_bound_starts_here(&self) -> bool {
        is_expression_start(self)
            && !self.newline_before_peek()
            && !(self.struct_literal_forbidden() && self.at_punct(Punct::LBrace))
    }

    fn peek_unary_op(&self) -> Option<UnaryOp> {
        let TokenKind::Punct(punct) = self.peek().kind else {
            return None;
        };
        Some(match punct {
            Punct::Minus => UnaryOp::Neg,
            Punct::Bang => UnaryOp::Not,
            Punct::Amp => UnaryOp::RefShared,
            Punct::Star => UnaryOp::Deref,
            _ => return None,
        })
    }

    fn parse_postfix(&mut self) -> Expr {
        let mut primary = self.parse_primary();
        loop {
            if self.at_punct(Punct::Dot) {
                primary = self.parse_dot_suffix(primary);
                continue;
            }
            if self.at_punct(Punct::LParen) {
                // A `(` on the next source line never attaches to the
                // previous expression. Without this break, a function
                // body like `for {...}\n(a, 7.0)` parses as one
                // expression - calling the for-loop's `()` result with
                // `(a, 7.0)` as args - instead of two statements.
                // Mirrors the same statement-boundary rule already
                // applied to `&` / `*` / `-` in `parse_expr_with_prec`.
                if self.newline_before_peek() {
                    break;
                }
                primary = self.parse_call_suffix(primary);
                continue;
            }
            if self.at_punct(Punct::LBracket) {
                // Same as `(` above: a `[` on the next line opens a
                // fresh array literal, not an index into the previous
                // expression.
                if self.newline_before_peek() {
                    break;
                }
                primary = self.parse_index_suffix(primary);
                continue;
            }
            if self.at_punct(Punct::Question) {
                let q_span = self.peek_span();
                self.bump();
                let span = self.join(primary.span, q_span);
                let id = self.alloc_id();
                primary = Expr::new(id, span, ExprKind::Try(Box::new(primary)));
                continue;
            }
            // `[a, b]>` used to spell a stack literal. Report the
            // removal against the whole spelling rather than letting the
            // `>` surface as a stray comparison operator.
            if self.at_punct(Punct::Gt)
                && !self.newline_before_peek()
                && self.peek_span().start == primary.span.end
                && matches!(primary.kind, ExprKind::Array(_))
            {
                let end_span = self.peek_span();
                self.bump();
                let span = self.join(primary.span, end_span);
                self.record(
                    ParseError::RemovedCollectionLiteral {
                        spelling: "[..]>".to_string(),
                        container: "Stack".to_string(),
                    },
                    span,
                );
                let id = self.alloc_id();
                primary = Expr::new(id, span, ExprKind::Error);
                continue;
            }
            break;
        }
        primary
    }

    fn parse_dot_suffix(&mut self, receiver: Expr) -> Expr {
        self.bump();
        let token = self.peek();
        let start_span = receiver.span;
        match token.kind {
            TokenKind::IntLit => {
                self.bump();
                let text = self.slice(token.span);
                let index = text.parse::<u32>().unwrap_or_else(|_| {
                    self.record(ParseError::InvalidTupleIndex, token.span);
                    0
                });
                let span = self.join(start_span, token.span);
                let id = self.alloc_id();
                Expr::new(
                    id,
                    span,
                    ExprKind::FieldAccess {
                        receiver: Box::new(receiver),
                        field: FieldSelector::Index(index),
                    },
                )
            }
            // The lexer reads `t.0.1` as `t` `.` `0.1`, so a float literal in
            // field position is a chained pair of tuple indices.
            TokenKind::FloatLit => self.parse_chained_tuple_index(receiver, token.span),
            TokenKind::Ident => {
                self.bump();
                let name = Ident::new(self.slice(token.span));
                self.parse_method_or_field(receiver, name, token.span)
            }
            TokenKind::Keyword(Keyword::Await) => {
                self.bump();
                let name = Ident::new("await");
                self.parse_method_or_field(receiver, name, token.span)
            }
            _ => {
                self.record(
                    ParseError::unexpected("a field or method name after `.`", self.peek_text()),
                    token.span,
                );
                receiver
            }
        }
    }

    /// Splits a float literal in field position (`t.0.1`) into the two tuple
    /// indices it spells.
    fn parse_chained_tuple_index(&mut self, receiver: Expr, token_span: Span) -> Expr {
        self.bump();
        let text = self.slice(token_span);
        let start_span = receiver.span;
        let span = self.join(start_span, token_span);
        let Some((outer, inner)) = text.split_once('.') else {
            self.record(ParseError::InvalidTupleIndex, token_span);
            return receiver;
        };
        let (Ok(outer), Ok(inner)) = (outer.parse::<u32>(), inner.parse::<u32>()) else {
            self.record(ParseError::InvalidTupleIndex, token_span);
            return receiver;
        };
        let outer_id = self.alloc_id();
        let outer_access = Expr::new(
            outer_id,
            span,
            ExprKind::FieldAccess {
                receiver: Box::new(receiver),
                field: FieldSelector::Index(outer),
            },
        );
        let id = self.alloc_id();
        Expr::new(
            id,
            span,
            ExprKind::FieldAccess {
                receiver: Box::new(outer_access),
                field: FieldSelector::Index(inner),
            },
        )
    }

    fn parse_method_or_field(&mut self, receiver: Expr, name: Ident, name_span: Span) -> Expr {
        let generics = if self.at_punct(Punct::ColonColon)
            && self.peek_nth(1).kind == TokenKind::Punct(Punct::Lt)
        {
            self.bump();
            let checkpoint = self.tokens.checkpoint();
            self.bump();
            let args = self.parse_generic_args_in_turbofish();
            if args.is_empty() {
                self.tokens.rewind(checkpoint);
                Vec::new()
            } else {
                args
            }
        } else if self.bare_method_turbofish_call_ahead() {
            let checkpoint = self.tokens.checkpoint();
            self.bump();
            let args = self.parse_generic_args_in_turbofish();
            if args.is_empty() || !self.at_punct(Punct::LParen) {
                self.tokens.rewind(checkpoint);
                Vec::new()
            } else {
                args
            }
        } else {
            Vec::new()
        };
        if self.at_punct(Punct::LParen) {
            self.bump();
            let (args, labels) = self.parse_labelled_call_args();
            let end_span = self.last_span();
            let span = self.join(receiver.span, end_span);
            let id = self.alloc_id();
            self.record_named_args(id, labels);
            return Expr::new(
                id,
                span,
                ExprKind::MethodCall {
                    receiver: Box::new(receiver),
                    name,
                    name_span,
                    desugared_from: None,
                    generics,
                    args,
                },
            );
        }
        let span = self.join(receiver.span, name_span);
        let id = self.alloc_id();
        Expr::new(
            id,
            span,
            ExprKind::FieldAccess {
                receiver: Box::new(receiver),
                field: FieldSelector::Named(name),
            },
        )
    }

    fn parse_generic_args_in_turbofish(&mut self) -> Vec<gossamer_ast::GenericArg> {
        let mut args = Vec::new();
        while !self.at_close_angle() && !self.at_eof() {
            args.push(self.parse_generic_arg());
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_close_angle("to close turbofish generics");
        args
    }

    fn parse_call_suffix(&mut self, callee: Expr) -> Expr {
        self.bump();
        let (args, labels) = self.parse_labelled_call_args();
        let end_span = self.last_span();
        let span = self.join(callee.span, end_span);
        // 0.7.0: `errors::newf(fmt, args…)` is a format-shaped sibling
        // of `errors::new`. Rewrite at parse time to
        // `errors::new(format!(fmt, args…))` so the same parse-time
        // template expansion that powers `format!` produces a single
        // `__concat`-based String - keeps all three tiers identical
        // (the VM gets the same lowered call shape compiled-mode
        // sees, no separate variadic runtime helper required).
        if is_errors_newf_path(&callee) {
            return self.rewrite_errors_newf(span, args);
        }
        let id = self.alloc_id();
        self.record_named_args(id, labels);
        Expr::new(
            id,
            span,
            ExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    fn rewrite_errors_newf(&mut self, span: gossamer_lex::Span, args: Vec<Expr>) -> Expr {
        let concat = self.expand_format_macro("format", args);
        let concat_id = self.alloc_id();
        let concat_expr = Expr::new(concat_id, span, concat);
        // Build a real two-segment `errors::new` path so the
        // resolver picks up the module-qualified import instead
        // of trying to look up a single-segment "errors::new"
        // identifier (which doesn't exist).
        let new_path = PathExpr::from_names(["errors", "new"]);
        let callee_id = self.alloc_id();
        let callee = Expr::new(callee_id, span, ExprKind::Path(new_path));
        let call_id = self.alloc_id();
        Expr::new(
            call_id,
            span,
            ExprKind::Call {
                callee: Box::new(callee),
                args: vec![concat_expr],
            },
        )
    }

    fn parse_index_suffix(&mut self, base: Expr) -> Expr {
        // `_[a, b]` and `_[]` used to spell a min-heap literal; a single
        // `_[i]` is an ordinary index on a binding named `_`, so only the
        // list shapes are rejected here.
        if matches!(&base.kind, ExprKind::Path(path) if is_underscore_path(path)) {
            return self.reject_min_heap_literal(base);
        }
        self.bump();
        let index = self.with_struct_literals_allowed(Self::parse_expr_no_assign);
        self.expect_punct(Punct::RBracket, "to close index expression");
        let end_span = self.last_span();
        let span = self.join(base.span, end_span);
        let id = self.alloc_id();
        Expr::new(
            id,
            span,
            ExprKind::Index {
                base: Box::new(base),
                index: Box::new(index),
            },
        )
    }

    /// Parses `_[...]`, which is an ordinary index for a single index and
    /// the removed min-heap literal for every other shape.
    fn reject_min_heap_literal(&mut self, base: Expr) -> Expr {
        self.bump();
        if self.at_punct(Punct::RBracket) {
            let end_span = self.peek_span();
            self.bump();
            let span = self.join(base.span, end_span);
            self.record(
                ParseError::RemovedCollectionLiteral {
                    spelling: "_[..]".to_string(),
                    container: "MinHeap".to_string(),
                },
                span,
            );
            let id = self.alloc_id();
            return Expr::new(id, span, ExprKind::Error);
        }
        let index = self.with_struct_literals_allowed(Self::parse_expr_no_assign);
        if self.at_punct(Punct::Comma) || self.at_punct(Punct::Semi) {
            while !self.at_punct(Punct::RBracket) && !self.at_eof() {
                self.bump();
            }
            let end_span = self.peek_span();
            self.bump();
            let span = self.join(base.span, end_span);
            self.record(
                ParseError::RemovedCollectionLiteral {
                    spelling: "_[..]".to_string(),
                    container: "MinHeap".to_string(),
                },
                span,
            );
            let id = self.alloc_id();
            return Expr::new(id, span, ExprKind::Error);
        }
        self.expect_punct(Punct::RBracket, "to close index expression");
        let end_span = self.last_span();
        let span = self.join(base.span, end_span);
        let id = self.alloc_id();
        Expr::new(
            id,
            span,
            ExprKind::Index {
                base: Box::new(base),
                index: Box::new(index),
            },
        )
    }

    /// Parses a macro-style argument list. `name: value` is not a label
    /// here - macros take positional arguments only - so it reaches the
    /// expression parser and is reported there.
    pub(crate) fn parse_call_args(&mut self) -> Vec<Expr> {
        self.parse_arg_list(false).0
    }

    /// Parses a call's argument list, returning the arguments as written
    /// alongside any `name = value` labels.
    pub(crate) fn parse_labelled_call_args(&mut self) -> (Vec<Expr>, Vec<gossamer_ast::NamedArg>) {
        self.parse_arg_list(true)
    }

    /// Source text of `span`, for building a rewrite that keeps the author's
    /// own spelling of the surrounding code.
    fn span_text(&self, span: Span) -> &str {
        self.source
            .get(span.start as usize..span.end as usize)
            .unwrap_or("")
    }

    /// Rewrites a step that writes every argument into the closure it stands
    /// for: `f(a)` becomes `|v| f(v, a)`. Every std free function takes its
    /// data first, so the leading slot is the one a piped value fills.
    /// Answers `None` when the call's own text cannot be recovered, which
    /// suppresses the suggestion rather than offering a wrong one.
    fn pipe_step_closure_rewrite(&self, span: Span) -> Option<String> {
        let text = self.span_text(span);
        let open = text.find('(')?;
        let close = text.rfind(')')?;
        let parameter = closure_parameter_name(text)?;
        let written = text.get(open + 1..close)?.trim();
        if written.is_empty() {
            return Some(format!("|{parameter}| {}{parameter})", &text[..=open]));
        }
        Some(format!(
            "|{parameter}| {}{parameter}, {written})",
            &text[..=open]
        ))
    }

    fn parse_arg_list(&mut self, allow_labels: bool) -> (Vec<Expr>, Vec<gossamer_ast::NamedArg>) {
        self.with_struct_literals_allowed(|p| {
            let mut args = Vec::new();
            let mut labels = Vec::new();
            // Whether each element was separated from the next by a
            // newline rather than a comma, which is what decides whether
            // dropping a `&` can change the parse.
            let mut newline_separated: Vec<bool> = Vec::new();
            while !p.at_punct(Punct::RParen) && !p.at_eof() {
                if p.at_punct(Punct::DotDotDot) {
                    p.bump();
                    continue;
                }
                if allow_labels && let Some((name, span)) = p.eat_argument_name() {
                    labels.push(gossamer_ast::NamedArg {
                        index: args.len(),
                        name,
                        span,
                    });
                } else if allow_labels
                    && matches!(p.peek().kind, TokenKind::Ident)
                    && matches!(p.peek_nth(1).kind, TokenKind::Punct(Punct::Eq))
                {
                    // `=` at a call site reads as assignment in a language
                    // where assignment is an expression; a label binds with
                    // `:`, as every other keyed form does.
                    let span = p.peek_span();
                    let name = p.slice(span).to_string();
                    p.bump();
                    // The rewrite covers the separator alone, so applying it
                    // leaves the label and the value exactly as written.
                    let separator = p.peek_span();
                    p.record(
                        crate::ParseError::ArgumentLabelSeparator { name: name.clone() },
                        separator,
                    );
                    p.bump();
                    labels.push(gossamer_ast::NamedArg {
                        index: args.len(),
                        name: Ident::new(name),
                        span,
                    });
                }
                let amp = p.peek_span();
                // Dropping this `&` joins the argument to the one before
                // it when the two are separated by a newline and this one
                // opens with `(`: `f(\n  &a\n  &(b)\n)` becomes
                // `f(a(b))`, a different call with a different arity.
                let joins_previous = !newline_separated.is_empty()
                    && *newline_separated.last().unwrap_or(&false)
                    && matches!(p.peek().kind, TokenKind::Punct(Punct::Amp))
                    && matches!(p.peek_nth(1).kind, TokenKind::Punct(Punct::LParen));
                let arg = p.parse_expr_no_assign();
                args.push(p.strip_shared_argument_reference(arg, amp, joins_previous));
                let comma = p.at_punct(Punct::Comma);
                if !p.eat_list_separator() {
                    break;
                }
                newline_separated.push(!comma);
            }
            if !p.expect_punct(Punct::RParen, "to close the argument list") {
                p.recover_to_close(Punct::LParen, Punct::RParen);
            }
            (args, labels)
        })
    }

    /// Drops the `&` of a shared reference written on a call argument.
    /// No parameter is a shared reference any more, so the sigil names no
    /// choice: the argument is passed without copying either way and the
    /// callee cannot write through it. `amp` is the span of the `&`, so
    /// the fix deletes exactly that.
    /// `joins_previous` says that deleting the `&` would let this
    /// argument bind as a call on the one before it, so the diagnostic
    /// carries no automatic fix: `--fix` would otherwise rewrite the
    /// program into a different one that still compiles.
    pub(crate) fn strip_shared_argument_reference(
        &mut self,
        arg: Expr,
        amp: Span,
        joins_previous: bool,
    ) -> Expr {
        let ExprKind::Unary {
            op: gossamer_ast::UnaryOp::RefShared,
            operand,
        } = arg.kind
        else {
            return arg;
        };
        if !self.in_synthesized_item() {
            self.record(
                if joins_previous {
                    ParseError::SharedReferenceArgumentJoins
                } else {
                    ParseError::SharedReferenceArgument
                },
                amp,
            );
        }
        *operand
    }

    /// Drops the `&` of a shared reference written on a binary operand,
    /// which names no choice for the same reason it names none on an
    /// argument: an operand is read without copying whatever its type,
    /// and no operator writes through one.
    fn strip_shared_operand_reference(&mut self, operand: Expr, amp: Span) -> Expr {
        let ExprKind::Unary {
            op: gossamer_ast::UnaryOp::RefShared,
            operand: inner,
        } = operand.kind
        else {
            return operand;
        };
        if !self.in_synthesized_item() {
            self.record(ParseError::SharedReferenceArgument, amp);
        }
        *inner
    }

    /// Consumes a `name =` argument label when one starts here. `==` is a
    /// single token, so an equality test in argument position cannot be
    /// mistaken for a label.
    ///
    /// A path segment is separated by `::`, which lexes as its own
    /// token, so a bare identifier followed by `:` is only ever a label.
    /// Consumes `name:` at an argument position.
    ///
    /// `name: value` is what every other keyed form in the language writes -
    /// a struct field, a map entry, a `cohort` header setting - so it is what
    /// an argument label writes too.
    fn eat_argument_name(&mut self) -> Option<(Ident, gossamer_lex::Span)> {
        if !matches!(self.peek().kind, TokenKind::Ident)
            || !matches!(self.peek_nth(1).kind, TokenKind::Punct(Punct::Colon))
        {
            return None;
        }
        let span = self.peek_span();
        let name = Ident::new(self.slice(span));
        self.bump();
        self.bump();
        Some((name, span))
    }

    /// The keyword-introduced block forms in expression position:
    /// `unsafe`, `comptime`, and the contextual `cohort`. The last is
    /// an expression rather than a statement, as `arena` is, because its
    /// `Result` can be bound or propagated with `?`.
    /// `true` at the head of a `select { .. }` expression. `select` is
    /// contextual, never reserved, so a binding or method of that name
    /// keeps working; only `select` directly before `{` opens the block.
    pub(crate) fn at_select_block(&self) -> bool {
        self.at_contextual_word("select")
            && matches!(
                self.peek_nth(1).kind,
                gossamer_lex::TokenKind::Punct(Punct::LBrace)
            )
            && !self.struct_literal_forbidden()
    }

    /// `true` at the head of a `comptime { .. }` block, the expression
    /// form of the contextual word.
    pub(crate) fn at_comptime_block(&self) -> bool {
        self.at_contextual_word("comptime")
            && matches!(
                self.peek_nth(1).kind,
                gossamer_lex::TokenKind::Punct(Punct::LBrace)
            )
            && !self.struct_literal_forbidden()
    }

    fn parse_contextual_block(&mut self) -> Option<ExprKind> {
        // `unsafe` grants nothing: there is no operation the language refuses
        // outside it, so the keyword marked a boundary that does not exist.
        // It stays reserved, and the block it opened is an ordinary one.
        if self.at_keyword(Keyword::Unsafe) {
            let span = self.peek_span();
            self.bump();
            self.record(ParseError::UnsafeGrantsNothing, span);
            self.expect_punct(Punct::LBrace, "to open the block");
            return Some(ExprKind::Unsafe(self.parse_block_body()));
        }
        if self.at_comptime_block() {
            self.bump();
            self.expect_punct(Punct::LBrace, "to open `comptime` block");
            let mut block = self.parse_block_body();
            block.kind = gossamer_ast::BlockKind::Comptime;
            return Some(ExprKind::Block(block));
        }
        if self.at_cohort_block() {
            return Some(self.parse_cohort_expr());
        }
        None
    }

    fn parse_primary(&mut self) -> Expr {
        let start_span = self.peek_span();
        let kind = self.parse_primary_kind();
        let end_span = self.last_span();
        let span = self.join(start_span, end_span);
        let id = self.alloc_id();
        Expr::new(id, span, kind)
    }

    fn parse_primary_kind(&mut self) -> ExprKind {
        // The flag reaches the closure that is the step itself and nothing
        // nested inside one: a closure written as a call argument is an
        // ordinary closure whose body runs to the end of the expression.
        let pipe_step = std::mem::replace(&mut self.parsing_pipe_step, false);
        if self.eat_punct(Punct::LParen) {
            return self.parse_paren_or_tuple();
        }
        if self.eat_punct(Punct::LBracket) {
            return self.parse_array_expr();
        }
        if self.at_punct(Punct::Lt) && self.peek_nth(1).kind == TokenKind::Punct(Punct::LBracket) {
            return self.reject_removed_collection_literal("<[..]", "Queue");
        }
        if self.at_punct(Punct::Caret) && self.peek_nth(1).kind == TokenKind::Punct(Punct::LBracket)
        {
            return self.reject_removed_collection_literal("^[..]", "MaxHeap");
        }
        if self.eat_punct(Punct::Hash) {
            return self.parse_hash_prefixed_literal();
        }
        if self.eat_punct(Punct::LBrace) {
            if self.prefer_empty_brace_block() && self.at_punct(Punct::RBrace) {
                return ExprKind::Block(self.parse_block_body());
            }
            if let Some(map) = self.try_parse_map_literal() {
                return map;
            }
            return ExprKind::Block(self.parse_block_body());
        }
        if let Some(literal) = self.try_parse_literal() {
            return ExprKind::Literal(literal);
        }
        if self.at_keyword(Keyword::If) {
            return self.parse_if_expr();
        }
        if self.at_keyword(Keyword::Match) {
            return self.parse_match_expr();
        }
        if self.at_keyword(Keyword::Loop) {
            return self.parse_loop_expr(None);
        }
        if self.at_keyword(Keyword::While) {
            return self.parse_while_expr(None);
        }
        if self.at_keyword(Keyword::For) {
            return self.parse_for_expr(None);
        }
        if self.at_keyword(Keyword::Return) {
            self.bump();
            if !is_expression_start(self) || at_block_end(self) {
                return ExprKind::Return(None);
            }
            let value = self.parse_expr_no_assign();
            return ExprKind::Return(Some(Box::new(value)));
        }
        if self.at_keyword(Keyword::Break) {
            return self.parse_break_expr();
        }
        if self.at_keyword(Keyword::Continue) {
            return self.parse_continue_expr();
        }
        if self.at_select_block() {
            return self.parse_select_expr();
        }
        if let Some(kind) = self.parse_contextual_block() {
            return kind;
        }
        if self.at_punct(Punct::Pipe) || self.at_punct(Punct::PipePipe) {
            return self.parse_closure_expr(pipe_step);
        }
        if self.at_keyword(Keyword::Fn) {
            return self.parse_fn_closure_expr();
        }
        if self.at_label_start() {
            return self.parse_labelled_loop();
        }
        if self.at_punct(Punct::Dollar) {
            self.record(ParseError::PipePlaceholderRetired, self.peek_span());
            self.bump();
            return ExprKind::Error;
        }
        if self.is_path_expr_start() {
            return self.parse_path_expr_or_struct();
        }
        self.record(
            ParseError::unexpected("an expression", self.peek_text()),
            self.peek_span(),
        );
        self.bump();
        ExprKind::Error
    }

    fn parse_hash_prefixed_literal(&mut self) -> ExprKind {
        if self.eat_punct(Punct::LBracket) {
            return self.parse_fixed_array_expr();
        }
        if self.eat_punct(Punct::LBrace) {
            return self.parse_set_literal_expr();
        }
        self.record(
            ParseError::unexpected(
                "`[` for a `Vec` literal or `{` for a `Set` literal after `#`",
                self.peek_text(),
            ),
            self.peek_span(),
        );
        ExprKind::Error
    }

    /// Parses `{}` or `{key: value, ...}` as a hash map literal. HIR lowering keeps every
    /// existing backend on the `HashMap::from([(key, value), ...])` constructor
    /// path while giving source code a dedicated map syntax.
    /// If the contents are an ordinary block, the token stream and diagnostics
    /// are restored and the caller parses a block normally.
    fn try_parse_map_literal(&mut self) -> Option<ExprKind> {
        if self.eat_punct(Punct::RBrace) {
            return Some(ExprKind::MapLiteral(Vec::new()));
        }
        if !self.first_brace_entry_has_top_level_colon() {
            return None;
        }
        let checkpoint = self.tokens.checkpoint();
        let diagnostic_count = self.diagnostics.len();
        let first_key = self.with_struct_literals_allowed(Self::parse_expr_no_assign);
        if !self.eat_punct(Punct::Colon) {
            self.tokens.rewind(checkpoint);
            self.diagnostics.truncate(diagnostic_count);
            return None;
        }

        let mut entries = Vec::new();
        let mut key = first_key;
        loop {
            if self.at_eof() {
                break;
            }
            let value = self.with_struct_literals_allowed(Self::parse_expr_no_assign);
            let span = self.join(key.span, value.span);
            let id = self.alloc_id();
            entries.push(Expr::new(id, span, ExprKind::Tuple(vec![key, value])));
            if self.at_punct(Punct::RBrace) {
                break;
            }
            if self.at_eof() {
                break;
            }
            if !self.eat_list_separator() {
                self.expect_punct(Punct::Comma, "between map entries");
                break;
            }
            if self.at_punct(Punct::RBrace) {
                break;
            }
            key = self.with_struct_literals_allowed(Self::parse_expr_no_assign);
            if !self.expect_punct(Punct::Colon, "between a map key and value") {
                break;
            }
        }
        self.expect_punct(Punct::RBrace, "to close map literal");
        Some(ExprKind::MapLiteral(entries))
    }

    /// Checks the first brace-delimited entry for a map separator without
    /// recursively parsing it. This keeps ordinary nested blocks linear:
    /// speculatively parsing each nested block as a map and then rewinding
    /// made malformed runs of `{` take exponential time.
    fn first_brace_entry_has_top_level_colon(&self) -> bool {
        let mut parens = 0usize;
        let mut brackets = 0usize;
        let mut braces = 0usize;
        let mut offset = 0usize;
        loop {
            let token = self.peek_nth(offset);
            match token.kind {
                TokenKind::Eof => return false,
                TokenKind::Punct(Punct::LParen) => parens += 1,
                TokenKind::Punct(Punct::RParen) => parens = parens.saturating_sub(1),
                TokenKind::Punct(Punct::LBracket) => brackets += 1,
                TokenKind::Punct(Punct::RBracket) => brackets = brackets.saturating_sub(1),
                TokenKind::Punct(Punct::LBrace) => braces += 1,
                TokenKind::Punct(Punct::RBrace) if braces == 0 => return false,
                TokenKind::Punct(Punct::RBrace) => braces -= 1,
                TokenKind::Punct(Punct::Colon) if parens == 0 && brackets == 0 && braces == 0 => {
                    return true;
                }
                TokenKind::Punct(Punct::Comma | Punct::Semi)
                    if parens == 0 && brackets == 0 && braces == 0 =>
                {
                    return false;
                }
                _ => {}
            }
            offset += 1;
        }
    }

    fn parse_paren_or_tuple(&mut self) -> ExprKind {
        self.with_struct_literals_allowed(|p| {
            p.in_paren_group = true;
            if p.eat_punct(Punct::RParen) {
                return ExprKind::Literal(Literal::Unit);
            }
            let first = p.parse_expr();
            if p.eat_punct(Punct::RParen) {
                return first.kind;
            }
            let mut elements = vec![first];
            while p.eat_list_separator() {
                if p.at_punct(Punct::RParen) {
                    break;
                }
                elements.push(p.parse_expr());
            }
            p.expect_punct(Punct::RParen, "to close tuple expression");
            ExprKind::Tuple(elements)
        })
    }

    fn parse_array_expr(&mut self) -> ExprKind {
        self.with_struct_literals_allowed(Self::parse_array_expr_inner)
    }

    fn parse_fixed_array_expr(&mut self) -> ExprKind {
        self.with_struct_literals_allowed(Self::parse_fixed_array_expr_inner)
    }

    fn parse_set_literal_expr(&mut self) -> ExprKind {
        self.with_struct_literals_allowed(Self::parse_set_literal_expr_inner)
    }

    /// Records [`ParseError::RemovedCollectionLiteral`] for a bracket
    /// spelling that no longer names a container, then consumes the
    /// bracketed group so the rest of the statement still parses and one
    /// diagnostic stands for the whole literal.
    fn reject_removed_collection_literal(&mut self, spelling: &str, container: &str) -> ExprKind {
        let start = self.peek_span();
        self.bump();
        self.bump();
        let mut depth = 1u32;
        while depth > 0 && !self.at_eof() {
            if self.at_punct(Punct::LBracket) {
                depth += 1;
            } else if self.at_punct(Punct::RBracket) {
                depth -= 1;
            }
            self.bump();
        }
        let span = self.join(start, self.last_span());
        self.record(
            ParseError::RemovedCollectionLiteral {
                spelling: spelling.to_string(),
                container: container.to_string(),
            },
            span,
        );
        ExprKind::Error
    }

    /// `[a, b]` / `[value; count]` - the fixed-array literal.
    fn parse_array_expr_inner(&mut self) -> ExprKind {
        if self.eat_punct(Punct::RBracket) {
            return ExprKind::FixedArray(ArrayExpr::List(Vec::new()));
        }
        let first = self.parse_expr_no_assign();
        if self.eat_punct(Punct::Semi) {
            let count = self.parse_expr_no_assign();
            self.expect_punct(Punct::RBracket, "to close fixed array expression");
            return ExprKind::FixedArray(ArrayExpr::Repeat {
                value: Box::new(first),
                count: Box::new(count),
            });
        }
        let mut elements = vec![first];
        while self.eat_list_separator() {
            if self.at_punct(Punct::RBracket) {
                break;
            }
            elements.push(self.parse_expr_no_assign());
        }
        self.expect_punct(Punct::RBracket, "to close fixed array expression");
        ExprKind::FixedArray(ArrayExpr::List(elements))
    }

    /// `#[a, b]` - the Vec literal, and `#[value; count]` - the Vec of
    /// `count` copies of `value`. The bracket spelling picks the container:
    /// `[value; count]` builds the fixed array of the same shape.
    fn parse_fixed_array_expr_inner(&mut self) -> ExprKind {
        if self.eat_punct(Punct::RBracket) {
            return ExprKind::Array(ArrayExpr::List(Vec::new()));
        }
        let first = self.parse_expr_no_assign();
        if self.eat_punct(Punct::Semi) {
            let count = self.parse_expr_no_assign();
            self.expect_punct(Punct::RBracket, "to close Vec expression");
            return ExprKind::Array(ArrayExpr::Repeat {
                value: Box::new(first),
                count: Box::new(count),
            });
        }
        let mut elements = vec![first];
        while self.eat_list_separator() {
            if self.at_punct(Punct::RBracket) {
                break;
            }
            elements.push(self.parse_expr_no_assign());
        }
        self.expect_punct(Punct::RBracket, "to close Vec expression");
        ExprKind::Array(ArrayExpr::List(elements))
    }

    fn parse_set_literal_expr_inner(&mut self) -> ExprKind {
        if self.eat_punct(Punct::RBrace) {
            return ExprKind::SetLiteral(Vec::new());
        }
        let mut elements = vec![self.parse_expr_no_assign()];
        while self.eat_list_separator() {
            if self.at_punct(Punct::RBrace) {
                break;
            }
            elements.push(self.parse_expr_no_assign());
        }
        self.expect_punct(Punct::RBrace, "to close the `Set` literal");
        ExprKind::SetLiteral(elements)
    }

    fn parse_if_expr(&mut self) -> ExprKind {
        self.bump();
        self.enter_no_struct();
        let condition = self.parse_condition();
        self.leave_no_struct();
        self.expect_punct(Punct::LBrace, "to open `if` branch");
        let then_block_span_start = self.last_span();
        let then_block = self.parse_block_body();
        let then_span = self.join(then_block_span_start, self.last_span());
        let then_expr = Expr::new(self.alloc_id(), then_span, ExprKind::Block(then_block));
        let else_branch = if self.eat_keyword(Keyword::Else) {
            if self.at_keyword(Keyword::If) {
                let start = self.peek_span();
                let kind = self.parse_if_expr();
                let end = self.last_span();
                let span = self.join(start, end);
                let id = self.alloc_id();
                Some(Box::new(Expr::new(id, span, kind)))
            } else {
                self.expect_punct(Punct::LBrace, "to open `else` branch");
                let start = self.last_span();
                let block = self.parse_block_body();
                let span = self.join(start, self.last_span());
                let id = self.alloc_id();
                Some(Box::new(Expr::new(id, span, ExprKind::Block(block))))
            }
        } else {
            None
        };
        match condition {
            Condition::Plain(condition) => ExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(then_expr),
                else_branch,
            },
            Condition::Chain(clauses) => self.desugar_if_chain(clauses, then_expr, else_branch),
        }
    }

    fn bare_method_turbofish_arg_start(&self) -> bool {
        matches!(
            self.peek_nth(1).kind,
            TokenKind::Ident
                | TokenKind::Keyword(
                    Keyword::SelfUpper
                        | Keyword::SelfLower
                        | Keyword::Super
                        | Keyword::Crate
                        | Keyword::Fn
                )
                | TokenKind::Punct(Punct::LParen | Punct::LBracket | Punct::Amp | Punct::Bang)
        )
    }

    fn bare_method_turbofish_call_ahead(&self) -> bool {
        if !self.at_punct(Punct::Lt) || !self.bare_method_turbofish_arg_start() {
            return false;
        }
        let mut depth = 0_i32;
        for offset in 0..128 {
            match self.peek_nth(offset).kind {
                TokenKind::Punct(Punct::Lt) => depth += 1,
                TokenKind::Punct(Punct::Gt) => {
                    depth -= 1;
                    if depth == 0 {
                        return self.peek_nth(offset + 1).kind == TokenKind::Punct(Punct::LParen);
                    }
                    if depth < 0 {
                        return false;
                    }
                }
                TokenKind::Punct(Punct::ShiftR) => {
                    depth -= 2;
                    if depth == 0 {
                        return self.peek_nth(offset + 1).kind == TokenKind::Punct(Punct::LParen);
                    }
                    if depth < 0 {
                        return false;
                    }
                }
                TokenKind::Punct(
                    Punct::GtEq
                    | Punct::ShiftREq
                    | Punct::Dot
                    | Punct::DotDot
                    | Punct::DotDotEq
                    | Punct::LBrace
                    | Punct::RBrace
                    | Punct::Semi,
                )
                | TokenKind::Eof => return false,
                _ => {}
            }
        }
        false
    }

    /// Parses an `if`/`while` condition, recognising `let`-chains.
    ///
    /// Each clause is parsed at [`COND_CLAUSE_PREC`] so it stops at the
    /// next `&&`. When no clause binds a `let`, the `&&` chain is folded
    /// back into a single boolean expression and full-precedence parsing
    /// resumes - so plain `&&`/`||` conditions are unchanged.
    fn parse_condition(&mut self) -> Condition {
        let mut clauses = vec![self.parse_cond_clause()];
        while self.at_punct(Punct::AmpAmp) {
            self.bump();
            clauses.push(self.parse_cond_clause());
        }
        let has_let = clauses
            .iter()
            .any(|clause| matches!(clause, CondClause::Let { .. }));
        if has_let {
            if self.peek_binary_op().is_some() {
                self.record(
                    ParseError::unexpected_help(
                        "`&&`",
                        self.peek_text(),
                        "a `let` clause in a condition chains only with `&&`",
                    ),
                    self.peek_span(),
                );
                // Consume the trailing operand so the branch-opening `{`
                // is still found and the parse does not cascade.
                let stub = Expr::new(self.alloc_id(), self.peek_span(), ExprKind::Error);
                let _ = self.continue_binary(stub, PREC_BELOW_ASSIGN, false);
            }
            return Condition::Chain(clauses);
        }
        let mut iter = clauses.into_iter();
        let Some(CondClause::Bool(mut expr)) = iter.next() else {
            unreachable!("a condition without a `let` clause is all boolean");
        };
        for clause in iter {
            let CondClause::Bool(rhs) = clause else {
                unreachable!("a condition without a `let` clause is all boolean");
            };
            let span = self.join(expr.span, rhs.span);
            expr = Expr::new(
                self.alloc_id(),
                span,
                ExprKind::Binary {
                    op: BinaryOp::And,
                    lhs: Box::new(expr),
                    rhs: Box::new(rhs),
                },
            );
        }
        Condition::Plain(self.continue_binary(expr, PREC_BELOW_ASSIGN, false))
    }

    /// Parses one clause of a condition chain: either `let PAT = EXPR`
    /// or a boolean expression, stopping at the next `&&`.
    fn parse_cond_clause(&mut self) -> CondClause {
        if self.at_keyword(Keyword::Let) {
            self.bump();
            let pattern = self.parse_pattern();
            self.expect_punct(Punct::Eq, "after `let` pattern in condition");
            let scrutinee = self.parse_expr_with_prec(COND_CLAUSE_PREC, false);
            CondClause::Let { pattern, scrutinee }
        } else {
            CondClause::Bool(self.parse_expr_with_prec(COND_CLAUSE_PREC, false))
        }
    }

    /// Lowers an `if` let-chain to nested `match`/`if` so every tier
    /// sees only constructs it already handles. The chain succeeds only
    /// when every `let` matches and every boolean is `true`; any failure
    /// runs the `else` branch (an empty block when none was written).
    fn desugar_if_chain(
        &mut self,
        clauses: Vec<CondClause>,
        then_expr: Expr,
        else_branch: Option<Box<Expr>>,
    ) -> ExprKind {
        let else_template = else_branch.map(|boxed| *boxed);
        let mut acc = then_expr;
        for clause in clauses.into_iter().rev() {
            acc = match clause {
                CondClause::Bool(cond) => {
                    let fail = self.make_chain_else(else_template.as_ref());
                    self.make_if_clause(cond, acc, fail)
                }
                CondClause::Let { pattern, scrutinee } if is_irrefutable_binding(&pattern) => {
                    self.make_block_let(pattern, scrutinee, acc)
                }
                CondClause::Let { pattern, scrutinee } => {
                    let fail = self.make_chain_else(else_template.as_ref());
                    self.make_match_clause(pattern, scrutinee, acc, fail)
                }
            };
        }
        acc.kind
    }

    /// `if cond { acc } else { fail }`.
    fn make_if_clause(&mut self, cond: Expr, acc: Expr, fail: Expr) -> Expr {
        let span = self.join(cond.span, acc.span);
        let then_block = self.wrap_in_block(acc);
        let else_block = self.wrap_in_block(fail);
        Expr::new(
            self.alloc_id(),
            span,
            ExprKind::If {
                condition: Box::new(cond),
                then_branch: Box::new(then_block),
                else_branch: Some(Box::new(else_block)),
            },
        )
    }

    /// `match scrutinee { pattern => acc, _ => fail }` for a refutable
    /// `let` clause.
    fn make_match_clause(
        &mut self,
        pattern: Pattern,
        scrutinee: Expr,
        acc: Expr,
        fail: Expr,
    ) -> Expr {
        let span = self.join(scrutinee.span, acc.span);
        let wildcard = Pattern::new(self.alloc_id(), fail.span, PatternKind::Wildcard);
        Expr::new(
            self.alloc_id(),
            span,
            ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![
                    MatchArm {
                        pattern,
                        guard: None,
                        body: acc,
                    },
                    MatchArm {
                        pattern: wildcard,
                        guard: None,
                        body: fail,
                    },
                ],
            },
        )
    }

    /// `{ let pattern = scrutinee; acc }` for an irrefutable `let`
    /// clause. The binding always succeeds, so the chain has no failure
    /// edge here and the bound value types through ordinary `let`
    /// inference.
    fn make_block_let(&mut self, pattern: Pattern, scrutinee: Expr, acc: Expr) -> Expr {
        let span = self.join(scrutinee.span, acc.span);
        let stmt_span = self.join(pattern.span, scrutinee.span);
        let let_stmt = Stmt::new(
            self.alloc_id(),
            stmt_span,
            StmtKind::Let {
                pattern,
                ty: None,
                init: Some(Box::new(scrutinee)),
            },
        );
        let block = Block {
            stmts: vec![let_stmt],
            tail: Some(Box::new(acc)),
            synthetic: true,
            kind: gossamer_ast::BlockKind::Plain,
        };
        Expr::new(self.alloc_id(), span, ExprKind::Block(block))
    }

    /// Builds a fresh `else`-branch expression for one failure site of a
    /// chain. A written `else` is deep-cloned with fresh node ids so each
    /// site is an independent subtree; a missing `else` yields unit.
    fn make_chain_else(&mut self, template: Option<&Expr>) -> Expr {
        if let Some(expr) = template {
            self.clone_with_fresh_ids(expr)
        } else {
            let span = self.last_span();
            Expr::new(self.alloc_id(), span, ExprKind::Block(Block::empty()))
        }
    }

    /// Builds an unlabelled `break` for a `while` chain's failure edge.
    /// It targets the synthetic loop that replaces the `while`, so a
    /// failed clause exits the loop.
    fn make_loop_break(&mut self, span: Span) -> Expr {
        Expr::new(
            self.alloc_id(),
            span,
            ExprKind::Break {
                label: None,
                value: None,
            },
        )
    }

    /// Wraps an expression in a synthetic block unless it already is one,
    /// so `if`-branch slots always hold a block.
    fn wrap_in_block(&mut self, expr: Expr) -> Expr {
        if matches!(expr.kind, ExprKind::Block(_)) {
            return expr;
        }
        let span = expr.span;
        let block = Block {
            stmts: Vec::new(),
            tail: Some(Box::new(expr)),
            synthetic: true,
            kind: gossamer_ast::BlockKind::Plain,
        };
        Expr::new(self.alloc_id(), span, ExprKind::Block(block))
    }

    /// Deep-clones an expression subtree, assigning every node a fresh
    /// id so the copy is independent for resolution and type-checking.
    fn clone_with_fresh_ids(&mut self, expr: &Expr) -> Expr {
        let mut cloned = expr.clone();
        let mut reassign = ReassignIds { ids: &mut self.ids };
        reassign.visit_expr(&mut cloned);
        cloned
    }

    fn parse_match_expr(&mut self) -> ExprKind {
        self.bump();
        self.enter_no_struct();
        let scrutinee = self.parse_expr_no_assign();
        self.leave_no_struct();
        self.expect_punct(Punct::LBrace, "to open `match` body");
        let mut arms = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            let pattern = self.parse_pattern();
            let guard = if self.eat_keyword(Keyword::If) {
                Some(self.parse_expr_no_assign())
            } else {
                None
            };
            if !self.eat_punct(Punct::FatArrow) {
                self.record(
                    ParseError::MatchArmMissingArrow {
                        found: self.peek_text(),
                    },
                    self.peek_span(),
                );
            }
            let body = if self.at_punct(Punct::Comma) || self.at_punct(Punct::RBrace) {
                self.record(ParseError::MatchArmMissingBody, self.peek_span());
                Expr::new(self.alloc_id(), self.peek_span(), ExprKind::Error)
            } else {
                self.enter_match_arm_body();
                let body = self.with_empty_braces_as_blocks(Self::parse_expr);
                self.leave_match_arm_body();
                body
            };
            let body_is_block = matches!(body.kind, ExprKind::Block(_));
            arms.push(gossamer_ast::MatchArm {
                pattern,
                guard,
                body,
            });
            // F#-style line boundaries can separate expression arms. Rust's
            // block-arm rule also applies, and commas remain accepted.
            let ate_comma = self.eat_punct(Punct::Comma);
            if !ate_comma
                && !body_is_block
                && !self.at_punct(Punct::RBrace)
                && !self.newline_before_peek()
            {
                self.record(ParseError::MatchArmMissingSeparator, self.peek_span());
            }
        }
        self.expect_punct(Punct::RBrace, "to close `match` body");
        ExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms,
        }
    }

    fn parse_loop_expr(&mut self, label: Option<Label>) -> ExprKind {
        self.bump();
        self.expect_punct(Punct::LBrace, "to open loop body");
        let body = self.parse_block_body();
        let span = self.last_span();
        let id = self.alloc_id();
        let body_expr = Expr::new(id, span, ExprKind::Block(body));
        ExprKind::Loop {
            label,
            body: Box::new(body_expr),
        }
    }

    fn parse_while_expr(&mut self, label: Option<Label>) -> ExprKind {
        self.bump();
        self.enter_no_struct();
        let condition = self.parse_condition();
        self.leave_no_struct();
        self.expect_punct(Punct::LBrace, "to open `while` body");
        let body_start = self.last_span();
        let body_block = self.parse_block_body();
        let body_span = self.join(body_start, self.last_span());
        let body_expr = Expr::new(self.alloc_id(), body_span, ExprKind::Block(body_block));
        match condition {
            Condition::Plain(condition) => ExprKind::While {
                label,
                condition: Box::new(condition),
                body: Box::new(body_expr),
            },
            Condition::Chain(clauses) => self.desugar_while_chain(label, clauses, body_expr),
        }
    }

    /// Lowers a `while` let-chain to `loop { match/if ... }`. On success
    /// the body runs and the loop iterates; any failed clause `break`s
    /// out of the loop. The body's own `break`/`continue` target this
    /// loop, matching ordinary `while` semantics.
    fn desugar_while_chain(
        &mut self,
        label: Option<Label>,
        clauses: Vec<CondClause>,
        body_expr: Expr,
    ) -> ExprKind {
        let body_span = body_expr.span;
        let mut acc = body_expr;
        for clause in clauses.into_iter().rev() {
            acc = match clause {
                CondClause::Bool(cond) => {
                    let fail = self.make_loop_break(body_span);
                    self.make_if_clause(cond, acc, fail)
                }
                CondClause::Let { pattern, scrutinee } if is_irrefutable_binding(&pattern) => {
                    self.make_block_let(pattern, scrutinee, acc)
                }
                CondClause::Let { pattern, scrutinee } => {
                    let fail = self.make_loop_break(body_span);
                    self.make_match_clause(pattern, scrutinee, acc, fail)
                }
            };
        }
        let loop_body_block = Block {
            stmts: Vec::new(),
            tail: Some(Box::new(acc)),
            synthetic: true,
            kind: gossamer_ast::BlockKind::Plain,
        };
        let loop_body = Expr::new(self.alloc_id(), body_span, ExprKind::Block(loop_body_block));
        ExprKind::Loop {
            label,
            body: Box::new(loop_body),
        }
    }

    fn parse_for_expr(&mut self, label: Option<Label>) -> ExprKind {
        self.bump();
        let pattern = self.parse_pattern();
        self.expect_keyword(Keyword::In, "after `for` pattern");
        self.enter_no_struct();
        let iter = self.parse_expr_no_assign();
        self.leave_no_struct();
        self.expect_punct(Punct::LBrace, "to open `for` body");
        let body = self.parse_block_body();
        let span = self.last_span();
        let id = self.alloc_id();
        let body_expr = Expr::new(id, span, ExprKind::Block(body));
        ExprKind::For {
            label,
            pattern,
            iter: Box::new(iter),
            body: Box::new(body_expr),
        }
    }

    fn parse_break_expr(&mut self) -> ExprKind {
        self.bump();
        let label = self.try_parse_label();
        let value =
            if !self.newline_before_peek() && is_expression_start(self) && !at_block_end(self) {
                Some(Box::new(self.parse_expr_no_assign()))
            } else {
                None
            };
        ExprKind::Break { label, value }
    }

    fn parse_continue_expr(&mut self) -> ExprKind {
        self.bump();
        let label = self.try_parse_label();
        ExprKind::Continue { label }
    }

    fn try_parse_label(&mut self) -> Option<Label> {
        if !self.at_label_start() {
            return None;
        }
        let token = self.bump();
        Some(Label::new(label_name(self.slice(token.span))))
    }

    fn at_label_start(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Label)
    }

    fn parse_labelled_loop(&mut self) -> ExprKind {
        let token = self.bump();
        let label = Some(Label::new(label_name(self.slice(token.span))));
        self.expect_punct(Punct::Colon, "after loop label");
        if self.at_keyword(Keyword::Loop) {
            return self.parse_loop_expr(label);
        }
        if self.at_keyword(Keyword::While) {
            return self.parse_while_expr(label);
        }
        if self.at_keyword(Keyword::For) {
            return self.parse_for_expr(label);
        }
        self.record(
            ParseError::unexpected(
                "`loop`, `while`, or `for` after the label",
                self.peek_text(),
            ),
            self.peek_span(),
        );
        ExprKind::Error
    }

    fn parse_closure_expr(&mut self, pipe_step: bool) -> ExprKind {
        let params = if self.eat_punct(Punct::PipePipe) {
            Vec::new()
        } else {
            self.bump();
            let mut list = Vec::new();
            while !self.at_punct(Punct::Pipe) && !self.at_eof() {
                let pattern = self.parse_pattern_no_or();
                let ty = if self.eat_punct(Punct::Colon) {
                    let amp = self.peek_span();
                    let ty = self.parse_type();
                    Some(self.strip_shared_parameter_reference(ty, amp))
                } else {
                    None
                };
                list.push(ClosureParam { pattern, ty });
                if !self.eat_list_separator() {
                    break;
                }
            }
            self.expect_punct(Punct::Pipe, "to close closure parameters");
            list
        };
        let ret = if self.eat_punct(Punct::Arrow) {
            Some(self.parse_type())
        } else {
            None
        };
        // A closure written directly as a `|>` step ends at the next step:
        // the operator is left-associative, so `x |> |v| f(v) |> g` reads as
        // `g(f(x))` rather than folding the rest of the chain into the body.
        let body = self.with_empty_braces_as_blocks(|p| {
            if pipe_step {
                p.parse_expr_with_prec(BinaryOp::PipeGt.precedence(), true)
            } else {
                p.parse_expr()
            }
        });
        ExprKind::Closure {
            params,
            ret,
            body: Box::new(body),
        }
    }

    fn parse_fn_closure_expr(&mut self) -> ExprKind {
        self.bump();
        self.expect_punct(
            Punct::LParen,
            "to open the `fn(..)` closure literal's parameters",
        );
        let mut params = Vec::new();
        while !self.at_punct(Punct::RParen) && !self.at_eof() {
            let pattern = self.parse_pattern_no_or();
            let ty = if self.eat_punct(Punct::Colon) {
                let amp = self.peek_span();
                let ty = self.parse_type();
                Some(self.strip_shared_parameter_reference(ty, amp))
            } else {
                None
            };
            params.push(ClosureParam { pattern, ty });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(
            Punct::RParen,
            "to close the `fn(..)` closure literal's parameters",
        );
        let ret = if self.eat_punct(Punct::Arrow) {
            Some(self.parse_type())
        } else {
            None
        };
        self.expect_punct(Punct::LBrace, "to open the `fn(..)` closure literal's body");
        let block = self.parse_block_body();
        let span = self.last_span();
        let id = self.alloc_id();
        let body = Expr::new(id, span, ExprKind::Block(block));
        ExprKind::Closure {
            params,
            ret,
            body: Box::new(body),
        }
    }

    fn parse_select_expr(&mut self) -> ExprKind {
        self.bump();
        self.expect_punct(Punct::LBrace, "to open `select` body");
        let mut arms = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            let checkpoint = self.tokens.checkpoint();
            if self.eat_keyword(Keyword::Else) || self.at_ident_text("default") {
                if self.at_ident_text("default") {
                    self.bump();
                }
                self.expect_punct(Punct::FatArrow, "after `default`");
                let body = self.parse_expr();
                arms.push(gossamer_ast::SelectArm {
                    op: gossamer_ast::SelectOp::Default,
                    body,
                });
            } else {
                let pattern = self.parse_pattern();
                if self.eat_punct(Punct::Eq) {
                    let raw = self.parse_expr_no_assign();
                    let channel = strip_recv_call(raw);
                    self.expect_punct(
                        Punct::FatArrow,
                        "after a select receive arm, as in `x = rx.recv() => ...`",
                    );
                    let body = self.parse_expr();
                    arms.push(gossamer_ast::SelectArm {
                        op: gossamer_ast::SelectOp::Recv { pattern, channel },
                        body,
                    });
                } else {
                    // Not `pattern = chan.recv()`: try a send arm
                    // `chan.send(value) => body`.
                    self.tokens.rewind(checkpoint);
                    let raw = self.parse_expr_no_assign();
                    if let Some((channel, value)) = strip_send_call(raw) {
                        self.expect_punct(
                            Punct::FatArrow,
                            "after a select send arm, as in `tx.send(v) => ...`",
                        );
                        let body = self.parse_expr();
                        arms.push(gossamer_ast::SelectArm {
                            op: gossamer_ast::SelectOp::Send { channel, value },
                            body,
                        });
                    } else if self.eat_punct(Punct::FatArrow) {
                        // Unrecognised arm head; consume its body to keep
                        // forward progress instead of desyncing the parser.
                        let _ = self.parse_expr();
                    } else {
                        self.bump();
                    }
                }
            }
            // A comma separates two arms on one line; a newline separates
            // them across lines, the way every other delimited list in the
            // language reads. Only a run that is neither ends the body.
            if !self.eat_punct(Punct::Comma) && !self.at_punct(Punct::RBrace) && !self.at_eof() {
                continue;
            }
            if self.at_punct(Punct::RBrace) || self.at_eof() {
                break;
            }
        }
        self.expect_punct(Punct::RBrace, "to close `select` body");
        ExprKind::Select(arms)
    }

    fn at_ident_text(&self, text: &str) -> bool {
        matches!(self.peek().kind, TokenKind::Ident) && self.slice(self.peek_span()) == text
    }

    fn is_path_expr_start(&self) -> bool {
        matches!(
            self.peek().kind,
            TokenKind::Ident
                | TokenKind::Keyword(
                    Keyword::SelfUpper | Keyword::SelfLower | Keyword::Super | Keyword::Crate
                )
        )
    }

    fn parse_path_expr_or_struct(&mut self) -> ExprKind {
        let path_start = self.peek_span();
        let path = self.parse_path_expr();
        // A compiler-known name is recognised by the name and the `(`, the
        // way any other call is. The set is closed and known at parse time,
        // so there is nothing a sigil would disambiguate.
        if self.at_punct(Punct::LParen)
            && let Some(name) = single_segment_name(&path)
            && is_builtin_call_name(name)
        {
            let name = name.to_string();
            return self.parse_builtin_call(&name);
        }
        // `regex::compile("…")` and `sql::statement("…")` check a literal
        // argument while the program is parsed, so a malformed pattern or
        // statement is reported at the literal rather than reaching a run.
        if self.at_punct(Punct::LParen)
            && let Some(validated) = validated_module_call(&path)
        {
            let callee_span = Span::new(path_start.file, path_start.start, self.last_span().end);
            self.bump();
            let call_span = self.peek_span();
            let args = self.parse_call_args();
            let literal = args.first().and_then(literal_string);
            match validated {
                ValidatedCall::SqlStatement => {
                    // There is no run-time function behind the call: it is
                    // the statement it checked, and a statement built at run
                    // time is an ordinary `String`.
                    let Some(statement) = literal else {
                        self.record(ParseError::ValidatedCallNeedsLiteral, call_span);
                        return ExprKind::Error;
                    };
                    if let Some(reason) = sql_statement_error(&statement) {
                        self.record(ParseError::InvalidSqlStatement { reason }, args[0].span);
                    }
                    return ExprKind::Literal(Literal::String(statement));
                }
                ValidatedCall::RegexCompile => {
                    if let Some(pattern) = literal
                        && let Err(err) = regex::Regex::new(&pattern)
                    {
                        let reason = regex_error_reason(&err);
                        self.record(ParseError::InvalidRegexLiteral { reason }, args[0].span);
                    }
                    let id = self.alloc_id();
                    return ExprKind::Call {
                        callee: Box::new(Expr::new(id, callee_span, ExprKind::Path(path))),
                        args,
                    };
                }
            }
        }
        if self.at_punct(Punct::Bang) {
            return self.parse_macro_tail(path);
        }
        if self.at_punct(Punct::LBrace)
            && !self.struct_literal_forbidden()
            && self.can_begin_struct_literal()
        {
            return self.parse_struct_literal_tail(path);
        }
        ExprKind::Path(path)
    }

    fn can_begin_struct_literal(&self) -> bool {
        let mut depth = 1usize;
        let mut offset = 1usize;
        loop {
            let token = self.peek_nth(offset);
            match token.kind {
                TokenKind::Eof => return false,
                TokenKind::Punct(Punct::LBrace) => depth += 1,
                TokenKind::Punct(Punct::RBrace) => {
                    depth -= 1;
                    if depth == 0 {
                        return true;
                    }
                }
                TokenKind::Ident if depth == 1 => return true,
                TokenKind::Punct(Punct::DotDot) if depth == 1 => return true,
                TokenKind::IntLit
                | TokenKind::FloatLit
                | TokenKind::StringLit
                | TokenKind::RawStringLit { .. }
                | TokenKind::TripleStringLit
                | TokenKind::CharLit
                | TokenKind::ByteLit
                | TokenKind::ByteStringLit
                | TokenKind::RawByteStringLit { .. }
                | TokenKind::Keyword(_)
                    if depth == 1 =>
                {
                    return true;
                }
                TokenKind::Punct(Punct::LParen | Punct::LBracket | Punct::Minus | Punct::Bang)
                    if depth == 1 =>
                {
                    return true;
                }
                _ => {}
            }
            offset += 1;
            if offset > 64 {
                return false;
            }
        }
    }

    fn parse_struct_literal_tail(&mut self, path: PathExpr) -> ExprKind {
        self.bump();
        self.with_struct_literals_allowed(|p| p.parse_struct_literal_fields(path))
    }

    fn parse_struct_literal_fields(&mut self, path: PathExpr) -> ExprKind {
        let mut fields = Vec::new();
        let mut base = None;
        let mut positional_index = 0usize;
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            // `..base` functional update may appear anywhere in the field
            // list (`{ ..base, x: 1 }` or `{ x: 1, ..base }`); explicit
            // fields override the base's value for the same name. Only one
            // spread is allowed.
            let spread_span = self.peek_span();
            if self.eat_punct(Punct::DotDot) {
                let expr = self.parse_expr_no_assign();
                if base.is_some() {
                    self.record(ParseError::StructLiteralExtraSpread, spread_span);
                } else {
                    base = Some(Box::new(expr));
                }
                if !self.eat_list_separator() {
                    break;
                }
                continue;
            }
            let name = if matches!(self.peek().kind, TokenKind::Ident)
                && matches!(self.peek_nth(1).kind, TokenKind::Punct(Punct::Colon))
            {
                let name_span = self.peek_span();
                let name = Ident::new(self.slice(name_span));
                self.bump();
                self.expect_punct(Punct::Colon, "after field name");
                name
            } else {
                let name = Ident::new(positional_index.to_string());
                positional_index += 1;
                let value = self.parse_expr_no_assign();
                fields.push(StructExprField {
                    name,
                    value: Some(value),
                });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
                continue;
            };
            let value = Some(self.parse_expr_no_assign());
            fields.push(StructExprField { name, value });
            if !self.eat_list_separator() {
                break;
            }
        }
        self.expect_punct(Punct::RBrace, "to close struct literal");
        ExprKind::Struct {
            path,
            fields,
            base,
            syntax: gossamer_ast::expr::StructExprSyntax::Braced,
        }
    }

    /// Gossamer exposes a deliberately narrow macro surface: only
    /// `format!` / `println!` / `print!` / `eprintln!` / `eprint!` /
    /// `panic!`. Each is **expanded at parse time** to a plain call
    /// on the matching variadic builtin - there is no runtime macro
    /// engine, no custom macros, no procedural macros. The
    /// expansion shape is a single `Call` whose args are the
    /// alternating literal / interpolated segments, so the whole
    /// format builds in one pass inside the builtin rather than
    /// chained `+` allocations.
    ///
    /// Unrecognised `name!(...)` invocations land as a parse
    /// diagnostic steering users to the plain-function form.
    fn parse_macro_tail(&mut self, path: PathExpr) -> ExprKind {
        let bang_span = self.peek_span();
        self.bump();
        let macro_name = path
            .segments
            .last()
            .map_or("?", |s| s.name.name.as_str())
            .to_string();
        let paren = self.at_punct(Punct::LParen);

        // A compiler-known name is an ordinary call now. The bang reports
        // with the rewrite that drops it, and the call still parses so the
        // rest of the file is diagnosed on its own terms.
        if paren && is_builtin_call_name(&macro_name) {
            self.record(
                ParseError::MacroSigilRetired {
                    name: macro_name.clone(),
                },
                bang_span,
            );
            return self.parse_builtin_call(&macro_name);
        }

        // `regex!("…")` / `sql!("…")` moved into their modules:
        // `regex::compile("…")` and `sql::statement("…")` validate a literal
        // argument at build time and read as the ordinary calls they are.
        if paren && matches!(macro_name.as_str(), "regex" | "sql") {
            self.record(
                ParseError::ValidatingMacroMoved {
                    name: macro_name.clone(),
                },
                bang_span,
            );
            if !self.expect_punct(
                Punct::LParen,
                &format!("to open the arguments of `{macro_name}`"),
            ) {
                return ExprKind::Error;
            }
            let args = self.parse_call_args();
            let validator = format!("__gos_{macro_name}_validate");
            return self.alloc_function_call(&validator, args);
        }

        // Gossamer has no `vec!`: `#[...]` is the Vec literal and a plain
        // `[...]` is the fixed-array form. Steer the common Rust habit to the
        // growable spelling rather than to the misleading "drop the `!`" form.
        let expected = if macro_name == "vec" {
            "a Vec literal `#[...]` - Gossamer has no `vec!`; `#[...]` creates a `Vec<T>` \
             and `[...]` creates a fixed `[T; N]` array"
                .to_string()
        } else {
            format!("`{macro_name}(...)` - Gossamer has no user-defined macros, drop the `!`")
        };
        self.record(
            ParseError::unexpected(expected, "`!`".to_string()),
            bang_span,
        );
        let (open_punct, close_punct) = if self.at_punct(Punct::LBracket) {
            (Punct::LBracket, Punct::RBracket)
        } else if self.at_punct(Punct::LBrace) {
            (Punct::LBrace, Punct::RBrace)
        } else {
            (Punct::LParen, Punct::RParen)
        };
        if self.eat_punct(open_punct) {
            let _tokens = self.collect_delimited_tokens(open_punct, close_punct);
            self.eat_punct(close_punct);
        }
        ExprKind::Error
    }

    /// Parses a compiler-known call whose name was recognised at the `(`.
    ///
    /// Every one of these parses its arguments exactly as an ordinary call
    /// does; `matches` is the sole exception, and its rule keys on the name,
    /// which is in hand before the `(` is consumed.
    fn parse_builtin_call(&mut self, name: &str) -> ExprKind {
        if is_desugar_macro(name) {
            return self.expand_builtin_macro(name);
        }
        if !self.expect_punct(Punct::LParen, &format!("to open the arguments of `{name}`")) {
            return ExprKind::Error;
        }
        let args = self.parse_call_args();
        if name == "codegen" {
            return self.alloc_function_call("__gos_codegen", args);
        }
        self.expand_format_macro(name, args)
    }

    /// Expands the parser-level desugar macros into ordinary AST. None of
    /// these introduce a new node kind, so they lower uniformly on every
    /// tier: `matches!(expr, pat)` -> a two-arm boolean `match`; `todo!` /
    /// `unimplemented!` / `unreachable!` -> `panic!` with a fixed (or
    /// supplied) message; `dbg!(expr)` -> see `expand_dbg_macro`. The caller
    /// has matched the name and the `(` delimiter; the paren is unconsumed.
    fn expand_builtin_macro(&mut self, macro_name: &str) -> ExprKind {
        if macro_name == "matches" {
            if !self.expect_punct(
                Punct::LParen,
                &format!("to open the arguments of `{macro_name}`"),
            ) {
                return ExprKind::Error;
            }
            let scrutinee = self.with_struct_literals_allowed(Self::parse_expr_no_assign);
            // A newline separates the scrutinee from the pattern wherever a
            // comma could, so the multiline form the formatter writes parses
            // as the one-line form does.
            if !self.eat_punct(Punct::Comma) && !self.newline_before_peek() {
                let found = self.peek_text();
                self.record(
                    ParseError::unexpected("`,` after the `matches` scrutinee".to_string(), found),
                    self.peek_span(),
                );
            }
            let pattern = self.parse_pattern();
            self.expect_punct(
                Punct::RParen,
                &format!("to close the arguments of `{macro_name}!`"),
            );
            let yes = self.alloc_literal_expr(Literal::Bool(true));
            let no = self.alloc_literal_expr(Literal::Bool(false));
            return self.make_match_clause(pattern, scrutinee, yes, no).kind;
        }
        if macro_name == "dbg" {
            return self.expand_dbg_macro();
        }
        if !self.expect_punct(
            Punct::LParen,
            &format!("to open the arguments of `{macro_name}`"),
        ) {
            return ExprKind::Error;
        }
        let args = self.parse_call_args();
        let args = if args.is_empty() {
            let message = match macro_name {
                "todo" => "not yet implemented",
                "unimplemented" => "not implemented",
                _ => "internal error: entered unreachable code",
            };
            vec![self.alloc_literal_expr(Literal::String(message.to_string()))]
        } else {
            args
        };
        self.expand_format_macro("panic", args)
    }

    /// Expands `dbg!(expr)` to `{ let __dbg = expr; eprintln!("{:?}", __dbg);
    /// __dbg }`, so the value is printed for inspection yet flows on
    /// unchanged. Any non-one arity degrades to a bare `eprintln!("")`.
    fn expand_dbg_macro(&mut self) -> ExprKind {
        if !self.expect_punct(Punct::LParen, "to open the arguments of `dbg!`") {
            return ExprKind::Error;
        }
        let mut args = self.parse_call_args();
        if args.len() != 1 {
            let empty = self.alloc_literal_expr(Literal::String(String::new()));
            return self.expand_format_macro("eprintln", vec![empty]);
        }
        let value = args.pop().expect("dbg! has exactly one argument here");
        let value_span = value.span;
        let binding = Pattern::new(
            self.alloc_id(),
            value_span,
            PatternKind::Ident {
                mutability: Mutability::Immutable,
                name: Ident::new("__dbg"),
                subpattern: None,
            },
        );
        let let_stmt = Stmt::new(
            self.alloc_id(),
            value_span,
            StmtKind::Let {
                pattern: binding,
                ty: None,
                init: Some(Box::new(value)),
            },
        );
        let fmt = self.alloc_literal_expr(Literal::String("{:?}".to_string()));
        let printed = self.alloc_path_expr("__dbg");
        let eprintln_kind = self.expand_format_macro("eprintln", vec![fmt, printed]);
        let eprintln_expr = Expr::new(self.alloc_id(), value_span, eprintln_kind);
        let print_stmt = Stmt::new(
            self.alloc_id(),
            value_span,
            StmtKind::Expr {
                expr: Box::new(eprintln_expr),
                has_semi: true,
            },
        );
        let tail = self.alloc_path_expr("__dbg");
        let block = Block {
            stmts: vec![let_stmt, print_stmt],
            tail: Some(Box::new(tail)),
            synthetic: true,
            kind: gossamer_ast::BlockKind::Plain,
        };
        ExprKind::Block(block)
    }

    /// Compile-time expansion for the six recognised format-shaped
    /// macros. Splits the leading format-string literal into
    /// alternating `Literal` / `Named` / `Positional` segments and
    /// emits one call to the internal zero-separator concat
    /// builtin. For `format!` the concat *is* the result; for
    /// `println!` / `print!` / `eprintln!` / `eprint!` / `panic!`
    /// the concat is passed as the single argument to the outer
    /// function - so the whole format builds in one allocation
    /// inside `__concat` rather than chained `+` calls.
    ///
    /// Rust-style format macros require a literal first argument so their
    /// positional placeholders can be checked during parsing.
    fn expand_format_macro(&mut self, macro_name: &str, args: Vec<Expr>) -> ExprKind {
        let (first, rest) = match args.split_first() {
            Some((first, rest)) => (first.clone(), rest.to_vec()),
            None => {
                return self.alloc_function_call(macro_name, Vec::new());
            }
        };
        let Some(template) = literal_string(&first) else {
            // The first argument is the template when it is a string
            // literal. A lone argument that is not one renders, which is the
            // `"{}"` template written out - and a value is never parsed as a
            // template, which is the property the literal-only rule exists
            // to protect.
            if rest.is_empty() {
                let rendered = self.alloc_function_call_expr("__concat", vec![first]);
                return self.alloc_function_call(macro_name, vec![rendered]);
            }
            let mut all = vec![first];
            all.extend(rest);
            self.record(ParseError::FormatStringMustBeLiteral, all[0].span);
            return self.alloc_function_call(macro_name, all);
        };
        let segments = parse_format_template(&template);
        let expected = segments
            .iter()
            .filter(|segment| {
                matches!(
                    segment,
                    FormatSegment::Positional
                        | FormatSegment::PositionalPrec(_)
                        | FormatSegment::PositionalSpec(_)
                )
            })
            .count();
        // A malformed placeholder has no arity of its own, so its diagnostic
        // stands alone rather than beside a count that miscounts it.
        let has_invalid = segments.iter().any(|segment| {
            matches!(
                segment,
                FormatSegment::Invalid(_) | FormatSegment::Indexed(_)
            )
        });
        if expected != rest.len() && !has_invalid {
            self.record(
                ParseError::FormatArgumentCount {
                    expected,
                    found: rest.len(),
                },
                first.span,
            );
        }
        let mut positional_iter = rest.into_iter();
        let mut concat_args = self.format_segment_args(segments, &mut positional_iter, &first);
        // Retain surplus arguments in the recovery AST. The format-arity
        // diagnostic above already rejects them, but keeping an explicit `_`
        // lets pipe substitution recognize it and avoids a misleading second
        // diagnostic claiming that the pipe had no placeholder.
        concat_args.extend(positional_iter);
        let concat_call = self.alloc_function_call_expr("__concat", concat_args);
        if macro_name == "format" {
            return concat_call.kind;
        }
        self.alloc_function_call(macro_name, vec![concat_call])
    }

    /// The `__concat` arguments a parsed template stands for: each literal
    /// piece, each named capture, and each positional placeholder filled from
    /// `positional_iter` in order, rendered through its spec.
    fn format_segment_args(
        &mut self,
        segments: Vec<FormatSegment>,
        positional_iter: &mut std::vec::IntoIter<Expr>,
        first: &Expr,
    ) -> Vec<Expr> {
        let mut concat_args: Vec<Expr> = Vec::new();
        for segment in segments {
            match segment {
                FormatSegment::Invalid(text) => {
                    self.record(
                        ParseError::MalformedFormatPlaceholder { text: text.clone() },
                        first.span,
                    );
                    concat_args
                        .push(self.alloc_literal_expr(Literal::String(format!("{{{text}}}"))));
                }
                FormatSegment::Indexed(text) => {
                    self.record(
                        ParseError::FormatArgumentIndex { text: text.clone() },
                        first.span,
                    );
                    concat_args
                        .push(self.alloc_literal_expr(Literal::String(format!("{{{text}}}"))));
                }
                FormatSegment::Literal(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    concat_args.push(self.alloc_literal_expr(Literal::String(text)));
                }
                FormatSegment::Named(name) => {
                    let expr = self.alloc_named_capture_expr(&name);
                    concat_args.push(expr);
                }
                FormatSegment::Positional => {
                    if let Some(expr) = positional_iter.next() {
                        concat_args.push(expr);
                    }
                }
                FormatSegment::PositionalPrec(prec) => {
                    if let Some(expr) = positional_iter.next() {
                        let prec_lit = self.alloc_literal_expr(Literal::Int(prec.to_string()));
                        concat_args.push(
                            self.alloc_function_call_expr("__fmt_prec", vec![expr, prec_lit]),
                        );
                    }
                }
                FormatSegment::NamedPrec(name, prec) => {
                    let arg = self.alloc_named_capture_expr(&name);
                    let prec_lit = self.alloc_literal_expr(Literal::Int(prec.to_string()));
                    concat_args
                        .push(self.alloc_function_call_expr("__fmt_prec", vec![arg, prec_lit]));
                }
                FormatSegment::PositionalSpec(spec) => {
                    if let Some(expr) = positional_iter.next() {
                        let e = self.build_format_spec_expr(expr, &spec);
                        concat_args.push(e);
                    }
                }
                FormatSegment::NamedSpec(name, spec) => {
                    let arg = self.alloc_named_capture_expr(&name);
                    let e = self.build_format_spec_expr(arg, &spec);
                    concat_args.push(e);
                }
            }
        }
        concat_args
    }

    fn alloc_function_call_expr(&mut self, name: &str, args: Vec<Expr>) -> Expr {
        let id = self.alloc_id();
        let span = self.last_span();
        Expr::new(id, span, self.alloc_function_call(name, args))
    }

    /// Expands a `{:spec}` argument into a composition of already-wired
    /// stdlib calls: render the value (`__concat` for Display,
    /// `strconv::format_i64_radix` for `{:x}`/`{:b}`/`{:o}`, `__fmt_prec` for
    /// `{:.N}`), then pad to width via `strings::pad_left` / `pad_right` /
    /// `center`. No tier-specific lowering is needed.
    fn build_format_spec_expr(&mut self, value: Expr, spec: &FormatSpec) -> Expr {
        let rendered = if let Some(base) = spec.radix {
            let base_lit = self.alloc_literal_expr(Literal::Int(base.to_string()));
            let r = self.alloc_function_call_expr("__fmt_radix", vec![value, base_lit]);
            if spec.upper {
                self.alloc_function_call_expr("__fmt_upper", vec![r])
            } else {
                r
            }
        } else if let Some(prec) = spec.precision {
            let prec_lit = self.alloc_literal_expr(Literal::Int(prec.to_string()));
            self.alloc_function_call_expr("__fmt_prec", vec![value, prec_lit])
        } else if spec.debug {
            self.alloc_function_call_expr("__debug", vec![value])
        } else {
            self.alloc_function_call_expr("__concat", vec![value])
        };
        let rendered = if spec.alternate {
            let prefix = match (spec.radix, spec.upper) {
                (Some(16), true) => "0X",
                (Some(16), false) => "0x",
                (Some(2), _) => "0b",
                (Some(8), _) => "0o",
                _ => "",
            };
            if prefix.is_empty() {
                rendered
            } else {
                let prefix = self.alloc_literal_expr(Literal::String(prefix.to_string()));
                self.alloc_function_call_expr("__concat", vec![prefix, rendered])
            }
        } else {
            rendered
        };
        if spec.width == 0 {
            return rendered;
        }
        let width_lit = self.alloc_literal_expr(Literal::Int(spec.width.to_string()));
        let fill_lit = self.alloc_literal_expr(Literal::Int((spec.fill as u32).to_string()));
        // The alignment a spec asks for, plus the zero flag, as HIR lowering
        // reads them: it resolves both against the padded value's type before
        // the runtime sees a concrete alignment.
        let align_code = match spec.align {
            Align::Default => PAD_REQUEST_DEFAULT,
            Align::Left => PAD_REQUEST_LEFT,
            Align::Center => PAD_REQUEST_CENTER,
            Align::Right => PAD_REQUEST_RIGHT,
        } | if spec.zero_pad {
            PAD_REQUEST_ZERO_FLAG
        } else {
            0
        };
        let align_lit = self.alloc_literal_expr(Literal::Int(align_code.to_string()));
        self.alloc_function_call_expr("__fmt_pad", vec![rendered, width_lit, fill_lit, align_lit])
    }

    fn alloc_function_call(&mut self, name: &str, args: Vec<Expr>) -> ExprKind {
        let callee = self.alloc_path_expr(name);
        ExprKind::Call {
            callee: Box::new(callee),
            args,
        }
    }

    fn alloc_literal_expr(&mut self, lit: Literal) -> Expr {
        let id = self.alloc_id();
        let span = self.last_span();
        Expr::new(id, span, ExprKind::Literal(lit))
    }

    fn alloc_path_expr(&mut self, name: &str) -> Expr {
        let id = self.alloc_id();
        let span = self.last_span();
        Expr::new(id, span, ExprKind::Path(PathExpr::single(name.to_string())))
    }

    /// Expression for a named format capture: a bare `{ident}` is a
    /// path, a dotted `{a.balance}` / `{t.0}` folds field / tuple-index
    /// accesses over the leading binding.
    fn alloc_named_capture_expr(&mut self, name: &str) -> Expr {
        let mut parts = name.split('.');
        let head = parts.next().unwrap_or(name);
        let mut expr = self.alloc_path_expr(head);
        for part in parts {
            let selector = match part.parse::<u32>() {
                Ok(index) => FieldSelector::Index(index),
                Err(_) => FieldSelector::Named(Ident::new(part.to_string())),
            };
            let id = self.alloc_id();
            let span = self.last_span();
            expr = Expr::new(
                id,
                span,
                ExprKind::FieldAccess {
                    receiver: Box::new(expr),
                    field: selector,
                },
            );
        }
        expr
    }

    fn collect_delimited_tokens(&mut self, open: Punct, close: Punct) -> String {
        let mut depth = 1u32;
        let mut output = String::new();
        while !self.at_eof() {
            let token = self.peek();
            match token.kind {
                TokenKind::Punct(found) if found == open => {
                    depth += 1;
                    output.push_str(self.slice(token.span));
                    output.push(' ');
                    self.bump();
                }
                TokenKind::Punct(found) if found == close => {
                    depth -= 1;
                    if depth == 0 {
                        return output.trim_end().to_string();
                    }
                    output.push_str(self.slice(token.span));
                    output.push(' ');
                    self.bump();
                }
                _ => {
                    output.push_str(self.slice(token.span));
                    output.push(' ');
                    self.bump();
                }
            }
        }
        output.trim_end().to_string()
    }

    /// Parses a `PathExpr`: one or more `::`-separated segments, each of
    /// which may carry a turbofish `::<...>` list of generic arguments.
    pub(crate) fn parse_path_expr(&mut self) -> PathExpr {
        let first = self.parse_path_expr_segment();
        let mut segments = vec![first];
        while self.at_punct(Punct::ColonColon) {
            let checkpoint = self.tokens.checkpoint();
            self.bump();
            if !self.is_path_expr_start() && !self.at_punct(Punct::Lt) {
                self.tokens.rewind(checkpoint);
                break;
            }
            if self.at_punct(Punct::Lt) {
                let mut tail = segments.pop().expect("at least one path segment");
                self.bump();
                tail.generics = self.parse_generic_args_in_turbofish();
                segments.push(tail);
                continue;
            }
            segments.push(self.parse_path_expr_segment());
        }
        PathExpr { segments }
    }

    fn parse_path_expr_segment(&mut self) -> PathSegment {
        let token = self.peek();
        let name = match token.kind {
            TokenKind::Ident
            | TokenKind::Keyword(
                Keyword::SelfUpper | Keyword::SelfLower | Keyword::Super | Keyword::Crate,
            ) => {
                self.bump();
                keyword_or_ident_text(self.slice(token.span))
            }
            _ => {
                self.record(
                    ParseError::unexpected("a name after `::`", self.peek_text()),
                    token.span,
                );
                gossamer_ast::ERROR_IDENT.to_string()
            }
        };
        PathSegment::new(name)
    }

    /// Parses a block body (`{ ... }`) after the opening brace has been consumed.
    pub(crate) fn parse_block_body(&mut self) -> Block {
        self.with_struct_literals_allowed(Self::parse_block_body_inner)
    }

    fn parse_block_body_inner(&mut self) -> Block {
        let mut stmts = Vec::new();
        let mut tail: Option<Box<Expr>> = None;
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            let before = self.tokens.checkpoint();
            let stmt = self.parse_stmt();
            if self.tokens.checkpoint() == before {
                self.bump();
                continue;
            }
            let is_tail = matches!(
                &stmt.kind,
                gossamer_ast::StmtKind::Expr {
                    has_semi: false,
                    ..
                }
            ) && self.at_punct(Punct::RBrace);
            if is_tail {
                let gossamer_ast::StmtKind::Expr { expr, .. } = stmt.kind else {
                    unreachable!("is_tail only accepts expression statements");
                };
                tail = Some(expr);
                break;
            }
            stmts.push(stmt);
        }
        self.expect_punct(Punct::RBrace, "to close block");
        Block {
            stmts,
            tail,
            synthetic: false,
            kind: gossamer_ast::BlockKind::Plain,
        }
    }

    fn try_parse_literal(&mut self) -> Option<Literal> {
        let token = self.peek();
        match token.kind {
            TokenKind::IntLit => {
                self.bump();
                Some(Literal::Int(self.slice(token.span).to_string()))
            }
            TokenKind::FloatLit => {
                self.bump();
                Some(Literal::Float(self.slice(token.span).to_string()))
            }
            TokenKind::StringLit => {
                self.bump();
                Some(Literal::String(string_literal_value(
                    self.slice(token.span),
                )))
            }
            TokenKind::TripleStringLit => {
                self.bump();
                let text = self.slice(token.span);
                if gossamer_lex::triple_string(text).opening_line_text {
                    self.record(ParseError::TripleStringOpeningLine, token.span);
                }
                Some(Literal::String(string_literal_value(text)))
            }
            TokenKind::RawStringLit { hashes } => {
                self.bump();
                let body = self.slice(token.span);
                Some(Literal::RawString {
                    hashes,
                    value: extract_raw_string_body(body, hashes),
                })
            }
            TokenKind::CharLit => {
                self.bump();
                Some(Literal::Char(char_literal_value(self.slice(token.span))))
            }
            TokenKind::ByteLit => {
                self.bump();
                Some(Literal::Byte(byte_literal_value(self.slice(token.span))))
            }
            TokenKind::ByteStringLit => {
                self.bump();
                Some(Literal::ByteString(byte_string_literal_value(
                    self.slice(token.span),
                )))
            }
            TokenKind::RawByteStringLit { hashes } => {
                self.bump();
                let body = self.slice(token.span);
                Some(Literal::RawByteString {
                    hashes,
                    value: extract_raw_string_body(body, hashes).into_bytes(),
                })
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump();
                Some(Literal::Bool(true))
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump();
                Some(Literal::Bool(false))
            }
            _ => None,
        }
    }
}

/// Reassigns a fresh node id to every node in a cloned subtree.
///
/// Desugaring a let-chain `else` branch duplicates the written block at
/// each failure site; each copy must own a distinct set of node ids so
/// resolution and type-checking treat the copies independently.
struct ReassignIds<'a> {
    ids: &'a mut NodeIdGenerator,
}

impl VisitorMut for ReassignIds<'_> {
    fn visit_expr(&mut self, expr: &mut Expr) {
        expr.id = self.ids.next();
        walk_expr_mut(self, expr);
    }

    fn visit_pattern(&mut self, pattern: &mut Pattern) {
        pattern.id = self.ids.next();
        walk_pattern_mut(self, pattern);
    }

    fn visit_stmt(&mut self, stmt: &mut Stmt) {
        stmt.id = self.ids.next();
        walk_stmt_mut(self, stmt);
    }

    fn visit_type(&mut self, ty: &mut Type) {
        ty.id = self.ids.next();
        walk_type_mut(self, ty);
    }

    fn visit_item(&mut self, item: &mut Item) {
        item.id = self.ids.next();
        walk_item_mut(self, item);
    }
}

/// Returns `true` for patterns that always match, so a `let` clause
/// using them binds unconditionally and lowers to a plain binding with
/// no failure edge.
fn is_irrefutable_binding(pattern: &Pattern) -> bool {
    match &pattern.kind {
        PatternKind::Wildcard => true,
        PatternKind::Ident { subpattern, .. } => subpattern
            .as_ref()
            .is_none_or(|sub| is_irrefutable_binding(sub)),
        PatternKind::Tuple(elems) => elems.iter().all(is_irrefutable_binding),
        PatternKind::Ref { inner, .. } => is_irrefutable_binding(inner),
        _ => false,
    }
}

fn is_non_associative_compare(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    )
}

fn keyword_or_ident_text(text: &str) -> String {
    text.to_string()
}

/// Whether a binary operator's punctuation also begins an expression in
/// Gossamer. These are the only ops for which a leading newline must be
/// treated as a statement boundary, so that `let x = expr\n&y` parses as two
/// statements (`let x = expr;` followed by `&y`) rather than the binary
/// `expr & y`, and a closure `|x| x * k` on the line after a statement is a
/// closure rather than `expr | x | x * k`. The other unary prefix `!` has no
/// binary form so does not need this guard; `||` stays a continuation, since
/// a leading `||` continues a condition far more often than it opens a
/// parameterless closure.
fn is_unary_startable(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Sub | BinaryOp::BitAnd | BinaryOp::Mul | BinaryOp::BitOr
    )
}

fn extract_raw_string_body(source: &str, hashes: u8) -> String {
    let prefix_len = 1 + usize::from(hashes) + 1;
    let suffix_len = 1 + usize::from(hashes);
    if source.len() < prefix_len + suffix_len {
        return String::new();
    }
    // Unterminated raw strings can leave the trailing suffix
    // offset inside a multi-byte codepoint when the lexer
    // synthesised the token's span to end of input. Walk the end
    // back to the closest char boundary so the slice is well-typed
    // - Rust's panic on bad boundaries would otherwise tear the
    // parser down on adversarial input.
    let mut end = source.len() - suffix_len;
    while end > prefix_len && !source.is_char_boundary(end) {
        end -= 1;
    }
    if prefix_len >= end {
        return String::new();
    }
    source[prefix_len..end].to_string()
}

/// Returns `true` when the next token could begin an expression.
pub(crate) fn is_expression_start(parser: &Parser<'_>) -> bool {
    let token = parser.peek();
    match token.kind {
        TokenKind::Ident
        | TokenKind::IntLit
        | TokenKind::FloatLit
        | TokenKind::StringLit
        | TokenKind::RawStringLit { .. }
        | TokenKind::TripleStringLit
        | TokenKind::CharLit
        | TokenKind::ByteLit
        | TokenKind::ByteStringLit
        | TokenKind::RawByteStringLit { .. }
        | TokenKind::Label => true,
        TokenKind::Keyword(keyword) => matches!(
            keyword,
            Keyword::True
                | Keyword::False
                | Keyword::If
                | Keyword::Match
                | Keyword::Loop
                | Keyword::While
                | Keyword::For
                | Keyword::Unsafe
                | Keyword::Return
                | Keyword::Break
                | Keyword::Continue
                | Keyword::Fn
                | Keyword::SelfLower
                | Keyword::SelfUpper
                | Keyword::Super
                | Keyword::Crate
                | Keyword::Mut
        ),
        TokenKind::Punct(punct) => matches!(
            punct,
            Punct::LParen
                | Punct::LBracket
                | Punct::LBrace
                | Punct::Minus
                | Punct::Bang
                | Punct::Amp
                | Punct::Star
                | Punct::Hash
                | Punct::Lt
                | Punct::Caret
                | Punct::Pipe
                | Punct::PipePipe
                | Punct::DotDot
                | Punct::DotDotEq
        ),
        _ => false,
    }
}

/// Returns `true` when the parser is looking at a block terminator.
pub(crate) fn at_block_end(parser: &Parser<'_>) -> bool {
    parser.at_punct(Punct::RBrace)
        || parser.at_punct(Punct::Semi)
        || parser.at_punct(Punct::Comma)
        || parser.at_punct(Punct::RParen)
        || parser.at_punct(Punct::RBracket)
        || parser.at_eof()
}

/// One parsed segment of a format-string template.
/// True when a pipe step was built from an expression the parser already
/// rejected, either along its receiver chain or in one of its arguments.
fn step_contains_parse_error(expr: &Expr) -> bool {
    let own = match &expr.kind {
        ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => {
            args.iter().any(|arg| matches!(arg.kind, ExprKind::Error))
        }
        ExprKind::Error => true,
        _ => false,
    };
    own || match &expr.kind {
        ExprKind::Call { callee, .. } => step_contains_parse_error(callee),
        ExprKind::MethodCall { receiver, .. } | ExprKind::FieldAccess { receiver, .. } => {
            step_contains_parse_error(receiver)
        }
        ExprKind::Index { base, .. } => step_contains_parse_error(base),
        _ => false,
    }
}

/// True when a pipe step is a call that already writes every argument, so the
/// piped value has no slot of its own.
fn pipe_step_has_arguments(expr: &Expr) -> bool {
    let own = match &expr.kind {
        ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => !args.is_empty(),
        _ => false,
    };
    own || match &expr.kind {
        ExprKind::MethodCall { receiver, .. } | ExprKind::FieldAccess { receiver, .. } => {
            pipe_step_has_arguments(receiver)
        }
        ExprKind::Index { base, .. } => pipe_step_has_arguments(base),
        _ => false,
    }
}

/// Parameter name for a `|>` step rewritten as a closure: the first of a
/// short list that the step's own text does not already use, so the rewrite
/// cannot capture a name the call already names.
fn closure_parameter_name(step: &str) -> Option<&'static str> {
    ["v", "value", "piped"]
        .into_iter()
        .find(|name| !mentions_identifier(step, name))
}

/// Whether `text` contains `name` as a whole identifier.
fn mentions_identifier(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(index, _)| {
        let before = text[..index].chars().next_back();
        let after = text[index + name.len()..].chars().next();
        !before.is_some_and(is_identifier_char) && !after.is_some_and(is_identifier_char)
    })
}

fn is_identifier_char(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

/// True if `path` is a bare `_`, which used to head the removed min-heap
/// literal spelling.
fn is_underscore_path(path: &PathExpr) -> bool {
    path.segments.len() == 1
        && path.segments[0].name.name == "_"
        && path.segments[0].generics.is_empty()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Default,
    Left,
    Center,
    Right,
}

/// A parsed format spec covering width / alignment / fill / zero-pad /
/// alternate radix prefix / precision / radix - the subset of Rust's `{:spec}` grammar Gossamer
/// expands by composing `__concat`, `strconv::format_i64_radix`, and the
/// `strings` padding helpers (all already wired on every tier).
// Each flag is one bit of the `{:spec}` grammar, and the grammar is what
// decides how many there are; a bit-set here would name the same four
// things behind an index.
#[allow(clippy::struct_excessive_bools, reason = "one field per spec flag")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct FormatSpec {
    fill: char,
    align: Align,
    width: usize,
    precision: Option<usize>,
    /// Integer radix (`x`/`X` => 16, `b` => 2, `o` => 8); `None` is decimal.
    radix: Option<u32>,
    /// `{:X}` - uppercase the radix digits.
    upper: bool,
    /// `{:#x}` / `{:#b}` / `{:#o}` - include the radix prefix.
    alternate: bool,
    /// `{:?}` - render through the Debug channel rather than Display.
    debug: bool,
    /// `{:08}` - the zero flag. Kept apart from `fill`, because it means
    /// "sign-aware zero pad" on a number and nothing at all on any other
    /// value, a distinction an explicit `{:0>8}` fill does not carry.
    zero_pad: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FormatSegment {
    /// Plain text written into the output verbatim.
    Literal(String),
    /// `{ident}` - expands to a path expression that resolves
    /// `ident` from the enclosing scope.
    Named(String),
    /// `{}` - consumed in order from the macro's trailing args.
    Positional,
    /// `{:.N}` - consumed in order, formatted with N fractional
    /// digits (replaces the hand-rolled `fmt9` helper used by
    /// the benchmark ports).
    PositionalPrec(usize),
    /// `{ident:.N}` - same as `Positional` but with precision.
    NamedPrec(String, usize),
    /// `{:spec}` - a positional argument with a width/align/fill/radix spec.
    PositionalSpec(FormatSpec),
    /// `{ident:spec}` - same as `PositionalSpec` but a named binding.
    NamedSpec(String, FormatSpec),
    /// `{age + 1}` - a placeholder whose name part is an expression, not a
    /// binding. Carries the inner text for the diagnostic; the caller
    /// records a `MalformedFormatPlaceholder` error.
    Invalid(String),
    /// `{0}` / `{1:>3}` - a placeholder naming an argument by position,
    /// which templates do not take. Carries the inner text for the
    /// diagnostic; the caller records a `FormatArgumentIndex` error.
    Indexed(String),
}

/// Whether a placeholder's name part (the text before any `:`) looks like an
/// expression rather than a binding name or numeric index. Used to reject
/// `{age + 1}`-style placeholders that the macros silently passed through as
/// literal text. Empty name parts (`{:spec}`) and bare identifiers/numbers are
/// not flagged.
fn format_name_looks_like_expr(inner: &str) -> bool {
    let name = match format_spec_colon(inner) {
        Some(idx) => &inner[..idx],
        None => inner,
    }
    .trim();
    !name.is_empty()
        && !is_capture_name(name)
        && !name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Byte offset of the `:` that introduces a placeholder's format spec, or
/// `None` when the placeholder is all name. A `::` is a path separator, so it
/// belongs to the name part: `{runtime::root()}` is one expression with no
/// spec, not a `runtime` capture rendered under a `:root()` spec.
fn format_spec_colon(inner: &str) -> Option<usize> {
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' {
            if bytes.get(i + 1) == Some(&b':') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Splits a template into `FormatSegment`s. `{{` / `}}` escape
/// literal braces; malformed specs (`{x:?}`, `{x:0>5}`) fall
/// through as literal text so the resulting expression still
/// compiles.
/// Returns the byte width of a UTF-8 code point given its leading byte.
/// Used by `parse_format_template` to step past multi-byte chars
/// without splitting them into 1-byte (Latin-1-style) cells. Falls
/// back to 1 for malformed leaders so the loop still makes
/// progress.
fn utf8_char_len(leader: u8) -> usize {
    if leader < 0x80 {
        1
    } else if leader & 0xE0 == 0xC0 {
        2
    } else if leader & 0xF0 == 0xE0 {
        3
    } else if leader & 0xF8 == 0xF0 {
        4
    } else {
        1
    }
}

fn parse_format_template(template: &str) -> Vec<FormatSegment> {
    let bytes = template.as_bytes();
    let mut segments: Vec<FormatSegment> = Vec::new();
    let mut literal = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
                literal.push('{');
                i += 2;
            }
            b'}' if i + 1 < bytes.len() && bytes[i + 1] == b'}' => {
                literal.push('}');
                i += 2;
            }
            b'{' => {
                let close = if let Some(off) = template[i + 1..].find('}') {
                    i + 1 + off
                } else {
                    literal.push('{');
                    i += 1;
                    continue;
                };
                if !literal.is_empty() {
                    segments.push(FormatSegment::Literal(std::mem::take(&mut literal)));
                }
                let inner = template[i + 1..close].trim();
                let name_part = format_spec_colon(inner)
                    .map_or(inner, |at| &inner[..at])
                    .trim();
                if inner.is_empty() {
                    segments.push(FormatSegment::Positional);
                } else if !name_part.is_empty() && name_part.bytes().all(|b| b.is_ascii_digit()) {
                    segments.push(FormatSegment::Indexed(inner.to_string()));
                } else if is_capture_name(inner) {
                    segments.push(FormatSegment::Named(inner.to_string()));
                } else if format_name_looks_like_expr(inner) {
                    segments.push(FormatSegment::Invalid(inner.to_string()));
                } else if let Some(seg) = parse_precision_spec(inner) {
                    segments.push(seg);
                } else if inner == ":?" {
                    segments.push(FormatSegment::PositionalSpec(debug_format_spec()));
                } else if let Some(name) = inner.strip_suffix(":?") {
                    let name = name.trim();
                    if is_capture_name(name) {
                        segments.push(FormatSegment::NamedSpec(
                            name.to_string(),
                            debug_format_spec(),
                        ));
                    } else {
                        segments.push(FormatSegment::Literal(format!("{{{inner}}}")));
                    }
                } else if let Some(seg) = parse_format_spec(inner) {
                    segments.push(seg);
                } else if inner.split_once(':').is_some_and(|(head, _)| {
                    let head = head.trim();
                    head.is_empty() || is_capture_name(head)
                }) {
                    // A placeholder with a spec the grammar does not take
                    // (`{:+}`, `{:e}`) is reported rather than printed as text.
                    segments.push(FormatSegment::Invalid(inner.to_string()));
                } else {
                    segments.push(FormatSegment::Literal(format!("{{{inner}}}")));
                }
                i = close + 1;
            }
            byte if byte < 0x80 => {
                literal.push(byte as char);
                i += 1;
            }
            byte => {
                // Non-ASCII UTF-8 sequence: copy the whole code
                // point's bytes verbatim. The previous
                // `literal.push(bytes[i] as char)` cast a single
                // byte to char (giving U+0080..U+00FF) and re-
                // encoded it as 2-byte UTF-8, which double-encoded
                // every multi-byte char (e.g. an em-dash `-` came
                // out as `â\x80\x94` after the runtime treated
                // each character as Latin-1 again).
                let len = utf8_char_len(byte);
                let end = (i + len).min(bytes.len());
                literal.push_str(&template[i..end]);
                i = end;
            }
        }
    }
    if !literal.is_empty() {
        segments.push(FormatSegment::Literal(literal));
    }
    segments
}

/// Unwraps a trailing `.recv()` method call so that `select` arms can
/// store only the channel expression.
///
/// Source syntax writes the recv explicitly (`x = chan.recv() => …`),
/// but the pretty-printer re-synthesises the `.recv()` on output; if
/// the parser stored the call it would stack up one extra `.recv()`
/// per format round-trip.
fn strip_recv_call(expr: Expr) -> Expr {
    if let ExprKind::MethodCall {
        receiver,
        name,
        generics,
        args,
        ..
    } = &expr.kind
    {
        if name.name == "recv" && generics.is_empty() && args.is_empty() {
            return (**receiver).clone();
        }
    }
    expr
}

/// Splits a `chan.send(value)` method call into its `(channel, value)` parts
/// for a `select` send arm. Returns `None` when the expression is not a
/// single-argument `.send(...)` call.
fn strip_send_call(expr: Expr) -> Option<(Expr, Expr)> {
    if let ExprKind::MethodCall {
        receiver,
        name,
        generics,
        args,
        ..
    } = &expr.kind
    {
        if name.name == "send" && generics.is_empty() && args.len() == 1 {
            return Some(((**receiver).clone(), args[0].clone()));
        }
    }
    None
}

/// The spec for a bare `{:?}`: Debug rendering with no width, fill, radix,
/// or precision.
fn debug_format_spec() -> FormatSpec {
    FormatSpec {
        fill: ' ',
        align: Align::Default,
        width: 0,
        precision: None,
        radix: None,
        upper: false,
        alternate: false,
        debug: true,
        zero_pad: false,
    }
}

/// Parses `:.N` or `name:.N` precision specs out of a `{...}` body.
/// Returns `None` for anything that doesn't match - the caller falls
/// back to emitting the brace block as a literal so unknown specs
/// don't break compilation.
fn parse_precision_spec(inner: &str) -> Option<FormatSegment> {
    let (head, prec_str) = inner.split_once(":.")?;
    let prec: usize = prec_str.parse().ok()?;
    let head = head.trim();
    if head.is_empty() {
        Some(FormatSegment::PositionalPrec(prec))
    } else if is_capture_name(head) {
        Some(FormatSegment::NamedPrec(head.to_string(), prec))
    } else {
        None
    }
}

/// Parses a full `{[name]:[[fill]align][#][0][width][.prec][type]}` spec into a
/// `PositionalSpec` / `NamedSpec`. Returns `None` (so the caller falls back to
/// emitting the brace block literally) for anything it doesn't understand, so
/// an unrecognized spec never breaks compilation. `type` is one of
/// `x`/`X`/`b`/`o`/`d`. The grammar mirrors Rust's `std::fmt` subset.
fn parse_format_spec(inner: &str) -> Option<FormatSegment> {
    let (head, spec) = inner.split_once(':')?;
    let head = head.trim();
    let chars: Vec<char> = spec.chars().collect();
    let mut pos = 0;

    let is_align = |c: char| matches!(c, '<' | '>' | '^');
    let to_align = |c: char| match c {
        '<' => Align::Left,
        '^' => Align::Center,
        _ => Align::Right,
    };

    let mut fill = ' ';
    let mut align = Align::Default;
    // `[fill]align`: a fill char only counts when an align char follows it.
    if chars.len() >= 2 && is_align(chars[1]) {
        fill = chars[0];
        align = to_align(chars[1]);
        pos = 2;
    } else if !chars.is_empty() && is_align(chars[0]) {
        align = to_align(chars[0]);
        pos = 1;
    }

    // `#` requests a binary, octal, or hexadecimal radix prefix.
    let alternate = chars.get(pos) == Some(&'#');
    pos += usize::from(alternate);

    // `0` zero-pad flag. Its meaning depends on what is being padded, so it
    // travels as its own field and is resolved once the value's type is known.
    let zero_pad = chars.get(pos) == Some(&'0');
    pos += usize::from(zero_pad);

    // width
    let mut width = 0usize;
    let mut saw_width = false;
    while let Some(c) = chars.get(pos) {
        if let Some(d) = c.to_digit(10) {
            width = width.checked_mul(10)?.checked_add(d as usize)?;
            saw_width = true;
            pos += 1;
        } else {
            break;
        }
    }

    // `.precision`
    let mut precision = None;
    if chars.get(pos) == Some(&'.') {
        pos += 1;
        let mut p = 0usize;
        let mut saw = false;
        while let Some(c) = chars.get(pos) {
            if let Some(d) = c.to_digit(10) {
                p = p.checked_mul(10)?.checked_add(d as usize)?;
                saw = true;
                pos += 1;
            } else {
                break;
            }
        }
        if !saw {
            return None;
        }
        precision = Some(p);
    }

    // type
    let mut radix = None;
    let mut upper = false;
    if let Some(&c) = chars.get(pos) {
        match c {
            'x' => radix = Some(16),
            'X' => {
                radix = Some(16);
                upper = true;
            }
            'b' => radix = Some(2),
            'o' => radix = Some(8),
            'd' => radix = None,
            _ => return None,
        }
        pos += 1;
    }

    // Reject trailing junk and specs that carry no formatting at all
    // (a bare `{x:}` should not shadow the plain-name path).
    if pos != chars.len() {
        return None;
    }
    if align == Align::Default && !alternate && !saw_width && precision.is_none() && radix.is_none()
    {
        return None;
    }

    let spec = FormatSpec {
        fill,
        align,
        width,
        precision,
        radix,
        upper,
        alternate,
        debug: false,
        zero_pad,
    };
    if head.is_empty() {
        Some(FormatSegment::PositionalSpec(spec))
    } else if is_capture_name(head) {
        Some(FormatSegment::NamedSpec(head.to_string(), spec))
    } else {
        None
    }
}

/// Strips the leading apostrophe from a `'name` label token's source text.
fn label_name(source: &str) -> &str {
    source.strip_prefix('\'').unwrap_or(source)
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// A dotted capture path (`a.balance`, `t.0.name`): an identifier head
/// followed by identifier or tuple-index segments. Bare identifiers are
/// NOT field paths - they stay on the plain `Named` classification.
fn is_field_path(text: &str) -> bool {
    let mut parts = text.split('.');
    let Some(head) = parts.next() else {
        return false;
    };
    if !is_identifier(head) {
        return false;
    }
    let mut any = false;
    for part in parts {
        if !(is_identifier(part) || (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())))
        {
            return false;
        }
        any = true;
    }
    any
}

/// A bare identifier or a dotted capture path - the shapes a named
/// format capture accepts.
fn is_capture_name(text: &str) -> bool {
    is_identifier(text) || is_field_path(text)
}

fn literal_string(expr: &Expr) -> Option<String> {
    if let ExprKind::Literal(Literal::String(s)) = &expr.kind {
        Some(s.clone())
    } else {
        None
    }
}

/// True when `expr` is a Path expression that resolves to
/// `errors::newf` (either as a fully qualified two-segment path
/// or a single-segment `newf` import alias). Used by
/// [`Parser::parse_call_suffix`] to redirect a `errors::newf(...)`
/// call into the format-template expansion path so the same
/// `__concat`-shaped string assembly works on every tier.
fn is_errors_newf_path(expr: &Expr) -> bool {
    let ExprKind::Path(path) = &expr.kind else {
        return false;
    };
    let segs: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
    matches!(segs.as_slice(), ["errors", "newf"] | ["newf"])
}

/// The single segment a path names, or `None` when it is qualified.
fn single_segment_name(path: &PathExpr) -> Option<&str> {
    if path.segments.len() != 1 {
        return None;
    }
    path.segments.first().map(|s| s.name.name.as_str())
}

/// Whether `name` is one the parser expands rather than resolves.
fn is_builtin_call_name(name: &str) -> bool {
    is_format_macro(name) || is_desugar_macro(name) || name == "codegen"
}

/// A module call whose literal argument is checked while the program is parsed.
#[derive(Clone, Copy)]
enum ValidatedCall {
    RegexCompile,
    SqlStatement,
}

/// The parse-time check a module call carries, if any.
fn validated_module_call(path: &PathExpr) -> Option<ValidatedCall> {
    let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
    match names.as_slice() {
        [.., "regex", "compile"] => Some(ValidatedCall::RegexCompile),
        [.., "sql", "statement"] => Some(ValidatedCall::SqlStatement),
        _ => None,
    }
}

/// The last line of the regex engine's report, which names the fault; the
/// lines above it repeat the pattern with a caret the diagnostic's own
/// underline already draws.
fn regex_error_reason(err: &regex::Error) -> String {
    let text = err.to_string();
    text.lines()
        .rev()
        .find_map(|line| line.strip_prefix("error: "))
        .map_or_else(|| text.clone(), str::to_string)
}

/// What makes `statement` malformed, if anything: it is empty, or its
/// parentheses do not balance.
fn sql_statement_error(statement: &str) -> Option<String> {
    if statement.is_empty() {
        return Some("the statement is empty".to_string());
    }
    let mut depth = 0i64;
    for byte in statement.bytes() {
        match byte {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return Some("a `)` closes nothing".to_string());
        }
    }
    (depth != 0).then(|| "a `(` is never closed".to_string())
}

#[cfg(test)]
mod tests {
    use super::Parser;
    use crate::diagnostic::ParseError;
    use gossamer_ast::{BinaryOp, ExprKind};
    use gossamer_lex::SourceMap;

    fn parse_expr_for_test(source: &str) -> gossamer_ast::Expr {
        parse_expr_with_diagnostics(source).0
    }

    /// The parsed expression alongside whatever the parse reported, for the
    /// forms that are now diagnostics rather than desugarings.
    fn parse_expr_with_diagnostics(
        source: &str,
    ) -> (gossamer_ast::Expr, Vec<crate::diagnostic::ParseDiagnostic>) {
        let mut source_map = SourceMap::new();
        let file = source_map.add_file("test.gos", source.to_string());
        let mut parser = Parser::new(source, file);
        let expr = parser.parse_expr();
        let diagnostics = std::mem::take(&mut parser.diagnostics);
        (expr, diagnostics)
    }

    #[test]
    fn precedence_climber_groups_plus_times() {
        let expression = parse_expr_for_test("1 + 2 * 3");
        let ExprKind::Binary { op, rhs, .. } = expression.kind else {
            panic!("expected binary expression");
        };
        assert_eq!(op, BinaryOp::Add);
        let ExprKind::Binary { op: inner_op, .. } = rhs.kind else {
            panic!("expected inner binary expression");
        };
        assert_eq!(inner_op, BinaryOp::Mul);
    }

    /// The single argument of `xs.map(..)` in `source`.
    fn mapped_argument(source: &str) -> gossamer_ast::Expr {
        let expression = parse_expr_for_test(source);
        let ExprKind::MethodCall { mut args, .. } = expression.kind else {
            panic!("expected a method call");
        };
        assert_eq!(args.len(), 1, "expected one argument");
        args.pop().expect("one argument")
    }

    #[test]
    fn a_dollar_is_rejected_wherever_it_is_written() {
        for source in [
            "xs.map($.abs)",
            "xs.map($)",
            "x |> f($, 2)",
            "xs |> $.len()",
            "x |> $",
            "f($)",
        ] {
            let (_expr, diags) = parse_expr_with_diagnostics(source);
            assert!(
                diags
                    .iter()
                    .any(|d| matches!(d.error, ParseError::PipePlaceholderRetired)),
                "{source} should report the retired placeholder: {diags:?}"
            );
        }
    }

    #[test]
    fn a_path_qualified_placeholder_is_malformed() {
        // The name part runs to the spec's `:`, and a `::` is a path
        // separator rather than that colon, so these are expressions in
        // placeholder position and not a `runtime` capture under a spec.
        for source in [
            r#"println("{runtime::root()}")"#,
            r#"println("{runtime::root}")"#,
            r#"println("{std::nope::bar()}")"#,
            r#"println("{a::b:>8}")"#,
        ] {
            let (_expr, diags) = parse_expr_with_diagnostics(source);
            assert!(
                diags
                    .iter()
                    .any(|d| matches!(d.error, ParseError::MalformedFormatPlaceholder { .. })),
                "{source} should report a malformed placeholder: {diags:?}"
            );
        }
    }

    #[test]
    fn ordinary_placeholders_survive_the_path_check() {
        for source in [
            r#"println("{x}")"#,
            r#"println("{}", x)"#,
            r#"println("{:?}", x)"#,
            r#"println("{x:>8}")"#,
            r#"println("{x:.2}")"#,
            r#"println("{a.b}")"#,
            r#"println("{t.0}")"#,
        ] {
            let (_expr, diags) = parse_expr_with_diagnostics(source);
            assert!(
                !diags
                    .iter()
                    .any(|d| matches!(d.error, ParseError::MalformedFormatPlaceholder { .. })),
                "{source} is a well-formed placeholder: {diags:?}"
            );
        }
    }

    #[test]
    fn a_pipe_step_with_arguments_must_be_a_closure() {
        let (_expr, diags) = parse_expr_with_diagnostics("x |> f(a)");
        assert!(
            diags
                .iter()
                .any(|d| matches!(d.error, ParseError::PipeStepNeedsClosure { .. })),
            "an argument-taking step is a closure: {diags:?}"
        );
    }

    #[test]
    fn a_closure_rewrite_avoids_a_name_the_step_already_uses() {
        let (_expr, diags) = parse_expr_with_diagnostics("x |> f(v)");
        let replacement = diags
            .iter()
            .find_map(|d| match &d.error {
                ParseError::PipeStepNeedsClosure { replacement } => replacement.clone(),
                _ => None,
            })
            .expect("expected a rewrite");
        assert_eq!(replacement, "|value| f(value, v)");
    }

    #[test]
    fn a_closure_step_ends_at_the_next_step() {
        // `|>` is left-associative, so the closure is one step and `g` is the
        // next, rather than the body folding the rest of the chain in.
        let expression = parse_expr_for_test("x |> |v| f(v) |> g");
        let ExprKind::Binary { op, lhs, rhs } = expression.kind else {
            panic!("expected a pipe: {:?}", expression.kind);
        };
        assert_eq!(op, BinaryOp::PipeGt);
        assert!(matches!(rhs.kind, ExprKind::Path(_)), "{:?}", rhs.kind);
        let ExprKind::Binary { rhs: inner, .. } = lhs.kind else {
            panic!(
                "expected the closure step as the left operand: {:?}",
                lhs.kind
            );
        };
        assert!(
            matches!(inner.kind, ExprKind::Closure { .. }),
            "{:?}",
            inner.kind
        );
    }

    #[test]
    fn a_closure_argument_inside_a_step_keeps_its_whole_body() {
        // Only the step itself ends at the next `|>`; a closure written as
        // one of its arguments is an ordinary closure.
        let expression = parse_expr_for_test("x |> |v| f(|k| k |> g, v)");
        let ExprKind::Binary { rhs, .. } = expression.kind else {
            panic!("expected a pipe: {:?}", expression.kind);
        };
        let ExprKind::Closure { body, .. } = rhs.kind else {
            panic!("expected the step closure: {:?}", rhs.kind);
        };
        let ExprKind::Call { args, .. } = body.kind else {
            panic!("expected the step's call: {:?}", body.kind);
        };
        let ExprKind::Closure { body: inner, .. } = &args[0].kind else {
            panic!("expected a closure argument: {:?}", args[0].kind);
        };
        assert!(
            matches!(
                inner.kind,
                ExprKind::Binary {
                    op: BinaryOp::PipeGt,
                    ..
                }
            ),
            "{:?}",
            inner.kind
        );
    }

    #[test]
    fn a_closure_outside_a_pipe_step_keeps_its_whole_body() {
        let expression = parse_expr_for_test("|v| f(v) |> g");
        let ExprKind::Closure { body, .. } = expression.kind else {
            panic!("expected a closure: {:?}", expression.kind);
        };
        assert!(
            matches!(
                body.kind,
                ExprKind::Binary {
                    op: BinaryOp::PipeGt,
                    ..
                }
            ),
            "{:?}",
            body.kind
        );
    }

    #[test]
    fn a_bare_callable_step_needs_no_closure() {
        let (_expr, diags) = parse_expr_with_diagnostics("x |> f");
        assert!(diags.is_empty(), "a one-slot step is bare: {diags:?}");
    }

    #[test]
    fn a_closure_step_is_the_way_a_slot_is_named() {
        for source in [
            "x |> |v| f(v, a)",
            "x |> |v| f(a, v)",
            "x |> |v| f(a, v).len()",
        ] {
            let (_expr, diags) = parse_expr_with_diagnostics(source);
            assert!(diags.is_empty(), "{source} should parse clean: {diags:?}");
        }
    }

    #[test]
    fn a_function_named_in_value_position_is_untouched() {
        let arg = mapped_argument("xs.map(helper)");
        assert!(matches!(arg.kind, ExprKind::Path(_)), "{:?}", arg.kind);
    }
}
