# `std::http::csrf`

Status: experimental

Double-submit-cookie CSRF protection with Origin / Referer allowlist.

## Items

| Item | Signature | Description |
|---|---|---|
| `Config` | `type Config` | Signing key, cookie / header names, and origin allowlist. |
| `RouteAuth` | `type RouteAuth` | Per-route policy: Required, Optional, or Skipped. |
| `issue_token` | `fn issue_token(secret: Vec<u8>) -> Result<String, errors::Error>` | Mint a fresh CSRF token bound to the configured signing key. |
| `verify_token` | `fn verify_token(cookie_token: String, supplied_token: String, secret: Vec<u8>) -> Result<(), errors::Error>` | Constant-time verify of a presented token against the cookie value. |
| `extract_token` | `fn extract_token(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Pull a token from the configured header or form field. |
| `origin_allowed` | `fn origin_allowed(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Origin / Referer allowlist check for unsafe methods. |
| `check` | `fn check(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Combined origin + token gate; returns Err on failure. |
| `attach_cookie` | `fn attach_cookie(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Set the CSRF cookie on a Response. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
