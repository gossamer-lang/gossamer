//! HTML-template support and string escaping utilities.

#![forbid(unsafe_code)]

// The context-aware template engine lives in the leaf crate
// `gossamer-template` (below `gossamer-runtime`) so the compiled tier
// can render templates without a dependency cycle; re-exported here so
// `html::template::*` keeps its path.
pub use gossamer_template::html as template;

/// Escapes `s` for safe insertion into HTML text or attribute values.
///
/// Escapes the OWASP "CSP-grade" defensive set: `&`, `<`, `>`, `"`, `'`,
/// `/`, and backtick, so the result is safe in element content and in
/// quoted or unquoted attribute values. Context-specific escaping for URL /
/// JS / CSS sinks still requires the `html::template` engine.
#[must_use]
pub fn escape(s: &str) -> String {
    gossamer_runtime::codec::html::escape(s)
}

/// Unescapes `&amp; &lt; &gt; &quot; &apos; &nbsp;` and decimal (`&#NNN;`)
/// or hex (`&#xHHH;`) references; an `&` that starts no known entity is
/// kept as written.
#[must_use]
pub fn unescape(s: &str) -> String {
    gossamer_runtime::codec::html::unescape(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_special_chars() {
        // `/` is escaped to `&#x2F;` (OWASP CSP-grade set), so the
        // closing tag's slash becomes a numeric entity.
        assert_eq!(
            escape("<b>Hello & 'World'</b>"),
            "&lt;b&gt;Hello &amp; &#39;World&#39;&lt;&#x2F;b&gt;"
        );
    }

    #[test]
    fn escape_csp_grade_set() {
        // Forward slash and backtick are escaped to defuse closing-tag
        // and IE attribute-delimiter edge cases.
        assert_eq!(escape("a/b`c"), "a&#x2F;b&#x60;c");
        // Round-trips through unescape (hex numeric references).
        assert_eq!(unescape(&escape("</script>`")), "</script>`");
    }

    #[test]
    fn escape_quotes() {
        assert_eq!(escape("say \"hi\""), "say &quot;hi&quot;");
    }

    #[test]
    fn unescape_named_entities() {
        assert_eq!(
            unescape("&lt;b&gt;Hello &amp; World&lt;/b&gt;"),
            "<b>Hello & World</b>"
        );
    }

    #[test]
    fn unescape_numeric_decimal() {
        assert_eq!(unescape("&#65;"), "A");
    }

    #[test]
    fn unescape_numeric_hex() {
        assert_eq!(unescape("&#x41;"), "A");
    }

    #[test]
    fn escape_unescape_roundtrip() {
        let original = "<script>alert('XSS & \"danger\"');</script>";
        assert_eq!(unescape(&escape(original)), original);
    }
}
