# `std::net::url`

Network URL parsing and component escaping; never use filesystem-path rules.

## Items

| Item | Signature | Description |
|---|---|---|
| `Url` | `type Url` | Parsed URL. |
| `query_escape` | `fn query_escape(text: String) -> String` | Percent-encodes a query parameter. |
| `query_unescape` | `fn query_unescape(text: String) -> String` | Inverse of `query_escape`. |
| `path_escape` | `fn path_escape(text: String) -> String` | Percent-encodes a URL path segment. |
| `path_unescape` | `fn path_unescape(text: String) -> String` | Inverse of `path_escape`. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
