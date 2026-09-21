// ----------------------------------------------------------------
// Server fixtures.
//
// `web_server.gos` is the only HTTP server in the example set. We
// verify that each tier boots the listener within the boot
// budget, responds 200 to `GET /health`, and exits cleanly when
// the test process tears it down. The probe is a hand-rolled
// `TcpStream` so the test depends on no crate-level HTTP client.
// ----------------------------------------------------------------

#[test]
fn web_server_smoke_vm() {
    server_smoke(Tier::Vm);
}

#[test]
fn web_server_smoke_cranelift() {
    server_smoke(Tier::Cranelift);
}

#[test]
fn web_server_smoke_llvm() {
    server_smoke(Tier::Llvm);
}

/// Runs a self-terminating loopback client+server fixture (server
/// goroutines + client in `main` + explicit `process::exit`) on
/// all three tiers sequentially and demands identical stdout and
/// exit codes. These fixtures bind fixed loopback ports, so they
/// are excluded from the parallel SPECS walks (`skip_all`) and
/// serialised under [`SERVER_PORT_LOCK`] here instead.
/// `expect_contains` guards against an all-tiers-identically-broken
/// pass (e.g. every tier printing the same connection error).
fn self_terminating_server_parity(path: &'static str, expect_contains: &[&str]) {
    // Every tier of one fixture is given the same pair of addresses, so the
    // three runs stay comparable, and no two fixtures are given the same pair.
    let fixture = Spec {
        args: leaked_free_addrs(),
        ..spec(path)
    };
    let vm = run_tier(&fixture, Tier::Vm).expect("vm run");
    assert_eq!(
        vm.code,
        Some(0),
        "{path}: vm exit={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        vm.code,
        vm.stdout,
        vm.stderr,
    );
    for needle in expect_contains {
        assert!(
            vm.stdout.contains(needle),
            "{path}: vm stdout missing {needle:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            vm.stdout,
            vm.stderr,
        );
    }
    for tier in [Tier::Cranelift, Tier::Llvm] {
        let run = run_tier(&fixture, tier)
            .unwrap_or_else(|e| panic!("{path}: {} error: {e}", tier.label()));
        if let Some(d) = divergence(&fixture, (Tier::Vm, &vm), (tier, &run)) {
            panic!("{d}\n--- {} stderr ---\n{}", tier.label(), run.stderr);
        }
    }
}

/// `go <stdlib-free-call>` must spawn a goroutine on every tier rather
/// than run inline. The fixture's two-line output is reachable only
/// when the spawned `Barrier::wait` runs asynchronously (it is one of
/// two barrier parties; main is the other). A synchronous inline call
/// would deadlock main on the barrier and print nothing. Asserting the
/// exact output plus cross-tier parity proves the spawn is async and
/// identical across the bytecode VM, Cranelift JIT, and LLVM AOT.
#[test]
fn go_stdlib_spawn_is_async_across_tiers() {
    let fixture = spec("feature-testing-examples/go_stdlib_spawn.gos");
    let expected = "main reached barrier\nreleased\n";
    let vm = run_tier(&fixture, Tier::Vm).expect("vm run");
    assert_eq!(
        normalize_newlines(&vm.stdout),
        expected,
        "vm stdout\n--- stderr ---\n{}",
        vm.stderr,
    );
    assert_eq!(vm.code, Some(0), "vm exit={:?}", vm.code);
    for tier in [Tier::Cranelift, Tier::Llvm] {
        let run =
            run_tier(&fixture, tier).unwrap_or_else(|e| panic!("{} error: {e}", tier.label()));
        if let Some(d) = divergence(&fixture, (Tier::Vm, &vm), (tier, &run)) {
            panic!("{d}\n--- {} stderr ---\n{}", tier.label(), run.stderr);
        }
    }
}

/// The one-shot client verbs `http::head` / `options` / `post` / `put`
/// / `delete` each lower to a per-verb `gos_rt_http_<verb>` shim so the
/// method string is fixed at the runtime boundary; the request method,
/// body, and Content-Type must round-trip bit-identically on every tier.
#[test]
fn http_client_verbs_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_client_verbs.gos",
        &[
            "get status=200 body=m=GET b= ct=",
            "options status=200 body=m=OPTIONS b= ct=",
            "post status=200 body=m=POST b=hello-post ct=application/json",
            "put status=200 body=m=PUT b=hello-put ct=text/plain",
            "delete status=200 body=m=DELETE b=hello-delete ct=",
            "head status=200",
        ],
    );
}

/// The canonical classifier free functions
/// `http::middleware::decode_basic_auth` (header -> Option<(user, pass)>)
/// and `http::websocket::is_websocket_upgrade` (request -> bool) must
/// classify bit-identically on every tier, degrading to `None` / `false`
/// when the relevant headers are absent.
#[test]
fn http_middleware_ws_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_middleware_ws.gos",
        &[
            "A status=200 body=cred=admin:s3cret up=yes",
            "B status=200 body=cred=none up=no",
        ],
    );
}

/// Go-style middleware composition `http::middleware::tag(inner) ->
/// Handler` must wrap a handler and prepend `mw:` to each response body
/// bit-identically on every tier; a double-wrap `tag(tag(App{}))` proves
/// the chained path (the inner middleware serves through
/// `gos_rt_middleware_serve`), yielding `mw:mw:ok`.
#[test]
fn http_middleware_compose_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_middleware_compose.gos",
        &["status=200 body=mw:mw:ok"],
    );
}

