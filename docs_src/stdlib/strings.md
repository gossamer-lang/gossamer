# `std::strings`

Status: experimental

String operations.

## Items

| Item | Signature | Description |
|---|---|---|
| `split` | `fn split(text: String, sep: String | char) -> Vec<String>` | Splits a string by a delimiter. |
| `splitn` | `fn splitn(text: String, n: i64, sep: String | char) -> Vec<String>` | Splits a string into at most `n` parts. |
| `trim` | `fn trim(text: String) -> String` | Removes leading and trailing whitespace. |
| `contains` | `fn contains(text: String, needle: String | char) -> bool` | Returns whether the string contains a substring. |
| `find` | `fn find(text: String, needle: String | char) -> Option<i64>` | Returns the character index of the first match, or None. |
| `replace` | `fn replace(text: String, from: String | char, to: String | char) -> String` | Replaces every occurrence of `from` with `to`. |
| `to_lowercase` | `fn to_lowercase(text: String) -> String` | Lowercases every character. |
| `to_uppercase` | `fn to_uppercase(text: String) -> String` | Uppercases every character. |
| `starts_with` | `fn starts_with(text: String, needle: String | char) -> bool` | Returns whether the string starts with the given prefix. |
| `ends_with` | `fn ends_with(text: String, needle: String | char) -> bool` | Returns whether the string ends with the given suffix. |
| `split_once` | `fn split_once(text: String, sep: String | char) -> Option<(String, String)>` | Splits on the first occurrence of `sep`; returns Option<(String, String)>. |
| `rsplit_once` | `fn rsplit_once(text: String, sep: String | char) -> Option<(String, String)>` | Splits on the last occurrence of `sep`; returns Option<(String, String)>. |
| `count` | `fn count(text: String, needle: String | char) -> i64` | Counts non-overlapping occurrences of `needle`. |
| `byte_len` | `fn byte_len(text: String) -> i64` | Returns the UTF-8 byte length. |
| `byte_at` | `fn byte_at(text: String, index: i64) -> i64` | Returns the UTF-8 byte at an index. |
| `bytes` | `fn bytes(text: String) -> Vec<u8>` | Returns the UTF-8 bytes of the string. |
| `chars` | `fn chars(text: String) -> Iterator<char>` | Returns a cursor over the string's Unicode scalar values; `collect` materialises it. |
| `center` | `fn center(text: String, width: i64, fill: char) -> String` | Symmetric pad to `width` using the given pad character. |
| `slice` | `fn slice(text: String, start: i64, end: i64) -> Result<String, errors::Error>` | Safe byte-range slice returning Result<String, errors::Error>. |
| `substring` | `fn substring(text: String, start: i64, end: i64) -> String` | Byte-offset substring returning a String. |
| `split_whitespace` | `fn split_whitespace(text: String) -> Vec<String>` | Splits on runs of whitespace, dropping empty fields. |
| `trim_start` | `fn trim_start(text: String) -> String` | Removes leading whitespace. |
| `trim_end` | `fn trim_end(text: String) -> String` | Removes trailing whitespace. |
| `rfind` | `fn rfind(text: String, needle: String | char) -> Option<i64>` | Returns the character index of the last match, or None. |
| `trim_start_matches` | `fn trim_start_matches(text: String, cutset: String | char) -> String` | Removes leading characters in the given set. |
| `trim_end_matches` | `fn trim_end_matches(text: String, cutset: String | char) -> String` | Removes trailing characters in the given set. |
| `replacen` | `fn replacen(text: String, from: String | char, to: String | char, n: i64) -> String` | Replaces the first n occurrences of a substring. |
| `repeat` | `fn repeat(text: String, count: i64) -> String` | Concatenates n copies of the string. |
| `lines` | `fn lines(text: String) -> Vec<String>` | Splits into lines, dropping line terminators. |
| `join` | `fn join(parts: Vec<String>, sep: String) -> String` | Joins string parts with a separator. |
| `strip_prefix` | `fn strip_prefix(text: String, prefix: String | char) -> Option<String>` | Removes a leading prefix if present. |
| `strip_suffix` | `fn strip_suffix(text: String, suffix: String | char) -> Option<String>` | Removes a trailing suffix if present. |
| `pad_left` | `fn pad_left(text: String, width: i64, fill: char) -> String` | Left-pads to `width` with the given character. |
| `pad_right` | `fn pad_right(text: String, width: i64, fill: char) -> String` | Right-pads to `width` with the given character. |
| `contains_any` | `fn contains_any(text: String, needle: String | char) -> bool` | Reports whether the string contains any rune in a set. |
| `find_any` | `fn find_any(text: String, needle: String | char) -> Option<i64>` | Byte index of the first rune in a set, or None. |
| `rfind_any` | `fn rfind_any(text: String, needle: String | char) -> Option<i64>` | Byte index of the last rune in a set, or None. |
| `equal_fold` | `fn equal_fold(text: String, needle: String | char) -> bool` | Case-insensitive Unicode string equality. |
| `trim_matches` | `fn trim_matches(text: String, cutset: String | char) -> String` | Removes characters in the given set from both ends. |
| `to_title` | `fn to_title(text: String) -> String` | Title-cases the first letter of each word. |
| `to_i64` | `fn to_i64(text: String) -> Option<i64>` | Strict full-string parse to Option<i64>. |
| `to_f64` | `fn to_f64(text: String) -> Option<f64>` | Strict full-string parse to Option<f64>. |
| `to_bool` | `fn to_bool(text: String) -> Option<bool>` | Parses exactly `true` / `false` to Option<bool>. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
