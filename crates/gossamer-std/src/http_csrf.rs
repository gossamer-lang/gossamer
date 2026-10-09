//! CSRF (Cross-Site Request Forgery) protection for `std::http::csrf`.
//!
//! The threat: a malicious origin uses an authenticated user's
//! ambient cookies to issue state-changing requests against a
//! cookie-session site the user is logged into. This module defends
//! with the *signed double-submit cookie* pattern: at session start,
//! the server issues a random token, HMAC-signs it with a server-side
//! key, and stores `random.signature` in a non-`HttpOnly` cookie.
//! The single-page-app reads that cookie via JS and echoes the value
//! in an `X-CSRF-Token` request header (or a `_csrf` form field for
//! traditional `<form>` posts). On every unsafe-method request the
//! server verifies (1) the `Origin` / `Referer` matches a trusted
//! list and (2) the header/form token byte-equals the cookie token
//! and the HMAC verifies under the server key. A cross-origin
//! attacker cannot read the victim's cookie, so cannot reproduce
//! the header - even though their forged request still ships the
//! cookie automatically.

#![forbid(unsafe_code)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value
)]

use crate::crypto::hmac::sha256_mac;
use crate::crypto::rand;
use crate::crypto::subtle::constant_time_eq;
use crate::errors::Error;
use crate::http::{Request, Response};
use crate::http_cookie::{Cookie, SameSite, parse_cookie_header};

const MIN_CSRF_KEY_BYTES: usize = 32;
const MAX_CSRF_TOKEN_BYTES: usize = 512;

/// Route-marker enum a handler attaches to declare its auth model.
///
/// CSRF middleware reads this to decide whether to enforce a token
/// check - bearer-only routes are exempt because the attacker would
/// need to steal the bearer token outright, which the browser does
/// not auto-attach the way it does cookies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteAuth {
    /// Bearer-token authenticated; CSRF check is skipped entirely.
    BearerOnly,
    /// Session-cookie authenticated; CSRF is enforced on unsafe methods.
    CookieSession,
    /// Public route; CSRF is exempt for safe methods and enforced
    /// for unsafe methods that arrive with cookies.
    None,
}

/// CSRF middleware configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Name of the cookie that carries the CSRF token (default `gos_csrf`).
    pub cookie_name: String,
    /// Request header the SPA echoes the token in (default `X-CSRF-Token`).
    pub header_name: String,
    /// Form field name for the token in `application/x-www-form-urlencoded`
    /// bodies (default `_csrf`).
    pub form_field: String,
    /// HMAC-SHA-256 signing key. Generated random when constructed via
    /// [`Config::random`]; callers may also pass an existing key bound
    /// to a long-lived secret (e.g. derived from `argon2`).
    pub key: Vec<u8>,
    /// Allow-list of trusted request origins. Compared verbatim against
    /// the `Origin` header (or the scheme+host of `Referer`). Empty
    /// means "fall back to same-Host check".
    pub trusted_origins: Vec<String>,
    /// `SameSite` attribute on the issued cookie (default `Lax`).
    pub same_site: SameSite,
    /// `Secure` attribute on the issued cookie (default `true`).
    pub secure: bool,
    /// HTTP methods that are exempt from CSRF (default `GET`, `HEAD`,
    /// `OPTIONS`, `TRACE`).
    pub safe_methods: Vec<String>,
    /// URL-path prefixes that skip CSRF outright (e.g. `/api/webhook/`).
    pub exempt_prefixes: Vec<String>,
    /// Cookie `Max-Age` in seconds (default 86400 = 24h).
    pub max_age_secs: i64,
}

impl Config {
    /// Constructs a config from an existing key.
    ///
    /// Panics when the key is too short. Prefer [`Self::try_new`] when
    /// configuration errors should be returned to the caller.
    pub fn new(key: Vec<u8>) -> Self {
        match Self::try_new(key) {
            Ok(config) => config,
            Err(err) => panic!("csrf config: {}", err.message()),
        }
    }

