# `std::utf16`

Status: experimental

UTF-16 encoding/decoding and surrogate pair helpers.

## Items

| Item | Signature | Description |
|---|---|---|
| `is_surrogate` | `fn is_surrogate(unit: u16) -> bool` | Whether a code unit lies in the surrogate range `0xD800..=0xDFFF`. |
| `rune_len` | `fn rune_len(rune: char) -> i64` | Number of UTF-16 code units a `char` encodes to (1 or 2). |
| `decode_surrogate_pair` | `fn decode_surrogate_pair(high: u16, low: u16) -> Option<char>` | The `char` a high and low surrogate pair encode, or `None` when they are not a pair. |
| `encode_string` | `fn encode_string(text: String) -> Vec<u16>` | Encodes a String directly to Vec<u16>. |
| `decode_to_string` | `fn decode_to_string(units: Vec<u16>) -> String` | Decodes UTF-16 code units to a String; an unpaired surrogate becomes U+FFFD. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
