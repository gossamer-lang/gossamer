//! Runtime support for `std::regex`.
//!
//! Wraps the upstream `regex` crate so Gossamer programs get a
//! first-party regex engine without pulling a whole C library.
//! The Gossamer-side API is intentionally minimal - `compile`,
//! `is_match`, `find`, `find_all`, `captures`, `replace`,
//! `replace_all` - matching the surface a typical script actually
//! uses. Pattern syntax is Rust regex syntax (PCRE-like subset;
//! no backreferences or look-around, Unicode-aware character
//! classes).

#![forbid(unsafe_code)]

use regex::Regex;

/// Opaque compiled-pattern handle. The Gossamer-facing value keeps
/// the original pattern string alongside the compiled engine so it
/// can be rendered for diagnostics and cloned cheaply by the
/// interpreter.
#[derive(Debug, Clone)]
pub struct Pattern {
    pattern: String,
    engine: Regex,
}

impl Pattern {
    /// Original pattern source as passed to [`compile`].
    #[must_use]
    pub fn source(&self) -> &str {
        &self.pattern
    }
}

/// Error shape for regex operations.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RegexError {
    /// Pattern failed to parse.
    #[error("regex: invalid pattern `{pattern}`: {reason}")]
    InvalidPattern {
        /// Original pattern text.
        pattern: String,
        /// Human-readable reason from the upstream parser.
        reason: String,
    },
    /// Caller asked for a capture group that does not exist.
    #[error("regex: no such capture group {index}")]
    NoSuchGroup {
        /// Requested group index.
        index: usize,
    },
}

/// Compiles `pattern` into a reusable [`Pattern`].
pub fn compile(pattern: &str) -> Result<Pattern, RegexError> {
    Regex::new(pattern)
        .map(|engine| Pattern {
            pattern: pattern.to_string(),
            engine,
        })
        .map_err(|err| RegexError::InvalidPattern {
            pattern: pattern.to_string(),
            reason: err.to_string(),
        })
}

/// Returns whether the pattern matches anywhere in `text`.
#[must_use]
pub fn is_match(pattern: &Pattern, text: &str) -> bool {
    pattern.engine.is_match(text)
}

/// Returns the first match as `(start_byte, end_byte, matched_text)`.
#[must_use]
pub fn find(pattern: &Pattern, text: &str) -> Option<(usize, usize, String)> {
    pattern
        .engine
        .find(text)
        .map(|m| (m.start(), m.end(), m.as_str().to_string()))
}

/// Counts the non-overlapping matches in `text` without materialising them.
#[must_use]
pub fn count(pattern: &Pattern, text: &str) -> usize {
    pattern.engine.find_iter(text).count()
}

/// Returns every non-overlapping match as `(start, end, text)`.
#[must_use]
pub fn find_all(pattern: &Pattern, text: &str) -> Vec<(usize, usize, String)> {
    pattern
        .engine
        .find_iter(text)
        .map(|m| (m.start(), m.end(), m.as_str().to_string()))
        .collect()
}

/// Returns the first set of capture groups as a vector of
/// `Option<String>`s. Index 0 is the full match; indices 1..N are
/// the parenthesised groups. An unmatched optional group yields
/// `None`.
#[must_use]
pub fn captures(pattern: &Pattern, text: &str) -> Option<Vec<Option<String>>> {
    pattern.engine.captures(text).map(|caps| {
        caps.iter()
            .map(|m| m.map(|mat| mat.as_str().to_string()))
            .collect()
    })
}

/// Returns every set of capture groups in `text`.
#[must_use]
pub fn captures_all(pattern: &Pattern, text: &str) -> Vec<Vec<Option<String>>> {
    pattern
        .engine
        .captures_iter(text)
        .map(|caps| {
            caps.iter()
                .map(|m| m.map(|mat| mat.as_str().to_string()))
                .collect()
        })
        .collect()
}

/// Replaces the first match with `replacement`. Supports `$N`
/// group references (delegates to the upstream engine).
#[must_use]
pub fn replace(pattern: &Pattern, text: &str, replacement: &str) -> String {
    pattern.engine.replace(text, replacement).into_owned()
}

/// Replaces every non-overlapping match with `replacement`.
#[must_use]
pub fn replace_all(pattern: &Pattern, text: &str, replacement: &str) -> String {
    pattern.engine.replace_all(text, replacement).into_owned()
}

/// Splits `text` on every match of `pattern`, returning the
/// intervening segments.
#[must_use]
pub fn split(pattern: &Pattern, text: &str) -> Vec<String> {
    pattern.engine.split(text).map(str::to_string).collect()
}