    /// Constructs a config from an existing cryptographic key.
    pub fn try_new(key: Vec<u8>) -> Result<Self, Error> {
        validate_key(&key)?;
        Ok(Self {
            cookie_name: "gos_csrf".to_string(),
            header_name: "X-CSRF-Token".to_string(),
            form_field: "_csrf".to_string(),
            key,
            trusted_origins: Vec::new(),
            same_site: SameSite::Lax,
            secure: true,
            safe_methods: vec![
                "GET".to_string(),
                "HEAD".to_string(),
                "OPTIONS".to_string(),
                "TRACE".to_string(),
            ],
            exempt_prefixes: Vec::new(),
            max_age_secs: 86_400,
        })
    }

    /// Constructs a config with a freshly generated 32-byte key.
    pub fn random() -> Result<Self, Error> {
        let key = rand::bytes(32)?;
        Self::try_new(key)
    }

    /// Appends an entry to [`Config::trusted_origins`].
    #[must_use]
    pub fn trust_origin(mut self, origin: impl Into<String>) -> Self {
        self.trusted_origins.push(origin.into());
        self
    }

    /// Appends an entry to [`Config::exempt_prefixes`].
    #[must_use]
    pub fn exempt_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.exempt_prefixes.push(prefix.into());
        self
    }

    /// Overrides the safe-method list.
    #[must_use]
    pub fn safe_methods(mut self, methods: Vec<String>) -> Self {
        self.safe_methods = methods;
        self
    }

    /// Overrides the cookie `SameSite` attribute.
    #[must_use]
    pub fn same_site(mut self, ss: SameSite) -> Self {
        self.same_site = ss;
        self
    }

    /// Overrides the cookie `Secure` attribute.
    #[must_use]
    pub fn secure(mut self, on: bool) -> Self {
        self.secure = on;
        self
    }
}

/// Issues a fresh CSRF token signed with `key`.
///
/// Wire shape: `base64url(random32) + "." + base64url(hmac_sha256(key, random32))`.
/// Call once per session and stash in the user-visible (non-HttpOnly)
/// cookie; reissue when rotating.
pub fn issue_token(key: &[u8]) -> Result<String, Error> {
    validate_key(key)?;
    let nonce = rand::bytes(32)?;
    let mac = sha256_mac(key, &nonce);
    Ok(format!("{}.{}", b64url_encode(&nonce), b64url_encode(&mac)))
}

/// Issues a CSRF token bound to one authenticated session identifier.
///
/// The session identifier is never sent in the CSRF cookie. It derives a
/// per-session HMAC key, so replaying a token from one signed-in session in
/// another fails even when both users share the same server-wide CSRF key.
pub fn issue_token_for_session(key: &[u8], session_id: &str) -> Result<String, Error> {
    let session_key = derive_session_key(key, session_id)?;
    issue_token(&session_key)
}

/// Verifies a token round-trip.
///
/// Returns `Ok(())` when both presentations decode, share the same
/// random portion, and the HMAC over that random portion verifies
/// under `key`. Returns `Err` on any tamper. Constant-time on the
/// shared-portion compare.
pub fn verify_token(
    token_from_cookie: &str,
    token_from_header_or_form: &str,
    key: &[u8],
) -> Result<(), Error> {
    validate_key(key)?;
    let (cookie_nonce_b64, cookie_sig_b64) = split_token(token_from_cookie)?;
    let (header_nonce_b64, _) = split_token(token_from_header_or_form)?;

    // The nonce portion of cookie vs supplied must match exactly.
    // Constant-time on the base64 form is fine - the byte length is
    // identical for any valid 32-byte nonce.
    if !constant_time_eq(cookie_nonce_b64.as_bytes(), header_nonce_b64.as_bytes()) {
        return Err(Error::new("csrf: token mismatch"));
    }

    let nonce =
        b64url_decode(cookie_nonce_b64).map_err(|e| Error::wrap(e, "csrf: cookie nonce decode"))?;
    let sig = b64url_decode(cookie_sig_b64)
        .map_err(|e| Error::wrap(e, "csrf: cookie signature decode"))?;
    let expected = sha256_mac(key, &nonce);
    if !constant_time_eq(&sig, &expected) {
        return Err(Error::new("csrf: signature mismatch"));
    }
    Ok(())
}

