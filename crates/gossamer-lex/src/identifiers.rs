//! Identifiers that read as something they are not (Unicode UTS #39).
//!
//! A name that mixes scripts (`is_аdmin`, with a Cyrillic `а`) or whose
//! characters look like another name in the same file renders as that other
//! name in an editor and a review tool, so the code a reader checks is not
//! the code that runs.

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;
use unicode_security::MixedScript;

use crate::{LexError, Span, Token, TokenKind};

/// Whether `text`, an identifier, draws its letters from more than one
/// script. ASCII text and the scripts that are written together (Han with
/// Hiragana and Katakana, Han with Hangul) count as one.
#[must_use]
pub fn mixes_scripts(text: &str) -> bool {
    !text.is_ascii() && !text.is_single_script()
}

/// The refusal of each identifier among `tokens` that looks like a different
/// identifier of the same file: the two share a UTS #39 confusable
/// skeleton. Only a name with a non-ASCII character is refused, since two
/// ASCII names never render the same.
#[must_use]
pub fn confusable_identifiers(source: &str, tokens: &[Token]) -> Vec<LexError> {
    let text_of = |token: &Token| source.get(token.span.start as usize..token.span.end as usize);
    // Two ASCII names never render alike, so a file with no other kind of
    // name has nothing to compare.
    let any_non_ascii = tokens.iter().any(|token| {
        token.kind == TokenKind::Ident && text_of(token).is_some_and(|t| !t.is_ascii())
    });
    if !any_non_ascii {
        return Vec::new();
    }
    let mut first_by_skeleton: HashMap<String, Vec<(&str, Span)>> = HashMap::new();
    for token in tokens {
        if token.kind != TokenKind::Ident {
            continue;
        }
        let Some(text) = source.get(token.span.start as usize..token.span.end as usize) else {
            continue;
        };
        let names = first_by_skeleton.entry(skeleton(text)).or_default();
        if !names.iter().any(|(name, _)| *name == text) {
            names.push((text, token.span));
        }
    }
    let mut out = Vec::new();
    for names in first_by_skeleton.values() {
        if names.len() < 2 {
            continue;
        }
        let Some(&(_, other)) = names
            .iter()
            .find(|(name, _)| name.is_ascii())
            .or_else(|| names.first())
        else {
            continue;
        };
        for &(name, span) in names {
            if !name.is_ascii() && span != other {
                out.push(LexError::ConfusableIdent { span, other });
            }
        }
    }
    out.sort_by_key(|err| err.span().start);
    out
}

include!(concat!(env!("OUT_DIR"), "/confusables.rs"));

/// The UTS #39 skeleton of `text`: its decomposition, each scalar replaced
/// by the prototype it renders like, decomposed again.
fn skeleton(text: &str) -> String {
    let mut mapped = String::with_capacity(text.len());
    for c in text.nfd() {
        match KEYS.binary_search(&u32::from(c)) {
            Ok(at) => {
                let (start, end) = (OFFSETS[at] as usize, OFFSETS[at + 1] as usize);
                mapped.push_str(&PROTOTYPES[start..end]);
            }
            Err(_) => mapped.push(c),
        }
    }
    mapped.nfd().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileId, tokenize};

    #[test]
    fn a_name_mixing_latin_and_cyrillic_mixes_scripts() {
        assert!(mixes_scripts("is_\u{430}dmin"));
        assert!(!mixes_scripts("is_admin"));
        assert!(!mixes_scripts("\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}"));
        assert!(!mixes_scripts("\u{65e5}\u{672c}\u{3054}"));
    }

    #[test]
    fn a_name_that_looks_like_another_is_confusable() {
        let source = "fn is_admin() {}\nfn is_\u{430}dmin() {}\nfn other() {}\n";
        let (tokens, _) = tokenize(source, FileId(0));
        let found = confusable_identifiers(source, &tokens);
        assert_eq!(found.len(), 1, "{found:?}");
        let LexError::ConfusableIdent { span, other } = found[0] else {
            panic!("{found:?}");
        };
        assert_eq!(
            &source[span.start as usize..span.end as usize],
            "is_\u{430}dmin"
        );
        assert_eq!(
            &source[other.start as usize..other.end as usize],
            "is_admin"
        );
    }

    #[test]
    fn the_flat_table_answers_the_skeleton_unicode_security_computes() {
        for text in [
            "is_\u{430}dmin",
            "\u{3b1}\u{3b2}",
            "na\u{ef}ve",
            "\u{fb01}le",
            "a\u{200d}b",
            "x\u{fe0f}",
            "\u{212b}",
        ] {
            let expected: String = unicode_security::skeleton(text).collect();
            assert_eq!(skeleton(text), expected, "{text}");
        }
    }

    #[test]
    fn distinct_non_latin_names_are_not_confusable() {
        let source = "let \u{3b1} = 1\nlet \u{3b2} = \u{3b1}\n";
        let (tokens, _) = tokenize(source, FileId(0));
        assert!(confusable_identifiers(source, &tokens).is_empty());
    }
}
