# `std::httptest`

Status: experimental

Fixtures for testing HTTP code. A handler is a function from a request to a response, so `record` calls one in memory; a test that is about the wire builds an `http::Server`, binds port 0, and reads the address back.

## Items

| Item | Signature | Description |
|---|---|---|
| `record` | `fn record(handler: http::Handler, method: String, path: String, body: String) -> Result<http::Response, errors::Error>` | `record(handler, method, path, body) -> Result<Response, Error>` - calls `handler` with a request built in memory and answers its response. No socket, no port, no accept loop. |
| `server` | `fn server(status: i64, body: String) -> String` | server(status, body) -> String: starts an isolated loopback static-response server and returns its http:// base URL. Use http::get or http::Client as the test client. The server is test-process scoped and stops when that process exits. For a programmable server, build an `http::Server` over a `Router` and bind port 0. |