/// The composable middleware stack - `cors` + `security_headers` + `etag` +
/// `hsts` + `cache_control` + `compress_gzip` + `timeout` wrapped in a
/// `rate_limit` - must produce the same headers, statuses, and bodies on
/// every tier, including the 429 the token bucket returns once its
/// budget is spent.
#[test]
fn stdlib_http_middleware_stack_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/stdlib_http_middleware_stack.gos",
        &[
            "cors=https://app.test|GET, PUT|X-Api-Key|600",
            "allowed status=200 body=ok",
            "  access-control-allow-origin: *",
            "  content-security-policy: default-src 'self'",
            "  strict-transport-security: max-age=31536000",
            "  cache-control: no-store",
            "  x-timeout-ms: 2500",
            // The limiter sheds before the inner handler runs, so the
            // layers inside it never decorate this answer - only what the
            // limiter itself owes the client is present.
            "limited status=429 body=too many requests",
            "  access-control-allow-origin: <absent>",
            "  retry-after: 1",
        ],
    );
}

/// The bare HTTP free-function aliases `native_client::{get,post,put,delete}`,
/// `proxy::forward`, and `static_files::serve_file` must resolve to their
/// canonical compiled shims and behave bit-identically on every tier.
#[test]
fn http_bare_aliases_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_bare_aliases.gos",
        &[
            "nc_get status=200 body=m=GET blen=0",
            "nc_post status=200 body=m=POST blen=1",
            "nc_put status=200 body=m=PUT blen=1",
            "nc_delete status=200 body=m=DELETE blen=0",
            "proxy_get status=200 body=m=GET blen=0",
            "proxy_post status=200 body=m=POST blen=1",
            "serve_file status=200 body=served-from-disk",
        ],
    );
}

/// `FileServer` byte-range (RFC 7233) responses must be bit-identical on
/// every tier: a single `Range` yields 206 + `Content-Range` + the
/// sliced body, a multi-range yields a 206 `multipart/byteranges` body
/// with the fixed boundary, an out-of-range request yields 416. Both the
/// compiled `gos_rt_file_server_serve` and interp `native_file_server_serve`
/// route through the shared gossamer-runtime Range helpers.
#[test]
fn http_static_range_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_static_range.gos",
        &[
            "single status=206 cr=bytes 2-5/16 body=2345",
            "multi status=206 ct=multipart/byteranges; boundary=gossamer_byteranges_boundary",
            "Content-Range: bytes 0-2/16",
            "Content-Range: bytes 5-7/16",
            "bad status=416 cr=bytes */16",
            "whole status=200 body=0123456789ABCDEF",
        ],
    );
}

/// Bare-`http::Response` handlers (no `Result` wrapper) must serve
/// identically on every tier: the MIR-synthesized `::__ok_wrap`
/// thunk adapts them to the packed-Result handler C-ABI. Covers
/// the `impl http::Handler` env path and the Router bare-fn path.
#[test]
fn http_bare_handler_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_bare_handler.gos",
        &[
            "struct status=200 body=bare struct ok",
            "route status=200 body=bare route ok",
        ],
    );
}

/// A plain `fn` handler must dispatch on every tier: it carries no
/// environment, so it reaches the runtime's two-argument handler ABI
/// through the MIR-synthesized `::__env_wrap` thunk. Covers the direct
/// `http::serve(addr, f)` path and the same function wrapped in
/// middleware.
#[test]
fn http_plain_fn_handler_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_plain_fn_handler.gos",
        &[
            "direct status=200 body=plain fn ok",
            "wrapped status=200 body=wrapped fn ok",
        ],
    );
}

/// A closure written where a handler is taken is typed as that handler, so an
/// unannotated parameter's field reads lower on every tier, including as the
/// argument of a call a `match` destructures.
#[test]
fn http_handler_closure_params_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_handler_closure_params.gos",
        &[
            "object status=201 body={\"path\":\"/o/j\"}",
            "object status=404 body=object miss",
            "routed status=201 body={\"path\":\"/r/j\"}",
            "free status=201 body={\"path\":\"/f/j\"}",
        ],
    );
}

/// The status line carries the phrase the code is registered under on every
/// tier, and an empty one for a code with none - never a borrowed `OK`.
#[test]
fn http_status_reason_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_status_reason.gos",
        &[
            "[HTTP/1.1 200 OK]",
            "[HTTP/1.1 405 Method Not Allowed]",
            "[HTTP/1.1 422 Unprocessable Content]",
            "[HTTP/1.1 599 ]",
        ],
    );
}

/// The middleware that are controls rather than response decorations
/// must decide identically on every tier: a body budget refuses an
/// oversized request before the handler runs, a credential gate answers
/// 401 without running it, and a rate limit refills over time.
#[test]
fn http_middleware_controls_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_middleware_controls.gos",
        &[
            "under status=200 body=body=4",
            "over status=413",
            "anonymous status=401",
            "bearer status=200",
            "limit1 status=200",
            "limit2 status=200",
            "limit3 status=429",
            "limit4 status=200",
        ],
    );
}

