# Gossamer standard library

One page per module. Source is `crates/gossamer-std/src/`; this index is regenerated from `manifest::ALL_MODULES` by `gos doc --emit-stdlib`.

For receiver methods on built-in types, see [`Methods by type`](../method_support.md).

| Module | Summary |
|---|---|
| [`std::archive::tar`](archive_tar.md) | Unix tar reader and writer (USTAR / PAX-aware decode). |
| [`std::archive::zip`](archive_zip.md) | ZIP archive reader and writer. |
| [`std::bufio`](bufio.md) | Buffered readers, writers, and line scanners. |
| [`std::bytes`](bytes.md) | Byte buffers, builders, and slice helpers. |
| [`std::collections`](collections.md) | Built-in container types. |
| [`std::compress::bzip2`](compress_bzip2.md) | bzip2 encoder / decoder (BZh format). |
| [`std::compress::flate`](compress_flate.md) | Raw DEFLATE (RFC 1951) encoder / decoder. |
| [`std::compress::gzip`](compress_gzip.md) | gzip encoder / decoder (RFC 1952; flate2-backed). |
| [`std::compress::zlib`](compress_zlib.md) | zlib (RFC 1950) encoder / decoder. |
| [`std::compress::zstd`](compress_zstd.md) | Zstandard encoder / decoder (RFC 8478; libzstd-vendored). |
| [`std::context`](context.md) | Request-scoped cancellation, deadlines, and timeouts. |
| [`std::crypto::aead`](crypto_aead.md) | Authenticated encryption with associated data. |
| [`std::crypto::blake3`](crypto_blake3.md) | BLAKE3 hashing. |
| [`std::crypto::ecdsa`](crypto_ecdsa.md) | ECDSA over the NIST P-256 curve. |
| [`std::crypto::ed25519`](crypto_ed25519.md) | Ed25519 digital signatures. |
| [`std::crypto::hmac`](crypto_hmac.md) | HMAC-SHA-256 keyed MACs. |
| [`std::crypto::insecure`](crypto_insecure.md) | Legacy / broken hashes (MD5, SHA-1). Compat only - never use for new code. |
| [`std::crypto::kdf`](crypto_kdf.md) | Password-based key-derivation functions. |
| [`std::crypto::password`](crypto_password.md) | Argon2id password hashing facade: PHC-string hash / verify / re-hash policy. |
| [`std::crypto::rand`](crypto_rand.md) | Secure random bytes from the host CSPRNG. |
| [`std::crypto::sha256`](crypto_sha256.md) | SHA-256 hashing. |
| [`std::crypto::sha512`](crypto_sha512.md) | SHA-512 hashing. |
| [`std::crypto::subtle`](crypto_subtle.md) | Constant-time comparison helpers. |
| [`std::crypto::x509`](crypto_x509.md) | X.509 certificate parsing. |
| [`std::database::sql`](database_sql.md) | Driver-pluggable SQL database access. No driver ships in the box; bring your own (Postgres, MySQL, SQLite, ...) by registering one at startup. |
| [`std::encoding::ascii85`](encoding_ascii85.md) | ASCII85 / base85 encode / decode. |
| [`std::encoding::base32`](encoding_base32.md) | RFC 4648 base32 (uppercase) encode / decode. |
| [`std::encoding::base64`](encoding_base64.md) | RFC 4648 base64 encode/decode. |
| [`std::encoding::binary`](encoding_binary.md) | Big/little-endian integer packing and varint codecs. |
| [`std::encoding::csv`](encoding_csv.md) | CSV record reader and writer. |
| [`std::encoding::hex`](encoding_hex.md) | Lowercase hex encode/decode. |
| [`std::encoding::json`](encoding_json.md) | JSON parser, emitter, and derive support. |
| [`std::encoding::pem`](encoding_pem.md) | PEM block encoder and decoder. |
| [`std::encoding::toml`](encoding_toml.md) | TOML 1.0 parsing + emission. Pair with the turbofish `from_toml::<Type>` for typed decoding (struct auto-derive). |
| [`std::encoding::xml`](encoding_xml.md) | Streaming XML decoder + builder (quick-xml). |
| [`std::encoding::yaml`](encoding_yaml.md) | YAML 1.2 parser/emitter (serde_norway-backed). |
| [`std::env`](env.md) | Process environment, command-line arguments, working directory. |
| [`std::errors`](errors.md) | Error construction, wrapping, and chain traversal. |
| [`std::flag`](flag.md) | Batteries-included CLI argument parsing. |
| [`std::fmt`](fmt.md) | Formatted printing and string interpolation. |
| [`std::fs`](fs.md) | Filesystem reading, writing, and traversal (Rust std::fs shape). |
| [`std::hash::adler32`](hash_adler32.md) | Adler-32 checksums. |
| [`std::hash::crc32`](hash_crc32.md) | CRC-32 (IEEE) checksums. |
| [`std::hash::crc32c`](hash_crc32c.md) | CRC-32C (Castagnoli) checksums, computed with the CPU's CRC instruction where there is one. |
| [`std::hash::fnv`](hash_fnv.md) | FNV-1a non-cryptographic hash (32-bit, 64-bit). |
| [`std::html`](html.md) | HTML text escaping and unescaping. |
| [`std::html::template`](html_template.md) | Context-aware HTML templates with auto-escape (text/attr/URL/JS). The context classifier is heuristic - sound for typical server-rendered responses but NOT a content-security-policy substitute; sanitize untrusted HTML fragments with a dedicated sanitizer. |
| [`std::http`](http.md) | HTTP/1.1 and HTTP/2 client and server. HTTP/2 negotiates via ALPN over TLS automatically (Go-style); h2c entry points are explicit. Write a handler as a cohort and an arena: `cohort { }` joins or cancels every goroutine the request spawned before the response is written, and its first child failure becomes the block's `Err` for the handler to turn into a status; `arena { }` bump-allocates what the request builds and frees it wholesale on every exit path, with escape checked at compile time. Dependency injection is closure capture - build the router from closures capturing the pool and the configuration. |
| [`std::http::chunked`](http_chunked.md) | RFC 7230 §4.1 chunked transfer-encoding reader and writer. |
| [`std::http::cookie`](http_cookie.md) | RFC 6265 cookie parser and Set-Cookie builder. |
| [`std::http::csrf`](http_csrf.md) | Double-submit-cookie CSRF protection with Origin / Referer allowlist. |
| [`std::http::form`](http_form.md) | application/x-www-form-urlencoded parser and builder. |
| [`std::http::health`](http_health.md) | Liveness and readiness endpoints are ordinary handlers over `std::lifecycle`: answer 200 from a liveness route, and 200/503 from `lifecycle::is_ready()` on a readiness route, which drops to false on its own when shutdown begins. A probe registry with per-check timeouts belongs in an application package. |
| [`std::http::middleware`](http_middleware.md) | Composable middleware: request_id, cors, security_headers, hsts, cache_control, etag, rate_limit, body_limit, timeout, compress_gzip, logger, recoverer, basic_auth, bearer_auth, safe_defaults. |
| [`std::http::multipart`](http_multipart.md) | RFC 7578 multipart/form-data streaming parser. |
| [`std::http::native_client`](http_native_client.md) | Goroutine-driven HTTP/1.1 client over std::net (no ureq, no blocking pool). |
| [`std::http::proxy`](http_proxy.md) | Reverse proxy on top of http::Client. Director-style request mutator + hop-by-hop strip + error handler. |
| [`std::http::query`](http_query.md) | A request's query string is already parsed: read `request.query` for the raw text and `request.query_pairs` for the decoded name/value pairs. |
| [`std::http::router`](http_router.md) | Go 1.22-class ServeMux: method-aware path patterns with parameter captures + prefix routes. |
| [`std::http::session`](http_session.md) | Signs and verifies a session payload. The cookie itself - name, attributes, expiry, a server-side store, id rotation on privilege change, revocation - is application policy and belongs in a session package built on these two. |
| [`std::http::sse`](http_sse.md) | Server-Sent Events (text/event-stream) emitter with heartbeat ticks and retry hint. |
| [`std::http::state`](http_state.md) | Dependency injection is closure capture: build the router from closures that capture the pool, the cache, and the configuration, and each handler reads what it captured. A captured heap value is shared, so one map serves every request. |
| [`std::http::static_files`](http_static_files.md) | Caching static-file handler: ETag, Last-Modified, byte ranges, MIME sniff. |
| [`std::http::websocket`](http_websocket.md) | RFC 6455 WebSocket support. Server-side accept + send_text / send_binary / ping / pong / close. |
| [`std::http_h3`](http_h3.md) | HTTP/3 over QUIC. std::http_h3 is the retained 0.27 spelling; no std::http::h3 alias. |
| [`std::httptest`](httptest.md) | Fixtures for testing HTTP code. A handler is a function from a request to a response, so `record` calls one in memory; a test that is about the wire builds an `http::Server`, binds port 0, and reads the address back. |
| [`std::image`](image.md) | Opaque RGBA8 image handles with PNG and JPEG codecs. |
| [`std::io`](io.md) | Stream-oriented I/O abstractions and process standard streams. |
| [`std::iter`](iter.md) | Sequence adapters: map, filter, fold, zip, enumerate, chain, etc. A `Vec` argument is traversed eagerly; an `Iterator` argument keeps the adapter lazy and answers with another iterator. |
| [`std::jwt`](jwt.md) | RFC 7519 tokens. Signs with HS256 / HS384 / HS512, ES256, and EdDSA; verifies those plus the RS256 / RS384 / RS512 family every mainstream identity provider mints with. Claims cross the boundary as JSON text. |
| [`std::lifecycle`](lifecycle.md) | Process readiness and graceful shutdown, with systemd sd_notify. Shutdown is observed, not dispatched: wait for it, then drain with ordinary statements - `spawn(|| serve())`, `lifecycle::ready()`, `lifecycle::await_shutdown()`, then the cleanup. |
| [`std::math`](math.md) | Mathematical constants and f64 functions (Go's math package shape). |
| [`std::math::big`](math_big.md) | Arbitrary-precision integers (num-bigint). |
| [`std::math::bits`](math_bits.md) | Integer bit-manipulation operations (Go's math/bits shape). |
| [`std::math::rand`](math_rand.md) | Deterministic pseudo-random number generation. |
| [`std::metrics`](metrics.md) | Prometheus-compatible primitives (Counter, Gauge, Histogram) and a Registry rendering the standard text-exposition format. |
| [`std::mime`](mime.md) | RFC 2045 media type parsing, parameter extraction, and extension lookup. |
| [`std::net`](net.md) | TCP/UDP networking primitives. |
| [`std::net::ip`](net_ip.md) | String-level IPv4 / IPv6 parsing and classification helpers. |
| [`std::net::netip`](net_netip.md) | Typed IP-address parsing, classification, and addr:port helpers (Go's net/netip shape). |
| [`std::net::smtp`](net_smtp.md) | Sends one message per call, so an application can mail a password reset, an address verification, a magic link, or a security notice. A pool, a queue, retries, and bounce handling are application policy and belong in a package built on these. Port 465 speaks TLS from the first byte; any other port starts in the clear and upgrades through STARTTLS when the server offers it, and credentials are refused rather than sent to a server offering no encryption. |
| [`std::net::url`](net_url.md) | Network URL parsing and component escaping; never use filesystem-path rules. |
| [`std::option`](option.md) | Data-last Option combinators for pipeline chaining: map, filter, unwrap_or, and_then, etc. |
| [`std::os`](os.md) | Operating-system identity. |
| [`std::os::exec`](os_exec.md) | Deprecated compatibility facade for child processes; new code uses std::process. |
| [`std::os::signal`](os_signal.md) | POSIX-style signal subscription (Go's os/signal shape). |
| [`std::os::user`](os_user.md) | POSIX user / group lookup. Unix-backed by `nix`; Windows falls back to env vars. |
| [`std::panic`](panic.md) | Panic / `catch_unwind` integration. |
| [`std::path`](path.md) | Lexical filesystem-path operations; platform path grammar, no URL parsing. |
| [`std::pprof`](pprof.md) | Runtime profiles in the text format `go tool pprof` reads, plus a Chrome-trace scheduler capture. |
| [`std::process`](process.md) | Canonical process control and child-process API; std::os::exec is compatibility-only. |
| [`std::regex`](regex.md) | Compiled regular expressions (Rust `regex` crate syntax; no backreferences or look-around). |
| [`std::result`](result.md) | Data-last Result combinators for pipeline chaining: map, map_err, unwrap_or_else, etc. |
| [`std::runtime`](runtime.md) | Goroutine / scheduler introspection and tuning. |
| [`std::slog`](slog.md) | Structured, levelled logging. |
| [`std::sort`](sort.md) | Explicit stable ordering and sorted-sequence search, the deliberate counterpart to Vec's unstable inherent `sort`. |
| [`std::strconv`](strconv.md) | Conversions between strings and primitive numeric types. |
| [`std::strings`](strings.md) | String operations. |
| [`std::sync`](sync.md) | Synchronisation primitives beyond channels. |
| [`std::testing`](testing.md) | Assertions and sub-test harness helpers. |
| [`std::thread`](thread.md) | OS-thread scheduling hints and CPU introspection; user concurrency uses goroutines, not thread spawning. |
| [`std::time`](time.md) | Wall-clock and monotonic time facilities. |
| [`std::tls`](tls.md) | Rustls-backed TLS support exposed through `http::serve_tls` and `net::TcpStream` TLS upgrades. The configuration constructors are host-runtime internals, not Gossamer callables. |
| [`std::trace`](trace.md) | W3C trace-context-compatible distributed tracing. Identifier types, request-scoped SpanContext, process-level Tracer, and OTLP JSON export. |
| [`std::unicode`](unicode.md) | Unicode general-category predicates, casing, normalization, and segmentation. |
| [`std::utf16`](utf16.md) | UTF-16 encoding/decoding and surrogate pair helpers. |
| [`std::utf8`](utf8.md) | UTF-8 validation and scalar decoding. |
| [`std::uuid`](uuid.md) | UUID v4 (random) and v7 (timestamp-ordered) generation, parse, and normalize. |
| [`std::validate`](validate.md) | Trait-based field validation: implement Validate, collect FieldErrors into Errors. |

