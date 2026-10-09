//! Percent-encoding (RFC 3986) for URL queries, paths, and form values.

/// Which URL component a text is escaped for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// A query parameter or `x-www-form-urlencoded` value: space is `+`.
    Query,
    /// A path: space is `%20`, `+` is itself, and `/`, `:`, `@` stay.
    Path,
}

/// RFC 3986 unreserved bytes, which no component escapes.
#[must_use]
pub const fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

/// Escapes `text` for `component`.
#[must_use]
pub fn encode(text: &str, component: Component) -> String {
    match component {
        Component::Query => encode_keeping(text, is_unreserved, true),
        Component::Path => encode_keeping(
            text,
            |b| is_unreserved(b) || matches!(b, b'/' | b':' | b'@'),
            false,
        ),
    }
}

/// Escapes every byte of `text` that `keep` refuses as `%HH`, writing a
/// space as `+` when `space_as_plus`.
#[must_use]
pub fn encode_keeping(text: &str, keep: impl Fn(u8) -> bool, space_as_plus: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for &b in text.as_bytes() {
        if keep(b) {
            out.push(char::from(b));
        } else if space_as_plus && b == b' ' {
            out.push('+');
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0xF)]));
        }
    }
    out
}

/// Unescapes `text` for `component`, keeping a `%` that starts no escape as
/// written and replacing bytes that are not UTF-8 with U+FFFD.
#[must_use]
pub fn decode(text: &str, component: Component) -> String {
    let mut out = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%'
            && let Some(byte) = escape_at(bytes, i)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(if component == Component::Query && b == b'+' {
            b' '
        } else {
            b
        });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Unescapes `text`, refusing a malformed escape or a result that is not
/// UTF-8. `+` is a space when `plus_as_space`.
pub fn decode_strict(text: &str, plus_as_space: bool) -> Result<String, String> {
    let mut out = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err("truncated percent-escape".to_string());
                }
                out.push(escape_at(bytes, i).ok_or_else(|| "bad hex".to_string())?);
                i += 3;
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "non-UTF-8 percent-escape".to_string())
}

/// The byte the `%HH` escape at `at` spells, if two hex digits follow.
fn escape_at(bytes: &[u8], at: usize) -> Option<u8> {
    let digit = |b: u8| char::from(b).to_digit(16);
    let hi = digit(*bytes.get(at + 1)?)?;
    let lo = digit(*bytes.get(at + 2)?)?;
    Some((hi * 16 + lo) as u8)
}

#[cfg(test)]
mod tests {
    use super::{Component, decode, decode_strict, encode};

    #[test]
    fn components_escape_their_own_sets() {
        assert_eq!(
            encode("a b/c:d@e+f&g", Component::Query),
            "a+b%2Fc%3Ad%40e%2Bf%26g"
        );
        assert_eq!(
            encode("a b/c:d@e+f&g", Component::Path),
            "a%20b/c:d@e%2Bf%26g"
        );
        assert_eq!(encode("é", Component::Query), "%C3%A9");
    }

    #[test]
    fn lenient_decode_keeps_what_is_not_an_escape() {
        assert_eq!(decode("a+b%20c%C3%A9", Component::Query), "a b cé");
        assert_eq!(decode("a+b", Component::Path), "a+b");
        assert_eq!(decode("100%", Component::Query), "100%");
        assert_eq!(decode("%zz%4", Component::Query), "%zz%4");
        assert_eq!(decode("%41", Component::Query), "A");
        assert_eq!(decode("%FF", Component::Query), "\u{FFFD}");
    }

    #[test]
    fn strict_decode_refuses_malformed_escapes() {
        assert_eq!(decode_strict("a+%41", true).as_deref(), Ok("a A"));
        assert_eq!(decode_strict("a+%41", false).as_deref(), Ok("a+A"));
        assert!(decode_strict("%4", true).is_err());
        assert!(decode_strict("%zz", true).is_err());
        assert!(decode_strict("%FF", true).is_err());
    }
}