/// `std::lifecycle` must answer identically on every tier: readiness
/// gates a probe endpoint, `shutdown()` begins the drain, and readiness
/// drops on its own once it has.
#[test]
fn lifecycle_readiness_shutdown_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/lifecycle_readiness_shutdown.gos",
        &[
            "before ready: 503",
            "after ready: 200",
            "ready after shutdown: false",
        ],
    );
}

/// `http::Server` must configure, bind, serve, and drain identically on
/// every tier: the setters chain, `listen` binds before `serve` blocks so
/// a port-0 assignment reads back, the body budget is the one the server
/// was given, and `shutdown` reports whether the drain finished.
#[test]
fn http_server_object_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_server_object.gos",
        &[
            "bound: true",
            "get status=200 body=hi",
            "big status=413",
            "drained: true",
        ],
    );
}

/// A request carries a `Context` that the server's `request_timeout_ms`
/// deadline cancels, identically on every tier. A handler that watches it
/// stops on time instead of running to completion.
#[test]
fn http_request_context_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_request_context.gos",
        &["cancelled after 300ms"],
    );
}

/// A handler may write its response body as it goes, and the server
/// frames each write to the client, identically on every tier.
#[test]
fn http_response_stream_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_response_stream.gos",
        &["status=200 bytes=42"],
    );
}

/// A request written as a cohort and as an arena must behave identically
/// on every tier: the children are joined before the response, a failing
/// child cancels its siblings and becomes the block's `Err`, and what the
/// handler allocates inside `arena { }` is released wholesale.
#[test]
fn http_request_cohort_arena_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_request_cohort_arena.gos",
        &[
            "fanout: 200 fanout ok: a! b!",
            "fails: 503 dependency failed",
            "assemble: 200 rows=2000",
        ],
    );
}

/// The `http::Client` cookie jar (`Client::builder().cookie_jar(true)`)
/// must persist `Set-Cookie` across requests on the same client and
/// re-send it bit-identically on every tier: the compiled tiers keep a
/// persistent `ureq::Agent` on the boxed client, the interp tier an
/// id-keyed `gossamer_std::http::Client` registry. The handler echoes
/// the `Cookie` header it received on the second request.
#[test]
fn http_client_cookie_jar_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_client_cookie_jar.gos",
        &["login status=200", "me_body=cookie=sid=abc123"],
    );
}

/// `httptest::server` must bind an isolated loopback listener before returning
/// its URL, then serve the requested status/body through the ordinary HTTP
/// client identically on the VM, Cranelift, and LLVM tiers.
#[test]
fn httptest_static_server_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/httptest_static_server.gos",
        &["status=201 body=fixture body"],
    );
}

/// The checked-in diagnostics consumer example uses the same pre-bound
/// `httptest::server` fixture as an application readiness probe. Its success
/// result must include both response diagnostics and a real client transport
/// round trip on the VM, Cranelift, and LLVM tiers.
#[test]
fn http_diagnostics_transport_consumer_parity_across_tiers() {
    self_terminating_server_parity(
        "examples/http_diagnostics_transport.gos",
        &["ready status=200 body=database=up"],
    );
}

/// Request-scoped values (`r.set_value(k, v)` / `r.value(k)`, Go's
/// `context.WithValue`) must read back bit-identically on every tier;
/// re-setting a key overwrites, an absent key yields `""`.
#[test]
fn http_request_values_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_request_values.gos",
        &["status=200 body=user=bob role=admin missing=[]"],
    );
}

/// `r.query_pairs` is the query string as decoded `(name, value)`
/// pairs, derived from the URL on each read. Every tier has to derive
/// them the same way: a `+` is a space, a `%NN` escape is its byte, a
/// name with no `=` pairs with the empty string, and a repeated name is
/// a second entry rather than a replacement.
#[test]
fn http_request_query_pairs_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_request_query_pairs.gos",
        &[
            "status=200 body=path=/list query=a=1&b=hello+world&c&d=x%2Fy&a=2 n=5 \
             [a=1] [b=hello world] [c=] [d=x/y] [a=2]",
        ],
    );
}

/// `r.form_value(key)` reads an x-www-form-urlencoded body field and
/// `r.basic_auth()` decodes the `Authorization: Basic` header into
/// `Option<(String, String)>`; both must read back bit-identically on
/// every tier, degrading to `""` / `None` when absent.
#[test]
fn http_request_form_auth_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_request_form_auth.gos",
        &[
            "status=200 body=form_user=alice form_role=admin missing=[] auth=admin:s3cret",
            "status=200 body=form_user= form_role= missing=[] auth=none",
        ],
    );
}

/// `r.form_file(name)` parses a `multipart/form-data` request body off
/// `raw_body` (boundary from the `Content-Type` header) and returns the
/// matching file part's `filename` / `content_type` / `[u8]` content.
/// The upload echo and the no-body 404 must read back bit-identically
/// on every tier.
#[test]
fn http_form_file_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_form_file.gos",
        &[
            "status=200 body=file=x.txt ctype=text/plain len=5 sum=335",
            "status=404 body=no file",
        ],
    );
}

/// `http::middleware::bearer_ok` runs the caller's verify closure on
/// the request's Bearer token across the C-ABI; a valid token reaches
/// the handler (200), an invalid or absent one is rejected (401).
#[test]
fn http_middleware_bearer_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_middleware_bearer.gos",
        &[
            "valid status=200 body=welcome",
            "wrong status=401 body=unauthorized",
            "none status=401 body=unauthorized",
        ],
    );
}

