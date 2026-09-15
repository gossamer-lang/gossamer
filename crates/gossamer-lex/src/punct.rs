//! Punctuation and operator tokenization.

use crate::cursor::Cursor;
use crate::token::Punct;

/// Attempts to lex a punctuation token at the current cursor.
///
/// Uses longest-match dispatch: `|>` beats `|`, `..=` beats `..`, and
/// so on. Returns `None` if the current character does not start any
/// known punctuation token; the cursor is unchanged in that case.
pub(crate) fn lex_punct(cursor: &mut Cursor<'_>) -> Option<Punct> {
    let first = cursor.peek();
    let second = cursor.peek_nth(1);
    let third = cursor.peek_nth(2);
    let (punct, consumed) = classify(first, second, third)?;
    for _ in 0..consumed {
        cursor.bump();
    }
    Some(punct)
}

/// Returns the punctuation token and how many characters it consumes
/// for a three-character lookahead window, or `None` when no token
/// matches.
fn classify(first: char, second: char, third: char) -> Option<(Punct, usize)> {
    if let Some(hit) = classify_three(first, second, third) {
        return Some(hit);
    }
    if let Some(hit) = classify_two(first, second) {
        return Some(hit);
    }
    classify_one(first).map(|punct| (punct, 1))
}

/// Matches the three-character operators. Returns `None` if none apply.
fn classify_three(first: char, second: char, third: char) -> Option<(Punct, usize)> {
    let hit = match (first, second, third) {
        ('<', '<', '=') => Punct::ShiftLEq,
        ('>', '>', '=') => Punct::ShiftREq,
        ('.', '.', '=') => Punct::DotDotEq,
        ('.', '.', '.') => Punct::DotDotDot,
        ('+', '%', '=') => Punct::PlusPercentEq,
        ('-', '%', '=') => Punct::MinusPercentEq,
        ('*', '%', '=') => Punct::StarPercentEq,
        _ => return None,
    };
    Some((hit, 3))
}

/// Matches the two-character operators. Returns `None` if none apply.
fn classify_two(first: char, second: char) -> Option<(Punct, usize)> {
    let hit = match (first, second) {
        ('+', '=') => Punct::PlusEq,
        ('+', '%') => Punct::PlusPercent,
        ('-', '%') => Punct::MinusPercent,
        ('*', '%') => Punct::StarPercent,
        ('-', '=') => Punct::MinusEq,
        ('-', '>') => Punct::Arrow,
        ('*', '=') => Punct::StarEq,
        ('/', '=') => Punct::SlashEq,
        ('%', '=') => Punct::PercentEq,
        ('&', '=') => Punct::AmpEq,
        ('&', '&') => Punct::AmpAmp,
        ('|', '=') => Punct::PipeEq,
        ('|', '|') => Punct::PipePipe,
        ('|', '>') => Punct::PipeGt,
        ('^', '=') => Punct::CaretEq,
        ('<', '<') => Punct::ShiftL,
        ('<', '=') => Punct::LtEq,
        ('>', '>') => Punct::ShiftR,
        ('>', '=') => Punct::GtEq,
        ('=', '=') => Punct::EqEq,
        ('=', '>') => Punct::FatArrow,
        ('!', '=') => Punct::NotEq,
        ('.', '.') => Punct::DotDot,
        (':', ':') => Punct::ColonColon,
        _ => return None,
    };
    Some((hit, 2))
}

/// Matches the single-character punctuation tokens.
fn classify_one(first: char) -> Option<Punct> {
    Some(match first {
        '+' => Punct::Plus,
        '-' => Punct::Minus,
        '*' => Punct::Star,
        '/' => Punct::Slash,
        '%' => Punct::Percent,
        '&' => Punct::Amp,
        '|' => Punct::Pipe,
        '^' => Punct::Caret,
        '=' => Punct::Eq,
        '<' => Punct::Lt,
        '>' => Punct::Gt,
        '!' => Punct::Bang,
        '.' => Punct::Dot,
        ':' => Punct::Colon,
        '(' => Punct::LParen,
        ')' => Punct::RParen,
        '[' => Punct::LBracket,
        ']' => Punct::RBracket,
        '{' => Punct::LBrace,
        '}' => Punct::RBrace,
        ',' => Punct::Comma,
        ';' => Punct::Semi,
        '?' => Punct::Question,
        '#' => Punct::Hash,
        '@' => Punct::At,
        '$' => Punct::Dollar,
        _ => return None,
    })
}
