# `std::http::router`

Status: experimental

Go 1.22-class ServeMux: method-aware path patterns with parameter captures + prefix routes.

## Items

| Item | Signature | Description |
|---|---|---|
| `Router` | `type Router` | Routing table. Build with `Router::new()`, register routes via the verb methods, then pass to `http::serve`. Verb methods return the router so they chain with `|>`. |
| `Params` | `type Params` | Captured path parameters. Read inside a handler with `r.path_value(name) -> String`; returns `""` for an undeclared name. All tiers. |
| `Handler` | `trait Handler` | Anything callable as `Fn(Request, Params) -> Response`. |
| `new` | `fn new() -> http::router::Router` | Allocate a fresh Router handle. |
| `add` | `fn add(router: http::router::Router, method: String, pattern: String) -> Result<(), errors::Error>` | Register a pattern-only route: `(router, method, pattern)`. Used with `lookup` for low-level dispatch. |
| `lookup` | `fn lookup(router: http::router::Router, method: String, path: String) -> Option<http::router::Match>` | Find the index of the first route matching `(method, path)`. Returns `Option<i64>`. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Routing syntax

Patterns follow Go 1.22 `http.ServeMux` style:

```text
/users/{id}          single-segment capture (no /).
/files/{path...}     trailing greedy capture.
/static/*            wildcard (any path under /static/).
/health              literal exact match.
```

Matching precedence: method-specific beats method-agnostic; more
specific (literal > capture > wildcard) beats less specific; first
registered wins among ties.

## Building a router

Verb methods return the router, so a method chain is the idiomatic form:

<!-- fragment -->
```gos
use std::http
use std::http::router

fn main() -> Result<(), http::Error> {
    router::Router::new()
        .get("/health", health)
        .get("/users", list_users)
        .post("/users", create_user)
        .get("/users/{id}", show_user)
        |> |v| http::serve("0.0.0.0:8080", v)
}
```

Binding the router to a `let` between steps also works and is
equivalent - it is just longer. `|>` carries the finished router into
`http::serve`, a free function, which is what the pipe is for.

## Path parameters

Inside a handler, read captured segments via the request:

```gos
use std::http
fn show_user(r: http::Request) -> http::Response {
    let id = r.path_value("id")           // -> String, "" if absent
    let n  = r.path_int("id")             // -> Option<i64>
    let f  = r.path_float("qty")          // -> Option<f64>
    http::Response::text(200, f"id={id}")
}
```