/// Verifies a CSRF token bound to one authenticated session identifier.
pub fn verify_token_for_session(
    token_from_cookie: &str,
    token_from_header_or_form: &str,
    key: &[u8],
    session_id: &str,
) -> Result<(), Error> {
    let session_key = derive_session_key(key, session_id)?;
    verify_token(token_from_cookie, token_from_header_or_form, &session_key)
}

/// Pulls the CSRF token from a request: header first, then the form
/// field when the body is `application/x-www-form-urlencoded`.
pub fn extract_token(request: &Request, config: &Config) -> Option<String> {
    if let Some(v) = request.headers.get(&config.header_name) {
        let t = v.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    let content_type = request.headers.get("content-type").unwrap_or("");
    if content_type
        .to_ascii_lowercase()
        .starts_with("application/x-www-form-urlencoded")
    {
        let body = std::str::from_utf8(&request.body).ok()?;
        for pair in body.split('&') {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if name == config.form_field {
                let decoded = url_decode_form_value(value);
                if !decoded.is_empty() {
                    return Some(decoded);
                }
            }
        }
    }
    None
}

/// Checks the `Origin` or `Referer` of `request` against
/// `config.trusted_origins`.
///
/// When the trusted list is empty the check falls back to "same
/// `Host`" - i.e. the request's `Origin` scheme+host must equal its
/// own `Host` header. Missing both `Origin` and `Referer` is treated
/// conservatively: rejected for unsafe methods, allowed for safe
/// ones. Same-`Host` fallback exists because a fresh deployment with
/// no explicit allow-list still wants a sensible default.
pub fn origin_allowed(request: &Request, config: &Config) -> bool {
    let method = request.method.as_str();
    let is_safe = config
        .safe_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method));

    let origin = request.headers.get("origin").map(str::trim);
    let referer = request.headers.get("referer").map(str::trim);

    let candidate = match (origin, referer) {
        (Some(o), _) if !o.is_empty() => o.to_string(),
        (_, Some(r)) if !r.is_empty() => match origin_from_referer(r) {
            Some(o) => o,
            None => return is_safe,
        },
        _ => return is_safe,
    };

    if !config.trusted_origins.is_empty() {
        return config
            .trusted_origins
            .iter()
            .any(|t| origins_equal(t, &candidate));
    }

    // Fallback: insist Origin scheme+host matches Host header.
    let host = request.headers.get("host").unwrap_or("").trim();
    if host.is_empty() {
        return false;
    }
    same_host(&candidate, host)
}

/// The main guard: returns `Ok(())` if the request passes CSRF,
/// otherwise `Err` with a description of which check failed.
pub fn check(request: &Request, route_auth: RouteAuth, config: &Config) -> Result<(), Error> {
    check_with_session(request, route_auth, config, None)
}

/// CSRF guard with optional binding to an authenticated session identifier.
///
/// Cookie-session routes must provide their stable authenticated session ID.
/// Public and bearer-only routes retain the same exemption behavior as
/// [`check`].
pub fn check_with_session(
    request: &Request,
    route_auth: RouteAuth,
    config: &Config,
    session_id: Option<&str>,
) -> Result<(), Error> {
    if route_auth == RouteAuth::BearerOnly {
        return Ok(());
    }

    let method = request.method.as_str();
    if config
        .safe_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method))
    {
        return Ok(());
    }

    let path = request.path();
    if config
        .exempt_prefixes
        .iter()
        .any(|p| path.starts_with(p.as_str()))
    {
        return Ok(());
    }

    if !origin_allowed(request, config) {
        return Err(Error::new("csrf: origin not allowed"));
    }

    let cookie_header = request
        .headers
        .get("cookie")
        .ok_or_else(|| Error::new("csrf: missing cookie header"))?;
    let pairs = parse_cookie_header(cookie_header);
    let cookie_token = pairs
        .iter()
        .find(|(k, _)| k == &config.cookie_name)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| Error::new("csrf: missing csrf cookie"))?;

    let supplied =
        extract_token(request, config).ok_or_else(|| Error::new("csrf: missing csrf token"))?;

    if route_auth == RouteAuth::CookieSession {
        let session_id = session_id.ok_or_else(|| Error::new("csrf: session binding required"))?;
        verify_token_for_session(&cookie_token, &supplied, &config.key, session_id)
    } else {
        verify_token(&cookie_token, &supplied, &config.key)
    }
}

