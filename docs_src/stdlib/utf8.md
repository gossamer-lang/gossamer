# `std::utf8`

Status: experimental

UTF-8 validation and scalar decoding.

## Items

| Item | Signature | Description |
|---|---|---|
| `is_valid` | `fn is_valid(bytes: Vec<u8>) -> bool` | Validates a byte slice as UTF-8. |
| `rune_count` | `fn rune_count(bytes: Vec<u8>) -> i64` | Counts Unicode scalar values. |
| `rune_count_in_string` | `fn rune_count_in_string(text: String) -> i64` | Counts the runes in a String. |
| `rune_len` | `fn rune_len(rune: char) -> i64` | Number of bytes needed to encode a rune. |
| `valid_string` | `fn valid_string(text: String) -> bool` | Reports whether a String is valid UTF-8. |
| `valid_rune` | `fn valid_rune(rune: char) -> bool` | Reports whether a code point can be legally encoded. |
| `full_rune` | `fn full_rune(bytes: Vec<u8>) -> bool` | Reports whether the bytes begin with a full rune. |
| `full_rune_in_string` | `fn full_rune_in_string(text: String) -> bool` | Reports whether the String begins with a full rune. |
| `rune_start` | `fn rune_start(byte: u8) -> bool` | Reports whether the byte could be the first of a rune. |
| `decode_rune` | `fn decode_rune(bytes: Vec<u8>) -> (char, i64)` | Decodes the first rune from bytes, returning (rune, width). |
| `decode_rune_in_string` | `fn decode_rune_in_string(text: String) -> (char, i64)` | Decodes the first rune from a String, returning (rune, width). |
| `decode_last_rune` | `fn decode_last_rune(bytes: Vec<u8>) -> (char, i64)` | Decodes the last rune from bytes, returning (rune, width). |
| `decode_last_rune_in_string` | `fn decode_last_rune_in_string(text: String) -> (char, i64)` | Decodes the last rune from a String, returning (rune, width). |
| `append_rune` | `fn append_rune(bytes: Vec<u8>, rune: char) -> Vec<u8>` | Appends the UTF-8 encoding of a rune to a byte Vec. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
