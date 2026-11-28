//! HTML templates with context-aware auto-escape.
//!
//! Same syntax as [`crate::text`], with one critical
//! difference: every `{{ .x }}` substitution is HTML-escaped by
//! default. Authors who want raw HTML insert opt out per-substitution
//! with the `safe` keyword: `{{ safe .body }}`. URL attributes (`href`,
//! `src`, `action`, `formaction`, `cite`) get URL-escaped values; JS
//! contexts (inside `<script>`) get JSON-encoded values.
//!
//! The escape mode is inferred from where the substitution lands in
//! the source text: text body, attribute body, URL attribute, or JS
//! body. The classifier is heuristic - sufficient for typical web-form
//! responses but not a substitute for a content-security policy.

#![forbid(unsafe_code)]

pub use crate::text::{Error, MissingKey, Value};

/// Compiled HTML template: the text engine's tree, rendered with every
/// substitution escaped by the context it lands in.
#[derive(Debug, Clone)]
pub struct Template {
    parsed: crate::text::Template,
}

impl Template {
    /// Parses `source`.
    pub fn from_source(source: &str) -> Result<Self, Error> {
        Ok(Self {
            parsed: crate::text::parse(source)?,
        })
    }

    /// Renders the template against `data`, escaping every dynamic
    /// substitution by context.
    pub fn render(&self, data: &Value) -> Result<String, Error> {
        self.render_with_missing(data, MissingKey::Empty)
    }

    /// [`Self::render`], with what a field the data does not hold renders as.
    pub fn render_with_missing(&self, data: &Value, missing: MissingKey) -> Result<String, Error> {
        render_parsed(&self.parsed, data, missing)
    }
}

/// Parses + renders in one shot.
pub fn render(source: &str, data: &Value) -> Result<String, Error> {
    Template::from_source(source)?.render(data)
}

/// Parses `source` into a [`Template`].
pub fn parse(source: &str) -> Result<Template, Error> {
    Template::from_source(source)
}

/// Renders `source` against a JSON-encoded data context, escaping
/// every dynamic substitution by context. The JSON document is
/// projected onto the template [`Value`] tree so the call marshals
/// across every tier with two plain string arguments - the shape the
/// Gossamer `html::template::render(source, data)` surface lowers to.
///
/// The context classifier is heuristic (text / attribute / URL / JS):
/// sound for typical server-rendered responses, but NOT a
/// content-security-policy substitute. Treat untrusted HTML fragments
/// with a real sanitizer.
pub fn render_json(source: &str, json_data: &str) -> Result<String, Error> {
    let data = crate::text::json_to_value(json_data)?;
    render(source, &data)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Body,
    Attr,
    AttrUnquoted,
    Url,
    Js,
}

fn detect_context(prefix: &str) -> Context {
    // Walk backward from end-of-prefix to figure out where we are.
    let bytes = prefix.as_bytes();
    let mut in_tag = false;
    let mut in_script = 0i32;
    let mut last_attr_name = String::new();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    let mut cursor_attr = String::new();
    // True when the cursor sits in an unquoted attribute value (after
    // `=` with no opening quote, until whitespace / `>` ends it).
    let mut after_equals = false;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'<' {
            in_tag = true;
            cursor_attr.clear();
            after_equals = false;
            // Detect <script>.
            if bytes[i..].starts_with(b"<script") {
                in_script += 1;
            } else if bytes[i..].starts_with(b"</script") {
                in_script = (in_script - 1).max(0);
            }
            i += 1;
            continue;
        }
        if b == b'>' {
            in_tag = false;
            cursor_attr.clear();
            after_equals = false;
            i += 1;
            continue;
        }
        if in_tag {
            if b == b'"' || b == b'\'' {
                quote = Some(b);
                after_equals = false;
                if !cursor_attr.is_empty() {
                    last_attr_name = std::mem::take(&mut cursor_attr);
                }
                i += 1;
                continue;
            }
            if b == b'=' {
                if !cursor_attr.is_empty() {
                    last_attr_name = std::mem::take(&mut cursor_attr);
                }
                after_equals = true;
                i += 1;
                continue;
            }
            if b.is_ascii_whitespace() {
                cursor_attr.clear();
                after_equals = false;
                i += 1;
                continue;
            }
            cursor_attr.push(b as char);
        }
        i += 1;
    }
    if in_script > 0 {
        return Context::Js;
    }
    // Inside an attribute value, whether quoted (`quote.is_some()`) or
    // unquoted (`after_equals` with no opening quote).
    if quote.is_some() || (in_tag && after_equals) {
        let lower = last_attr_name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "href" | "src" | "action" | "formaction" | "cite" | "background"
        ) {
            // URL escaping percent-encodes spaces and filters dangerous
            // schemes, so it is safe in both quoted and unquoted slots.
            return Context::Url;
        }
        if quote.is_some() {
            return Context::Attr;
        }
        return Context::AttrUnquoted;
    }
    Context::Body
}