/// Attaches a freshly issued CSRF cookie to `response`.
///
/// The cookie is intentionally **not** `HttpOnly` because the SPA
/// needs to read it from JS to echo into the `X-CSRF-Token` header.
/// This is the defining tradeoff of the double-submit pattern:
/// the cookie's value is not itself a secret - it is the round-trip
/// equality of cookie and header (under the server's HMAC) that
/// proves the request came from a page that ran on the same origin.
pub fn attach_cookie(response: &mut Response, token: &str, config: &Config) {
    let cookie = Cookie::builder(config.cookie_name.clone(), token.to_string())
        .path("/")
        .max_age(config.max_age_secs)
        .http_only(false)
        .secure(config.secure)
        .same_site(config.same_site)
        .build();
    response
        .headers
        .insert("set-cookie", &cookie.to_header_value());
}

// -- helpers --------------------------------------------------------------

fn split_token(token: &str) -> Result<(&str, &str), Error> {
    if token.len() > MAX_CSRF_TOKEN_BYTES {
        return Err(Error::new("csrf: token exceeds size limit"));
    }
    let (a, b) = token
        .split_once('.')
        .ok_or_else(|| Error::new("csrf: token missing separator"))?;
    if a.is_empty() || b.is_empty() {
        return Err(Error::new("csrf: token has empty component"));
    }
    Ok((a, b))
}

fn derive_session_key(key: &[u8], session_id: &str) -> Result<[u8; 32], Error> {
    validate_key(key)?;
    if session_id.is_empty() || session_id.len() > MAX_CSRF_TOKEN_BYTES {
        return Err(Error::new("csrf: invalid session binding"));
    }
    Ok(sha256_mac(key, session_id.as_bytes()))
}

fn validate_key(key: &[u8]) -> Result<(), Error> {
    if key.len() < MIN_CSRF_KEY_BYTES {
        return Err(Error::new(format!(
            "csrf: key must be at least {MIN_CSRF_KEY_BYTES} bytes"
        )));
    }
    Ok(())
}

fn origin_from_referer(referer: &str) -> Option<String> {
    // Strip scheme://host[:port] from a Referer URL.
    let scheme_end = referer.find("://")?;
    let rest = &referer[scheme_end + 3..];
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(format!(
        "{}://{}",
        &referer[..scheme_end],
        &rest[..host_end]
    ))
}

fn origins_equal(a: &str, b: &str) -> bool {
    a.trim_end_matches('/')
        .eq_ignore_ascii_case(b.trim_end_matches('/'))
}

fn same_host(origin: &str, host: &str) -> bool {
    // Origin is scheme://host[:port]; compare the host[:port] tail.
    let Some(scheme_end) = origin.find("://") else {
        return false;
    };
    let host_part = &origin[scheme_end + 3..];
    let host_part = host_part.split(['/', '?', '#']).next().unwrap_or(host_part);
    host_part.eq_ignore_ascii_case(host)
}

fn url_decode_form_value(s: &str) -> String {
    gossamer_runtime::codec::percent::decode(s, gossamer_runtime::codec::percent::Component::Query)
}

// -- base64url (RFC 4648 §5, no padding) ---------------------------------