/// Canonical authenticated API example: a path-parameter router, a
/// `middleware::bearer_ok` auth gate, typed `r.path_int` extraction,
/// and signed `session::sign` / `verify` cookies - all composed in
/// one program that must behave bit-identically on every tier.
#[test]
fn web_auth_api_parity_across_tiers() {
    self_terminating_server_parity(
        "examples/web_auth_api.gos",
        &[
            "login session={\"user\":\"ada\"}",
            "order status=200 body={\"order\":42}",
            "noauth status=401",
        ],
    );
}

/// Router `{id}` / `{rest...}` path captures must reach a Gossamer
/// handler via `r.path_value(name)` bit-identically on every tier.
/// An undeclared capture name yields `""`.
#[test]
fn http_router_params_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_router_params.gos",
        &[
            "A status=200 body=user=42",
            "B status=200 body=file=docs/readme.md",
            "C status=200 body=missing=[]",
        ],
    );
}

/// Typed path extractors `r.path_int` / `r.path_float` (Option<T>) must
/// parse captures and return None on unparseable/absent identically on
/// every tier - exercises the packed-Option C-ABI.
#[test]
fn http_router_typed_params_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_router_typed_params.gos",
        &["A id=42 amt=3.5 raw=42", "B id=-1 amt=-1 raw=notnum"],
    );
}

/// Router verb methods (`get`, `post`, etc.) must return the router so that
/// `|>` chaining composes the route table as an expression. Confirms identical
/// 3-route dispatch on the bytecode VM, Cranelift JIT, and LLVM AOT.
#[test]
fn http_router_chain_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_router_chain.gos",
        &[
            "get_a status=200 body=a-ok",
            "post_b status=201 body=b-created",
            "get_c status=200 body=c-ok",
        ],
    );
}

/// `http::static_files::FileServer` served through `http::serve` must
/// resolve a real file (200 + body + MIME) and 404 a missing path
/// bit-identically on every tier - compiled wires `gos_rt_file_server_*`,
/// interp the `native_file_server_serve` dispatch.
#[test]
fn http_static_file_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_static_file.gos",
        &[
            "status=200 body=static file ok",
            "missing status=404",
            // A directory is served as the index inside it, and one
            // named without a trailing slash is redirected to the form
            // the page's own relative links resolve under.
            "root status=200 body=root index",
            "dir status=200 body=tour index",
            "dir-no-slash status=301 location=/tour/",
        ],
    );
}

/// `http::websocket::accept` (RFC 6455 server handshake) must validate
/// the upgrade headers and build a 101 Response identically on every
/// tier - compiled wires `gos_rt_ws_accept`, interp the native
/// `websocket::accept`; a request without the headers is rejected with
/// the handshake error string.
#[test]
fn http_websocket_accept_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_websocket_accept.gos",
        &[
            "accept_key=s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
            "valid=status=101",
            "reject=missing Upgrade header",
        ],
    );
}

/// Bidirectional WebSocket messaging (RFC 6455): an echo server bound
/// via `websocket::serve` on a goroutine, a `websocket::connect` client
/// that sends a text message and verifies the echo. All three tiers
/// drive the shared `gossamer_ws` framing engine, so the output is
/// bit-identical on the bytecode VM, Cranelift JIT, and LLVM AOT.
#[test]
fn websocket_echo_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/websocket_echo.gos",
        &["ws echo: ok"],
    );
}

/// `http::serve_tls` (server-side HTTPS) terminating a real TLS
/// handshake, plus the three `TcpStream::start_tls*` client modes
/// (skip-verify, public-root verify, custom-CA verify), must behave
/// identically on every tier. A private CA signs a localhost leaf the
/// server presents; `start_tls_insecure` and `start_tls_ca` complete
/// the request while the public-root `start_tls` rejects the private
/// chain - bit-identically on the bytecode VM, Cranelift JIT, and LLVM
/// AOT.
#[test]
fn http_serve_tls_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_serve_tls_roundtrip.gos",
        &["insecure: ok", "default-verify: rejected", "custom-ca: ok"],
    );
}

/// The compiled HTTP server must emit the RFC 9110 origin headers
/// `Date` and `Server` that the interp tier already sends, so a client
/// observes the same response-header set on every tier.
#[test]
fn http_server_headers_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_server_headers.gos",
        &["server-header: true", "date-header: true"],
    );
}

/// `match http::serve(..) { Err(e) => println!("{}", e) }` must
/// compile and run identically on every tier. The serve expression
/// is `Result<(), errors::Error>`-typed (the Err binding used to
/// lower as void and break LLVM with "sext void to i64"), and a
/// bind failure is the caller's `Err` value - printed via the match
/// arm and exit 0 on every tier.
#[test]
fn http_serve_err_binding_parity_across_tiers() {
    let fixture = spec("feature-testing-examples/http_serve_err_binding.gos");
    let expected_stdout = "about to bind\nError: http::serve: invalid socket address\n";
    for tier in [Tier::Vm, Tier::Cranelift, Tier::Llvm] {
        let run =
            run_tier(&fixture, tier).unwrap_or_else(|e| panic!("{} error: {e}", tier.label()));
        assert_eq!(
            run.code,
            Some(0),
            "{} must exit 0 - serve failure is the caller's Err value, not a panic\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            tier.label(),
            run.stdout,
            run.stderr,
        );
        assert_eq!(run.stdout, expected_stdout, "{} stdout", tier.label());
        assert!(
            !run.stderr.contains("GX0005"),
            "{} must not panic on serve failure\n--- stderr ---\n{}",
            tier.label(),
            run.stderr,
        );
    }
}

