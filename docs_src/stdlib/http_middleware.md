# `std::http::middleware`

Status: experimental

Composable middleware: request_id, cors, security_headers, hsts, cache_control, etag, rate_limit, body_limit, timeout, compress_gzip, logger, recoverer, basic_auth, bearer_auth, safe_defaults.

## Items

| Item | Signature | Description |
|---|---|---|
| `Handler` | `trait Handler` | Anything serving (Request, Params) -> Response. |
| `Chain` | `type Chain` | Helper for composing middleware in a single value. |
| `new_request_id` | `fn new_request_id() -> String` | Generate a process-monotonic request id string. Available in interp + compiled. |
| `tag` | `fn tag(handler: http::Handler) -> http::Handler` | Wrap a handler (`tag(inner) -> Handler`), prepending `mw:` to each response body. Deterministic composition primitive; available in interp + compiled. |
| `accepts_gzip` | `fn accepts_gzip(request: http::Request) -> bool` | Check an Accept-Encoding header for a gzip token. Available in interp + compiled. |
| `decode_basic_auth` | `fn decode_basic_auth(request: http::Request) -> Option<(String, String)>` | Decode a Basic-auth Authorization header into (user, password). Interp tier. |
| `bearer_ok` | `fn bearer_ok(request: http::Request, verify: Fn(String) -> bool) -> bool` | Run a verify closure on the request's Bearer token; false (without calling verify) when no Bearer header is present. Available in interp + compiled. |
| `CorsConfig` | `type CorsConfig` | CORS configuration. `CorsConfig::permissive()` allows any origin and the common verbs; `CorsConfig::new(origin, methods, headers, max_age)` spells one out. |
| `HstsConfig` | `type HstsConfig` | HSTS configuration. `HstsConfig::safe_default()` is one year for this host; `HstsConfig::strict()` is two years with subdomains and preload. |
| `SecurityHeaders` | `type SecurityHeaders` | Security-header preset. `SecurityHeaders::strict()` adds CSP / COOP / Permissions-Policy on top of the baseline; `SecurityHeaders::off()` emits nothing. |
| `CacheControl` | `type CacheControl` | Cache-Control policy. `CacheControl::no_store()` never caches; `CacheControl::immutable_for(seconds)` marks a content-hashed asset immutable. |
| `RateLimit` | `type RateLimit` | Token-bucket budget. `RateLimit::per_ip(capacity, refill_per_sec)`. |
| `request_id` | `fn request_id(inner: T) -> T` | `request_id(inner) -> Handler` - stamps `X-Request-Id` on every response, using a process-monotonic `req-<n>` counter so a chain's output is identical on every tier. |
| `cors` | `fn cors(inner: T, config: String) -> T` | `cors(inner, config: CorsConfig) -> Handler` - CORS response headers. Example: `middleware::cors(app, middleware::CorsConfig::permissive())`. |
| `security_headers` | `fn security_headers(inner: T, preset: String) -> T` | `security_headers(inner, preset: SecurityHeaders) -> Handler` - X-Content-Type-Options, X-Frame-Options, and Referrer-Policy; the `strict` preset adds CSP, COOP, and Permissions-Policy. Example: `middleware::security_headers(app, middleware::SecurityHeaders::strict())`. |
| `etag` | `fn etag(inner: T) -> T` | `etag(inner) -> Handler` - sets a strong `ETag` derived from the response body, so the same body always yields the same validator. |
| `rate_limit` | `fn rate_limit(inner: T, config: String) -> T` | `rate_limit(inner, config: RateLimit) -> Handler` - token-bucket limiter; past the budget the response becomes 429 with `Retry-After`. Example: `middleware::rate_limit(app, middleware::RateLimit::per_ip(100, 10))`. |
| `hsts` | `fn hsts(inner: T, config: String) -> T` | `hsts(inner, config: HstsConfig) -> Handler` - sets `Strict-Transport-Security`. Example: `middleware::hsts(app, middleware::HstsConfig::safe_default())`. |
| `cache_control` | `fn cache_control(inner: T, config: String) -> T` | `cache_control(inner, config: CacheControl) -> Handler` - sets `Cache-Control`. Example: `middleware::cache_control(app, middleware::CacheControl::no_store())`. |
| `body_limit` | `fn body_limit(inner: T, max_bytes: i64) -> T` | `body_limit(inner, max_bytes: i64) -> Handler` - responses larger than the budget become 413. |
| `compress_gzip` | `fn compress_gzip(inner: T) -> T` | `compress_gzip(inner) -> Handler` - advertises negotiated compression with `Vary: Accept-Encoding`; pair with `middleware::accepts_gzip` to decide per request. |
| `logger` | `fn logger(inner: T) -> T` | `logger(inner) -> Handler` - writes one `[http] <status> <bytes>b` line per response to stderr. |
| `recoverer` | `fn recoverer(inner: T) -> T` | `recoverer(inner) -> Handler` - replaces a 5xx response body with a fixed `internal server error`, so handler internals never leak. |
| `timeout` | `fn timeout(inner: T, budget_ms: i64) -> T` | `timeout(inner, budget_ms: i64) -> Handler` - stamps the budget as `X-Timeout-Ms` for downstream proxies. |
| `basic_auth` | `fn basic_auth(inner: T, realm: String) -> T` | `basic_auth(inner, realm: String) -> Handler` - adds `WWW-Authenticate: Basic realm="..."` to a 401 response. Decode credentials with `middleware::decode_basic_auth`. |
| `bearer_auth` | `fn bearer_auth(inner: T, realm: String) -> T` | `bearer_auth(inner, realm: String) -> Handler` - adds `WWW-Authenticate: Bearer` to a 401 response. Verify tokens with `middleware::bearer_ok`. |
| `safe_defaults` | `fn safe_defaults(inner: T) -> T` | `safe_defaults(inner) -> Handler` - strict security headers, HSTS, and a request id in one wrapper. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
