//! A filtered token stream that hides whitespace and comments from the parser.

#![forbid(unsafe_code)]

use gossamer_lex::{FileId, Keyword, LexError, Lexer, Punct, Span, Token, TokenKind};

/// Classification of a comment preserved from the raw token stream. The
/// parser does not consume doc-comment semantics yet, but keeping the
/// kind available lets later phases attach leading `//` comments to
/// their documented items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// `// ...` comment.
    Line,
    /// `/* ... */` comment.
    Block,
}

/// One stored comment: its span and kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredComment {
    /// Source range.
    pub span: Span,
    /// Whether the comment is line or block form.
    pub kind: DocKind,
}

/// A buffered, whitespace- and comment-filtered view over the lexer.
pub struct TokenStream {
    /// Backing file id used to construct end-of-file spans.
    file: FileId,
    /// Significant tokens (non-whitespace, non-comment).
    tokens: Vec<Token>,
    /// Current position inside `tokens`.
    position: usize,
    /// Synthetic terminating `Eof` token span.
    eof_span: Span,
    /// Stored comments in source order for later diagnostic/doc work.
    comments: Vec<StoredComment>,
    /// Tokenization errors collected while buffering; drained once by
    /// the parser and surfaced as parse diagnostics.
    lex_errors: Vec<LexError>,
}

impl TokenStream {
    /// Lexes `source`, buffers every significant token, and records
    /// comments on the side. The final entry in `tokens` is always `Eof`.
    #[must_use]
    pub fn new(source: &str, file: FileId) -> Self {
        let mut lexer = Lexer::new(source, file);
        let mut tokens = Vec::new();
        let mut comments = Vec::new();
        let mut eof_span;
        loop {
            let token = lexer.next_token();
            eof_span = token.span;
            match token.kind {
                TokenKind::Whitespace => {}
                TokenKind::LineComment => comments.push(StoredComment {
                    span: token.span,
                    kind: DocKind::Line,
                }),
                TokenKind::BlockComment => comments.push(StoredComment {
                    span: token.span,
                    kind: DocKind::Block,
                }),
                TokenKind::Eof => {
                    tokens.push(token);
                    break;
                }
                _ => tokens.push(token),
            }
        }
        Self {
            file,
            tokens,
            position: 0,
            eof_span,
            comments,
            lex_errors: lexer.take_diagnostics(),
        }
    }

    /// Lexes `source[start..end]` - an expression embedded in a larger
    /// token, such as an interpolated string's placeholder - with every span
    /// in the coordinates of the whole of `source`.
    #[must_use]
    pub fn range(source: &str, file: FileId, start: u32, end: u32) -> Self {
        let text = source.get(start as usize..end as usize).unwrap_or_default();
        let mut stream = Self::new(text, file);
        let shift = |span: Span| Span::new(span.file, span.start + start, span.end + start);
        for token in &mut stream.tokens {
            token.span = shift(token.span);
        }
        for comment in &mut stream.comments {
            comment.span = shift(comment.span);
        }
        stream.eof_span = Span::new(file, end, end);
        let last = stream.tokens.len() - 1;
        stream.tokens[last].span = stream.eof_span;
        stream.lex_errors = stream
            .lex_errors
            .into_iter()
            .map(|err| err.shifted(start))
            .collect();
        stream
    }

    /// Drains the tokenization errors collected while buffering.
    pub fn take_lex_errors(&mut self) -> Vec<LexError> {
        std::mem::take(&mut self.lex_errors)
    }

    /// Returns the file id the stream was built for.
    #[must_use]
    pub const fn file(&self) -> FileId {
        self.file
    }

    /// Returns the comments discovered while lexing, in source order.
    #[must_use]
    pub fn comments(&self) -> &[StoredComment] {
        &self.comments
    }

    /// Returns the current token without advancing.
    #[must_use]
    pub fn peek(&self) -> Token {
        self.tokens[self.position]
    }