/// `http_h3::serve` is the QUIC + HTTP/3 server entry, wired across
/// all three tiers through the shared `gossamer-http3` engine. A full
/// QUIC round trip is too slow / nondeterministic for the parity walk
/// (the loopback handshake takes tens of seconds), so this fixture
/// exercises the same handler-fn-ptr dispatch and `Result<(), Error>`
/// surface deterministically: HTTP/3 mandates TLS, so the server
/// reads the cert / key PEM before binding, and a missing cert file
/// is the caller's `Err` value on every tier - not a panic. The cert
/// read goes through `std::fs::read` on both tiers, so the OS error
/// tail is identical; this pins the stable prefix and asserts
/// cross-tier equality of the full line.
#[test]
fn http3_serve_err_binding_parity_across_tiers() {
    let fixture = spec("feature-testing-examples/http3_serve_err_binding.gos");
    let stable_prefix = "about to bind\nError: http_h3::serve: h3 io: read cert:";
    let mut outputs: Vec<(String, String)> = Vec::new();
    for tier in [Tier::Vm, Tier::Cranelift, Tier::Llvm] {
        let run =
            run_tier(&fixture, tier).unwrap_or_else(|e| panic!("{} error: {e}", tier.label()));
        assert_eq!(
            run.code,
            Some(0),
            "{} must exit 0 - a cert read failure is the caller's Err value, not a panic\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            tier.label(),
            run.stdout,
            run.stderr,
        );
        assert!(
            run.stdout.starts_with(stable_prefix),
            "{} stdout must carry the stable cert-read-error prefix\n--- stdout ---\n{}",
            tier.label(),
            run.stdout,
        );
        assert!(
            !run.stderr.contains("GX0005"),
            "{} must not panic on a cert read failure\n--- stderr ---\n{}",
            tier.label(),
            run.stderr,
        );
        outputs.push((tier.label().to_string(), run.stdout));
    }
    // The OS error tail is machine-specific but identical across
    // tiers on the same host: every tier's full stdout must match.
    let (first_label, first_out) = &outputs[0];
    for (label, out) in &outputs[1..] {
        assert_eq!(
            out, first_out,
            "{label} stdout must match {first_label} byte-for-byte",
        );
    }
}

/// Inbound server request headers must be readable identically on
/// every tier: `for (name, value) in r.headers` (the historical
/// MIR-lowering panic / first-request segfault shape), borrowed
/// `&r.headers` lookups, the lowercase/dedupe/name-sorted interp
/// `Headers` view, and `r.path` query-stripping + `r.query` parity.
#[test]
fn http_request_headers_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_request_headers.gos",
        &[
            "status=200",
            "custom=2 alpha=a1 beta=b2 path=/echo query=k=1&n=2",
        ],
    );
}

/// Handler-set response headers must reach the wire identically on
/// every tier: `Response::with_header` is replace-then-push (the
/// second same-name attach wins, case-insensitively) and the
/// constructor's content type survives alongside custom headers
/// (explicit header > `content_type` field > text/plain default).
#[test]
fn http_response_headers_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_response_headers.gos",
        &[
            "status=201 body=created",
            "x-a=2",
            "x-b=3",
            "content-type=text/plain; charset=utf-8",
        ],
    );
}

/// Programmer-selectable redirect policy must behave identically on
/// every tier: the default `Client::builder().build()` follows the
/// 302 to the final 200 body, `max_redirects(0)` returns the 302 raw
/// with its Location header intact, and `request_bytes` honors the
/// same configured client.
#[test]
fn http_redirect_policy_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_redirect_policy.gos",
        &[
            "a_status=200 a_body=landed",
            "b_status=302 b_location=/data",
            "c_status=200 c_body=hi",
        ],
    );
}

/// `ResponseStream::next_chunk(max)` must drain a streamed body in
/// identical byte chunks on every tier: the Some payload is a
/// packed `elem_bytes=1` `GosVec` (the `raw_bytes` representation
/// contract), consumed through the canonical `while let
/// Some(chunk)` shape with len / indexing / for-loop sum /
/// `hex::encode` all reading byte-stride.
#[test]
fn http_next_chunk_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_next_chunk.gos",
        &[
            "len=4 b0=65 hex=41c3bfe2",
            "len=4 b0=132 hex=84a27a41",
            "len=2 b0=66 hex=4243",
            "total=10 sum=1291",
        ],
    );
}

/// Streamed server responses (`Response::stream` - the
/// proxy-passthrough shape) must behave identically on every tier:
/// the proxy opens a fresh upstream `http::stream` per request, the
/// server drains it as chunked frames, and constructing the
/// response consumes the `ResponseStream` handle (`next_chunk`
/// yields `None` afterwards - the /consumed handler answers 500 if
/// it ever sees leftover data).
#[test]
fn http_proxy_stream_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_proxy_stream.gos",
        &[
            "first status=200 ct=text/plain; charset=utf-8 len=37 \
             body=upstream payload: the quick brown fox",
            "second status=200 ct=text/plain; charset=utf-8 len=37 \
             body=upstream payload: the quick brown fox",
            "consumed status=200 ct=text/plain; charset=utf-8 len=37 \
             body=upstream payload: the quick brown fox",
        ],
    );
}

