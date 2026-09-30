# `std::http::cookie`

Status: experimental

RFC 6265 cookie parser and Set-Cookie builder.

## Items

| Item | Signature | Description |
|---|---|---|
| `Cookie` | `type Cookie` | Parsed cookie with name, value, and Set-Cookie attributes. |
| `CookieBuilder` | `type CookieBuilder` | Fluent builder for Set-Cookie response headers. |
| `SameSite` | `type SameSite` | SameSite attribute: Strict / Lax / None. |
| `parse_cookie_header` | `fn parse_cookie_header(header: String) -> Vec<http::cookie::Cookie>` | Parse a Cookie request header into (name, value) pairs. |
| `serialize` | `fn serialize(name: String, value: String) -> String` | Render a Cookie as a Set-Cookie header value. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
