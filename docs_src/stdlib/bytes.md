# `std::bytes`

Status: experimental

Byte buffers, builders, and slice helpers.

## Items

| Item | Signature | Description |
|---|---|---|
| `Buffer` | `type Buffer` | Growable byte buffer for incremental assembly: new, with_capacity, write_str, push, len, is_empty, clear, to_string. A buffer you index, slice, or edit at an offset is a Vec<u8>. |
| `Builder` | `type Builder` | Incremental string builder: new, write, write_char, len, build, as_str. Cheaper than repeated `+` on a String, which copies. |
| `index_of` | `fn index_of(haystack: Vec<u8>, needle: Vec<u8>) -> Option<i64>` | First occurrence of a byte needle. |
| `split` | `fn split(haystack: Vec<u8>, sep: Vec<u8>) -> Vec<Vec<u8>>` | Splits on every separator occurrence. |
| `replace` | `fn replace(haystack: Vec<u8>, from: Vec<u8>, to: Vec<u8>) -> Vec<u8>` | Replaces every occurrence of a byte needle. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
