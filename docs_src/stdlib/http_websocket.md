# `std::http::websocket`

Status: experimental

RFC 6455 WebSocket support. Server-side accept + send_text / send_binary / ping / pong / close.

## Items

| Item | Signature | Description |
|---|---|---|
| `Conn` | `type Conn` | An open connection, from `connect` or handed to a `serve` handler's `fn handle(&self, ws: Conn)`. Every copy shares the one connection, which closes at `close` or with the last copy. |
| `WebSocket` | `type WebSocket` | Accepted WebSocket connection (Rust-side framing). |
| `Message` | `type Message` | Text / Binary / Ping / Pong / Close. |
| `accept` | `fn accept(request: http::Request) -> Result<http::Response, errors::Error>` | Validate a WebSocket upgrade request and answer the 101 Switching Protocols response that completes the handshake. |
| `Error` | `type Error` | Io / Protocol / BadHandshake. |
| `accept_key` | `fn accept_key(key: String) -> String` | Compute RFC 6455 Sec-WebSocket-Accept from a client nonce. Available in interp + compiled. |
| `is_websocket_upgrade` | `fn is_websocket_upgrade(request: http::Request) -> bool` | Test whether an incoming Request carries a WebSocket upgrade handshake. Interp tier. |
| `serve` | `fn serve<H>(addr: String, handler: H) -> Result<(), errors::Error>` | serve(addr, handler) -> Result<(), Error>: bind, upgrade each connection, dispatch the handler's handle(self, ws) per connection. |
| `connect` | `fn connect(url: String) -> Result<http::websocket::Conn, errors::Error>` | connect(url) -> Result<Conn, Error>: client TCP connect + RFC 6455 upgrade; returns the connection. |
| `send_text` | `fn send_text(conn: http::websocket::Conn, text: String) -> Result<(), errors::Error>` | send_text(ws, s) -> Result<(), Error>: send one text frame. |
| `send_binary` | `fn send_binary(conn: http::websocket::Conn, data: Vec<u8>) -> Result<(), errors::Error>` | send_binary(ws, data) -> Result<(), Error>: send one binary frame. |
| `recv` | `fn recv(conn: http::websocket::Conn) -> Result<String, errors::Error>` | recv(ws) -> Result<String, Error>: next text message; Err on close/error. |
| `close` | `fn close(conn: http::websocket::Conn) -> Result<(), errors::Error>` | close(ws) -> Result<(), Error>: send a close frame and release the handle. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
