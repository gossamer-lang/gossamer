# `std::uuid`

Status: experimental

UUID v4 (random) and v7 (timestamp-ordered) generation, parse, and normalize.

## Items

| Item | Signature | Description |
|---|---|---|
| `v4` | `fn v4() -> String` | Generate a fresh random v4 UUID as a canonical hyphenated string. |
| `v7` | `fn v7() -> String` | Generate a fresh v7 (timestamp-ordered) UUID. |
| `is_valid` | `fn is_valid(text: String) -> bool` | Return true iff the string parses as a canonical UUID. |
| `normalize` | `fn normalize(text: String) -> String` | Lowercase canonical UUID form of the input, or empty string on parse failure. |
| `simple` | `fn simple(text: String) -> String` | 32-character unhyphenated form of the input, or empty string on parse failure. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