/// Returns every named capture group in `pattern`, in declaration
/// order. Anonymous groups (no `?P<name>` prefix) are absent.
/// Mirrors Python's `re.Pattern.groupindex` keys.
#[must_use]
pub fn capture_names(pattern: &Pattern) -> Vec<String> {
    pattern
        .engine
        .capture_names()
        .flatten()
        .map(str::to_string)
        .collect()
}

/// Returns the first set of capture groups as a `{name → value}`
/// map. Only named groups appear in the map; unnamed positional
/// groups are accessible via [`captures`]. Mirrors Python's
/// `re.Match.groupdict()`.
#[must_use]
pub fn captures_named(
    pattern: &Pattern,
    text: &str,
) -> Option<std::collections::HashMap<String, String>> {
    let caps = pattern.engine.captures(text)?;
    let mut out = std::collections::HashMap::new();
    for name in pattern.engine.capture_names().flatten() {
        if let Some(m) = caps.name(name) {
            out.insert(name.to_string(), m.as_str().to_string());
        }
    }
    Some(out)
}

/// Returns every match of `pattern` as a `{name → value}` map.
/// Lazy callers should pair this with `iter::take`/`iter::collect`;
/// the implementation returns a `Vec` to stay consistent with
/// [`captures_all`].
#[must_use]
pub fn captures_named_all(
    pattern: &Pattern,
    text: &str,
) -> Vec<std::collections::HashMap<String, String>> {
    let names: Vec<&str> = pattern.engine.capture_names().flatten().collect();
    pattern
        .engine
        .captures_iter(text)
        .map(|caps| {
            let mut out = std::collections::HashMap::new();
            for name in &names {
                if let Some(m) = caps.name(name) {
                    out.insert((*name).to_string(), m.as_str().to_string());
                }
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_rejects_invalid_pattern_with_context() {
        let err = compile("(unclosed").unwrap_err();
        let RegexError::InvalidPattern { pattern, .. } = err else {
            panic!("expected InvalidPattern");
        };
        assert_eq!(pattern, "(unclosed");
    }

    #[test]
    fn is_match_finds_pattern_anywhere_in_the_haystack() {
        let re = compile(r"\d+").unwrap();
        assert!(is_match(&re, "port 8080 open"));
        assert!(!is_match(&re, "no digits here"));
    }

    #[test]
    fn find_returns_byte_offsets_and_matched_text() {
        let re = compile(r"\d+").unwrap();
        let (start, end, text) = find(&re, "port 8080 open").unwrap();
        assert_eq!((start, end, text.as_str()), (5, 9, "8080"));
    }

    #[test]
    fn find_all_collects_every_non_overlapping_match() {
        let re = compile(r"\d+").unwrap();
        let hits = find_all(&re, "1, 22, 333");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].2, "1");
        assert_eq!(hits[2].2, "333");
    }

    #[test]
    fn unicode_classes_case_folding_and_offsets_are_explicit() {
        let letters = compile(r"(?iu)\p{Greek}+").unwrap();
        assert!(is_match(&letters, "prefix ΔέΛΤΑ suffix"));

        let words = compile(r"\w+").unwrap();
        let hits = find_all(&words, "café 東京");
        assert_eq!(
            hits.iter().map(|hit| hit.2.as_str()).collect::<Vec<_>>(),
            ["café", "東京"]
        );

        let accent = compile("é").unwrap();
        assert_eq!(find(&accent, "aé").unwrap(), (1, 3, "é".to_string()));
    }

    #[test]
    fn captures_returns_none_for_unmatched_optional_groups() {
        let re = compile(r"(\w+)=(\d+)?").unwrap();
        let caps = captures(&re, "port=").unwrap();
        assert_eq!(caps[0].as_deref(), Some("port="));
        assert_eq!(caps[1].as_deref(), Some("port"));
        assert_eq!(caps[2], None);
    }

    #[test]
    fn captures_all_yields_one_entry_per_match() {
        let re = compile(r"(\w+)=(\d+)").unwrap();
        let rows = captures_all(&re, "a=1 b=22 c=333");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][1].as_deref(), Some("b"));
        assert_eq!(rows[1][2].as_deref(), Some("22"));
    }

    #[test]
    fn replace_and_replace_all_honour_group_references() {
        let re = compile(r"(\w+)=(\d+)").unwrap();
        assert_eq!(
            replace(&re, "port=80 host=443", "$2:$1"),
            "80:port host=443"
        );
        assert_eq!(
            replace_all(&re, "port=80 host=443", "$2:$1"),
            "80:port 443:host"
        );
    }

    #[test]
    fn split_cuts_on_every_match() {
        let re = compile(r"\s*,\s*").unwrap();
        assert_eq!(
            split(&re, "a,  b , c,d"),
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string()
            ]
        );
    }
}
