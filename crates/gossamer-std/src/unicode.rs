//! Runtime support for `std::unicode` - Unicode general-category
//! predicates, casing operations, normalization forms, and
//! grapheme / word / sentence segmentation.
//!
//! Properties come from the Unicode 16 tables shipped by the
//! `unicode-properties`, `unicode-normalization`, and
//! `unicode-segmentation` crates. The earlier hand-rolled ASCII /
//! BMP-range stubs are gone - every predicate now answers against
//! the real general-category data so user code that asks
//! `is_digit('٧')` (Arabic-Indic seven, U+0667), or whether an
//! em dash (U+2014) is punctuation, gets the right answer.

#![forbid(unsafe_code)]

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;
use unicode_properties::{GeneralCategory, GeneralCategoryGroup, UnicodeGeneralCategory};
use unicode_segmentation::UnicodeSegmentation;

// ---------------------------------------------------------------------------
// General-category predicates
// ---------------------------------------------------------------------------

/// Returns `true` if `r` is in Unicode general-category group L (any
/// letter - Lu, Ll, Lt, Lm, Lo).
#[must_use]
pub fn is_letter(r: char) -> bool {
    matches!(r.general_category_group(), GeneralCategoryGroup::Letter)
}

/// Returns `true` if `r` is in category Nd (decimal digit) - covers
/// ASCII `0`..`9`, Arabic-Indic `٠`..`٩`, Devanagari `०`..`९`, etc.
#[must_use]
pub fn is_digit(r: char) -> bool {
    matches!(r.general_category(), GeneralCategory::DecimalNumber)
}

/// Returns `true` if `r` is in general-category group N (any numeric
/// character - Nd decimal, Nl letter-number, No other-number).
#[must_use]
pub fn is_number(r: char) -> bool {
    matches!(r.general_category_group(), GeneralCategoryGroup::Number)
}

