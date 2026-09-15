//! Token kinds emitted by the lexer.

use crate::span::Span;

/// A lexical token: a kind paired with the source span it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// Classifies which terminal symbol this token represents.
    pub kind: TokenKind,
    /// Byte range of this token in its source file.
    pub span: Span,
}

impl Token {
    /// Returns a new token with the given kind and span.
    #[must_use]
    pub const fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// Classification of a single token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A reserved keyword.
    Keyword(Keyword),
    /// A plain identifier (non-keyword).
    Ident,
    /// An operator or punctuation token.
    Punct(Punct),
    /// A decimal, binary, octal, or hexadecimal integer literal.
    IntLit,
    /// A floating-point literal.
    FloatLit,
    /// A double-quoted string literal with escape sequences.
    StringLit,
    /// A triple-quoted string literal `"""..."""`, whose body has the
    /// indentation it shares with its closing delimiter removed.
    TripleStringLit,
    /// A raw string literal `r"..."` with `hashes` surrounding `#` characters.
    RawStringLit {
        /// Number of `#` characters flanking the raw string delimiters.
        hashes: u8,
    },
    /// A single-quoted character literal.
    CharLit,
    /// A byte literal `b'x'`.
    ByteLit,
    /// A loop label `'name` (the leading apostrophe is included in the
    /// span). Distinguished from a char literal by the lexer: a `'`
    /// followed by an identifier whose character run does not terminate
    /// at a closing `'` is a label.
    Label,
    /// A byte string literal `b"..."`.
    ByteStringLit,
    /// A raw byte string literal `br"..."` with `hashes` surrounding `#` characters.
    RawByteStringLit {
        /// Number of `#` characters flanking the raw byte string delimiters.
        hashes: u8,
    },
    /// A line comment introduced with `//`.
    LineComment,
    /// A block comment `/* ... */`.
    BlockComment,
    /// A run of ASCII whitespace (spaces, tabs, carriage returns, newlines).
    Whitespace,
    /// A byte sequence the lexer could not classify. Accompanied by a diagnostic.
    Invalid,
    /// End of the input stream.
    Eof,
}

/// Every reserved keyword recognised by the lexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(
    missing_docs,
    reason = "each variant is its own spelling and a doc line would only repeat it"
)]
pub enum Keyword {
    As,
    Async,
    Await,
    Break,
    Const,
    Continue,
    Crate,
    Else,
    Enum,
    Extern,
    False,
    Fn,
    For,
    If,
    Impl,
    In,
    Let,
    Loop,
    Match,
    Mod,
    Mut,
    Package,
    Pub,
    Return,
    SelfLower,
    SelfUpper,
    Static,
    Struct,
    Super,
    Trait,
    True,
    Type,
    Unsafe,
    Use,
    Where,
    While,
    Yield,
}

impl Keyword {
    /// Returns the keyword matching `ident`, or `None` if it is not reserved.
    #[must_use]
    pub fn from_ident(ident: &str) -> Option<Self> {
        Some(match ident {
            "as" => Self::As,
            "async" => Self::Async,
            "await" => Self::Await,
            "break" => Self::Break,
            "const" => Self::Const,
            "continue" => Self::Continue,
            "crate" => Self::Crate,
            "else" => Self::Else,
            "enum" => Self::Enum,
            "extern" => Self::Extern,
            "false" => Self::False,
            "fn" => Self::Fn,
            "for" => Self::For,
            "if" => Self::If,
            "impl" => Self::Impl,
            "in" => Self::In,
            "let" => Self::Let,
            "loop" => Self::Loop,
            "match" => Self::Match,
            "mod" => Self::Mod,
            "mut" => Self::Mut,
            "package" => Self::Package,
            "pub" => Self::Pub,
            "return" => Self::Return,
            "self" => Self::SelfLower,
            "Self" => Self::SelfUpper,
            "static" => Self::Static,
            "struct" => Self::Struct,
            "super" => Self::Super,
            "trait" => Self::Trait,
            "true" => Self::True,
            "type" => Self::Type,
            "unsafe" => Self::Unsafe,
            "use" => Self::Use,
            "where" => Self::Where,
            "while" => Self::While,
            "yield" => Self::Yield,
            _ => return None,
        })
    }

