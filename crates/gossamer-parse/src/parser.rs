//! Core parser state shared across all productions.

#![forbid(unsafe_code)]

use gossamer_ast::{NodeId, NodeIdGenerator};
use gossamer_lex::{FileId, Keyword, Punct, Span, Token, TokenKind};

use crate::diagnostic::{ParseDiagnostic, ParseError};
use crate::stream::TokenStream;

/// Hard limit on parser recursion depth. Sized to comfortably exceed
/// every real-world program while preventing adversarial inputs like
/// `((((((((x))))))))` from blowing the C stack.
pub(crate) const RECURSION_LIMIT: u32 = 256;

/// Hand-written recursive-descent parser over a buffered token stream.
pub struct Parser<'src> {
    /// Raw source text (so the parser can recover identifier names and
    /// preserve literal spellings).
    pub(crate) source: &'src str,
    /// Token source.
    pub(crate) tokens: TokenStream,
    /// Monotonic AST id generator.
    pub(crate) ids: NodeIdGenerator,
    /// Accumulated diagnostics (drained by `parse_source_file`).
    pub(crate) diagnostics: Vec<ParseDiagnostic>,
    /// Depth of nested contexts that forbid an unparenthesised struct
    /// literal (`if`, `while`, `match` scrutinee).
    pub(crate) no_struct_literal_depth: u32,
    /// Depth of contexts where `|` denotes a pattern alternative and
    /// must not be consumed as bitwise-or by the Pratt loop.
    pub(crate) pattern_pipe_depth: u32,
    /// Set while the right operand of `|>` is being parsed, so a closure
    /// written directly as a step ends its body at the next `|>` rather
    /// than swallowing the rest of the chain.
    pub(crate) parsing_pipe_step: bool,
    /// Depth of match-arm body parses where a newline may introduce the
    /// next arm's pattern.
    pub(crate) match_arm_body_depth: u32,
    /// Depth of contexts where an empty brace pair should be parsed as a unit
    /// block instead of the expression-position empty `HashMap` literal.
    pub(crate) empty_brace_block_depth: u32,
    /// Running depth of recursive entries into expression, type, and
    /// pattern parsers. Compared against [`RECURSION_LIMIT`] by
    /// [`Parser::enter_recursion`].
    pub(crate) recursion_depth: u32,
    /// Set once the parser has emitted a recursion-limit diagnostic so
    /// subsequent overflows in the same parse do not flood the
    /// diagnostic stream with duplicates.
    pub(crate) recursion_limit_reported: bool,
    /// `use` declarations encountered inside inline `mod ... { ... }`
    /// bodies. The mod-body grammar collects them into this side
    /// channel so [`crate::parse_source_file`] can hoist them to the
    /// `SourceFile.uses` list - the resolver only walks the
    /// file-level `uses` slot, so a `use std::encoding::json` inside
    /// `mod chat { ... }` would otherwise be silently dropped.
    pub(crate) hoisted_uses: Vec<gossamer_ast::UseDecl>,
    /// Inline modules the parser is inside, outermost first. A hoisted `use`
    /// carries this so a relative path keeps the anchor it was written at.
    pub(crate) mod_stack: Vec<String>,
    /// Argument labels written at call sites (`f(width: 10)`), moved into
    /// [`gossamer_ast::SourceFile::named_args`] by
    /// [`crate::parse_source_file`]. Held beside the tree rather than in
    /// the call expression so `ExprKind::Call` keeps one shape everywhere.
    pub(crate) named_args:
        std::collections::HashMap<gossamer_ast::NodeId, Vec<gossamer_ast::NamedArg>>,
    /// Token position at which a newline was last accepted as a list
    /// separator. A comma is consumed and so always advances the parser; a
    /// newline is not a token, so accepting the same one twice would let a
    /// list loop ask for an element that never arrives.
    pub(crate) newline_separator_at: Option<usize>,
    /// Whether the expression being parsed sits directly inside a grouping
    /// `( .. )`, where a line that opens with an operator continues the one
    /// above instead of starting a statement. Every other delimiter, block
    /// included, clears it for its contents.
    pub(crate) in_paren_group: bool,
    /// Trait named by the header of the `impl` block being parsed, when
    /// one is. A method's contract is decided by that header, so the name
    /// has to be in hand while the method itself is read.
    pub(crate) impl_trait_name: Option<String>,
    /// Depth of items the autoderive stage spliced into the unit. Their
    /// text has no source position an author could edit, so a spelling
    /// rewrite is reported against what the author wrote or not at all.
    pub(crate) synthesized_depth: u32,
}