fn escape_html_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes a value substituted into an unquoted attribute (`<a
/// href=VALUE>`). Beyond the HTML specials, the bytes that *terminate*
/// an unquoted value - whitespace, `` ` ``, `=` - are numeric-escaped
/// so the value cannot end early and inject a new attribute (e.g.
/// `onmouseover=...`).
fn escape_attr_unquoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '`' => out.push_str("&#96;"),
            '=' => out.push_str("&#61;"),
            ' ' => out.push_str("&#32;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
    out
}

/// Schemes allowed in a URL-attribute value. Anything else (notably
/// `javascript:` and `data:`) is replaced with an inert placeholder.
const SAFE_URL_SCHEMES: [&str; 5] = ["http", "https", "mailto", "tel", "ftp"];

/// Extracts the URL scheme (the run before the first `:`) when the
/// value actually has one - i.e. the `:` precedes any `/`, `?`, or `#`
/// and the scheme is a valid `ALPHA *( ALPHA / DIGIT / "+" / "-" /
/// "." )`. Returns `None` for relative URLs and fragments.
fn url_scheme(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first().is_none_or(|b| !b.is_ascii_alphabetic()) {
        return None;
    }
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b':' => return Some(&s[..i]),
            b'/' | b'?' | b'#' => return None,
            c if c.is_ascii_alphanumeric() || c == b'+' || c == b'-' || c == b'.' => {}
            _ => return None,
        }
    }
    None
}

/// True when `s` carries a scheme outside [`SAFE_URL_SCHEMES`]. Tabs and
/// newlines are stripped and leading controls/spaces skipped first,
/// because browsers do the same before parsing the scheme - otherwise
/// `java\tscript:` would slip through.
fn has_unsafe_scheme(s: &str) -> bool {
    let normalized: String = s
        .chars()
        .filter(|&c| c != '\t' && c != '\n' && c != '\r')
        .collect();
    let trimmed = normalized.trim_start_matches(|c: char| c.is_control() || c == ' ');
    match url_scheme(trimmed) {
        Some(scheme) => !SAFE_URL_SCHEMES
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed)),
        None => false,
    }
}

fn escape_url(s: &str) -> String {
    // A non-allowlisted scheme (javascript:, data:, vbscript:, …) is a
    // script-execution vector in href/src, so it is neutralised wholesale
    // rather than percent-encoded.
    if has_unsafe_scheme(s) {
        return "#".to_string();
    }
    // RFC 3986 reserved + unreserved characters are kept; everything
    // else is %-encoded. Sufficient for href/src/action attribute
    // values that came from untrusted sources.
    let safe = |c: char| {
        matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~')
            || matches!(
                c,
                ':' | '/'
                    | '?'
                    | '#'
                    | '['
                    | ']'
                    | '@'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | '%'
            )
    };
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if safe(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).as_bytes() {
                out.push('%');
                out.push(hex_nibble(byte >> 4));
                out.push(hex_nibble(byte & 0xf));
            }
        }
    }
    out
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + n - 10) as char,
        _ => '?',
    }
}

