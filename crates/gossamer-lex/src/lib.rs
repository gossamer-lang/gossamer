//! Lexer for the Gossamer language.
//! Converts a UTF-8 source string into a stream of `Token` values with
//! precise byte-range `Span`s and recoverable diagnostics. Populated as
//! part of of the implementation plan.

#![forbid(unsafe_code)]

mod comment;
mod cursor;
mod diagnostic;
mod interpolation;
mod lexer;
mod number;
mod punct;
mod source_map;
mod span;
mod string;
mod symbol;
mod token;

pub use diagnostic::LexError;
pub use interpolation::{InterpolationPiece, Placeholder, interpolation_pieces, scan_placeholder};
pub use lexer::{Lexer, tokenize};
pub use source_map::{OriginSpan, SourceMap};
pub use span::{FileId, LineCol, Span};
pub use string::{TripleString, triple_string};
pub use symbol::{Symbol, SymbolInterner, reset_interner};
pub use token::{Keyword, Punct, Token, TokenKind};

#[cfg(test)]
mod fuzz_regressions {
    use super::*;

    const SLOW_UNTERMINATED_ESCAPES: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fuzz/artifacts/lex/slow-unit-98933ced9c46a1b83be6b11010757e1d749042ed"
    ));
    const SLOW_UNTERMINATED_STRING: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fuzz/artifacts/lex/slow-unit-9544eea0e361237c2e1c66926039c4e00540c090"
    ));
    const SLOW_MIXED_UNIT: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fuzz/artifacts/lex/slow-unit-3b4f9c4a30cacc63bab2fe0cd2f84f6372393793"
    ));

    fn tokenize_retained(name: &str, bytes: &[u8]) {
        let source = std::str::from_utf8(bytes).expect("retained lexer artifact is UTF-8");
        let mut map = SourceMap::new();
        let file = map.add_file(name, source.to_owned());
        let _ = tokenize(source, file);
    }

    #[test]
    fn retained_lexer_slow_unit_does_not_panic() {
        tokenize_retained("slow-unterminated-escapes.gos", SLOW_UNTERMINATED_ESCAPES);
    }

    #[test]
    fn retained_lexer_unterminated_string_does_not_panic() {
        tokenize_retained("slow-unterminated-string.gos", SLOW_UNTERMINATED_STRING);
    }

    #[test]
    fn retained_lexer_mixed_unit_does_not_panic() {
        tokenize_retained("slow-mixed-unit.gos", SLOW_MIXED_UNIT);
    }
}