/// Integration fixture chaining the closed client/server gaps like
/// a real proxy session: binary `request_bytes` upload observed via
/// the server's `r.raw_body` (NUL byte included), a NUL-embedded
/// byte-array response body served in full by the native h1 writer
/// (`body_bytes` preferred over the c-string mirror), handler
/// `with_header` reaching the wire and read back through the
/// client's `resp.headers` then forwarded by a proxy hop, a 302
/// held raw under `max_redirects(0)`, and a `next_chunk` drain of a
/// `Response::stream` passthrough.
#[test]
fn http_roundtrip_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_roundtrip.gos",
        &[
            "echo status=200 body=len=4 first=1 last=255 sum=258",
            "nul status=200 len=5 hex=4100420043",
            "hop status=302 location=/data",
            "fwd status=200 body=fwd:landed-data x-up=u1",
            "stream total=31 chunks=4 first_hex=73747265616d6564",
        ],
    );
}

/// `resp.raw_bytes` is a packed `elem_bytes=1` `GosVec`; every
/// consumer op (indexing, for-loop, `first` / `last` / `contains`
/// / `count_of` / `index_of`, `hex::encode`, element writes) must
/// read byte-stride identically on every tier.
#[test]
fn http_raw_bytes_parity_across_tiers() {
    self_terminating_server_parity(
        "feature-testing-examples/http_raw_bytes.gos",
        &["hex=41c3bfe284a27a", "mutated_v0=66 hex2=42c3bfe284a27a"],
    );
}

/// A loopback address nothing on the box is listening on.
///
/// The kernel picks the port from its ephemeral range and the test
/// hands it to the fixture, so a server fixture competes neither with
/// another tier's run nor with whatever a developer left listening on
/// a well-known number.
fn free_addr() -> String {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = probe.local_addr().expect("read the bound address");
    drop(probe);
    addr.to_string()
}

/// Two distinct loopback addresses nothing is listening on, leaked so a
/// `Spec` can name them. Both listeners are held while the pair is read, so
/// the kernel cannot answer with one port twice.
fn leaked_free_addrs() -> &'static [&'static str] {
    let first = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let second = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let pair = [
        first.local_addr().expect("read the bound address").to_string(),
        second.local_addr().expect("read the bound address").to_string(),
    ];
    drop(first);
    drop(second);
    let leaked: Vec<&'static str> = pair
        .into_iter()
        .map(|a| &*Box::leak(a.into_boxed_str()))
        .collect();
    Box::leak(leaked.into_boxed_slice())
}

fn server_smoke(tier: Tier) {
    let spec = SPECS
        .iter()
        .find(|s| s.path == "examples/web_server.gos")
        .expect("web_server spec");
    let server = spec.server.expect("server fixture");
    let addr = free_addr();
    let deadline = Instant::now() + PER_RUN_TIMEOUT;

    let src = workspace_root().join(spec.path);
    let (mut child, scratch) = match tier {
        Tier::Vm => {
            let child = Command::new(gos_bin())
                .arg("run")
                .arg(&src)
                .arg(&addr)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn gos web_server");
            (child, None)
        }
        compiled => {
            let release = matches!(compiled, Tier::Llvm);
            let scratch = fresh_dir(&format!("server-{}", compiled.label()));
            let bin = match build_native(&src, release, &scratch) {
                Ok(p) => p,
                Err(e) => panic!("{} build of web_server.gos failed: {e}", compiled.label()),
            };
            let child = Command::new(&bin)
                .arg(&addr)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn web_server binary");
            (child, Some(scratch))
        }
    };

    std::thread::sleep(Duration::from_millis(server.boot_ms));

    let probe = http_probe(&addr, server.probe_path, deadline);
    let _ = child.kill();
    let captured = read_child_streams(&mut child);
    let _ = child.wait();
    if let Some(s) = scratch {
        let _ = fs::remove_dir_all(s);
    }

    // If the child reported a bind failure mid-run (e.g. another
    // process raced to grab the port between our pre-flight check
    // and the spawn), surface that explicitly instead of letting
    // the test panic on a status mismatch from the other server.
    let bind_raced = captured.stderr.contains("bind") && captured.stderr.contains("in use");
    assert!(
        !bind_raced,
        "{} web_server: bind raced - port {} taken before child could listen\n--- child stderr ---\n{}",
        tier.label(),
        addr,
        captured.stderr,
    );

    let (status, body) = probe.unwrap_or_else(|e| {
        panic!(
            "{} web_server probe failed: {e}\n--- child stdout ---\n{}\n--- child stderr ---\n{}",
            tier.label(),
            captured.stdout,
            captured.stderr,
        );
    });
    assert_eq!(
        status,
        200,
        "{} web_server returned status {status}, body={body:?}\n--- child stdout ---\n{}\n--- child stderr ---\n{}",
        tier.label(),
        captured.stdout,
        captured.stderr,
    );
    assert!(
        !body.is_empty(),
        "{} web_server returned empty body\n--- child stdout ---\n{}\n--- child stderr ---\n{}",
        tier.label(),
        captured.stdout,
        captured.stderr,
    );
}

struct ChildOutput {
    stdout: String,
    stderr: String,
}