fn escape_js(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\'' => out.push_str("\\u0027"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Renders `source` against `data` through the block-capable text
/// engine, escaping every substituted value by the context the literal
/// markup before it puts the value in.
///
/// The template is one AST, so `if`, `range`, `with`, `define`, and
/// `template` all work and a value inside any of them is escaped exactly
/// as one at top level is. `{{ safe .x }}` writes through unescaped -
/// that is the caller taking responsibility for the fragment.
fn render_parsed(
    template: &crate::text::Template,
    data: &Value,
    missing: MissingKey,
) -> Result<String, Error> {
    let funcs = crate::text::FuncMap::new();
    template.render_with_missing(
        data,
        &funcs,
        Some(&|literal: &str, value: &str, raw: bool| {
            if raw {
                return value.to_string();
            }
            match detect_context(literal) {
                Context::Body | Context::Attr => escape_html_text(value),
                Context::AttrUnquoted => escape_attr_unquoted(value),
                Context::Url => escape_url(value),
                Context::Js => escape_js(value),
            }
        }),
        missing,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn map(entries: &[(&str, Value)]) -> Value {
        let mut m = BTreeMap::new();
        for (k, v) in entries {
            m.insert((*k).to_string(), v.clone());
        }
        Value::Map(m)
    }

    #[test]
    fn range_escapes_each_element_by_context() {
        let data = map(&[(
            "items",
            Value::Seq(vec![
                Value::String("<b>one</b>".into()),
                Value::String("two & three".into()),
            ]),
        )]);
        let out = render("<ul>{{range .items}}<li>{{ . }}</li>{{end}}</ul>", &data).unwrap();
        assert_eq!(
            out, "<ul><li>&lt;b&gt;one&lt;/b&gt;</li><li>two &amp; three</li></ul>",
            "got {out}"
        );
    }

    #[test]
    fn if_else_selects_a_branch() {
        let truthy = map(&[("on", Value::Bool(true))]);
        let falsy = map(&[("on", Value::Bool(false))]);
        let src = "{{if .on}}yes{{else}}no{{end}}";
        assert_eq!(render(src, &truthy).unwrap(), "yes");
        assert_eq!(render(src, &falsy).unwrap(), "no");
    }

    #[test]
    fn with_rebinds_the_dot() {
        let data = map(&[("user", map(&[("name", Value::String("Ada".into()))]))]);
        let out = render("{{with .user}}{{ .name }}{{end}}", &data).unwrap();
        assert_eq!(out, "Ada");
    }

    #[test]
    fn define_and_template_compose() {
        let data = map(&[("title", Value::String("Hi & bye".into()))]);
        let src = "{{define \"head\"}}<h1>{{ .title }}</h1>{{end}}{{template \"head\"}}";
        assert_eq!(render(src, &data).unwrap(), "<h1>Hi &amp; bye</h1>");
    }

    #[test]
    fn a_url_attribute_inside_a_range_still_blocks_javascript() {
        // The escape context is decided by the literal markup, so a value
        // reached through a block is protected exactly as one at top level.
        let data = map(&[(
            "links",
            Value::Seq(vec![Value::String("javascript:alert(1)".into())]),
        )]);
        let out = render("{{range .links}}<a href=\"{{ . }}\">x</a>{{end}}", &data).unwrap();
        assert!(!out.contains("javascript:"), "got {out}");
    }

    #[test]
    fn a_function_may_be_called_prefix_first() {
        let data = map(&[(
            "items",
            Value::Seq(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        )]);
        assert_eq!(render("{{ len .items }}", &data).unwrap(), "3");
        assert_eq!(render("{{ .items | len }}", &data).unwrap(), "3");
    }

    #[test]
    fn body_substitution_html_escapes() {
        let data = map(&[("name", Value::String("<script>alert(1)</script>".into()))]);
        let out = render("<p>hi {{ .name }}</p>", &data).unwrap();
        assert!(out.contains("&lt;script&gt;"));
        assert!(!out.contains("<script>"));
    }

    #[test]
    fn url_attribute_is_url_escaped() {
        let data = map(&[("link", Value::String("/a b?c=d".into()))]);
        let out = render("<a href=\"{{ .link }}\">x</a>", &data).unwrap();
        assert!(out.contains("/a%20b?c=d"));
    }

    #[test]
    fn url_attribute_blocks_javascript_scheme() {
        let data = map(&[(
            "link",
            Value::String("javascript:alert(document.cookie)".into()),
        )]);
        let out = render("<a href=\"{{ .link }}\">x</a>", &data).unwrap();
        assert!(!out.contains("javascript:"), "got {out}");
        assert!(out.contains("href=\"#\""), "got {out}");
    }

    #[test]
    fn url_attribute_blocks_obfuscated_javascript_scheme() {
        // Browsers strip tabs/newlines before parsing the scheme.
        let data = map(&[("link", Value::String("java\tscript:alert(1)".into()))]);
        let out = render("<a href=\"{{ .link }}\">x</a>", &data).unwrap();
        assert!(
            !out.to_ascii_lowercase().contains("javascript"),
            "got {out}"
        );
    }

    #[test]
    fn url_attribute_blocks_data_scheme() {
        let data = map(&[(
            "link",
            Value::String("data:text/html,<script>alert(1)</script>".into()),
        )]);
        let out = render("<a href=\"{{ .link }}\">x</a>", &data).unwrap();
        assert!(out.contains("href=\"#\""), "got {out}");
    }

    #[test]
    fn url_attribute_allows_safe_schemes_and_relative() {
        let data = map(&[("link", Value::String("https://example.com/p".into()))]);
        let out = render("<a href=\"{{ .link }}\">x</a>", &data).unwrap();
        assert!(out.contains("https://example.com/p"), "got {out}");
        let data = map(&[("link", Value::String("/relative/path".into()))]);
        let out = render("<a href=\"{{ .link }}\">x</a>", &data).unwrap();
        assert!(out.contains("/relative/path"), "got {out}");
    }

    #[test]
    fn unquoted_attribute_cannot_inject_a_new_attribute() {
        let data = map(&[("x", Value::String("y onmouseover=alert(1)".into()))]);
        let out = render("<a href={{ .x }}>x</a>", &data).unwrap();
        // href is a URL attribute, so the space is percent-encoded and
        // the payload stays inside the single href value - no raw space
        // means no new attribute.
        assert!(!out.contains(" onmouseover"), "raw space breaks out: {out}");
        assert!(out.contains("y%20onmouseover"), "got {out}");
    }

    #[test]
    fn unquoted_non_url_attribute_escapes_terminators() {
        let data = map(&[("v", Value::String("y onmouseover=alert(1)".into()))]);
        let out = render("<span data-x={{ .v }}>x</span>", &data).unwrap();
        // The space and `=` are numeric-escaped, so no new attribute.
        assert!(!out.contains(" onmouseover=alert(1)"), "got {out}");
        assert!(
            out.contains("&#32;"),
            "space must be numeric-escaped: {out}"
        );
    }

    #[test]
    fn js_context_is_json_quoted() {
        let data = map(&[("msg", Value::String("</script>".into()))]);
        let out = render("<script>let x = {{ .msg }};</script>", &data).unwrap();
        assert!(out.contains("\"\\u003c/script\\u003e\""));
    }

    #[test]
    fn safe_opts_out_of_escape() {
        let data = map(&[("body", Value::String("<b>bold</b>".into()))]);
        let out = render("<div>{{ safe .body }}</div>", &data).unwrap();
        assert_eq!(out, "<div><b>bold</b></div>");
    }

    #[test]
    fn literal_multibyte_text_is_preserved() {
        // Literal template text outside `{{ }}` must round-trip
        // verbatim; copying it byte-by-byte as `char` corrupted any
        // multi-byte sequence.
        let data = map(&[("n", Value::String("x".into()))]);
        let out = render("café - 日本語 🦀 {{ .n }}", &data).unwrap();
        assert_eq!(out, "café - 日本語 🦀 x");
    }
}
