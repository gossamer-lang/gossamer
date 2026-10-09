//! Runtime support for `std::net::url`.
//! Minimal URL parser covering scheme, host, port, path, query, and
//! fragment. Deliberately narrower than `url` crate: enough for HTTP
//! client code and the package manager, without pulling in IDNA or
//! Unicode normalisation.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::percent;

use crate::errors::Error;

/// Parsed URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Url {
    /// Scheme (`http`, `https`, `file`, ...), lowercase.
    pub scheme: String,
    /// Host component (without port).
    pub host: String,
    /// Optional port.
    pub port: Option<u16>,
    /// Path, always starting with `/` when present.
    pub path: String,
    /// Raw query string, excluding the `?` sentinel.
    pub query: String,
    /// Fragment, excluding the `#` sentinel.
    pub fragment: String,
}

impl Url {
    /// Parses a string into a [`Url`]. Accepts `scheme://host[:port]
    /// /path?query#fragment`.
    pub fn parse(input: &str) -> Result<Self, Error> {
        let mut rest = input;
        let scheme = match rest.find("://") {
            Some(idx) => {
                let scheme = rest[..idx].to_ascii_lowercase();
                rest = &rest[idx + 3..];
                scheme
            }
            None => return Err(Error::new(format!("missing scheme in `{input}`"))),
        };
        let (authority, tail) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };
        let (host, port) = split_host_port(authority)?;
        let mut path = String::new();
        let mut query = String::new();
        let mut fragment = String::new();
        let mut cursor = tail;
        if let Some(idx) = cursor.find('#') {
            fragment = cursor[idx + 1..].to_string();
            cursor = &cursor[..idx];
        }
        if let Some(idx) = cursor.find('?') {
            query = cursor[idx + 1..].to_string();
            cursor = &cursor[..idx];
        }
        path.push_str(cursor);
        Ok(Self {
            scheme,
            host,
            port,
            path,
            query,
            fragment,
        })
    }

    /// Renders the URL back to its canonical string form.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("{}://{}", self.scheme, self.host);
        if let Some(port) = self.port {
            out.push(':');
            out.push_str(&port.to_string());
        }
        out.push_str(&self.path);
        if !self.query.is_empty() {
            out.push('?');
            out.push_str(&self.query);
        }
        if !self.fragment.is_empty() {
            out.push('#');
            out.push_str(&self.fragment);
        }
        out
    }
}

fn split_host_port(authority: &str) -> Result<(String, Option<u16>), Error> {
    match authority.rfind(':') {
        Some(idx) if !authority[idx + 1..].contains(']') => {
            let host = authority[..idx].to_string();
            let port: u16 = authority[idx + 1..]
                .parse()
                .map_err(|_| Error::new(format!("invalid port in `{authority}`")))?;
            Ok((host, Some(port)))
        }
        _ => Ok((authority.to_string(), None)),
    }
}

/// Escapes `text` for use in a URL query parameter.
#[must_use]
pub fn query_escape(text: &str) -> String {
    percent::encode(text, percent::Component::Query)
}

/// Inverts [`query_escape`].
pub fn query_unescape(text: &str) -> Result<String, Error> {
    percent::decode_strict(text, true).map_err(Error::new)
}

/// Encodes `pairs` as `key=value&key=value` query string.
#[must_use]
pub fn encode_query(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&query_escape(k));
        out.push('=');
        out.push_str(&query_escape(v));
    }
    out
}

/// Decodes a query string into a `(key, value)` list, preserving
/// source order.
pub fn decode_query(raw: &str) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    if raw.is_empty() {
        return Ok(out);
    }
    for pair in raw.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.push((query_unescape(k)?, query_unescape(v)?));
    }
    Ok(out)
}

/// Escapes `text` for use as one segment of a URL path. Unlike
/// [`query_escape`], spaces become `%20` (not `+`) and the `/`
/// is preserved per RFC 3986 §3.3.
#[must_use]
pub fn path_escape(text: &str) -> String {
    percent::encode_keeping(
        text,
        |b| {
            percent::is_unreserved(b)
                || matches!(
                    b,
                    b'/' | b':'
                        | b'@'
                        | b'!'
                        | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b','
                        | b';'
                        | b'='
                )
        },
        false,
    )
}

/// Inverts [`path_escape`]. Percent-encoded escapes are decoded;
/// `+` is NOT translated to space (that's a query convention).
pub fn path_unescape(text: &str) -> Result<String, Error> {
    percent::decode_strict(text, false).map_err(Error::new)
}

/// Userinfo pair (`user[:password]`) extracted from a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    /// Username (always present when this struct exists).
    pub username: String,
    /// Password (optional).
    pub password: Option<String>,
}

impl UserInfo {
    /// Parses a `user:pass` userinfo prefix. Both halves are
    /// percent-decoded.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        if let Some((u, p)) = raw.split_once(':') {
            Ok(Self {
                username: path_unescape(u)?,
                password: Some(path_unescape(p)?),
            })
        } else {
            Ok(Self {
                username: path_unescape(raw)?,
                password: None,
            })
        }
    }

    /// Renders to `user[:password]` with percent-encoding.
    #[must_use]
    pub fn render(&self) -> String {
        match &self.password {
            Some(p) => format!("{}:{}", path_escape(&self.username), path_escape(p)),
            None => path_escape(&self.username),
        }
    }
}

/// Multi-valued query map (Go's `url.Values`).
///
/// Preserves insertion order. Use [`Values::add`] to append a
/// new value for a key (allows duplicates); [`Values::set`] to
/// replace any existing values for a key.
#[derive(Debug, Clone, Default)]
pub struct Values {
    pairs: Vec<(String, String)>,
}

