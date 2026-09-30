//! Top-level lexer driver. Ties the sub-modules together and exposes a
//! `Lexer` that yields tokens until end of input.

use crate::comment::{CommentOutcome, lex_comment};
use crate::cursor::Cursor;
use crate::diagnostic::LexError;
use crate::number::lex_number;
use crate::punct::lex_punct;
use crate::span::{FileId, Span};
use crate::string::{
    QuotedOutcome, TRIPLE_QUOTE, lex_byte, lex_byte_string, lex_char, lex_raw_string, lex_string,
    lex_triple_string,
};
use crate::token::{Keyword, Token, TokenKind};

/// Streaming lexer that yields one `Token` per call to `next_token`.
///
/// The lexer is infallible: on bad input it emits `TokenKind::Invalid`
/// or partial-but-classified tokens and records a `LexError` on the
/// side. Callers drain diagnostics via `take_diagnostics` at any point.
pub struct Lexer<'src> {
    cursor: Cursor<'src>,
    file: FileId,
    diagnostics: Vec<LexError>,
}

impl<'src> Lexer<'src> {
    /// Constructs a lexer for `source` tagged with `file`.
    ///
    /// Token spans are byte offsets into `source` exactly as given -
    /// the lexer does not strip a leading BOM, because callers (the
    /// parser, the REPL highlighter) slice their own copy of `source`
    /// by those spans and must share one basis. BOM stripping is a
    /// source-preprocessing step done once in `Parser::new` and
    /// `SourceMap`'s `SourceFile::new`.
    #[must_use]
    pub const fn new(source: &'src str, file: FileId) -> Self {
        Self {
            cursor: Cursor::new(source),
            file,
            diagnostics: Vec::new(),
        }
    }

    /// Returns `true` when the lexer has consumed all of its input.
    #[must_use]
    pub fn is_eof(&self) -> bool {
        self.cursor.is_eof()
    }

    /// Removes and returns every diagnostic collected since the last drain.
    pub fn take_diagnostics(&mut self) -> Vec<LexError> {
        std::mem::take(&mut self.diagnostics)
    }

    /// Consumes and returns the next token, or `Eof` at end of input.
    pub fn next_token(&mut self) -> Token {
        let start = self.current_offset();
        let first = self.cursor.peek();
        if self.cursor.is_eof() {
            return Token::new(TokenKind::Eof, self.span_from(start));
        }
        let kind = self.dispatch(first, start);
        Token::new(kind, self.span_from(start))
    }

    /// Dispatches to the appropriate helper based on the first character.
    fn dispatch(&mut self, first: char, start: u32) -> TokenKind {
        if is_whitespace(first) {
            self.cursor.bump_while(is_whitespace);
            return TokenKind::Whitespace;
        }
        // Permit a Unix hashbang at the beginning of a source file. Treat it
        // exactly like a line comment so its newline stays in the source and
        // all following spans retain their on-disk line numbers. `#![` opens
        // a file-level inner attribute, which a hashbang never spells.
        if start == 0
            && first == '#'
            && self.cursor.peek_nth(1) == '!'
            && self.cursor.peek_nth(2) != '['
        {
            self.cursor.bump_while(|character| character != '\n');
            return TokenKind::LineComment;
        }
        if first == '/'
            && let Some(kind) = self.try_comment(start)
        {
            return kind;
        }
        if first == '"' {
            return self.finish_string(start);
        }
        if first == '\'' {
            return self.finish_label_or_char(start);
        }
        if first.is_ascii_digit() {
            return lex_number(&mut self.cursor);
        }
        if is_ident_start(first) {
            return self.lex_ident_or_prefix(start);
        }
        self.lex_punct_or_invalid(start)
    }

    /// Attempts to lex a `//` or `/* */` comment starting at the current
    /// `/`. Returns `None` if the `/` is an operator, not a comment.
    fn try_comment(&mut self, start: u32) -> Option<TokenKind> {
        match lex_comment(&mut self.cursor) {
            CommentOutcome::Lexed(kind) => Some(kind),
            CommentOutcome::Unterminated => {
                self.diagnostics.push(LexError::UnterminatedBlockComment {
                    span: self.span_from(start),
                });
                Some(TokenKind::BlockComment)
            }
            CommentOutcome::NotAComment => None,
        }
    }

    /// Classifies a run of identifier characters as a keyword, a
    /// string-prefix literal (`r"..."`, `b"..."`, `br"..."`, `f"..."`), or a
    /// plain identifier.
    fn lex_ident_or_prefix(&mut self, start: u32) -> TokenKind {
        if let Some(kind) = self.try_prefixed_string(start) {
            return kind;
        }
        self.cursor.bump_while(is_ident_continue);
        let text = &self.cursor.source()[start as usize..self.current_offset() as usize];
        Keyword::from_ident(text).map_or(TokenKind::Ident, TokenKind::Keyword)
    }

    /// If the cursor sits at `r"`, `r#`, `b"`, `b'`, `br"`/`br#`, or `f"`,
    /// lexes the corresponding literal and returns its token kind.
    fn try_prefixed_string(&mut self, start: u32) -> Option<TokenKind> {
        match (self.cursor.peek(), self.cursor.peek_nth(1)) {
            ('f', '"') => {
                self.cursor.bump();
                let outcome =
                    crate::string::lex_interpolated_string(&mut self.cursor, self.file, start);
                Some(match self.absorb_quoted(outcome) {
                    TokenKind::StringLit => TokenKind::FStringLit,
                    TokenKind::TripleStringLit => TokenKind::FTripleStringLit,
                    other => other,
                })
            }
            ('r', '"' | '#') => Some(self.drive_raw_string(start, false)),
            ('b', '"') => Some(self.drive_byte_string(start)),
            ('b', '\'') => Some(self.drive_byte_literal(start)),
            ('b', 'r') if matches!(self.cursor.peek_nth(2), '"' | '#') => {
                self.cursor.bump();
                Some(self.drive_raw_string(start, true))
            }
            _ => None,
        }
    }