    /// Returns the canonical source spelling of this keyword.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::As => "as",
            Self::Async => "async",
            Self::Await => "await",
            Self::Break => "break",
            Self::Const => "const",
            Self::Continue => "continue",
            Self::Crate => "crate",
            Self::Else => "else",
            Self::Enum => "enum",
            Self::Extern => "extern",
            Self::False => "false",
            Self::Fn => "fn",
            Self::For => "for",
            Self::If => "if",
            Self::Impl => "impl",
            Self::In => "in",
            Self::Let => "let",
            Self::Loop => "loop",
            Self::Match => "match",
            Self::Mod => "mod",
            Self::Mut => "mut",
            Self::Package => "package",
            Self::Pub => "pub",
            Self::Return => "return",
            Self::SelfLower => "self",
            Self::SelfUpper => "Self",
            Self::Static => "static",
            Self::Struct => "struct",
            Self::Super => "super",
            Self::Trait => "trait",
            Self::True => "true",
            Self::Type => "type",
            Self::Unsafe => "unsafe",
            Self::Use => "use",
            Self::Where => "where",
            Self::While => "while",
            Self::Yield => "yield",
        }
    }
}

/// Every operator or punctuation token recognised by the lexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(
    missing_docs,
    reason = "each variant is its own spelling and a doc line would only repeat it"
)]
pub enum Punct {
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    ShiftL,
    ShiftR,
    PlusPercent,
    MinusPercent,
    StarPercent,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShiftLEq,
    ShiftREq,
    PlusPercentEq,
    MinusPercentEq,
    StarPercentEq,
    Eq,
    EqEq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Bang,
    AmpAmp,
    PipePipe,
    PipeGt,
    Dot,
    DotDot,
    DotDotEq,
    DotDotDot,
    ColonColon,
    Arrow,
    FatArrow,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Semi,
    Colon,
    Question,
    Hash,
    At,
    Dollar,
}

impl Punct {
    /// Returns the canonical source spelling of this punctuation token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Star => "*",
            Self::Slash => "/",
            Self::Percent => "%",
            Self::Amp => "&",
            Self::Pipe => "|",
            Self::Caret => "^",
            Self::ShiftL => "<<",
            Self::ShiftR => ">>",
            Self::PlusPercent => "+%",
            Self::MinusPercent => "-%",
            Self::StarPercent => "*%",
            Self::PlusEq => "+=",
            Self::MinusEq => "-=",
            Self::StarEq => "*=",
            Self::SlashEq => "/=",
            Self::PercentEq => "%=",
            Self::AmpEq => "&=",
            Self::PipeEq => "|=",
            Self::CaretEq => "^=",
            Self::ShiftLEq => "<<=",
            Self::ShiftREq => ">>=",
            Self::PlusPercentEq => "+%=",
            Self::MinusPercentEq => "-%=",
            Self::StarPercentEq => "*%=",
            Self::Eq => "=",
            Self::EqEq => "==",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::Bang => "!",
            Self::AmpAmp => "&&",
            Self::PipePipe => "||",
            Self::PipeGt => "|>",
            Self::Dot => ".",
            Self::DotDot => "..",
            Self::DotDotEq => "..=",
            Self::DotDotDot => "...",
            Self::ColonColon => "::",
            Self::Arrow => "->",
            Self::FatArrow => "=>",
            Self::LParen => "(",
            Self::RParen => ")",
            Self::LBracket => "[",
            Self::RBracket => "]",
            Self::LBrace => "{",
            Self::RBrace => "}",
            Self::Comma => ",",
            Self::Semi => ";",
            Self::Colon => ":",
            Self::Question => "?",
            Self::Hash => "#",
            Self::At => "@",
            Self::Dollar => "$",
        }
    }
}