/// Drains the child's piped stdout / stderr. Must be called after
/// `kill()` and before `wait()` so the buffered output is not lost
/// when the kernel reclaims the pipes. Either end may be missing
/// if the caller did not configure `Stdio::piped()`.
fn read_child_streams(child: &mut Child) -> ChildOutput {
    use std::io::Read;
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut stdout);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    ChildOutput { stdout, stderr }
}

/// Probes `addr` with `GET {path}` and returns the status code and
/// body. Retries the *whole* attempt (connect + write + read) on
/// any transient error until `deadline`. A single attempt can fail
/// for reasons that resolve a moment later - the kernel may
/// complete a TCP handshake against a not-quite-ready application
/// (the listen backlog masks slow accept loops), and the read then
/// times out with EAGAIN even though the server will be serving
/// within a second. Retrying the full handshake decouples the test
/// from runtime bootstrap timing.
fn http_probe(addr: &str, path: &str, deadline: Instant) -> Result<(u16, String), String> {
    let socket = addr
        .parse::<std::net::SocketAddr>()
        .map_err(|e| format!("parse addr {addr}: {e}"))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let mut last_err = String::from("probe never attempted");
    while Instant::now() < deadline {
        match probe_once(&socket, req.as_bytes(), deadline) {
            Ok(reply) => return Ok(reply),
            Err(e) => {
                last_err = e;
                std::thread::sleep(Duration::from_millis(120));
            }
        }
    }
    Err(format!("probe deadline reached; last error: {last_err}"))
}

