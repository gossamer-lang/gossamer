# `std::mime`

Status: experimental

RFC 2045 media type parsing, parameter extraction, and extension lookup.

## Items

| Item | Signature | Description |
|---|---|---|
| `parse` | `fn parse(value: String) -> Result<mime::Mime, errors::Error>` | Canonical `type/subtype` form of the input, or empty on parse failure. |
| `top` | `fn top(mime: String) -> String` | Top-level type (e.g. `text`) of a media type, or empty. |
| `sub` | `fn sub(mime: String) -> String` | Subtype (e.g. `html`) of a media type, or empty. |
| `charset` | `fn charset(mime: String) -> String` | Return the `charset` parameter, or empty. |
| `boundary` | `fn boundary(mime: String) -> String` | Return the multipart `boundary` parameter, or empty. |
| `param` | `fn param(mime: String, name: String) -> Option<String>` | Return an arbitrary parameter by key, or empty. |
| `type_by_extension` | `fn type_by_extension(ext: String) -> Option<String>` | Canonical media type for a filename extension (dot optional), or empty. |
| `extension_by_type` | `fn extension_by_type(mime: String) -> Option<String>` | Canonical extension (no leading dot) for a media type, or empty. |
| `is_valid` | `fn is_valid(value: String) -> bool` | Return true iff the string parses as a valid media type. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