impl Values {
    /// Empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `value` under `key`. Existing values for the key
    /// are preserved.
    pub fn add(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.pairs.push((key.into(), value.into()));
    }

    /// Removes every existing value for `key` and sets it to
    /// the single given value.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let k = key.into();
        self.pairs.retain(|(existing, _)| existing != &k);
        self.pairs.push((k, value.into()));
    }

    /// Returns the first value for `key`, or `None`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Returns every value for `key` in insertion order.
    #[must_use]
    pub fn get_all(&self, key: &str) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Removes every value for `key`.
    pub fn delete(&mut self, key: &str) {
        self.pairs.retain(|(k, _)| k != key);
    }

    /// Returns `true` if `key` has at least one value.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.pairs.iter().any(|(k, _)| k == key)
    }

    /// Returns the number of pairs (with duplicates counted).
    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// `true` when there are no pairs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Iterates every `(key, value)` pair in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Renders the values as a sorted-by-key query string. Keys
    /// with multiple values are emitted in insertion order.
    /// Matches Go's `Values.Encode`.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut sorted: Vec<(&str, &str)> = self
            .pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = String::new();
        for (i, (k, v)) in sorted.iter().enumerate() {
            if i > 0 {
                out.push('&');
            }
            out.push_str(&query_escape(k));
            out.push('=');
            out.push_str(&query_escape(v));
        }
        out
    }

    /// Parses a query string (no leading `?`) into a `Values`.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let pairs = decode_query(raw)?;
        Ok(Self { pairs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_url() {
        let u = Url::parse("https://example.com:443/a/b?k=v#frag").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, Some(443));
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query, "k=v");
        assert_eq!(u.fragment, "frag");
    }

    #[test]
    fn parse_without_port_and_path() {
        let u = Url::parse("http://example.com").unwrap();
        assert_eq!(u.host, "example.com");
        assert!(u.port.is_none());
        assert_eq!(u.path, "");
    }

    #[test]
    fn parse_rejects_missing_scheme() {
        assert!(Url::parse("example.com").is_err());
    }

    #[test]
    fn render_round_trips_url() {
        let input = "http://example.com:8080/path?a=1&b=2#x";
        let u = Url::parse(input).unwrap();
        assert_eq!(u.render(), input);
    }

    #[test]
    fn query_escape_and_unescape_round_trip() {
        let raw = "hello world/!*'";
        let escaped = query_escape(raw);
        assert_eq!(escaped, "hello+world%2F%21%2A%27");
        assert_eq!(query_unescape(&escaped).unwrap(), raw);
    }

    #[test]
    fn path_escape_preserves_slash() {
        let s = path_escape("a/b c");
        assert_eq!(s, "a/b%20c");
    }

    #[test]
    fn path_unescape_does_not_decode_plus() {
        let decoded = path_unescape("a%20b+c").unwrap();
        assert_eq!(decoded, "a b+c");
    }

    #[test]
    fn user_info_parses_username_only() {
        let u = UserInfo::parse("alice").unwrap();
        assert_eq!(u.username, "alice");
        assert!(u.password.is_none());
    }

    #[test]
    fn user_info_parses_with_password() {
        let u = UserInfo::parse("alice:wonder%20land").unwrap();
        assert_eq!(u.username, "alice");
        assert_eq!(u.password, Some("wonder land".into()));
    }

    #[test]
    fn user_info_round_trips_through_render() {
        let u = UserInfo {
            username: "user".into(),
            password: Some("p:w".into()),
        };
        let s = u.render();
        let back = UserInfo::parse(&s).unwrap();
        assert_eq!(back, u);
    }

    #[test]
    fn values_basic_add_get_round_trip() {
        let mut v = Values::new();
        v.add("a", "1");
        v.add("a", "2");
        v.add("b", "3");
        assert_eq!(v.get("a"), Some("1"));
        assert_eq!(v.get_all("a"), vec!["1", "2"]);
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn values_set_replaces_all_existing() {
        let mut v = Values::new();
        v.add("a", "1");
        v.add("a", "2");
        v.set("a", "only");
        assert_eq!(v.get_all("a"), vec!["only"]);
    }

    #[test]
    fn values_delete_removes_every_match() {
        let mut v = Values::new();
        v.add("a", "1");
        v.add("a", "2");
        v.add("b", "3");
        v.delete("a");
        assert!(!v.has("a"));
        assert!(v.has("b"));
    }

    #[test]
    fn values_encode_sorts_by_key() {
        let mut v = Values::new();
        v.add("z", "1");
        v.add("a", "2");
        v.add("m", "3");
        assert_eq!(v.encode(), "a=2&m=3&z=1");
    }

    #[test]
    fn values_parse_round_trip() {
        let mut v = Values::new();
        v.add("name", "jane doe");
        v.add("city", "NYC");
        let s = v.encode();
        let parsed = Values::parse(&s).unwrap();
        // Encoding is sorted; parsed pairs preserve that order.
        let pairs: Vec<(&str, &str)> = parsed.iter().collect();
        assert_eq!(pairs.len(), 2);
        assert!(parsed.has("name"));
        assert_eq!(parsed.get("name"), Some("jane doe"));
    }

    #[test]
    fn encode_and_decode_query_pairs() {
        let encoded = encode_query(&[("name", "jane doe"), ("age", "30")]);
        assert_eq!(encoded, "name=jane+doe&age=30");
        let decoded = decode_query(&encoded).unwrap();
        assert_eq!(
            decoded,
            vec![
                ("name".to_string(), "jane doe".to_string()),
                ("age".to_string(), "30".to_string()),
            ]
        );
    }
}