fn probe_once(
    socket: &std::net::SocketAddr,
    req: &[u8],
    deadline: Instant,
) -> Result<(u16, String), String> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("deadline elapsed before attempt".to_string());
    }
    let connect_budget = remaining.min(Duration::from_secs(2));
    let mut stream =
        TcpStream::connect_timeout(socket, connect_budget).map_err(|e| format!("connect: {e}"))?;
    let read_budget = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(2))
        .max(Duration::from_millis(200));
    stream
        .set_read_timeout(Some(read_budget))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream
        .set_write_timeout(Some(read_budget))
        .map_err(|e| format!("set_write_timeout: {e}"))?;
    stream.write_all(req).map_err(|e| format!("write: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| format!("read status: {e}"))?;
    let parts: Vec<&str> = status_line.split_whitespace().collect();
    if parts.len() < 2 || !parts[0].starts_with("HTTP/") {
        return Err(format!("malformed status line: {status_line:?}"));
    }
    let code = parts[1]
        .parse::<u16>()
        .map_err(|e| format!("parse status: {e}"))?;
    let mut body = Vec::new();
    let _ = reader.read_to_end(&mut body);
    Ok((code, String::from_utf8_lossy(&body).into_owned()))
}

// ----------------------------------------------------------------
// LLVM strict-backend gate.
//
// `gos build --release` must lower frontend-valid programs through
// LLVM directly: a lowering gap is a hard build error, so this test
// fails the moment any example body trips an LLVM backend lowering bug.
// ----------------------------------------------------------------

/// One round-robin group of the strict-lowering battery (invoked by the
/// `llvm_strict_lower_group_N` tests). Builds only (to fresh per-spec
/// dirs), so groups can run concurrently without the parity lock.
fn strict_lowering_group(group: usize) {
    let mut lowering_gaps: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (idx, spec) in SPECS.iter().enumerate() {
        if idx % PARITY_GROUPS != group {
            continue;
        }
        if spec.skip_all.is_some() {
            continue;
        }
        let src = workspace_root().join(spec.path);
        let scratch = fresh_dir(&format!("strict-{}", file_tag(spec.path)));
        let out = Command::new(gos_bin())
            .arg("build")
            .arg("--release")
            .arg("--out-dir")
            .arg(&scratch)
            .arg(&src)
            .output()
            .expect("spawn gos build --release");
        let _ = fs::remove_dir_all(&scratch);
        if out.status.success() {
            continue;
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("llvm backend internal lowering bug") {
            let summary = stderr
                .lines()
                .find(|l| l.contains("llvm backend internal lowering bug"))
                .unwrap_or(&stderr)
                .trim()
                .to_string();
            lowering_gaps.push(format!("{}: {summary}", spec.path));
        } else {
            errors.push(format!(
                "{}: gos build --release failed: {stderr}",
                spec.path
            ));
        }
    }
    if !lowering_gaps.is_empty() || !errors.is_empty() {
        let mut report = String::new();
        if !lowering_gaps.is_empty() {
            report.push_str(&format!("{} LLVM lowering gap(s):\n", lowering_gaps.len()));
            for f in &lowering_gaps {
                report.push_str("  ");
                report.push_str(f);
                report.push('\n');
            }
        }
        if !errors.is_empty() {
            report.push_str(&format!("\n{} build error(s):\n", errors.len()));
            for e in &errors {
                report.push_str("  ");
                report.push_str(e);
                report.push('\n');
            }
        }
        panic!("{report}");
    }
}

/// Forced-JIT correctness gate. `GOSSAMER_JIT_THRESHOLD=1` promotes every
/// function to native on its first call - the most aggressive promotion
/// possible, and the policy the eager-promotion path relies on. The JIT
/// must produce output identical to the bytecode interpreter (`GOS_JIT=0`)
/// on the shapes it once silently miscompiled or segfaulted on: a closure
/// passed to a higher-order call, a `&mut` aggregate parameter,
/// `Vec`-parameter slice-pattern matching, and recursive enum/vector/string
/// ownership at the VM/native boundary. The eligibility gate
/// (`body_jit_unsupported`) keeps unsupported bodies on bytecode, while the
/// supported recursive fixtures force marshal/free paths through native code;
/// a divergence here means either the gate let an un-lowerable body through or
/// a supported boundary shape lost ownership parity.
#[test]
fn forced_jit_matches_bytecode_on_unlowerable_shapes() {
    let root = workspace_root();
    let fixtures = [
        "examples/factorial.gos",
        "feature-testing-examples/string_append_realloc.gos",
        "feature-testing-examples/slice_patterns.gos",
        "feature-testing-examples/jit_native_marshal.gos",
        "feature-testing-examples/vec_vec_i64_jit.gos",
        "feature-testing-examples/json_parse_jit.gos",
        "feature-testing-examples/enum_transform_jit.gos",
        "feature-testing-examples/for_kv_enum_payload.gos",
    ];
    for rel in fixtures {
        let path = root.join(rel);
        let run = |key: &str, val: &str| {
            let out = Command::new(gos_bin())
                .arg("run")
                .arg(&path)
                .env(key, val)
                .output()
                .unwrap_or_else(|e| panic!("spawn gos {rel}: {e}"));
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )
        };
        let (bc_code, bc_out) = run("GOS_JIT", "0");
        let (jit_code, jit_out) = run("GOSSAMER_JIT_THRESHOLD", "1");
        assert_eq!(
            bc_out, jit_out,
            "{rel}: forced-JIT stdout diverged from bytecode - the JIT eligibility gate let an un-lowerable body through"
        );
        assert_eq!(
            bc_code, jit_code,
            "{rel}: forced-JIT exit code diverged from bytecode"
        );
    }
}

// ----------------------------------------------------------------
// A server whose handlers share a pool of tokens through a channel.
//
// The runtime serves each connection on an OS thread rather than a
// goroutine, so with more requests in flight than the pool has
// tokens, a handler parks on a channel while no goroutine exists at
// all. That is ordinary backpressure - the peers that will return a
// token are the other connections - and the deadlock report must not
// fire on it, on any tier.
// ----------------------------------------------------------------

/// More than `http_channel_pool.gos` has tokens, so some handlers must wait.
const POOL_CLIENTS: usize = 8;

#[test]
fn http_channel_pool_vm() {
    channel_pool_smoke(Tier::Vm);
}

#[test]
fn http_channel_pool_cranelift() {
    channel_pool_smoke(Tier::Cranelift);
}

#[test]
fn http_channel_pool_llvm() {
    channel_pool_smoke(Tier::Llvm);
}

fn channel_pool_smoke(tier: Tier) {
    let deadline = Instant::now() + PER_RUN_TIMEOUT;
    let src = workspace_root().join("feature-testing-examples/http_channel_pool.gos");
    let addr = free_addr();

    let (mut child, scratch) = match tier {
        Tier::Vm => {
            let child = Command::new(gos_bin())
                .arg("run")
                .arg(&src)
                .arg(&addr)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn gos http_channel_pool");
            (child, None)
        }
        compiled => {
            let release = matches!(compiled, Tier::Llvm);
            let scratch = fresh_dir(&format!("chanpool-{}", compiled.label()));
            let bin = match build_native(&src, release, &scratch) {
                Ok(p) => p,
                Err(e) => panic!("{} build of http_channel_pool.gos failed: {e}", compiled.label()),
            };
            let child = Command::new(&bin)
                .arg(&addr)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn http_channel_pool binary");
            (child, Some(scratch))
        }
    };

    std::thread::sleep(Duration::from_millis(1500));

    // Every client is in flight at once, so the pool is emptied and the
    // handlers that find it empty park on the channel.
    let results: Vec<Result<(u16, String), String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..POOL_CLIENTS)
            .map(|_| scope.spawn(|| http_probe(&addr, "/pool", deadline)))
            .collect();
        handles.into_iter().map(|h| h.join().expect("probe")).collect()
    });

    let _ = child.kill();
    let captured = read_child_streams(&mut child);
    let _ = child.wait();
    if let Some(s) = scratch {
        let _ = fs::remove_dir_all(s);
    }

    assert!(
        !captured.stderr.contains("deadlock"),
        "{} channel pool: reported a deadlock while its handlers waited on the pool\n\
         --- child stdout ---\n{}\n--- child stderr ---\n{}",
        tier.label(),
        captured.stdout,
        captured.stderr,
    );
    for (i, result) in results.iter().enumerate() {
        let (status, body) = result.as_ref().unwrap_or_else(|e| {
            panic!(
                "{} channel pool: client {i} failed: {e}\n--- child stdout ---\n{}\n\
                 --- child stderr ---\n{}",
                tier.label(),
                captured.stdout,
                captured.stderr,
            )
        });
        assert_eq!(
            *status,
            200,
            "{} channel pool: client {i} got {status}, body={body:?}",
            tier.label(),
        );
        // `http_probe` answers the headers and the body together, so the
        // payload is what the reply ends with.
        assert!(
            body.ends_with("served"),
            "{} channel pool: client {i} body {body:?}",
            tier.label(),
        );
    }
}
