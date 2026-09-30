# `std::net::smtp`

Status: experimental

Sends one message per call, so an application can mail a password reset, an address verification, a magic link, or a security notice. A pool, a queue, retries, and bounce handling are application policy and belong in a package built on these. Port 465 speaks TLS from the first byte; any other port starts in the clear and upgrades through STARTTLS when the server offers it, and credentials are refused rather than sent to a server offering no encryption.

## Items

| Item | Signature | Description |
|---|---|---|
| `send` | `fn send(addr: String, from: String, to: String, subject: String, body: String) -> Result<(), errors::Error>` | `send(addr, from, to, subject, body) -> Result<(), errors::Error>` - one unauthenticated message. `to` may name several recipients, comma-separated. A CR or LF in an address or the subject is replaced with a space, so a caller-supplied value cannot end the header and add its own. |
| `send_auth` | `fn send_auth(addr: String, from: String, to: String, subject: String, body: String, username: String, password: String) -> Result<(), errors::Error>` | `send_auth(addr, from, to, subject, body, username, password) -> Result<(), errors::Error>` - the shape a transactional mail provider takes. Authenticates with AUTH PLAIN or AUTH LOGIN, whichever the server advertises, and only over TLS. |