impl<'src> Parser<'src> {
    /// Builds a parser for `source` tagged with `file`.
    #[must_use]
    pub fn new(source: &'src str, file: FileId) -> Self {
        // Strip a leading UTF-8 BOM (U+FEFF) once, here, so the stored
        // `source` and the `TokenStream` below are built from the same
        // text - token spans and `slice` then share one basis. Windows
        // editors emit a BOM by default; `SourceMap`'s `SourceFile::new`
        // strips the same prefix so diagnostic line/columns stay aligned.
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        let mut tokens = TokenStream::new(source, file);
        // Tokenization errors (unterminated comment/string, bad escape,
        // ...) become parse diagnostics here; dropping them with the
        // lexer made a file with an unterminated `/*` parse as an
        // empty-but-valid source file.
        let diagnostics = tokens
            .take_lex_errors()
            .into_iter()
            .map(|err| {
                ParseDiagnostic::new(
                    ParseError::Lex {
                        message: err.to_string(),
                    },
                    err.span(),
                )
            })
            .collect();
        Self {
            source,
            tokens,
            ids: NodeIdGenerator::new(),
            diagnostics,
            no_struct_literal_depth: 0,
            impl_trait_name: None,
            synthesized_depth: 0,
            pattern_pipe_depth: 0,
            parsing_pipe_step: false,
            match_arm_body_depth: 0,
            empty_brace_block_depth: 0,
            recursion_depth: 0,
            recursion_limit_reported: false,
            hoisted_uses: Vec::new(),
            mod_stack: Vec::new(),
            named_args: std::collections::HashMap::new(),
            newline_separator_at: None,
            in_paren_group: false,
        }
    }

    /// Drains the call-site argument names collected during the parse.
    /// See the field docs on the parser's `named_args` field.
    pub fn take_named_args(
        &mut self,
    ) -> std::collections::HashMap<gossamer_ast::NodeId, Vec<gossamer_ast::NamedArg>> {
        std::mem::take(&mut self.named_args)
    }

    /// Files `labels` under `call`, the id of the call they belong to.
    pub(crate) fn record_named_args(
        &mut self,
        call: gossamer_ast::NodeId,
        labels: Vec<gossamer_ast::NamedArg>,
    ) {
        if !labels.is_empty() {
            self.named_args.insert(call, labels);
        }
    }

    /// Drains the parser's collection of `use` decls hoisted out of
    /// inline-module bodies. See the field docs on the parser's
    /// `hoisted_uses` field.
    pub fn take_hoisted_uses(&mut self) -> Vec<gossamer_ast::UseDecl> {
        std::mem::take(&mut self.hoisted_uses)
    }

    /// Returns the file id being parsed.
    #[must_use]
    pub fn file(&self) -> FileId {
        self.tokens.file()
    }

    /// Allocates the next fresh AST node id.
    pub(crate) fn alloc_id(&mut self) -> NodeId {
        self.ids.next()
    }

    /// Builds a span covering [`lo.start`, `hi.end`) in the current file.
    #[must_use]
    pub(crate) fn join(&self, lo: Span, hi: Span) -> Span {
        Span::new(
            self.tokens.file(),
            lo.start.min(hi.start),
            lo.end.max(hi.end),
        )
    }

    /// Records a diagnostic without stopping the parser.
    pub(crate) fn record(&mut self, error: ParseError, span: Span) {
        self.diagnostics.push(ParseDiagnostic::new(error, span));
    }

    /// Number of diagnostics recorded so far. Compared across a
    /// sub-parse to tell whether it reported anything.
    #[must_use]
    pub(crate) fn diagnostic_count(&self) -> usize {
        self.diagnostics.len()
    }

    /// Returns the accumulated diagnostics, leaving the parser's vector empty.
    pub fn take_diagnostics(&mut self) -> Vec<ParseDiagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    /// Returns the span of the current (peeked) token.
    #[must_use]
    pub(crate) fn peek_span(&self) -> Span {
        self.tokens.peek().span
    }

    /// Peeks the current token.
    #[must_use]
    pub(crate) fn peek(&self) -> Token {
        self.tokens.peek()
    }

    /// Peeks the nth token after the cursor.
    #[must_use]
    pub(crate) fn peek_nth(&self, offset: usize) -> Token {
        self.tokens.peek_at(offset)
    }