const B64URL_ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n =
            (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8) | u32::from(input[i + 2]);
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let remaining = input.len() - i;
    if remaining == 1 {
        let n = u32::from(input[i]) << 16;
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
    } else if remaining == 2 {
        let n = (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8);
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 6) & 0x3f) as usize] as char);
    }
    out
}

fn b64url_decode(input: &str) -> Result<Vec<u8>, Error> {
    let trimmed = input.trim_end_matches('=');
    let bytes = trimmed.as_bytes();
    let n = bytes.len();
    let out_len = match n % 4 {
        0 => n / 4 * 3,
        2 => n / 4 * 3 + 1,
        3 => n / 4 * 3 + 2,
        _ => return Err(Error::new("csrf: base64url: invalid length")),
    };
    let mut out = Vec::with_capacity(out_len);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        let v = decode_b64url_char(b)?;
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if bits > 0 && (acc & ((1u32 << bits) - 1)) != 0 {
        return Err(Error::new("csrf: base64url: non-zero padding bits"));
    }
    Ok(out)
}

fn decode_b64url_char(c: u8) -> Result<u8, Error> {
    match c {
        b'A'..=b'Z' => Ok(c - b'A'),
        b'a'..=b'z' => Ok(c - b'a' + 26),
        b'0'..=b'9' => Ok(c - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err(Error::new(format!(
            "csrf: base64url: invalid character 0x{c:02x}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use crate::http::{Headers, Method, Request, Response, StatusCode};

    fn key() -> Vec<u8> {
        b"super-secret-32-byte-test-key!!!".to_vec()
    }

    fn make_request(method: Method, path: &str) -> Request {
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

    #[test]
    fn issue_then_verify_round_trip() {
        let k = key();
        let t = issue_token(&k).expect("issue");
        verify_token(&t, &t, &k).expect("round-trip verifies");
    }

    #[test]
    fn session_bound_tokens_do_not_cross_authenticated_sessions() {
        let k = key();
        let token = issue_token_for_session(&k, "session-a").unwrap();
        verify_token_for_session(&token, &token, &k, "session-a").unwrap();
        assert!(verify_token_for_session(&token, &token, &k, "session-b").is_err());
    }

    #[test]
    fn tampered_cookie_portion_fails_verify() {
        let k = key();
        let t = issue_token(&k).expect("issue");
        // Flip a character inside the nonce portion (before the dot).
        let mut bytes = t.clone().into_bytes();
        bytes[0] = if bytes[0] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).unwrap();
        // Tampered nonce differs from original supplied -> mismatch.
        let err = verify_token(&tampered, &t, &k).unwrap_err();
        assert!(err.message().contains("mismatch"));
        // And self-pairing the tampered token still fails because
        // the signature no longer covers the new nonce.
        assert!(verify_token(&tampered, &tampered, &k).is_err());
    }

    #[test]
    fn weak_keys_and_oversized_tokens_are_rejected() {
        assert!(Config::try_new(vec![0; 31]).is_err());
        assert!(issue_token(&[0; 31]).is_err());
        let oversized = "a".repeat(MAX_CSRF_TOKEN_BYTES + 1);
        assert!(verify_token(&oversized, &oversized, &key()).is_err());
    }

    #[test]
    fn mismatched_supplied_vs_cookie_fails() {
        let k = key();
        let a = issue_token(&k).expect("a");
        let b = issue_token(&k).expect("b");
        let err = verify_token(&a, &b, &k).unwrap_err();
        assert!(err.message().contains("mismatch"));
    }

    #[test]
    fn wrong_key_fails_verify() {
        let k1 = key();
        let k2 = b"different-32-byte-secret-key-!!!".to_vec();
        let t = issue_token(&k1).expect("issue");
        let err = verify_token(&t, &t, &k2).unwrap_err();
        assert!(err.message().contains("signature"));
    }

    #[test]
    fn check_bearer_only_always_passes() {
        let cfg = Config::new(key());
        let req = make_request(Method::Post, "/api/anything");
        check(&req, RouteAuth::BearerOnly, &cfg).expect("bearer-only bypasses");
    }

    #[test]
    fn check_get_always_passes() {
        let cfg = Config::new(key());
        let req = make_request(Method::Get, "/page");
        check(&req, RouteAuth::CookieSession, &cfg).expect("safe method passes");
    }

    #[test]
    fn check_post_with_valid_token_and_origin_passes() {
        let cfg = Config::new(key()).trust_origin("https://app.example.com");
        let session_id = "authenticated-session";
        let token = issue_token_for_session(&cfg.key, session_id).expect("issue");

        let mut req = make_request(Method::Post, "/api/do");
        req.headers.insert("origin", "https://app.example.com");
        req.headers
            .insert("cookie", &format!("{}={}", cfg.cookie_name, token));
        req.headers.insert(&cfg.header_name, &token);

        check_with_session(&req, RouteAuth::CookieSession, &cfg, Some(session_id))
            .expect("valid POST passes");
        let err = check(&req, RouteAuth::CookieSession, &cfg).unwrap_err();
        assert!(err.message().contains("session binding"));
    }

    #[test]
    fn check_post_missing_origin_fails() {
        let cfg = Config::new(key()).trust_origin("https://app.example.com");
        let token = issue_token(&cfg.key).expect("issue");

        let mut req = make_request(Method::Post, "/api/do");
        req.headers
            .insert("cookie", &format!("{}={}", cfg.cookie_name, token));
        req.headers.insert(&cfg.header_name, &token);
        // No Origin, no Referer.

        let err = check(&req, RouteAuth::CookieSession, &cfg).unwrap_err();
        assert!(err.message().contains("origin"), "got: {}", err.message());
    }

    #[test]
    fn check_post_untrusted_origin_fails() {
        let cfg = Config::new(key()).trust_origin("https://app.example.com");
        let token = issue_token(&cfg.key).expect("issue");

        let mut req = make_request(Method::Post, "/api/do");
        req.headers.insert("origin", "https://evil.example.org");
        req.headers
            .insert("cookie", &format!("{}={}", cfg.cookie_name, token));
        req.headers.insert(&cfg.header_name, &token);

        let err = check(&req, RouteAuth::CookieSession, &cfg).unwrap_err();
        assert!(err.message().contains("origin"));
    }

    #[test]
    fn check_post_mismatched_header_vs_cookie_fails() {
        let cfg = Config::new(key()).trust_origin("https://app.example.com");
        let session_id = "authenticated-session";
        let token_a = issue_token_for_session(&cfg.key, session_id).expect("a");
        let token_b = issue_token_for_session(&cfg.key, session_id).expect("b");

        let mut req = make_request(Method::Post, "/api/do");
        req.headers.insert("origin", "https://app.example.com");
        req.headers
            .insert("cookie", &format!("{}={}", cfg.cookie_name, token_a));
        req.headers.insert(&cfg.header_name, &token_b);

        let err =
            check_with_session(&req, RouteAuth::CookieSession, &cfg, Some(session_id)).unwrap_err();
        assert!(err.message().contains("mismatch"), "got: {}", err.message());
    }

    #[test]
    fn check_post_no_cookie_fails_with_message() {
        let cfg = Config::new(key()).trust_origin("https://app.example.com");
        let token = issue_token(&cfg.key).expect("issue");

        let mut req = make_request(Method::Post, "/api/do");
        req.headers.insert("origin", "https://app.example.com");
        req.headers.insert(&cfg.header_name, &token);

        let err = check(&req, RouteAuth::CookieSession, &cfg).unwrap_err();
        assert!(err.message().contains("cookie"), "got: {}", err.message());
    }

    #[test]
    fn check_exempt_prefix_bypasses() {
        let cfg = Config::new(key())
            .trust_origin("https://app.example.com")
            .exempt_prefix("/webhooks/");
        let req = make_request(Method::Post, "/webhooks/stripe");
        check(&req, RouteAuth::CookieSession, &cfg).expect("exempt prefix bypasses");
    }

    #[test]
    fn extract_token_from_header() {
        let cfg = Config::new(key());
        let mut req = make_request(Method::Post, "/x");
        req.headers.insert(&cfg.header_name, "deadbeef.cafef00d");
        assert_eq!(
            extract_token(&req, &cfg).as_deref(),
            Some("deadbeef.cafef00d")
        );
    }

    #[test]
    fn extract_token_from_form_body() {
        let cfg = Config::new(key());
        let mut req = make_request(Method::Post, "/x");
        req.headers
            .insert("content-type", "application/x-www-form-urlencoded");
        req.body = b"name=alice&_csrf=tok.sig&other=v".to_vec();
        assert_eq!(extract_token(&req, &cfg).as_deref(), Some("tok.sig"));
    }

    #[test]
    fn extract_token_form_decodes_percent_encoding() {
        let cfg = Config::new(key());
        let mut req = make_request(Method::Post, "/x");
        req.headers
            .insert("content-type", "application/x-www-form-urlencoded");
        // "abc.de%2Bf" -> "abc.de+f"
        req.body = b"_csrf=abc.de%2Bf".to_vec();
        assert_eq!(extract_token(&req, &cfg).as_deref(), Some("abc.de+f"));
    }

    #[test]
    fn origin_allowed_referer_fallback_when_origin_absent() {
        let cfg = Config::new(key()).trust_origin("https://app.example.com");
        let mut req = make_request(Method::Post, "/x");
        req.headers
            .insert("referer", "https://app.example.com/some/page?x=1");
        assert!(origin_allowed(&req, &cfg));
    }

    #[test]
    fn origin_allowed_safe_method_allows_missing_origin() {
        let cfg = Config::new(key()).trust_origin("https://app.example.com");
        let req = make_request(Method::Get, "/x");
        assert!(origin_allowed(&req, &cfg));
    }

    #[test]
    fn origin_allowed_same_host_fallback_when_trusted_empty() {
        let cfg = Config::new(key());
        let mut req = make_request(Method::Post, "/x");
        req.headers.insert("host", "self.example.com");
        req.headers.insert("origin", "https://self.example.com");
        assert!(origin_allowed(&req, &cfg));

        let mut req2 = make_request(Method::Post, "/x");
        req2.headers.insert("host", "self.example.com");
        req2.headers.insert("origin", "https://other.example.com");
        assert!(!origin_allowed(&req2, &cfg));
    }

    #[test]
    fn attach_cookie_sets_non_http_only_secure_lax_by_default() {
        let cfg = Config::new(key());
        let mut resp = Response::text(StatusCode::OK, "hi");
        let token = issue_token(&cfg.key).expect("issue");
        attach_cookie(&mut resp, &token, &cfg);
        let header = resp.headers.get("set-cookie").expect("set-cookie set");
        assert!(header.contains("gos_csrf="));
        assert!(!header.contains("HttpOnly"), "must not be HttpOnly");
        assert!(header.contains("Secure"));
        assert!(header.contains("SameSite=Lax"));
    }

    #[test]
    fn config_random_generates_32_byte_key() {
        let cfg = Config::random().expect("random key");
        assert_eq!(cfg.key.len(), 32);
    }

    #[test]
    fn check_post_none_auth_with_cookie_still_enforced() {
        // RouteAuth::None still enforces when an unsafe method arrives -
        // the spec is "exempt for safe, enforced for unsafe with cookies".
        let cfg = Config::new(key()).trust_origin("https://app.example.com");
        let token = issue_token(&cfg.key).expect("issue");
        let mut req = make_request(Method::Post, "/public");
        req.headers.insert("origin", "https://app.example.com");
        req.headers
            .insert("cookie", &format!("{}={}", cfg.cookie_name, token));
        req.headers.insert(&cfg.header_name, &token);
        check(&req, RouteAuth::None, &cfg).expect("valid token passes None auth");
    }
}