/// Returns `true` if `r` is Unicode whitespace - group Z (separator)
/// plus the ASCII whitespace control characters HT / LF / VT / FF / CR
/// and NEL (U+0085). Matches Go's `unicode.IsSpace`.
#[must_use]
pub fn is_space(r: char) -> bool {
    if matches!(r.general_category_group(), GeneralCategoryGroup::Separator) {
        return true;
    }
    matches!(r, '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | '\u{0085}')
}

/// Returns `true` if `r` is an uppercase letter (category Lu).
#[must_use]
pub fn is_upper(r: char) -> bool {
    matches!(r.general_category(), GeneralCategory::UppercaseLetter)
}

/// Returns `true` if `r` is a lowercase letter (category Ll).
#[must_use]
pub fn is_lower(r: char) -> bool {
    matches!(r.general_category(), GeneralCategory::LowercaseLetter)
}

/// Returns `true` if `r` is a titlecase letter (category Lt).
/// Covers digraphs like `ǅ` U+01C5, `ǈ` U+01C8, `ǋ` U+01CB.
#[must_use]
pub fn is_title(r: char) -> bool {
    matches!(r.general_category(), GeneralCategory::TitlecaseLetter)
}

/// Returns `true` if `r` is in general-category group P - any
/// punctuation (Pc, Pd, Ps, Pe, Pi, Pf, Po).
#[must_use]
pub fn is_punct(r: char) -> bool {
    matches!(
        r.general_category_group(),
        GeneralCategoryGroup::Punctuation
    )
}

/// Returns `true` if `r` is in general-category group S - any
/// symbol (Sm math, Sc currency, Sk modifier, So other).
#[must_use]
pub fn is_symbol(r: char) -> bool {
    matches!(r.general_category_group(), GeneralCategoryGroup::Symbol)
}

/// Returns `true` if `r` is in general-category group M - any
/// combining mark (Mn nonspacing, Mc spacing-combining, Me enclosing).
#[must_use]
pub fn is_mark(r: char) -> bool {
    matches!(r.general_category_group(), GeneralCategoryGroup::Mark)
}

/// Returns `true` if `r` is a printable Unicode character - not a
/// control, format char, surrogate, private-use, or unassigned
/// code point. Matches Go's `unicode.IsPrint` semantics: the
/// general-category group `Other` (Cc/Cf/Cs/Co/Cn) is excluded.
#[must_use]
pub fn is_print(r: char) -> bool {
    !matches!(r.general_category_group(), GeneralCategoryGroup::Other)
}

/// Returns `true` if `r` is a graphic character - printable and not
/// a whitespace separator.
#[must_use]
pub fn is_graphic(r: char) -> bool {
    is_print(r) && !is_space(r)
}

/// Returns `true` if `r` is a Unicode control character (category Cc).
#[must_use]
pub fn is_control(r: char) -> bool {
    matches!(r.general_category(), GeneralCategory::Control)
}

/// Returns `true` if `r` is an assigned (not `Cn`) Unicode code point.
#[must_use]
pub fn is_assigned(r: char) -> bool {
    !matches!(r.general_category(), GeneralCategory::Unassigned)
}

/// Terminal columns `r` occupies: 2 for East Asian wide and fullwidth
/// characters, 0 for combining marks, zero-width, and control characters,
/// otherwise 1.
#[must_use]
pub fn char_width(r: char) -> i64 {
    unicode_width::UnicodeWidthChar::width(r).map_or(0, |w| w as i64)
}

/// Terminal columns `s` occupies, reading emoji and variation sequences as
/// the clusters a terminal draws.
#[must_use]
pub fn str_width(s: &str) -> i64 {
    unicode_width::UnicodeWidthStr::width(s) as i64
}

/// Returns the canonical combining class for `r` (0-254). Used by
/// callers that implement custom normalization passes.
#[must_use]
pub fn combining_class(r: char) -> i64 {
    i64::from(canonical_combining_class(r))
}

// ---------------------------------------------------------------------------
// Casing
// ---------------------------------------------------------------------------

/// Maps `r` to its Unicode simple uppercase equivalent, or returns
/// `r` unchanged if it has no uppercase mapping. Note: this returns
/// a single rune. For locale-faithful full casing of strings
/// (`ß` → `SS`, Turkish dotted/dotless I), use `to_upper_str`.
#[must_use]
pub fn to_upper(r: char) -> char {
    r.to_uppercase().next().unwrap_or(r)
}

/// Maps `r` to its Unicode simple lowercase equivalent.
#[must_use]
pub fn to_lower(r: char) -> char {
    r.to_lowercase().next().unwrap_or(r)
}

/// Maps `r` to its Unicode titlecase equivalent.
#[must_use]
pub fn to_title(r: char) -> char {
    // Rust's stdlib has no per-char titlecase; uppercase is the
    // closest standard mapping (titlecase letters like `ǅ` are
    // their own uppercase equivalent already).
    to_upper(r)
}

/// Returns the next rune in the Unicode simple case-folding cycle
/// that follows `r`. Mirrors Go's `unicode.SimpleFold` shape.
#[must_use]
pub fn simple_fold(r: char) -> char {
    if r.is_lowercase() {
        let up = to_upper(r);
        if up != r {
            return up;
        }
    } else if r.is_uppercase() {
        let lo = to_lower(r);
        if lo != r {
            return lo;
        }
    }
    r
}

/// Returns the Unicode full lowercase mapping of `s`. Differs from
/// `s.chars().map(to_lower).collect()` because some runes expand
/// (German `ß` does NOT lowercase, but `İ` lowercases to two
/// runes `i\u{307}`).
#[must_use]
pub fn to_lower_str(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// Returns the Unicode full uppercase mapping of `s`. `ß`
/// uppercases to `SS`, fi ligature uppercases to `FI`, etc.
#[must_use]
pub fn to_upper_str(s: &str) -> String {
    s.chars().flat_map(char::to_uppercase).collect()
}

/// Returns the Unicode simple-case-folded form of `s` - the
/// canonical comparison form for case-insensitive equality. Built
/// by composing the simple lower mapping rune-by-rune.
#[must_use]
pub fn fold_case(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

// ---------------------------------------------------------------------------
// Normalization (NFC / NFD / NFKC / NFKD)
// ---------------------------------------------------------------------------

/// Returns the Normalization Form C (canonical composition) of `s`.
#[must_use]
pub fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// Returns the Normalization Form D (canonical decomposition) of `s`.
#[must_use]
pub fn nfd(s: &str) -> String {
    s.nfd().collect()
}

/// Returns the Normalization Form KC (compatibility composition) of `s`.
#[must_use]
pub fn nfkc(s: &str) -> String {
    s.nfkc().collect()
}

/// Returns the Normalization Form KD (compatibility decomposition) of `s`.
#[must_use]
pub fn nfkd(s: &str) -> String {
    s.nfkd().collect()
}

/// Returns `true` if `s` is already in NFC.
#[must_use]
pub fn is_nfc(s: &str) -> bool {
    matches!(
        unicode_normalization::is_nfc_quick(s.chars()),
        unicode_normalization::IsNormalized::Yes
    ) || s.chars().eq(s.nfc())
}

/// Returns `true` if `s` is already in NFD.
#[must_use]
pub fn is_nfd(s: &str) -> bool {
    matches!(
        unicode_normalization::is_nfd_quick(s.chars()),
        unicode_normalization::IsNormalized::Yes
    ) || s.chars().eq(s.nfd())
}

/// Returns `true` if `s` is already in NFKC.
#[must_use]
pub fn is_nfkc(s: &str) -> bool {
    matches!(
        unicode_normalization::is_nfkc_quick(s.chars()),
        unicode_normalization::IsNormalized::Yes
    ) || s.chars().eq(s.nfkc())
}

/// Returns `true` if `s` is already in NFKD.
#[must_use]
pub fn is_nfkd(s: &str) -> bool {
    matches!(
        unicode_normalization::is_nfkd_quick(s.chars()),
        unicode_normalization::IsNormalized::Yes
    ) || s.chars().eq(s.nfkd())
}

// ---------------------------------------------------------------------------
// Segmentation (graphemes / words / sentences)
// ---------------------------------------------------------------------------

/// Returns the extended grapheme clusters in `s` (UAX #29) as a
/// vector of owned strings. A grapheme is the user-perceived
/// "character" - `👨‍👩‍👧` is one grapheme, even though it spans
/// several Unicode scalars.
#[must_use]
pub fn graphemes(s: &str) -> Vec<String> {
    UnicodeSegmentation::graphemes(s, true)
        .map(String::from)
        .collect()
}

/// Returns the number of extended grapheme clusters in `s`.
#[must_use]
pub fn grapheme_count(s: &str) -> i64 {
    UnicodeSegmentation::graphemes(s, true).count() as i64
}

/// Returns `(byte_offset, grapheme)` pairs for each extended grapheme
/// cluster in `s` (UAX #29), preserving input order.
#[must_use]
pub fn grapheme_indices(s: &str) -> Vec<(i64, String)> {
    UnicodeSegmentation::grapheme_indices(s, true)
        .map(|(i, g)| (i as i64, String::from(g)))
        .collect()
}

/// Returns the Unicode word boundaries (UAX #29) in `s` - including
/// whitespace-only and punctuation-only spans, mirroring
/// `unicode-segmentation`'s `split_word_bounds`.
#[must_use]
pub fn word_bounds(s: &str) -> Vec<String> {
    s.split_word_bounds().map(String::from).collect()
}

/// Returns only the "real" Unicode words (UAX #29) in `s`, skipping
/// whitespace and punctuation runs.
#[must_use]
pub fn words(s: &str) -> Vec<String> {
    s.unicode_words().map(String::from).collect()
}

/// Returns the Unicode sentence boundaries (UAX #29) in `s`.
#[must_use]
pub fn sentences(s: &str) -> Vec<String> {
    s.unicode_sentences().map(String::from).collect()
}

/// Returns the number of Unicode words in `s`.
#[must_use]
pub fn word_count(s: &str) -> i64 {
    s.unicode_words().count() as i64
}

/// Returns the number of Unicode sentences in `s`.
#[must_use]
pub fn sentence_count(s: &str) -> i64 {
    s.unicode_sentences().count() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letter_predicate_covers_non_ascii_scripts() {
        assert!(is_letter('a'));
        assert!(is_letter('Z'));
        assert!(is_letter('é'));
        assert!(is_letter('名'));
        assert!(is_letter('Ω'));
        assert!(!is_letter('1'));
        assert!(!is_letter(' '));
        assert!(!is_letter('!'));
    }

    #[test]
    fn digit_predicate_covers_other_scripts() {
        assert!(is_digit('5'));
        // Arabic-Indic digit seven (U+0667)
        assert!(is_digit('\u{0667}'));
        // Devanagari digit zero (U+0966)
        assert!(is_digit('\u{0966}'));
        assert!(!is_digit('a'));
        // Roman numeral five (Nl, not Nd)
        assert!(!is_digit('\u{2164}'));
    }

    #[test]
    fn number_predicate_includes_letter_and_other_numbers() {
        assert!(is_number('5'));
        // Roman numeral five Ⅴ (Nl)
        assert!(is_number('\u{2164}'));
        // Vulgar fraction one-half ½ (No)
        assert!(is_number('\u{00BD}'));
        assert!(!is_number('a'));
    }

    #[test]
    fn punct_predicate_real_unicode_categories() {
        assert!(is_punct(','));
        assert!(is_punct('"'));
        // Em dash (U+2014, Pd)
        assert!(is_punct('\u{2014}'));
        // Left double angle quotation mark (U+00AB, Pi)
        assert!(is_punct('\u{00AB}'));
        // Fullwidth comma (U+FF0C)
        assert!(is_punct('\u{FF0C}'));
        assert!(!is_punct('a'));
        assert!(!is_punct('$'));
    }

    #[test]
    fn symbol_predicate_real_unicode_categories() {
        // Dollar sign (Sc)
        assert!(is_symbol('$'));
        // Yen sign (Sc)
        assert!(is_symbol('¥'));
        // For-all (Sm)
        assert!(is_symbol('\u{2200}'));
        // Snowman (So)
        assert!(is_symbol('\u{2603}'));
        assert!(!is_symbol('a'));
        assert!(!is_symbol(','));
    }

    #[test]
    fn mark_predicate_covers_combining() {
        // Combining acute accent (U+0301, Mn)
        assert!(is_mark('\u{0301}'));
        // Devanagari sign visarga (U+0903, Mc)
        assert!(is_mark('\u{0903}'));
        // Combining enclosing circle (U+20DD, Me)
        assert!(is_mark('\u{20DD}'));
        assert!(!is_mark('a'));
    }

    #[test]
    fn casing_unicode_round_trip() {
        assert_eq!(to_upper('a'), 'A');
        assert_eq!(to_lower('A'), 'a');
        assert_eq!(to_upper('é'), 'É');
        assert_eq!(to_lower('Ö'), 'ö');
    }

    #[test]
    fn full_case_string_handles_special_mappings() {
        assert_eq!(to_upper_str("ß"), "SS");
        assert_eq!(to_lower_str("Σ"), "σ");
        assert_eq!(to_upper_str("café"), "CAFÉ");
    }

    #[test]
    fn normalization_round_trips() {
        // A with acute accent: composed (U+00C1) vs decomposed (A + U+0301).
        let composed = "\u{00C1}";
        let decomposed = "A\u{0301}";
        assert_eq!(nfc(decomposed), composed);
        assert_eq!(nfd(composed), decomposed);
        assert!(is_nfc(composed));
        assert!(is_nfd(decomposed));
        assert!(!is_nfc(decomposed));
        assert!(!is_nfd(composed));
    }

    #[test]
    fn nfkc_collapses_compatibility() {
        // Halfwidth katakana ka (U+FF76) → fullwidth (U+30AB)
        let halfwidth = "\u{FF76}";
        assert_eq!(nfkc(halfwidth), "\u{30AB}");
    }

    #[test]
    fn grapheme_count_counts_user_chars() {
        // "café" written with combining acute = 4 graphemes but 5 scalars.
        let decomposed = "cafe\u{0301}";
        assert_eq!(grapheme_count(decomposed), 4);
        assert_eq!(decomposed.chars().count(), 5);
        // Family ZWJ sequence = 1 grapheme.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(grapheme_count(family), 1);
    }

    #[test]
    fn word_iteration_segments_uax29() {
        let v: Vec<String> = words("The quick brown fox.");
        assert_eq!(v, vec!["The", "quick", "brown", "fox"]);
        let v: Vec<String> = words("日本語のテキスト");
        assert!(v.len() >= 2);
    }

    #[test]
    fn sentence_iteration_segments_uax29() {
        // UAX #29 is abbreviation-blind, so the canonical test input
        // avoids leading honorifics ("Mr.") that the algorithm cannot
        // distinguish from sentence-ending periods.
        let sents = sentences("Hello world. This is a test! And one more?");
        assert_eq!(sents.len(), 3);
    }

    #[test]
    fn control_and_assigned_predicates() {
        assert!(is_control('\x00'));
        assert!(is_control('\x1b'));
        assert!(!is_control('a'));
        assert!(is_assigned('a'));
        // U+FDD0 is a non-character (still Cn).
        assert!(!is_assigned('\u{FDD0}'));
    }

    #[test]
    fn print_and_graphic_round_trip() {
        assert!(is_print('a'));
        assert!(is_graphic('a'));
        assert!(is_print(' '));
        assert!(!is_graphic(' '));
        assert!(!is_print('\x00'));
    }

    #[test]
    fn graphemes_ascii_one_per_char() {
        let gs = graphemes("hello");
        assert_eq!(gs, vec!["h", "e", "l", "l", "o"]);
        assert_eq!(grapheme_count("hello"), 5);
    }

    #[test]
    fn graphemes_combining_acute_counts_as_one() {
        // "cafe" + combining acute (U+0301) = 4 graphemes, 5 scalars.
        let decomposed = "cafe\u{0301}";
        let gs = graphemes(decomposed);
        assert_eq!(gs.len(), 4);
        assert_eq!(gs[0], "c");
        assert_eq!(gs[1], "a");
        assert_eq!(gs[2], "f");
        assert_eq!(gs[3], "e\u{0301}");
    }

    #[test]
    fn graphemes_family_zwj_is_single_cluster() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let gs = graphemes(family);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0], family);
    }

    #[test]
    fn graphemes_empty_string_yields_empty_vec() {
        let gs = graphemes("");
        assert!(gs.is_empty());
        assert_eq!(grapheme_count(""), 0);
        let idx = grapheme_indices("");
        assert!(idx.is_empty());
    }

    #[test]
    fn grapheme_indices_returns_byte_offsets() {
        // "a" (1 byte) + "é" (2 bytes, U+00E9) + "z" (1 byte).
        let s = "a\u{00E9}z";
        let idx = grapheme_indices(s);
        assert_eq!(idx.len(), 3);
        assert_eq!(idx[0], (0, "a".to_string()));
        assert_eq!(idx[1], (1, "\u{00E9}".to_string()));
        assert_eq!(idx[2], (3, "z".to_string()));
    }
}
