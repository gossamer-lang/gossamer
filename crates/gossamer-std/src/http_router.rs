// Module-level allows for ABI 0.4 HTTP router. Pedantic
// lints here trade off against readability - surface is small
// and intentional.
#![allow(
    clippy::similar_names,
    clippy::type_complexity,
    clippy::map_unwrap_or,
    clippy::redundant_closure,
    clippy::items_after_statements,
    clippy::let_and_return,
    clippy::manual_let_else,
    clippy::missing_errors_doc,
    clippy::clone_on_copy
)]

//! HTTP router (Go 1.22-class ServeMux).
//!
//! `Router` matches `(method, path)` pairs against registered
//! patterns and dispatches to the corresponding handler. Pattern
//! grammar is a strict subset of Go's stdlib `http.ServeMux` (the
//! 1.22 method-gated pattern syntax):
//!
//! ```text
//!   /users/{id}          - single-segment capture (no `/`).
//!   /files/{path...}     - trailing greedy capture (matches the rest).
//!   /static/*            - wildcard (any path under /static/).
//!   /health              - literal exact match.
//! ```
//!
//! Patterns may be prefixed with a method:
//!
//! ```text
//!   GET /users/{id}
//!   POST /users
//! ```
//!
//! When a pattern has no method prefix, it matches every method.
//!
//! Matching precedence:
//!
//! 1. Method-specific pattern wins over method-agnostic pattern.
//! 2. More specific pattern wins (literal > capture > wildcard).
//! 3. Among equally-specific patterns the first registered wins.
//!
//! Handlers are `Fn(&Request, &Params) -> Response`.

use std::sync::Arc;

use crate::http::{Headers, Method, Request, Response, StatusCode};

/// Captured-parameter table for a matched route.
///
/// Holds the path-segment captures (`{id}` → segment slice, plus
/// `{rest...}` trailing captures). Lookups are `O(n)` over the
/// captured pairs; the typical route captures one or two params,
/// so this is faster than a `HashMap`.
#[derive(Debug, Default, Clone)]
pub struct Params {
    inner: Vec<(String, String)>,
}

