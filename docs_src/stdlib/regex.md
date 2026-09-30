# `std::regex`

Status: experimental

Compiled regular expressions (Rust `regex` crate syntax; no backreferences or look-around).

## Items

| Item | Signature | Description |
|---|---|---|
| `Pattern` | `type Pattern` | Compiled pattern handle returned by `compile` and `new`. |
| `compile` | `fn compile(pattern: String) -> regex::Pattern` | Compiles a literal pattern, checked while the program is built, into a `Pattern`. |
| `new` | `fn new(pattern: String) -> Result<regex::Pattern, errors::Error>` | Compiles a pattern built at run time into a `Pattern`, or an `Err` with the reason. |
| `is_match` | `fn is_match(pattern: regex::Pattern, text: String) -> bool` | Returns whether the pattern matches anywhere in the text. |
| `find` | `fn find(pattern: regex::Pattern, text: String) -> Option<(i64, i64, String)>` | Returns the first match as `(start, end, text)`, or `None`. |
| `find_all` | `fn find_all(pattern: regex::Pattern, text: String) -> Vec<(i64, i64, String)>` | Returns every non-overlapping match as `(start, end, text)`. |
| `count` | `fn count(pattern: regex::Pattern, text: String) -> i64` | Counts the non-overlapping matches without building them. |
| `captures` | `fn captures(pattern: regex::Pattern, text: String) -> Option<Vec<Option<String>>>` | Returns capture groups for the first match; index 0 is the full match. |
| `captures_all` | `fn captures_all(pattern: regex::Pattern, text: String) -> Vec<Vec<Option<String>>>` | Returns capture groups for every match in the text. |
| `replace` | `fn replace(pattern: regex::Pattern, text: String, replacement: String) -> String` | Replaces the first match with the given replacement (supports `$N`). |
| `replace_all` | `fn replace_all(pattern: regex::Pattern, text: String, replacement: String) -> String` | Replaces every non-overlapping match. |
| `split` | `fn split(pattern: regex::Pattern, text: String) -> Vec<String>` | Splits the text on every pattern match. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Unicode behavior

Unicode mode is enabled by default. UTF-8 literals, `.`, `\w`, `\s`, Unicode
property classes such as `\p{Greek}`, case-insensitive matching, captures,
split, and replacement operate on Unicode scalar values.

Regex does not normalize text and does not treat an extended grapheme cluster
as one character. Normalize input explicitly when canonical equivalence is
required. `find` and `find_all` return UTF-8 byte offsets, matching the
underlying Rust regex API, while the matched text remains valid UTF-8.