    /// Lexes a `"..."` or `"""..."""` string literal and forwards any
    /// diagnostics.
    fn finish_string(&mut self, start: u32) -> TokenKind {
        let outcome = if self.cursor.rest().starts_with(TRIPLE_QUOTE) {
            lex_triple_string(&mut self.cursor, self.file, start)
        } else {
            lex_string(&mut self.cursor, self.file, start)
        };
        self.absorb_quoted(outcome)
    }

    /// Decides between a loop label (`'name`) and a character literal
    /// (`'x'`). A `'` followed by an identifier-start character whose
    /// run of identifier characters does not terminate at a closing
    /// `'` is a label; everything else is a character literal.
    fn finish_label_or_char(&mut self, start: u32) -> TokenKind {
        if self.at_label() {
            return self.finish_label();
        }
        self.finish_char(start)
    }

    /// Returns `true` when the cursor (sitting on `'`) begins a label.
    fn at_label(&self) -> bool {
        if !is_ident_start(self.cursor.peek_nth(1)) {
            return false;
        }
        let mut offset = 2;
        loop {
            let character = self.cursor.peek_nth(offset);
            if is_ident_continue(character) {
                offset += 1;
                continue;
            }
            // A closing `'` after the identifier run means this was a
            // (possibly over-long) char literal like `'a'` / `'ab'`.
            return character != '\'';
        }
    }

    /// Lexes a `'name` label, including the leading apostrophe.
    fn finish_label(&mut self) -> TokenKind {
        self.cursor.bump();
        self.cursor.bump_while(is_ident_continue);
        TokenKind::Label
    }

    /// Lexes a `'x'` character literal and forwards any diagnostics.
    fn finish_char(&mut self, start: u32) -> TokenKind {
        let outcome = lex_char(&mut self.cursor, self.file, start);
        self.absorb_quoted(outcome)
    }

    /// Drives the raw-string sub-lexer for either `r"..."` or `br"..."`.
    fn drive_raw_string(&mut self, start: u32, byte_flavor: bool) -> TokenKind {
        let outcome = lex_raw_string(&mut self.cursor, self.file, start, byte_flavor);
        self.absorb_quoted(outcome)
    }

    /// Drives the byte-string sub-lexer for `b"..."`.
    fn drive_byte_string(&mut self, start: u32) -> TokenKind {
        self.cursor.bump();
        let outcome = lex_byte_string(&mut self.cursor, self.file, start);
        self.absorb_quoted(outcome)
    }

    /// Drives the byte-literal sub-lexer for `b'x'`.
    fn drive_byte_literal(&mut self, start: u32) -> TokenKind {
        self.cursor.bump();
        let outcome = lex_byte(&mut self.cursor, self.file, start);
        self.absorb_quoted(outcome)
    }

    /// Attempts to lex a punctuation token at the current cursor.
    /// Emits `Invalid` plus a diagnostic when nothing matches.
    fn lex_punct_or_invalid(&mut self, start: u32) -> TokenKind {
        if let Some(punct) = lex_punct(&mut self.cursor) {
            TokenKind::Punct(punct)
        } else {
            self.cursor.bump();
            self.diagnostics.push(LexError::UnexpectedChar {
                span: self.span_from(start),
            });
            TokenKind::Invalid
        }
    }

    /// Moves diagnostics out of a `QuotedOutcome` into the lexer state.
    fn absorb_quoted(&mut self, outcome: QuotedOutcome) -> TokenKind {
        self.diagnostics.extend(outcome.diagnostics);
        outcome.kind
    }

    /// Returns the current cursor byte offset as a `u32`.
    fn current_offset(&self) -> u32 {
        u32::try_from(self.cursor.offset()).unwrap_or(u32::MAX)
    }

    /// Builds a span from `start` to the cursor's current offset.
    fn span_from(&self, start: u32) -> Span {
        Span::new(self.file, start, self.current_offset())
    }
}

/// Returns `true` when `character` is ASCII whitespace.
fn is_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\r' | '\n')
}

/// Returns `true` when `character` may start an identifier. Follows
/// UAX #31 `XID_Start` plus `_`, matching Rust 2024's identifier
/// surface. Lets user code name bindings with letters from any
/// script (e.g. `let café = 1`, `let π = 3.14159`, `let 名前 = "x"`).
fn is_ident_start(character: char) -> bool {
    character == '_' || unicode_ident::is_xid_start(character)
}

/// Returns `true` when `character` may continue an identifier.
/// Follows UAX #31 `XID_Continue`.
fn is_ident_continue(character: char) -> bool {
    unicode_ident::is_xid_continue(character)
}

/// Convenience helper: collect every token of `source` into a `Vec`.
#[must_use]
pub fn tokenize(source: &str, file: FileId) -> (Vec<Token>, Vec<LexError>) {
    let mut lexer = Lexer::new(source, file);
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token();
        let done = token.kind == TokenKind::Eof;
        tokens.push(token);
        if done {
            break;
        }
    }
    let diagnostics = lexer.take_diagnostics();
    (tokens, diagnostics)
}