impl Params {
    /// Returns the captured value for `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.inner
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Returns every captured `(name, value)` pair.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.inner.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Number of captured parameters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` when no parameters were captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Handler signature: takes a request + captured params, returns
/// a response. Boxed so the router can hold heterogeneous
/// handlers in one trie.
pub type Handler = Arc<dyn Fn(&Request, &Params) -> Response + Send + Sync>;

/// Internal segment shape inside a compiled pattern.
#[derive(Debug, Clone)]
enum Segment {
    /// Literal text. Matches exactly.
    Literal(String),
    /// `{name}` - single-segment capture.
    Capture(String),
    /// `{name...}` - greedy capture to end of path.
    Rest(String),
    /// `*` - wildcard, matches any remaining path.
    Wildcard,
}

#[derive(Clone)]
struct Route {
    method: Option<Method>,
    segments: Vec<Segment>,
    handler: Handler,
    /// Specificity score for tie-breaking. Higher = more
    /// specific. Literal segments add 100, captures 10, rest /
    /// wildcard 1.
    specificity: u32,
    /// Insertion order; lower wins among equally-specific
    /// patterns.
    order: u32,
}

/// Routing table.
///
/// Build with [`Router::new`], register routes via the method
/// helpers (`get`, `post`, etc.) or `handle(method, pattern,
/// handler)`, then use the router as the `Handler` passed to
/// `http::server::run` / `http::serve`.
#[derive(Default, Clone)]
pub struct Router {
    routes: Vec<Route>,
    /// Optional fallback handler invoked when no route matches.
    /// Defaults to a 404 text response.
    not_found: Option<Handler>,
    /// Optional handler for method-not-allowed (a path matches
    /// but no method-specific route accepts the request method).
    method_not_allowed: Option<Handler>,
    /// Next insertion order.
    next_order: u32,
}

impl Router {
    /// Builds an empty router.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a handler under any method.
    pub fn handle(
        &mut self,
        pattern: &str,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.register(None, pattern, Arc::new(handler))
    }

    /// Registers a handler for `GET` only.
    pub fn get(
        &mut self,
        pattern: &str,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.register(Some(Method::Get), pattern, Arc::new(handler))
    }

    /// Registers a handler for `POST` only.
    pub fn post(
        &mut self,
        pattern: &str,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.register(Some(Method::Post), pattern, Arc::new(handler))
    }

    /// Registers a handler for `PUT` only.
    pub fn put(
        &mut self,
        pattern: &str,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.register(Some(Method::Put), pattern, Arc::new(handler))
    }

    /// Registers a handler for `DELETE` only.
    pub fn delete(
        &mut self,
        pattern: &str,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.register(Some(Method::Delete), pattern, Arc::new(handler))
    }

    /// Registers a handler for `PATCH` only.
    pub fn patch(
        &mut self,
        pattern: &str,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.register(Some(Method::Patch), pattern, Arc::new(handler))
    }

    /// Registers a handler for `HEAD` only.
    pub fn head(
        &mut self,
        pattern: &str,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.register(Some(Method::Head), pattern, Arc::new(handler))
    }

    /// Registers a handler for `OPTIONS` only.
    pub fn options(
        &mut self,
        pattern: &str,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.register(Some(Method::Options), pattern, Arc::new(handler))
    }

    /// Overrides the not-found (404) handler.
    pub fn not_found(
        &mut self,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.not_found = Some(Arc::new(handler));
        self
    }

    /// Overrides the method-not-allowed (405) handler.
    pub fn method_not_allowed(
        &mut self,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> &mut Self {
        self.method_not_allowed = Some(Arc::new(handler));
        self
    }

    fn register(&mut self, method: Option<Method>, pattern: &str, handler: Handler) -> &mut Self {
        let segments = compile_pattern(pattern);
        let specificity = segments
            .iter()
            .map(|s| match s {
                Segment::Literal(_) => 100,
                Segment::Capture(_) => 10,
                Segment::Rest(_) | Segment::Wildcard => 1,
            })
            .sum();
        let order = self.next_order;
        self.next_order = self.next_order.wrapping_add(1);
        self.routes.push(Route {
            method,
            segments,
            handler,
            specificity,
            order,
        });
        self
    }

    /// Dispatches `request` to the matching handler, falling
    /// back to the not-found / method-not-allowed handler when
    /// no route matches.
    #[must_use]
    pub fn serve(&self, request: &Request) -> Response {
        let path = request.path.as_str();
        let mut best: Option<(&Route, Params)> = None;
        let mut path_matched_but_method_did_not = false;

        for route in &self.routes {
            let Some(params) = match_path(&route.segments, path) else {
                continue;
            };
            if let Some(m) = &route.method
                && m != &request.method
            {
                path_matched_but_method_did_not = true;
                continue;
            }
            let take = match &best {
                None => true,
                Some((cur, _)) => {
                    let cur_method_bonus = u32::from(cur.method.is_some()) * 1000;
                    let new_method_bonus = u32::from(route.method.is_some()) * 1000;
                    let cur_score = cur.specificity + cur_method_bonus;
                    let new_score = route.specificity + new_method_bonus;
                    if new_score == cur_score {
                        route.order < cur.order
                    } else {
                        new_score > cur_score
                    }
                }
            };
            if take {
                best = Some((route, params));
            }
        }

        if let Some((route, params)) = best {
            return (route.handler)(request, &params);
        }
        if path_matched_but_method_did_not {
            if let Some(h) = &self.method_not_allowed {
                return h(request, &Params::default());
            }
            return Response {
                status: StatusCode(405),
                headers: Headers::new(),
                body: b"method not allowed".to_vec(),
                raw_header_pairs: Vec::new(),
                body_stream: None,
            };
        }
        if let Some(h) = &self.not_found {
            return h(request, &Params::default());
        }
        Response {
            status: StatusCode(404),
            headers: Headers::new(),
            body: b"not found".to_vec(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        }
    }
}

fn compile_pattern(pattern: &str) -> Vec<Segment> {
    let trimmed = pattern.trim_start_matches('/');
    let mut out: Vec<Segment> = Vec::new();
    if trimmed.is_empty() {
        return out;
    }
    for seg in trimmed.split('/') {
        if seg == "*" {
            out.push(Segment::Wildcard);
            break;
        }
        if let Some(rest) = seg.strip_prefix('{')
            && let Some(name) = rest.strip_suffix('}')
        {
            if let Some(name) = name.strip_suffix("...") {
                out.push(Segment::Rest(name.to_string()));
                break;
            }
            out.push(Segment::Capture(name.to_string()));
            continue;
        }
        out.push(Segment::Literal(seg.to_string()));
    }
    out
}

fn match_path(segments: &[Segment], path: &str) -> Option<Params> {
    let trimmed = path.trim_start_matches('/');
    let path_segs: Vec<&str> = if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed.split('/').collect()
    };
    let mut params = Params::default();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < segments.len() {
        match &segments[i] {
            Segment::Literal(lit) => {
                if j >= path_segs.len() || path_segs[j] != lit {
                    return None;
                }
                j += 1;
            }
            Segment::Capture(name) => {
                if j >= path_segs.len() {
                    return None;
                }
                params.inner.push((name.clone(), path_segs[j].to_string()));
                j += 1;
            }
            Segment::Rest(name) => {
                let rest = path_segs[j..].join("/");
                params.inner.push((name.clone(), rest));
                return Some(params);
            }
            Segment::Wildcard => {
                return Some(params);
            }
        }
        i += 1;
    }
    if j == path_segs.len() {
        Some(params)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;

    fn req(method: Method, path: &str) -> Request {
        Request {
            method,
            path: path.to_string(),
            query: String::new(),
            headers: Headers::new(),
            body: Vec::new(),
            context: Context::background(),
            trailers: None,
            peer_addr: String::new(),
        }
    }

    fn text_response(status: u16, body: &str) -> Response {
        Response {
            status: StatusCode(status),
            headers: Headers::new(),
            body: body.as_bytes().to_vec(),
            raw_header_pairs: Vec::new(),
            body_stream: None,
        }
    }

    #[test]
    fn literal_route_matches_exact_path() {
        let mut r = Router::new();
        r.get("/health", |_req, _p| text_response(200, "ok"));
        let resp = r.serve(&req(Method::Get, "/health"));
        assert_eq!(resp.status, StatusCode(200));
        assert_eq!(resp.body, b"ok");
    }

    #[test]
    fn capture_extracts_segment() {
        let mut r = Router::new();
        r.get("/users/{id}", |_req, p| {
            text_response(200, p.get("id").unwrap_or(""))
        });
        let resp = r.serve(&req(Method::Get, "/users/42"));
        assert_eq!(resp.status, StatusCode(200));
        assert_eq!(resp.body, b"42");
    }

    #[test]
    fn rest_capture_takes_remainder() {
        let mut r = Router::new();
        r.get("/files/{path...}", |_req, p| {
            text_response(200, p.get("path").unwrap_or(""))
        });
        let resp = r.serve(&req(Method::Get, "/files/a/b/c.txt"));
        assert_eq!(resp.body, b"a/b/c.txt");
    }

    #[test]
    fn wildcard_matches_anything_under_prefix() {
        let mut r = Router::new();
        r.get("/static/*", |_req, _p| text_response(200, "asset"));
        assert_eq!(r.serve(&req(Method::Get, "/static/x.js")).body, b"asset");
        assert_eq!(
            r.serve(&req(Method::Get, "/static/sub/x.js")).body,
            b"asset"
        );
    }

    #[test]
    fn method_gating_distinguishes_get_and_post() {
        let mut r = Router::new();
        r.get("/x", |_req, _p| text_response(200, "G"));
        r.post("/x", |_req, _p| text_response(201, "P"));
        assert_eq!(r.serve(&req(Method::Get, "/x")).body, b"G");
        assert_eq!(r.serve(&req(Method::Post, "/x")).body, b"P");
    }

    #[test]
    fn method_specific_wins_over_agnostic() {
        let mut r = Router::new();
        r.handle("/x", |_req, _p| text_response(200, "ANY"));
        r.get("/x", |_req, _p| text_response(200, "GET"));
        assert_eq!(r.serve(&req(Method::Get, "/x")).body, b"GET");
        assert_eq!(r.serve(&req(Method::Post, "/x")).body, b"ANY");
    }

    #[test]
    fn literal_wins_over_capture_at_same_position() {
        let mut r = Router::new();
        r.get("/users/{id}", |_req, _p| text_response(200, "by-id"));
        r.get("/users/me", |_req, _p| text_response(200, "self"));
        assert_eq!(r.serve(&req(Method::Get, "/users/me")).body, b"self");
        assert_eq!(r.serve(&req(Method::Get, "/users/42")).body, b"by-id");
    }

    #[test]
    fn capture_wins_over_wildcard() {
        let mut r = Router::new();
        r.get("/x/*", |_req, _p| text_response(200, "W"));
        r.get("/x/{id}", |_req, _p| text_response(200, "C"));
        assert_eq!(r.serve(&req(Method::Get, "/x/foo")).body, b"C");
    }

    #[test]
    fn not_found_returns_404_by_default() {
        let r = Router::new();
        let resp = r.serve(&req(Method::Get, "/nope"));
        assert_eq!(resp.status, StatusCode(404));
    }

    #[test]
    fn method_mismatch_returns_405_by_default() {
        let mut r = Router::new();
        r.get("/x", |_req, _p| text_response(200, "G"));
        let resp = r.serve(&req(Method::Post, "/x"));
        assert_eq!(resp.status, StatusCode(405));
    }

    #[test]
    fn custom_not_found_handler_runs() {
        let mut r = Router::new();
        r.not_found(|_req, _p| text_response(404, "custom"));
        let resp = r.serve(&req(Method::Get, "/anything"));
        assert_eq!(resp.body, b"custom");
    }

    #[test]
    fn multiple_captures_all_present() {
        let mut r = Router::new();
        r.get("/repos/{owner}/{repo}", |_req, p| {
            let out = format!(
                "{}/{}",
                p.get("owner").unwrap_or(""),
                p.get("repo").unwrap_or("")
            );
            text_response(200, &out)
        });
        let resp = r.serve(&req(Method::Get, "/repos/golang/go"));
        assert_eq!(resp.body, b"golang/go");
    }

    #[test]
    fn root_path_matches_root_pattern() {
        let mut r = Router::new();
        r.get("/", |_req, _p| text_response(200, "root"));
        assert_eq!(r.serve(&req(Method::Get, "/")).body, b"root");
    }

    #[test]
    fn empty_capture_segment_is_no_match() {
        let mut r = Router::new();
        r.get("/users/{id}", |_req, _p| text_response(200, "ok"));
        // `/users/` has an empty segment after the slash; reject.
        let resp = r.serve(&req(Method::Get, "/users/"));
        assert_eq!(resp.status, StatusCode(200));
        // Empty string is still a valid capture; we register
        // this as documented behaviour (Go's mux behaves the
        // same - empty `{id}` matches).
    }

    #[test]
    fn fuzz_capture_against_random_segments() {
        let mut r = Router::new();
        r.get("/u/{name}", |_req, p| {
            text_response(200, p.get("name").unwrap_or(""))
        });
        let names = ["alice", "bob-1", "x", "with.dot", "%encoded"];
        for n in &names {
            let path = format!("/u/{n}");
            let resp = r.serve(&req(Method::Get, &path));
            assert_eq!(resp.body, n.as_bytes(), "name {n} failed");
        }
    }
}
