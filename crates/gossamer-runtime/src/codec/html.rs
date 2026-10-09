//! HTML entity escaping for text and attribute values.

/// Longest entity body [`unescape`] reads before deciding an `&` starts no
/// entity; `#x10FFFF` is the longest it knows.
const MAX_ENTITY: usize = 10;

/// Escapes `text` for HTML element content and quoted or unquoted attribute
/// values: `&`, `<`, `>`, `"`, `'`, plus `/` and backtick, which close
/// closing-tag and legacy attribute-delimiter parsing edge cases.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '/' => out.push_str("&#x2F;"),
            '`' => out.push_str("&#x60;"),
            c => out.push(c),
        }
    }
    out
}

/// Replaces the entities `&amp; &lt; &gt; &quot; &apos; &nbsp;` and decimal
/// or hex character references with their characters. An `&` that starts
/// no entity it knows is kept as written.
#[must_use]
pub fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let entity = after
            .char_indices()
            .take(MAX_ENTITY + 1)
            .find(|&(_, c)| c == ';')
            .and_then(|(semi, _)| entity_char(&after[..semi]).map(|c| (c, semi)));
        if let Some((c, semi)) = entity {
            out.push(c);
            rest = &after[semi + 1..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The character entity body `name` stands for.
fn entity_char(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{A0}'),
        _ => {
            let number = name.strip_prefix('#')?;
            let value = match number.strip_prefix(['x', 'X']) {
                Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                    u32::from_str_radix(hex, 16).ok()?
                }
                Some(_) => return None,
                None if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => {
                    number.parse().ok()?
                }
                None => return None,
            };
            char::from_u32(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{escape, unescape};

    #[test]
    fn escape_covers_the_defensive_set() {
        assert_eq!(
            escape("<b>Hello & 'World'</b>"),
            "&lt;b&gt;Hello &amp; &#39;World&#39;&lt;&#x2F;b&gt;"
        );
        assert_eq!(escape("a/b`c \"q\""), "a&#x2F;b&#x60;c &quot;q&quot;");
    }

    #[test]
    fn unescape_reads_named_and_numeric_entities() {
        assert_eq!(
            unescape("&lt;b&gt;Hello &amp; World&lt;/b&gt;"),
            "<b>Hello & World</b>"
        );
        assert_eq!(
            unescape("&#65;&#x41;&#X41;&apos;&#39;&nbsp;"),
            "AAA''\u{A0}"
        );
    }

    #[test]
    fn unescape_keeps_what_is_not_an_entity() {
        assert_eq!(unescape("&&amp;"), "&&");
        assert_eq!(unescape("a & b; c"), "a & b; c");
        assert_eq!(
            unescape("&unknown;&#xZZ;&#;&#x;&#+5;"),
            "&unknown;&#xZZ;&#;&#x;&#+5;"
        );
        assert_eq!(unescape("&#xD800;&#1114112;"), "&#xD800;&#1114112;");
        assert_eq!(unescape("trailing &"), "trailing &");
        assert_eq!(unescape("&é;"), "&é;");
    }

    #[test]
    fn round_trips_escaped_text() {
        let original = "<script>alert('XSS & \"danger\"');</script>`";
        assert_eq!(unescape(&escape(original)), original);
    }

    #[test]
    fn many_ampersands_stay_linear() {
        let text = "&".repeat(200_000);
        assert_eq!(unescape(&text), text);
    }
}
