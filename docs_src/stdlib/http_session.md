# `std::http::session`

Status: experimental

Signs and verifies a session payload. The cookie itself - name, attributes, expiry, a server-side store, id rotation on privilege change, revocation - is application policy and belongs in a session package built on these two.

## Items

| Item | Signature | Description |
|---|---|---|
| `with_session` | `fn with_session(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Run a closure with the session bound; persist any mutations. |
| `sign` | `fn sign(value: String, secret: Vec<u8>) -> String` | Sign session data into a tamper-evident cookie value. |
| `verify` | `fn verify(value: String, secret: Vec<u8>) -> Result<String, errors::Error>` | Verify and decode a signed session cookie value. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