    /// Consumes the current token and returns it.
    pub(crate) fn bump(&mut self) -> Token {
        self.tokens.bump()
    }

    /// Returns the span of the most recently consumed token, or the
    /// current token's span when nothing has been consumed yet. Used
    /// to close a span range after `start..`.
    #[must_use]
    pub(crate) fn last_span(&self) -> Span {
        let position = self.tokens.checkpoint();
        if position == 0 {
            return self.peek_span();
        }
        self.tokens.previous_span()
    }

    /// Returns `true` at end of input.
    #[must_use]
    pub(crate) fn at_eof(&self) -> bool {
        self.tokens.at_eof()
    }

    /// Returns `true` when the current token matches a punctuation kind.
    #[must_use]
    pub(crate) fn at_punct(&self, punct: Punct) -> bool {
        self.tokens.at_punct(punct)
    }

    /// Returns `true` when the current token matches a keyword.
    #[must_use]
    pub(crate) fn at_keyword(&self, keyword: Keyword) -> bool {
        self.tokens.at_keyword(keyword)
    }

    /// Attempts to consume `punct`, returning whether it was present.
    pub(crate) fn eat_punct(&mut self, punct: Punct) -> bool {
        self.tokens.eat_punct(punct)
    }

    /// Returns `true` when the cursor is at a closing `>` for a generic
    /// list, including a compound `>>` / `>=` / `>>=` token.
    #[must_use]
    pub(crate) fn at_close_angle(&self) -> bool {
        self.tokens.at_close_angle()
    }

    /// Consumes a closing `>` for a generic list, splitting a compound
    /// `>>` / `>=` / `>>=` token so the remainder stays available, and
    /// records a diagnostic when the cursor is not at a closing angle.
    pub(crate) fn expect_close_angle(&mut self, context: &str) -> bool {
        if self.tokens.eat_close_angle() {
            return true;
        }
        let found = self.peek_text();
        self.record(
            ParseError::unexpected(format!("`>` {context}"), found),
            self.peek_span(),
        );
        false
    }