    /// Returns the token `offset` positions after the cursor, clamped to `Eof`.
    #[must_use]
    pub fn peek_at(&self, offset: usize) -> Token {
        let index = self.position.saturating_add(offset);
        let last = self.tokens.len().saturating_sub(1);
        self.tokens[index.min(last)]
    }

    /// Returns the second token after the cursor.
    #[must_use]
    pub fn peek2(&self) -> Token {
        self.peek_at(1)
    }

    /// Returns the current cursor position as a checkpoint for rewinding.
    #[must_use]
    pub const fn checkpoint(&self) -> usize {
        self.position
    }

    /// Returns the span of the most recently consumed token. Falls back
    /// to the synthetic EOF span when nothing has been consumed yet.
    #[must_use]
    pub fn previous_span(&self) -> Span {
        if self.position == 0 {
            return self.tokens[0].span;
        }
        self.tokens[self.position - 1].span
    }

    /// Rewinds the cursor to a previously captured checkpoint.
    pub fn rewind(&mut self, mark: usize) {
        self.position = mark;
    }

    /// Consumes and returns the current token.
    pub fn bump(&mut self) -> Token {
        let token = self.peek();
        if !matches!(token.kind, TokenKind::Eof) {
            self.position += 1;
        }
        token
    }

    /// Returns `true` when the next token matches `kind`.
    #[must_use]
    pub fn at_kind(&self, kind: TokenKind) -> bool {
        self.peek().kind == kind
    }

    /// Returns `true` when the next token is the given keyword.
    #[must_use]
    pub fn at_keyword(&self, keyword: Keyword) -> bool {
        matches!(self.peek().kind, TokenKind::Keyword(found) if found == keyword)
    }

    /// Returns `true` when the next token is the given punctuation.
    #[must_use]
    pub fn at_punct(&self, punct: Punct) -> bool {
        matches!(self.peek().kind, TokenKind::Punct(found) if found == punct)
    }

    /// If the next token is `keyword`, consume it and return `true`.
    pub fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        if self.at_keyword(keyword) {
            self.bump();
            return true;
        }
        false
    }

    /// If the next token is `punct`, consume it and return `true`.
    pub fn eat_punct(&mut self, punct: Punct) -> bool {
        if self.at_punct(punct) {
            self.bump();
            return true;
        }
        false
    }

    /// Returns `true` when the cursor is at a token whose leading `>`
    /// closes a generic list - a bare `>`, or a compound `>>` / `>=` /
    /// `>>=` produced by the maximal-munch lexer for nested generics
    /// like `Vec<Vec<T>>`.
    #[must_use]
    pub fn at_close_angle(&self) -> bool {
        matches!(
            self.peek().kind,
            TokenKind::Punct(Punct::Gt | Punct::ShiftR | Punct::GtEq | Punct::ShiftREq)
        )
    }

    /// Consumes a single closing `>` for a generic list. A compound
    /// `>>` / `>=` / `>>=` token is split: the leading `>` is consumed
    /// and the remainder (`>` / `=` / `>=`) is rewritten in place so an
    /// enclosing generic list - or the trailing operator - still sees
    /// it. Returns `false` when the cursor is not at a closing angle.
    pub fn eat_close_angle(&mut self) -> bool {
        let tok = self.peek();
        let TokenKind::Punct(p) = tok.kind else {
            return false;
        };
        let remainder = match p {
            Punct::Gt => {
                self.bump();
                return true;
            }
            Punct::ShiftR => Punct::Gt,
            Punct::GtEq => Punct::Eq,
            Punct::ShiftREq => Punct::GtEq,
            _ => return false,
        };
        self.tokens[self.position] = Token {
            kind: TokenKind::Punct(remainder),
            span: Span {
                file: tok.span.file,
                start: tok.span.start + 1,
                end: tok.span.end,
            },
        };
        true
    }

    /// Returns `true` when the cursor is at the synthetic `Eof` token.
    #[must_use]
    pub fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    /// Returns the span that the parser should use for diagnostics beyond
    /// the end of input.
    #[must_use]
    pub const fn eof_span(&self) -> Span {
        self.eof_span
    }
}
