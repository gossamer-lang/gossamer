# `std::unicode`

Status: experimental

Unicode general-category predicates, casing, normalization, and segmentation.

## Items

| Item | Signature | Description |
|---|---|---|
| `char_width` | `fn char_width(r: char) -> i64` | Terminal columns r occupies: 2 for wide and fullwidth characters, 0 for combining marks, zero-width, and control characters, otherwise 1. |
| `str_width` | `fn str_width(s: String) -> i64` | Terminal columns s occupies, reading emoji and variation sequences as the clusters a terminal draws. |
| `is_letter` | `fn is_letter(rune: char) -> bool` | True if r is in general-category group L. |
| `is_digit` | `fn is_digit(rune: char) -> bool` | True if r is a decimal digit (category Nd). |
| `is_number` | `fn is_number(rune: char) -> bool` | True if r is any numeric (Nd\|Nl\|No). |
| `is_space` | `fn is_space(rune: char) -> bool` | True if r is whitespace (Z* plus HT/LF/VT/FF/CR/NEL). |
| `is_upper` | `fn is_upper(rune: char) -> bool` | True if r is category Lu. |
| `is_lower` | `fn is_lower(rune: char) -> bool` | True if r is category Ll. |
| `is_title` | `fn is_title(rune: char) -> bool` | True if r is category Lt. |
| `is_punct` | `fn is_punct(rune: char) -> bool` | True if r is in general-category group P. |
| `is_symbol` | `fn is_symbol(rune: char) -> bool` | True if r is in general-category group S. |
| `is_mark` | `fn is_mark(rune: char) -> bool` | True if r is in general-category group M. |
| `is_print` | `fn is_print(rune: char) -> bool` | True if r is printable (not Cc/Cf/Cs/Co/Cn). |
| `is_graphic` | `fn is_graphic(rune: char) -> bool` | True if r is graphic (printable and not whitespace). |
| `is_control` | `fn is_control(rune: char) -> bool` | True if r is category Cc. |
| `is_assigned` | `fn is_assigned(rune: char) -> bool` | True if r is an assigned code point (not Cn). |
| `to_upper` | `fn to_upper(rune: char) -> char` | Simple uppercase mapping for one rune. |
| `to_lower` | `fn to_lower(rune: char) -> char` | Simple lowercase mapping for one rune. |
| `to_title` | `fn to_title(rune: char) -> char` | Simple titlecase mapping for one rune. |
| `simple_fold` | `fn simple_fold(rune: char) -> char` | Next rune in Unicode case-folding cycle. |
| `combining_class` | `fn combining_class(rune: char) -> i64` | Canonical combining class (0-254) for r. |
| `to_upper_str` | `fn to_upper_str(text: String) -> String` | Full uppercase mapping for a string (ss -> SS). |
| `to_lower_str` | `fn to_lower_str(text: String) -> String` | Full lowercase mapping for a string. |
| `fold_case` | `fn fold_case(text: String) -> String` | Simple case-folded comparison form for a string. |
| `nfc` | `fn nfc(text: String) -> String` | Normalize a string to NFC (canonical composition). |
| `nfd` | `fn nfd(text: String) -> String` | Normalize a string to NFD (canonical decomposition). |
| `nfkc` | `fn nfkc(text: String) -> String` | Normalize a string to NFKC (compat composition). |
| `nfkd` | `fn nfkd(text: String) -> String` | Normalize a string to NFKD (compat decomposition). |
| `is_nfc` | `fn is_nfc(text: String) -> bool` | True if a string is already in NFC. |
| `is_nfd` | `fn is_nfd(text: String) -> bool` | True if a string is already in NFD. |
| `is_nfkc` | `fn is_nfkc(text: String) -> bool` | True if a string is already in NFKC. |
| `is_nfkd` | `fn is_nfkd(text: String) -> bool` | True if a string is already in NFKD. |
| `graphemes` | `fn graphemes(text: String) -> Vec<String>` | UAX #29 extended grapheme clusters of a string. |
| `grapheme_count` | `fn grapheme_count(text: String) -> i64` | Number of UAX #29 grapheme clusters in a string. |
| `words` | `fn words(text: String) -> Vec<String>` | UAX #29 Unicode words in a string (skips punct/whitespace). |
| `word_bounds` | `fn word_bounds(text: String) -> Vec<(i64, i64)>` | UAX #29 word boundaries (includes punct + whitespace runs). |
| `word_count` | `fn word_count(text: String) -> i64` | Number of UAX #29 words in a string. |
| `sentences` | `fn sentences(text: String) -> Vec<String>` | UAX #29 Unicode sentences in a string. |
| `sentence_count` | `fn sentence_count(text: String) -> i64` | Number of UAX #29 sentences in a string. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