    /// Attempts to consume `keyword`, returning whether it was present.
    pub(crate) fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        self.tokens.eat_keyword(keyword)
    }

    /// Returns `true` when the current token is the identifier `word`.
    /// The block words (`select`, `defer`, `comptime`, `cohort`, `arena`,
    /// `spawn`, `newtype`, `packed`) are contextual: each is a keyword
    /// only where its construct can start, and an ordinary name anywhere
    /// else.
    #[must_use]
    pub(crate) fn at_contextual_word(&self, word: &str) -> bool {
        let token = self.peek();
        matches!(token.kind, TokenKind::Ident) && self.slice(token.span) == word
    }

    /// Returns `true` when the token `offset` ahead is `punct`.
    #[must_use]
    pub(crate) fn peek_nth_is_punct(&self, offset: usize, punct: Punct) -> bool {
        matches!(self.peek_nth(offset).kind, TokenKind::Punct(p) if p == punct)
    }

    /// Whether the cursor sits inside an item the compiler synthesized.
    #[must_use]
    pub(crate) fn in_synthesized_item(&self) -> bool {
        self.synthesized_depth > 0
    }

    /// Consumes the identifier `word` when it is the current token.
    pub(crate) fn eat_contextual_word(&mut self, word: &str) -> bool {
        if self.at_contextual_word(word) {
            self.bump();
            return true;
        }
        false
    }

    /// Consumes `punct` or records a diagnostic if absent.
    pub(crate) fn expect_punct(&mut self, punct: Punct, context: &str) -> bool {
        if self.eat_punct(punct) {
            return true;
        }
        let found = self.peek_text();
        self.record(
            ParseError::unexpected(format!("`{}` {}", punct.as_str(), context), found),
            self.peek_span(),
        );
        false
    }

    /// Consumes `keyword` or records a diagnostic if absent.
    pub(crate) fn expect_keyword(&mut self, keyword: Keyword, context: &str) -> bool {
        if self.eat_keyword(keyword) {
            return true;
        }
        let found = self.peek_text();
        self.record(
            ParseError::unexpected(format!("`{}` {}", keyword.as_str(), context), found),
            self.peek_span(),
        );
        false
    }

    /// Returns a short human-readable description of the current token,
    /// used when composing "unexpected token" diagnostics.
    #[must_use]
    pub(crate) fn peek_text(&self) -> String {
        token_text(self.peek())
    }

    /// Enters a scope where unparenthesised struct literals are forbidden.
    pub(crate) fn enter_no_struct(&mut self) {
        self.no_struct_literal_depth = self.no_struct_literal_depth.saturating_add(1);
    }

    /// Leaves a scope where unparenthesised struct literals are forbidden.
    pub(crate) fn leave_no_struct(&mut self) {
        self.no_struct_literal_depth = self.no_struct_literal_depth.saturating_sub(1);
    }

    /// `true` when a struct literal is currently forbidden without parens.
    #[must_use]
    pub(crate) const fn struct_literal_forbidden(&self) -> bool {
        self.no_struct_literal_depth > 0
    }

    /// Suspends the no-struct-literal restriction for the duration of
    /// `f`. Delimited contexts (call arguments, parentheses, brackets,
    /// blocks, struct-literal fields) re-allow struct literals even
    /// inside a `match` scrutinee or `if`/`while` condition - the
    /// surrounding delimiter removes the `{` ambiguity the restriction
    /// exists to resolve.
    pub(crate) fn with_struct_literals_allowed<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::take(&mut self.no_struct_literal_depth);
        let saved_group = std::mem::take(&mut self.in_paren_group);
        let out = f(self);
        self.no_struct_literal_depth = saved;
        self.in_paren_group = saved_group;
        out
    }

    /// Enters a scope where `|` denotes a pattern alternative.
    pub(crate) fn enter_pattern_pipe(&mut self) {
        self.pattern_pipe_depth = self.pattern_pipe_depth.saturating_add(1);
    }

    /// Leaves a scope where `|` denotes a pattern alternative.
    pub(crate) fn leave_pattern_pipe(&mut self) {
        self.pattern_pipe_depth = self.pattern_pipe_depth.saturating_sub(1);
    }

    /// Runs `f` with the pipe-step flag set, and restores it after.
    pub(crate) fn with_pipe_step<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let previous = std::mem::replace(&mut self.parsing_pipe_step, true);
        let out = f(self);
        self.parsing_pipe_step = previous;
        out
    }

    /// `true` when the Pratt loop must treat bitwise `|` as a pattern separator.
    #[must_use]
    pub(crate) const fn in_pattern_pipe(&self) -> bool {
        self.pattern_pipe_depth > 0
    }

    /// Enters a match-arm body expression.
    pub(crate) fn enter_match_arm_body(&mut self) {
        self.match_arm_body_depth = self.match_arm_body_depth.saturating_add(1);
    }

    /// Leaves a match-arm body expression.
    pub(crate) fn leave_match_arm_body(&mut self) {
        self.match_arm_body_depth = self.match_arm_body_depth.saturating_sub(1);
    }

    /// `true` while parsing a match-arm body expression.
    #[must_use]
    pub(crate) const fn in_match_arm_body(&self) -> bool {
        self.match_arm_body_depth > 0
    }

    /// Parses `f` with empty `{}` treated as a block in brace-leading
    /// expression positions.
    pub(crate) fn with_empty_braces_as_blocks<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.empty_brace_block_depth = self.empty_brace_block_depth.saturating_add(1);
        let out = f(self);
        self.empty_brace_block_depth = self.empty_brace_block_depth.saturating_sub(1);
        out
    }

    /// Parses `f` with empty `{}` read as a map literal again.
    ///
    /// The block preference belongs to the brace-leading expression position
    /// it was entered for, not to everything nested inside it: an expression
    /// statement raises it, and a `for` or `while` is an expression, so its
    /// whole body would otherwise inherit a preference that only made sense
    /// at the statement's own head. A position where a block is not one of
    /// the alternatives - the initialiser of a `let`, an argument - clears it.
    pub(crate) fn with_empty_braces_as_maps<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::take(&mut self.empty_brace_block_depth);
        let out = f(self);
        self.empty_brace_block_depth = saved;
        out
    }

    /// `true` when `{}` should parse as an empty block in the current context.
    #[must_use]
    pub(crate) const fn prefer_empty_brace_block(&self) -> bool {
        self.empty_brace_block_depth > 0
    }

    /// Returns the raw source slice covered by `span`, or `""` if the
    /// span is out of range or not on UTF-8 char boundaries. `str::get`
    /// rejects both, so an adversarial or malformed span can never
    /// panic the parser on arbitrary input.
    #[must_use]
    pub(crate) fn slice(&self, span: Span) -> &'src str {
        self.source
            .get(span.start as usize..span.end as usize)
            .unwrap_or("")
    }

    /// Bumps the recursion counter and records a diagnostic when the
    /// configured [`RECURSION_LIMIT`] is hit. Returns `Err(())` when
    /// the limit has been reached; callers should then return a stub
    /// node (typically `ExprKind::Error`, `PatternKind::Error`, or an
    /// `Infer` type) so the parser can keep making forward progress.
    /// Successful calls must be paired with [`Parser::leave_recursion`].
    pub(crate) fn enter_recursion(&mut self, span: Span) -> Result<(), ()> {
        if self.recursion_depth >= RECURSION_LIMIT {
            if !self.recursion_limit_reported {
                self.recursion_limit_reported = true;
                self.record(
                    ParseError::RecursionLimit {
                        limit: RECURSION_LIMIT,
                    },
                    span,
                );
            }
            return Err(());
        }
        self.recursion_depth += 1;
        Ok(())
    }

    /// Decrements the recursion counter. Must follow a successful
    /// [`Parser::enter_recursion`] call on the same logical scope.
    pub(crate) fn leave_recursion(&mut self) {
        self.recursion_depth = self.recursion_depth.saturating_sub(1);
    }

    /// Returns `true` when at least one newline appears in the source
    /// between the most recently consumed token and the current peek
    /// token. Used by the Pratt loop to break statement continuation
    /// for the operators that also begin an expression (`&`, `*`, `-`,
    /// and a closure's `|`) so that `let x = expr\n&y` parses as two
    /// statements, not `expr & y`.
    #[must_use]
    pub(crate) fn newline_before_peek(&self) -> bool {
        let last_end = self.last_span().end as usize;
        let peek_start = self.peek_span().start as usize;
        if peek_start <= last_end || peek_start > self.source.len() {
            return false;
        }
        self.source[last_end..peek_start].contains('\n')
    }

    /// Consumes a comma separator, or accepts an authored newline as the
    /// separator for a multi-line delimited list.
    ///
    /// A comma is consumed, so reporting one always advances the parser. A
    /// newline is not a token and consumes nothing, so it is only a separator
    /// while an element could still follow: at the end of input there is
    /// nothing left to separate, and reporting one there would leave a list
    /// loop asking for another element forever.
    pub(crate) fn eat_list_separator(&mut self) -> bool {
        if self.eat_punct(Punct::Comma) {
            self.newline_separator_at = None;
            return true;
        }
        if self.at_eof() || !self.newline_before_peek() {
            return false;
        }
        // One newline separates one pair of elements. Reporting the same one
        // again would mean the element between them consumed nothing, so the
        // list has stopped making progress and the delimiter check takes over.
        let position = self.tokens.checkpoint();
        if self.newline_separator_at == Some(position) {
            return false;
        }
        self.newline_separator_at = Some(position);
        true
    }

    /// Consumes an optional semicolon after a statement.
    pub(crate) fn reject_trailing_semicolon(&mut self) -> bool {
        self.eat_statement_semicolon()
    }

    /// Consumes an optional statement semicolon.
    pub(crate) fn eat_statement_semicolon(&mut self) -> bool {
        self.eat_punct(Punct::Semi)
    }
}

/// Returns a short human-readable rendering of a token for diagnostics.
fn token_text(token: Token) -> String {
    match token.kind {
        TokenKind::Eof => "<end of input>".to_string(),
        TokenKind::Keyword(keyword) => format!("keyword `{}`", keyword.as_str()),
        TokenKind::Punct(punct) => format!("`{}`", punct.as_str()),
        TokenKind::Ident => "identifier".to_string(),
        TokenKind::IntLit => "integer literal".to_string(),
        TokenKind::FloatLit => "float literal".to_string(),
        TokenKind::StringLit | TokenKind::RawStringLit { .. } | TokenKind::TripleStringLit => {
            "string literal".to_string()
        }
        TokenKind::CharLit => "char literal".to_string(),
        TokenKind::ByteLit => "byte literal".to_string(),
        TokenKind::ByteStringLit | TokenKind::RawByteStringLit { .. } => {
            "byte string literal".to_string()
        }
        TokenKind::LineComment | TokenKind::BlockComment => "comment".to_string(),
        TokenKind::Whitespace => "whitespace".to_string(),
        TokenKind::Label => "label".to_string(),
        TokenKind::Invalid => "invalid token".to_string(),
    }
}
